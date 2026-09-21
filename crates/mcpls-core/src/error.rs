//! Error types for mcpls-core.
//!
//! This module defines the canonical error type for the library,
//! following the Microsoft Rust Guidelines for error handling.

use std::path::PathBuf;

use crate::config::{ServerId, ToolKind};

/// Rewrites an LSP server's raw error message for display to the MCP caller,
/// replacing rust-analyzer's "Invalid offset" internal error with a clean,
/// client-appropriate message.
///
/// rust-analyzer returns this `Debug`-formatted internal error (embedding its
/// `LineCol` struct and the line index's byte length, e.g. `"Invalid offset
/// LineCol { line: 2291, col: 0 } (line index length: 100417)"`) when a
/// position-based request's `line` or `character` falls outside the target
/// document. Every other [`Error`] variant produces a clean message; this
/// function keeps [`Error::LspServerError`]'s `Display` impl consistent with
/// that convention instead of forwarding the upstream server's internals
/// verbatim.
///
/// Matches via `contains` rather than `starts_with`: rust-analyzer's error
/// travels through `anyhow`/`lsp_server` before reaching mcpls, so a future
/// upstream `.context(...)` wrapper (or a truncation prefix added on the
/// mcpls side) could prepend text ahead of `"Invalid offset LineCol"` without
/// mcpls's control -- `contains` keeps the guard robust to that at no extra
/// cost. Deliberately not also gated on the JSON-RPC error `code`: this error
/// class has been observed under both `-32603` (internal error) and `-32803`
/// (`RequestFailed`) across rust-analyzer versions, so a code condition would
/// make the guard more fragile, not less.
fn sanitize_lsp_server_message(message: &str) -> String {
    if message.contains("Invalid offset LineCol") {
        "position out of range for this document".to_string()
    } else {
        message.to_string()
    }
}

/// Details of a single server spawn failure.
#[derive(Debug, Clone)]
pub struct ServerSpawnFailure {
    /// Routing identity of the failed server.
    pub server_id: ServerId,
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
            "{} [{}] ({}): {}",
            self.server_id, self.language_id, self.command, self.message
        )
    }
}

/// The main error type for mcpls-core operations.
///
/// This enum is `#[non_exhaustive]`: downstream crates that match on it must
/// include a wildcard arm. New variants (such as [`Error::ServerInitializing`])
/// can then be added without further breaking changes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// LSP server failed to initialize.
    #[error("LSP server initialization failed: {message}")]
    LspInitFailed {
        /// Description of the initialization failure.
        message: String,
    },

    /// LSP server returned an error response.
    #[error("LSP server error: {code} - {}", sanitize_lsp_server_message(message))]
    LspServerError {
        /// JSON-RPC error code.
        code: i32,
        /// Raw error message from the server, kept verbatim for diagnostics
        /// (logging, `Debug`, pattern matching). The `Display` impl for this
        /// variant rewrites known-internal upstream text before it reaches
        /// an MCP caller, so this field is not always what the caller sees.
        message: String,
        /// Optional additional data from the JSON-RPC error object.
        data: Option<serde_json::Value>,
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

    /// A server is configured for the language, but no server claims this
    /// specific tool (either no server lists it in `handles` and there is no
    /// catch-all, or the server that claimed it failed to spawn with no live
    /// catch-all to rebind to).
    #[error("no server handles tool '{tool}' for language '{language_id}'")]
    NoServerForTool {
        /// Language ID the request was for.
        language_id: String,
        /// Tool that no server claims.
        tool: ToolKind,
    },

    /// LSP server for the language is configured but still initializing.
    #[error(
        "LSP server '{server_id}' is still initializing (large project load in progress); wait and retry the request (this may take a few minutes on large projects)"
    )]
    ServerInitializing {
        /// Routing identity of the server that has not yet registered.
        server_id: ServerId,
    },

    /// A workspace-wide tool (one with no file to resolve a language from,
    /// e.g. `workspace_symbol_search`) could not be routed because at least
    /// one expected LSP server has not registered yet. Unlike
    /// [`Error::ServerInitializing`], resolution never narrowed down to a
    /// single candidate server, so no `server_id` is available.
    #[error(
        "LSP servers are still initializing (large project load in progress); wait and retry the request (this may take a few minutes on large projects)"
    )]
    WorkspaceServersInitializing,

    /// No LSP server is currently configured.
    #[error("no LSP server configured")]
    NoServerConfigured,

    /// At least one server is configured somewhere in the workspace, but
    /// none of them claims a workspace-wide tool that has no file to
    /// resolve a language from (e.g. `workspace_symbol_search`). The
    /// language-less counterpart of [`Error::NoServerForTool`].
    #[error("no server handles tool '{tool}' (no server's `handles` list or catch-all claims it)")]
    NoServerForWorkspaceTool {
        /// Tool that no server claims anywhere in the workspace.
        tool: ToolKind,
    },

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

    /// TOML deserialization error.
    #[error("TOML parsing error: {0}")]
    TomlDe(#[from] toml::de::Error),

    /// TOML serialization error.
    #[error("TOML serialization error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    /// LSP client transport error.
    #[error("transport error: {0}")]
    Transport(String),

    /// Request timeout.
    #[error("request timed out after {0} seconds")]
    Timeout(u64),

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

    /// Server process terminated unexpectedly.
    #[error("LSP server process terminated unexpectedly")]
    ServerTerminated,

    /// A crashed server could not be automatically respawned.
    ///
    /// Distinct from [`Self::ServerTerminated`] so a caller (or a log
    /// reader) can tell "the connection just died" apart from "mcpls tried
    /// to bring it back and could not" -- e.g. no respawn config was ever
    /// registered for it, or it is crash-looping and is being backed off.
    #[error("LSP server '{server_id}' is unavailable: {reason}")]
    ServerUnavailable {
        /// Routing identity of the server that could not be respawned.
        server_id: ServerId,
        /// Human-readable reason the respawn did not proceed.
        reason: String,
    },

    /// Invalid tool parameters provided.
    #[error("invalid tool parameters: {0}")]
    InvalidToolParams(String),

    /// File I/O error occurred.
    ///
    /// See [`Error::mcp_error_kind`] for the JSON-RPC classification: a
    /// `source.kind() == ErrorKind::NotFound` failure -- whether `path` was
    /// freshly supplied in this request or was tracked from an earlier one
    /// and has since been deleted/moved on disk -- is caller-fault; any
    /// other IO failure is not.
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

    /// No workspace roots are configured, so path-taking operations are
    /// rejected outright rather than allowed unrestricted (fail closed).
    #[error("no workspace roots configured: refusing access to {0}")]
    NoWorkspaceRoots(PathBuf),

    /// Document limit exceeded.
    #[error(
        "document limit exceeded: {current}/{max} (raise workspace.max_documents in config to increase this)"
    )]
    DocumentLimitExceeded {
        /// Current number of documents.
        current: usize,
        /// Maximum allowed documents.
        max: usize,
    },

    /// File size limit exceeded.
    #[error(
        "file size limit exceeded: {size} bytes, max {max} bytes (raise workspace.max_file_size in config to increase this)"
    )]
    FileSizeLimitExceeded {
        /// Actual file size.
        size: u64,
        /// Maximum allowed size.
        max: u64,
    },

    /// Path exists but does not refer to a regular file (e.g. a FIFO or a
    /// character/block device).
    ///
    /// mcpls refuses to read such paths: their reported size does not bound
    /// how much data reading them could produce, and opening some of them
    /// for reading can block indefinitely waiting for a peer. A Unix domain
    /// socket special file is not covered by this variant -- `open(2)` on
    /// one fails outright (`ENXIO`) before the file-type check that produces
    /// this error ever runs, so it surfaces as [`Self::FileIo`] instead.
    #[error("not a regular file: {0}")]
    NotARegularFile(PathBuf),

    /// All configured LSP servers failed to initialize.
    #[error("all LSP servers failed to initialize ({count} configured)")]
    AllServersFailedToInit {
        /// Number of servers that were configured.
        count: usize,
        /// Details of each failure.
        failures: Vec<ServerSpawnFailure>,
    },

    /// No LSP servers available (none configured or all failed).
    #[error("{0}")]
    NoServersAvailable(String),

    /// The server routed for this request does not advertise support for the
    /// requested LSP capability (e.g. no `renameProvider` in its
    /// `ServerCapabilities`).
    #[error("server '{server_id}' does not support capability '{capability}'")]
    CapabilityNotSupported {
        /// Routing identity of the server that lacks the capability.
        server_id: ServerId,
        /// The missing LSP capability's name (e.g. `"renameProvider"`), the
        /// `ServerCapabilities` field mcpls checked.
        capability: &'static str,
    },

    /// The routed server has an active signal indicating its initial
    /// workspace-load/indexing phase is still in progress, and the bounded
    /// wait for it to finish elapsed before it completed. Returned instead
    /// of an unqualified empty/`null` result so a caller cannot mistake
    /// "index not ready yet" for "this position/symbol genuinely has
    /// nothing here".
    #[error(
        "LSP server '{server_id}' is still indexing the workspace after {elapsed_secs}s; wait and retry the request"
    )]
    WorkspaceIndexing {
        /// Routing identity of the server still indexing.
        server_id: ServerId,
        /// How long mcpls waited for readiness before giving up.
        elapsed_secs: u64,
    },
}

/// Bespoke JSON-RPC code for [`Error::WorkspaceIndexing`].
///
/// Picked clear of rmcp's `-32002`/`-32020..-32022`; the range is convention, not a registry.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::WORKSPACE_INDEXING_ERROR_CODE;
///
/// assert_eq!(WORKSPACE_INDEXING_ERROR_CODE, -32050);
/// ```
pub const WORKSPACE_INDEXING_ERROR_CODE: i32 = -32050;

/// Bespoke JSON-RPC code for [`Error::ServerInitializing`].
///
/// Distinct from [`WORKSPACE_INDEXING_ERROR_CODE`] so a client can tell "the
/// server hasn't registered yet" apart from "the server registered but is
/// still indexing" -- both retryable, but for different reasons.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{SERVER_INITIALIZING_ERROR_CODE, WORKSPACE_INDEXING_ERROR_CODE};
///
/// assert_eq!(SERVER_INITIALIZING_ERROR_CODE, -32051);
/// assert_ne!(SERVER_INITIALIZING_ERROR_CODE, WORKSPACE_INDEXING_ERROR_CODE);
/// ```
pub const SERVER_INITIALIZING_ERROR_CODE: i32 = -32051;

/// JSON-RPC error-code classification for an [`Error`], returned by
/// [`Error::mcp_error_kind`].
///
/// mcpls-core has no dependency on the MCP transport crate, so this carries
/// only plain data; `crate::mcp` is responsible for turning it into the
/// actual wire-level error type.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{Error, McpErrorKind};
///
/// let err = Error::InvalidToolParams("missing `file_path`".to_string());
/// assert_eq!(err.mcp_error_kind(), McpErrorKind::InvalidParams);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpErrorKind {
    /// Caller-fault: the request itself was invalid. Maps to JSON-RPC
    /// `-32602` (`INVALID_PARAMS`).
    InvalidParams,
    /// A transient, retryable server-side condition, distinct from a crash.
    /// Maps to a bespoke JSON-RPC `code` with a structured `data` payload a
    /// caller can act on mechanically, rather than the generic
    /// `INTERNAL_ERROR`.
    Retryable {
        /// Bespoke JSON-RPC error code.
        code: i32,
        /// Structured details about the retryable condition.
        data: serde_json::Value,
    },
    /// An unexpected server-side failure. Maps to JSON-RPC `-32603`
    /// (`INTERNAL_ERROR`).
    Internal,
}

impl Error {
    /// Classify this error for JSON-RPC error-code mapping.
    ///
    /// Matched exhaustively with no wildcard arm: a newly added [`Error`]
    /// variant must be given an explicit classification here instead of
    /// silently defaulting to [`McpErrorKind::Internal`].
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::config::ServerId;
    /// use mcpls_core::error::{Error, McpErrorKind};
    ///
    /// let err = Error::WorkspaceIndexing {
    ///     server_id: ServerId::from("rust"),
    ///     elapsed_secs: 30,
    /// };
    /// let McpErrorKind::Retryable { code, data } = err.mcp_error_kind() else {
    ///     panic!("expected a retryable classification");
    /// };
    /// assert_eq!(data["serverId"], "rust");
    /// ```
    #[must_use]
    pub fn mcp_error_kind(&self) -> McpErrorKind {
        match self {
            Self::InvalidToolParams(_)
            | Self::PathOutsideWorkspace(_)
            | Self::NotARegularFile(_)
            | Self::InvalidUri(_)
            | Self::DocumentNotFound(_)
            | Self::FileSizeLimitExceeded { .. } => McpErrorKind::InvalidParams,

            // A path that doesn't exist -- whether freshly supplied in this
            // request or tracked from an earlier request and then
            // deleted/moved on disk since -- is caller-fault, same as
            // `DocumentNotFound`, and matches the MCP spec's expectation
            // that resource-not-found map to INVALID_PARAMS, not
            // INTERNAL_ERROR (rmcp's `read_resource` handling, SEP-2164).
            // Any other IO failure (permission denied, etc.) reaching here is
            // a genuine server-side problem the caller cannot fix by
            // changing their request.
            Self::FileIo { source, .. } => {
                if source.kind() == std::io::ErrorKind::NotFound {
                    McpErrorKind::InvalidParams
                } else {
                    McpErrorKind::Internal
                }
            }

            Self::WorkspaceIndexing {
                server_id,
                elapsed_secs,
            } => McpErrorKind::Retryable {
                code: WORKSPACE_INDEXING_ERROR_CODE,
                data: serde_json::json!({
                    "serverId": server_id.as_str(),
                    "elapsedSecs": elapsed_secs,
                }),
            },
            Self::ServerInitializing { server_id } => McpErrorKind::Retryable {
                code: SERVER_INITIALIZING_ERROR_CODE,
                data: serde_json::json!({
                    "serverId": server_id.as_str(),
                }),
            },
            // Same condition as `ServerInitializing` -- an expected LSP
            // server hasn't registered yet, retry -- just without a single
            // candidate server narrowed down (see the variant's doc), so
            // there's no `serverId` to report.
            Self::WorkspaceServersInitializing => McpErrorKind::Retryable {
                code: SERVER_INITIALIZING_ERROR_CODE,
                data: serde_json::json!({}),
            },

            Self::LspInitFailed { .. }
            | Self::LspServerError { .. }
            | Self::McpServer(_)
            | Self::NoServerForLanguage(_)
            | Self::NoServerForTool { .. }
            | Self::NoServerConfigured
            | Self::NoServerForWorkspaceTool { .. }
            | Self::ConfigNotFound(_)
            | Self::InvalidConfig(_)
            | Self::Io(_)
            | Self::Json(_)
            | Self::TomlDe(_)
            | Self::TomlSer(_)
            | Self::Transport(_)
            | Self::Timeout(_)
            | Self::ServerSpawnFailed { .. }
            | Self::LspProtocolError(_)
            | Self::ServerTerminated
            | Self::ServerUnavailable { .. }
            | Self::NoWorkspaceRoots(_)
            // Unlike `FileSizeLimitExceeded`, this fires on aggregate tracker
            // state, not this request's params -- it can succeed unchanged
            // once other documents close, so `InvalidParams` is wrong; not
            // `Retryable` either, since nothing evicts documents on a timer.
            | Self::DocumentLimitExceeded { .. }
            | Self::AllServersFailedToInit { .. }
            | Self::NoServersAvailable(_)
            | Self::CapabilityNotSupported { .. } => McpErrorKind::Internal,
        }
    }
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
            data: None,
        };
        assert_eq!(
            err.to_string(),
            "LSP server error: -32600 - Invalid request"
        );
    }

    #[test]
    fn test_error_display_lsp_server_error_sanitizes_invalid_offset() {
        let err = Error::LspServerError {
            code: -32603,
            message: "Invalid offset LineCol { line: 2291, col: 0 } (line index length: 100417)"
                .to_string(),
            data: None,
        };
        assert_eq!(
            err.to_string(),
            "LSP server error: -32603 - position out of range for this document"
        );
    }

    #[test]
    fn test_error_display_lsp_server_error_sanitizes_wrapped_invalid_offset() {
        // Guards the `contains` (not `starts_with`) match: an upstream
        // wrapper (e.g. an `anyhow::Context`) or a future mcpls-side prefix
        // could prepend text ahead of rust-analyzer's raw message.
        let err = Error::LspServerError {
            code: -32803,
            message: "request handler panicked: Invalid offset LineCol { line: 5, col: 0 } \
                      (line index length: 3)"
                .to_string(),
            data: None,
        };
        assert_eq!(
            err.to_string(),
            "LSP server error: -32803 - position out of range for this document"
        );
    }

    #[test]
    fn test_error_display_lsp_server_error_passes_through_unrelated_message() {
        let err = Error::LspServerError {
            code: -32602,
            message: "Invalid params: expected object".to_string(),
            data: None,
        };
        assert_eq!(
            err.to_string(),
            "LSP server error: -32602 - Invalid params: expected object"
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
    fn test_error_display_workspace_servers_initializing() {
        let err = Error::WorkspaceServersInitializing;
        assert!(err.to_string().contains("still initializing"));
    }

    #[test]
    fn test_error_display_no_server_for_workspace_tool() {
        let err = Error::NoServerForWorkspaceTool {
            tool: crate::config::ToolKind::WorkspaceSymbols,
        };
        assert!(err.to_string().contains("workspace_symbols"));
        assert!(err.to_string().contains("no server's `handles` list"));
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
        assert_eq!(
            err.to_string(),
            "document limit exceeded: 150/100 (raise workspace.max_documents in config to increase this)"
        );
    }

    #[test]
    fn test_error_display_file_size_limit() {
        let err = Error::FileSizeLimitExceeded {
            size: 20_000_000,
            max: 10_000_000,
        };
        assert_eq!(
            err.to_string(),
            "file size limit exceeded: 20000000 bytes, max 10000000 bytes (raise workspace.max_file_size in config to increase this)"
        );
    }

    #[test]
    fn test_error_display_not_a_regular_file() {
        let err = Error::NotARegularFile(PathBuf::from("/tmp/some.fifo"));
        assert_eq!(err.to_string(), "not a regular file: /tmp/some.fifo");
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
    fn test_error_from_toml_de() {
        let toml_str = "[invalid toml";
        let toml_err = toml::from_str::<toml::Value>(toml_str).unwrap_err();
        let err: Error = toml_err.into();
        assert!(matches!(err, Error::TomlDe(_)));
    }

    #[test]
    fn test_result_type_alias() {
        fn _returns_error() -> Result<i32> {
            Err(Error::InvalidConfig("test error".to_string()))
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
            server_id: ServerId::from("rust"),
            language_id: "rust".to_string(),
            command: "rust-analyzer".to_string(),
            message: "No such file or directory".to_string(),
        };
        assert_eq!(
            failure.to_string(),
            "rust [rust] (rust-analyzer): No such file or directory"
        );
    }

    #[test]
    fn test_server_spawn_failure_debug() {
        let failure = ServerSpawnFailure {
            server_id: ServerId::from("python"),
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
            server_id: ServerId::from("typescript"),
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
                server_id: ServerId::from("rust"),
                language_id: "rust".to_string(),
                command: "rust-analyzer".to_string(),
                message: "not found".to_string(),
            },
            ServerSpawnFailure {
                server_id: ServerId::from("python"),
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
    fn test_error_display_no_servers_available() {
        let err =
            Error::NoServersAvailable("none configured or all failed to initialize".to_string());
        assert_eq!(
            err.to_string(),
            "none configured or all failed to initialize"
        );
    }

    #[test]
    fn test_error_no_servers_available_with_custom_message() {
        let custom_msg = "none configured or all failed to initialize";
        let err = Error::NoServersAvailable(custom_msg.to_string());
        assert_eq!(err.to_string(), custom_msg);
    }

    #[test]
    fn test_error_display_capability_not_supported() {
        let err = Error::CapabilityNotSupported {
            server_id: ServerId::from("rust"),
            capability: "renameProvider",
        };
        assert_eq!(
            err.to_string(),
            "server 'rust' does not support capability 'renameProvider'"
        );
    }

    #[test]
    fn test_error_display_workspace_indexing() {
        let err = Error::WorkspaceIndexing {
            server_id: ServerId::from("rust"),
            elapsed_secs: 30,
        };
        assert_eq!(
            err.to_string(),
            "LSP server 'rust' is still indexing the workspace after 30s; wait and retry the request"
        );
    }

    /// #479: caller-fault variants must classify as `InvalidParams`, not fall
    /// through to the generic `Internal` bucket.
    #[test]
    fn test_mcp_error_kind_caller_fault_variants_are_invalid_params() {
        let caller_fault_errors = vec![
            Error::InvalidToolParams("bad params".to_string()),
            Error::PathOutsideWorkspace(PathBuf::from("/etc/passwd")),
            Error::NotARegularFile(PathBuf::from("/dev/null")),
            Error::InvalidUri("not a uri".to_string()),
            Error::DocumentNotFound(PathBuf::from("/missing.rs")),
            Error::FileSizeLimitExceeded { size: 100, max: 10 },
        ];

        for err in caller_fault_errors {
            assert_eq!(
                err.mcp_error_kind(),
                McpErrorKind::InvalidParams,
                "expected {err:?} to classify as InvalidParams"
            );
        }
    }

    #[test]
    fn test_mcp_error_kind_workspace_indexing_is_retryable_with_dedicated_code() {
        let err = Error::WorkspaceIndexing {
            server_id: ServerId::from("rust"),
            elapsed_secs: 30,
        };
        let McpErrorKind::Retryable { code, data } = err.mcp_error_kind() else {
            panic!("expected WorkspaceIndexing to classify as Retryable");
        };
        assert_eq!(code, WORKSPACE_INDEXING_ERROR_CODE);
        assert_eq!(data["serverId"], "rust");
        assert_eq!(data["elapsedSecs"], 30);
    }

    #[test]
    fn test_mcp_error_kind_server_initializing_is_retryable_with_dedicated_code() {
        let err = Error::ServerInitializing {
            server_id: ServerId::from("python"),
        };
        let McpErrorKind::Retryable { code, data } = err.mcp_error_kind() else {
            panic!("expected ServerInitializing to classify as Retryable");
        };
        assert_eq!(code, SERVER_INITIALIZING_ERROR_CODE);
        assert_eq!(data["serverId"], "python");
        assert_ne!(
            code, WORKSPACE_INDEXING_ERROR_CODE,
            "ServerInitializing must be distinguishable on the wire from WorkspaceIndexing"
        );
    }

    /// `WorkspaceServersInitializing` is `ServerInitializing`'s counterpart
    /// for a resolution that never narrowed down to a single server (see the
    /// variant's doc comment), so it must be retryable too -- a client that
    /// auto-retries on the bespoke retryable code must not treat this as a
    /// hard failure just because no `server_id` was available.
    #[test]
    fn test_mcp_error_kind_workspace_servers_initializing_is_retryable() {
        let err = Error::WorkspaceServersInitializing;
        let McpErrorKind::Retryable { code, .. } = err.mcp_error_kind() else {
            panic!("expected WorkspaceServersInitializing to classify as Retryable");
        };
        assert_eq!(code, SERVER_INITIALIZING_ERROR_CODE);
    }

    #[test]
    fn test_mcp_error_kind_unretained_variants_stay_internal() {
        let internal_errors = vec![
            Error::NoServerForLanguage("python".to_string()),
            Error::NoServerForTool {
                language_id: "rust".to_string(),
                tool: crate::config::ToolKind::Hover,
            },
            Error::CapabilityNotSupported {
                server_id: ServerId::from("rust"),
                capability: "renameProvider",
            },
            Error::NoWorkspaceRoots(PathBuf::from("/tmp")),
            Error::DocumentLimitExceeded {
                current: 150,
                max: 100,
            },
        ];

        for err in internal_errors {
            assert_eq!(
                err.mcp_error_kind(),
                McpErrorKind::Internal,
                "expected {err:?} to classify as Internal"
            );
        }
    }

    /// #479 regression: a client-supplied path that doesn't exist (the
    /// common case behind `validate_path_against_roots`'s `canonicalize()`
    /// failure) must classify as caller-fault, matching `DocumentNotFound`.
    #[test]
    fn test_mcp_error_kind_file_io_not_found_is_invalid_params() {
        let err = Error::FileIo {
            path: PathBuf::from("/no/such/file.rs"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file or directory"),
        };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::InvalidParams);
    }

    /// Counterpart: a non-not-found IO failure (permission denied, etc.) is
    /// a genuine server-side problem, not something the caller can fix by
    /// changing their request.
    #[test]
    fn test_mcp_error_kind_file_io_other_kind_stays_internal() {
        let err = Error::FileIo {
            path: PathBuf::from("/root/secret.rs"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied"),
        };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }
}
