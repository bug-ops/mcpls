//! A stdio [`Transport`] with an mcpls-owned message-size limit.
//!
//! `rmcp`'s own stdio framing silently drops a line over its internal cap
//! before parsing it, and drops one its codec cannot parse, so the client never
//! learns that its request was lost. [`StdioTransport`] bounds the line itself
//! and, for a rejected request, recovers the request `id` with a streaming
//! [`EnvelopeScan`] and answers with a JSON-RPC error. Lines within the limit
//! are decoded by `rmcp`'s own codec, so every other framing rule stays `rmcp`'s.

use std::num::NonZeroUsize;
use std::time::Instant;

use rmcp::RoleServer;
use rmcp::model::{ErrorData, RequestId};
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::{AsyncRwTransport, JsonRpcMessageCodec, JsonRpcMessageCodecError};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, BufReader, Empty};
use tokio::task::JoinHandle;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::Decoder as _;

use crate::json_envelope::{Envelope, EnvelopeScan, IdToken, Shape};
use crate::lsp::DropCounter;

/// Non-zero cap, in bytes, on one incoming stdio JSON-RPC line.
///
/// The count covers the line without its terminating `\n`; a `\r` before it
/// counts. A line of exactly the limit is accepted, one byte more is rejected.
/// A zero cap would reject every message, so it is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdioMessageLimit(NonZeroUsize);

impl StdioMessageLimit {
    /// 4 MiB, the same bound the HTTP transport applies to a request body.
    pub(crate) const DEFAULT: Self = Self(super::DEFAULT_MESSAGE_BYTES);

    /// The limit in bytes.
    pub(crate) const fn get(self) -> usize {
        self.0.get()
    }

    /// A limit of `bytes`, or `None` for zero.
    #[cfg(test)]
    pub(crate) fn new(bytes: usize) -> Option<Self> {
        NonZeroUsize::new(bytes).map(Self)
    }
}

/// What a rejected line asks of the transport.
#[derive(Debug, PartialEq, Eq)]
enum ScanOutcome {
    /// A request; the id is `None` when it cannot be echoed.
    Request(Option<RequestId>),
    /// An object with an `id` and neither `method`, `result` nor `error`.
    IdOnly,
    /// A notification or a client response, whose id (if any) is not a request's.
    NoReply,
    /// Not a JSON object, or an object with nothing to tell what it was.
    Unstructured,
}

impl ScanOutcome {
    /// Classifies a scanned line. A truncated line counts as a request only
    /// when its `method` began and its `id` was read in full.
    fn of(envelope: &Envelope) -> Self {
        let Envelope {
            shape,
            method,
            outcome,
            id,
        } = envelope;
        match (shape, method, id) {
            (Shape::Truncated | Shape::Malformed, true, IdToken::Complete(_)) => {
                Self::Request(id.decode())
            }
            (Shape::Malformed | Shape::Truncated, ..) | (Shape::Closed, false, IdToken::Absent) => {
                Self::Unstructured
            }
            (Shape::Closed, true, IdToken::Absent) => Self::NoReply,
            (Shape::Closed, true, _) => Self::Request(id.decode()),
            (Shape::Closed, false, _) if *outcome => Self::NoReply,
            (Shape::Closed, false, _) => Self::IdOnly,
        }
    }

    fn of_scan(scan: &EnvelopeScan) -> Self {
        Self::of(&scan.finish())
    }
}

/// Why a line was rejected.
#[derive(Debug)]
enum RejectCause {
    Oversized,
    /// Not JSON (or too deeply nested to read): a syntax or EOF error.
    Unparsable(serde_json::Error),
    /// JSON the MCP codec does not accept as a message: a data error.
    NotJsonRpc(serde_json::Error),
    /// A message the MCP codec skipped for compatibility, which can hide a request.
    Skipped,
}

impl RejectCause {
    fn of_decode_error(error: serde_json::Error) -> Self {
        match error.classify() {
            serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
                Self::Unparsable(error)
            }
            serde_json::error::Category::Data | serde_json::error::Category::Io => {
                Self::NotJsonRpc(error)
            }
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::Oversized => "oversized",
            Self::Unparsable(_) => "unparsable",
            Self::NotJsonRpc(_) => "not a JSON-RPC message",
            Self::Skipped => "skipped by the codec",
        }
    }

    const fn error(&self) -> Option<&serde_json::Error> {
        match self {
            Self::Oversized | Self::Skipped => None,
            Self::Unparsable(error) | Self::NotJsonRpc(error) => Some(error),
        }
    }
}

#[derive(Debug)]
struct Rejected {
    cause: RejectCause,
    outcome: ScanOutcome,
    bytes: usize,
}

type Reply = (ErrorData, Option<RequestId>);

impl Rejected {
    /// The error to send back, with the request id it answers, if any.
    ///
    /// A notification, a client response and an `id`-only object never get an
    /// id echoed: that id belongs to the server's own id space, or to nothing.
    fn reply(&self, limit: usize) -> Option<Reply> {
        let invalid = || ErrorData::invalid_request("Invalid request", None);
        match (&self.cause, &self.outcome) {
            (RejectCause::Oversized, ScanOutcome::Request(id)) => Some((
                ErrorData::invalid_request(
                    format!("request exceeds the {limit}-byte stdio message limit"),
                    None,
                ),
                id.clone(),
            )),
            (RejectCause::Unparsable(error), ScanOutcome::Request(id)) => Some((
                ErrorData::parse_error(format!("message is not parsable JSON: {error}"), None),
                id.clone(),
            )),
            (RejectCause::NotJsonRpc(_) | RejectCause::Skipped, ScanOutcome::Request(id)) => {
                Some((invalid(), id.clone()))
            }
            (
                RejectCause::NotJsonRpc(_),
                ScanOutcome::IdOnly | ScanOutcome::NoReply | ScanOutcome::Unstructured,
            ) => Some((invalid(), None)),
            (
                RejectCause::Oversized | RejectCause::Unparsable(_) | RejectCause::Skipped,
                ScanOutcome::IdOnly | ScanOutcome::NoReply | ScanOutcome::Unstructured,
            ) => None,
        }
    }
}

#[derive(Debug)]
enum ReadState {
    Buffering,
    Discarding { scan: EnvelopeScan, bytes: usize },
}

enum LineRead {
    Line,
    Rejected(Rejected),
    Eof,
    Io(std::io::Error),
}

enum Step {
    More,
    Line,
    Rejected(Rejected),
}

/// Bounded stdio framing for the MCP server role.
///
/// `receive` never lets a line grow past the [`StdioMessageLimit`]. An
/// oversized line is skipped to its newline and answered per its
/// [`EnvelopeScan`]; the stream stays in sync for the next line. A line within
/// the limit that `rmcp`'s codec cannot decode is classified the same way and
/// answered when it was a request. Replies to rejected input are written by a
/// spawned task, because the service loop drops a pending `receive` whenever an
/// outgoing message is ready, and a reply awaited inside it would be lost once
/// its line was consumed. At most one such reply is in flight: `receive`
/// settles it before reading the next line, which keeps the backpressure a peer
/// that stops reading stdout got from an inline write.
pub struct StdioTransport<R, W: AsyncWrite> {
    read: BufReader<R>,
    line: BytesMut,
    state: ReadState,
    limit: StdioMessageLimit,
    codec: JsonRpcMessageCodec<RxJsonRpcMessage<RoleServer>>,
    writer: AsyncRwTransport<RoleServer, Empty, W>,
    reply: Option<JoinHandle<()>>,
    undecodable_log: DropCounter,
}

impl<R, W> StdioTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    /// Frames `read` and `write` with lines bounded by `limit`.
    pub(crate) fn new(read: R, write: W, limit: StdioMessageLimit) -> Self {
        Self {
            read: BufReader::new(read),
            line: BytesMut::new(),
            state: ReadState::Buffering,
            limit,
            codec: JsonRpcMessageCodec::new(),
            writer: AsyncRwTransport::new(tokio::io::empty(), write),
            reply: None,
            undecodable_log: DropCounter::new(),
        }
    }

    /// Reads one line without letting it grow past the limit.
    ///
    /// Cancel-safe: all progress lives on `self`, and the copy out of the
    /// reader's buffer and its `consume` happen with no await between them.
    async fn read_line(&mut self) -> LineRead {
        let limit = self.limit.get();
        loop {
            let (take, step) = {
                let available = match self.read.fill_buf().await {
                    Ok(available) => available,
                    Err(e) => return LineRead::Io(e),
                };
                if available.is_empty() {
                    return LineRead::Eof;
                }
                let newline = available.iter().position(|b| *b == b'\n');
                let content_len = newline.unwrap_or(available.len());
                let take = newline.map_or(available.len(), |idx| idx.saturating_add(1));
                let content = available.get(..content_len).unwrap_or_default();
                let ended = newline.is_some();
                let step = match &mut self.state {
                    ReadState::Buffering => {
                        let total = self.line.len().saturating_add(content_len);
                        if total > limit {
                            let mut scan = EnvelopeScan::default();
                            scan.feed(&self.line);
                            scan.feed(content);
                            self.line.clear();
                            if ended {
                                Step::Rejected(oversized(&scan, total))
                            } else {
                                self.state = ReadState::Discarding { scan, bytes: total };
                                Step::More
                            }
                        } else {
                            self.line.extend_from_slice(content);
                            if ended { Step::Line } else { Step::More }
                        }
                    }
                    ReadState::Discarding { scan, bytes } => {
                        scan.feed(content);
                        *bytes = bytes.saturating_add(content_len);
                        if ended {
                            let rejected = oversized(scan, *bytes);
                            self.state = ReadState::Buffering;
                            Step::Rejected(rejected)
                        } else {
                            Step::More
                        }
                    }
                };
                (take, step)
            };
            self.read.consume(take);
            match step {
                Step::More => {}
                Step::Line => return LineRead::Line,
                Step::Rejected(rejected) => return LineRead::Rejected(rejected),
            }
        }
    }

    fn spawn_reply(&mut self, message: TxJsonRpcMessage<RoleServer>) {
        let send = self.writer.send(message);
        self.reply = Some(tokio::spawn(async move {
            if let Err(error) = send.await {
                tracing::debug!(%error, "stdio reply not delivered");
            }
        }));
    }

    /// Waits for the in-flight reply, if any. Cancel-safe: the task is
    /// independent of this future and the handle stays until it completes.
    async fn settle_reply(&mut self) {
        if let Some(handle) = &mut self.reply {
            if let Err(error) = handle.await {
                tracing::debug!(%error, "stdio reply task failed");
            }
            self.reply = None;
        }
    }

    /// Logs the rejection and answers it when it asks for an answer.
    ///
    /// An oversized line always warns; undecodable ones warn at most once a
    /// minute, with the number silenced since the last warning.
    fn reject(&mut self, rejected: &Rejected) {
        let limit = self.limit.get();
        let reply = rejected.reply(limit);
        let request_id = match &rejected.outcome {
            ScanOutcome::Request(id) => id.as_ref(),
            ScanOutcome::IdOnly | ScanOutcome::NoReply | ScanOutcome::Unstructured => None,
        };
        let suppressed = match rejected.cause {
            RejectCause::Oversized => Some(0),
            RejectCause::Unparsable(_) | RejectCause::NotJsonRpc(_) | RejectCause::Skipped => {
                self.undecodable_log.record(Instant::now())
            }
        };
        let cause = rejected.cause.label();
        let error = rejected.cause.error().map(tracing::field::display);
        let (bytes, replied) = (rejected.bytes, reply.is_some());
        if let Some(suppressed) = suppressed {
            tracing::warn!(
                cause,
                ?request_id,
                bytes,
                limit,
                replied,
                suppressed,
                error,
                "stdio message rejected"
            );
        } else {
            tracing::debug!(
                cause,
                ?request_id,
                bytes,
                limit,
                replied,
                error,
                "stdio message rejected"
            );
        }
        if let Some((error, id)) = reply {
            self.spawn_reply(TxJsonRpcMessage::<RoleServer>::error(error, id));
        }
    }

    /// Classifies the buffered line as if its line ending followed it, so LF
    /// and CRLF clients are treated alike.
    fn line_outcome(&self) -> ScanOutcome {
        let mut scan = EnvelopeScan::default();
        scan.feed(&self.line);
        scan.feed(b"\n");
        ScanOutcome::of_scan(&scan)
    }

    /// Decodes the buffered line, leaving `self.line` untouched so a decode
    /// failure can still be classified from its bytes.
    fn decode_line(&mut self) -> Result<Option<RxJsonRpcMessage<RoleServer>>, DecodeFailure> {
        let mut frame = BytesMut::with_capacity(self.line.len().saturating_add(1));
        frame.extend_from_slice(&self.line);
        frame.extend_from_slice(b"\n");
        match self.codec.decode(&mut frame) {
            Ok(Some(message)) => Ok(Some(message)),
            Ok(None) => match self.line_outcome() {
                outcome @ ScanOutcome::Request(_) => Err(DecodeFailure::Rejected(Rejected {
                    cause: RejectCause::Skipped,
                    outcome,
                    bytes: self.line.len(),
                })),
                ScanOutcome::IdOnly | ScanOutcome::NoReply | ScanOutcome::Unstructured => Ok(None),
            },
            Err(JsonRpcMessageCodecError::Serde(error)) => Err(DecodeFailure::Rejected(Rejected {
                cause: RejectCause::of_decode_error(error),
                outcome: self.line_outcome(),
                bytes: self.line.len(),
            })),
            Err(error) => Err(DecodeFailure::Fatal(error)),
        }
    }
}

fn oversized(scan: &EnvelopeScan, bytes: usize) -> Rejected {
    Rejected {
        cause: RejectCause::Oversized,
        outcome: ScanOutcome::of_scan(scan),
        bytes,
    }
}

enum DecodeFailure {
    Rejected(Rejected),
    Fatal(JsonRpcMessageCodecError),
}

impl<R, W> Transport<RoleServer> for StdioTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.writer.send(item)
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        loop {
            self.settle_reply().await;
            match self.read_line().await {
                LineRead::Line => {}
                LineRead::Rejected(rejected) => {
                    self.reject(&rejected);
                    continue;
                }
                LineRead::Eof => return None,
                LineRead::Io(error) => {
                    tracing::error!(%error, "error reading from stdin");
                    return None;
                }
            }
            if self.line.is_empty() || self.line.as_ref() == b"\r" {
                self.line.clear();
                continue;
            }
            match self.decode_line() {
                Ok(Some(message)) => {
                    self.line.clear();
                    return Some(message);
                }
                Ok(None) => {}
                Err(DecodeFailure::Rejected(rejected)) => self.reject(&rejected),
                Err(DecodeFailure::Fatal(error)) => {
                    tracing::error!(%error, "error decoding stdin");
                    return None;
                }
            }
            self.line.clear();
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.writer.close().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use futures::FutureExt as _;
    use serde_json::{Value, json};
    use tokio::io::{AsyncWriteExt as _, DuplexStream, ReadHalf, WriteHalf};
    use tokio::sync::Mutex;

    use super::*;

    const INVALID_REQUEST: i64 = -32600;
    const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

    type Server = StdioTransport<ReadHalf<DuplexStream>, WriteHalf<DuplexStream>>;
    type ClientReader = BufReader<ReadHalf<DuplexStream>>;
    type ClientWriter = WriteHalf<DuplexStream>;

    fn padded(base: &str, len: usize) -> String {
        let pad = len.checked_sub(base.len()).expect("base fits the target");
        format!("{base}{}", " ".repeat(pad))
    }

    fn ping(id: u32) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#)
    }

    fn oversized_call(id_member: &str, size: usize) -> String {
        let head = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"pad":""#;
        let tail = format!(r#""}},{id_member}}}"#);
        let fill = size.saturating_sub(head.len() + tail.len());
        format!("{head}{}{tail}", "x".repeat(fill))
    }

    fn link(limit: usize, capacity: usize) -> (Server, ClientReader, ClientWriter) {
        let (server_io, client_io) = tokio::io::duplex(capacity);
        let (server_r, server_w) = tokio::io::split(server_io);
        let (client_r, client_w) = tokio::io::split(client_io);
        let limit = StdioMessageLimit::new(limit).unwrap();
        (
            StdioTransport::new(server_r, server_w, limit),
            BufReader::new(client_r),
            client_w,
        )
    }

    async fn next_json(reader: &mut ClientReader) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .expect("a reply arrives before the deadline")
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn assert_no_reply(reader: &mut ClientReader) {
        let mut line = String::new();
        let waited =
            tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut line)).await;
        assert!(waited.is_err(), "unexpected reply: {line:?}");
    }

    async fn received_id(server: &mut Server) -> Value {
        let message = tokio::time::timeout(Duration::from_secs(5), server.receive())
            .await
            .expect("a message arrives before the deadline")
            .expect("the stream is open");
        serde_json::to_value(&message).unwrap()["id"].clone()
    }

    #[test]
    fn test_default_limit_is_four_mib() {
        assert_eq!(StdioMessageLimit::DEFAULT.get(), 4 * 1024 * 1024);
        assert!(StdioMessageLimit::new(0).is_none());
    }

    #[cfg(feature = "transport-http")]
    #[test]
    fn test_default_limit_matches_the_http_request_body_cap() {
        assert_eq!(
            StdioMessageLimit::DEFAULT.get(),
            crate::RequestBodyLimit::DEFAULT.get()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_line_at_the_limit_is_accepted_and_one_more_byte_is_rejected() {
        let (mut server, mut reader, mut writer) = link(256, 64 * 1024);
        let at_limit = padded(&ping(1), 256);
        let over = padded(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call"}"#, 257);
        writer
            .write_all(format!("{at_limit}\n{over}\n{}\n", ping(3)).as_bytes())
            .await
            .unwrap();

        assert_eq!(received_id(&mut server).await, json!(1));
        assert_eq!(received_id(&mut server).await, json!(3));

        let reply = next_json(&mut reader).await;
        assert_eq!(reply["id"], json!(2));
        assert_eq!(reply["error"]["code"], json!(INVALID_REQUEST));
        assert_eq!(
            reply["error"]["message"],
            json!("request exceeds the 256-byte stdio message limit")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_carriage_return_counts_toward_the_limit() {
        let (mut server, mut reader, mut writer) = link(64, 4096);
        let line = padded(&ping(1), 63);
        writer
            .write_all(format!("{line}\r\n{}\r\n", ping(2)).as_bytes())
            .await
            .unwrap();
        assert_eq!(received_id(&mut server).await, json!(1));
        assert_eq!(received_id(&mut server).await, json!(2));

        let over = padded(&ping(5), 64);
        writer
            .write_all(format!("{over}\r\n{}\n", ping(6)).as_bytes())
            .await
            .unwrap();
        assert_eq!(received_id(&mut server).await, json!(6));
        assert_eq!(next_json(&mut reader).await["id"], json!(5));
    }

    #[tokio::test(start_paused = true)]
    async fn test_oversized_line_split_across_many_reads_is_answered() {
        let (mut server, mut reader, mut writer) = link(1024, 64);
        let line = format!("{}\n{}\n", oversized_call(r#""id":5"#, 20_000), ping(6));
        let feeder = tokio::spawn(async move {
            for chunk in line.as_bytes().chunks(37) {
                writer.write_all(chunk).await.unwrap();
            }
            writer
        });

        let (id, reply) = tokio::join!(received_id(&mut server), next_json(&mut reader));
        assert_eq!(id, json!(6));
        assert_eq!(reply["id"], json!(5));
        assert_eq!(reply["error"]["code"], json!(INVALID_REQUEST));
        feeder.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn test_reply_survives_dropping_receive_right_after_the_oversized_line() {
        let (mut server, mut reader, mut writer) = link(128, 64 * 1024);
        let line = format!("{}\n", oversized_call(r#""id":"abc""#, 400));
        writer.write_all(line.as_bytes()).await.unwrap();

        assert!(server.receive().now_or_never().is_none());

        let reply = next_json(&mut reader).await;
        assert_eq!(reply["id"], json!("abc"));
        assert_eq!(reply["error"]["code"], json!(INVALID_REQUEST));
    }

    #[tokio::test(start_paused = true)]
    async fn test_receive_cancelled_mid_discard_resumes_the_same_line() {
        let (mut server, mut reader, mut writer) = link(128, 64 * 1024);
        let line = oversized_call(r#""id":9"#, 400);
        let (head, tail) = line.split_at(250);
        writer.write_all(head.as_bytes()).await.unwrap();
        assert!(server.receive().now_or_never().is_none());

        writer
            .write_all(format!("{tail}\n{}\n", ping(10)).as_bytes())
            .await
            .unwrap();
        assert_eq!(received_id(&mut server).await, json!(10));
        assert_eq!(next_json(&mut reader).await["id"], json!(9));
    }

    #[tokio::test(start_paused = true)]
    async fn test_receive_cancelled_mid_line_within_the_limit_resumes() {
        let (mut server, _reader, mut writer) = link(1024, 64 * 1024);
        let line = ping(4);
        let (head, tail) = line.split_at(10);
        writer.write_all(head.as_bytes()).await.unwrap();
        assert!(server.receive().now_or_never().is_none());

        writer
            .write_all(format!("{tail}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(received_id(&mut server).await, json!(4));
    }

    #[tokio::test(start_paused = true)]
    async fn test_unusable_id_on_an_oversized_request_gets_an_id_less_error() {
        let (mut server, mut reader, mut writer) = link(128, 64 * 1024);
        let line = format!("{}\n{}\n", oversized_call(r#""id":1.5"#, 400), ping(2));
        writer.write_all(line.as_bytes()).await.unwrap();

        assert_eq!(received_id(&mut server).await, json!(2));
        let reply = next_json(&mut reader).await;
        assert!(reply.get("id").is_none(), "{reply}");
        assert_eq!(reply["error"]["code"], json!(INVALID_REQUEST));
    }

    #[tokio::test(start_paused = true)]
    async fn test_oversized_notification_and_response_get_no_reply() {
        let (mut server, mut reader, mut writer) = link(128, 64 * 1024);
        let notification = format!(
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized","params":{{"pad":"{}"}}}}"#,
            "x".repeat(300)
        );
        let response = format!(
            r#"{{"jsonrpc":"2.0","id":3,"result":{{"pad":"{}"}}}}"#,
            "x".repeat(300)
        );
        writer
            .write_all(format!("{notification}\n{response}\n{}\n", ping(4)).as_bytes())
            .await
            .unwrap();

        assert_eq!(received_id(&mut server).await, json!(4));
        assert_no_reply(&mut reader).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_oversized_line_with_a_bom_is_answered() {
        let (mut server, mut reader, mut writer) = link(128, 64 * 1024);
        let mut line = BOM.to_vec();
        line.extend_from_slice(oversized_call(r#""id":12"#, 400).as_bytes());
        line.push(b'\n');
        writer.write_all(&line).await.unwrap();
        writer
            .write_all(format!("{}\n", ping(13)).as_bytes())
            .await
            .unwrap();

        assert_eq!(received_id(&mut server).await, json!(13));
        assert_eq!(next_json(&mut reader).await["id"], json!(12));
    }

    #[tokio::test(start_paused = true)]
    async fn test_ordinary_framing_rules_are_unchanged() {
        let (mut server, mut reader, mut writer) = link(1024, 64 * 1024);
        let mut bom_line = BOM.to_vec();
        bom_line.extend_from_slice(ping(1).as_bytes());
        bom_line.extend_from_slice(b"\r\n");
        writer.write_all(&bom_line).await.unwrap();
        writer
            .write_all(format!("\n\r\nnot json\n{{\"foo\":1}}\n{}\n", ping(2)).as_bytes())
            .await
            .unwrap();

        assert_eq!(received_id(&mut server).await, json!(1));
        assert_eq!(received_id(&mut server).await, json!(2));
        let reply = next_json(&mut reader).await;
        assert!(reply.get("id").is_none(), "{reply}");
        assert_eq!(reply["error"]["code"], json!(INVALID_REQUEST));
        assert_eq!(reply["error"]["message"], json!("Invalid request"));
        assert_no_reply(&mut reader).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_eof_inside_an_oversized_line_ends_the_stream_without_a_reply() {
        let (mut server, mut reader, mut writer) = link(128, 64 * 1024);
        writer
            .write_all(oversized_call(r#""id":1"#, 400).as_bytes())
            .await
            .unwrap();
        writer.shutdown().await.unwrap();

        assert!(server.receive().await.is_none());
        server.close().await.unwrap();
        drop(server);
        let mut rest = String::new();
        reader.read_line(&mut rest).await.unwrap();
        assert!(rest.is_empty(), "{rest:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn test_oversized_tools_call_is_answered_through_the_mcp_service() {
        use crate::bridge::{NotificationCache, Translator, WorkspaceRoots};
        use crate::config::McpConfig;
        use crate::mcp::{McplsServer, SubscriptionRegistry};

        let server = McplsServer::new(
            Arc::new(Translator::new()),
            Arc::new(Mutex::new(NotificationCache::new())),
            WorkspaceRoots::default(),
            SubscriptionRegistry::new(),
            crate::ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let (server_r, server_w) = tokio::io::split(server_io);
        let (client_r, mut writer) = tokio::io::split(client_io);
        let mut reader = BufReader::new(client_r);
        let limit = StdioMessageLimit::new(1024).unwrap();
        let transport = StdioTransport::new(server_r, server_w, limit);
        let serving = tokio::spawn(crate::transport::serve_until(
            server,
            transport,
            std::future::pending(),
        ));

        let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
        writer
            .write_all(format!("{initialize}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(next_json(&mut reader).await["id"], json!(1));
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();

        let big = oversized_call(r#""id":2"#, 8 * 1024);
        writer
            .write_all(format!("{big}\n{}\n", ping(3)).as_bytes())
            .await
            .unwrap();

        let mut answered = Vec::new();
        while answered.len() < 2 {
            let reply = next_json(&mut reader).await;
            if reply.get("id").is_some() {
                answered.push(reply);
            }
        }
        let rejected = answered.iter().find(|r| r["id"] == json!(2)).unwrap();
        assert_eq!(rejected["error"]["code"], json!(INVALID_REQUEST));
        let pong = answered.iter().find(|r| r["id"] == json!(3)).unwrap();
        assert!(pong.get("result").is_some(), "{pong}");

        serving.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn test_many_rejected_lines_are_answered_one_at_a_time_without_loss() {
        let (mut server, mut reader, mut writer) = link(1024, 16);
        let lines = format!("{}{}\n", "{}\n".repeat(40), ping(7));
        let feeder = tokio::spawn(async move {
            writer.write_all(lines.as_bytes()).await.unwrap();
            writer
        });

        let drain = async {
            for _ in 0..40 {
                let reply = next_json(&mut reader).await;
                assert_eq!(reply["error"]["code"], json!(INVALID_REQUEST));
            }
        };
        let (id, ()) = tokio::join!(received_id(&mut server), drain);
        assert_eq!(id, json!(7));
        feeder.await.unwrap();
    }

    const PARSE_ERROR: i64 = -32700;

    fn deep_call(depth: usize, id: u32) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"tools/call","id":{id},"params":{{"name":"get_hover","arguments":{{"file_path":"x","z":{}{}}}}}}}"#,
            "[".repeat(depth),
            "]".repeat(depth)
        )
    }

    async fn send_lines(
        server: &mut Server,
        reader: &mut ClientReader,
        writer: ClientWriter,
        lines: String,
    ) -> (Value, Value) {
        let feeder = tokio::spawn(async move {
            let mut writer = writer;
            writer.write_all(lines.as_bytes()).await.unwrap();
            writer
        });
        let (delivered, reply) = tokio::join!(received_id(server), next_json(reader));
        feeder.await.unwrap();
        (delivered, reply)
    }

    #[tokio::test(start_paused = true)]
    async fn test_recursion_limit_boundary_is_decided_by_the_codec() {
        let (mut server, mut reader, mut writer) = link(1 << 20, 64 * 1024);
        let lines = format!(
            "{}\n{}\n{}\n",
            deep_call(124, 7),
            deep_call(125, 8),
            ping(9)
        );
        writer.write_all(lines.as_bytes()).await.unwrap();

        assert_eq!(received_id(&mut server).await, json!(7));
        assert_eq!(received_id(&mut server).await, json!(9));
        let reply = next_json(&mut reader).await;
        assert_eq!(reply["id"], json!(8));
        assert_eq!(reply["error"]["code"], json!(PARSE_ERROR));
    }

    #[tokio::test(start_paused = true)]
    async fn test_request_nested_past_the_recursion_limit_is_answered_with_its_id() {
        for depth in [126, 150, 100_000] {
            let (mut server, mut reader, writer) = link(4 << 20, 64 * 1024);
            let lines = format!("{}\n{}\n", deep_call(depth, 7), ping(99));

            let (delivered, reply) = send_lines(&mut server, &mut reader, writer, lines).await;

            assert_eq!(delivered, json!(99), "depth {depth}");
            assert_eq!(reply["id"], json!(7), "depth {depth}");
            assert_eq!(reply["error"]["code"], json!(PARSE_ERROR), "depth {depth}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_truncated_request_is_answered_with_its_id() {
        let (mut server, mut reader, writer) = link(1024, 64 * 1024);
        let lines = format!(
            "{}\n{}\n",
            r#"{"jsonrpc":"2.0","id":7,"method":"x""#,
            ping(8)
        );

        let (delivered, reply) = send_lines(&mut server, &mut reader, writer, lines).await;

        assert_eq!(delivered, json!(8));
        assert_eq!(reply["id"], json!(7));
        assert_eq!(reply["error"]["code"], json!(PARSE_ERROR));
    }

    #[tokio::test(start_paused = true)]
    async fn test_unparsable_lines_without_a_readable_request_id_get_no_reply() {
        let (mut server, mut reader, mut writer) = link(4 << 20, 64 * 1024);
        let notification = format!(
            r#"{{"jsonrpc":"2.0","method":"notifications/x","params":{}{}}}"#,
            "[".repeat(200),
            "]".repeat(200)
        );
        let response = format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{}{}}}"#,
            "[".repeat(200),
            "]".repeat(200)
        );
        let escaped_id_key = deep_call(200, 5).replace(r#""id""#, r#""\u0069d""#);
        let lines = [
            r#"{"jsonrpc":"2.0","method":"x","id":"ab"#.to_owned(),
            r#"{"jsonrpc":"2.0","method":"x","id":"#.to_owned(),
            notification,
            response,
            escaped_id_key,
            ping(2),
        ];
        writer
            .write_all(format!("{}\n", lines.join("\n")).as_bytes())
            .await
            .unwrap();

        assert_eq!(received_id(&mut server).await, json!(2));
        assert_no_reply(&mut reader).await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_non_message_json_is_answered_per_the_scanned_envelope() {
        let (mut server, mut reader, mut writer) = link(1024, 64 * 1024);
        writer
            .write_all(
                format!(
                    "{}\n{}\n{}\n",
                    r#"{"jsonrpc":"2.0","id":1}"#,
                    r#"{"jsonrpc":"2.0","id":5,"method":"initialize","params":5}"#,
                    ping(3)
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        assert_eq!(received_id(&mut server).await, json!(3));
        let id_only = next_json(&mut reader).await;
        assert!(id_only.get("id").is_none(), "{id_only}");
        assert_eq!(id_only["error"]["code"], json!(INVALID_REQUEST));
        let request = next_json(&mut reader).await;
        assert_eq!(request["id"], json!(5));
        assert_eq!(request["error"]["code"], json!(INVALID_REQUEST));
    }

    #[tokio::test(start_paused = true)]
    async fn test_unclosed_oversized_request_with_an_id_is_answered() {
        let (mut server, mut reader, mut writer) = link(128, 64 * 1024);
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"m","params":{{"pad":"{}"#,
            "x".repeat(400)
        );
        writer
            .write_all(format!("{line}\n{}\n", ping(5)).as_bytes())
            .await
            .unwrap();

        assert_eq!(received_id(&mut server).await, json!(5));
        let reply = next_json(&mut reader).await;
        assert_eq!(reply["id"], json!(4));
        assert_eq!(reply["error"]["code"], json!(INVALID_REQUEST));
    }

    #[tokio::test(start_paused = true)]
    async fn test_undecodable_lines_warn_once_with_the_silenced_count() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let captured = crate::test_lsp::CapturedLogs::default();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
        let (mut server, _reader, mut writer) = link(1024, 64 * 1024);
        writer
            .write_all(format!("not json\nstill not\nnor this\n{}\n", ping(1)).as_bytes())
            .await
            .unwrap();

        assert_eq!(received_id(&mut server).await, json!(1));

        let entries = captured.entries();
        let count = |level| {
            entries
                .iter()
                .filter(|(l, m)| *l == level && m.contains("stdio message rejected"))
                .count()
        };
        assert_eq!(count(tracing::Level::WARN), 1);
        assert_eq!(count(tracing::Level::DEBUG), 2);
    }

    fn reply_of(cause: RejectCause, outcome: ScanOutcome) -> Option<(i32, Option<RequestId>)> {
        Rejected {
            cause,
            outcome,
            bytes: 0,
        }
        .reply(10)
        .map(|(error, id)| (error.code.0, id))
    }

    #[test]
    fn test_reply_table_for_every_cause_and_outcome() {
        let unparsable =
            || RejectCause::Unparsable(serde_json::from_str::<Value>("{").unwrap_err());
        let not_json_rpc =
            || RejectCause::NotJsonRpc(serde_json::from_str::<u8>("\"x\"").unwrap_err());
        let request = || ScanOutcome::Request(Some(RequestId::Number(3)));
        let id = Some(RequestId::Number(3));

        assert_eq!(
            reply_of(RejectCause::Oversized, request()),
            Some((-32600, id.clone()))
        );
        assert_eq!(
            reply_of(unparsable(), request()),
            Some((-32700, id.clone()))
        );
        assert_eq!(
            reply_of(RejectCause::Skipped, request()),
            Some((-32600, id.clone()))
        );
        assert_eq!(reply_of(not_json_rpc(), request()), Some((-32600, id)));
        let without_request: [fn() -> ScanOutcome; 3] = [
            || ScanOutcome::IdOnly,
            || ScanOutcome::NoReply,
            || ScanOutcome::Unstructured,
        ];
        for outcome in without_request {
            assert_eq!(reply_of(RejectCause::Oversized, outcome()), None);
            assert_eq!(reply_of(RejectCause::Skipped, outcome()), None);
            assert_eq!(reply_of(unparsable(), outcome()), None);
            assert_eq!(reply_of(not_json_rpc(), outcome()), Some((-32600, None)));
        }
    }

    #[test]
    fn test_scan_outcome_classification() {
        let scan = |bytes: &[u8]| {
            let mut scan = EnvelopeScan::default();
            scan.feed(bytes);
            ScanOutcome::of_scan(&scan)
        };
        let request = |id: i64| ScanOutcome::Request(Some(RequestId::Number(id)));
        assert_eq!(scan(br#"{"id":7,"method":"m"}"#), request(7));
        assert_eq!(scan(br#"{"id":7,"method":"m","#), request(7));
        assert_eq!(scan(br#"{"id":7,"method":"m""#), request(7));
        assert_eq!(
            scan(br#"{"method":"m","id":1.5}"#),
            ScanOutcome::Request(None)
        );
        assert_eq!(scan(br#"{"method":"m","id":7"#), ScanOutcome::Unstructured);
        assert_eq!(scan(br#"{"id":7,"method":"m" x}"#), request(7));
        assert_eq!(scan(br#"{"id":7,"#), ScanOutcome::Unstructured);
        assert_eq!(scan(br#"{"method":"m"}"#), ScanOutcome::NoReply);
        assert_eq!(scan(br#"{"id":3,"result":{}}"#), ScanOutcome::NoReply);
        assert_eq!(scan(br#"{"id":3,"error":{}}"#), ScanOutcome::NoReply);
        assert_eq!(scan(br#"{"id":3}"#), ScanOutcome::IdOnly);
        assert_eq!(scan(br#"{"jsonrpc":"2.0"}"#), ScanOutcome::Unstructured);
        assert_eq!(scan(b"[1,2]"), ScanOutcome::Unstructured);
        assert_eq!(scan(b""), ScanOutcome::Unstructured);
    }

    #[tokio::test(start_paused = true)]
    async fn test_request_broken_after_its_method_and_id_is_answered() {
        let (mut server, mut reader, mut writer) = link(1024, 64 * 1024);
        let lines = [
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/list" "params":{}}"#,
            r#"{"jsonrpc":"2.0","id":8,"method":"x",params:{}}"#,
        ];
        writer
            .write_all(format!("{}\n{}\n", lines.join("\n"), ping(9)).as_bytes())
            .await
            .unwrap();

        assert_eq!(received_id(&mut server).await, json!(9));
        for id in [7, 8] {
            let reply = next_json(&mut reader).await;
            assert_eq!(reply["id"], json!(id));
            assert_eq!(reply["error"]["code"], json!(PARSE_ERROR));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_line_ending_does_not_change_the_answer_for_a_number_id() {
        let (mut server, mut reader, mut writer) = link(1024, 64 * 1024);
        let cut = r#"{"jsonrpc":"2.0","method":"tools/call","id":12"#;
        writer
            .write_all(format!("{cut}\n{cut}\r\n{}\n", ping(9)).as_bytes())
            .await
            .unwrap();

        assert_eq!(received_id(&mut server).await, json!(9));
        for _ in 0..2 {
            let reply = next_json(&mut reader).await;
            assert_eq!(reply["id"], json!(12));
            assert_eq!(reply["error"]["code"], json!(PARSE_ERROR));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_request_skipped_by_the_codec_is_answered_and_a_notification_is_not() {
        let (mut server, mut reader, mut writer) = link(1024, 64 * 1024);
        let lines = format!(
            "{}\n{}\n{}\n",
            r#"{"jsonrpc":"2.0","method":"notifications/x","params":5}"#,
            r#"{"id":5,"method":"notifications/x","params":5}"#,
            ping(9)
        );
        writer.write_all(lines.as_bytes()).await.unwrap();

        assert_eq!(received_id(&mut server).await, json!(9));
        let reply = next_json(&mut reader).await;
        assert_eq!(reply["id"], json!(5));
        assert_eq!(reply["error"]["code"], json!(INVALID_REQUEST));
        assert_no_reply(&mut reader).await;
    }
}
