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

use lsp_types::{
    ClientCapabilities, ClientInfo, GeneralClientCapabilities, InitializeParams, InitializeResult,
    InitializedParams, PositionEncodingKind, ServerCapabilities, WorkspaceFolder,
};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tracing::{debug, info};

use crate::bridge::try_path_to_uri;
use crate::config::{LspServerConfig, ServerId};
use crate::error::{Error, Result, ServerSpawnFailure};
use crate::lsp::client::LspClient;
use crate::lsp::transport::LspTransport;
use crate::lsp::types::LspNotification;

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
    /// Receiver for push notifications from the LSP server.
    ///
    /// Extract this before registering the server to receive real-time
    /// notifications (e.g., `textDocument/publishDiagnostics`, `$/progress`).
    pub notification_rx: mpsc::Receiver<LspNotification>,
    /// Child process handle. Kept alive for process lifetime management and
    /// queried by [`Self::has_exited`] to detect a crash. When dropped, the
    /// process is terminated via SIGKILL (`kill_on_drop`).
    child: tokio::process::Child,
}

impl std::fmt::Debug for LspServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspServer")
            .field("client", &self.client)
            .field("capabilities", &self.capabilities)
            .field("position_encoding", &self.position_encoding)
            .field("notification_rx", &"<channel>")
            .field("child", &"<process>")
            .finish()
    }
}

impl LspServer {
    /// Take the notification receiver out of this server, replacing it with a dummy channel.
    ///
    /// Use this to extract the receiver for a background pump task before registering
    /// the server with the translator. After this call, the server's `notification_rx`
    /// will never receive messages.
    pub fn take_notification_rx(&mut self) -> tokio::sync::mpsc::Receiver<LspNotification> {
        let (_, dummy) = tokio::sync::mpsc::channel(1);
        std::mem::replace(&mut self.notification_rx, dummy)
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
        info!(
            "Spawning LSP server: {} {:?}",
            config.server_config.command, config.server_config.args
        );

        let mut command = Self::build_command(&config.server_config, |key| std::env::var_os(key));

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
            base + windows
        };
        debug!(
            "Effective LSP server env: {passthrough_present} allowlisted key(s) present, \
             {} configured override(s) applied",
            config.server_config.env.len()
        );

        let mut child = command.spawn().map_err(|e| Error::ServerSpawnFailed {
            command: config.server_config.command.clone(),
            source: e,
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Transport("Failed to capture stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Transport("Failed to capture stdout".to_string()))?;

        let transport = LspTransport::new(stdin, stdout);
        let (notification_tx, notification_rx) = mpsc::channel(64);
        let client = LspClient::from_transport_with_notifications(
            config.server_config.clone(),
            transport,
            notification_tx,
        );

        let (capabilities, position_encoding) = Self::initialize(&client, &config).await?;

        info!("LSP server initialized successfully");

        Ok(Self {
            client,
            capabilities,
            position_encoding,
            notification_rx,
            child,
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
            .stderr(Stdio::null())
            .kill_on_drop(true);

        command
    }

    /// Perform LSP initialization handshake.
    ///
    /// Sends initialize request and waits for response, then sends initialized notification.
    #[allow(clippy::too_many_lines)]
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
            process_id: Some(std::process::id()),
            #[allow(deprecated)]
            root_uri: None,
            initialization_options: config.initialization_options.clone(),
            capabilities: ClientCapabilities {
                general: Some(GeneralClientCapabilities {
                    position_encodings: Some(vec![
                        PositionEncodingKind::UTF8,
                        PositionEncodingKind::UTF16,
                    ]),
                    ..Default::default()
                }),
                text_document: Some(lsp_types::TextDocumentClientCapabilities {
                    hover: Some(lsp_types::HoverClientCapabilities {
                        dynamic_registration: Some(false),
                        content_format: Some(vec![
                            lsp_types::MarkupKind::Markdown,
                            lsp_types::MarkupKind::PlainText,
                        ]),
                    }),
                    definition: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    references: Some(lsp_types::ReferenceClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    code_action: Some(lsp_types::CodeActionClientCapabilities {
                        dynamic_registration: Some(false),
                        data_support: Some(true),
                        resolve_support: Some(lsp_types::CodeActionCapabilityResolveSupport {
                            properties: vec!["edit".to_string()],
                        }),
                        // Declare supported action kinds so the server returns
                        // CodeAction objects (not just legacy Command objects).
                        code_action_literal_support: Some(lsp_types::CodeActionLiteralSupport {
                            code_action_kind: lsp_types::CodeActionKindLiteralSupport {
                                value_set: [
                                    lsp_types::CodeActionKind::EMPTY,
                                    lsp_types::CodeActionKind::QUICKFIX,
                                    lsp_types::CodeActionKind::REFACTOR,
                                    lsp_types::CodeActionKind::REFACTOR_EXTRACT,
                                    lsp_types::CodeActionKind::REFACTOR_INLINE,
                                    lsp_types::CodeActionKind::REFACTOR_REWRITE,
                                    lsp_types::CodeActionKind::SOURCE,
                                    lsp_types::CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                                ]
                                .iter()
                                .map(|k| k.as_str().to_string())
                                .collect(),
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
                ..Default::default()
            },
            client_info: Some(ClientInfo {
                name: "mcpls".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            workspace_folders: Some(workspace_folders),
            ..Default::default()
        };

        // Use the server's configured timeout for the initialize handshake too,
        // not a hardcoded 30s: large solutions (e.g. a 130-project Unity .sln via
        // OmniSharp) take minutes to respond to `initialize`.
        let result: InitializeResult = client
            .request(
                "initialize",
                params,
                Duration::from_secs(config.server_config.timeout_seconds),
            )
            .await
            .map_err(|e| Error::LspInitFailed {
                message: format!("Initialize request failed: {e}"),
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
            .notify("initialized", InitializedParams {})
            .await
            .map_err(|e| Error::LspInitFailed {
                message: format!("Initialized notification failed: {e}"),
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
    /// Uses [`tokio::process::Child::try_wait`], which never blocks waiting
    /// for the process: `true` means it is gone (crashed, killed, or exited
    /// on its own), and any [`LspClient`] obtained from [`Self::client`] is
    /// now permanently disconnected -- new requests through it fail with
    /// [`crate::error::Error::ServerTerminated`]. Callers that want to
    /// recover substitute a freshly [`Self::spawn`]ed replacement.
    ///
    /// # Errors
    ///
    /// Returns an error if the OS fails to report the process's status.
    pub fn has_exited(&mut self) -> Result<bool> {
        Ok(self.child.try_wait()?.is_some())
    }

    /// Shutdown server gracefully.
    ///
    /// Sends shutdown request, waits for response, then sends exit notification.
    ///
    /// # Errors
    ///
    /// Returns an error if shutdown sequence fails.
    pub async fn shutdown(self) -> Result<()> {
        debug!("Shutting down LSP server");

        let _: serde_json::Value = self
            .client
            .request("shutdown", serde_json::Value::Null, Duration::from_secs(5))
            .await?;

        self.client.notify("exit", serde_json::Value::Null).await?;

        self.client.shutdown().await?;

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
    ///         notification_tx: None,
    ///     },
    ///     ServerInitConfig {
    ///         server_config: LspServerConfig::pyright(),
    ///         workspace_roots: vec![PathBuf::from("/workspace")],
    ///         initialization_options: None,
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
                        message: e.to_string(),
                    });
                }
            }
        }

        result
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

#[cfg(test)]
impl LspServer {
    /// Construct an `LspServer` fixture carrying the given capabilities, for
    /// tests elsewhere in the crate that need to drive capability-gated
    /// dispatch paths in `Translator` without spawning a real language server.
    ///
    /// The underlying client and child process are inert placeholders — only
    /// `capabilities()` is meaningful on the returned value.
    ///
    /// Uses `LspClient::new` (uninitialized, no background task) rather than
    /// `LspClient::from_transport`, so this does not depend on the Tokio
    /// message loop — only `child`'s spawn needs a Tokio runtime, i.e. an
    /// async test context (`#[tokio::test]`).
    #[allow(clippy::unwrap_used)]
    pub(crate) fn new_for_test(capabilities: ServerCapabilities) -> Self {
        let child = Command::new("echo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let client = LspClient::new(LspServerConfig::rust_analyzer());
        let (_, notification_rx) = mpsc::channel(1);

        Self {
            client,
            capabilities,
            position_encoding: PositionEncodingKind::UTF16,
            notification_rx,
            child,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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

        assert_eq!(folder.uri.as_str(), expected);
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

        assert_eq!(folder.uri.as_str(), expected);
        assert_eq!(folder.name, "[env]");
    }

    #[test]
    fn test_workspace_folder_rejects_relative_root() {
        let err = workspace_folder(Path::new("relative/root")).unwrap_err();
        assert!(matches!(err, Error::InvalidUri(_)), "got {err:?}");
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
                heuristics: None,
                name: None,
                handles: None,
            },
            workspace_roots: vec![PathBuf::from("/workspace")],
            initialization_options: Some(init_opts),
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
            notification_tx: None,
        };

        assert!(config.workspace_roots.is_empty());
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
            notification_tx: None,
        };

        assert_eq!(config.workspace_roots.len(), 3);
    }

    /// #249: `has_exited` must distinguish a live child from one that has
    /// already exited, since this is the signal the respawn path relies on
    /// to detect a crashed LSP server.
    #[tokio::test]
    async fn test_has_exited_reflects_child_process_state() {
        use lsp_types::ServerCapabilities;

        let mock_child = tokio::process::Command::new("sleep")
            .arg("2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mock_stdin = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap()
            .stdin
            .take()
            .unwrap();
        let mock_stdout = tokio::process::Command::new("echo")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .stdout
            .take()
            .unwrap();

        let transport = LspTransport::new(mock_stdin, mock_stdout);
        let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);
        let (_, mock_notification_rx) = mpsc::channel(1);

        let mut server = LspServer {
            client,
            capabilities: ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            notification_rx: mock_notification_rx,
            child: mock_child,
        };

        assert!(
            !server.has_exited().unwrap(),
            "freshly spawned `sleep 2` should still be running"
        );

        server.child.kill().await.unwrap();
        // `kill().await` waits for the process to actually exit, so the
        // very next `try_wait` reliably observes it as gone.
        assert!(
            server.has_exited().unwrap(),
            "killed child must report as exited"
        );
    }

    #[tokio::test]
    async fn test_lsp_server_getters() {
        use lsp_types::ServerCapabilities;

        let mock_child = tokio::process::Command::new("echo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mock_stdin = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap()
            .stdin
            .take()
            .unwrap();

        let mock_stdout = tokio::process::Command::new("echo")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .stdout
            .take()
            .unwrap();

        let transport = LspTransport::new(mock_stdin, mock_stdout);
        let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);
        let (_, mock_notification_rx) = mpsc::channel(1);

        let server = LspServer {
            client,
            capabilities: ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            notification_rx: mock_notification_rx,
            child: mock_child,
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
            message: "not found".to_string(),
        });

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("python"),
            language_id: "python".to_string(),
            command: "pyright".to_string(),
            message: "permission denied".to_string(),
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

        let mock_child1 = tokio::process::Command::new("echo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mock_stdin1 = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap()
            .stdin
            .take()
            .unwrap();

        let mock_stdout1 = tokio::process::Command::new("echo")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .stdout
            .take()
            .unwrap();

        let transport1 = LspTransport::new(mock_stdin1, mock_stdout1);
        let client1 = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport1);
        let (_, mock_notification_rx1) = mpsc::channel(1);

        let server1 = LspServer {
            client: client1,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            notification_rx: mock_notification_rx1,
            child: mock_child1,
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

        let mock_child = tokio::process::Command::new("echo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mock_stdin = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap()
            .stdin
            .take()
            .unwrap();

        let mock_stdout = tokio::process::Command::new("echo")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .stdout
            .take()
            .unwrap();

        let transport = LspTransport::new(mock_stdin, mock_stdout);
        let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);
        let (_, mock_notification_rx) = mpsc::channel(1);

        let server = LspServer {
            client,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            notification_rx: mock_notification_rx,
            child: mock_child,
        };

        result.add_server("rust".to_string(), server);

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("python"),
            language_id: "python".to_string(),
            command: "pyright".to_string(),
            message: "not found".to_string(),
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
            let mock_child = tokio::process::Command::new("echo")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();

            let mock_stdin = tokio::process::Command::new("cat")
                .stdin(Stdio::piped())
                .spawn()
                .unwrap()
                .stdin
                .take()
                .unwrap();

            let mock_stdout = tokio::process::Command::new("echo")
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
                .stdout
                .take()
                .unwrap();

            let transport = LspTransport::new(mock_stdin, mock_stdout);
            let config = if i == 0 {
                LspServerConfig::rust_analyzer()
            } else if i == 1 {
                LspServerConfig::pyright()
            } else {
                LspServerConfig::typescript()
            };
            let client = LspClient::from_transport(config.clone(), transport);
            let (_, mock_notification_rx) = mpsc::channel(1);

            let server = LspServer {
                client,
                capabilities: lsp_types::ServerCapabilities::default(),
                position_encoding: PositionEncodingKind::UTF8,
                notification_rx: mock_notification_rx,
                child: mock_child,
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

        let mock_child1 = tokio::process::Command::new("echo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mock_stdin1 = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap()
            .stdin
            .take()
            .unwrap();

        let mock_stdout1 = tokio::process::Command::new("echo")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .stdout
            .take()
            .unwrap();

        let transport1 = LspTransport::new(mock_stdin1, mock_stdout1);
        let client1 = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport1);
        let (_, mock_notification_rx1) = mpsc::channel(1);

        let server1 = LspServer {
            client: client1,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            notification_rx: mock_notification_rx1,
            child: mock_child1,
        };

        result.add_server("rust".to_string(), server1);
        assert_eq!(result.server_count(), 1);

        let mock_child2 = tokio::process::Command::new("echo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let mock_stdin2 = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap()
            .stdin
            .take()
            .unwrap();

        let mock_stdout2 = tokio::process::Command::new("echo")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .stdout
            .take()
            .unwrap();

        let transport2 = LspTransport::new(mock_stdin2, mock_stdout2);
        let client2 = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport2);
        let (_, mock_notification_rx2) = mpsc::channel(1);

        let server2 = LspServer {
            client: client2,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF16,
            notification_rx: mock_notification_rx2,
            child: mock_child2,
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
            message: "not found".to_string(),
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
            message: "not found".to_string(),
        });

        result.add_failure(ServerSpawnFailure {
            server_id: ServerId::from("typescript"),
            language_id: "typescript".to_string(),
            command: "tsserver".to_string(),
            message: "command not found".to_string(),
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
                heuristics: None,
                name: None,
                handles: None,
            },
            workspace_roots: vec![],
            initialization_options: None,
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
        assert!(failure.message.contains("spawn"));
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
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
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
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
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
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
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
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
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
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
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
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
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
                    heuristics: None,
                    name: None,
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
                notification_tx: None,
            },
        ];

        let result = LspServer::spawn_batch(&configs).await;

        assert_eq!(result.failure_count(), 2);
        assert_eq!(result.failures[0].language_id, "test1");
        assert_eq!(result.failures[1].language_id, "test2");
    }

    /// Builds an `LspServer` backed by mock `echo`/`cat` child processes, so
    /// it can be registered without a real language server. Mirrors the
    /// pattern already used by this module's other `LspServer`-literal
    /// tests (e.g. `test_server_init_result_partial_success`).
    fn fake_lsp_server() -> LspServer {
        let mock_child = tokio::process::Command::new("echo")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mock_stdin = tokio::process::Command::new("cat")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap()
            .stdin
            .take()
            .unwrap();
        let mock_stdout = tokio::process::Command::new("echo")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .stdout
            .take()
            .unwrap();
        let transport = LspTransport::new(mock_stdin, mock_stdout);
        let client = LspClient::from_transport(LspServerConfig::pyright(), transport);
        let (_, mock_notification_rx) = mpsc::channel(1);
        LspServer {
            client,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            notification_rx: mock_notification_rx,
            child: mock_child,
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
            timeout_seconds: 5,
            heuristics: None,
            name: None,
            handles: None,
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
                heuristics: None,
                name: Some("pyright-diag".to_string()),
                handles: Some(vec![ToolKind::Diagnostics]),
            },
            LspServerConfig {
                language_id: "python".to_string(),
                command: "pylsp".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                timeout_seconds: 30,
                heuristics: None,
                name: Some("pylsp".to_string()),
                handles: None,
            },
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();
        let translator = Translator::new().with_router(router);

        // Only pylsp actually registers; pyright-diag never spawned.
        let mut result = ServerInitResult::new();
        result.add_server(pylsp_id.clone(), fake_lsp_server());

        let registered = crate::register_servers(result, &translator, &HashMap::new());

        assert_eq!(
            registered.diagnostics_flags.get(&pylsp_id),
            Some(&true),
            "pylsp must inherit the diagnostics route once pyright-diag is \
             known dead, and the flag must reflect that post-rebind state"
        );
    }
}
