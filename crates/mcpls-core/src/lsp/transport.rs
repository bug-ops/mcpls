//! LSP transport layer for stdio communication.
//!
//! This module implements the LSP header-content message format over stdin/stdout.
//! Messages follow the format:
//! ```text
//! Content-Length: 123\r\n
//! \r\n
//! {"jsonrpc":"2.0",...}
//! ```

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use lsp_types::Uri;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::{Level, debug, trace, warn};

use crate::error::{Error, RedactedText, Result};
use crate::json_envelope::{Envelope, EnvelopeScan, Members, MessageKind, Shape};
use crate::lsp::types::{
    BoundedUri, DroppedNotification, InboundMessage, JsonRpcNotification, JsonRpcVersion,
    LostPublish, RequestId, UndecodableReason, is_publish_diagnostics,
};
use crate::redaction::Redactions;
use crate::util::{DropCounter, WarnLimiter};

/// Largest frame that is buffered and decoded (10 MiB).
pub const MAX_CONTENT_LENGTH: usize = 10 * 1024 * 1024;

/// Largest frame that is skipped in constant memory when it exceeds
/// [`MAX_CONTENT_LENGTH`]. A frame announced above this is implausible and stays
/// fatal, so a server that streams forever is still torn down.
const MAX_SKIPPED_CONTENT_LENGTH: usize = 256 * 1024 * 1024;

/// Maximum length of a single header line, including its terminating `\n`
/// (#457). Bounds `read_headers` against a spawned server that writes one
/// endless line with no `\n` -- without this, `read_line` would grow its
/// buffer without limit.
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// Maximum number of header lines read per frame before the terminating
/// blank line (#457). Bounds `read_headers` against a spawned server that
/// emits endlessly many distinct header lines.
const MAX_HEADERS: usize = 100;

/// Write half of the LSP transport, handling the header-content format for
/// outbound messages.
///
/// Boxes its writer as a trait object rather than carrying it as a type
/// parameter: [`Self::new`] accepts any `AsyncWrite`/`AsyncRead` pair (a
/// spawned LSP server's `ChildStdin`/`ChildStdout` in production, an
/// in-memory `tokio::io::duplex` pipe in tests -- see `crate::test_lsp`),
/// so `LspClient` and `LspServer` don't need to become generic over the
/// underlying transport just to support both.
///
/// Split from the read half ([`LspTransportReader`]) by [`Self::new`]: the
/// read side must be driven exclusively by a dedicated background task
/// (see `lsp::client::spawn_reader_task` -- a private free function, not an
/// intra-doc link here since it isn't part of the public API), never raced
/// inside a `tokio::select!` alongside outbound sends -- see
/// [`LspTransportReader`] for why.
pub struct LspTransport {
    stdin: Box<dyn AsyncWrite + Unpin + Send>,
    redactions: Arc<Redactions>,
}

impl fmt::Debug for LspTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LspTransport").finish_non_exhaustive()
    }
}

/// Read half of the LSP transport, handling the header-content format for
/// inbound messages.
///
/// Not cancel-safe: [`Self::receive`] reads headers and content into local
/// buffers across multiple `.await` points, and dropping it mid-read
/// discards those buffers while the underlying `BufReader` has already
/// consumed the corresponding bytes from the pipe -- permanently
/// desynchronizing the Content-Length-framed stream (#451). Callers must
/// drive it from a task of its own that owns it exclusively and is never
/// raced in a `tokio::select!` against another branch; see
/// `lsp::client::spawn_reader_task`, the only production caller.
///
/// # Examples
///
/// ```
/// use mcpls_core::lsp::LspTransport;
///
/// tokio::runtime::Runtime::new().unwrap().block_on(async {
///     let (mut writer, mut reader) = LspTransport::new(tokio::io::sink(), tokio::io::empty());
///     writer
///         .send(&serde_json::json!({"jsonrpc": "2.0", "method": "exit"}))
///         .await
///         .unwrap();
///
///     // An empty `stdout` yields EOF immediately, which `receive` reports
///     // as `Error::ServerTerminated` rather than hanging.
///     assert!(reader.receive().await.is_err());
/// });
/// ```
pub struct LspTransportReader {
    stdout: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    redactions: Arc<Redactions>,
    malformed_header_warn: WarnLimiter,
    skipped_frames: DropCounter,
}

impl fmt::Debug for LspTransportReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LspTransportReader").finish_non_exhaustive()
    }
}

impl LspTransport {
    /// Create a transport from a reader/writer pair, split into its write
    /// half (this type) and its read half ([`LspTransportReader`]).
    ///
    /// # Arguments
    ///
    /// * `stdin` - Where to write outbound messages (a spawned server's
    ///   stdin in production)
    /// * `stdout` - Where to read inbound messages from (a spawned server's
    ///   stdout in production)
    #[must_use]
    pub fn new(
        stdin: impl AsyncWrite + Unpin + Send + 'static,
        stdout: impl AsyncRead + Unpin + Send + 'static,
    ) -> (Self, LspTransportReader) {
        Self::with_redactions(stdin, stdout, Arc::default())
    }

    /// As [`Self::new`], redacting `redactions` from the wire frames both
    /// halves log at `trace` level.
    pub(crate) fn with_redactions(
        stdin: impl AsyncWrite + Unpin + Send + 'static,
        stdout: impl AsyncRead + Unpin + Send + 'static,
        redactions: Arc<Redactions>,
    ) -> (Self, LspTransportReader) {
        (
            Self {
                stdin: Box::new(stdin),
                redactions: Arc::clone(&redactions),
            },
            LspTransportReader {
                stdout: BufReader::new(Box::new(stdout)),
                redactions,
                malformed_header_warn: WarnLimiter::default(),
                skipped_frames: DropCounter::new(),
            },
        )
    }

    /// Send message to LSP server.
    ///
    /// Formats the message with proper Content-Length header and sends it
    /// to the LSP server via stdin.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Message serialization fails
    /// - Writing to stdin fails
    /// - Flushing stdin fails
    pub async fn send(&mut self, message: &Value) -> Result<()> {
        let content = serde_json::to_string(message)?;
        let header = format!("Content-Length: {}\r\n\r\n", content.len());

        if tracing::enabled!(Level::TRACE) {
            trace!("Sending LSP message: {}", self.redactions.apply(&content));
        }

        self.stdin.write_all(header.as_bytes()).await?;
        self.stdin.write_all(content.as_bytes()).await?;
        self.stdin.flush().await?;

        Ok(())
    }
}

impl LspTransportReader {
    /// Receive next message from LSP server.
    ///
    /// Reads headers, extracts Content-Length, reads exact message content,
    /// and parses it as a response, request or notification.
    ///
    /// A frame whose body cannot be decoded (not JSON or not UTF-8, nested past
    /// the parser's limit, a malformed message) does not fail the stream: it is returned as
    /// one of the `Undecodable*` messages, which say what was lost so the
    /// caller can fail one request or drop one notification and read on. A
    /// frame larger than the buffering limit, up to 256 MiB, is skipped without
    /// being buffered and reported the same way.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Reading headers fails
    /// - Content-Length header is missing or invalid, or announces more than 256 MiB
    /// - Reading message content fails, or the stream ends inside a frame
    /// - The stream ends
    pub async fn receive(&mut self) -> Result<InboundMessage> {
        loop {
            let headers = self.read_headers().await?;

            let content_length = headers
                .get("content-length")
                .ok_or_else(|| {
                    Error::LspProtocolError(RedactedText::fixed("Missing Content-Length header"))
                })?
                .parse::<usize>()
                .map_err(|e| {
                    self.redactions
                        .protocol_error(format_args!("Invalid Content-Length: {e}"))
                })?;

            if content_length > MAX_SKIPPED_CONTENT_LENGTH {
                return Err(self.redactions.protocol_error(format_args!(
                    "Content-Length {content_length} exceeds maximum allowed size of {MAX_SKIPPED_CONTENT_LENGTH} bytes"
                )));
            }

            if content_length > MAX_CONTENT_LENGTH {
                let envelope = self.skip_content(content_length).await?;
                if let Some(suppressed) = self.skipped_frames.record(Instant::now()) {
                    warn!(
                        bytes = content_length,
                        limit = MAX_CONTENT_LENGTH,
                        suppressed,
                        "skipped an oversized LSP frame"
                    );
                } else {
                    debug!(
                        bytes = content_length,
                        limit = MAX_CONTENT_LENGTH,
                        "skipped an oversized LSP frame"
                    );
                }
                return Ok(oversized_message(&envelope, content_length));
            }

            let content = self.read_content(content_length).await?;

            if tracing::enabled!(Level::TRACE) {
                trace!(
                    "Received LSP message: {}",
                    self.redactions.apply(&String::from_utf8_lossy(&content))
                );
            }

            let value: Value = match serde_json::from_slice(&content) {
                Ok(value) => value,
                Err(error) => {
                    debug!(
                        "{}",
                        self.redactions
                            .protocol_error(format_args!("Invalid JSON: {error}"))
                    );
                    return Ok(undecodable_message(&content));
                }
            };

            // Some servers (notably OmniSharp) occasionally emit a bare `null`
            // (or other non-object) JSON-RPC message. Skip it and read the next
            // framed message instead of killing the whole message loop.
            if !value.is_object() {
                // Some servers (notably OmniSharp) emit a burst of these during
                // startup; log at debug to avoid flooding the logs for what is a
                // recoverable, expected condition.
                debug!(
                    "Skipping non-object LSP message: {}",
                    self.redactions.apply(&value.to_string())
                );
                continue;
            }

            return Ok(parse_inbound_message(value, &self.redactions));
        }
    }

    /// Read headers until blank line.
    ///
    /// Headers are in the format "Key: Value\r\n" and are terminated by
    /// a blank line ("\r\n").
    async fn read_headers(&mut self) -> Result<HashMap<String, String>> {
        let mut headers = HashMap::new();
        let mut line = String::new();
        let mut lines_read = 0usize;

        loop {
            line.clear();
            let bytes_read = (&mut self.stdout)
                .take(MAX_HEADER_LINE_BYTES as u64)
                .read_line(&mut line)
                .await?;

            // EOF - stream closed (read_line returns 0 bytes on EOF)
            if bytes_read == 0 || line.is_empty() {
                trace!(
                    "EOF detected in read_headers: bytes_read={}, line_len={}",
                    bytes_read,
                    line.len()
                );
                return Err(Error::ServerTerminated);
            }

            // The `take` limit was hit with no line ending, as opposed to a
            // short final line truncated by a genuine EOF (caught above on
            // the next iteration) -- see `MAX_HEADER_LINE_BYTES`.
            if bytes_read == MAX_HEADER_LINE_BYTES && !line.ends_with('\n') {
                return Err(self.redactions.protocol_error(format_args!(
                    "LSP header line exceeded {MAX_HEADER_LINE_BYTES} bytes without a newline"
                )));
            }

            if line == "\r\n" || line == "\n" {
                break;
            }

            lines_read = lines_read.saturating_add(1);
            if lines_read > MAX_HEADERS {
                return Err(self.redactions.protocol_error(format_args!(
                    "LSP frame exceeded {MAX_HEADERS} header lines"
                )));
            }

            if let Some((key, value)) = line.trim_end().split_once(':') {
                headers.insert(key.trim().to_lowercase(), value.trim().to_string());
            } else {
                let shown =
                    crate::util::truncate_str(line.trim(), crate::util::MAX_LOG_STRING_BYTES);
                if self
                    .malformed_header_warn
                    .due(Instant::now(), WarnLimiter::DEFAULT_PERIOD)
                {
                    warn!("Malformed header: {shown}");
                } else {
                    debug!("Malformed header: {shown}");
                }
            }
        }

        Ok(headers)
    }

    /// Read exact number of content bytes.
    ///
    /// Reads exactly `length` bytes from stdout. They are not required to be
    /// UTF-8 here: JSON parsing rejects invalid text, and a frame that fails
    /// that way is dropped alone, without altering any text it carried.
    async fn read_content(&mut self, length: usize) -> Result<Vec<u8>> {
        let mut buffer = vec![0u8; length];
        self.stdout.read_exact(&mut buffer).await?;
        Ok(buffer)
    }

    /// Consumes exactly `length` content bytes, scanning their envelope in
    /// constant memory. Never reads past the frame, whatever the scan state:
    /// the next frame may already sit in the same buffer.
    ///
    /// # Errors
    ///
    /// [`Error::ServerTerminated`] when the stream ends inside the frame.
    async fn skip_content(&mut self, length: usize) -> Result<Envelope> {
        let mut scan = EnvelopeScan::with_params_uri();
        let mut remaining = length;
        while remaining > 0 {
            let available = self.stdout.fill_buf().await?;
            if available.is_empty() {
                return Err(Error::ServerTerminated);
            }
            let take = available.len().min(remaining);
            scan.feed(available.get(..take).unwrap_or_default());
            self.stdout.consume(take);
            remaining = remaining.saturating_sub(take);
        }
        Ok(scan.finish())
    }
}

/// The message to hand on for a frame whose body could not be decoded: what it
/// was and, for a request or response, whom it addresses, recovered without
/// building its body. The scan is tolerant, so what made the full decode fail
/// (nesting past the parser's recursion limit, a lone surrogate escape, an
/// out-of-range number) does not stop it.
fn undecodable_message(content: &[u8]) -> InboundMessage {
    let mut scan = EnvelopeScan::with_params_uri();
    scan.feed(content);
    dropped_message(&scan.finish(), UndecodableReason::Malformed)
}

/// The message for a frame that was skipped for its size, from the envelope
/// scanned while skipping it.
fn oversized_message(envelope: &Envelope, bytes: usize) -> InboundMessage {
    dropped_message(envelope, UndecodableReason::Oversized { bytes })
}

/// What a frame that could not be decoded was, from its scanned envelope. A
/// `publishDiagnostics` is recognized whatever the shape of the rest, since
/// losing one makes the cache stale; any other message needs a closed object.
fn dropped_message(envelope: &Envelope, reason: UndecodableReason) -> InboundMessage {
    let kind = envelope.kind();
    if kind == MessageKind::Notification
        && envelope
            .method
            .decode::<String>()
            .is_some_and(|method| is_publish_diagnostics(&method))
    {
        let lost = envelope
            .params_uri
            .decode::<Uri>()
            .and_then(BoundedUri::new)
            .map_or(LostPublish::Unidentified, LostPublish::File);
        return InboundMessage::UndecodableNotification(DroppedNotification::Publish(lost));
    }
    if envelope.shape != Shape::Closed {
        return InboundMessage::UndecodableFrame;
    }
    undecodable_of_kind(kind, envelope.id.decode(), reason)
}

fn undecodable_of_kind(
    kind: MessageKind,
    id: Option<RequestId>,
    reason: UndecodableReason,
) -> InboundMessage {
    match (kind, id) {
        (MessageKind::Request, Some(id)) => InboundMessage::UndecodableRequest { id, reason },
        (MessageKind::Notification, _) => {
            InboundMessage::UndecodableNotification(DroppedNotification::Other)
        }
        (MessageKind::Response | MessageKind::IncompleteResponse, Some(id)) => {
            InboundMessage::UndecodableResponse { id, reason }
        }
        _ => InboundMessage::UndecodableFrame,
    }
}

/// The request id `value` carries, when it is a number or a string.
fn request_id_of(value: &Value) -> Option<RequestId> {
    value
        .get("id")
        .and_then(|id| serde_json::from_value(id.clone()).ok())
}

/// The envelope of a notification, without its `params`.
#[derive(serde::Deserialize)]
struct NotificationHead {
    jsonrpc: JsonRpcVersion,
    method: String,
}

/// Decodes a notification object. Its `params` are moved out first, so the
/// message is built without copying them and a failed decode still knows which
/// file a `publishDiagnostics` was for.
fn decode_notification(mut value: Value, redactions: &Redactions) -> InboundMessage {
    let is_publish = value
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(is_publish_diagnostics);
    let params = value
        .get_mut("params")
        .map(Value::take)
        .filter(|params| !params.is_null());
    match decode::<NotificationHead>(value, "notification", redactions) {
        Some(NotificationHead { jsonrpc, method }) => {
            InboundMessage::Notification(JsonRpcNotification {
                jsonrpc,
                method,
                params,
            })
        }
        None if is_publish => InboundMessage::UndecodableNotification(
            DroppedNotification::Publish(LostPublish::of_params(params.as_ref())),
        ),
        None => InboundMessage::UndecodableNotification(DroppedNotification::Other),
    }
}

/// Decodes `value` as the message its keys say it is. A message that does not
/// decode becomes the matching `Undecodable*` message; what serde said is
/// logged redacted, since it can echo server-controlled text.
fn parse_inbound_message(value: Value, redactions: &Redactions) -> InboundMessage {
    let kind = MessageKind::of(Members::of_value(&value));
    let id = request_id_of(&value);
    let decoded = match kind {
        MessageKind::Request => decode(value, "request", redactions).map(InboundMessage::Request),
        MessageKind::Notification => return decode_notification(value, redactions),
        MessageKind::Response => {
            decode(value, "response", redactions).map(InboundMessage::Response)
        }
        MessageKind::IncompleteResponse | MessageKind::Unrecognized => None,
    };
    decoded.unwrap_or_else(|| {
        debug!("Dropping a message that is not a request, response or notification ({kind:?})");
        undecodable_of_kind(kind, id, UndecodableReason::Malformed)
    })
}

fn decode<T: serde::de::DeserializeOwned>(
    value: Value,
    what: &str,
    redactions: &Redactions,
) -> Option<T> {
    serde_json::from_value(value)
        .inspect_err(|error| {
            debug!(
                "{}",
                redactions.protocol_error(format_args!("Invalid {what}: {error}"))
            );
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::fmt::Write as _;

    use super::*;
    use crate::lsp::types::RequestId;

    #[tokio::test]
    async fn test_trace_wire_logs_redact_secrets_in_both_directions() {
        use tracing_subscriber::prelude::*;

        use crate::test_lsp::CapturedLogs;

        let secret = "pa\"ss\\word-12345";
        let redactions = Arc::new(Redactions::new([(
            "API_TOKEN".to_owned(),
            secret.to_owned(),
        )]));
        let logs = CapturedLogs::default();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::registry()
                .with(tracing_subscriber::filter::LevelFilter::TRACE)
                .with(logs.clone()),
        );
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "params": {"initializationOptions": {"token": secret}},
        });
        let body = frame.to_string();
        let inbound = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let (mut writer_half, mut reader_half) = LspTransport::with_redactions(
            tokio::io::sink(),
            std::io::Cursor::new(inbound.into_bytes()),
            redactions,
        );

        writer_half.send(&frame).await.unwrap();
        reader_half.receive().await.unwrap();

        let output = logs.messages().join("\n");
        assert!(output.contains("Sending LSP message"), "{output}");
        assert!(output.contains("Received LSP message"), "{output}");
        assert!(!output.contains("word-12345"), "{output}");
        assert_eq!(
            output.matches("[redacted:API_TOKEN]").count(),
            2,
            "{output}"
        );
    }

    #[tokio::test]
    async fn test_skipped_non_object_frame_log_redacts_secrets() {
        use tracing_subscriber::prelude::*;

        use crate::test_lsp::CapturedLogs;

        let redactions = Arc::new(Redactions::new([(
            "API_TOKEN".to_owned(),
            "SuperSecretValue123".to_owned(),
        )]));
        let logs = CapturedLogs::default();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::registry()
                .with(tracing_subscriber::filter::LevelFilter::DEBUG)
                .with(logs.clone()),
        );
        let body = "\"echo SuperSecretValue123\"";
        let inbound = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let (_, mut reader) = LspTransport::with_redactions(
            tokio::io::sink(),
            std::io::Cursor::new(inbound.into_bytes()),
            redactions,
        );

        assert!(reader.receive().await.is_err());

        let output = logs.messages().join("\n");
        assert!(output.contains("Skipping non-object"), "{output}");
        assert!(!output.contains("SuperSecretValue123"), "{output}");
    }

    #[test]
    fn test_header_parsing() {
        let headers_text = "Content-Length: 123\r\nContent-Type: application/json\r\n";
        let mut headers = HashMap::new();

        for line in headers_text.lines() {
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.trim().to_lowercase(), value.trim().to_string());
            }
        }

        assert_eq!(headers.get("content-length"), Some(&"123".to_string()));
        assert_eq!(
            headers.get("content-type"),
            Some(&"application/json".to_string())
        );
    }

    #[test]
    fn test_message_format() {
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });

        let content = serde_json::to_string(&message).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", content.len());

        assert!(header.starts_with("Content-Length:"));
        assert!(header.ends_with("\r\n\r\n"));
        assert!(content.contains("\"jsonrpc\":\"2.0\""));
    }

    #[test]
    fn test_header_case_insensitive() {
        let headers_text = "CONTENT-LENGTH: 123\r\nContent-Type: application/json\r\n";
        let mut headers = HashMap::new();

        for line in headers_text.lines() {
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.trim().to_lowercase(), value.trim().to_string());
            }
        }

        assert_eq!(headers.get("content-length"), Some(&"123".to_string()));
    }

    #[test]
    fn test_max_content_length_constant() {
        assert_eq!(MAX_CONTENT_LENGTH, 10 * 1024 * 1024);
    }

    #[test]
    fn test_header_format_with_multiple_headers() {
        let headers_text =
            "Content-Length: 42\r\nContent-Type: application/json\r\nX-Custom: value\r\n";
        let mut headers = HashMap::new();

        for line in headers_text.lines() {
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.trim().to_lowercase(), value.trim().to_string());
            }
        }

        assert_eq!(headers.len(), 3);
        assert_eq!(headers.get("content-length"), Some(&"42".to_string()));
        assert_eq!(headers.get("x-custom"), Some(&"value".to_string()));
    }

    #[test]
    fn test_message_serialization_response() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"key": "value"}
        });

        let content = serde_json::to_string(&response).unwrap();
        assert!(content.contains("\"jsonrpc\":\"2.0\""));
        assert!(content.contains("\"id\":1"));
        assert!(content.contains("\"result\""));
    }

    #[test]
    fn test_message_serialization_notification() {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "window/showMessage",
            "params": {"type": 1, "message": "Hello"}
        });

        let content = serde_json::to_string(&notification).unwrap();
        assert!(content.contains("\"method\""));
        assert!(!content.contains("\"id\""));
    }

    fn deeply_nested(depth: usize) -> String {
        format!("{}1{}", "{\"parent\":".repeat(depth), "}".repeat(depth))
    }

    async fn receive_all(inbound: String) -> Vec<Result<InboundMessage>> {
        let (_, mut reader) = LspTransport::new(
            tokio::io::sink(),
            std::io::Cursor::new(inbound.into_bytes()),
        );
        let mut received = Vec::new();
        loop {
            let message = reader.receive().await;
            let failed = message.is_err();
            received.push(message);
            if failed || received.len() == 2 {
                return received;
            }
        }
    }

    fn frame(body: &str) -> String {
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    #[tokio::test]
    async fn test_response_nested_past_the_recursion_limit_is_attributed_to_its_request() {
        let deep = frame(&format!(
            r#"{{"jsonrpc":"2.0","id":7,"result":{}}}"#,
            deeply_nested(200)
        ));
        let next = frame(r#"{"jsonrpc":"2.0","id":8,"result":null}"#);

        let received = receive_all(format!("{deep}{next}")).await;

        assert_matches!(
            &received[0],
            Ok(InboundMessage::UndecodableResponse {
                id: RequestId::Number(7),
                ..
            })
        );
        assert_matches!(&received[1], Ok(InboundMessage::Response(response)) if response.id == RequestId::Number(8));
    }

    #[tokio::test]
    async fn test_response_with_a_lone_surrogate_escape_is_attributed_to_its_request() {
        let body = frame(r#"{"jsonrpc":"2.0","id":9,"result":"\ud800"}"#);

        let received = receive_all(body).await;

        assert_matches!(
            &received[0],
            Ok(InboundMessage::UndecodableResponse {
                id: RequestId::Number(9),
                ..
            })
        );
    }

    /// #681: bytes that are not UTF-8 inside a response fail only that request;
    /// nothing is decoded lossily and the stream goes on.
    #[tokio::test]
    async fn test_response_with_invalid_utf8_is_attributed_to_its_request() {
        let mut body = br#"{"jsonrpc":"2.0","id":4,"result":"a"#.to_vec();
        body.extend_from_slice(&[0xff, 0xfe]);
        body.extend_from_slice(br#"b"}"#);
        let mut inbound = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        inbound.extend(body);
        inbound.extend(frame(r#"{"jsonrpc":"2.0","id":5,"result":null}"#).into_bytes());
        let (_, mut reader) = LspTransport::new(tokio::io::sink(), std::io::Cursor::new(inbound));

        assert_matches!(
            reader.receive().await,
            Ok(InboundMessage::UndecodableResponse {
                id: RequestId::Number(4),
                ..
            })
        );
        assert_matches!(
            reader.receive().await,
            Ok(InboundMessage::Response(response)) if response.id == RequestId::Number(5)
        );
    }

    #[tokio::test]
    async fn test_string_id_is_recovered_from_an_undecodable_response() {
        let deep = frame(&format!(
            r#"{{"jsonrpc":"2.0","id":"abc","error":{}}}"#,
            deeply_nested(200)
        ));

        let received = receive_all(deep).await;

        assert_matches!(
            &received[0],
            Ok(InboundMessage::UndecodableResponse { id: RequestId::String(id), .. }) if id == "abc"
        );
    }

    #[tokio::test]
    async fn test_undecodable_request_is_attributed_to_its_id() {
        let deep = frame(&format!(
            r#"{{"jsonrpc":"2.0","id":"r1","method":"m","params":{}}}"#,
            deeply_nested(200)
        ));
        let next = frame(r#"{"jsonrpc":"2.0","method":"exit"}"#);

        let received = receive_all(format!("{deep}{next}")).await;

        assert_matches!(
            &received[0],
            Ok(InboundMessage::UndecodableRequest { id: RequestId::String(id), .. }) if id == "r1"
        );
        assert_matches!(&received[1], Ok(InboundMessage::Notification(_)));
    }

    #[tokio::test]
    async fn test_undecodable_notification_is_dropped_and_the_stream_goes_on() {
        let deep = frame(&format!(
            r#"{{"jsonrpc":"2.0","method":"m","params":{}}}"#,
            deeply_nested(200)
        ));
        let next = frame(r#"{"jsonrpc":"2.0","id":8,"result":null}"#);

        let received = receive_all(format!("{deep}{next}")).await;

        assert_matches!(
            &received[0],
            Ok(InboundMessage::UndecodableNotification(
                DroppedNotification::Other
            ))
        );
        assert_matches!(&received[1], Ok(InboundMessage::Response(_)));
    }

    #[tokio::test]
    async fn test_frames_that_identify_nothing_are_dropped_not_fatal() {
        let bodies = [
            r#"{"jsonrpc":"2.0","id":1,"result":{"a":}"#.to_owned(),
            format!(r#"{{"jsonrpc":"2.0","result":{}}}"#, deeply_nested(200)),
            format!(
                r#"{{"jsonrpc":"2.0","id":null,"result":{}}}"#,
                deeply_nested(200)
            ),
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"extra":{}}}"#,
                deeply_nested(200)
            ),
            r#"{"jsonrpc":"2.0","method":7,"id":{"a":1}}"#.to_owned(),
        ];
        for body in bodies {
            let received = receive_all(frame(&body)).await;
            assert_matches!(
                &received[0],
                Ok(InboundMessage::UndecodableFrame | InboundMessage::UndecodableResponse { .. }),
                "{body:.80}"
            );
        }
    }

    #[tokio::test]
    async fn test_malformed_messages_of_each_kind_are_undecodable() {
        let cases = [
            (r#"{"jsonrpc":"2.0","id":3,"method":5}"#, "request"),
            (r#"{"jsonrpc":"2.0","method":5}"#, "notification"),
            (r#"{"jsonrpc":"2.0","id":3,"error":5}"#, "response"),
            (r#"{"jsonrpc":"2.0","id":3}"#, "incomplete response"),
            (r#"{"jsonrpc":"2.0"}"#, "unrecognized"),
        ];
        for (body, what) in cases {
            let received = receive_all(frame(body)).await;
            let message = received[0].as_ref().unwrap();
            let expected = match what {
                "request" => matches!(message, InboundMessage::UndecodableRequest { .. }),
                "notification" => matches!(message, InboundMessage::UndecodableNotification(_)),
                "response" | "incomplete response" => {
                    matches!(message, InboundMessage::UndecodableResponse { id, .. } if *id == RequestId::Number(3))
                }
                _ => matches!(message, InboundMessage::UndecodableFrame),
            };
            assert!(expected, "{what}: {message:?}");
        }
    }

    #[tokio::test]
    async fn test_top_level_array_is_not_read_as_an_envelope() {
        let nested_array = format!("[{}]", "[".repeat(200) + &"]".repeat(200));

        let received = receive_all(frame(&nested_array)).await;

        assert_matches!(&received[0], Ok(InboundMessage::UndecodableFrame));
    }

    #[test]
    fn test_server_request_parsing() {
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "ts1",
            "method": "client/registerCapability",
            "params": {"registrations": []}
        });

        let message = parse_inbound_message(value, &Redactions::default());
        match message {
            InboundMessage::Request(request) => {
                assert_eq!(request.id, RequestId::String("ts1".to_string()));
                assert_eq!(request.method, "client/registerCapability");
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn test_decode_failure_text_is_redacted_in_the_log() {
        use tracing_subscriber::prelude::*;

        use crate::test_lsp::CapturedLogs;

        let redactions =
            Redactions::new([("API_TOKEN".to_owned(), "SuperSecretValue123".to_owned())]);
        let logs = CapturedLogs::default();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::registry()
                .with(tracing_subscriber::filter::LevelFilter::DEBUG)
                .with(logs.clone()),
        );
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": "SuperSecretValue123", "message": "boom"}
        });

        let message = parse_inbound_message(value, &redactions);

        assert_matches!(
            message,
            InboundMessage::UndecodableResponse {
                id: RequestId::Number(1),
                ..
            }
        );
        let text = logs.messages().join("\n");
        assert!(text.contains("[redacted:API_TOKEN]"), "{text}");
        assert!(!text.contains("SuperSecretValue123"), "{text}");
    }

    #[test]
    fn test_id_only_message_is_an_undecodable_response() {
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1
        });

        let message = parse_inbound_message(value, &Redactions::default());

        assert_matches!(
            message,
            InboundMessage::UndecodableResponse {
                id: RequestId::Number(1),
                ..
            }
        );
    }

    #[test]
    fn test_message_serialization_error_response() {
        let error_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "error": {
                "code": -32601,
                "message": "Method not found"
            }
        });

        let content = serde_json::to_string(&error_response).unwrap();
        assert!(content.contains("\"error\""));
        assert!(content.contains("-32601"));
        assert!(content.contains("Method not found"));
    }

    #[test]
    fn test_content_length_calculation() {
        let message = serde_json::json!({"test": "data"});
        let content = serde_json::to_string(&message).unwrap();
        let expected_len = content.len();

        let header = format!("Content-Length: {}\r\n\r\n", content.len());
        assert!(header.contains(&expected_len.to_string()));
    }

    /// A server that sends many malformed header lines is warned about once.
    #[tokio::test]
    async fn test_malformed_header_warnings_are_rate_limited() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let captured = crate::test_lsp::CapturedLogs::default();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let inbound = format!(
            "no colon one\r\nno colon two\r\nno colon three\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let (_, mut reader) = LspTransport::new(
            tokio::io::sink(),
            std::io::Cursor::new(inbound.into_bytes()),
        );

        assert_matches!(reader.receive().await, Ok(InboundMessage::Response(_)));

        let warnings = captured
            .entries()
            .into_iter()
            .filter(|(level, message)| {
                *level == tracing::Level::WARN && message.contains("Malformed header")
            })
            .count();
        assert_eq!(warnings, 1);
    }

    #[test]
    fn test_header_without_colon() {
        let malformed_line = "Malformed header without colon";
        let result = malformed_line.split_once(':');
        assert!(result.is_none(), "Should not parse malformed header");
    }

    #[test]
    fn test_header_with_whitespace() {
        let header_line = "  Content-Length  :  456  ";
        if let Some((key, value)) = header_line.split_once(':') {
            let key_trimmed = key.trim().to_lowercase();
            let value_trimmed = value.trim();

            assert_eq!(key_trimmed, "content-length");
            assert_eq!(value_trimmed, "456");
        }
    }

    /// #457: a spawned server writing one endless header line with no `\n`
    /// must not grow `read_headers`' buffer without bound -- it must fail
    /// fast instead.
    #[tokio::test]
    async fn test_read_headers_rejects_oversized_line() {
        let (mut peer, stdout) = tokio::io::duplex(MAX_HEADER_LINE_BYTES + 4096);
        let (_transport, mut reader) = LspTransport::new(tokio::io::sink(), stdout);

        let oversized_line = vec![b'a'; MAX_HEADER_LINE_BYTES + 10];
        peer.write_all(&oversized_line).await.unwrap();

        let result = reader.receive().await;
        assert_matches!(result, Err(Error::LspProtocolError(_)), "got {result:?}");
    }

    /// #457: a spawned server writing endlessly many distinct header lines
    /// before the blank-line terminator must not grow the header map without
    /// bound -- it must fail fast instead.
    #[tokio::test]
    async fn test_read_headers_rejects_too_many_headers() {
        let mut body = String::new();
        for i in 0..=MAX_HEADERS {
            let _ = write!(body, "X-Header-{i}: value\r\n");
        }

        let (mut peer, stdout) = tokio::io::duplex(body.len() + 4096);
        let (_transport, mut reader) = LspTransport::new(tokio::io::sink(), stdout);

        peer.write_all(body.as_bytes()).await.unwrap();

        let result = reader.receive().await;
        assert_matches!(result, Err(Error::LspProtocolError(_)), "got {result:?}");
    }

    /// #457 boundary: a header line whose length lands exactly at
    /// `MAX_HEADER_LINE_BYTES` (including its `\r\n`) must still parse --
    /// the cap only rejects a line that *exceeds* it.
    #[tokio::test]
    async fn test_read_headers_accepts_line_at_exact_length_boundary() {
        let key = "X-Pad: ";
        let terminator = "\r\n";
        let pad_len = MAX_HEADER_LINE_BYTES - key.len() - terminator.len();
        let padded_value = "a".repeat(pad_len);
        let mut body = format!("{key}{padded_value}{terminator}");
        assert_eq!(body.len(), MAX_HEADER_LINE_BYTES);
        body.push_str("\r\n");

        let (mut peer, stdout) = tokio::io::duplex(body.len() + 64);
        let (_transport, mut reader) = LspTransport::new(tokio::io::sink(), stdout);
        peer.write_all(body.as_bytes()).await.unwrap();

        let headers = reader.read_headers().await.unwrap();
        assert_eq!(headers.get("x-pad"), Some(&padded_value));
    }

    /// #457 boundary: exactly `MAX_HEADERS` header lines, properly
    /// terminated, must still parse -- the cap only rejects a frame that
    /// *exceeds* it.
    #[tokio::test]
    async fn test_read_headers_accepts_exactly_max_headers() {
        let mut body = String::new();
        for i in 0..MAX_HEADERS {
            let _ = write!(body, "X-Header-{i}: value\r\n");
        }
        body.push_str("\r\n");

        let (mut peer, stdout) = tokio::io::duplex(body.len() + 4096);
        let (_transport, mut reader) = LspTransport::new(tokio::io::sink(), stdout);
        peer.write_all(body.as_bytes()).await.unwrap();

        let headers = reader.read_headers().await.unwrap();
        assert_eq!(headers.len(), MAX_HEADERS);
    }

    fn oversized_body(head: &str, tail: &str) -> String {
        format!("{head}{}{tail}", "x".repeat(MAX_CONTENT_LENGTH + 1))
    }

    const OVERSIZED_RESULT: (&str, &str) = (r#"{"jsonrpc":"2.0","id":7,"result":""#, r#""}"#);

    #[tokio::test]
    async fn test_oversized_response_fails_only_its_request_and_the_next_frame_is_read() {
        let body = oversized_body(OVERSIZED_RESULT.0, OVERSIZED_RESULT.1);
        let next = frame(r#"{"jsonrpc":"2.0","id":8,"result":null}"#);

        let received = receive_all(format!("{}{next}", frame(&body))).await;

        assert_matches!(
            &received[0],
            Ok(InboundMessage::UndecodableResponse {
                id: RequestId::Number(7),
                reason: UndecodableReason::Oversized { bytes },
            }) if *bytes == body.len()
        );
        assert_matches!(&received[1], Ok(InboundMessage::Response(r)) if r.id == RequestId::Number(8));
    }

    #[tokio::test]
    async fn test_oversized_notification_is_dropped_and_the_next_frame_is_read() {
        let body = oversized_body(r#"{"jsonrpc":"2.0","method":"m","params":""#, r#""}"#);
        let next = frame(r#"{"jsonrpc":"2.0","id":8,"result":null}"#);

        let received = receive_all(format!("{}{next}", frame(&body))).await;

        assert_matches!(
            &received[0],
            Ok(InboundMessage::UndecodableNotification(
                DroppedNotification::Other
            ))
        );
        assert_matches!(&received[1], Ok(InboundMessage::Response(_)));
    }

    #[tokio::test]
    async fn test_oversized_server_request_is_attributed_to_its_id() {
        let body = oversized_body(
            r#"{"jsonrpc":"2.0","id":"r1","method":"m","params":""#,
            r#""}"#,
        );

        let received = receive_all(frame(&body)).await;

        assert_matches!(
            &received[0],
            Ok(InboundMessage::UndecodableRequest {
                id: RequestId::String(id),
                reason: UndecodableReason::Oversized { .. },
            }) if id == "r1"
        );
    }

    #[tokio::test]
    async fn test_oversized_frame_that_never_closes_its_object_identifies_nothing() {
        let body = oversized_body(r#"{"jsonrpc":"2.0","id":7,"result":""#, "");
        let next = frame(r#"{"jsonrpc":"2.0","id":8,"result":null}"#);

        let received = receive_all(format!("{}{next}", frame(&body))).await;

        assert_matches!(&received[0], Ok(InboundMessage::UndecodableFrame));
        assert_matches!(&received[1], Ok(InboundMessage::Response(_)));
    }

    const PUBLISH_HEAD: &str = r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","#;

    fn lost_publish_of(message: &Result<InboundMessage>) -> LostPublish {
        match message {
            Ok(InboundMessage::UndecodableNotification(DroppedNotification::Publish(lost))) => {
                lost.clone()
            }
            other => panic!("expected a lost publish, got {other:?}"),
        }
    }

    fn file(uri: &str) -> LostPublish {
        LostPublish::File(BoundedUri::new(Uri::from(uri.to_owned())).unwrap())
    }

    #[tokio::test]
    async fn test_oversized_publish_names_its_file_whatever_the_member_order() {
        let uri = r#""uri":"file:///a.rs""#;
        let heads = [
            format!(r#"{PUBLISH_HEAD}"params":{{{uri},"diagnostics":""#),
            format!(r#"{PUBLISH_HEAD}"params":{{"diagnostics":"{{}}",{uri},"x":""#),
        ];
        for head in heads {
            let body = oversized_body(&head, r#""}}"#);
            let next = frame(r#"{"jsonrpc":"2.0","id":8,"result":null}"#);

            let received = receive_all(format!("{}{next}", frame(&body))).await;

            assert_eq!(lost_publish_of(&received[0]), file("file:///a.rs"));
            assert_matches!(&received[1], Ok(InboundMessage::Response(_)));
        }
    }

    #[tokio::test]
    async fn test_oversized_publish_without_a_usable_uri_is_unidentified() {
        let bodies = [
            oversized_body(
                &format!(r#"{PUBLISH_HEAD}"params":{{"diagnostics":""#),
                r#""}}"#,
            ),
            oversized_body(
                &format!(r#"{PUBLISH_HEAD}"params":{{"uri":5,"d":""#),
                r#""}}"#,
            ),
            oversized_body(&format!(r#"{PUBLISH_HEAD}"params":[""#), r#""]}"#),
            oversized_body(
                &format!(r#"{PUBLISH_HEAD}"params":{{"uri":"a","uri":"b","d":""#),
                r#""}}"#,
            ),
            oversized_body(PUBLISH_HEAD, r#""x":1}"#),
            oversized_body(
                &format!(
                    r#"{PUBLISH_HEAD}"params":{{"uri":"{}","d":""#,
                    "u".repeat(20_000)
                ),
                r#""}}"#,
            ),
        ];
        for body in bodies {
            let received = receive_all(frame(&body)).await;

            assert_eq!(
                lost_publish_of(&received[0]),
                LostPublish::Unidentified,
                "{}",
                &body[..120]
            );
        }
    }

    #[tokio::test]
    async fn test_oversized_publish_that_never_closes_is_still_a_lost_publish() {
        let body = oversized_body(
            &format!(r#"{PUBLISH_HEAD}"params":{{"uri":"file:///a.rs","d":""#),
            "",
        );

        let received = receive_all(frame(&body)).await;

        assert_eq!(lost_publish_of(&received[0]), file("file:///a.rs"));
    }

    #[tokio::test]
    async fn test_oversized_publish_with_an_escaped_method_key_records_no_loss() {
        let body = oversized_body(
            r#"{"jsonrpc":"2.0","\u006dethod":"textDocument/publishDiagnostics","params":{"uri":"file:///a.rs","d":""#,
            r#""}}"#,
        );

        let received = receive_all(frame(&body)).await;

        assert_matches!(&received[0], Ok(InboundMessage::UndecodableFrame));
    }

    #[tokio::test]
    async fn test_malformed_publish_within_the_limit_names_its_file() {
        let deep = frame(&format!(
            r#"{PUBLISH_HEAD}"params":{{"uri":"file:///a.rs","diagnostics":{}}}}}"#,
            deeply_nested(200)
        ));
        let no_uri = frame(&format!(
            r#"{PUBLISH_HEAD}"params":{{"diagnostics":{}}}}}"#,
            deeply_nested(200)
        ));

        let received = receive_all(format!("{deep}{no_uri}")).await;

        assert_eq!(lost_publish_of(&received[0]), file("file:///a.rs"));
        assert_eq!(lost_publish_of(&received[1]), LostPublish::Unidentified);
    }

    #[tokio::test]
    async fn test_publish_with_a_bad_jsonrpc_version_names_its_file() {
        let bad_version = frame(
            r#"{"jsonrpc":"1.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///b.rs","diagnostics":[]}}"#,
        );

        let received = receive_all(bad_version).await;

        assert_eq!(lost_publish_of(&received[0]), file("file:///b.rs"));
    }

    #[tokio::test]
    async fn test_publishes_with_bad_params_pass_the_transport_as_notifications() {
        let bad_body = frame(&format!(
            r#"{PUBLISH_HEAD}"params":{{"uri":"file:///a.rs","diagnostics":"x"}}}}"#
        ));
        let no_params = frame(&format!(r#"{PUBLISH_HEAD}"x":1}}"#));

        let received = receive_all(format!("{bad_body}{no_params}")).await;

        assert_matches!(&received[0], Ok(InboundMessage::Notification(_)));
        assert_matches!(&received[1], Ok(InboundMessage::Notification(_)));
    }

    #[test]
    fn test_notification_with_a_bad_envelope_is_dropped_as_other_unless_it_is_a_publish() {
        let other =
            serde_json::json!({"jsonrpc": "1.0", "method": "window/logMessage", "params": {}});

        let message = parse_inbound_message(other, &Redactions::default());

        assert_matches!(
            message,
            InboundMessage::UndecodableNotification(DroppedNotification::Other)
        );
    }

    #[test]
    fn test_notification_keeps_its_params_and_treats_null_as_absent() {
        let with_null = serde_json::json!({"jsonrpc": "2.0", "method": "m", "params": null});
        let with_params = serde_json::json!({"jsonrpc": "2.0", "method": "m", "params": {"a": 1}});

        let null = parse_inbound_message(with_null, &Redactions::default());
        let kept = parse_inbound_message(with_params, &Redactions::default());

        assert_matches!(null, InboundMessage::Notification(n) if n.params.is_none());
        assert_matches!(kept, InboundMessage::Notification(n) if n.params == Some(serde_json::json!({"a": 1})));
    }

    #[tokio::test]
    async fn test_skip_consumes_exactly_the_announced_length_whatever_the_scan_state() {
        let mut body = oversized_body(OVERSIZED_RESULT.0, OVERSIZED_RESULT.1);
        body.push_str("  }{\"id\":99,\"result\":1}");
        let next = frame(r#"{"jsonrpc":"2.0","id":8,"result":null}"#);

        let received = receive_all(format!("{}{next}", frame(&body))).await;

        assert_matches!(
            &received[0],
            Ok(InboundMessage::UndecodableResponse {
                id: RequestId::Number(7),
                ..
            })
        );
        assert_matches!(&received[1], Ok(InboundMessage::Response(r)) if r.id == RequestId::Number(8));
    }

    #[tokio::test]
    async fn test_frame_announced_above_the_skip_cap_stays_fatal() {
        let inbound = format!("Content-Length: {}\r\n\r\n", MAX_SKIPPED_CONTENT_LENGTH + 1);

        let received = receive_all(inbound).await;

        assert_matches!(&received[0], Err(Error::LspProtocolError(_)));
    }

    #[tokio::test]
    async fn test_eof_inside_a_skipped_frame_ends_the_stream() {
        let inbound = format!(
            "Content-Length: {}\r\n\r\n{}",
            MAX_CONTENT_LENGTH + 1,
            r#"{"jsonrpc":"2.0","id":7,"result":"#
        );

        let received = receive_all(inbound).await;

        assert_matches!(&received[0], Err(Error::ServerTerminated));
    }

    #[tokio::test]
    async fn test_skipped_oversized_frame_is_warned_with_its_size() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let captured = crate::test_lsp::CapturedLogs::default();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
        let body = oversized_body(r#"{"jsonrpc":"2.0","method":"m","params":""#, r#""}"#);

        receive_all(frame(&body)).await;

        let fields = captured.fields();
        assert!(
            fields.contains(&format!("bytes={}", body.len())),
            "{fields:?}"
        );
        assert!(
            fields.contains(&format!("limit={MAX_CONTENT_LENGTH}")),
            "{fields:?}"
        );
        assert!(fields.contains(&"suppressed=0".to_owned()), "{fields:?}");
        assert!(
            captured
                .entries()
                .iter()
                .any(|(level, message)| *level == tracing::Level::WARN
                    && message == "skipped an oversized LSP frame"),
            "{:?}",
            captured.entries()
        );
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;

        fn block_on<F: std::future::Future>(future: F) -> F::Output {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(future)
        }

        proptest! {
            #[test]
            fn test_send_receive_round_trips_notifications(
                method in any::<String>(),
                text in any::<String>(),
            ) {
                let message = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": {"text": text},
                });
                let received = block_on(async {
                    let (client_end, server_end) = tokio::io::duplex(1 << 20);
                    let (mut writer, _) = LspTransport::new(client_end, tokio::io::empty());
                    let (_, mut reader) = LspTransport::new(tokio::io::sink(), server_end);
                    writer.send(&message).await.unwrap();
                    reader.receive().await.unwrap()
                });
                let InboundMessage::Notification(notification) = received else {
                    panic!("expected notification, got {received:?}");
                };
                prop_assert_eq!(notification.method, method);
                prop_assert_eq!(notification.params, Some(serde_json::json!({"text": text})));
            }

            #[test]
            fn test_arbitrary_bytes_never_panic_the_frame_parser(
                data in proptest::collection::vec(any::<u8>(), 0..2048),
            ) {
                block_on(async {
                    let (_, mut reader) =
                        LspTransport::new(tokio::io::sink(), std::io::Cursor::new(data));
                    while reader.receive().await.is_ok() {}
                });
            }

            #[test]
            fn test_arbitrary_framed_bodies_never_panic_the_frame_parser(
                header in "Content-Length: [0-9]{0,12}\\r\\n([A-Za-z-]{0,12}: [ -~]{0,24}\\r\\n){0,4}\\r\\n",
                body in proptest::collection::vec(any::<u8>(), 0..512),
            ) {
                let mut data = header.into_bytes();
                data.extend(body);
                block_on(async {
                    let (_, mut reader) =
                        LspTransport::new(tokio::io::sink(), std::io::Cursor::new(data));
                    while reader.receive().await.is_ok() {}
                });
            }
        }
    }
}
