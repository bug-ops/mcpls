//! Error types for mcpls-core.
//!
//! This module defines the canonical error type for the library,
//! following the Microsoft Rust Guidelines for error handling.

use std::path::PathBuf;

/// The main error type for mcpls-core operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// LSP server failed to initialize.
    #[error("LSP server initialization failed: {message}")]
    LspInitFailed {
        /// Description of the initialization failure.
        message: String,
    },

    /// LSP server returned an error response.
    #[error("LSP server error: {code} - {message}")]
    LspServerError {
        /// JSON-RPC error code.
        code: i32,
        /// Error message from the server.
        message: String,
    },

    /// MCP server error.
    #[error("MCP server error: {0}")]
    McpServer(String),

    /// Document was not found or could not be opened.
    #[error("document not found: {0}")]
    DocumentNotFound(PathBuf),

    /// No LSP server configured for the given language.
    #[error("no LSP server configured for language: {0}")]
    NoServerForLanguage(String),

    /// Configuration error.
    #[error("configuration error: {0}")]
    Config(String),

    /// Configuration file not found.
    #[error("configuration file not found: {0}")]
    ConfigNotFound(PathBuf),

    /// Invalid configuration format.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// TOML parsing error.
    #[error("TOML parsing error: {0}")]
    Toml(#[from] toml::de::Error),

    /// LSP client transport error.
    #[error("transport error: {0}")]
    Transport(String),

    /// Request timeout.
    #[error("request timed out after {0} seconds")]
    Timeout(u64),

    /// Server shutdown requested.
    #[error("server shutdown requested")]
    Shutdown,

    /// LSP server failed to spawn.
    #[error("failed to spawn LSP server '{command}': {source}")]
    ServerSpawnFailed {
        /// Command that failed to spawn.
        command: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// LSP protocol error during message parsing.
    #[error("LSP protocol error: {0}")]
    LspProtocolError(String),

    /// Invalid URI format.
    #[error("invalid URI: {0}")]
    InvalidUri(String),

    /// Position encoding error.
    #[error("position encoding error: {0}")]
    EncodingError(String),

    /// Server process terminated unexpectedly.
    #[error("LSP server process terminated unexpectedly")]
    ServerTerminated,
}

/// A specialized Result type for mcpls-core operations.
pub type Result<T> = std::result::Result<T, Error>;
