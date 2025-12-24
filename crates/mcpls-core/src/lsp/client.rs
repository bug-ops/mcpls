//! LSP client implementation.

use crate::config::LspServerConfig;
use crate::error::Result;

/// LSP client for communicating with a language server.
#[derive(Debug)]
pub struct LspClient {
    /// Configuration for this LSP server.
    config: LspServerConfig,
    /// Current server state.
    state: super::ServerState,
}

impl LspClient {
    /// Create a new LSP client with the given configuration.
    #[must_use]
    pub fn new(config: LspServerConfig) -> Self {
        Self {
            config,
            state: super::ServerState::Uninitialized,
        }
    }

    /// Get the language ID for this client.
    #[must_use]
    pub fn language_id(&self) -> &str {
        &self.config.language_id
    }

    /// Get the current server state.
    #[must_use]
    pub fn state(&self) -> &super::ServerState {
        &self.state
    }

    /// Initialize the LSP server.
    ///
    /// # Errors
    ///
    /// Returns an error if server initialization fails.
    pub async fn initialize(&mut self) -> Result<()> {
        // TODO: Implement LSP initialization
        // 1. Spawn server process
        // 2. Send initialize request
        // 3. Wait for response
        // 4. Send initialized notification
        tracing::debug!(
            language_id = %self.config.language_id,
            "initializing LSP server"
        );
        self.state = super::ServerState::Ready;
        Ok(())
    }

    /// Shutdown the LSP server gracefully.
    ///
    /// # Errors
    ///
    /// Returns an error if shutdown fails.
    pub async fn shutdown(&mut self) -> Result<()> {
        // TODO: Implement LSP shutdown
        // 1. Send shutdown request
        // 2. Wait for response
        // 3. Send exit notification
        // 4. Wait for process to exit
        tracing::debug!(
            language_id = %self.config.language_id,
            "shutting down LSP server"
        );
        self.state = super::ServerState::Shutdown;
        Ok(())
    }
}
