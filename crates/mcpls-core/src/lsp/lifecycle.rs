//! LSP server lifecycle management.
//!
//! This module handles the complete lifecycle of an LSP server:
//! 1. Spawn server process
//! 2. Initialize → initialized handshake
//! 3. Capability negotiation
//! 4. Active request handling
//! 5. Graceful shutdown sequence

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr;

use lsp_types::{
    ClientCapabilities, ClientInfo, GeneralClientCapabilities, InitializeParams, InitializeResult,
    InitializedParams, PositionEncodingKind, ServerCapabilities, Uri, WorkspaceFolder,
};
use tokio::process::Command;
use tokio::time::Duration;
use tracing::{debug, info};

use crate::config::LspServerConfig;
use crate::error::{Error, Result, ServerSpawnFailure};
use crate::lsp::client::LspClient;
use crate::lsp::transport::LspTransport;

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
    /// Successfully initialized servers (`language_id` -> server).
    pub servers: HashMap<String, LspServer>,
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
    pub fn failure_count(&self) -> usize {
        self.failures.len()
    }

    /// Add a successful server.
    ///
    /// If a server with the same `language_id` already exists, it will be replaced.
    pub fn add_server(&mut self, language_id: String, server: LspServer) {
        self.servers.insert(language_id, server);
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
    /// Child process handle. Kept alive for process lifetime management.
    /// When dropped, the process is terminated via SIGKILL (`kill_on_drop`).
    _child: tokio::process::Child,
}

impl std::fmt::Debug for LspServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspServer")
            .field("client", &self.client)
            .field("capabilities", &self.capabilities)
            .field("position_encoding", &self.position_encoding)
            .field("_child", &"<process>")
            .finish()
    }
}

impl LspServer {
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

        let mut child = Command::new(&config.server_config.command)
            .args(&config.server_config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::ServerSpawnFailed {
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
        let client = LspClient::from_transport(config.server_config.clone(), transport);

        let (capabilities, position_encoding) = Self::initialize(&client, &config).await?;

        info!("LSP server initialized successfully");

        Ok(Self {
            client,
            capabilities,
            position_encoding,
            _child: child,
        })
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
            .map(|root| {
                let path_str = root.to_str().ok_or_else(|| {
                    let root_display = root.display();
                    Error::InvalidUri(format!("Invalid UTF-8 in path: {root_display}"))
                })?;
                let uri_str = if cfg!(windows) {
                    format!("file:///{}", path_str.replace('\\', "/"))
                } else {
                    format!("file://{path_str}")
                };
                let uri = Uri::from_str(&uri_str).map_err(|_| {
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
            })
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

        let result: InitializeResult = client
            .request("initialize", params, Duration::from_secs(30))
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
    ///     },
    ///     ServerInitConfig {
    ///         server_config: LspServerConfig::pyright(),
    ///         workspace_roots: vec![PathBuf::from("/workspace")],
    ///         initialization_options: None,
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
            let language_id = config.server_config.language_id.clone();
            let command = config.server_config.command.clone();

            match Self::spawn(config.clone()).await {
                Ok(server) => {
                    info!(
                        "Successfully spawned LSP server: {} ({})",
                        language_id, command
                    );
                    result.add_server(language_id, server);
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to spawn LSP server: {} ({}): {}",
                        language_id,
                        command,
                        e
                    );
                    result.add_failure(ServerSpawnFailure {
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
            },
            workspace_roots: vec![PathBuf::from("/workspace")],
            initialization_options: Some(init_opts),
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
        };

        assert_eq!(config.workspace_roots.len(), 3);
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

        let server = LspServer {
            client,
            capabilities: ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            _child: mock_child,
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
            language_id: "rust".to_string(),
            command: "rust-analyzer".to_string(),
            message: "not found".to_string(),
        });

        result.add_failure(ServerSpawnFailure {
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

        let server1 = LspServer {
            client: client1,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            _child: mock_child1,
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

        let server = LspServer {
            client,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            _child: mock_child,
        };

        result.add_server("rust".to_string(), server);

        result.add_failure(ServerSpawnFailure {
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

            let server = LspServer {
                client,
                capabilities: lsp_types::ServerCapabilities::default(),
                position_encoding: PositionEncodingKind::UTF8,
                _child: mock_child,
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

        let server1 = LspServer {
            client: client1,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF8,
            _child: mock_child1,
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

        let server2 = LspServer {
            client: client2,
            capabilities: lsp_types::ServerCapabilities::default(),
            position_encoding: PositionEncodingKind::UTF16,
            _child: mock_child2,
        };

        result.add_server("rust".to_string(), server2);
        assert_eq!(result.server_count(), 1);
    }

    #[test]
    fn test_server_init_result_debug() {
        let mut result = ServerInitResult::new();

        result.add_failure(ServerSpawnFailure {
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
            language_id: "python".to_string(),
            command: "pyright".to_string(),
            message: "not found".to_string(),
        });

        result.add_failure(ServerSpawnFailure {
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
            },
            workspace_roots: vec![],
            initialization_options: None,
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
                },
                workspace_roots: vec![],
                initialization_options: None,
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
                },
                workspace_roots: vec![],
                initialization_options: None,
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
                },
                workspace_roots: vec![],
                initialization_options: None,
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
                },
                workspace_roots: vec![],
                initialization_options: None,
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
                },
                workspace_roots: vec![],
                initialization_options: None,
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
                },
                workspace_roots: vec![],
                initialization_options: None,
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
                },
                workspace_roots: vec![],
                initialization_options: None,
            },
        ];

        let result = LspServer::spawn_batch(&configs).await;

        assert_eq!(result.failure_count(), 2);
        assert_eq!(result.failures[0].language_id, "test1");
        assert_eq!(result.failures[1].language_id, "test2");
    }
}
