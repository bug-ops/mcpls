//! LSP server lifecycle management.
//!
//! This module handles the complete lifecycle of an LSP server:
//! 1. Spawn server process
//! 2. Initialize → initialized handshake
//! 3. Capability negotiation
//! 4. Active request handling
//! 5. Graceful shutdown sequence

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use lsp_types::{
    ClientCapabilities, ClientInfo, ExitNotification, GeneralClientCapabilities, InitializeParams,
    InitializeRequest, InitializeResult, InitializedNotification, InitializedParams,
    PositionEncodingKind, Request, ServerCapabilities, ShutdownRequest, StaleRequestSupportOptions,
    SymbolKind, WorkspaceFolder,
};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, timeout, timeout_at};
use tracing::{debug, info, warn};

use crate::bridge::try_path_to_uri;
use crate::config::{LspServerConfig, ServerId};
use crate::error::{Error, Result, ServerSpawnFailure, StartupFailure};
use crate::lsp::CONTENT_MODIFIED_RETRY_METHODS;
use crate::lsp::client::{LspClient, SHUTDOWN_TIMEOUT};
use crate::lsp::process::ServerProcess;
use crate::lsp::stderr::StderrCapture;
use crate::lsp::transport::LspTransport;
use crate::lsp::types::LspNotification;
use crate::redaction::Redactions;

/// Environment variables passed through to a spawned LSP server even though
/// its environment is otherwise cleared.
///
/// `PATH` lets the server resolve its own toolchain (e.g. rustup shims, venv
/// binaries); `HOME`/`USERPROFILE` and `TMPDIR`/`TEMP`/`TMP` let it find user
/// config/cache and scratch directories.
///
/// This list is not exhaustive: session-specific values that cannot be
/// hardcoded into a static [`LspServerConfig::env`] table (e.g.
/// `SSH_AUTH_SOCK`, which changes every login session) have no way through
/// today. See [`LspServerConfig::env`] for the config-level override/addition
/// mechanism this list feeds into.
const ENV_PASSTHROUGH: &[&str] = &["PATH", "HOME", "USERPROFILE", "TMPDIR", "TEMP", "TMP"];

/// Upper bound [`LspServer::shutdown`] waits for the child process to exit on
/// its own after sending the LSP `exit` notification, before falling back to
/// killing it on drop. Only the leader is awaited; descendants are reaped by
/// the lifeline binding.
const CHILD_EXIT_GRACE: Duration = Duration::from_secs(3);

/// How long a failed `initialize` waits for the child to be reaped before
/// concluding it is still running and keeping the original error.
const EARLY_EXIT_PROBE: Duration = Duration::from_millis(500);

/// Capacity of the diagnostics/log/showMessage notification channel (P3).
///
/// Raised from the pre-#422 value of 64: P1 makes mcpls advertise
/// `window.workDoneProgress`, so a server may now attach progress reporting
/// to ordinary request capabilities too, raising overall notification
/// volume generally even though progress itself moved to its own
/// [`LIFECYCLE_CHANNEL_CAPACITY`] lane.
const NOTIFICATION_CHANNEL_CAPACITY: usize = 256;

/// Capacity of the lifecycle channel (P3): `$/progress` `begin`/`end`
/// frames plus `Other` (rust-analyzer's `experimental/serverStatus`).
///
/// O(phases), not O(reports): a `$/progress` `report` frame is filtered out
/// before it ever reaches this channel (see
/// `lsp::client::LspClient::message_loop_inner`), so this only needs to hold
/// a handful of `begin`/`end` transitions even for a server with several
/// concurrent operations, not a `report`-per-package stream like gopls can
/// produce.
const LIFECYCLE_CHANNEL_CAPACITY: usize = 128;

/// Every symbol kind defined by LSP 3.17 and understood by mcpls.
///
/// Single source of truth for both the `initialize` request's
/// `value_set` and the `workspace/symbol` `kind_filter` validation in
/// [`crate::bridge::translator::symbols`] — the latter derives its accepted
/// string names from this array via `format!("{:?}", kind)`.
pub const SUPPORTED_SYMBOL_KINDS: [SymbolKind; 26] = [
    SymbolKind::File,
    SymbolKind::Module,
    SymbolKind::Namespace,
    SymbolKind::Package,
    SymbolKind::Class,
    SymbolKind::Method,
    SymbolKind::Property,
    SymbolKind::Field,
    SymbolKind::Constructor,
    SymbolKind::Enum,
    SymbolKind::Interface,
    SymbolKind::Function,
    SymbolKind::Variable,
    SymbolKind::Constant,
    SymbolKind::String,
    SymbolKind::Number,
    SymbolKind::Boolean,
    SymbolKind::Array,
    SymbolKind::Object,
    SymbolKind::Key,
    SymbolKind::Null,
    SymbolKind::EnumMember,
    SymbolKind::Struct,
    SymbolKind::Event,
    SymbolKind::Operator,
    SymbolKind::TypeParameter,
];

/// Windows-only additions to [`ENV_PASSTHROUGH`].
///
/// `SystemRoot`/`SystemDrive`/`windir` are required by the Windows process
/// loader itself; `APPDATA`/`LOCALAPPDATA` are read by the Node-based default
/// servers (pyright, typescript-language-server) for global config and
/// cache; the rest are conventionally expected by Windows child processes.
#[cfg(windows)]
const ENV_PASSTHROUGH_WINDOWS: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "windir",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "COMSPEC",
    "PATHEXT",
    "NUMBER_OF_PROCESSORS",
    "USERNAME",
];

/// State of an LSP server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    /// Server has not been initialized.
    Uninitialized,
    /// Server is currently initializing.
    Initializing,
    /// Server is ready to handle requests.
    Ready,
    /// Server is shutting down.
    ShuttingDown,
    /// Server has been shut down.
    Shutdown,
}

impl ServerState {
    /// Check if the server is ready to handle requests.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Check if the server can accept new requests.
    #[must_use]
    pub const fn can_accept_requests(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Configuration for LSP server initialization.
#[derive(Debug, Clone)]
pub struct ServerInitConfig {
    /// LSP server configuration.
    pub server_config: LspServerConfig,
    /// Workspace root paths.
    pub workspace_roots: Vec<PathBuf>,
    /// Initialization options (server-specific JSON).
    pub initialization_options: Option<serde_json::Value>,
    /// Position encoding preference order from
    /// [`crate::config::WorkspaceConfig::position_encodings`].
    ///
    /// Sent as `capabilities.general.positionEncodings` during [`LspServer::spawn`]'s
    /// `initialize` handshake, in the configured order. Values that don't parse
    /// as a valid [`PositionEncodingKind`] are skipped with a warning rather than
    /// failing the handshake: `serve`/`serve_with` validate the top-level
    /// `ServerConfig` via [`crate::config::ServerConfig::validate`] before this
    /// is ever built, but `LspServer::spawn`/`spawn_batch` are `pub` and
    /// reachable directly by a library embedder bypassing that validation
    /// entirely (same reasoning as the `initialize` timeout clamp below), so
    /// this can't assume the value was already checked. If nothing parses,
    /// falls back to `config::default_position_encodings()`'s default.
    pub position_encodings: Vec<String>,
    /// Optional channel for forwarding LSP notifications to the notification cache.
    ///
    /// When `Some`, the spawned LSP client sends every notification it receives
    /// (publishDiagnostics, logMessage, showMessage, …) through this sender.
    /// The caller is responsible for draining the corresponding receiver and
    /// storing entries in [`crate::bridge::NotificationCache`].
    pub notification_tx: Option<mpsc::Sender<LspNotification>>,
}

/// Result of attempting to spawn multiple LSP servers.
///
/// This type enables graceful degradation by collecting both
/// successful initializations and failures. Use the helper methods
/// to inspect the outcome and make decisions about how to proceed.
///
/// # Examples
///
/// ```
/// use mcpls_core::lsp::ServerInitResult;
/// use mcpls_core::error::ServerSpawnFailure;
///
/// let mut result = ServerInitResult::new();
///
/// // Check for different scenarios
/// if result.all_failed() {
///     eprintln!("All servers failed to initialize");
/// } else if result.partial_success() {
///     println!("Some servers succeeded, some failed");
/// } else if result.has_servers() {
///     println!("All servers initialized successfully");
/// }
/// ```
#[derive(Debug)]
pub struct ServerInitResult {
    /// Successfully initialized servers, keyed by routing identity.
    pub servers: HashMap<ServerId, LspServer>,
    /// Failures that occurred during spawn attempts.
    pub failures: Vec<ServerSpawnFailure>,
}

impl ServerInitResult {
    /// Create a new empty result.
    #[must_use]
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            failures: Vec::new(),
        }
    }

    /// Check if any servers were successfully initialized.
    ///
    /// Returns `true` if at least one server is available for use.
    #[must_use]
    pub fn has_servers(&self) -> bool {
        !self.servers.is_empty()
    }

    /// Check if all attempted servers failed.
    ///
    /// Returns `true` only if there were failures and no servers succeeded.
    /// Returns `false` for empty results (no servers configured).
    #[must_use]
    pub fn all_failed(&self) -> bool {
        self.servers.is_empty() && !self.failures.is_empty()
    }

    /// Check if some but not all servers failed.
    ///
    /// Returns `true` if there are both successful servers and failures.
    #[must_use]
    pub fn partial_success(&self) -> bool {
        !self.servers.is_empty() && !self.failures.is_empty()
    }

    /// Get the number of successfully initialized servers.
    #[must_use]
    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    /// Get the number of failures.
    #[must_use]
    pub const fn failure_count(&self) -> usize {
        self.failures.len()
    }

    /// Add a successful server.
    ///
    /// If a server with the same [`ServerId`] already exists, it will be replaced.
    pub fn add_server(&mut self, id: impl Into<ServerId>, server: LspServer) {
        self.servers.insert(id.into(), server);
    }

    /// Add a failure.
    pub fn add_failure(&mut self, failure: ServerSpawnFailure) {
        self.failures.push(failure);
    }
}

impl Default for ServerInitResult {
    fn default() -> Self {
        Self::new()
    }
}

/// Managed LSP server instance with capabilities and encoding.
pub struct LspServer {
    client: LspClient,
    capabilities: ServerCapabilities,
    position_encoding: PositionEncodingKind,
    /// Receiver for push notifications from the LSP server: diagnostics,
    /// log messages, and show-message requests.
    ///
    /// Extract this before registering the server to receive real-time
    /// notifications (e.g., `textDocument/publishDiagnostics`).
    pub notification_rx: mpsc::Receiver<LspNotification>,
    /// Receiver for the lifecycle lane (P3): `$/progress` `begin`/`end`
    /// frames and unrecognized notifications (which carry e.g.
    /// rust-analyzer's `experimental/serverStatus`), kept separate from
    /// [`Self::notification_rx`] so a high-volume diagnostics publisher can
    /// never starve out a workspace-readiness signal, or vice versa.
    ///
    /// Extract this before registering the server, the same way as
    /// [`Self::notification_rx`] -- see [`Self::take_lifecycle_rx`].
    pub lifecycle_rx: mpsc::Receiver<LspNotification>,
    /// Child process handle. Kept alive for process lifetime management and
    /// queried by [`Self::has_exited`] to detect a crash. [`LspServer::shutdown`]
    /// waits for it to exit after sending `exit`; otherwise, or if that wait
    /// times out, dropping it terminates the process via SIGKILL
    /// (`kill_on_drop`). The process is also bound to the mcpls process
    /// lifetime (see [`ServerProcess`]), so it dies with mcpls on any exit.
    ///
    /// `None` only for test fixtures that never spawn a real server process
    /// (see `crate::test_lsp`, `Self::new_for_test_with_encoding`) --
    /// [`Self::spawn`] always populates this with `Some`.
    child: Option<ServerProcess>,
    /// Config this server was spawned from; the single source of its routing
    /// identity, respawn config and indexing policy.
    init_config: ServerInitConfig,
}

impl std::fmt::Debug for LspServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspServer")
            .field("client", &self.client)
            .field("capabilities", &self.capabilities)
            .field("position_encoding", &self.position_encoding)
            .field("notification_rx", &"<channel>")
            .field("lifecycle_rx", &"<channel>")
            .field("child", &"<process>")
            .field("id", &self.init_config.server_config.id())
            .finish()
    }
}

impl LspServer {
    /// The config this server was spawned from.
    pub(crate) const fn init_config(&self) -> &ServerInitConfig {
        &self.init_config
    }

    /// Replace the config a respawn of this server would use.
    #[cfg(all(test, unix))]
    pub(crate) fn set_init_config(&mut self, config: ServerInitConfig) {
        self.init_config = config;
    }

    /// Take the notification receiver out of this server, replacing it with a dummy channel.
    ///
    /// Use this to extract the receiver for a background pump task before registering
    /// the server with the translator. After this call, the server's `notification_rx`
    /// will never receive messages.
    pub fn take_notification_rx(&mut self) -> tokio::sync::mpsc::Receiver<LspNotification> {
        let (_, dummy) = tokio::sync::mpsc::channel(1);
        std::mem::replace(&mut self.notification_rx, dummy)
    }

    /// Take the lifecycle receiver out of this server, replacing it with a
    /// dummy channel -- the lifecycle-lane counterpart to
    /// [`Self::take_notification_rx`]. Extract this before registering the
    /// server for a background pump task to drain, the same way as
    /// [`Self::notification_rx`].
    pub fn take_lifecycle_rx(&mut self) -> tokio::sync::mpsc::Receiver<LspNotification> {
        let (_, dummy) = tokio::sync::mpsc::channel(1);
        std::mem::replace(&mut self.lifecycle_rx, dummy)
    }

    /// Spawn and initialize LSP server.
    ///
    /// This performs the complete initialization sequence:
    /// 1. Spawns the LSP server as a child process
    /// 2. Sends initialize request with client capabilities
    /// 3. Receives server capabilities from initialize response
    /// 4. Sends initialized notification
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Server process fails to spawn
    /// - Initialize request fails or times out
    /// - Server returns error during initialization
    pub async fn spawn(config: ServerInitConfig) -> Result<Self> {
        let redactions = Arc::new(Redactions::for_server(
            &config.server_config,
            std::env::vars_os().filter_map(|(name, value)| {
                Some((name.into_string().ok()?, value.into_string().ok()?))
            }),
        ));
        info!(
            "Spawning LSP server: {} ({} arg(s))",
            config.server_config.command,
            config.server_config.args.len()
        );
        debug!(
            "LSP server args: {:?}",
            config
                .server_config
                .args
                .iter()
                .map(|arg| redactions.apply(arg))
                .collect::<Vec<_>>()
        );

        let command = Self::build_command(&config.server_config, |key| std::env::var_os(key));

        // Log allowlist presence and an override count only — never the
        // configured keys themselves, since `config.server_config.env` may
        // hold secret-bearing names (e.g. `AWS_SECRET_ACCESS_KEY`) whose
        // mere presence in a debug log would be its own disclosure.
        let passthrough_present = {
            let base = ENV_PASSTHROUGH
                .iter()
                .filter(|key| std::env::var_os(key).is_some())
                .count();
            #[cfg(windows)]
            let windows = ENV_PASSTHROUGH_WINDOWS
                .iter()
                .filter(|key| std::env::var_os(key).is_some())
                .count();
            #[cfg(not(windows))]
            let windows = 0;
            base.saturating_add(windows)
        };
        debug!(
            "Effective LSP server env: {passthrough_present} allowlisted key(s) present, \
             {} configured override(s) applied",
            config.server_config.env.len()
        );

        let mut child = ServerProcess::spawn(command)
            .map_err(|e| spawn_error(config.server_config.command.clone(), e))?;

        let stdin = child
            .take_stdin()
            .ok_or_else(|| Error::Transport("Failed to capture stdin".to_string()))?;
        let stdout = child
            .take_stdout()
            .ok_or_else(|| Error::Transport("Failed to capture stdout".to_string()))?;

        let stderr = child
            .take_stderr()
            .ok_or_else(|| Error::Transport("Failed to capture stderr".to_string()))?;
        let stderr_capture = StderrCapture::start(stderr);

        let transport = LspTransport::with_redactions(stdin, stdout, Arc::clone(&redactions));
        let (notification_tx, notification_rx) = mpsc::channel(NOTIFICATION_CHANNEL_CAPACITY);
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel(LIFECYCLE_CHANNEL_CAPACITY);
        let client = LspClient::from_transport_with_notifications(
            config.server_config.clone(),
            transport,
            notification_tx,
            lifecycle_tx,
            Arc::clone(&redactions),
        );
        let (capabilities, position_encoding) = match Self::initialize(&client, &config).await {
            Ok(negotiated) => negotiated,
            Err(init_error) if is_connection_loss(&init_error) => {
                let exit_status = early_exit_status(&mut child).await;
                let stderr = stderr_capture
                    .finish(exit_status.is_some(), &redactions)
                    .await;
                return Err(match exit_status {
                    Some(status) => Error::ServerExitedDuringInit {
                        command: config.server_config.command.clone(),
                        exit_code: status.code(),
                        stderr,
                    },
                    None => Error::LspInitFailed {
                        message: format!("Initialize request failed: {init_error}"),
                        stderr,
                    },
                });
            }
            Err(Error::LspInitFailed { message, .. }) => {
                // The server may be about to exit after printing its reason,
                // so wait the (bounded) end-of-file grace whether or not it
                // has exited yet.
                let stderr = stderr_capture.finish(true, &redactions).await;
                return Err(Error::LspInitFailed { message, stderr });
            }
            Err(init_error) => return Err(init_error),
        };

        info!("LSP server initialized successfully");

        Ok(Self {
            client,
            capabilities,
            position_encoding,
            notification_rx,
            lifecycle_rx,
            child: Some(child),
            init_config: config,
        })
    }

    /// Build the child `Command` for a spawned LSP server, without spawning it.
    ///
    /// The child's environment is cleared, then [`ENV_PASSTHROUGH`] (plus
    /// [`ENV_PASSTHROUGH_WINDOWS`] under `cfg(windows)`) is copied in from
    /// `parent_env` for whichever of those keys it returns `Some` for, then
    /// `config.env` is applied last so it can override any passthrough
    /// value. `parent_env` is injected (production passes
    /// `std::env::var_os`) so tests can supply a fixed environment without
    /// racing on real process-global state.
    fn build_command(
        config: &LspServerConfig,
        parent_env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Command {
        let mut command = Command::new(&config.command);
        command.args(&config.args).env_clear();

        for key in ENV_PASSTHROUGH {
            if let Some(value) = parent_env(key) {
                command.env(key, value);
            }
        }
        #[cfg(windows)]
        for key in ENV_PASSTHROUGH_WINDOWS {
            if let Some(value) = parent_env(key) {
                command.env(key, value);
            }
        }

        command
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        command
    }

    /// Build the `capabilities` mcpls advertises in the `initialize`
    /// request, given the configured position-encoding preference order.
    ///
    /// Extracted from [`Self::initialize`] (S2) so a unit test can assert
    /// directly on the returned value -- in particular that
    /// `window.work_done_progress == Some(true)` (P1), without which no
    /// spec-compliant LSP server may ever initiate `$/progress` at all (see
    /// the matching `window/workDoneProgress/create` allowlist entry in
    /// [`crate::lsp::client::LspClient::server_request_result`]). Before
    /// this existed, deleting those capability lines would silently no-op
    /// the whole feature with an otherwise-green test suite.
    #[allow(clippy::too_many_lines)]
    fn client_capabilities(position_encodings: &[String]) -> ClientCapabilities {
        ClientCapabilities {
            general: Some(GeneralClientCapabilities {
                position_encodings: Some(resolve_position_encodings(position_encodings)),
                stale_request_support: Some(StaleRequestSupportOptions {
                    // mcpls does not implement active in-flight request
                    // cancellation.
                    cancel: false,
                    retry_on_content_modified: CONTENT_MODIFIED_RETRY_METHODS
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                }),
                ..Default::default()
            }),
            text_document: Some(lsp_types::TextDocumentClientCapabilities {
                document_symbol: Some(lsp_types::DocumentSymbolClientCapabilities {
                    dynamic_registration: Some(false),
                    symbol_kind: Some(lsp_types::ClientSymbolKindOptions {
                        value_set: Some(SUPPORTED_SYMBOL_KINDS.to_vec()),
                    }),
                    hierarchical_document_symbol_support: Some(true),
                    ..Default::default()
                }),
                hover: Some(lsp_types::HoverClientCapabilities {
                    dynamic_registration: Some(false),
                    content_format: Some(vec![
                        lsp_types::MarkupKind::Markdown,
                        lsp_types::MarkupKind::PlainText,
                    ]),
                }),
                definition: Some(lsp_types::DefinitionClientCapabilities {
                    dynamic_registration: Some(false),
                    link_support: Some(true),
                }),
                references: Some(lsp_types::ReferenceClientCapabilities {
                    dynamic_registration: Some(false),
                }),
                code_action: Some(lsp_types::CodeActionClientCapabilities {
                    dynamic_registration: Some(false),
                    data_support: Some(true),
                    resolve_support: Some(lsp_types::ClientCodeActionResolveOptions {
                        properties: vec!["edit".to_string()],
                    }),
                    // Declare supported action kinds so the server returns
                    // CodeAction objects (not just legacy Command objects).
                    code_action_literal_support: Some(lsp_types::ClientCodeActionLiteralOptions {
                        code_action_kind: lsp_types::ClientCodeActionKindOptions {
                            value_set: vec![
                                lsp_types::CodeActionKind::Empty,
                                lsp_types::CodeActionKind::QuickFix,
                                lsp_types::CodeActionKind::Refactor,
                                lsp_types::CodeActionKind::RefactorExtract,
                                lsp_types::CodeActionKind::RefactorInline,
                                lsp_types::CodeActionKind::RefactorRewrite,
                                lsp_types::CodeActionKind::Source,
                                lsp_types::CodeActionKind::SourceOrganizeImports,
                            ],
                        },
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            workspace: Some(lsp_types::WorkspaceClientCapabilities {
                workspace_folders: Some(true),
                ..Default::default()
            }),
            // Required per LSP 3.17 before a server may send $/progress at all -- see this fn's doc.
            window: Some(lsp_types::WindowClientCapabilities {
                work_done_progress: Some(true),
                ..Default::default()
            }),
            // Required for rust-analyzer to ever emit experimental/serverStatus; other servers ignore this unrecognized key.
            experimental: Some(serde_json::json!({ "serverStatusNotification": true })),
            ..Default::default()
        }
    }

    /// Perform LSP initialization handshake.
    ///
    /// Sends initialize request and waits for response, then sends initialized notification.
    async fn initialize(
        client: &LspClient,
        config: &ServerInitConfig,
    ) -> Result<(ServerCapabilities, PositionEncodingKind)> {
        debug!("Sending initialize request");

        let workspace_folders: Vec<WorkspaceFolder> = config
            .workspace_roots
            .iter()
            .map(|root| workspace_folder(root))
            .collect::<Result<Vec<_>>>()?;

        let params = InitializeParams {
            process_id: Some(i32::try_from(std::process::id()).unwrap_or(i32::MAX)),
            #[allow(deprecated)]
            root_uri: None,
            initialization_options: config.initialization_options.clone(),
            capabilities: Self::client_capabilities(&config.position_encodings),
            client_info: Some(ClientInfo {
                name: "mcpls".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            workspace_folders_initialize_params: lsp_types::WorkspaceFoldersInitializeParams {
                workspace_folders: Some(lsp_types::WorkspaceFolders::WorkspaceFolderList(
                    workspace_folders,
                )),
            },
            ..Default::default()
        };

        // Use the server's configured timeout for the initialize handshake too,
        // not a hardcoded 30s: large solutions (e.g. a 130-project Unity .sln via
        // OmniSharp) take minutes to respond to `initialize`.
        let result: InitializeResult = client
            .request_typed::<InitializeRequest>(
                params,
                // Clamped for the same reason as `LspClient::request_timeout`:
                // `serve()`/`serve_with()` now validate the top-level
                // `ServerConfig` via `ServerConfig::validate()`, but this call
                // operates on the per-server `config.server_config` reached
                // through `LspServer::spawn`/`spawn_batch`, which bypass that
                // top-level validation entirely, so an out-of-range value (0,
                // or an unbounded one that would silently disable the timeout
                // via tokio's `Instant::far_future()` fallback) is still
                // reachable here and needs a last-line-of-defense clamp.
                Duration::from_secs(
                    config
                        .server_config
                        .timeout_seconds
                        .clamp(1, crate::config::MAX_TIMEOUT_SECONDS),
                ),
            )
            .await
            .map_err(|e| {
                if is_connection_loss(&e) {
                    e
                } else {
                    Error::LspInitFailed {
                        message: format!("Initialize request failed: {e}"),
                        stderr: None,
                    }
                }
            })?;

        let position_encoding = result
            .capabilities
            .position_encoding
            .clone()
            .unwrap_or(PositionEncodingKind::UTF16);

        debug!(
            "Server capabilities received, encoding: {:?}",
            position_encoding
        );

        client
            .notify_typed::<InitializedNotification>(InitializedParams {})
            .await
            .map_err(|e| Error::LspInitFailed {
                message: format!("Initialized notification failed: {e}"),
                stderr: None,
            })?;

        Ok((result.capabilities, position_encoding))
    }

    /// Get server capabilities.
    #[must_use]
    pub const fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }

    /// Get negotiated position encoding.
    #[must_use]
    pub fn position_encoding(&self) -> PositionEncodingKind {
        self.position_encoding.clone()
    }

    /// Get client for making requests.
    #[must_use]
    pub const fn client(&self) -> &LspClient {
        &self.client
    }

    /// Non-blocking check for whether the child process has already exited.
    ///
    /// Uses [`tokio::process::Child::try_wait`] on the leader process, which never blocks waiting
    /// for the process: `true` means it is gone (crashed, killed, or exited
    /// on its own), and any [`LspClient`] obtained from [`Self::client`] is
    /// now permanently disconnected -- new requests through it fail with
    /// [`crate::error::Error::ServerTerminated`]. Callers that want to
    /// recover substitute a freshly [`Self::spawn`]ed replacement.
    ///
    /// Always returns `Ok(false)` for a test fixture with no real backing
    /// process (`self.child` is `None`) -- there is nothing to have exited.
    /// [`Self::spawn`] always populates `child`, so production code never
    /// observes this case.
    ///
    /// # Errors
    ///
    /// Returns an error if the OS fails to report the process's status.
    pub fn has_exited(&mut self) -> Result<bool> {
        match &mut self.child {
            Some(child) => Ok(child.try_wait()?.is_some()),
            None => Ok(false),
        }
    }

    /// Whether this server can no longer serve requests: its child process
    /// has exited, or its message loop has stopped (e.g. it panicked) while
    /// the child is still running.
    ///
    /// Unlike [`Self::has_exited`], this catches a live child whose connection
    /// is already dead, which nothing else would ever respawn. Test fixtures
    /// with no child process are never reported dead.
    ///
    /// # Errors
    ///
    /// Returns an error if the OS fails to report the process's status.
    pub(crate) fn is_dead(&mut self) -> Result<bool> {
        if self.has_exited()? {
            return Ok(true);
        }
        Ok(self.child.is_some() && self.client.is_message_loop_finished())
    }

    /// Shutdown server gracefully, with an overall deadline of [`SHUTDOWN_TIMEOUT`].
    ///
    /// Sends the LSP `shutdown` request, waits for the response, sends the
    /// `exit` notification, stops the message loop, then waits up to a grace
    /// period (never past the overall deadline) for the child process to exit
    /// on its own. If it hasn't by then, or if the handshake itself fails or
    /// times out, the child is simply dropped here — `kill_on_drop`
    /// terminates it via SIGKILL (a no-op if it has already exited); on Windows
    /// this also kills its descendants. A test
    /// fixture with no real backing process (`child` is `None`) skips this
    /// step entirely -- there is nothing to wait for or kill.
    ///
    /// # Errors
    ///
    /// Returns the first error of the handshake or the message-loop stop,
    /// including [`Error::ShutdownTimeout`] when the deadline elapsed. The
    /// child process is still torn down (gracefully if it exits in time,
    /// killed otherwise) regardless of whether this returns `Ok` or `Err`.
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "SHUTDOWN_TIMEOUT and CHILD_EXIT_GRACE are small constants"
    )]
    pub async fn shutdown(self) -> Result<()> {
        debug!("Shutting down LSP server");

        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let client = self.client;

        let handshake: Result<()> = timeout_at(deadline, async {
            let _: serde_json::Value = client
                .request(ShutdownRequest::METHOD.as_str(), (), Duration::from_secs(5))
                .await?;
            client.notify_typed::<ExitNotification>(()).await
        })
        .await
        .unwrap_or(Err(Error::ShutdownTimeout));
        let handshake = match client.shutdown_until(deadline).await {
            Ok(()) => handshake,
            Err(e) => handshake.and(Err(e)),
        };

        if let Some(mut child) = self.child {
            let child_deadline = deadline.min(Instant::now() + CHILD_EXIT_GRACE);
            match timeout_at(child_deadline, child.wait()).await {
                Ok(Ok(status)) => {
                    debug!(
                        ?status,
                        "LSP server process exited after `exit` notification"
                    );
                }
                Ok(Err(e)) => warn!(error = %e, "failed to wait for LSP server process exit"),
                Err(_) => warn!(
                    timeout = ?CHILD_EXIT_GRACE,
                    "LSP server process did not exit within grace period after `exit` \
                     notification, killing it"
                ),
            }
            // `child` drops here: it kills the leader (and on Windows the job) if still
            // running, and is a no-op if `wait()` above already reaped it.
        }

        handshake?;
        info!("LSP server shut down successfully");
        Ok(())
    }

    /// Spawn multiple LSP servers in batch mode with graceful degradation.
    ///
    /// Attempts to spawn and initialize all configured servers. If some servers
    /// fail to spawn, the successful servers are still returned. This enables
    /// graceful degradation where the system can continue to operate with
    /// partial functionality.
    ///
    /// # Behavior
    ///
    /// - Attempts to spawn each server sequentially
    /// - Logs success (info) and failure (error) for each server
    /// - Accumulates successful servers and failures
    /// - Never panics or returns early - attempts all servers
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::lsp::{LspServer, ServerInitConfig};
    /// use mcpls_core::config::LspServerConfig;
    /// use std::path::PathBuf;
    ///
    /// # async fn example() {
    /// let configs = vec![
    ///     ServerInitConfig {
    ///         server_config: LspServerConfig::rust_analyzer(),
    ///         workspace_roots: vec![PathBuf::from("/workspace")],
    ///         initialization_options: None,
    ///         position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
    ///         notification_tx: None,
    ///     },
    ///     ServerInitConfig {
    ///         server_config: LspServerConfig::pyright(),
    ///         workspace_roots: vec![PathBuf::from("/workspace")],
    ///         initialization_options: None,
    ///         position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
    ///         notification_tx: None,
    ///     },
    /// ];
    ///
    /// let result = LspServer::spawn_batch(&configs).await;
    ///
    /// if result.has_servers() {
    ///     println!("Successfully spawned {} servers", result.server_count());
    /// }
    ///
    /// if result.partial_success() {
    ///     eprintln!("Warning: {} servers failed", result.failure_count());
    /// }
    /// # }
    /// ```
    pub async fn spawn_batch(configs: &[ServerInitConfig]) -> ServerInitResult {
        let mut result = ServerInitResult::new();

        for config in configs {
            let server_id = config.server_config.id();
            let language_id = config.server_config.language_id.clone();
            let command = config.server_config.command.clone();

            match Self::spawn(config.clone()).await {
                Ok(server) => {
                    info!(
                        "Successfully spawned LSP server: {} ({})",
                        server_id, command
                    );
                    result.add_server(server_id, server);
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to spawn LSP server: {} ({}): {}",
                        server_id,
                        command,
                        e
                    );
                    result.add_failure(ServerSpawnFailure {
                        server_id,
                        language_id,
                        command,
                        reason: StartupFailure::Spawn(Arc::new(e)),
                    });
                }
            }
        }

        result
    }
}

/// Convert configured position-encoding strings into the ordered
/// [`PositionEncodingKind`] list offered during the `initialize` handshake.
///
/// Values that don't parse are skipped with a warning instead of failing the
/// handshake (see [`ServerInitConfig::position_encodings`] for why this can't
/// assume [`crate::config::ServerConfig::validate`] already ran). Falls back
/// to `config::default_position_encodings()` -- the same default used when
/// nothing is configured at all -- if no configured value parses.
fn resolve_position_encodings(configured: &[String]) -> Vec<PositionEncodingKind> {
    let encodings: Vec<PositionEncodingKind> = configured
        .iter()
        .filter_map(|value| {
            let kind = crate::config::parse_position_encoding(value);
            if kind.is_none() {
                warn!(value = %value, "ignoring invalid configured position encoding");
            }
            kind
        })
        .collect();

    if encodings.is_empty() {
        crate::config::default_position_encodings()
            .iter()
            .filter_map(|value| crate::config::parse_position_encoding(value))
            .collect()
    } else {
        encodings
    }
}

/// Whether `error` means the connection to the server is gone, as opposed to
/// the server answering with a failure of its own.
const fn is_connection_loss(error: &Error) -> bool {
    matches!(error, Error::ServerTerminated | Error::Transport(_))
}

/// Exit status of `child` if it has exited (or does so within
/// [`EARLY_EXIT_PROBE`]), `None` if it is still running.
async fn early_exit_status(child: &mut ServerProcess) -> Option<std::process::ExitStatus> {
    timeout(EARLY_EXIT_PROBE, child.wait()).await.ok()?.ok()
}

/// Classify a spawn failure: a missing executable gets its own variant so the
/// message can carry PATH and install guidance.
fn spawn_error(command: String, source: std::io::Error) -> Error {
    if source.kind() == std::io::ErrorKind::NotFound {
        Error::ServerNotFound { command, source }
    } else {
        Error::ServerSpawnFailed { command, source }
    }
}

/// Build the `workspace/workspaceFolders` entry for one configured root.
///
/// Reserved characters have to be percent-encoded here: an unencoded `#`
/// would truncate the path into a URI fragment, and `[` / `]` are rejected
/// outright by `Uri`.
fn workspace_folder(root: &Path) -> Result<WorkspaceFolder> {
    let uri = try_path_to_uri(root).ok_or_else(|| {
        let root_display = root.display();
        Error::InvalidUri(format!("Invalid workspace root: {root_display}"))
    })?;
    Ok(WorkspaceFolder {
        uri,
        name: root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace")
            .to_string(),
    })
}

/// Builds an `LspServer` that can be registered without a real language
/// server: an inert, in-memory transport (see `crate::test_lsp`) and no
/// backing child process (`child: None`) -- neither is ever read from or
/// written to.
///
/// `pub` rather than private to this module's own `tests` (`lifecycle` is a
/// private module, so this stays crate-scoped in practice, per the
/// `redundant_pub_crate` clippy lint): it constructs `LspServer` via a
/// struct literal, which only code inside this module can do (all its
/// fields are private), so this is the one place other modules'
/// shutdown-path tests (`bridge::translator`, `lib.rs`) can get a real,
/// registerable `LspServer` from.
#[cfg(test)]
pub fn fake_lsp_server() -> LspServer {
    fake_lsp_server_with_config(LspServerConfig::pyright())
}

/// As [`fake_lsp_server`], with a caller-chosen config, which is also what
/// the returned server reports as its `init_config`.
#[cfg(test)]
pub fn fake_lsp_server_with_config(server_config: LspServerConfig) -> LspServer {
    let transport = crate::test_lsp::inert_transport();
    let client = LspClient::from_transport(server_config.clone(), transport);
    let (_, mock_notification_rx) = mpsc::channel(1);
    let (_, mock_lifecycle_rx) = mpsc::channel(1);
    LspServer {
        client,
        capabilities: lsp_types::ServerCapabilities::default(),
        position_encoding: PositionEncodingKind::UTF8,
        notification_rx: mock_notification_rx,
        lifecycle_rx: mock_lifecycle_rx,
        child: None,
        init_config: crate::test_lsp::init_config_for(server_config),
    }
}

/// A server whose child process (`sleep`) is alive but whose message loop
/// dies at once on its inert transport -- the shape of a panicked loop.
#[cfg(all(test, unix))]
pub fn fake_lsp_server_with_dead_loop_and_live_child() -> LspServer {
    let child = tokio::process::Command::new("sleep")
        .arg("30")
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn sleep: {e}"));
    let mut server = fake_lsp_server();
    server.child = Some(ServerProcess::from_unbound(child));
    server
}

#[cfg(test)]
impl LspServer {
    /// Construct an `LspServer` fixture carrying the given capabilities, for
    /// tests elsewhere in the crate that need to drive capability-gated
    /// dispatch paths in `Translator` without spawning a real language server.
    ///
    /// The underlying client and child process (`child: None`) are inert
    /// placeholders — only `capabilities()` is meaningful on the returned
    /// value.
    ///
    /// Uses `LspClient::new` (uninitialized, no background task) rather than
    /// `LspClient::from_transport`, so this does not depend on the Tokio
    /// message loop or a real process at all.
    pub(crate) fn new_for_test(capabilities: ServerCapabilities) -> Self {
        Self::new_for_test_with_encoding(capabilities, PositionEncodingKind::UTF16)
    }

    /// As [`Self::new_for_test`], but with a caller-chosen negotiated
    /// encoding -- for tests exercising a non-UTF-16 conversion path (e.g.
    /// `EncodingCtx`-driven range conversion) without spawning a real
    /// process.
    pub(crate) fn new_for_test_with_encoding(
        capabilities: ServerCapabilities,
        position_encoding: PositionEncodingKind,
    ) -> Self {
        let client = LspClient::new(LspServerConfig::rust_analyzer());
        let (_, notification_rx) = mpsc::channel(1);
        let (_, lifecycle_rx) = mpsc::channel(1);

        Self {
            client,
            capabilities,
            position_encoding,
            notification_rx,
            lifecycle_rx,
            child: None,
            init_config: crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::assert_matches;

    use super::*;

    #[test]
    fn test_resolve_position_encodings_preserves_configured_order() {
        let result = resolve_position_encodings(&["utf-32".to_string(), "utf-8".to_string()]);
        assert_eq!(
            result,
            vec![PositionEncodingKind::UTF32, PositionEncodingKind::UTF8]
        );
    }

    #[test]
    fn test_resolve_position_encodings_skips_invalid_and_keeps_valid() {
        let result = resolve_position_encodings(&["utf-7".to_string(), "utf-16".to_string()]);
        assert_eq!(result, vec![PositionEncodingKind::UTF16]);
    }

    #[test]
    fn test_resolve_position_encodings_falls_back_when_all_invalid() {
        let result = resolve_position_encodings(&["utf-7".to_string(), "bogus".to_string()]);
        assert_eq!(
            result,
            vec![PositionEncodingKind::UTF8, PositionEncodingKind::UTF16]
        );
    }

    #[test]
    fn test_resolve_position_encodings_falls_back_when_empty() {
        let result = resolve_position_encodings(&[]);
        assert_eq!(
            result,
            vec![PositionEncodingKind::UTF8, PositionEncodingKind::UTF16]
        );
    }

    /// P1/S2: without `window.work_done_progress == Some(true)`, no
    /// spec-compliant LSP server may ever initiate `$/progress` at all.
    /// Regression guard: deleting the capability lines in
    /// `client_capabilities` must fail this test, not silently no-op the
    /// whole feature with an otherwise-green suite.
    #[test]
    fn test_client_capabilities_advertises_work_done_progress() {
        let capabilities =
            LspServer::client_capabilities(&["utf-8".to_string(), "utf-16".to_string()]);

        assert_eq!(
            capabilities.window.and_then(|w| w.work_done_progress),
            Some(true)
        );
    }

    #[test]
    fn test_server_state_ready() {
        assert!(ServerState::Ready.is_ready());
        assert!(ServerState::Ready.can_accept_requests());
    }

    #[test]
    fn test_server_state_uninitialized() {
        assert!(!ServerState::Uninitialized.is_ready());
        assert!(!ServerState::Uninitialized.can_accept_requests());
    }

    #[test]
    fn test_server_state_initializing() {
        assert!(!ServerState::Initializing.is_ready());
        assert!(!ServerState::Initializing.can_accept_requests());
    }

    #[test]
    fn test_workspace_folder_encodes_fragment_char() {
        // An unencoded `#` parses as a fragment, silently handing the server
        // the parent directory as its root.
        #[cfg(windows)]
        let (root, expected) = (
            Path::new(r"C:\home\me\dev\#work"),
            "file:///C:/home/me/dev/%23work",
        );
        #[cfg(not(windows))]
        let (root, expected) = (
            Path::new("/home/me/dev/#work"),
            "file:///home/me/dev/%23work",
        );

        let folder = workspace_folder(root).unwrap();

        assert_eq!(folder.uri.as_ref(), expected);
        assert_eq!(folder.name, "#work");
    }

    #[test]
    fn test_workspace_folder_encodes_bracket_chars() {
        #[cfg(windows)]
        let (root, expected) = (
            Path::new(r"C:\home\me\dev\[env]"),
            "file:///C:/home/me/dev/%5Benv%5D",
        );
        #[cfg(not(windows))]
        let (root, expected) = (
            Path::new("/home/me/dev/[env]"),
            "file:///home/me/dev/%5Benv%5D",
        );

        let folder = workspace_folder(root).unwrap();

        assert_eq!(folder.uri.as_ref(), expected);
        assert_eq!(folder.name, "[env]");
    }

    #[test]
    fn test_workspace_folder_rejects_relative_root() {
        let err = workspace_folder(Path::new("relative/root")).unwrap_err();
        assert_matches!(err, Error::InvalidUri(_), "got {err:?}");
    }

    #[test]
    fn test_server_state_shutting_down() {
        assert!(!ServerState::ShuttingDown.is_ready());
        assert!(!ServerState::ShuttingDown.can_accept_requests());
    }

    #[test]
    fn test_server_state_shutdown() {
        assert!(!ServerState::Shutdown.is_ready());
        assert!(!ServerState::Shutdown.can_accept_requests());
    }

    #[test]
    fn test_server_state_equality() {
        assert_eq!(ServerState::Ready, ServerState::Ready);
        assert_ne!(ServerState::Ready, ServerState::Uninitialized);
        assert_eq!(ServerState::Shutdown, ServerState::Shutdown);
    }

    #[test]
    fn test_server_state_clone() {
        let state = ServerState::Ready;
        let cloned = state;
        assert_eq!(state, cloned);
    }

    #[test]
    fn test_server_state_debug() {
        let state = ServerState::Ready;
        let debug_str = format!("{state:?}");
        assert!(debug_str.contains("Ready"));
    }

    #[test]
    fn test_server_init_config_clone() {
        let config = ServerInitConfig {
            server_config: LspServerConfig::rust_analyzer(),
            workspace_roots: vec![PathBuf::from("/tmp/workspace")],
            initialization_options: Some(serde_json::json!({"key": "value"})),
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
            notification_tx: None,
        };

        #[allow(clippy::redundant_clone)]
        let cloned = config.clone();
        assert_eq!(cloned.server_config.language_id, "rust");
        assert_eq!(cloned.workspace_roots.len(), 1);
    }

    #[test]
    fn test_server_init_config_debug() {
        let config = ServerInitConfig {
            server_config: LspServerConfig::pyright(),
            workspace_roots: vec![],
            initialization_options: None,
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
            notification_tx: None,
        };

        let debug_str = format!("{config:?}");
        assert!(debug_str.contains("python"));
        assert!(debug_str.contains("pyright"));
    }

    #[test]
    fn test_server_init_config_with_options() {
        use std::collections::HashMap;

        let init_opts = serde_json::json!({
            "settings": {
                "python": {
                    "analysis": {
                        "typeCheckingMode": "strict"
                    }
                }
            }
        });

        let mut env = HashMap::new();
        env.insert("PYTHONPATH".to_string(), "/usr/lib".to_string());

        let config = ServerInitConfig {
            server_config: LspServerConfig {
                language_id: "python".to_string(),
                command: "pyright-langserver".to_string(),
                args: vec!["--stdio".to_string()],
                env,
                file_patterns: vec!["**/*.py".to_string()],
                initialization_options: Some(init_opts.clone()),
                timeout_seconds: 10,
                request_timeout_seconds: 10,
                heuristics: None,
                name: None,
                handles: None,
                indexing: crate::bridge::IndexingPolicy::Auto,
            },
            workspace_roots: vec![PathBuf::from("/workspace")],
            initialization_options: Some(init_opts),
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
            notification_tx: None,
        };

        assert!(config.initialization_options.is_some());
        assert_eq!(config.workspace_roots.len(), 1);
    }

    #[test]
    fn test_server_init_config_empty_workspace() {
        let config = ServerInitConfig {
            server_config: LspServerConfig::typescript(),
            workspace_roots: vec![],
            initialization_options: None,
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
            notification_tx: None,
        };

        assert_eq!(config.workspace_roots.len(), 0);
    }

    #[test]
    fn test_server_init_config_multiple_workspaces() {
        let config = ServerInitConfig {
            server_config: LspServerConfig::rust_analyzer(),
            workspace_roots: vec![
                PathBuf::from("/workspace1"),
                PathBuf::from("/workspace2"),
                PathBuf::from("/workspace3"),
            ],
            initialization_options: None,
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
            notification_tx: None,
        };

        assert_eq!(config.workspace_roots.len(), 3);
    }

    /// #249: `has_exited` must distinguish a live child from one that has
    /// already exited, since this is the signal the respawn path relies on
    /// to detect a crashed LSP server.
    ///
    /// Unix-only: spawns a real `sleep` subprocess, which is unavailable on
    /// the Windows CI runner.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_has_exited_reflects_child_process_state() {
        use lsp_types::ServerCapabilities;

        let mut mock_child = tokio::process::Command::new("sleep")
            .arg("2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mock_stdin = mock_child.stdin.take().unwrap();
        let mock_stdout = mock_child.stdout.take().unwrap();

        let transport = LspTransport::new(mock_stdin, mock_stdout);
        let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);
        let (_, mock_notification_rx) = mpsc::channel(1);
        let (_, mock_lifecycle_rx) = mpsc::channel(1);

        let mut server = LspServer {
            client,
            capabilities: ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            notification_rx: mock_notification_rx,
            lifecycle_rx: mock_lifecycle_rx,
            child: Some(ServerProcess::from_unbound(mock_child)),
            init_config: crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer()),
        };

        assert!(
            !server.has_exited().unwrap(),
            "freshly spawned `sleep 2` should still be running"
        );

        server.child.as_mut().unwrap().kill().await.unwrap();
        // `kill().await` waits for the process to actually exit, so the
        // very next `try_wait` reliably observes it as gone.
        assert!(
            server.has_exited().unwrap(),
            "killed child must report as exited"
        );
    }

    /// A live child whose message loop has stopped must be reported dead.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_is_dead_when_message_loop_finished_but_child_alive() {
        let mut server = fake_lsp_server_with_dead_loop_and_live_child();

        tokio::time::timeout(Duration::from_secs(2), async {
            while !server.client.is_message_loop_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        assert!(!server.has_exited().unwrap());
        assert!(server.is_dead().unwrap());
    }

    /// A server that never answers `shutdown` fails the handshake with the
    /// request timeout, well inside `SHUTDOWN_TIMEOUT`, and still stops its
    /// message loop.
    #[tokio::test(start_paused = true)]
    async fn test_shutdown_reports_handshake_timeout_within_deadline() {
        let (client, _fake_server) = crate::test_lsp::fake_lsp_client();
        let probe = client.clone();
        let (_, notification_rx) = mpsc::channel(1);
        let (_, lifecycle_rx) = mpsc::channel(1);
        let server = LspServer {
            client,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            notification_rx,
            lifecycle_rx,
            child: None,
            init_config: crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer()),
        };

        let started = Instant::now();
        let result = server.shutdown().await;

        assert_matches!(result, Err(Error::Timeout(_)), "got {result:?}");
        assert!(started.elapsed() <= SHUTDOWN_TIMEOUT);
        assert_matches!(probe.state().await, ServerState::Shutdown);
    }

    #[tokio::test]
    async fn test_is_dead_false_for_childless_fixture() {
        let mut server = fake_lsp_server();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!server.is_dead().unwrap());
    }

    #[tokio::test]
    async fn test_lsp_server_getters() {
        use lsp_types::ServerCapabilities;

        let transport = crate::test_lsp::inert_transport();
        let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);
        let (_, mock_notification_rx) = mpsc::channel(1);
        let (_, mock_lifecycle_rx) = mpsc::channel(1);

        let server = LspServer {
            client,
            capabilities: ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            notification_rx: mock_notification_rx,
            lifecycle_rx: mock_lifecycle_rx,
            child: None,
            init_config: crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer()),
        };

        assert_eq!(server.position_encoding(), PositionEncodingKind::UTF8);
        assert!(server.capabilities().text_document_sync.is_none());

        let debug_str = format!("{server:?}");
        assert!(debug_str.contains("LspServer"));
        assert!(debug_str.contains("<process>"));
    }

    #[test]
    fn test_server_init_result_new_empty() {
        let result = ServerInitResult::new();
        assert!(!result.has_servers());
        assert!(!result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 0);
        assert_eq!(result.failure_count(), 0);
    }

    #[test]
    fn test_server_init_result_default() {
        let result = ServerInitResult::default();
        assert!(!result.has_servers());
        assert_eq!(result.server_count(), 0);
        assert_eq!(result.failure_count(), 0);
    }

    #[test]
    fn test_server_init_result_all_failures() {
        let mut result = ServerInitResult::new();

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("rust"),
            language_id: "rust".to_string(),
            command: "rust-analyzer".to_string(),
            reason: StartupFailure::InitTaskPanicked,
        });

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("python"),
            language_id: "python".to_string(),
            command: "pyright".to_string(),
            reason: StartupFailure::InitTaskPanicked,
        });

        assert!(!result.has_servers());
        assert!(result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 0);
        assert_eq!(result.failure_count(), 2);
    }

    #[tokio::test]
    async fn test_server_init_result_all_success() {
        let mut result = ServerInitResult::new();

        let transport1 = crate::test_lsp::inert_transport();
        let client1 = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport1);
        let (_, mock_notification_rx1) = mpsc::channel(1);
        let (_, mock_lifecycle_rx1) = mpsc::channel(1);

        let server1 = LspServer {
            client: client1,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            notification_rx: mock_notification_rx1,
            lifecycle_rx: mock_lifecycle_rx1,
            child: None,
            init_config: crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer()),
        };

        result.add_server("rust".to_string(), server1);

        assert!(result.has_servers());
        assert!(!result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 1);
        assert_eq!(result.failure_count(), 0);
    }

    #[tokio::test]
    async fn test_server_init_result_partial_success() {
        let mut result = ServerInitResult::new();

        let transport = crate::test_lsp::inert_transport();
        let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);
        let (_, mock_notification_rx) = mpsc::channel(1);
        let (_, mock_lifecycle_rx) = mpsc::channel(1);

        let server = LspServer {
            client,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            notification_rx: mock_notification_rx,
            lifecycle_rx: mock_lifecycle_rx,
            child: None,
            init_config: crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer()),
        };

        result.add_server("rust".to_string(), server);

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("python"),
            language_id: "python".to_string(),
            command: "pyright".to_string(),
            reason: StartupFailure::InitTaskPanicked,
        });

        assert!(result.has_servers());
        assert!(!result.all_failed());
        assert!(result.partial_success());
        assert_eq!(result.server_count(), 1);
        assert_eq!(result.failure_count(), 1);
    }

    #[tokio::test]
    async fn test_server_init_result_multiple_servers() {
        let mut result = ServerInitResult::new();

        for i in 0..3 {
            let transport = crate::test_lsp::inert_transport();
            let config = if i == 0 {
                LspServerConfig::rust_analyzer()
            } else if i == 1 {
                LspServerConfig::pyright()
            } else {
                LspServerConfig::typescript()
            };
            let client = LspClient::from_transport(config.clone(), transport);
            let (_, mock_notification_rx) = mpsc::channel(1);
            let (_, mock_lifecycle_rx) = mpsc::channel(1);

            let server = LspServer {
                client,
                capabilities: lsp_types::ServerCapabilities::default(),
                position_encoding: PositionEncodingKind::UTF8,
                notification_rx: mock_notification_rx,
                lifecycle_rx: mock_lifecycle_rx,
                child: None,
                init_config: crate::test_lsp::init_config_for(config.clone()),
            };

            result.add_server(config.language_id, server);
        }

        assert!(result.has_servers());
        assert!(!result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 3);
        assert_eq!(result.failure_count(), 0);
    }

    #[tokio::test]
    async fn test_server_init_result_replace_server() {
        let mut result = ServerInitResult::new();

        let transport1 = crate::test_lsp::inert_transport();
        let client1 = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport1);
        let (_, mock_notification_rx1) = mpsc::channel(1);
        let (_, mock_lifecycle_rx1) = mpsc::channel(1);

        let server1 = LspServer {
            client: client1,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            notification_rx: mock_notification_rx1,
            lifecycle_rx: mock_lifecycle_rx1,
            child: None,
            init_config: crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer()),
        };

        result.add_server("rust".to_string(), server1);
        assert_eq!(result.server_count(), 1);

        let transport2 = crate::test_lsp::inert_transport();
        let client2 = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport2);
        let (_, mock_notification_rx2) = mpsc::channel(1);
        let (_, mock_lifecycle_rx2) = mpsc::channel(1);

        let server2 = LspServer {
            client: client2,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF16,
            notification_rx: mock_notification_rx2,
            lifecycle_rx: mock_lifecycle_rx2,
            child: None,
            init_config: crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer()),
        };

        result.add_server("rust".to_string(), server2);
        assert_eq!(result.server_count(), 1);
    }

    #[test]
    fn test_server_init_result_debug() {
        let mut result = ServerInitResult::new();

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("rust"),
            language_id: "rust".to_string(),
            command: "rust-analyzer".to_string(),
            reason: StartupFailure::InitTaskPanicked,
        });

        let debug_str = format!("{result:?}");
        assert!(debug_str.contains("ServerInitResult"));
    }

    #[test]
    fn test_server_init_result_multiple_failures() {
        let mut result = ServerInitResult::new();

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("python"),
            language_id: "python".to_string(),
            command: "pyright".to_string(),
            reason: StartupFailure::InitTaskPanicked,
        });

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("typescript"),
            language_id: "typescript".to_string(),
            command: "tsserver".to_string(),
            reason: StartupFailure::InitTaskPanicked,
        });

        assert_eq!(result.failure_count(), 2);
        assert_eq!(result.server_count(), 0);
        assert!(result.all_failed());
        assert!(!result.partial_success());
    }

    #[tokio::test]
    async fn test_spawn_batch_empty_configs() {
        let configs: &[ServerInitConfig] = &[];
        let result = LspServer::spawn_batch(configs).await;

        assert!(!result.has_servers());
        assert!(!result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 0);
        assert_eq!(result.failure_count(), 0);
    }

    #[test]
    fn test_spawn_error_classifies_not_found() {
        use std::io::{Error as IoError, ErrorKind};

        let missing = spawn_error("x".to_string(), IoError::from(ErrorKind::NotFound));
        assert_matches!(missing, Error::ServerNotFound { .. });
        let denied = spawn_error("x".to_string(), IoError::from(ErrorKind::PermissionDenied));
        assert_matches!(denied, Error::ServerSpawnFailed { .. });
    }

    #[tokio::test]
    async fn test_spawn_nonexistent_command_is_server_not_found() {
        let mut server_config = LspServerConfig::rust_analyzer();
        server_config.command = "nonexistent-lsp-cmd-xyz".to_string();
        let config = ServerInitConfig {
            server_config,
            workspace_roots: vec![],
            initialization_options: None,
            position_encodings: vec![],
            notification_tx: None,
        };
        let err = LspServer::spawn(config).await.unwrap_err();
        assert_matches!(err, Error::ServerNotFound { .. }, "got {err:?}");
    }

    #[tokio::test]
    async fn test_spawn_batch_single_invalid_config() {
        let configs = vec![ServerInitConfig {
            server_config: LspServerConfig {
                language_id: "rust".to_string(),
                command: "nonexistent-command-12345".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
                file_patterns: vec!["**/*.rs".to_string()],
                initialization_options: None,
                timeout_seconds: 10,
                request_timeout_seconds: 10,
                heuristics: None,
                name: None,
                handles: None,
                indexing: crate::bridge::IndexingPolicy::Auto,
            },
            workspace_roots: vec![],
            initialization_options: None,
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
            notification_tx: None,
        }];

        let result = LspServer::spawn_batch(&configs).await;

        assert!(!result.has_servers());
        assert!(result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 0);
        assert_eq!(result.failure_count(), 1);

        let failure = &result.failures[0];
        assert_eq!(failure.language_id, "rust");
        assert_eq!(failure.command, "nonexistent-command-12345");
        assert_matches!(&failure.reason, StartupFailure::Spawn(e) if matches!(**e, Error::ServerNotFound { .. }),
            "got {:?}",
            failure.reason
        );
        assert!(failure.to_string().contains("failed to spawn"));
    }

    #[test]
    fn test_is_connection_loss_excludes_server_replies() {
        assert!(is_connection_loss(&Error::ServerTerminated));
        assert!(is_connection_loss(&Error::Transport("eof".to_string())));
        assert!(!is_connection_loss(&Error::LspServerError {
            code: -32603,
            message: "bad".to_string(),
            data: None,
        }));
    }

    /// A server that answers `initialize` with an error and then exits keeps
    /// its own error instead of being reported as an early exit.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawn_keeps_servers_own_initialize_error_when_it_exits_afterwards() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = crate::test_lsp::sh_script_init_config(
            dir.path(),
            &crate::test_lsp::with_read_preamble(
                r#"body='{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"rejected by server"}}'
printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
"#,
            ),
        );

        let err = LspServer::spawn(config).await.unwrap_err();

        assert_matches!(&err, Error::LspInitFailed { message, .. } if message.contains("rejected by server"),
            "got {err:?}"
        );
    }

    /// Connection lost while the child is still running: the error keeps the
    /// "initialize failed" context rather than a bare transport error.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawn_connection_loss_with_live_child_is_lsp_init_failed() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = crate::test_lsp::sh_script_init_config(dir.path(), "exec 1>&-\nsleep 5\n");

        let err = LspServer::spawn(config).await.unwrap_err();

        assert_matches!(&err, Error::LspInitFailed { message, .. } if message.contains("Initialize request failed"),
            "got {err:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawn_exiting_before_initialize_reply_is_server_exited_during_init() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = crate::test_lsp::sh_script_init_config(dir.path(), "exit 1\n");

        let err = LspServer::spawn(config).await.unwrap_err();

        assert_matches!(
            err,
            Error::ServerExitedDuringInit {
                exit_code: Some(1),
                ..
            },
            "got {err:?}"
        );
    }

    /// #534: what the server printed before exiting reaches the error.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawn_exit_during_init_carries_stderr() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = crate::test_lsp::sh_script_init_config(
            dir.path(),
            "echo 'fatal: bad toolchain' >&2\nexit 1\n",
        );

        let err = LspServer::spawn(config).await.unwrap_err();

        let Error::ServerExitedDuringInit {
            stderr: Some(stderr),
            ..
        } = &err
        else {
            panic!("got {err:?}");
        };
        assert_eq!(stderr.head(), "fatal: bad toolchain");
        assert!(err.to_string().contains("stderr: fatal: bad toolchain"));
    }

    /// A server that rejects `initialize` and prints its reason just before
    /// exiting must not lose that last line.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawn_rejection_keeps_stderr_written_just_before_exit() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = crate::test_lsp::sh_script_init_config(
            dir.path(),
            &crate::test_lsp::with_read_preamble(
                r#"body='{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"rejected by server"}}'
printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
sleep 0.02
echo 'fatal: bad toolchain' >&2
"#,
            ),
        );

        let err = LspServer::spawn(config).await.unwrap_err();

        let Error::LspInitFailed {
            stderr: Some(stderr),
            ..
        } = &err
        else {
            panic!("got {err:?}");
        };
        assert_eq!(stderr.head(), "fatal: bad toolchain");
    }

    /// #534: a server that hangs after writing to stderr times out with its
    /// output attached.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawn_initialize_timeout_carries_stderr() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut config = crate::test_lsp::sh_script_init_config(
            dir.path(),
            "echo 'indexing forever' >&2\nsleep 5\n",
        );
        config.server_config.timeout_seconds = 1;

        let err = LspServer::spawn(config).await.unwrap_err();

        let Error::LspInitFailed {
            stderr: Some(stderr),
            ..
        } = &err
        else {
            panic!("got {err:?}");
        };
        assert_eq!(stderr.head(), "indexing forever");
    }

    /// Values configured in `env` never reach the error text.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawn_stderr_redacts_configured_env_values() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut config = crate::test_lsp::sh_script_init_config(
            dir.path(),
            "echo \"token=$API_TOKEN toolchain=$RUSTUP_TOOLCHAIN\" >&2\nexit 1\n",
        );
        config.server_config.env.insert(
            "RUSTUP_TOOLCHAIN".to_string(),
            "nightly-2024-01-01".to_string(),
        );
        config
            .server_config
            .env
            .insert("API_TOKEN".to_string(), "s3cr3t-value".to_string());

        let err = LspServer::spawn(config).await.unwrap_err();

        let text = err.to_string();
        assert!(text.contains("token=[redacted:API_TOKEN]"), "{text}");
        assert!(text.contains("toolchain=nightly-2024-01-01"), "{text}");
        assert!(!text.contains("s3cr3t-value"), "{text}");
    }

    /// `window/logMessage` and `window/showMessage` text echoing configured
    /// secrets is redacted before it reaches the notification lane (#554).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawn_redacts_secrets_in_server_log_and_show_messages() {
        let dir = tempfile::TempDir::new().unwrap();
        let script = r#"body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
body="{\"jsonrpc\":\"2.0\",\"method\":\"window/logMessage\",\"params\":{\"type\":3,\"message\":\"env=$API_TOKEN arg=$1\"}}"
printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
body="{\"jsonrpc\":\"2.0\",\"method\":\"window/showMessage\",\"params\":{\"type\":3,\"message\":\"env=$API_TOKEN\"}}"
printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
sleep 5
"#;
        let mut config = crate::test_lsp::sh_script_init_config(
            dir.path(),
            &crate::test_lsp::with_read_preamble(script),
        );
        config
            .server_config
            .args
            .push("--api-key=SuperSecretArg456".to_string());
        config
            .server_config
            .env
            .insert("API_TOKEN".to_string(), "SuperSecretValue123".to_string());

        let mut server = LspServer::spawn(config).await.unwrap();
        let mut rx = server.take_notification_rx();
        let mut texts = Vec::new();
        for _ in 0..2 {
            let notification = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            texts.push(format!("{notification:?}"));
        }

        let joined = texts.join("\n");
        assert!(!joined.contains("SuperSecretValue123"), "{joined}");
        assert!(!joined.contains("SuperSecretArg456"), "{joined}");
        assert!(joined.contains("[redacted:API_TOKEN]"), "{joined}");
        assert!(joined.contains("[redacted:api-key]"), "{joined}");
    }

    /// The server's own `initialize` error text is redacted (#554).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawn_redacts_secrets_in_initialize_error_message() {
        let dir = tempfile::TempDir::new().unwrap();
        let script = r#"body="{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32603,\"message\":\"bad token $API_TOKEN\"}}"
printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
sleep 5
"#;
        let mut config = crate::test_lsp::sh_script_init_config(
            dir.path(),
            &crate::test_lsp::with_read_preamble(script),
        );
        config
            .server_config
            .env
            .insert("API_TOKEN".to_string(), "SuperSecretValue123".to_string());

        let err = LspServer::spawn(config).await.unwrap_err();

        let text = err.to_string();
        assert!(text.contains("[redacted:API_TOKEN]"), "{text}");
        assert!(!text.contains("SuperSecretValue123"), "{text}");
    }

    /// Regression guard for the drain: a server flooding stderr both before
    /// and after it answers `initialize` must never block or lose its pipe
    /// (`EPIPE`/`SIGPIPE`) once `spawn` has returned.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawn_keeps_draining_stderr_after_initialize() {
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("flooded");
        let script = format!(
            r#"head -c 200000 /dev/zero | tr '\0' 'e' >&2
body='{{"jsonrpc":"2.0","id":1,"result":{{"capabilities":{{}}}}}}'
printf 'Content-Length: %d\r\n\r\n%s' ${{#body}} "$body"
sleep 0.3
head -c 200000 /dev/zero | tr '\0' 'e' >&2 && echo done > '{}'
sleep 5
"#,
            marker.display()
        );
        let config = crate::test_lsp::sh_script_init_config(
            dir.path(),
            &crate::test_lsp::with_read_preamble(&script),
        );

        let mut server = LspServer::spawn(config).await.unwrap();

        let flooded = tokio::time::timeout(Duration::from_secs(5), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            flooded.is_ok(),
            "server's post-initialize stderr flood did not complete"
        );
        assert!(!server.has_exited().unwrap());
    }

    #[tokio::test]
    async fn test_spawn_batch_all_invalid_configs() {
        let configs = vec![
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "rust".to_string(),
                    command: "nonexistent-rust-analyzer".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec!["**/*.rs".to_string()],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                    indexing: crate::bridge::IndexingPolicy::Auto,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "python".to_string(),
                    command: "nonexistent-pyright".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec!["**/*.py".to_string()],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                    indexing: crate::bridge::IndexingPolicy::Auto,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "typescript".to_string(),
                    command: "nonexistent-tsserver".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec!["**/*.ts".to_string()],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                    indexing: crate::bridge::IndexingPolicy::Auto,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
        ];

        let result = LspServer::spawn_batch(&configs).await;

        assert!(!result.has_servers());
        assert!(result.all_failed());
        assert!(!result.partial_success());
        assert_eq!(result.server_count(), 0);
        assert_eq!(result.failure_count(), 3);

        let failure_languages: Vec<_> = result
            .failures
            .iter()
            .map(|f| f.language_id.as_str())
            .collect();
        assert!(failure_languages.contains(&"rust"));
        assert!(failure_languages.contains(&"python"));
        assert!(failure_languages.contains(&"typescript"));
    }

    #[tokio::test]
    async fn test_spawn_batch_multiple_invalid_configs_ordering() {
        let configs = vec![
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "lang1".to_string(),
                    command: "cmd1-nonexistent".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                    indexing: crate::bridge::IndexingPolicy::Auto,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "lang2".to_string(),
                    command: "cmd2-nonexistent".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                    indexing: crate::bridge::IndexingPolicy::Auto,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
        ];

        let result = LspServer::spawn_batch(&configs).await;

        assert_eq!(result.failure_count(), 2);

        assert_eq!(result.failures[0].language_id, "lang1");
        assert_eq!(result.failures[0].command, "cmd1-nonexistent");

        assert_eq!(result.failures[1].language_id, "lang2");
        assert_eq!(result.failures[1].command, "cmd2-nonexistent");
    }

    /// Wire-level regressions for the `initialize` request. The tests
    /// capture the real bytes `LspServer::initialize` writes over an
    /// in-memory duplex pipe standing in for the LSP server. Mirrors the
    /// `fake_lsp_client`/`FakeServer` pattern in
    /// `client.rs::tests::retry_behavior`.
    mod initialize_wire {
        use tempfile::TempDir;
        use tokio::io::BufReader;

        use super::*;
        use crate::test_lsp::{
            fake_lsp_client, read_framed_message, write_response as write_success_response,
        };

        #[tokio::test]
        async fn test_initialize_sends_configured_position_encodings() {
            let (client, mut server) = fake_lsp_client();

            let config = ServerInitConfig {
                server_config: LspServerConfig::rust_analyzer(),
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-32".to_string(), "utf-8".to_string()],
                notification_tx: None,
            };

            let init_task =
                tokio::spawn(async move { LspServer::initialize(&client, &config).await });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;

            assert_eq!(request["method"], "initialize");
            assert_eq!(
                request["params"]["capabilities"]["general"]["positionEncodings"],
                serde_json::json!(["utf-32", "utf-8"]),
                "initialize request must carry the configured encoding order, not the \
                 hardcoded [UTF8, UTF16] default"
            );

            write_success_response(
                &mut server.read_half_stdin,
                &request["id"].clone(),
                serde_json::json!({ "capabilities": {} }),
            )
            .await;

            // The response written above must let `initialize` complete successfully.
            init_task.await.unwrap().unwrap();
        }

        #[tokio::test]
        async fn test_initialize_advertises_stale_request_support() {
            let (client, mut server) = fake_lsp_client();

            let config = ServerInitConfig {
                server_config: LspServerConfig::rust_analyzer(),
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            };

            let init_task =
                tokio::spawn(async move { LspServer::initialize(&client, &config).await });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;
            let params: InitializeParams =
                serde_json::from_value(request["params"].clone()).unwrap();
            let stale_request_support = params
                .capabilities
                .general
                .unwrap()
                .stale_request_support
                .unwrap();

            assert_eq!(request["method"], "initialize");
            assert!(
                !stale_request_support.cancel,
                "mcpls does not implement active in-flight request cancellation"
            );
            assert_eq!(
                stale_request_support.retry_on_content_modified,
                CONTENT_MODIFIED_RETRY_METHODS
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                "the wire-advertised capability must match the methods LspClient::request \
                 actually retries -32801 for, not drift from it"
            );

            write_success_response(
                &mut server.read_half_stdin,
                &request["id"].clone(),
                serde_json::json!({ "capabilities": {} }),
            )
            .await;

            init_task.await.unwrap().unwrap();
        }

        #[tokio::test]
        async fn test_initialize_advertises_server_status_notification_support() {
            let (client, mut server) = fake_lsp_client();

            let config = ServerInitConfig {
                server_config: LspServerConfig::rust_analyzer(),
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            };

            let init_task =
                tokio::spawn(async move { LspServer::initialize(&client, &config).await });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;

            assert_eq!(request["method"], "initialize");
            assert_eq!(
                request["params"]["capabilities"]["experimental"]["serverStatusNotification"],
                serde_json::json!(true),
                "without this, rust-analyzer never emits experimental/serverStatus and the \
                 indexing-readiness gate in Translator::wait_for_indexing_ready is a \
                 permanent no-op"
            );

            write_success_response(
                &mut server.read_half_stdin,
                &request["id"].clone(),
                serde_json::json!({ "capabilities": {} }),
            )
            .await;

            init_task.await.unwrap().unwrap();
        }

        #[tokio::test]
        async fn test_initialize_advertises_hierarchical_document_symbols() {
            let (client, mut server) = fake_lsp_client();

            let config = ServerInitConfig {
                server_config: LspServerConfig::rust_analyzer(),
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            };

            let init_task =
                tokio::spawn(async move { LspServer::initialize(&client, &config).await });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;
            let params: InitializeParams =
                serde_json::from_value(request["params"].clone()).unwrap();
            let document_symbol = params
                .capabilities
                .text_document
                .unwrap()
                .document_symbol
                .unwrap();

            assert_eq!(request["method"], "initialize");
            assert_eq!(document_symbol.dynamic_registration, Some(false));
            assert_eq!(
                document_symbol.hierarchical_document_symbol_support,
                Some(true)
            );
            assert_eq!(
                document_symbol.symbol_kind.unwrap().value_set,
                Some(SUPPORTED_SYMBOL_KINDS.to_vec())
            );

            write_success_response(
                &mut server.read_half_stdin,
                &request["id"].clone(),
                serde_json::json!({ "capabilities": {} }),
            )
            .await;

            init_task.await.unwrap().unwrap();
        }

        #[tokio::test]
        async fn test_initialize_accepts_resolved_dot_workspace_root() {
            let temp_dir = TempDir::new().unwrap();
            let base = dunce::canonicalize(temp_dir.path()).unwrap();
            let workspace_roots =
                crate::resolve_workspace_roots(&[PathBuf::from(".")], &base).unwrap();
            assert_eq!(workspace_roots, vec![base.clone()]);

            let (client, mut server) = fake_lsp_client();
            let config = ServerInitConfig {
                server_config: LspServerConfig::rust_analyzer(),
                workspace_roots,
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            };

            let init_task =
                tokio::spawn(async move { LspServer::initialize(&client, &config).await });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;
            let expected_uri = try_path_to_uri(&base).unwrap();
            assert_eq!(
                request["params"]["workspaceFolders"][0]["uri"],
                expected_uri.as_ref()
            );

            write_success_response(
                &mut server.read_half_stdin,
                &request["id"].clone(),
                serde_json::json!({ "capabilities": {} }),
            )
            .await;

            init_task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn test_spawn_batch_logs_each_failure() {
        let configs = vec![
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "test1".to_string(),
                    command: "nonexistent-test1".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                    indexing: crate::bridge::IndexingPolicy::Auto,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: "test2".to_string(),
                    command: "nonexistent-test2".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                    indexing: crate::bridge::IndexingPolicy::Auto,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            },
        ];

        let result = LspServer::spawn_batch(&configs).await;

        assert_eq!(result.failure_count(), 2);
        assert_eq!(result.failures[0].language_id, "test1");
        assert_eq!(result.failures[1].language_id, "test2");
    }

    /// Minimal [`LspServerConfig`] for `build_command` tests, where only
    /// `command`/`args`/`env` matter.
    fn bare_server_config(env: HashMap<String, String>) -> LspServerConfig {
        LspServerConfig {
            language_id: "test".to_string(),
            command: "irrelevant-for-build-command".to_string(),
            args: vec![],
            env,
            file_patterns: vec![],
            initialization_options: None,
            timeout_seconds: 5,
            request_timeout_seconds: 5,
            heuristics: None,
            name: None,
            handles: None,
            indexing: crate::bridge::IndexingPolicy::Auto,
        }
    }

    /// Collects the env vars a `Command` would set, resolving `env_clear`
    /// removals (`None` values from `get_envs`) away so the map reflects
    /// what the child process would actually see.
    fn effective_envs(command: &Command) -> HashMap<String, String> {
        command
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    /// Regression test for #236/#246: a spawned LSP server used to inherit
    /// mcpls's entire environment. `build_command` must only pass through
    /// `ENV_PASSTHROUGH` keys from `parent_env`, not arbitrary ones.
    #[test]
    fn test_build_command_excludes_non_allowlisted_parent_env_vars() {
        let config = bare_server_config(HashMap::new());
        let command = LspServer::build_command(&config, |key| match key {
            "PATH" => Some("/parent/bin".into()),
            "MCPLS_TEST_LEAK_CANARY" => Some("should-not-reach-child".into()),
            _ => None,
        });

        let envs = effective_envs(&command);

        assert!(
            !envs.contains_key("MCPLS_TEST_LEAK_CANARY"),
            "non-allowlisted parent env var leaked into child command: {envs:?}"
        );

        // The assertion above is provably vacuous on its own:
        // `Command::get_envs()` only reports explicit `.env()`/`.envs()`
        // modifications and is blind to whether `.env_clear()` was called,
        // and `build_command`'s passthrough loop never even queries
        // `parent_env` for a key outside `ENV_PASSTHROUGH`, so it would
        // pass unchanged even if `.env_clear()` were deleted from
        // `build_command` entirely. `std::process::Command`'s `Debug` impl
        // does encode clearing, prefixing the formatted command with
        // `env -i ` on Unix once `.env_clear()` has run; assert on that to
        // actually guard against the clear being removed.
        #[cfg(unix)]
        assert!(
            format!("{:?}", command.as_std()).starts_with("env -i "),
            "build_command must call .env_clear() so the child doesn't inherit the full parent environment"
        );
    }

    /// Regression test for #236/#246: allowlisted vars present in the parent
    /// (e.g. `PATH`) must still reach the child.
    #[test]
    fn test_build_command_passes_through_allowlisted_env_vars() {
        let config = bare_server_config(HashMap::new());
        let command =
            LspServer::build_command(&config, |key| (key == "PATH").then(|| "/parent/bin".into()));

        let envs = effective_envs(&command);

        assert_eq!(envs.get("PATH"), Some(&"/parent/bin".to_string()));
    }

    /// Regression test for #247: `LspServerConfig::env` entries must reach
    /// the spawned child (previously dead configuration).
    #[test]
    fn test_build_command_includes_configured_env_vars() {
        let mut env = HashMap::new();
        env.insert(
            "MCPLS_TEST_CONFIGURED".to_string(),
            "from-server-config".to_string(),
        );
        let config = bare_server_config(env);
        let command = LspServer::build_command(&config, |_| None);

        let envs = effective_envs(&command);

        assert_eq!(
            envs.get("MCPLS_TEST_CONFIGURED"),
            Some(&"from-server-config".to_string())
        );
    }

    /// Regression test for #247: a `LspServerConfig::env` entry must be able
    /// to override an allowlisted passthrough value, since `config.env` is
    /// applied after the passthrough loop in `build_command`.
    #[test]
    fn test_build_command_configured_env_overrides_allowlisted_var() {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/configured/override/path".to_string());
        let config = bare_server_config(env);
        let command =
            LspServer::build_command(&config, |key| (key == "PATH").then(|| "/parent/bin".into()));

        let envs = effective_envs(&command);

        assert_eq!(
            envs.get("PATH"),
            Some(&"/configured/override/path".to_string())
        );
    }

    /// #174 §8/S2 regression: `register_servers`'s diagnostics-cache flags
    /// must be computed from the *rebound* router, not the pre-rebind view.
    /// Sets up a `python` config where a narrow "diagnostics-only" server
    /// (`pyright-diag`) is configured but never actually registers (as if
    /// it failed to spawn), leaving only a catch-all (`pylsp`) live. Before
    /// the fix, computing the flags from the pre-rebind router would resolve
    /// `Diagnostics` to the dead `pyright-diag` for every survivor, so
    /// `pylsp` would be flagged `false` and the diagnostics cache would go
    /// silently dark for `python` despite a live server being available.
    #[tokio::test]
    async fn test_register_servers_computes_diagnostics_flags_from_rebound_router() {
        use crate::bridge::Translator;
        use crate::config::{ServerId, ToolKind, ToolRouter};

        let pylsp_id = ServerId::from("pylsp");
        let configs = vec![
            LspServerConfig {
                language_id: "python".to_string(),
                command: "pyright-langserver".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                timeout_seconds: 30,
                request_timeout_seconds: 30,
                heuristics: None,
                name: Some("pyright-diag".to_string()),
                handles: Some(vec![ToolKind::Diagnostics]),
                indexing: crate::bridge::IndexingPolicy::Auto,
            },
            LspServerConfig {
                language_id: "python".to_string(),
                command: "pylsp".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                timeout_seconds: 30,
                request_timeout_seconds: 30,
                heuristics: None,
                name: Some("pylsp".to_string()),
                handles: None,
                indexing: crate::bridge::IndexingPolicy::Auto,
            },
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();
        let translator = Translator::new().with_router(router);

        // Only pylsp actually registers; pyright-diag never spawned.
        let mut result = ServerInitResult::new();
        result.add_server(
            pylsp_id.clone(),
            fake_lsp_server_with_config(configs[1].clone()),
        );

        let registered = crate::register_servers(result, &translator);

        assert_eq!(
            registered.diagnostics_flags.get(&pylsp_id),
            Some(&true),
            "pylsp must inherit the diagnostics route once pyright-diag is \
             known dead, and the flag must reflect that post-rebind state"
        );
    }

    /// The indexing policy comes from the server's own `init_config`.
    #[tokio::test]
    async fn test_register_servers_reports_indexing_policy_from_init_config() {
        use crate::bridge::{IndexingPolicy, Translator};

        let mut config = LspServerConfig::rust_analyzer();
        config.indexing = IndexingPolicy::Disabled;
        let id = config.id();

        let mut result = ServerInitResult::new();
        result.add_server(id.clone(), fake_lsp_server_with_config(config));

        let registered = crate::register_servers(result, &Translator::new());

        assert_eq!(
            registered.indexing_policies.get(&id),
            Some(&IndexingPolicy::Disabled)
        );
    }
}
