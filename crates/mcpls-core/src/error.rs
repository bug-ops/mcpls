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

    /// Invalid tool parameters provided.
    #[error("invalid tool parameters: {0}")]
    InvalidToolParams(String),

    /// File I/O error occurred.
    #[error("file I/O error for {path:?}: {source}")]
    FileIo {
        /// Path to the file.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Path is outside allowed workspace boundaries.
    #[error("path outside workspace: {0}")]
    PathOutsideWorkspace(PathBuf),

    /// Document limit exceeded.
    #[error("document limit exceeded: {current}/{max}")]
    DocumentLimitExceeded {
        /// Current number of documents.
        current: usize,
        /// Maximum allowed documents.
        max: usize,
    },

    /// File size limit exceeded.
    #[error("file size limit exceeded: {size} bytes (max: {max} bytes)")]
    FileSizeLimitExceeded {
        /// Actual file size.
        size: u64,
        /// Maximum allowed size.
        max: u64,
    },
}

/// A specialized Result type for mcpls-core operations.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display_lsp_init_failed() {
        let err = Error::LspInitFailed {
            message: "server not found".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "LSP server initialization failed: server not found"
        );
    }

    #[test]
    fn test_error_display_lsp_server_error() {
        let err = Error::LspServerError {
            code: -32600,
            message: "Invalid request".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "LSP server error: -32600 - Invalid request"
        );
    }

    #[test]
    fn test_error_display_document_not_found() {
        let err = Error::DocumentNotFound(PathBuf::from("/path/to/file.rs"));
        assert!(err.to_string().contains("document not found"));
        assert!(err.to_string().contains("file.rs"));
    }

    #[test]
    fn test_error_display_no_server_for_language() {
        let err = Error::NoServerForLanguage("rust".to_string());
        assert_eq!(
            err.to_string(),
            "no LSP server configured for language: rust"
        );
    }

    #[test]
    fn test_error_display_timeout() {
        let err = Error::Timeout(30);
        assert_eq!(err.to_string(), "request timed out after 30 seconds");
    }

    #[test]
    fn test_error_display_document_limit() {
        let err = Error::DocumentLimitExceeded {
            current: 150,
            max: 100,
        };
        assert_eq!(err.to_string(), "document limit exceeded: 150/100");
    }

    #[test]
    fn test_error_display_file_size_limit() {
        let err = Error::FileSizeLimitExceeded {
            size: 20_000_000,
            max: 10_000_000,
        };
        assert!(err.to_string().contains("file size limit exceeded"));
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_error_from_json() {
        let json_str = "{invalid json}";
        let json_err = serde_json::from_str::<serde_json::Value>(json_str).unwrap_err();
        let err: Error = json_err.into();
        assert!(matches!(err, Error::Json(_)));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn test_error_from_toml() {
        let toml_str = "[invalid toml";
        let toml_err = toml::from_str::<toml::Value>(toml_str).unwrap_err();
        let err: Error = toml_err.into();
        assert!(matches!(err, Error::Toml(_)));
    }

    #[test]
    fn test_result_type_alias() {
        fn _returns_error() -> Result<i32> {
            Err(Error::Config("test error".to_string()))
        }

        let result: Result<i32> = Ok(42);
        assert!(result.is_ok());
        if let Ok(value) = result {
            assert_eq!(value, 42);
        }
    }

    #[test]
    fn test_error_source_chain() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err = Error::ServerSpawnFailed {
            command: "rust-analyzer".to_string(),
            source: io_err,
        };

        let source = std::error::Error::source(&err);
        assert!(source.is_some());
    }
}
