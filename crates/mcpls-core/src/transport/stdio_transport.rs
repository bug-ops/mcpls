//! A stdio [`Transport`] with an mcpls-owned message-size limit.
//!
//! `rmcp`'s own stdio framing silently drops a line over its internal cap
//! before parsing it, so the client never learns that its request was lost.
//! [`StdioTransport`] bounds the line itself and, for an oversized request,
//! recovers the request `id` with a streaming [`TopLevelScan`] and answers
//! with a JSON-RPC `-32600` error. Lines within the limit are decoded by
//! `rmcp`'s own codec, so every other framing rule stays `rmcp`'s.

use std::num::NonZeroUsize;

use rmcp::RoleServer;
use rmcp::model::{ErrorData, RequestId};
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::async_rw::{AsyncRwTransport, JsonRpcMessageCodec, JsonRpcMessageCodecError};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, BufReader, Empty};
use tokio::task::JoinHandle;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::Decoder as _;

const MAX_ID_BYTES: usize = 128;
const KEY_BYTES: usize = 6;
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Non-zero cap, in bytes, on one incoming stdio JSON-RPC line.
///
/// The count covers the line without its terminating `\n`; a `\r` before it
/// counts. A line of exactly the limit is accepted, one byte more is rejected.
/// A zero cap would reject every message, so it is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdioMessageLimit(NonZeroUsize);

impl StdioMessageLimit {
    /// 4 MiB, the same bound the HTTP transport applies to a request body.
    pub(crate) const DEFAULT: Self = match NonZeroUsize::new(4 * 1024 * 1024) {
        Some(limit) => Self(limit),
        None => unreachable!(),
    };

    /// The limit in bytes.
    pub(crate) const fn get(self) -> usize {
        self.0.get()
    }

    /// A limit of `bytes`, or `None` for zero.
    #[cfg(test)]
    pub(crate) const fn new(bytes: usize) -> Option<Self> {
        match NonZeroUsize::new(bytes) {
            Some(limit) => Some(Self(limit)),
            None => None,
        }
    }
}

/// Which top-level object member the scanner is positioned on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Id,
    Method,
    Other,
}

/// A top-level key, kept only as far as it can still equal `id` or `method`.
#[derive(Debug, Default)]
struct KeyBuf {
    bytes: [u8; KEY_BYTES],
    len: usize,
    matchable: bool,
}

impl KeyBuf {
    fn new() -> Self {
        Self {
            matchable: true,
            ..Self::default()
        }
    }

    fn push(&mut self, byte: u8) {
        match self.bytes.get_mut(self.len) {
            Some(cell) => {
                *cell = byte;
                self.len = self.len.saturating_add(1);
            }
            None => self.matchable = false,
        }
    }

    fn slot(&self) -> Slot {
        let key = self.bytes.get(..self.len).unwrap_or_default();
        match (self.matchable, key) {
            (true, b"id") => Slot::Id,
            (true, b"method") => Slot::Method,
            _ => Slot::Other,
        }
    }
}

/// The raw token of the top-level `id` member.
#[derive(Debug)]
enum IdCapture {
    Absent,
    Raw(Vec<u8>),
    Unusable,
}

impl IdCapture {
    fn begin(&mut self) {
        *self = match self {
            Self::Absent => Self::Raw(Vec::new()),
            Self::Raw(_) | Self::Unusable => Self::Unusable,
        };
    }

    fn push(&mut self, byte: u8) {
        if let Self::Raw(raw) = self {
            if raw.len() < MAX_ID_BYTES {
                raw.push(byte);
            } else {
                *self = Self::Unusable;
            }
        }
    }

    fn mark_unusable(&mut self) {
        *self = Self::Unusable;
    }

    fn resolve(&self) -> Option<RequestId> {
        match self {
            Self::Raw(raw) => serde_json::from_slice(raw).ok(),
            Self::Absent | Self::Unusable => None,
        }
    }
}

#[derive(Debug)]
enum Phase {
    BeforeRoot,
    ExpectKey,
    InKey(KeyBuf),
    AfterKey(Slot),
    ValueStart(Slot),
    InValue(Slot),
    AfterValue,
    Finished,
    Malformed,
}

/// What an oversized line asks of the transport.
#[derive(Debug, PartialEq, Eq)]
enum ScanOutcome {
    /// A request; the id is `None` when it cannot be echoed.
    Request(Option<RequestId>),
    /// A notification, a client response, or not a JSON object at all.
    NoReply,
}

/// Streaming scanner for the top-level `id` and `method` members of one JSON
/// object, in constant memory whatever the object's size.
///
/// It never panics on malformed input. Only a cleanly closed top-level object
/// can ask for a reply, and the id is echoed only when its whole token was
/// captured at depth one, so a nested `id` inside `params` is never mistaken
/// for the request id. A leading byte order mark is skipped; member order does
/// not matter. A key written with escapes is never matched as `id` or `method`.
#[derive(Debug)]
struct TopLevelScan {
    phase: Phase,
    nested: usize,
    in_string: bool,
    escaped: bool,
    id: IdCapture,
    method_seen: bool,
}

impl Default for TopLevelScan {
    fn default() -> Self {
        Self {
            phase: Phase::BeforeRoot,
            nested: 0,
            in_string: false,
            escaped: false,
            id: IdCapture::Absent,
            method_seen: false,
        }
    }
}

const fn is_json_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

impl TopLevelScan {
    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if matches!(self.phase, Phase::Finished | Phase::Malformed) {
                return;
            }
            self.step(byte);
        }
    }

    fn finish(&self) -> ScanOutcome {
        let has_id = !matches!(self.id, IdCapture::Absent);
        if matches!(self.phase, Phase::Finished) && self.method_seen && has_id {
            ScanOutcome::Request(self.id.resolve())
        } else {
            ScanOutcome::NoReply
        }
    }

    fn step(&mut self, byte: u8) {
        match &mut self.phase {
            Phase::BeforeRoot => match byte {
                b'{' => self.phase = Phase::ExpectKey,
                _ if is_json_space(byte) || BOM.contains(&byte) => {}
                _ => self.phase = Phase::Malformed,
            },
            Phase::ExpectKey => match byte {
                b'"' => {
                    self.escaped = false;
                    self.phase = Phase::InKey(KeyBuf::new());
                }
                b'}' => self.phase = Phase::Finished,
                _ if is_json_space(byte) => {}
                _ => self.phase = Phase::Malformed,
            },
            Phase::InKey(key) => {
                if self.escaped {
                    self.escaped = false;
                    key.matchable = false;
                } else {
                    match byte {
                        b'\\' => self.escaped = true,
                        b'"' => {
                            let slot = key.slot();
                            self.phase = Phase::AfterKey(slot);
                        }
                        _ => key.push(byte),
                    }
                }
            }
            Phase::AfterKey(slot) => {
                let slot = *slot;
                match byte {
                    b':' => self.phase = Phase::ValueStart(slot),
                    _ if is_json_space(byte) => {}
                    _ => self.phase = Phase::Malformed,
                }
            }
            Phase::ValueStart(slot) => {
                if !is_json_space(byte) {
                    let slot = *slot;
                    self.begin_value(slot);
                    self.phase = Phase::InValue(slot);
                    self.value_byte(slot, byte);
                }
            }
            Phase::InValue(slot) => {
                let slot = *slot;
                self.value_byte(slot, byte);
            }
            Phase::AfterValue => match byte {
                b',' => self.phase = Phase::ExpectKey,
                b'}' => self.phase = Phase::Finished,
                _ if is_json_space(byte) => {}
                _ => self.phase = Phase::Malformed,
            },
            Phase::Finished | Phase::Malformed => {}
        }
    }

    fn begin_value(&mut self, slot: Slot) {
        self.nested = 0;
        self.in_string = false;
        self.escaped = false;
        match slot {
            Slot::Id => self.id.begin(),
            Slot::Method => self.method_seen = true,
            Slot::Other => {}
        }
    }

    fn value_byte(&mut self, slot: Slot, byte: u8) {
        let capture = slot == Slot::Id;
        if self.in_string {
            if capture {
                self.id.push(byte);
            }
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.in_string = false;
                if self.nested == 0 {
                    self.phase = Phase::AfterValue;
                }
            }
            return;
        }
        match byte {
            b'"' => {
                self.in_string = true;
                if capture {
                    self.id.push(byte);
                }
            }
            b'{' | b'[' => {
                self.nested = self.nested.saturating_add(1);
                if capture {
                    self.id.mark_unusable();
                }
            }
            b'}' | b']' if self.nested == 0 => {
                self.phase = if byte == b'}' {
                    Phase::Finished
                } else {
                    Phase::Malformed
                };
            }
            b'}' | b']' => {
                self.nested = self.nested.saturating_sub(1);
                if self.nested == 0 {
                    self.phase = Phase::AfterValue;
                }
            }
            b',' if self.nested == 0 => self.phase = Phase::ExpectKey,
            _ if self.nested == 0 && is_json_space(byte) => self.phase = Phase::AfterValue,
            _ => {
                if capture && self.nested == 0 {
                    self.id.push(byte);
                }
            }
        }
    }
}

#[derive(Debug)]
struct Rejected {
    outcome: ScanOutcome,
    bytes: usize,
}

#[derive(Debug)]
enum ReadState {
    Buffering,
    Discarding { scan: TopLevelScan, bytes: usize },
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
/// [`TopLevelScan`]; the stream stays in sync for the next line. Replies to
/// rejected input are written by a spawned task, because the service loop
/// drops a pending `receive` whenever an outgoing message is ready, and a
/// reply awaited inside it would be lost once its line was consumed. At most
/// one such reply is in flight: `receive` settles it before reading the next
/// line, which keeps the backpressure a peer that stops reading stdout got
/// from an inline write.
pub struct StdioTransport<R, W: AsyncWrite> {
    read: BufReader<R>,
    line: BytesMut,
    state: ReadState,
    limit: StdioMessageLimit,
    codec: JsonRpcMessageCodec<RxJsonRpcMessage<RoleServer>>,
    writer: AsyncRwTransport<RoleServer, Empty, W>,
    reply: Option<JoinHandle<()>>,
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
                            let mut scan = TopLevelScan::default();
                            scan.feed(&self.line);
                            scan.feed(content);
                            self.line.clear();
                            if ended {
                                Step::Rejected(Rejected {
                                    outcome: scan.finish(),
                                    bytes: total,
                                })
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
                            let outcome = scan.finish();
                            let bytes = *bytes;
                            self.state = ReadState::Buffering;
                            Step::Rejected(Rejected { outcome, bytes })
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
            if let Err(e) = send.await {
                tracing::debug!("stdio reply not delivered: {e}");
            }
        }));
    }

    /// Waits for the in-flight reply, if any. Cancel-safe: the task is
    /// independent of this future and the handle stays until it completes.
    async fn settle_reply(&mut self) {
        if let Some(handle) = &mut self.reply {
            if let Err(e) = handle.await {
                tracing::debug!("stdio reply task failed: {e}");
            }
            self.reply = None;
        }
    }

    fn reject(&mut self, rejected: Rejected) {
        let limit = self.limit.get();
        let Rejected { outcome, bytes } = rejected;
        let (id, replies) = match outcome {
            ScanOutcome::Request(id) => (id, true),
            ScanOutcome::NoReply => (None, false),
        };
        tracing::warn!(
            request_id = ?id,
            bytes,
            limit,
            replied = replies,
            "stdio message exceeds the size limit, discarding it"
        );
        if replies {
            let error = ErrorData::invalid_request(
                format!("request exceeds the {limit}-byte stdio message limit"),
                None,
            );
            self.spawn_reply(TxJsonRpcMessage::<RoleServer>::error(error, id));
        }
    }
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
                    self.reject(rejected);
                    continue;
                }
                LineRead::Eof => return None,
                LineRead::Io(e) => {
                    tracing::error!("error reading from stdin: {e}");
                    return None;
                }
            }
            if self.line.is_empty() || self.line.as_ref() == b"\r" {
                self.line.clear();
                continue;
            }
            self.line.extend_from_slice(b"\n");
            let decoded = self.codec.decode(&mut self.line);
            self.line.clear();
            match decoded {
                Ok(Some(message)) => return Some(message),
                Ok(None) => {}
                Err(JsonRpcMessageCodecError::Serde(e)) => match e.classify() {
                    serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
                        tracing::debug!("ignoring unparsable incoming message: {e}");
                    }
                    serde_json::error::Category::Data | serde_json::error::Category::Io => {
                        tracing::debug!("protocol error on incoming message: {e}");
                        let error = ErrorData::invalid_request("Invalid request", None);
                        self.spawn_reply(TxJsonRpcMessage::<RoleServer>::error(error, None));
                    }
                },
                Err(e) => {
                    tracing::error!("error decoding stdin: {e}");
                    return None;
                }
            }
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

    type Server = StdioTransport<ReadHalf<DuplexStream>, WriteHalf<DuplexStream>>;
    type ClientReader = BufReader<ReadHalf<DuplexStream>>;
    type ClientWriter = WriteHalf<DuplexStream>;

    #[derive(Clone, Copy)]
    enum Id {
        Number(i64),
        Text(&'static str),
        Unusable,
    }

    fn scan(line: &[u8]) -> ScanOutcome {
        let mut scan = TopLevelScan::default();
        scan.feed(line);
        scan.finish()
    }

    fn request(id: Id) -> ScanOutcome {
        ScanOutcome::Request(match id {
            Id::Number(n) => Some(RequestId::Number(n)),
            Id::Text(s) => Some(RequestId::String(s.into())),
            Id::Unusable => None,
        })
    }

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

    #[test]
    fn test_scan_finds_id_in_any_member_order() {
        let first = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{}}"#;
        let last = br#"{"jsonrpc":"2.0","method":"tools/call","params":{"x":[1,{"y":2}]},"id":8}"#;
        let spaced = b"{ \"id\" : 4 , \"method\" : \"m\" }\r";
        assert_eq!(scan(first), request(Id::Number(7)));
        assert_eq!(scan(last), request(Id::Number(8)));
        assert_eq!(scan(spaced), request(Id::Number(4)));
    }

    #[test]
    fn test_scan_ignores_nested_id_and_method() {
        let nested = br#"{"params":{"id":99,"method":"n","s":"\"id\":5"},"jsonrpc":"2.0"}"#;
        let with_outer_id = br#"{"params":{"id":99,"a":[{"id":1}]},"method":"m","id":3}"#;
        assert_eq!(scan(nested), ScanOutcome::NoReply);
        assert_eq!(scan(with_outer_id), request(Id::Number(3)));
    }

    #[test]
    fn test_scan_decodes_string_ids_with_escapes() {
        let line = br#"{"method":"m","id":"a\"b\\c"}"#;
        assert_eq!(scan(line), request(Id::Text("a\"b\\c")));
    }

    #[test]
    fn test_scan_skips_a_leading_bom() {
        let mut line = BOM.to_vec();
        line.extend_from_slice(br#"{"id":2,"method":"m"}"#);
        assert_eq!(scan(&line), request(Id::Number(2)));
    }

    #[test]
    fn test_scan_unusable_ids_get_an_id_less_request() {
        let long = format!(r#"{{"method":"m","id":"{}"}}"#, "i".repeat(MAX_ID_BYTES));
        let cases: [Vec<u8>; 7] = [
            br#"{"method":"m","id":1.5}"#.to_vec(),
            br#"{"method":"m","id":null}"#.to_vec(),
            br#"{"method":"m","id":true}"#.to_vec(),
            br#"{"method":"m","id":{"a":1}}"#.to_vec(),
            br#"{"method":"m","id":[1]}"#.to_vec(),
            br#"{"method":"m","id":1,"id":2}"#.to_vec(),
            long.into_bytes(),
        ];
        for case in cases {
            assert_eq!(
                scan(&case),
                request(Id::Unusable),
                "{}",
                String::from_utf8_lossy(&case)
            );
        }
    }

    #[test]
    fn test_scan_id_just_under_the_length_cap_is_captured() {
        let id = "i".repeat(MAX_ID_BYTES - 2);
        let line = format!(r#"{{"method":"m","id":"{id}"}}"#);
        assert_eq!(
            scan(line.as_bytes()),
            ScanOutcome::Request(Some(RequestId::String(id.into())))
        );
    }

    #[test]
    fn test_scan_gives_no_reply_to_notifications_responses_and_garbage() {
        let cases: [&[u8]; 8] = [
            br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            br#"{"jsonrpc":"2.0","id":3,"result":{}}"#,
            br#"{"method":"m","id":1"#,
            b"not json at all",
            b"[1,2,3]",
            b"",
            br#"{"method":"m","id" 1}"#,
            b"{\"\\u0069d\":1,\"method\":\"m\"}",
        ];
        for case in cases {
            assert_eq!(
                scan(case),
                ScanOutcome::NoReply,
                "{}",
                String::from_utf8_lossy(case)
            );
        }
    }

    #[test]
    fn test_scan_result_is_independent_of_chunking() {
        let line =
            br#"{"jsonrpc":"2.0","method":"tools/call","params":{"a":"\"}","b":[1]},"id":"x\\y"}"#;
        let whole = scan(line);
        assert_eq!(whole, request(Id::Text("x\\y")));
        for split in 0..=line.len() {
            let (head, tail) = line.split_at(split);
            let mut chunked = TopLevelScan::default();
            chunked.feed(head);
            chunked.feed(tail);
            assert_eq!(chunked.finish(), whole, "split at {split}");
        }
    }

    #[test]
    fn test_scan_survives_every_prefix_and_junk_bytes() {
        let line = br#"{"jsonrpc":"2.0","method":"m","params":{"a":[1,"}"]},"id":5}"#;
        for end in 0..=line.len() {
            let _ = scan(&line[..end]);
        }
        for byte in 0..=u8::MAX {
            let _ = scan(&[b'{', b'"', b'i', b'd', b'"', b':', byte, byte, b'}', byte]);
        }
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

    fn json_value() -> impl proptest::strategy::Strategy<Value = Value> {
        use proptest::prelude::*;

        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            (-1.0e6_f64..1.0e6).prop_map(Value::from),
            "\\PC{0,8}".prop_map(Value::from),
        ];
        leaf.prop_recursive(3, 24, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::vec(("[a-z]{0,4}", inner), 0..4)
                    .prop_map(|members| Value::Object(members.into_iter().collect())),
            ]
        })
    }

    fn expected_outcome(object: &serde_json::Map<String, Value>) -> ScanOutcome {
        match (object.get("id"), object.contains_key("method")) {
            (Some(id), true) => {
                ScanOutcome::Request(serde_json::from_value::<RequestId>(id.clone()).ok())
            }
            _ => ScanOutcome::NoReply,
        }
    }

    proptest::proptest! {
        #[test]
        fn prop_scan_agrees_with_serde_json_on_generated_objects(
            id in proptest::option::of(json_value()),
            method in proptest::option::of(json_value()),
            extra in proptest::collection::vec(("[a-z]{1,4}", json_value()), 0..4),
            rotation in 0_usize..8,
            spacing in proptest::sample::select(vec!["", " ", "\t", "\r"]),
            split in 0_usize..400,
        ) {
            let mut object: serde_json::Map<String, Value> = extra.into_iter().collect();
            if let Some(id) = id {
                object.insert("id".to_owned(), id);
            }
            if let Some(method) = method {
                object.insert("method".to_owned(), method);
            }
            let mut members: Vec<(String, Value)> =
                object.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            let rotate_by = rotation.checked_rem(members.len().max(1)).unwrap_or(0);
            members.rotate_left(rotate_by);
            let body = members
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}{spacing}:{spacing}{}",
                        serde_json::to_string(k).unwrap(),
                        serde_json::to_string(v).unwrap()
                    )
                })
                .collect::<Vec<_>>()
                .join(&format!("{spacing},{spacing}"));
            let line = format!("{{{spacing}{body}{spacing}}}");
            let expected = expected_outcome(&object);

            proptest::prop_assert_eq!(scan(line.as_bytes()), expected);

            let cut = split.min(line.len());
            let (head, tail) = line.as_bytes().split_at(cut);
            let mut chunked = TopLevelScan::default();
            chunked.feed(head);
            chunked.feed(tail);
            proptest::prop_assert_eq!(chunked.finish(), expected_outcome(&object));
        }
    }

    #[test]
    fn test_scan_escaped_method_key_is_not_recognized() {
        let line = br#"{"id":1,"\u006dethod":"m"}"#;
        assert_eq!(scan(line), ScanOutcome::NoReply);
    }

    #[test]
    fn test_scan_escaped_unrelated_key_does_not_hide_the_id() {
        let line = br#"{"id":1,"\u0078":2,"method":"m"}"#;
        assert_eq!(scan(line), request(Id::Number(1)));
    }
}
