//! LSP server lifecycle management.
//!
//! This module handles the complete lifecycle of an LSP server:
//! 1. Spawn server process
//! 2. Initialize → initialized handshake
//! 3. Capability negotiation
//! 4. Active request handling
//! 5. Graceful shutdown sequence

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use futures::FutureExt as _;
use lsp_types::{
    ClientCapabilities, ClientInfo, DidChangeConfigurationNotification,
    DidChangeConfigurationParams, ExitNotification, GeneralClientCapabilities, InitializeParams,
    InitializeRequest, InitializeResult, InitializedNotification, InitializedParams,
    PositionEncodingKind, Request, ServerCapabilities, ShutdownRequest, StaleRequestSupportOptions,
    SymbolKind, WorkspaceFolder,
};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, timeout, timeout_at};
use tracing::{debug, info, warn};

use crate::bridge::try_path_to_uri;
use crate::config::LspServerConfig;
use crate::error::{Error, Result, ServerSpawnFailure, StartupFailure};
use crate::lsp::CONTENT_MODIFIED_RETRY_METHODS;
use crate::lsp::client::{LspClient, SHUTDOWN_TIMEOUT};
#[cfg(unix)]
use crate::lsp::process::Binding;
use crate::lsp::process::{MarkOutcome, ServerProcess};
use crate::lsp::stderr::{EofWait, StderrCapture};
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
/// killing its process tree. Only the leader is awaited here.
const CHILD_EXIT_GRACE: Duration = Duration::from_secs(3);

/// Least time the final process-tree sweep may take even when the shutdown
/// deadline is already spent, so a whole shutdown stays within
/// `SHUTDOWN_TIMEOUT` plus this.
const MIN_SWEEP_BUDGET: Duration = Duration::from_secs(3);

/// How long [`LspServer::terminate`] lets the leader process exit on its own
/// after the `shutdown` handshake, before the process tree is killed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitGrace {
    /// Up to the rest of the handshake budget, at most [`CHILD_EXIT_GRACE`]
    /// (normal shutdown).
    WithinBudget,
    /// A server that answered `shutdown` may take this long to exit, since a
    /// healthy server can be slow to flush; one that did not answer is killed
    /// at once.
    IfAnswered(Duration),
}

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
    /// is ever built, but `LspServer::spawn` is `pub` and
    /// reachable directly by a library embedder bypassing that validation
    /// entirely (same reasoning as the `initialize` timeout clamp below), so
    /// this can't assume the value was already checked. If nothing parses,
    /// falls back to `config::default_position_encodings()`'s default.
    pub position_encodings: Vec<String>,
}

/// The terminal outcome of starting one configured server.
///
/// A closed type so a settled server is either running or has a recorded
/// failure, never both and never neither.
#[derive(Debug)]
pub enum ServerStartOutcome {
    /// The server initialized and is ready to register.
    Started(Box<LspServer>),
    /// The server failed to start.
    Failed(ServerSpawnFailure),
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

/// Resolve one environment variable as the spawned server would see it:
/// the server config's `env` override wins, otherwise the parent value.
///
/// `parent_env` is injected so callers and tests need not touch the real
/// process environment.
pub fn child_env_var(
    config: &LspServerConfig,
    key: &str,
    parent_env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<std::ffi::OsString> {
    config
        .env
        .get(key)
        .map(std::ffi::OsString::from)
        .or_else(|| parent_env(key))
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
    /// 5. Sends `workspace/didChangeConfiguration` with null settings, which
    ///    servers that wait for a configuration push (pyright) need before
    ///    they answer requests
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Server process fails to spawn
    /// - Initialize request fails or times out
    /// - Server returns error during initialization
    /// - The `initialized` or `workspace/didChangeConfiguration` notification
    ///   cannot be written ([`Error::LspInitFailed`])
    pub async fn spawn(config: ServerInitConfig) -> Result<Self> {
        let redactions = Arc::new(Redactions::for_server(
            &config.server_config,
            std::env::vars_os().filter_map(|(name, value)| {
                Some((name.into_string().ok()?, value.into_string().ok()?))
            }),
        ));
        Self::log_spawn(&config.server_config, &redactions);

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
        #[cfg(unix)]
        let process_id = initialize_process_id(child.binding());
        #[cfg(windows)]
        let process_id = Some(mcpls_process_id());
        let (capabilities, position_encoding) =
            match Self::initialize_as(&client, &config, process_id).await {
                Ok(negotiated) => negotiated,
                Err(init_error) if is_connection_loss(&init_error) => {
                    let exit_status = early_exit_status(&mut child).await;
                    let eof_wait = if exit_status.is_some() {
                        EofWait::Grace
                    } else {
                        EofWait::Skip
                    };
                    let stderr = stderr_capture.finish(eof_wait, &redactions).await;
                    return Err(match exit_status {
                        Some(status) => Error::ServerExitedDuringInit {
                            command: config.server_config.command.clone(),
                            exit_code: status.code(),
                            stderr,
                        },
                        None => Error::LspInitFailed {
                            message: redactions
                                .apply(&format!("Initialize request failed: {init_error}"))
                                .into_owned(),
                            stderr,
                        },
                    });
                }
                Err(Error::LspInitFailed { message, .. }) => {
                    // The server may be about to exit after printing its reason,
                    // so wait the (bounded) end-of-file grace whether or not it
                    // has exited yet.
                    let stderr = stderr_capture.finish(EofWait::Grace, &redactions).await;
                    let message = redactions.apply(&message).into_owned();
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

    /// Logs the command and argument count at `info`, and the argument values
    /// (redacted) only at `debug`.
    fn log_spawn(server_config: &LspServerConfig, redactions: &Redactions) {
        info!(
            "Spawning LSP server: {} ({} arg(s))",
            server_config.command,
            server_config.args.len()
        );
        debug!(
            "LSP server args: {:?}",
            server_config
                .args
                .iter()
                .map(|arg| redactions.apply(arg))
                .collect::<Vec<_>>()
        );
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
            if let Some(value) = child_env_var(config, key, &parent_env) {
                command.env(key, value);
            }
        }
        #[cfg(windows)]
        for key in ENV_PASSTHROUGH_WINDOWS {
            if let Some(value) = child_env_var(config, key, &parent_env) {
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
                declaration: Some(lsp_types::DeclarationClientCapabilities {
                    dynamic_registration: Some(false),
                    link_support: Some(true),
                }),
                references: Some(lsp_types::ReferenceClientCapabilities {
                    dynamic_registration: Some(false),
                }),
                // clangd and typescript-language-server answer `prepareProvider` only
                // when the client advertises `prepareSupport`.
                rename: Some(lsp_types::RenameClientCapabilities {
                    dynamic_registration: Some(false),
                    prepare_support: Some(true),
                    prepare_support_default_behavior: Some(
                        lsp_types::PrepareSupportDefaultBehavior::Identifier,
                    ),
                    ..Default::default()
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
    /// Sends initialize request and waits for response, then sends the
    /// initialized and `workspace/didChangeConfiguration` notifications.
    ///
    /// The settings are null rather than `{}`: some servers rebuild their
    /// preferences from an empty settings map and so drop their
    /// `initialization_options`, while a non-map value is ignored.
    #[cfg(test)]
    async fn initialize(
        client: &LspClient,
        config: &ServerInitConfig,
    ) -> Result<(ServerCapabilities, PositionEncodingKind)> {
        Self::initialize_as(client, config, Some(mcpls_process_id())).await
    }

    /// As [`Self::initialize`], reporting `process_id` as the parent process
    /// the server may watch (`None` is allowed by LSP and sent while a
    /// lifeline watchdog is bound: a server that exits when mcpls vanishes
    /// would otherwise race the watchdog's freeze of its descendants).
    async fn initialize_as(
        client: &LspClient,
        config: &ServerInitConfig,
        process_id: Option<i32>,
    ) -> Result<(ServerCapabilities, PositionEncodingKind)> {
        debug!("Sending initialize request");

        let workspace_folders: Vec<WorkspaceFolder> = config
            .workspace_roots
            .iter()
            .map(|root| workspace_folder(root))
            .collect::<Result<Vec<_>>>()?;

        let params = InitializeParams {
            process_id,
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
                // through `LspServer::spawn`, which bypasses that
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

        notify_handshake::<InitializedNotification>(client, InitializedParams {}).await?;
        // TODO(#598): push the configured per-server settings instead of null
        notify_handshake::<DidChangeConfigurationNotification>(
            client,
            DidChangeConfigurationParams {
                settings: serde_json::Value::Null,
            },
        )
        .await?;

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
    /// Sends the LSP `shutdown` request, waits for the response, has the
    /// lifeline watchdog freeze and record descendants that escaped the
    /// server's process group (Unix), sends the `exit` notification, stops the
    /// message loop, then waits up to a grace period (never past the overall
    /// deadline) for the leader process to exit on its own. Whatever is left,
    /// including escapees that left the group through `setsid`/`setpgid` and
    /// shared daemons such as Gradle or Bloop, is then killed and waited for,
    /// up to [`crate::lsp::LIFELINE_SWEEP_BUDGET`] beyond the deadline, so
    /// when this returns the server's whole tree is gone. If the escapee mark
    /// fails the `exit` notification is skipped and the tree is killed right
    /// away. A test fixture with no real backing process skips the process
    /// steps.
    ///
    /// # Errors
    ///
    /// Returns the first error of the handshake or the message-loop stop,
    /// including [`Error::ShutdownTimeout`] when the deadline elapsed. The
    /// process tree is still torn down regardless of whether this returns
    /// `Ok` or `Err`.
    pub async fn shutdown(mut self) -> Result<()> {
        self.terminate(SHUTDOWN_TIMEOUT, ExitGrace::WithinBudget)
            .await
    }

    /// Stop the server and its whole process tree, leaving `self` usable as a
    /// dead server (`is_dead()` is `true` for a real process).
    ///
    /// Sends the LSP `shutdown` request, waits for the response, sends the
    /// `exit` notification and stops the message loop, all within `budget`.
    /// It then lets the leader process exit on its own for as long as `grace`
    /// allows, which is the only path that lets the server reap its own
    /// children. Whatever is left in the server's process group is then killed
    /// (on Unix this includes shared daemons such as Gradle or Bloop started
    /// by the server). A test fixture with no real backing process (`child`
    /// is `None`) skips the process steps.
    ///
    /// # Errors
    ///
    /// Returns the first error of the handshake or the message-loop stop,
    /// including [`Error::ShutdownTimeout`] when `budget` elapsed. The
    /// process tree is still torn down regardless of whether this returns
    /// `Ok` or `Err`.
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "SHUTDOWN_TIMEOUT and CHILD_EXIT_GRACE are small constants"
    )]
    pub(crate) async fn terminate(&mut self, budget: Duration, grace: ExitGrace) -> Result<()> {
        debug!("Shutting down LSP server");

        let deadline = Instant::now() + budget;
        let client = &mut self.client;
        let shutdown_request_timeout = Duration::from_secs(5).min(budget);

        let handshake: Result<MarkOutcome> = timeout_at(deadline, async {
            let _: serde_json::Value = client
                .request(
                    ShutdownRequest::METHOD.as_str(),
                    (),
                    shutdown_request_timeout,
                )
                .await?;
            #[cfg(unix)]
            let mark = match self.child.as_mut() {
                Some(child) => child.mark_escapees().await,
                None => MarkOutcome::Confirmed,
            };
            #[cfg(windows)]
            let mark = MarkOutcome::Confirmed;
            if mark == MarkOutcome::Confirmed {
                client.notify_typed::<ExitNotification>(()).await?;
            }
            Ok(mark)
        })
        .await
        .unwrap_or(Err(Error::ShutdownTimeout));
        let mark = handshake
            .as_ref()
            .map_or(MarkOutcome::Confirmed, |mark| *mark);
        let handshake = handshake.map(drop);
        let handshake = match client.shutdown_until(deadline).await {
            Ok(()) => handshake,
            Err(e) => handshake.and(Err(e)),
        };

        if let Some(child) = &mut self.child {
            let child_deadline = match grace {
                _ if mark != MarkOutcome::Confirmed => Instant::now(),
                ExitGrace::WithinBudget => deadline.min(Instant::now() + CHILD_EXIT_GRACE),
                ExitGrace::IfAnswered(grace) if handshake.is_ok() => Instant::now() + grace,
                ExitGrace::IfAnswered(_) => Instant::now(),
            };
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
            let sweep_budget = deadline
                .saturating_duration_since(Instant::now())
                .clamp(MIN_SWEEP_BUDGET, crate::lsp::LIFELINE_SWEEP_BUDGET);
            child.terminate_tree(sweep_budget).await;
        }

        handshake?;
        info!("LSP server shut down successfully");
        Ok(())
    }

    /// Spawns and initializes one server, turning an error or a panic into
    /// that server's [`ServerStartOutcome::Failed`] so it can never affect a
    /// sibling. Logs the outcome with the id and elapsed time.
    ///
    /// Runs on the caller's task: dropping the future drops the child
    /// process it owns.
    pub(crate) async fn start_contained(config: &ServerInitConfig) -> ServerStartOutcome {
        Box::pin(contain(config, Self::spawn(config.clone()))).await
    }
}

/// Drives `start` (one server's spawn and handshake) and classifies how it
/// ended; see [`LspServer::start_contained`].
async fn contain(
    config: &ServerInitConfig,
    start: impl std::future::Future<Output = Result<LspServer>>,
) -> ServerStartOutcome {
    let server_id = config.server_config.id();
    let language_id = config.server_config.language_id.clone();
    let command = config.server_config.command.clone();
    let started = Instant::now();

    let reason = match AssertUnwindSafe(start).catch_unwind().await {
        Ok(Ok(server)) => {
            info!(
                "Successfully spawned LSP server: {} ({}) in {:?}",
                server_id,
                command,
                started.elapsed()
            );
            return ServerStartOutcome::Started(Box::new(server));
        }
        Ok(Err(e)) => {
            tracing::error!(
                "Failed to spawn LSP server: {} ({}) after {:?}: {}",
                server_id,
                command,
                started.elapsed(),
                e
            );
            StartupFailure::Spawn(Arc::new(e))
        }
        Err(payload) => {
            tracing::error!(
                "Starting LSP server {} ({}) panicked after {:?}: {}",
                server_id,
                command,
                started.elapsed(),
                crate::panic_message(payload.as_ref())
            );
            StartupFailure::InitTaskPanicked
        }
    };
    ServerStartOutcome::Failed(ServerSpawnFailure {
        server_id,
        language_id,
        command,
        reason,
    })
}

fn mcpls_process_id() -> i32 {
    i32::try_from(std::process::id()).unwrap_or(i32::MAX)
}

/// The `processId` to send: none while a lifeline watchdog is bound, the real
/// mcpls pid otherwise.
#[cfg(unix)]
fn initialize_process_id(binding: Binding) -> Option<i32> {
    match binding {
        Binding::Bound => None,
        Binding::Unbound => Some(mcpls_process_id()),
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

/// Sends one handshake notification, mapping a failed write to
/// [`Error::LspInitFailed`] naming the notification's method.
async fn notify_handshake<N>(client: &LspClient, params: N::Params) -> Result<()>
where
    N: lsp_types::Notification,
{
    client
        .notify_typed::<N>(params)
        .await
        .map_err(|e| Error::LspInitFailed {
            message: format!("{} notification failed: {e}", N::METHOD.as_str()),
            stderr: None,
        })
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
    use std::collections::HashMap;

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

    /// Without `prepareSupport`, clangd and typescript-language-server never
    /// advertise `prepareProvider`, which would make `prepare_rename`
    /// unreachable on them.
    #[test]
    fn test_client_capabilities_advertises_prepare_rename_support() {
        let rename = LspServer::client_capabilities(&["utf-16".to_string()])
            .text_document
            .and_then(|t| t.rename)
            .unwrap();

        assert_eq!(rename.prepare_support, Some(true));
        assert_eq!(
            rename.prepare_support_default_behavior,
            Some(lsp_types::PrepareSupportDefaultBehavior::Identifier)
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
                settings: None,
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
        };
        let err = LspServer::spawn(config).await.unwrap_err();
        assert_matches!(err, Error::ServerNotFound { .. }, "got {err:?}");
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

    #[cfg(unix)]
    #[test]
    fn test_unbound_server_gets_the_real_process_id() {
        assert_eq!(
            initialize_process_id(Binding::Unbound),
            Some(i32::try_from(std::process::id()).unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_bound_server_gets_no_process_id() {
        assert_eq!(initialize_process_id(Binding::Bound), None);
    }

    /// A server guarded by a lifeline watchdog must not be handed the mcpls
    /// pid to watch: it could exit before the watchdog has frozen its tree.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawn_sends_null_process_id_while_a_lifeline_is_bound() {
        let dir = tempfile::TempDir::new().unwrap();
        let dump = dir.path().join("request");
        let script = format!(
            "read -r header\nread -r _\nhead -c \"$(printf %s \"$header\" | tr -dc 0-9)\" > '{}'\nexit 1\n",
            dump.display()
        );
        let config = crate::test_lsp::sh_script_init_config(dir.path(), &script);

        LspServer::spawn(config).await.unwrap_err();

        let request = std::fs::read_to_string(&dump).unwrap();
        assert!(request.contains(r#""processId":null"#), "got {request}");
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
# Shell-only pause: an external `sleep` can take longer than the 100 ms EOF grace just to start on a loaded macOS runner.
i=0; while [ $i -lt 300 ]; do i=$((i+1)); done
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

    #[test]
    fn test_log_spawn_hides_argument_values() {
        use tracing_subscriber::prelude::*;

        use crate::test_lsp::CapturedLogs;

        let mut config = LspServerConfig::rust_analyzer();
        config.args = vec!["--api-key=SuperSecretArg456".to_string()];
        let redactions = Redactions::for_server(&config, std::iter::empty());
        let logs = CapturedLogs::default();
        let info_only = tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::INFO)
            .with(logs.clone());

        tracing::subscriber::with_default(info_only, || {
            LspServer::log_spawn(&config, &redactions);
        });

        let output = logs.messages().join("\n");
        assert!(output.contains("(1 arg(s))"), "{output}");
        assert!(!output.contains("SuperSecretArg456"), "{output}");
    }

    /// FR-012: a panic while starting one server is that server's failure.
    #[tokio::test]
    async fn contain_attributes_a_panic_to_its_own_server() {
        let config = crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer());

        let outcome = contain(&config, async { panic!("start boom") }).await;

        assert_matches!(
            outcome,
            ServerStartOutcome::Failed(failure)
                if failure.server_id == config.server_config.id()
                    && matches!(failure.reason, StartupFailure::InitTaskPanicked)
        );
    }

    #[tokio::test]
    async fn contain_reports_a_spawn_error_as_that_servers_failure() {
        let config = crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer());

        let outcome = contain(&config, async { Err(Error::ServerTerminated) }).await;

        assert_matches!(
            outcome,
            ServerStartOutcome::Failed(failure) if matches!(failure.reason, StartupFailure::Spawn(_))
        );
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

    /// A secret echoed into a mistyped `initialize` result reaches the serde
    /// error text, which is redacted too (#554).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_spawn_redacts_secrets_in_initialize_decode_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let script = r#"body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":"SuperSecretValue123"}}'
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
        assert!(!text.contains("SuperSecretValue123"), "{text}");
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
        async fn test_initialize_pushes_null_settings_right_after_initialized() {
            let (client, mut server) = fake_lsp_client();
            let config = crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer());
            let init_task =
                tokio::spawn(async move { LspServer::initialize(&client, &config).await });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;
            assert_eq!(request["method"], "initialize");
            write_success_response(
                &mut server.read_half_stdin,
                &request["id"].clone(),
                serde_json::json!({ "capabilities": {} }),
            )
            .await;

            let initialized = read_framed_message(&mut reader).await;
            let configuration = read_framed_message(&mut reader).await;
            init_task.await.unwrap().unwrap();

            assert_eq!(initialized["method"], "initialized");
            assert_eq!(configuration["method"], "workspace/didChangeConfiguration");
            assert_eq!(
                configuration["params"],
                serde_json::json!({ "settings": null })
            );
        }

        #[tokio::test]
        async fn test_initialize_advertises_stale_request_support() {
            let (client, mut server) = fake_lsp_client();

            let config = ServerInitConfig {
                server_config: LspServerConfig::rust_analyzer(),
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
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
                crate::bridge::WorkspaceRoots::from_configured_with(&[PathBuf::from(".")], || {
                    Ok(crate::bridge::ProcessCwd::new(base.clone(), None))
                })
                .unwrap()
                .canonical()
                .to_vec();
            assert_eq!(workspace_roots, vec![base.clone()]);

            let (client, mut server) = fake_lsp_client();
            let config = ServerInitConfig {
                server_config: LspServerConfig::rust_analyzer(),
                workspace_roots,
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
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
            settings: None,
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

    /// #174 section 8 regression: the diagnostics route must be read from the
    /// router as re-derived after the explicit diagnostics server failed, not
    /// from the configured one. A narrow "diagnostics-only" server
    /// (`pyright-diag`) fails to start, leaving only a catch-all (`pylsp`);
    /// resolving `Diagnostics` against the configured router would name the
    /// dead `pyright-diag` and silence diagnostics for `python`.
    #[tokio::test]
    async fn test_settling_dead_diagnostics_server_hands_the_route_to_the_catch_all() {
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
                settings: None,
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
                settings: None,
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
        translator.set_expected_servers(
            [ServerId::from("pyright-diag"), pylsp_id.clone()]
                .into_iter()
                .collect(),
        );

        translator.settle_failed(&ServerSpawnFailure {
            server_id: ServerId::from("pyright-diag"),
            language_id: "python".to_string(),
            command: "pyright-langserver".to_string(),
            reason: StartupFailure::InitTaskPanicked,
        });
        translator.settle_started(fake_lsp_server_with_config(configs[1].clone()));

        assert!(
            translator.is_diagnostics_route("python", &pylsp_id),
            "pylsp must inherit the diagnostics route once pyright-diag is known dead"
        );
    }
}
