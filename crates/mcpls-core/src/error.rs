//! Error types for mcpls-core.
//!
//! This module defines the canonical error type for the library,
//! following the Microsoft Rust Guidelines for error handling.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::config::{BuiltinServer, ServerId, ToolKind};
use crate::lsp::MAX_ERROR_MESSAGE_CALLER_BYTES;
use crate::util::{escape_control, truncate_str};

/// Host platform, as far as [`NotFoundGuidance`] cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    Windows,
    Other,
}

impl Platform {
    const CURRENT: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Other
    };
}

/// Display suffix explaining how to fix a missing LSP server executable.
struct NotFoundGuidance<'a>(&'a str, Platform);

impl fmt::Display for NotFoundGuidance<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(command, platform) = *self;
        if Path::new(command)
            .parent()
            .is_some_and(|p| !p.as_os_str().is_empty())
        {
            return f.write_str("; check that the configured path exists");
        }
        write!(
            f,
            "; '{command}' is not on the PATH mcpls runs with -- if it is installed, add its \
             directory to the MCP client's PATH or set `command` to an absolute path"
        )?;
        let Some(builtin) = BuiltinServer::from_command(command) else {
            return Ok(());
        };
        if platform == Platform::Windows && builtin.is_npm_package() {
            write!(
                f,
                " (npm-installed servers need the `.cmd` name, e.g. `{command}.cmd`)"
            )?;
        }
        write!(f, "; otherwise install it: {}", builtin.install_hint())
    }
}

/// Substring rust-analyzer's raw error text carries when a position-based
/// request's `line`/`character` falls outside the target document. Shared
/// between [`sanitize_lsp_server_message`] (rewrites the message shown to the
/// caller) and [`Error::mcp_error_kind`] (classifies this shape of
/// [`Error::LspServerError`] as caller-fault) so the two stay in sync.
const INVALID_OFFSET_MARKER: &str = "Invalid offset LineCol";

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
    if message.contains(INVALID_OFFSET_MARKER) {
        "position out of range for this document".to_string()
    } else {
        escape_control(message).into_owned()
    }
}

/// The raw LSP error behind a position-out-of-range rewrite, carried as the
/// JSON-RPC `data` of the resulting `-32602` error.
///
/// [`Error::LspServerError`]'s `Display` replaces rust-analyzer's internal
/// "Invalid offset" text with a clean message; this keeps the original
/// reachable for the caller. The fields are private so the message can only
/// be built through [`Self::new`], which bounds it.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::RewrittenServerError;
///
/// let raw = RewrittenServerError::new(-32603, "Invalid offset LineCol { line: 9, col: 0 }");
/// assert_eq!(raw.code(), -32603);
/// assert_eq!(
///     serde_json::to_value(&raw).unwrap(),
///     serde_json::json!({"code": -32603, "raw_message": "Invalid offset LineCol { line: 9, col: 0 }"})
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RewrittenServerError {
    code: i32,
    raw_message: String,
}

impl RewrittenServerError {
    /// Build from the server's JSON-RPC `code` and raw message, truncating
    /// the message to the budget used for text forwarded to MCP callers.
    #[must_use]
    pub fn new(code: i32, raw_message: &str) -> Self {
        Self {
            code,
            raw_message: truncate_str(raw_message, MAX_ERROR_MESSAGE_CALLER_BYTES),
        }
    }

    /// The server's JSON-RPC error code.
    #[must_use]
    pub const fn code(&self) -> i32 {
        self.code
    }

    /// The server's raw message, bounded by [`Self::new`].
    #[must_use]
    pub fn raw_message(&self) -> &str {
        &self.raw_message
    }
}

/// Why a configured LSP server never registered during startup.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::StartupFailure;
///
/// assert!(StartupFailure::InitTaskPanicked.to_string().contains("panicked"));
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum StartupFailure {
    /// Spawning or initializing the server failed.
    Spawn(Arc<Error>),
    /// The background initialization task panicked before the server
    /// registered.
    InitTaskPanicked,
}

impl fmt::Display for StartupFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => error.fmt(f),
            Self::InitTaskPanicked => {
                f.write_str("the initialization task panicked (see the mcpls log)")
            }
        }
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
    /// Why the server never registered.
    pub reason: StartupFailure,
}

impl fmt::Display for ServerSpawnFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} [{}] ({}): {}",
            self.server_id, self.language_id, self.command, self.reason
        )
    }
}

/// `Display` adapter listing every failure, separated by `; `.
struct FailureList<'a>(&'a [ServerSpawnFailure]);

impl fmt::Display for FailureList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, failure) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            failure.fmt(f)?;
        }
        Ok(())
    }
}

/// `Display` suffix for [`Error::ServerExitedDuringInit`] carrying the exit
/// status and, for builtin servers with a known early-exit cause, a hint.
struct EarlyExitDetail<'a>(&'a str, Option<i32>);

impl fmt::Display for EarlyExitDetail<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(command, exit_code) = *self;
        match exit_code {
            Some(code) => write!(f, " with exit code {code}")?,
            None => f.write_str(" (terminated by a signal)")?,
        }
        BuiltinServer::from_command(command)
            .and_then(BuiltinServer::early_exit_hint)
            .map_or(Ok(()), |hint| write!(f, "; {hint}"))
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

    /// The server configured for this request failed to start and, because
    /// startup failures are never retried, will not become available until
    /// mcpls is restarted.
    ///
    /// Boxed to keep [`Error`] small.
    #[error(
        "LSP server '{}' for language '{}' failed to start: {}; restart mcpls after fixing it (startup failures are not retried)",
        .0.server_id, .0.language_id, .0.reason
    )]
    ServerFailedToStart(Box<ServerSpawnFailure>),

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

    /// LSP server executable was not found.
    ///
    /// Distinct from [`Error::ServerSpawnFailed`] so the message can carry
    /// PATH and install guidance.
    #[error("failed to spawn LSP server '{command}': {source}{}", NotFoundGuidance(.command, Platform::CURRENT))]
    ServerNotFound {
        /// Command that could not be found.
        command: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// LSP protocol error during message parsing.
    #[error("LSP protocol error: {}", escape_control(.0))]
    LspProtocolError(String),

    /// Invalid URI format.
    #[error("invalid URI: {0}")]
    InvalidUri(String),

    /// Server process terminated unexpectedly.
    #[error("LSP server process terminated unexpectedly")]
    ServerTerminated,

    /// LSP server shutdown did not complete before its deadline.
    #[error("LSP server shutdown did not complete before its deadline")]
    ShutdownTimeout,

    /// LSP server process exited before completing the `initialize`
    /// handshake.
    #[error("LSP server '{command}' exited during initialization{}", EarlyExitDetail(.command, *.exit_code))]
    ServerExitedDuringInit {
        /// Command that was spawned.
        command: String,
        /// Exit code, or `None` if the process was terminated by a signal.
        exit_code: Option<i32>,
    },

    /// A crashed server could not be automatically respawned.
    ///
    /// Distinct from [`Self::ServerTerminated`] so a caller (or a log
    /// reader) can tell "the connection just died" apart from "mcpls tried
    /// to bring it back and could not" -- it is crash-looping and is being
    /// backed off.
    #[error(
        "LSP server '{server_id}' is unavailable: crash-looping, retry in {:.1}s",
        .retry_in.as_secs_f64()
    )]
    ServerUnavailable {
        /// Routing identity of the server that could not be respawned.
        server_id: ServerId,
        /// Remaining backoff before the next respawn attempt.
        retry_in: Duration,
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

    /// Resource-subscription limit exceeded for the session.
    ///
    /// See [`Error::mcp_error_kind`] for the JSON-RPC classification: same
    /// shape as [`Self::DocumentLimitExceeded`] -- fires on aggregate
    /// per-session tracker state, not this request's params -- so it is
    /// classified the same way, not `InvalidParams`.
    #[error("subscription limit of {max} reached")]
    SubscriptionLimitReached {
        /// Maximum number of subscriptions allowed per session.
        max: usize,
    },

    /// Too many concurrent `subscriptions/listen` streams are open.
    ///
    /// Transient: a stream closing frees a slot, so it is classified as
    /// retryable (see [`Error::mcp_error_kind`]).
    #[error("listen stream limit of {max} reached; retry once another stream closes")]
    ListenStreamsExhausted {
        /// Maximum number of concurrent listen streams.
        max: usize,
    },

    /// A `subscriptions/listen` request asked for more resource URIs than a
    /// single stream may watch.
    #[error(
        "subscriptions/listen request exceeds the limit of {max} resource URIs or their total size budget"
    )]
    ListenFilterTooLarge {
        /// Maximum number of resource URIs per listen stream.
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
    #[error("all LSP servers failed to initialize: {}", FailureList(failures))]
    AllServersFailedToInit {
        /// Details of each failure.
        failures: Vec<ServerSpawnFailure>,
    },

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

/// Bespoke JSON-RPC code for a resource subscription request rejected
/// because it was served over rmcp's stateless per-request HTTP path (#482).
///
/// Same convention range as [`WORKSPACE_INDEXING_ERROR_CODE`]/
/// [`SERVER_INITIALIZING_ERROR_CODE`], next unused slot.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{
///     SERVER_INITIALIZING_ERROR_CODE, STATELESS_SUBSCRIPTION_ERROR_CODE,
///     WORKSPACE_INDEXING_ERROR_CODE,
/// };
///
/// assert_eq!(STATELESS_SUBSCRIPTION_ERROR_CODE, -32052);
/// assert_ne!(STATELESS_SUBSCRIPTION_ERROR_CODE, WORKSPACE_INDEXING_ERROR_CODE);
/// assert_ne!(STATELESS_SUBSCRIPTION_ERROR_CODE, SERVER_INITIALIZING_ERROR_CODE);
/// ```
pub const STATELESS_SUBSCRIPTION_ERROR_CODE: i32 = -32052;

/// Bespoke JSON-RPC code for [`Error::ListenStreamsExhausted`].
///
/// Same convention range as [`WORKSPACE_INDEXING_ERROR_CODE`], next unused
/// slot after [`STATELESS_SUBSCRIPTION_ERROR_CODE`].
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{
///     LISTEN_STREAMS_EXHAUSTED_ERROR_CODE, STATELESS_SUBSCRIPTION_ERROR_CODE,
/// };
///
/// assert_eq!(LISTEN_STREAMS_EXHAUSTED_ERROR_CODE, -32053);
/// assert_ne!(LISTEN_STREAMS_EXHAUSTED_ERROR_CODE, STATELESS_SUBSCRIPTION_ERROR_CODE);
/// ```
pub const LISTEN_STREAMS_EXHAUSTED_ERROR_CODE: i32 = -32053;

/// Structured `data` payload of a retryable JSON-RPC error.
///
/// Each variant pairs a bespoke error code ([`Self::code`]) with its own
/// payload, so a code can never be sent with another variant's data. Keys
/// are `snake_case`; the wire form is the variant's fields as a bare object.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ServerId;
/// use mcpls_core::error::{RetryableErrorData, WORKSPACE_INDEXING_ERROR_CODE};
///
/// let data = RetryableErrorData::WorkspaceIndexing {
///     server_id: ServerId::from("rust"),
///     elapsed_secs: 30,
/// };
/// assert_eq!(data.code(), WORKSPACE_INDEXING_ERROR_CODE);
/// assert_eq!(
///     serde_json::to_value(&data).unwrap(),
///     serde_json::json!({"server_id": "rust", "elapsed_secs": 30})
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum RetryableErrorData {
    /// The routed server is still indexing the workspace.
    WorkspaceIndexing {
        /// Server that is indexing.
        server_id: ServerId,
        /// Seconds spent waiting for indexing to finish before giving up.
        elapsed_secs: u64,
    },
    /// The routed server has not finished initializing.
    ServerInitializing {
        /// Server that is initializing.
        server_id: ServerId,
    },
    /// An expected server has not registered yet and no single candidate
    /// could be narrowed down, so there is no server to name. The braces
    /// keep the wire value an empty object rather than `null`.
    WorkspaceServersInitializing {},
    /// The concurrent `subscriptions/listen` stream limit is reached.
    ListenStreamsExhausted {
        /// Maximum number of concurrent listen streams.
        max_listen_streams: usize,
    },
}

impl RetryableErrorData {
    /// The bespoke JSON-RPC error code for this retryable condition.
    #[must_use]
    pub const fn code(&self) -> i32 {
        match self {
            Self::WorkspaceIndexing { .. } => WORKSPACE_INDEXING_ERROR_CODE,
            Self::ServerInitializing { .. } | Self::WorkspaceServersInitializing {} => {
                SERVER_INITIALIZING_ERROR_CODE
            }
            Self::ListenStreamsExhausted { .. } => LISTEN_STREAMS_EXHAUSTED_ERROR_CODE,
        }
    }
}

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
///
/// This enum is `#[non_exhaustive]`: match it with a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpErrorKind {
    /// Caller-fault: the request itself was invalid. Maps to JSON-RPC
    /// `-32602` (`INVALID_PARAMS`).
    InvalidParams,
    /// Caller-fault: the position fell outside the document and the server's
    /// raw error was rewritten for display. Maps to `-32602` with the
    /// original error as `data`.
    InvalidPosition(RewrittenServerError),
    /// A transient, retryable server-side condition, distinct from a crash.
    /// Maps to a bespoke JSON-RPC `code` with a structured `data` payload a
    /// caller can act on mechanically, rather than the generic
    /// `INTERNAL_ERROR`.
    Retryable(RetryableErrorData),
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
    /// let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
    ///     panic!("expected a retryable classification");
    /// };
    /// assert_eq!(data.code(), mcpls_core::error::WORKSPACE_INDEXING_ERROR_CODE);
    /// ```
    #[must_use]
    pub fn mcp_error_kind(&self) -> McpErrorKind {
        match self {
            Self::InvalidToolParams(_)
            | Self::PathOutsideWorkspace(_)
            | Self::NotARegularFile(_)
            | Self::InvalidUri(_)
            | Self::ListenFilterTooLarge { .. }
            | Self::DocumentNotFound(_)
            | Self::FileSizeLimitExceeded { .. } => McpErrorKind::InvalidParams,

            // Frees up as soon as another listen stream closes.
            Self::ListenStreamsExhausted { max } => {
                McpErrorKind::Retryable(RetryableErrorData::ListenStreamsExhausted {
                    max_listen_streams: *max,
                })
            }

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
            } => McpErrorKind::Retryable(RetryableErrorData::WorkspaceIndexing {
                server_id: server_id.clone(),
                elapsed_secs: *elapsed_secs,
            }),
            Self::ServerInitializing { server_id } => {
                McpErrorKind::Retryable(RetryableErrorData::ServerInitializing {
                    server_id: server_id.clone(),
                })
            }
            // Same condition as `ServerInitializing` -- an expected LSP
            // server hasn't registered yet, retry -- just without a single
            // candidate server narrowed down (see the variant's doc), so
            // there's no `server_id` to report.
            Self::WorkspaceServersInitializing => {
                McpErrorKind::Retryable(RetryableErrorData::WorkspaceServersInitializing {})
            }

            // Same recognized shape `sanitize_lsp_server_message` rewrites
            // for display: rust-analyzer reports this when a position-based
            // request's line/character falls outside the target document --
            // caller-fault. Every other `LspServerError` shape is a genuine
            // server-side problem and stays `Internal`.
            Self::LspServerError { code, message, .. }
                if message.contains(INVALID_OFFSET_MARKER) =>
            {
                McpErrorKind::InvalidPosition(RewrittenServerError::new(*code, message))
            }

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
            | Self::ServerNotFound { .. }
            | Self::LspProtocolError(_)
            | Self::ServerTerminated
            | Self::ShutdownTimeout
            | Self::ServerUnavailable { .. }
            | Self::ServerFailedToStart(_)
            | Self::ServerExitedDuringInit { .. }
            | Self::NoWorkspaceRoots(_)
            // Unlike `FileSizeLimitExceeded`, this fires on aggregate tracker
            // state, not this request's params -- it can succeed unchanged
            // once other documents close, so `InvalidParams` is wrong; not
            // `Retryable` either, since nothing evicts documents on a timer.
            | Self::DocumentLimitExceeded { .. }
            // Same shape as `DocumentLimitExceeded` above -- see this
            // variant's doc comment.
            | Self::SubscriptionLimitReached { .. }
            | Self::AllServersFailedToInit { .. }
            | Self::CapabilityNotSupported { .. } => McpErrorKind::Internal,
        }
    }
}

/// A specialized Result type for mcpls-core operations.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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

    fn not_found(command: &str) -> Error {
        Error::ServerNotFound {
            command: command.to_string(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        }
    }

    #[test]
    fn test_server_not_found_bare_builtin_has_path_and_install_hint() {
        let msg = not_found("rust-analyzer").to_string();
        assert!(msg.contains("not on the PATH"), "{msg}");
        assert!(msg.contains("rustup component add rust-analyzer"), "{msg}");
    }

    #[test]
    fn test_server_not_found_bare_unknown_has_no_install_hint() {
        let msg = not_found("my-custom-lsp").to_string();
        assert!(msg.contains("not on the PATH"), "{msg}");
        assert!(!msg.contains("install it"), "{msg}");
        assert!(!msg.contains(".cmd"), "{msg}");
    }

    #[test]
    fn test_server_not_found_path_command_has_no_path_text() {
        let msg = not_found("/nonexistent/rust-analyzer").to_string();
        assert!(msg.contains("configured path exists"), "{msg}");
        assert!(!msg.contains("PATH"), "{msg}");
        assert!(!msg.contains("install it"), "{msg}");
    }

    #[test]
    fn test_lifecycle_errors_map_to_internal() {
        for err in [Error::ShutdownTimeout, not_found("x")] {
            assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal, "{err:?}");
        }
    }

    #[test]
    fn test_not_found_guidance_cmd_note_only_for_npm_builtins_on_windows() {
        let cases = [
            ("pyright-langserver", Platform::Windows, true),
            ("typescript-language-server", Platform::Windows, true),
            ("pyright-langserver", Platform::Other, false),
            ("rust-analyzer", Platform::Windows, false),
            ("gopls", Platform::Windows, false),
            ("my-custom-lsp", Platform::Windows, false),
        ];
        for (command, platform, expects_cmd_note) in cases {
            let msg = NotFoundGuidance(command, platform).to_string();
            assert_eq!(
                msg.contains(".cmd"),
                expects_cmd_note,
                "{command} {platform:?}: {msg}"
            );
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

    fn spawn_failure(id: &str, command: &str, error: Error) -> ServerSpawnFailure {
        ServerSpawnFailure {
            server_id: ServerId::from(id),
            language_id: id.to_string(),
            command: command.to_string(),
            reason: StartupFailure::Spawn(Arc::new(error)),
        }
    }

    #[test]
    fn test_server_spawn_failure_display() {
        let failure = spawn_failure(
            "rust",
            "rust-analyzer",
            Error::LspInitFailed {
                message: "boom".to_string(),
            },
        );
        assert_eq!(
            failure.to_string(),
            "rust [rust] (rust-analyzer): LSP server initialization failed: boom"
        );
    }

    #[test]
    fn test_server_spawn_failure_clone_shares_reason() {
        let failure = spawn_failure("python", "pyright", not_found("pyright"));
        let cloned = failure.clone();
        assert_eq!(failure.language_id, cloned.language_id);
        assert_eq!(failure.to_string(), cloned.to_string());
    }

    #[test]
    fn test_server_failed_to_start_display_carries_not_found_guidance() {
        let err = Error::ServerFailedToStart(Box::new(spawn_failure(
            "rust",
            "rust-analyzer",
            not_found("rust-analyzer"),
        )));
        let msg = err.to_string();
        assert!(
            msg.contains("'rust' for language 'rust' failed to start"),
            "{msg}"
        );
        assert!(msg.contains("not on the PATH"), "{msg}");
        assert!(msg.contains("rustup component add rust-analyzer"), "{msg}");
        assert!(msg.contains("restart mcpls"), "{msg}");
    }

    #[test]
    fn test_server_failed_to_start_init_task_panicked_display() {
        let err = Error::ServerFailedToStart(Box::new(ServerSpawnFailure {
            server_id: ServerId::from("rust"),
            language_id: "rust".to_string(),
            command: "rust-analyzer".to_string(),
            reason: StartupFailure::InitTaskPanicked,
        }));
        assert!(err.to_string().contains("initialization task panicked"));
    }

    #[test]
    fn test_startup_errors_map_to_internal() {
        let failure = spawn_failure("rust", "rust-analyzer", not_found("rust-analyzer"));
        for err in [
            Error::ServerFailedToStart(Box::new(failure.clone())),
            Error::AllServersFailedToInit {
                failures: vec![failure],
            },
            Error::ServerExitedDuringInit {
                command: "x".to_string(),
                exit_code: Some(1),
            },
            Error::ServerUnavailable {
                server_id: ServerId::from("rust"),
                retry_in: Duration::from_secs(1),
            },
        ] {
            assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal, "{err:?}");
        }
    }

    #[test]
    fn test_server_exited_during_init_display_hints_only_for_rust_analyzer() {
        let hinted = Error::ServerExitedDuringInit {
            command: "rust-analyzer".to_string(),
            exit_code: Some(1),
        }
        .to_string();
        assert!(hinted.contains("exit code 1"), "{hinted}");
        assert!(
            hinted.contains("rustup component add rust-analyzer"),
            "{hinted}"
        );

        let plain = Error::ServerExitedDuringInit {
            command: "gopls".to_string(),
            exit_code: None,
        }
        .to_string();
        assert!(plain.contains("terminated by a signal"), "{plain}");
        assert!(!plain.contains("rustup"), "{plain}");
    }

    #[test]
    fn test_server_unavailable_display_names_retry_delay() {
        let err = Error::ServerUnavailable {
            server_id: ServerId::from("rust"),
            retry_in: Duration::from_secs(2),
        };
        assert_eq!(
            err.to_string(),
            "LSP server 'rust' is unavailable: crash-looping, retry in 2.0s"
        );
    }

    #[test]
    fn test_error_display_all_servers_failed_lists_each_failure() {
        let err = Error::AllServersFailedToInit {
            failures: vec![
                spawn_failure("rust", "rust-analyzer", not_found("rust-analyzer")),
                spawn_failure(
                    "python",
                    "pyright",
                    Error::LspInitFailed {
                        message: "denied".to_string(),
                    },
                ),
            ],
        };
        let msg = err.to_string();
        assert!(
            msg.starts_with("all LSP servers failed to initialize: "),
            "{msg}"
        );
        assert!(msg.contains("rust [rust] (rust-analyzer)"), "{msg}");
        assert!(msg.contains("python [python] (pyright)"), "{msg}");
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
            Error::ListenFilterTooLarge { max: 1000 },
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
        let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
            panic!("expected WorkspaceIndexing to classify as Retryable");
        };
        assert_eq!(data.code(), WORKSPACE_INDEXING_ERROR_CODE);
        assert_eq!(
            serde_json::to_value(&data).unwrap(),
            serde_json::json!({"server_id": "rust", "elapsed_secs": 30})
        );
    }

    #[test]
    fn test_mcp_error_kind_server_initializing_is_retryable_with_dedicated_code() {
        let err = Error::ServerInitializing {
            server_id: ServerId::from("python"),
        };
        let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
            panic!("expected ServerInitializing to classify as Retryable");
        };
        assert_eq!(data.code(), SERVER_INITIALIZING_ERROR_CODE);
        assert_eq!(
            serde_json::to_value(&data).unwrap(),
            serde_json::json!({"server_id": "python"})
        );
        assert_ne!(
            data.code(),
            WORKSPACE_INDEXING_ERROR_CODE,
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
        let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
            panic!("expected WorkspaceServersInitializing to classify as Retryable");
        };
        assert_eq!(data.code(), SERVER_INITIALIZING_ERROR_CODE);
        assert_eq!(
            serde_json::to_value(&data).unwrap(),
            serde_json::json!({}),
            "the empty struct variant must serialize as `{{}}`, not `null`"
        );
    }

    #[test]
    fn test_retryable_error_data_codes_and_snake_case_keys() {
        let cases = [
            (
                RetryableErrorData::WorkspaceIndexing {
                    server_id: ServerId::from("rust"),
                    elapsed_secs: 7,
                },
                WORKSPACE_INDEXING_ERROR_CODE,
                serde_json::json!({"server_id": "rust", "elapsed_secs": 7}),
            ),
            (
                RetryableErrorData::ServerInitializing {
                    server_id: ServerId::from("python"),
                },
                SERVER_INITIALIZING_ERROR_CODE,
                serde_json::json!({"server_id": "python"}),
            ),
            (
                RetryableErrorData::WorkspaceServersInitializing {},
                SERVER_INITIALIZING_ERROR_CODE,
                serde_json::json!({}),
            ),
            (
                RetryableErrorData::ListenStreamsExhausted {
                    max_listen_streams: 100,
                },
                LISTEN_STREAMS_EXHAUSTED_ERROR_CODE,
                serde_json::json!({"max_listen_streams": 100}),
            ),
        ];
        for (data, code, wire) in cases {
            assert_eq!(data.code(), code);
            assert_eq!(serde_json::to_value(&data).unwrap(), wire);
        }
    }

    #[test]
    fn test_mcp_error_kind_listen_streams_exhausted_is_retryable() {
        let err = Error::ListenStreamsExhausted { max: 100 };
        let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
            panic!("expected ListenStreamsExhausted to classify as Retryable");
        };
        assert_eq!(data.code(), LISTEN_STREAMS_EXHAUSTED_ERROR_CODE);
        assert_eq!(
            data,
            RetryableErrorData::ListenStreamsExhausted {
                max_listen_streams: 100
            }
        );
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
            Error::SubscriptionLimitReached { max: 1000 },
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

    /// #496: an `LspServerError` carrying the recognized "position out of
    /// range" shape (same substring `sanitize_lsp_server_message` rewrites
    /// for display) is caller-fault, not a generic internal failure.
    #[test]
    fn test_mcp_error_kind_lsp_server_error_invalid_offset_is_invalid_params() {
        let err = Error::LspServerError {
            code: -32603,
            message: "Invalid offset LineCol { line: 2291, col: 0 } (line index length: 100417)"
                .to_string(),
            data: None,
        };
        assert_eq!(
            err.mcp_error_kind(),
            McpErrorKind::InvalidPosition(RewrittenServerError::new(
                -32603,
                "Invalid offset LineCol { line: 2291, col: 0 } (line index length: 100417)"
            ))
        );
    }

    /// #465: server-supplied control characters never reach an error `Display`
    /// raw, so a logged or echoed error cannot forge log lines.
    #[test]
    fn test_display_escapes_control_characters_from_the_server() {
        let server_error = Error::LspServerError {
            code: -32603,
            message: "boom\nERROR forged\x1b[31m".to_string(),
            data: None,
        };
        assert_eq!(
            server_error.to_string(),
            "LSP server error: -32603 - boom\\nERROR forged\\u{1b}[31m"
        );

        let protocol_error = Error::LspProtocolError("bad\nline".to_string());
        assert_eq!(protocol_error.to_string(), "LSP protocol error: bad\\nline");
    }

    /// The raw message carried as `data` is bounded at construction.
    #[test]
    fn test_rewritten_server_error_bounds_raw_message() {
        let raw =
            RewrittenServerError::new(-32603, &"x".repeat(MAX_ERROR_MESSAGE_CALLER_BYTES * 2));
        assert!(raw.raw_message().len() < MAX_ERROR_MESSAGE_CALLER_BYTES * 2);
        assert!(raw.raw_message().ends_with("(truncated)"));
    }

    /// Counterpart: an `LspServerError` whose message doesn't match the
    /// recognized position-out-of-range shape is a genuine server-side
    /// problem and must stay `Internal`.
    #[test]
    fn test_mcp_error_kind_lsp_server_error_other_message_stays_internal() {
        let err = Error::LspServerError {
            code: -32603,
            message: "internal error".to_string(),
            data: None,
        };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }

    /// #496: `SubscriptionLimitReached` fires on aggregate per-session
    /// tracker state, not this request's params, so it must classify the
    /// same way as `DocumentLimitExceeded` -- not `InvalidParams`.
    #[test]
    fn test_mcp_error_kind_subscription_limit_reached_stays_internal() {
        let err = Error::SubscriptionLimitReached { max: 1000 };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }
}
