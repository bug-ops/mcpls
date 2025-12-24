//! LSP transport layer for stdio communication.

use crate::error::Result;

/// Stdio transport for LSP communication.
///
/// Handles the LSP header-content protocol over stdin/stdout.
#[derive(Debug)]
pub struct StdioTransport {
    // TODO: Add fields for stdin/stdout handles
}

impl StdioTransport {
    /// Create a new stdio transport.
    #[must_use]
    pub fn new() -> Self {
        Self {}
    }

    /// Send a message to the LSP server.
    ///
    /// # Errors
    ///
    /// Returns an error if sending fails.
    pub async fn send(&mut self, _message: &[u8]) -> Result<()> {
        // TODO: Implement message sending with Content-Length header
        Ok(())
    }

    /// Receive a message from the LSP server.
    ///
    /// # Errors
    ///
    /// Returns an error if receiving fails.
    pub async fn receive(&mut self) -> Result<Vec<u8>> {
        // TODO: Implement message receiving with Content-Length parsing
        Ok(Vec::new())
    }
}

impl Default for StdioTransport {
    fn default() -> Self {
        Self::new()
    }
}
