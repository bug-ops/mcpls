//! Error types for mcpls-core.
//!
//! This module defines the canonical error type for the library,
//! following the Microsoft Rust Guidelines for error handling.

use std::path::PathBuf;

/// Details of a single server spawn failure.
#[derive(Debug, Clone)]
pub struct ServerSpawnFailure {
    /// Language ID of the failed server.
    pub language_id: String,
    /// Command that was attempted.
    pub command: String,
    /// Error message describing the failure.
    pub message: String,
}

impl std::fmt::Display for ServerSpawnFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.language_id, self.command, self.message
        )
    }
}

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

    /// No LSP server is currently configured.
    #[error("no LSP server configured")]
    NoServerConfigured,

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

    /// Partial server initialization - some servers failed but at least one succeeded.
    #[error("some LSP servers failed to initialize: {failed_count}/{total_count} servers")]
    PartialServerInit {
        /// Number of servers that failed.
        failed_count: usize,
        /// Total number of configured servers.
        total_count: usize,
        /// Details of each failure.
        failures: Vec<ServerSpawnFailure>,
    },

    /// All configured LSP servers failed to initialize.
    #[error("all LSP servers failed to initialize ({count} configured)")]
    AllServersFailedToInit {
        /// Number of servers that were configured.
        count: usize,
        /// Details of each failure.
        failures: Vec<ServerSpawnFailure>,
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

    #[test]
    fn test_server_spawn_failure_display() {
        let failure = ServerSpawnFailure {
            language_id: "rust".to_string(),
            command: "rust-analyzer".to_string(),
            message: "No such file or directory".to_string(),
        };
        assert_eq!(
            failure.to_string(),
            "rust (rust-analyzer): No such file or directory"
        );
    }

    #[test]
    fn test_server_spawn_failure_debug() {
        let failure = ServerSpawnFailure {
            language_id: "python".to_string(),
            command: "pyright".to_string(),
            message: "command not found".to_string(),
        };
        let debug_str = format!("{failure:?}");
        assert!(debug_str.contains("python"));
        assert!(debug_str.contains("pyright"));
        assert!(debug_str.contains("command not found"));
    }

    #[test]
    fn test_server_spawn_failure_clone() {
        let failure = ServerSpawnFailure {
            language_id: "typescript".to_string(),
            command: "tsserver".to_string(),
            message: "failed to start".to_string(),
        };
        let cloned = failure.clone();
        assert_eq!(failure.language_id, cloned.language_id);
        assert_eq!(failure.command, cloned.command);
        assert_eq!(failure.message, cloned.message);
    }

    #[test]
    fn test_error_display_partial_server_init() {
        let err = Error::PartialServerInit {
            failed_count: 2,
            total_count: 3,
            failures: vec![],
        };
        assert_eq!(
            err.to_string(),
            "some LSP servers failed to initialize: 2/3 servers"
        );
    }

    #[test]
    fn test_error_display_all_servers_failed_to_init() {
        let err = Error::AllServersFailedToInit {
            count: 2,
            failures: vec![],
        };
        assert_eq!(
            err.to_string(),
            "all LSP servers failed to initialize (2 configured)"
        );
    }

    #[test]
    fn test_error_all_servers_failed_with_failures() {
        let failures = vec![
            ServerSpawnFailure {
                language_id: "rust".to_string(),
                command: "rust-analyzer".to_string(),
                message: "not found".to_string(),
            },
            ServerSpawnFailure {
                language_id: "python".to_string(),
                command: "pyright".to_string(),
                message: "permission denied".to_string(),
            },
        ];

        let err = Error::AllServersFailedToInit { count: 2, failures };

        assert!(err.to_string().contains("all LSP servers failed"));
        assert!(err.to_string().contains("2 configured"));
    }

    #[test]
    fn test_error_partial_server_init_with_failures() {
        let failures = vec![ServerSpawnFailure {
            language_id: "python".to_string(),
            command: "pyright".to_string(),
            message: "not found".to_string(),
        }];

        let err = Error::PartialServerInit {
            failed_count: 1,
            total_count: 2,
            failures,
        };

        assert!(err.to_string().contains("some LSP servers failed"));
        assert!(err.to_string().contains("1/2"));
    }
}
