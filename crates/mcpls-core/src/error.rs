//! Error types for mcpls-core.
//!
//! This module defines the canonical error type for the library,
//! following the Microsoft Rust Guidelines for error handling.
//!
//! Related vocabulary lives in private submodules and is re-exported here, so
//! every item keeps its `mcpls_core::error::` path:
//!
//! ```
//! use mcpls_core::error::{
//!     EchoedArgument, EchoedPath, HomeVariable, InitFailureHint, InlineFlag, LaunchTrigger,
//!     LauncherRefusal, LongCommandName, LongInlineFlag, McpErrorKind, OperandKind,
//!     PowerShellParameter, ResolvedItem, RetryableErrorData, RunnerSubcommand, ShellFlag,
//!     StderrExcerpt, StdinSource, SymbolCandidate, SymbolResolutionData, UnanalyzableLaunch,
//!     UntrustedRefusal,
//! };
//! use mcpls_core::error::{
//!     LISTEN_STREAMS_EXHAUSTED_ERROR_CODE, SERVER_INITIALIZING_ERROR_CODE,
//!     SERVER_RESTARTED_ERROR_CODE, STATELESS_SUBSCRIPTION_ERROR_CODE,
//!     WORKSPACE_INDEXING_ERROR_CODE,
//! };
//!
//! assert_eq!(WORKSPACE_INDEXING_ERROR_CODE, -32050);
//! ```

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::bridge::resources::ResourceUriError;
use crate::bridge::{
    Capability, InvalidClientPath, InvalidHierarchyItem, InvalidKindFilter, InvalidPosition,
    InvalidRange, InvalidTextField,
};
use crate::config::{
    BuiltinServer, DuplicateEnvKey, EntrySummary, EnvKey, EnvTooLarge, FileKey, FilePattern,
    InvalidEnvValue, InvalidServerArgs, LanguageId, ServerCommand, ServerId, ToolKind,
    UnsupportedFilePattern,
};
use crate::lsp::JsonRpcError;
pub use crate::redaction::RedactedText;
pub use crate::util::SizeExceeded;
use crate::util::{MAX_ERROR_MESSAGE_CALLER_BYTES, escape_control, truncate_str};

mod codes;
mod echo;
mod launch;
mod stderr;
mod symbols;

pub use codes::{
    LISTEN_STREAMS_EXHAUSTED_ERROR_CODE, McpErrorKind, RetryableErrorData,
    SERVER_INITIALIZING_ERROR_CODE, SERVER_RESTARTED_ERROR_CODE, STATELESS_SUBSCRIPTION_ERROR_CODE,
    WORKSPACE_INDEXING_ERROR_CODE,
};
use echo::BoundedDebugPath;
pub use echo::{EchoedArgument, EchoedPath, EchoedUri};
pub use launch::{
    HomeVariable, InlineFlag, LaunchTrigger, LauncherRefusal, LongCommandName, LongInlineFlag,
    OperandKind, PowerShellParameter, ResolvedItem, RunnerSubcommand, ShellFlag, StdinSource,
    UnanalyzableLaunch, UntrustedRefusal,
};
use stderr::{EarlyExitDetail, HintSuffix, StderrSuffix};
pub use stderr::{InitFailureHint, StderrExcerpt};
pub use symbols::{SymbolCandidate, SymbolResolutionData};

/// Explains a `plaintext` routing failure: which extension or file name had no
/// mapping and which `file_patterns` were configured. Empty for any other
/// language.
fn no_server_detail(language: &LanguageId, file: &FileKey, patterns: &[FilePattern]) -> String {
    if *language != LanguageId::PLAINTEXT {
        return String::new();
    }
    let (subject, remedy) = match file {
        FileKey::Extension(ext) => (
            format!("file extension '{ext}' is not mapped to any language"),
            "a `*.EXT` file_patterns entry or workspace.language_extensions".to_owned(),
        ),
        FileKey::Name(name) => (
            format!("file name '{name}' is not mapped to any language"),
            format!("a `**/{name}` file_patterns entry"),
        ),
        FileKey::Unmappable => (
            "the file has no usable extension or name".to_owned(),
            "a `*.EXT` file_patterns entry or workspace.language_extensions".to_owned(),
        ),
    };
    let configured = if patterns.is_empty() {
        "no file_patterns are configured".to_owned()
    } else {
        let list: Vec<&str> = patterns.iter().map(FilePattern::as_str).collect();
        format!("configured file_patterns: {}", list.join(", "))
    };
    format!(" ({subject}; {configured}; map it with {remedy})")
}

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
        if Path::new(command).parent().is_some_and(|p| !p.is_empty()) {
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

/// Whether `response` is rust-analyzer's "position out of range" internal
/// error, which [`Error::mcp_error_kind`] classifies as caller-fault.
pub(crate) fn is_invalid_offset(response: &JsonRpcError) -> bool {
    response.message.contains(INVALID_OFFSET_MARKER)
}

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
    /// Starting this server, or the background initialization task as a whole,
    /// panicked before the server registered.
    InitTaskPanicked,
    /// The workspace is untrusted and this server was not started.
    RefusedUntrustedWorkspace(UntrustedRefusal),
}

impl fmt::Display for StartupFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => ErrorChain(error.as_ref()).fmt(f),
            Self::InitTaskPanicked => {
                f.write_str("the initialization task panicked (see the mcpls log)")
            }
            Self::RefusedUntrustedWorkspace(refusal) => refusal.fmt(f),
        }
    }
}

/// Details of a single server spawn failure.
#[derive(Debug, Clone)]
pub struct ServerSpawnFailure {
    /// Routing identity of the failed server.
    pub server_id: ServerId,
    /// Language ID of the failed server.
    pub language_id: LanguageId,
    /// Command that was attempted.
    pub command: ServerCommand,
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

impl ServerSpawnFailure {
    /// The caller-facing text of [`Error::ServerFailedToStart`]. A refusal
    /// was a decision, not a failure, so it names the remedy instead of the
    /// startup-failure tail.
    pub(crate) const fn failed_to_start(&self) -> FailedToStart<'_> {
        FailedToStart(self)
    }
}

/// `Display` adapter for [`ServerSpawnFailure::failed_to_start`].
pub(crate) struct FailedToStart<'a>(&'a ServerSpawnFailure);

impl fmt::Display for FailedToStart<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let failure = self.0;
        let (id, language) = (&failure.server_id, &failure.language_id);
        match &failure.reason {
            StartupFailure::RefusedUntrustedWorkspace(refusal) => {
                write!(
                    f,
                    "LSP server '{id}' for language '{language}' was not started: {refusal}"
                )?;
                match refusal {
                    UntrustedRefusal::NotAllowed { .. } => {
                        write!(f, "; restart mcpls with `--allow-server {id}` to start it")
                    }
                    UntrustedRefusal::WorkspaceExecutable { .. }
                    | UntrustedRefusal::WorkspaceExecutableArgument { .. }
                    | UntrustedRefusal::UnreadableWorkspaceArgument { .. }
                    | UntrustedRefusal::UnresolvedExecutable { .. }
                    | UntrustedRefusal::AutoSelectionTarget { .. }
                    | UntrustedRefusal::WorkspaceHome { .. }
                    | UntrustedRefusal::EmptyHome { .. }
                    | UntrustedRefusal::UnknownHome
                    | UntrustedRefusal::WorkspaceTsserver { .. }
                    | UntrustedRefusal::ProjectLauncher { .. }
                    | UntrustedRefusal::UnresolvedWrappedProgram { .. }
                    | UntrustedRefusal::WrappedProgramPathContainsEquals { .. }
                    | UntrustedRefusal::UnpinnedTypescriptLauncher { .. }
                    | UntrustedRefusal::NonUtf8Path { .. }
                    | UntrustedRefusal::BlankResolvedPath { .. }
                    | UntrustedRefusal::NulInResolvedPath { .. }
                    | UntrustedRefusal::NoSafeWorkingDirectory
                    | UntrustedRefusal::EnvTooLarge { .. } => Ok(()),
                }
            }
            reason @ (StartupFailure::Spawn(_) | StartupFailure::InitTaskPanicked) => write!(
                f,
                "LSP server '{id}' for language '{language}' failed to start: {reason}; restart \
                 mcpls after fixing it (startup failures are not retried)"
            ),
        }
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

/// `Display` adapter listing server ids, separated by `, `.
struct IdList<'a>(&'a [ServerId]);

impl fmt::Display for IdList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, id) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "'{id}'")?;
        }
        Ok(())
    }
}

/// A background task whose failure surfaces as [`Error::TaskFailed`] or is
/// reported by its supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackgroundTask {
    /// The MCP service loop that serves one client connection.
    McpService,
    /// The task that reads an LSP server's messages.
    LspReceiver,
    /// The blocking resolution of `subscriptions/listen` URIs.
    ListenResolution,
    /// The blocking canonicalization of a client path.
    PathValidation,
    /// Opening and verifying a file on the blocking pool.
    FileOpen,
    /// Planning which configured servers to start, on the blocking pool.
    ServerPlanning,
    /// The reaper that expires idle HTTP sessions.
    IdleSessionReaper,
    /// The task that delivers `resources/updated` notifications to one session.
    SessionDelivery,
}

impl fmt::Display for BackgroundTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::McpService => "MCP service",
            Self::LspReceiver => "LSP receiver",
            Self::ListenResolution => "listen URI resolution",
            Self::PathValidation => "path validation",
            Self::FileOpen => "file open",
            Self::ServerPlanning => "server start planning",
            Self::IdleSessionReaper => "idle HTTP session reaper",
            Self::SessionDelivery => "resources/updated delivery",
        })
    }
}

/// A standard stream of a spawned LSP server process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StdioStream {
    /// Standard input.
    Stdin,
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl fmt::Display for StdioStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stdin => "stdin",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
}

/// A configuration value that is invalid in the context of the entry that holds it.
///
/// Field-level rules live in the types of the fields (a rejected value cannot be
/// built); this enum carries what only the surrounding entry or the whole
/// configuration can add, such as which server a bad value belongs to.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// A server entry lists a `file_patterns` form that maps to no extension.
    #[error("lsp_servers entry '{server}'")]
    UnsupportedFilePattern {
        /// The id of the entry that holds the pattern.
        server: ServerId,
        /// The rejected pattern and the supported forms.
        #[source]
        pattern: UnsupportedFilePattern,
    },

    /// The config file's bytes are not UTF-8.
    #[error("config file is not valid UTF-8")]
    NotUtf8(#[source] std::string::FromUtf8Error),

    /// The config file is over its fixed size limit, which is not configurable.
    #[error("config file is {} bytes, over the fixed {} byte limit", .0.size, .0.max)]
    FileTooLarge(SizeExceeded),

    /// A server's `env` table sets one variable twice, which the host treats as
    /// one name.
    #[error("lsp_servers entry '{server}'")]
    DuplicateEnvKey {
        /// The id of the entry that holds the table.
        server: ServerId,
        /// The colliding keys.
        #[source]
        error: DuplicateEnvKey,
    },

    /// A server's `env` table holds a value the operating system cannot pass.
    #[error("lsp_servers entry '{server}': environment variable '{key}'")]
    InvalidEnvValue {
        /// The id of the entry that holds the table.
        server: ServerId,
        /// The variable whose value is invalid.
        key: EnvKey,
        /// Why the value is invalid.
        #[source]
        error: InvalidEnvValue,
    },

    /// A server's `env` table is too large to pass to a process.
    #[error("lsp_servers entry '{server}'")]
    EnvTooLarge {
        /// The id of the entry that holds the table.
        server: ServerId,
        /// Which size limit the table is over.
        #[source]
        error: EnvTooLarge,
    },

    /// An entry of a server's `args` holds a NUL, which the operating system
    /// cannot pass in an argument.
    #[error("lsp_servers entry '{server}'")]
    ArgContainsNul {
        /// The id of the entry that holds the argument.
        server: ServerId,
        /// Which argument holds the NUL.
        #[source]
        error: InvalidServerArgs,
    },

    /// A config path has no directory to resolve relative entries against.
    #[error("configuration path has no parent directory: {}", .path.display())]
    NoParentDirectory {
        /// The config path.
        path: PathBuf,
    },

    /// `selection = "auto"` on an entry that is not typescript-language-server.
    #[error(
        "selection = \"auto\" is only valid for typescript-language-server entries (language \
         '{language}'); remove `selection` to use `command` as written"
    )]
    SelectionAutoOnNonTypescript {
        /// The language of the offending entry.
        language: LanguageId,
    },

    /// An allowed server id names no configured server.
    #[error(
        "allowed server '{server}' is not a configured server (configured: {})",
        IdList(configured)
    )]
    UnknownAllowedServer {
        /// The allowed id that matches nothing.
        server: ServerId,
        /// The ids of the configured servers.
        configured: Vec<ServerId>,
    },

    /// Two applicable entries share one server id.
    #[error(
        "duplicate server id '{id}' in this workspace (used by both an entry with {first} and \
         one with {second}); add a unique `name` to each `[[lsp_servers]]` entry"
    )]
    DuplicateServerId {
        /// The shared id.
        id: ServerId,
        /// The first entry.
        first: Box<EntrySummary>,
        /// The second entry.
        second: Box<EntrySummary>,
    },

    /// A language has two servers that both omit `handles`.
    #[error(
        "language '{language}' has two catch-all servers ('{existing}' and '{id}'); at most one \
         server per language may omit `handles`"
    )]
    TwoCatchAllServers {
        /// The language both serve.
        language: LanguageId,
        /// The server registered first.
        existing: ServerId,
        /// The server that collides with it.
        id: ServerId,
    },

    /// Two servers of one language claim the same tool in `handles`.
    #[error("tool '{tool}' for language '{language}' is claimed by both '{existing}' and '{id}'")]
    ToolClaimedTwice {
        /// The contested tool.
        tool: ToolKind,
        /// The language both serve.
        language: LanguageId,
        /// The server registered first.
        existing: ServerId,
        /// The server that collides with it.
        id: ServerId,
    },

    /// A configured workspace root cannot be canonicalized.
    #[error(
        "workspace root '{}' resolved relative to '{}' as '{}' could not be canonicalized",
        .written.display(), .base_dir.display(), .probe.display()
    )]
    UnresolvableWorkspaceRoot {
        /// The root as written in the config.
        written: PathBuf,
        /// The directory a relative root is resolved against.
        base_dir: PathBuf,
        /// The path that was canonicalized.
        probe: PathBuf,
        /// Why canonicalization failed.
        #[source]
        source: std::io::Error,
    },
}

/// A step of the LSP initialization handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InitPhase {
    /// The `initialize` request.
    Initialize,
    /// The `initialized` notification.
    Initialized,
    /// The `workspace/didChangeConfiguration` notification.
    DidChangeConfiguration,
}

impl fmt::Display for InitPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Initialize => "Initialize request failed",
            Self::Initialized => "initialized notification failed",
            Self::DidChangeConfiguration => "workspace/didChangeConfiguration notification failed",
        })
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
    #[error("LSP server initialization failed: {phase}: {}{}{}", ErrorChain(.cause.as_ref()), HintSuffix(.hint), StderrSuffix(.stderr))]
    LspInitFailed {
        /// The handshake step that failed.
        phase: InitPhase,
        /// Why the step failed, so a timeout and a rejection by the server
        /// stay distinguishable. Rendered by `Display` through
        /// [`ErrorChain`] instead of being exposed through `source()`, so the
        /// chain is not printed twice.
        cause: Box<Self>,
        /// The likely cause and remedy, when one is known.
        hint: Option<InitFailureHint>,
        /// What the server wrote to stderr before failing, if anything.
        stderr: Option<StderrExcerpt>,
    },

    /// LSP server returned an error response.
    ///
    /// The payload keeps the message verbatim for diagnostics (logging,
    /// `Debug`, pattern matching). The `Display` impl rewrites known-internal
    /// upstream text before it reaches an MCP caller, so `message` is not
    /// always what the caller sees.
    #[error("LSP server error: {} - {}", .0.code, sanitize_lsp_server_message(&.0.message))]
    LspServerError(JsonRpcError),

    /// The MCP server could not complete its handshake with the client.
    #[error("failed to start MCP server")]
    McpServerStart(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// A background task panicked or was cancelled.
    #[error("{task} task failed")]
    TaskFailed {
        /// The task that failed.
        task: BackgroundTask,
        /// The join failure, carrying the panic payload or cancellation.
        #[source]
        source: tokio::task::JoinError,
    },

    /// A standard stream of the spawned LSP server could not be captured.
    #[error("failed to capture LSP server {0}")]
    StdioCapture(StdioStream),

    /// The HTTP transport could not bind its listener.
    #[error("failed to bind HTTP listener on {addr}")]
    HttpBind {
        /// Address the listener was asked to bind.
        addr: std::net::SocketAddr,
        /// Underlying I/O error, e.g. `AddrInUse`.
        #[source]
        source: std::io::Error,
    },

    /// Document was not found or could not be opened.
    #[error("document not found: {}", EchoedPath::new(.0))]
    DocumentNotFound(PathBuf),

    /// No LSP server configured for the given language.
    #[error(
        "no LSP server configured for language: {language}{}",
        no_server_detail(.language, .file, .patterns)
    )]
    NoServerForLanguage {
        /// The language detected for the file.
        language: LanguageId,
        /// What identifies the file to the language map: its extension, its
        /// extensionless name, or [`FileKey::Unmappable`].
        file: FileKey,
        /// The `file_patterns` configured across all servers, so the error can
        /// show what was available to map the extension.
        patterns: Arc<[FilePattern]>,
    },

    /// A server is configured for the language, but no server claims this
    /// specific tool (either no server lists it in `handles` and there is no
    /// catch-all, or the server that claimed it failed to spawn with no live
    /// catch-all to rebind to).
    #[error("no server handles tool '{tool}' for language '{language_id}'")]
    NoServerForTool {
        /// Language ID the request was for.
        language_id: LanguageId,
        /// Tool that no server claims.
        tool: ToolKind,
    },

    /// The server configured for this request failed to start and, because
    /// startup failures are never retried, will not become available until
    /// mcpls is restarted.
    ///
    /// Boxed to keep [`Error`] small.
    #[error("{}", .0.failed_to_start())]
    ServerFailedToStart(Box<ServerSpawnFailure>),

    /// LSP server for the language is configured but still initializing.
    #[error(
        "LSP server '{server_id}' is still initializing (large project load in progress); wait and retry the request (this may take a few minutes on large projects)"
    )]
    ServerInitializing {
        /// Routing identity of the server that has not yet registered.
        server_id: ServerId,
    },

    /// The server was restarted on request while this call was in flight or
    /// being routed; the request never reached the replacement.
    #[error(
        "LSP server '{server_id}' was restarted while this request was in flight; retry the request"
    )]
    ServerRestarted {
        /// Routing identity of the restarted server.
        server_id: ServerId,
    },

    /// A symbol name could not be resolved to exactly one position.
    ///
    /// Boxed to keep [`Error`] small.
    #[error("{0}")]
    SymbolResolution(Box<SymbolResolutionData>),

    /// A restart request named servers that are not configured.
    #[error(
        "unknown LSP server(s) {}; configured servers: {}",
        IdList(unknown),
        IdList(configured)
    )]
    UnknownServers {
        /// The requested ids that matched no configured server.
        unknown: Vec<ServerId>,
        /// Every configured server id.
        configured: Vec<ServerId>,
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

    /// Untrusted-workspace mode refused a config file that lies inside the
    /// workspace or the current directory, because the analyzed checkout
    /// controls it. A startup error; never reaches a tool response.
    #[error(
        "config file {} lies inside the workspace or the current directory, which untrusted mode does not trust; move it elsewhere",
        .path.display()
    )]
    ConfigInsideWorkspace {
        /// The canonical path of the config file.
        path: PathBuf,
    },

    /// Invalid configuration.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// I/O error; displays the OS text once, with no prefix.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error.
    #[error("JSON error")]
    Json(#[from] serde_json::Error),

    /// TOML deserialization error.
    ///
    /// Displays the parser's message and position, never the source line it
    /// prints: that line can hold a secret from an `env` table. The parser
    /// error is deliberately not a `source`, so no error chain prints it
    /// either.
    #[error("{0}")]
    TomlDe(TomlParseError),

    /// TOML serialization error.
    #[error(transparent)]
    TomlSer(#[from] toml::ser::Error),

    /// Request timeout, carrying the elapsed limit.
    #[error("request timed out after {0:?}")]
    Timeout(Duration),

    /// LSP server failed to spawn.
    #[error("failed to spawn LSP server '{command}'")]
    ServerSpawnFailed {
        /// Command that failed to spawn.
        command: ServerCommand,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// LSP server executable was not found.
    ///
    /// Distinct from [`Error::ServerSpawnFailed`] so the message can carry
    /// PATH and install guidance.
    #[error("failed to spawn LSP server '{command}': {io}{}", NotFoundGuidance(.command.as_str(), Platform::CURRENT))]
    ServerNotFound {
        /// Command that could not be found.
        command: ServerCommand,
        /// Underlying IO error. Not a `source()`: the guidance text follows
        /// it, so it is rendered inline to keep the cause before the advice.
        io: std::io::Error,
    },

    /// LSP protocol error during message parsing.
    #[error("LSP protocol error: {}", escape_control(.0.as_str()))]
    LspProtocolError(RedactedText),

    /// None of the resource URIs a `subscriptions/listen` request named
    /// resolves inside the workspace.
    #[error("none of the requested resource URIs resolve inside the workspace")]
    NoResolvableListenUris,

    /// A client-supplied position is out of bounds.
    #[error(transparent)]
    InvalidPositionInput(#[from] InvalidPosition),

    /// A client-supplied range is malformed or too large.
    #[error(transparent)]
    InvalidRangeInput(#[from] InvalidRange),

    /// A client-supplied hierarchy item has an invalid range.
    #[error(transparent)]
    InvalidHierarchyItemInput(#[from] InvalidHierarchyItem),

    /// A client-supplied line lies beyond the end of the tracked document.
    ///
    /// Only the line is checked: an LSP server clamps a character past the
    /// end of its line to the line length (LSP 3.17, `Position`), so such a
    /// character is forwarded unchanged.
    #[error(
        "line {line} is beyond the end of the document (the document ends at line {last_line})"
    )]
    PositionBeyondDocument {
        /// The 1-based line the client supplied.
        line: std::num::NonZeroU32,
        /// The 1-based number of the document's last line.
        last_line: std::num::NonZeroU32,
    },

    /// A client-supplied `lsp-diagnostics://` resource URI was rejected.
    #[error(transparent)]
    ResourceUri(#[from] ResourceUriError),

    /// A path mcpls itself derived could not be represented as a URI.
    #[error("cannot convert path to URI: {}", EchoedPath::new(.0))]
    PathToUri(PathBuf),

    /// Server process terminated unexpectedly.
    #[error("LSP server process terminated unexpectedly")]
    ServerTerminated,

    /// LSP server shutdown did not complete before its deadline.
    #[error("LSP server shutdown did not complete before its deadline")]
    ShutdownTimeout,

    /// LSP server process exited before completing the `initialize`
    /// handshake.
    #[error("LSP server '{command}' exited during initialization{}{}{}", EarlyExitDetail(.command.as_str(), *.exit_code), HintSuffix(.hint), StderrSuffix(.stderr))]
    ServerExitedDuringInit {
        /// Command that was spawned.
        command: ServerCommand,
        /// Exit code, or `None` if the process was terminated by a signal.
        exit_code: Option<i32>,
        /// The likely cause and remedy, when one is known.
        hint: Option<InitFailureHint>,
        /// What the server wrote to stderr before exiting, if anything.
        stderr: Option<StderrExcerpt>,
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

    /// A `kind_filter` / `symbol_kind` / `kind` value names no kind or is
    /// over-long.
    #[error("invalid tool parameters: {0}")]
    InvalidKindFilter(InvalidKindFilter),

    /// A client URI is not an absolute `file://` URI.
    #[error("invalid tool parameters: Invalid URI, expected an absolute file:// URI but got: {0}")]
    NotFileUri(EchoedUri),

    /// A client-supplied text field is blank or too long.
    #[error("invalid tool parameters: {0}")]
    InvalidTextField(InvalidTextField),

    /// A client-supplied file path is empty or contains a NUL byte.
    #[error(transparent)]
    InvalidClientPath(#[from] InvalidClientPath),

    /// A client-supplied file path is malformed for the filesystem: it runs
    /// through a regular file, or has an invalid or over-long name.
    ///
    /// Produced only while validating the client path
    /// (`WorkspaceRoots::validate`); the same IO kinds raised later, while
    /// reading or opening an already validated file, stay [`Error::FileIo`]
    /// because there they are environmental.
    #[error("malformed file path {}", BoundedDebugPath(.path))]
    MalformedPath {
        /// The path as supplied by the client.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// File I/O error occurred.
    ///
    /// See [`Error::mcp_error_kind`] for the JSON-RPC classification: a
    /// `source.kind() == ErrorKind::NotFound` failure -- whether `path` was
    /// freshly supplied in this request or was tracked from an earlier one
    /// and has since been deleted/moved on disk -- is caller-fault; any
    /// other IO failure is not.
    #[error("file I/O error for {}", BoundedDebugPath(.path))]
    FileIo {
        /// Path to the file.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Path is outside allowed workspace boundaries.
    #[error("path outside workspace: {}", EchoedPath::new(.0))]
    PathOutsideWorkspace(PathBuf),

    /// No workspace roots are configured, so path-taking operations are
    /// rejected outright rather than allowed unrestricted (fail closed).
    #[error("no workspace roots configured: refusing access to {}", EchoedPath::new(.0))]
    NoWorkspaceRoots(PathBuf),

    /// Document limit exceeded.
    #[error(
        "document limit exceeded: {current}/{max} (raise workspace.max_documents in config to increase this)"
    )]
    DocumentLimitExceeded {
        /// Current number of documents.
        current: usize,
        /// Maximum allowed documents.
        max: std::num::NonZeroUsize,
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
        "file size limit exceeded: {} bytes, max {} bytes (raise workspace.max_file_size in config to increase this)",
        .0.size,
        .0.max
    )]
    FileSizeLimitExceeded(SizeExceeded),

    /// Path exists but does not refer to a regular file (e.g. a FIFO or a
    /// character/block device).
    ///
    /// mcpls refuses to read such paths: their reported size does not bound
    /// how much data reading them could produce, and opening some of them
    /// for reading can block indefinitely waiting for a peer. A Unix domain
    /// socket special file is not covered by this variant -- `open(2)` on
    /// one fails outright (`ENXIO`) before the file-type check that produces
    /// this error ever runs, so it surfaces as [`Self::FileIo`] from a
    /// document read and as [`Self::Io`] from the config loader.
    #[error("not a regular file: {}", EchoedPath::new(.0))]
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
        /// The missing LSP capability, naming the `ServerCapabilities` field
        /// mcpls checked.
        capability: Capability,
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
    ///     server_id: ServerId::from_static("rust"),
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
            Self::InvalidKindFilter(_)
            | Self::NotFileUri(_)
            | Self::UnknownServers { .. }
            | Self::InvalidTextField(_)
            | Self::InvalidClientPath(_)
            | Self::MalformedPath { .. }
            | Self::PathOutsideWorkspace(_)
            | Self::NotARegularFile(_)
            | Self::NoResolvableListenUris
            | Self::ResourceUri(_)
            | Self::InvalidPositionInput(_)
            | Self::InvalidRangeInput(_)
            | Self::InvalidHierarchyItemInput(_)
            | Self::PositionBeyondDocument { .. }
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
            // Any other IO failure (permission denied, an invalid-input error
            // while reading an already validated file, etc.) reaching here is
            // a genuine server-side problem the caller cannot fix by changing
            // their request. A malformed client path is `MalformedPath`.
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
            Self::SymbolResolution(data) => McpErrorKind::SymbolResolution((**data).clone()),
            Self::ServerRestarted { server_id } => {
                McpErrorKind::Retryable(RetryableErrorData::ServerRestarted {
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
            Self::LspServerError(response) if is_invalid_offset(response) => {
                McpErrorKind::InvalidPosition(RewrittenServerError::new(
                    response.code,
                    &response.message,
                ))
            }

            Self::LspInitFailed { .. }
            | Self::LspServerError(..)
            | Self::McpServerStart(_)
            | Self::TaskFailed { .. }
            | Self::StdioCapture(_)
            | Self::PathToUri(_)
            | Self::HttpBind { .. }
            | Self::NoServerForLanguage { .. }
            | Self::NoServerForTool { .. }
            | Self::NoServerConfigured
            | Self::NoServerForWorkspaceTool { .. }
            | Self::ConfigNotFound(_)
            | Self::ConfigInsideWorkspace { .. }
            | Self::Config(_)
            | Self::Io(_)
            | Self::Json(_)
            | Self::TomlDe(_)
            | Self::TomlSer(_)
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

    /// A well-formed resource URI whose path cannot be resolved (deleted, outside
    /// the workspace): unsubscribing falls back to the recorded alias (#499).
    /// Every other failure is a malformed URI or an internal fault.
    pub(crate) const fn is_unresolvable_resource(&self) -> bool {
        match self {
            Self::MalformedPath { .. }
            | Self::FileIo { .. }
            | Self::PathOutsideWorkspace(..)
            | Self::NoWorkspaceRoots(..) => true,
            Self::LspInitFailed { .. }
            | Self::LspServerError(..)
            | Self::McpServerStart(..)
            | Self::TaskFailed { .. }
            | Self::StdioCapture(..)
            | Self::HttpBind { .. }
            | Self::DocumentNotFound(..)
            | Self::NoServerForLanguage { .. }
            | Self::NoServerForTool { .. }
            | Self::ServerFailedToStart(..)
            | Self::ServerInitializing { .. }
            | Self::ServerRestarted { .. }
            | Self::SymbolResolution(..)
            | Self::UnknownServers { .. }
            | Self::WorkspaceServersInitializing
            | Self::NoServerConfigured
            | Self::NoServerForWorkspaceTool { .. }
            | Self::ConfigNotFound(..)
            | Self::ConfigInsideWorkspace { .. }
            | Self::Config(..)
            | Self::Io(..)
            | Self::Json(..)
            | Self::TomlDe(..)
            | Self::TomlSer(..)
            | Self::Timeout(..)
            | Self::ServerSpawnFailed { .. }
            | Self::ServerNotFound { .. }
            | Self::LspProtocolError(..)
            | Self::NoResolvableListenUris
            | Self::InvalidPositionInput(..)
            | Self::InvalidRangeInput(..)
            | Self::InvalidHierarchyItemInput(..)
            | Self::PositionBeyondDocument { .. }
            | Self::ResourceUri(..)
            | Self::PathToUri(..)
            | Self::ServerTerminated
            | Self::ShutdownTimeout
            | Self::ServerExitedDuringInit { .. }
            | Self::ServerUnavailable { .. }
            | Self::InvalidKindFilter(..)
            | Self::NotFileUri(..)
            | Self::InvalidTextField(..)
            | Self::InvalidClientPath(..)
            | Self::DocumentLimitExceeded { .. }
            | Self::SubscriptionLimitReached { .. }
            | Self::ListenStreamsExhausted { .. }
            | Self::ListenFilterTooLarge { .. }
            | Self::FileSizeLimitExceeded { .. }
            | Self::NotARegularFile(..)
            | Self::AllServersFailedToInit { .. }
            | Self::CapabilityNotSupported { .. }
            | Self::WorkspaceIndexing { .. } => false,
        }
    }
}

// Not `#[from]`: that would make the inner error this one's `source()`, and the
// error chain sent to clients would then print its message twice.
impl From<InvalidKindFilter> for Error {
    fn from(error: InvalidKindFilter) -> Self {
        Self::InvalidKindFilter(error)
    }
}

impl From<InvalidTextField> for Error {
    fn from(error: InvalidTextField) -> Self {
        Self::InvalidTextField(error)
    }
}

/// A TOML parse error that names where it happened without quoting the source.
///
/// The parser's own `Display` quotes the offending source line, which can hold
/// a secret from an `env` table. This type shows the parser's message and the
/// line and column of the error, or its byte span when the source is not at
/// hand.
#[derive(Debug, Clone)]
pub struct TomlParseError {
    error: toml::de::Error,
    line_column: Option<(usize, usize)>,
}

impl TomlParseError {
    /// Wraps `error`, locating it in the `source` text that was parsed.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::error::TomlParseError;
    ///
    /// let src = "a = = 1";
    /// let err = toml::from_str::<toml::Table>(src).unwrap_err();
    /// let shown = TomlParseError::new(err, src).to_string();
    /// assert!(shown.contains("line 1"));
    /// assert!(!shown.contains(src));
    /// ```
    #[must_use]
    pub fn new(error: toml::de::Error, source: &str) -> Self {
        let line_column = error
            .span()
            .and_then(|span| source.get(..span.start))
            .map(|before| {
                let line = before.matches('\n').count().saturating_add(1);
                let column = before
                    .rsplit('\n')
                    .next()
                    .map_or_default(|last| last.chars().count())
                    .saturating_add(1);
                (line, column)
            });
        Self { error, line_column }
    }

    /// The parser's message, without the source line.
    #[must_use]
    pub fn message(&self) -> &str {
        self.error.message()
    }
}

impl fmt::Display for TomlParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())?;
        if let Some((line, column)) = self.line_column {
            write!(f, " (line {line}, column {column})")
        } else if let Some(span) = self.error.span() {
            write!(f, " (at bytes {}..{})", span.start, span.end)
        } else {
            Ok(())
        }
    }
}

/// A specialized Result type for mcpls-core operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Renders an error followed by every cause in its
/// [`source`](std::error::Error::source) chain, `outer: cause: root`.
///
/// An error type's own message names what failed and leaves the cause to
/// [`std::error::Error::source`]; this is where a reader that wants the whole
/// story (an MCP client, a log line) joins them once. No cause is skipped, so
/// a type must not also print its source in its own message.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
///
/// use mcpls_core::error::{Error, ErrorChain};
///
/// let error = Error::FileIo {
///     path: PathBuf::from("a.rs"),
///     source: std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
/// };
///
/// assert_eq!(ErrorChain(&error).to_string(), "file I/O error for \"a.rs\": gone");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ErrorChain<'a>(pub &'a (dyn std::error::Error + 'static));

impl fmt::Display for ErrorChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut cause = self.0.source();
        while let Some(error) = cause {
            write!(f, ": {error}")?;
            cause = error.source();
        }
        Ok(())
    }
}

/// An error that renders its whole [`ErrorChain`] from `Display`, for a
/// consumer that prints only the outermost error (serde's `try_from` turns the
/// conversion error into a message with `Display`).
#[derive(Debug)]
pub(crate) struct ChainedError<E>(pub(crate) E);

impl<E> From<E> for ChainedError<E> {
    fn from(error: E) -> Self {
        Self(error)
    }
}

impl<E: std::error::Error + 'static> fmt::Display for ChainedError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        ErrorChain(&self.0).fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::config::{FileExtension, FileName};
    use crate::redaction::Redactions;
    use crate::util::{MAX_ECHOED_PATH_BYTES, TRUNCATION_MARKER};

    #[test]
    fn test_all_servers_failed_to_init_error() {
        let failures = vec![
            ServerSpawnFailure {
                server_id: ServerId::from_static("rust"),
                language_id: LanguageId::from_static("rust"),
                command: ServerCommand::from_static("rust-analyzer"),
                reason: StartupFailure::InitTaskPanicked,
            },
            ServerSpawnFailure {
                server_id: ServerId::from_static("python"),
                language_id: LanguageId::from_static("python"),
                command: ServerCommand::from_static("pyright"),
                reason: StartupFailure::InitTaskPanicked,
            },
        ];

        let err = Error::AllServersFailedToInit { failures };

        assert!(err.to_string().contains("all LSP servers failed"));

        // Verify failures are preserved
        if let Error::AllServersFailedToInit { failures: f } = err {
            assert_eq!(f.len(), 2);
            assert_eq!(f[0].language_id, "rust");
            assert_eq!(f[1].language_id, "python");
        } else {
            panic!("Expected AllServersFailedToInit error");
        }
    }

    #[test]
    fn test_server_spawn_failure_display_names_init_panic() {
        let failure = ServerSpawnFailure {
            server_id: ServerId::from_static("typescript"),
            language_id: LanguageId::from_static("typescript"),
            command: ServerCommand::from_static("tsserver"),
            reason: StartupFailure::InitTaskPanicked,
        };

        let display = failure.to_string();
        assert!(display.contains("typescript"));
        assert!(display.contains("tsserver"));
        assert!(display.contains("panicked"));
    }

    #[test]
    fn test_error_display_lsp_init_failed() {
        let err = Error::LspInitFailed {
            phase: InitPhase::Initialize,
            cause: Box::new(Error::Io(std::io::Error::other("server not found"))),
            hint: None,
            stderr: None,
        };
        assert_eq!(
            err.to_string(),
            "LSP server initialization failed: Initialize request failed: server not found"
        );
    }

    #[test]
    fn test_init_errors_append_stderr_to_display() {
        let stderr = StderrExcerpt::complete(b"fatal: bad config", &Redactions::default());
        let failed = Error::LspInitFailed {
            phase: InitPhase::Initialize,
            cause: Box::new(Error::Io(std::io::Error::other("boom"))),
            hint: None,
            stderr: stderr.clone(),
        };
        assert_eq!(
            failed.to_string(),
            "LSP server initialization failed: Initialize request failed: boom; stderr: fatal: bad config"
        );
        let exited = Error::ServerExitedDuringInit {
            command: crate::config::ServerCommand::from_static("gopls"),
            exit_code: Some(2),
            hint: None,
            stderr,
        };
        assert_eq!(
            exited.to_string(),
            "LSP server 'gopls' exited during initialization with exit code 2; stderr: fatal: bad config"
        );
    }

    #[test]
    fn test_error_display_lsp_server_error() {
        let err = Error::LspServerError(JsonRpcError {
            code: -32600,
            message: "Invalid request".to_string(),
            data: None,
        });
        assert_eq!(
            err.to_string(),
            "LSP server error: -32600 - Invalid request"
        );
    }

    #[test]
    fn test_error_display_lsp_server_error_sanitizes_invalid_offset() {
        let err = Error::LspServerError(JsonRpcError {
            code: -32603,
            message: "Invalid offset LineCol { line: 2291, col: 0 } (line index length: 100417)"
                .to_string(),
            data: None,
        });
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
        let err = Error::LspServerError(JsonRpcError {
            code: -32803,
            message: "request handler panicked: Invalid offset LineCol { line: 5, col: 0 } \
                      (line index length: 3)"
                .to_string(),
            data: None,
        });
        assert_eq!(
            err.to_string(),
            "LSP server error: -32803 - position out of range for this document"
        );
    }

    #[test]
    fn test_error_display_lsp_server_error_passes_through_unrelated_message() {
        let err = Error::LspServerError(JsonRpcError {
            code: -32602,
            message: "Invalid params: expected object".to_string(),
            data: None,
        });
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
        let err = no_server_for_language("rust");
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
        assert_eq!(
            Error::Timeout(Duration::from_secs(30)).to_string(),
            "request timed out after 30s"
        );
        assert_eq!(
            Error::Timeout(Duration::from_millis(500)).to_string(),
            "request timed out after 500ms"
        );
    }

    #[test]
    fn test_error_display_document_limit() {
        let err = Error::DocumentLimitExceeded {
            current: 150,
            max: std::num::NonZeroUsize::new(100).unwrap(),
        };
        assert_eq!(
            err.to_string(),
            "document limit exceeded: 150/100 (raise workspace.max_documents in config to increase this)"
        );
    }

    #[test]
    fn test_error_display_file_size_limit() {
        let err = Error::FileSizeLimitExceeded(SizeExceeded {
            size: 20_000_000,
            max: std::num::NonZeroU64::new(10_000_000).unwrap(),
        });
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
        assert_matches!(err, Error::Io(_));
    }

    #[test]
    fn test_error_from_json() {
        let json_str = "{invalid json}";
        let json_err = serde_json::from_str::<serde_json::Value>(json_str).unwrap_err();
        let err: Error = json_err.into();
        assert_matches!(err, Error::Json(_));
    }

    fn assert_rendered_once(err: &(dyn std::error::Error + 'static), marker: &str) {
        let text = ErrorChain(err).to_string();
        assert_eq!(text.matches(marker).count(), 1, "{text}");
        let segments: Vec<&str> = text.split(": ").collect();
        let distinct: std::collections::HashSet<&str> = segments.iter().copied().collect();
        assert_eq!(distinct.len(), segments.len(), "repeated segment in {text}");
    }

    fn io_sourced_cases() -> Vec<(Error, String)> {
        let io = |marker: &str| std::io::Error::other(marker.to_owned());
        let command = ServerCommand::from_static("rust-analyzer");
        vec![
            (
                Error::HttpBind {
                    addr: std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
                    source: io("bind-marker"),
                },
                "bind-marker".to_owned(),
            ),
            (
                Error::ServerSpawnFailed {
                    command: command.clone(),
                    source: io("spawn-marker"),
                },
                "spawn-marker".to_owned(),
            ),
            (
                Error::ServerNotFound {
                    command: command.clone(),
                    io: io("missing-marker"),
                },
                "missing-marker".to_owned(),
            ),
            (
                Error::LspInitFailed {
                    phase: InitPhase::Initialize,
                    cause: Box::new(Error::ServerSpawnFailed {
                        command,
                        source: io("init-marker"),
                    }),
                    hint: None,
                    stderr: None,
                },
                "init-marker".to_owned(),
            ),
            (
                Error::from(ConfigError::UnresolvableWorkspaceRoot {
                    written: PathBuf::from("root"),
                    base_dir: PathBuf::from("base"),
                    probe: PathBuf::from("base/root"),
                    source: io("root-marker"),
                }),
                "root-marker".to_owned(),
            ),
            (Error::from(io("io-marker")), "io-marker".to_owned()),
            (
                Error::FileIo {
                    path: PathBuf::from("a.rs"),
                    source: io("file-marker"),
                },
                "file-marker".to_owned(),
            ),
            (
                Error::MalformedPath {
                    path: PathBuf::from("b.rs"),
                    source: io("path-marker"),
                },
                "path-marker".to_owned(),
            ),
        ]
    }

    /// #735: every variant that carries a cause renders it exactly once.
    #[tokio::test]
    async fn test_error_chain_renders_each_cause_exactly_once() {
        let toml_text = "[invalid toml";
        let toml_err = toml::from_str::<toml::Value>(toml_text).unwrap_err();
        let json_err = serde_json::from_str::<serde_json::Value>("{bad").unwrap_err();
        let json_text = json_err.to_string();
        let join_error = tokio::spawn(async { panic!("task-marker") })
            .await
            .unwrap_err();
        let url_err = url::Url::parse("http://[").unwrap_err();
        let non_utf8 = String::from_utf8(vec![0xff]).unwrap_err();

        let mut cases = io_sourced_cases();
        cases.extend([
            (
                Error::TomlDe(TomlParseError::new(toml_err.clone(), toml_text)),
                toml_err.message().to_owned(),
            ),
            (Error::from(json_err), json_text),
            (
                Error::McpServerStart("mcp-marker".into()),
                "mcp-marker".to_owned(),
            ),
            (
                Error::TaskFailed {
                    task: BackgroundTask::McpService,
                    source: join_error,
                },
                "task-marker".to_owned(),
            ),
            (
                Error::from(ConfigError::UnsupportedFilePattern {
                    server: ServerId::from_static("rust"),
                    pattern: FilePattern::parse("zz/yy").unwrap_err(),
                }),
                "'zz/yy'".to_owned(),
            ),
            (
                Error::from(ConfigError::NotUtf8(non_utf8.clone())),
                non_utf8.to_string(),
            ),
            (
                Error::from(ResourceUriError::UrlParse(url_err)),
                url_err.to_string(),
            ),
            (
                Error::from(ResourceUriError::ClientPath(InvalidClientPath::Empty)),
                InvalidClientPath::Empty.to_string(),
            ),
        ]);
        for (err, marker) in &cases {
            assert_rendered_once(err, marker);
        }
    }

    #[test]
    fn test_startup_failure_spawn_renders_the_cause_once() {
        let failure = StartupFailure::Spawn(Arc::new(Error::ServerSpawnFailed {
            command: ServerCommand::from_static("rust-analyzer"),
            source: std::io::Error::other("spawn-marker"),
        }));
        let text = failure.to_string();
        assert_eq!(text.matches("spawn-marker").count(), 1, "{text}");
    }

    /// #749 FR-014: every error that echoes client text renders a bounded
    /// message for a 1 MiB value, and keeps the whole stored value.
    #[test]
    fn test_client_text_echoes_stay_bounded() {
        let huge = "Z".repeat(1 << 20);
        let path = PathBuf::from(format!("/{huge}"));
        let io = || std::io::Error::other("cause");
        let errors = [
            Error::PathOutsideWorkspace(path.clone()),
            Error::NoWorkspaceRoots(path.clone()),
            Error::NotARegularFile(path.clone()),
            Error::DocumentNotFound(path.clone()),
            Error::PathToUri(path.clone()),
            Error::FileIo {
                path: path.clone(),
                source: io(),
            },
            Error::MalformedPath {
                path: path.clone(),
                source: io(),
            },
            Error::from(ResourceUriError::InvalidPath(huge.clone())),
            Error::from(ResourceUriError::InvalidScheme(huge.clone())),
            Error::from(ResourceUriError::NotAFilePath(huge)),
        ];
        for error in &errors {
            let text = ErrorChain(error).to_string();
            let len = text.len();
            assert!(len <= 2 * MAX_ECHOED_PATH_BYTES, "{len} bytes: {text:.80}");
            assert!(text.contains(TRUNCATION_MARKER), "{text:.80}");
        }
        assert_matches!(&errors[0], Error::PathOutsideWorkspace(stored) if *stored == path);
    }

    /// #749 FR-013: a path at the bound is echoed whole, one byte over is cut.
    #[test]
    fn test_path_echo_variants_cut_one_byte_over_the_bound() {
        let at_bound = PathBuf::from("a".repeat(MAX_ECHOED_PATH_BYTES));
        let text = Error::PathOutsideWorkspace(at_bound).to_string();
        assert!(!text.contains(TRUNCATION_MARKER), "{text:.80}");

        let over = PathBuf::from("a".repeat(MAX_ECHOED_PATH_BYTES + 1));
        let text = Error::NotARegularFile(over).to_string();
        assert!(text.ends_with(TRUNCATION_MARKER), "{text:.80}");

        let controls = PathBuf::from("\u{1b}".repeat(MAX_ECHOED_PATH_BYTES));
        let text = Error::NoWorkspaceRoots(controls).to_string();
        assert!(text.len() <= 2 * MAX_ECHOED_PATH_BYTES, "{}", text.len());
    }

    #[test]
    fn test_document_not_found_and_path_to_uri_escape_control_characters() {
        let path = || PathBuf::from("/a\nERROR forged\u{1b}[31m");

        for error in [Error::DocumentNotFound(path()), Error::PathToUri(path())] {
            let text = error.to_string();
            assert!(!text.contains('\n') && !text.contains('\u{1b}'), "{text}");
        }
    }

    #[test]
    fn test_error_chain_joins_causes_once() {
        let not_found = || std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let file_io = Error::FileIo {
            path: PathBuf::from("a.rs"),
            source: not_found(),
        };
        let spawn = Error::ServerSpawnFailed {
            command: crate::config::ServerCommand::new("x").unwrap(),
            source: not_found(),
        };

        assert_eq!(
            ErrorChain(&file_io).to_string(),
            "file I/O error for \"a.rs\": gone"
        );
        assert_eq!(
            ErrorChain(&spawn).to_string(),
            "failed to spawn LSP server 'x': gone"
        );
    }

    #[test]
    fn test_toml_parse_error_reports_line_and_column_without_the_line() {
        let text = "a = 1\nsecret = \"sk-SECRET\" bad\n";
        let error = toml::from_str::<toml::Value>(text).unwrap_err();

        let rendered = TomlParseError::new(error, text).to_string();

        assert!(rendered.contains("(line 2, column "), "{rendered}");
        assert!(!rendered.contains("sk-SECRET"), "{rendered}");
    }

    /// #751: the parser quotes the source line of a failed entry; an `env`
    /// secret on it must not reach the error text or its chain.
    #[test]
    fn test_toml_error_never_quotes_an_env_secret() {
        let text = "lsp_servers = [{ language_id = \"rust\", command = \"ra\", \
                    env = { API_TOKEN = \"sk-SECRET\\u0000\" } }]";
        let parse_error = toml::from_str::<crate::config::ServerConfig>(text).unwrap_err();
        let err = Error::TomlDe(TomlParseError::new(parse_error, text));

        let rendered = ErrorChain(&err).to_string();
        assert!(!rendered.contains("sk-SECRET"), "{rendered}");
        assert!(
            rendered.contains("environment variable 'API_TOKEN'"),
            "{rendered}"
        );
        assert!(rendered.contains("cannot contain NUL"), "{rendered}");
    }

    #[test]
    fn test_result_type_alias() {
        fn _returns_error() -> Result<i32> {
            Err(Error::Config(ConfigError::NoParentDirectory {
                path: PathBuf::new(),
            }))
        }

        let result: Result<i32> = Ok(42);
        assert!(result.is_ok());
        if let Ok(value) = result {
            assert_eq!(value, 42);
        }
    }

    fn not_found(command: &str) -> Error {
        Error::ServerNotFound {
            command: crate::config::ServerCommand::new(command).unwrap(),
            io: std::io::Error::from(std::io::ErrorKind::NotFound),
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
            command: crate::config::ServerCommand::from_static("rust-analyzer"),
            source: io_err,
        };

        let source = std::error::Error::source(&err);
        assert!(source.is_some());
    }

    fn spawn_failure(id: &str, command: &str, error: Error) -> ServerSpawnFailure {
        ServerSpawnFailure {
            server_id: ServerId::new(id).unwrap(),
            language_id: LanguageId::new(id).unwrap(),
            command: ServerCommand::new(command).unwrap(),
            reason: StartupFailure::Spawn(Arc::new(error)),
        }
    }

    #[test]
    fn test_server_spawn_failure_display() {
        let failure = spawn_failure(
            "rust",
            "rust-analyzer",
            Error::LspInitFailed {
                phase: InitPhase::Initialize,
                cause: Box::new(Error::Io(std::io::Error::other("boom"))),
                hint: None,
                stderr: None,
            },
        );
        assert_eq!(
            failure.to_string(),
            "rust [rust] (rust-analyzer): LSP server initialization failed: Initialize request failed: boom"
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
            server_id: ServerId::from_static("rust"),
            language_id: LanguageId::from_static("rust"),
            command: ServerCommand::from_static("rust-analyzer"),
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
                command: crate::config::ServerCommand::from_static("x"),
                exit_code: Some(1),
                hint: None,
                stderr: None,
            },
            Error::ServerUnavailable {
                server_id: ServerId::from_static("rust"),
                retry_in: Duration::from_secs(1),
            },
        ] {
            assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal, "{err:?}");
        }
    }

    #[test]
    fn test_init_failure_hint_renders_between_detail_and_stderr() {
        let stderr = StderrExcerpt::complete(b"boom", &Redactions::default());
        let hint = Some(InitFailureHint::NativeTypescriptOnly);
        let failed = Error::LspInitFailed {
            phase: InitPhase::Initialize,
            cause: Box::new(Error::Io(std::io::Error::other("x"))),
            hint,
            stderr: stderr.clone(),
        }
        .to_string();
        let exited = Error::ServerExitedDuringInit {
            command: crate::config::ServerCommand::from_static("typescript-language-server"),
            exit_code: Some(1),
            hint,
            stderr,
        }
        .to_string();
        for text in [failed, exited] {
            let hint_at = text.find("TypeScript 7").unwrap();
            assert!(hint_at < text.find("; stderr: boom").unwrap(), "{text}");
            assert!(
                text.contains(BuiltinServer::TypescriptLanguageServer.install_hint()),
                "{text}"
            );
        }
    }

    #[test]
    fn test_server_exited_during_init_display_hints_only_for_rust_analyzer() {
        let hinted = Error::ServerExitedDuringInit {
            command: crate::config::ServerCommand::from_static("rust-analyzer"),
            exit_code: Some(1),
            hint: None,
            stderr: None,
        }
        .to_string();
        assert!(hinted.contains("exit code 1"), "{hinted}");
        assert!(
            hinted.contains("rustup component add rust-analyzer"),
            "{hinted}"
        );

        let plain = Error::ServerExitedDuringInit {
            command: crate::config::ServerCommand::from_static("gopls"),
            exit_code: None,
            hint: None,
            stderr: None,
        }
        .to_string();
        assert!(plain.contains("terminated by a signal"), "{plain}");
        assert!(!plain.contains("rustup"), "{plain}");
    }

    #[test]
    fn test_server_unavailable_display_names_retry_delay() {
        let err = Error::ServerUnavailable {
            server_id: ServerId::from_static("rust"),
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
                        phase: InitPhase::Initialize,
                        cause: Box::new(Error::Io(std::io::Error::other("denied"))),
                        hint: None,
                        stderr: None,
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

    pub(super) fn no_server_for_language(language: &'static str) -> Error {
        Error::NoServerForLanguage {
            language: LanguageId::from_static(language),
            file: FileKey::Unmappable,
            patterns: Arc::default(),
        }
    }

    #[test]
    fn test_no_server_for_plaintext_names_extension_and_patterns() {
        let err = Error::NoServerForLanguage {
            language: LanguageId::PLAINTEXT,
            file: FileKey::Extension(FileExtension::from_static("cpp")),
            patterns: Arc::from([
                FilePattern::from_static("**/*.rs"),
                FilePattern::from_static("**/*.h"),
            ]),
        };
        let message = err.to_string();
        assert!(message.starts_with("no LSP server configured for language: plaintext ("));
        assert!(
            message.contains("file extension 'cpp' is not mapped"),
            "{message}"
        );
        assert!(
            message.contains("configured file_patterns: **/*.rs, **/*.h"),
            "{message}"
        );
    }

    #[test]
    fn test_no_server_for_plaintext_without_patterns_or_extension() {
        let err = Error::NoServerForLanguage {
            language: LanguageId::PLAINTEXT,
            file: FileKey::Unmappable,
            patterns: Arc::default(),
        };
        let message = err.to_string();
        assert!(
            message.contains("the file has no usable extension or name"),
            "{message}"
        );
        assert!(
            message.contains("no file_patterns are configured"),
            "{message}"
        );
    }

    #[test]
    fn test_no_server_for_other_language_text_is_unchanged() {
        let err = Error::NoServerForLanguage {
            language: LanguageId::from_static("nushell"),
            file: FileKey::Extension(FileExtension::from_static("nu")),
            patterns: Arc::from([FilePattern::from_static("**/*.rs")]),
        };
        assert_eq!(
            err.to_string(),
            "no LSP server configured for language: nushell"
        );
    }

    #[test]
    fn test_no_server_for_plaintext_name_suggests_a_name_pattern() {
        let err = Error::NoServerForLanguage {
            language: LanguageId::PLAINTEXT,
            file: FileKey::Name(FileName::from_static("Makefile")),
            patterns: Arc::from([FilePattern::from_static("**/*.rs")]),
        };
        let message = err.to_string();
        assert!(
            message.contains("file name 'Makefile' is not mapped"),
            "{message}"
        );
        assert!(
            message.contains("a `**/Makefile` file_patterns entry"),
            "{message}"
        );
        assert!(!message.contains("*.EXT"), "{message}");
    }

    #[test]
    fn test_error_display_capability_not_supported() {
        let err = Error::CapabilityNotSupported {
            server_id: ServerId::from_static("rust"),
            capability: Capability::Rename,
        };
        assert_eq!(
            err.to_string(),
            "server 'rust' does not support capability 'renameProvider'"
        );
    }

    #[test]
    fn test_error_display_workspace_indexing() {
        let err = Error::WorkspaceIndexing {
            server_id: ServerId::from_static("rust"),
            elapsed_secs: 30,
        };
        assert_eq!(
            err.to_string(),
            "LSP server 'rust' is still indexing the workspace after 30s; wait and retry the request"
        );
    }

    #[tokio::test]
    async fn test_task_failed_keeps_the_panic_payload_as_source() {
        let join_error = tokio::spawn(async { panic!("secret boom") })
            .await
            .unwrap_err();
        let err = Error::TaskFailed {
            task: BackgroundTask::LspReceiver,
            source: join_error,
        };
        let source = std::error::Error::source(&err).unwrap();
        assert!(source.to_string().contains("secret boom"), "{source}");
        assert!(err.to_string().starts_with("LSP receiver task failed"));
    }

    /// #465: server-supplied control characters never reach an error `Display`
    /// raw, so a logged or echoed error cannot forge log lines.
    #[test]
    fn test_display_escapes_control_characters_from_the_server() {
        let server_error = Error::LspServerError(JsonRpcError {
            code: -32603,
            message: "boom\nERROR forged\x1b[31m".to_string(),
            data: None,
        });
        assert_eq!(
            server_error.to_string(),
            "LSP server error: -32603 - boom\\nERROR forged\\u{1b}[31m"
        );

        let protocol_error = Error::LspProtocolError(RedactedText::fixed("bad\nline"));
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

    fn refusal_failure(refusal: UntrustedRefusal) -> ServerSpawnFailure {
        ServerSpawnFailure {
            server_id: ServerId::from_static("rust"),
            language_id: LanguageId::new("rust").unwrap(),
            command: ServerCommand::from_static("rust-analyzer"),
            reason: StartupFailure::RefusedUntrustedWorkspace(refusal),
        }
    }

    #[test]
    fn test_not_allowed_refusal_names_the_server_class_and_the_remedy() {
        let err =
            Error::ServerFailedToStart(Box::new(refusal_failure(UntrustedRefusal::NotAllowed {
                builtin: Some(BuiltinServer::RustAnalyzer),
            })));
        assert_eq!(
            err.to_string(),
            "LSP server 'rust' for language 'rust' was not started: the workspace is untrusted \
             and it may run workspace code (Cargo build scripts and procedural macros); restart \
             mcpls with `--allow-server rust` to start it"
        );
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }

    #[test]
    fn test_workspace_executable_refusal_offers_no_allow_remedy() {
        let err = Error::ServerFailedToStart(Box::new(refusal_failure(
            UntrustedRefusal::WorkspaceExecutable {
                executable: PathBuf::from("/ws/bin/rust-analyzer"),
            },
        )));
        let text = err.to_string();
        assert!(
            text.contains("its executable /ws/bin/rust-analyzer lies inside the workspace"),
            "{text}"
        );
        assert!(!text.contains("--allow-server"), "{text}");
        assert!(!text.contains("not retried"), "{text}");
    }

    #[test]
    fn test_custom_server_refusal_says_its_workspace_code_is_unknown() {
        let text = UntrustedRefusal::NotAllowed { builtin: None }.to_string();
        assert!(text.contains("not a built-in server"), "{text}");
    }

    #[test]
    fn test_config_inside_workspace_is_a_startup_error_of_kind_internal() {
        let err = Error::ConfigInsideWorkspace {
            path: PathBuf::from("/ws/mcpls.toml"),
        };
        assert!(err.to_string().contains("/ws/mcpls.toml"));
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }
}
