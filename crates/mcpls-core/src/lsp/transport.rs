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

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tracing::{trace, warn};

use crate::error::{Error, Result};
use crate::lsp::types::{InboundMessage, JsonRpcNotification, JsonRpcResponse};

/// LSP transport layer handling header-content format.
///
/// This transport handles the LSP protocol's header-content message format,
/// parsing Content-Length headers and reading exact message content.
#[derive(Debug)]
pub struct LspTransport {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl LspTransport {
    /// Create transport from child process stdio.
    ///
    /// # Arguments
    ///
    /// * `stdin` - The child process's stdin handle for sending messages
    /// * `stdout` - The child process's stdout handle for receiving messages
    #[must_use]
    pub fn new(stdin: ChildStdin, stdout: ChildStdout) -> Self {
        Self {
            stdin,
            stdout: BufReader::new(stdout),
        }
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
        let headers = self.read_headers().await?;

        let content_length = headers
            .get("content-length")
            .ok_or_else(|| Error::LspProtocolError("Missing Content-Length header".to_string()))?
            .parse::<usize>()
            .map_err(|e| Error::LspProtocolError(format!("Invalid Content-Length: {e}")))?;

        let content = self.read_content(content_length).await?;

        trace!("Received LSP message: {}", content);

        let value: Value = serde_json::from_str(&content)?;

        if value.get("id").is_some() {
            let response: JsonRpcResponse = serde_json::from_value(value)
                .map_err(|e| Error::LspProtocolError(format!("Invalid response: {e}")))?;
            Ok(InboundMessage::Response(response))
        } else {
            let notification: JsonRpcNotification = serde_json::from_value(value)
                .map_err(|e| Error::LspProtocolError(format!("Invalid notification: {e}")))?;
            Ok(InboundMessage::Notification(notification))
        }
    }

    /// Read headers until blank line.
    ///
    /// Headers are in the format "Key: Value\r\n" and are terminated by
    /// a blank line ("\r\n").
    async fn read_headers(&mut self) -> Result<HashMap<String, String>> {
        let mut headers = HashMap::new();
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = self.stdout.read_line(&mut line).await?;

            // EOF - stream closed (read_line returns 0 bytes on EOF)
            if bytes_read == 0 || line.is_empty() {
                trace!(
                    "EOF detected in read_headers: bytes_read={}, line_len={}",
                    bytes_read,
                    line.len()
                );
                return Err(Error::ServerTerminated);
            }

            if line == "\r\n" || line == "\n" {
                break;
            }

            if let Some((key, value)) = line.trim_end().split_once(':') {
                headers.insert(key.trim().to_lowercase(), value.trim().to_string());
            } else {
                warn!("Malformed header: {}", line.trim());
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
}
