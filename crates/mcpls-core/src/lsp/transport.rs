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

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::{debug, trace, warn};

use crate::error::{Error, Result};
use crate::lsp::types::{InboundMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};

/// Maximum allowed Content-Length (10 MB)
const MAX_CONTENT_LENGTH: usize = 10 * 1024 * 1024;

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
        (
            Self {
                stdin: Box::new(stdin),
            },
            LspTransportReader {
                stdout: BufReader::new(Box::new(stdout)),
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

        trace!("Sending LSP message: {}", content);

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
    /// and parses it as either a response or notification.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Reading headers fails
    /// - Content-Length header is missing or invalid
    /// - Reading message content fails
    /// - JSON parsing fails
    /// - Message format is invalid
    pub async fn receive(&mut self) -> Result<InboundMessage> {
        loop {
            let headers = self.read_headers().await?;

            let content_length = headers
                .get("content-length")
                .ok_or_else(|| {
                    Error::LspProtocolError("Missing Content-Length header".to_string())
                })?
                .parse::<usize>()
                .map_err(|e| Error::LspProtocolError(format!("Invalid Content-Length: {e}")))?;

            if content_length > MAX_CONTENT_LENGTH {
                return Err(Error::LspProtocolError(format!(
                    "Content-Length {content_length} exceeds maximum allowed size of {MAX_CONTENT_LENGTH} bytes"
                )));
            }

            let content = self.read_content(content_length).await?;

            trace!("Received LSP message: {}", content);

            let value: Value = serde_json::from_str(&content)?;

            // Some servers (notably OmniSharp) occasionally emit a bare `null`
            // (or other non-object) JSON-RPC message. Skip it and read the next
            // framed message instead of killing the whole message loop.
            if !value.is_object() {
                // Some servers (notably OmniSharp) emit a burst of these during
                // startup; log at debug to avoid flooding the logs for what is a
                // recoverable, expected condition.
                debug!("Skipping non-object LSP message: {}", value);
                continue;
            }

            return parse_inbound_message(value);
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
                return Err(Error::LspProtocolError(format!(
                    "LSP header line exceeded {MAX_HEADER_LINE_BYTES} bytes without a newline"
                )));
            }

            if line == "\r\n" || line == "\n" {
                break;
            }

            lines_read += 1;
            if lines_read > MAX_HEADERS {
                return Err(Error::LspProtocolError(format!(
                    "LSP frame exceeded {MAX_HEADERS} header lines"
                )));
            }

            if let Some((key, value)) = line.trim_end().split_once(':') {
                headers.insert(key.trim().to_lowercase(), value.trim().to_string());
            } else {
                warn!(
                    "Malformed header: {}",
                    crate::util::truncate_str(line.trim(), crate::util::MAX_LOG_STRING_BYTES)
                );
            }
        }

        Ok(headers)
    }

    /// Read exact number of content bytes.
    ///
    /// Reads exactly `length` bytes from stdout and converts to UTF-8 string.
    async fn read_content(&mut self, length: usize) -> Result<String> {
        let mut buffer = vec![0u8; length];
        self.stdout.read_exact(&mut buffer).await?;

        String::from_utf8(buffer)
            .map_err(|e| Error::LspProtocolError(format!("Invalid UTF-8 in content: {e}")))
    }
}

fn parse_inbound_message(value: Value) -> Result<InboundMessage> {
    if value.get("method").is_some() {
        if value.get("id").is_some() {
            let request: JsonRpcRequest = serde_json::from_value(value)
                .map_err(|e| Error::LspProtocolError(format!("Invalid request: {e}")))?;
            Ok(InboundMessage::Request(request))
        } else {
            let notification: JsonRpcNotification = serde_json::from_value(value)
                .map_err(|e| Error::LspProtocolError(format!("Invalid notification: {e}")))?;
            Ok(InboundMessage::Notification(notification))
        }
    } else if value.get("id").is_some()
        && (value.get("result").is_some() || value.get("error").is_some())
    {
        let response: JsonRpcResponse = serde_json::from_value(value)
            .map_err(|e| Error::LspProtocolError(format!("Invalid response: {e}")))?;
        Ok(InboundMessage::Response(response))
    } else if value.get("id").is_some() {
        Err(Error::LspProtocolError(
            "Response messages with an id must include either result or error".to_string(),
        ))
    } else {
        Err(Error::LspProtocolError(
            "Message must be a request, response, or notification".to_string(),
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use crate::lsp::types::RequestId;

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

    #[test]
    fn test_server_request_parsing() {
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "ts1",
            "method": "client/registerCapability",
            "params": {"registrations": []}
        });

        let message = parse_inbound_message(value).unwrap();
        match message {
            InboundMessage::Request(request) => {
                assert_eq!(request.id, RequestId::String("ts1".to_string()));
                assert_eq!(request.method, "client/registerCapability");
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn test_id_only_message_is_protocol_error() {
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1
        });

        let error = parse_inbound_message(value).unwrap_err();
        assert!(matches!(error, Error::LspProtocolError(_)));
        assert!(
            error
                .to_string()
                .contains("must include either result or error")
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
        assert!(
            matches!(result, Err(Error::LspProtocolError(_))),
            "got {result:?}"
        );
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
        assert!(
            matches!(result, Err(Error::LspProtocolError(_))),
            "got {result:?}"
        );
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
}
