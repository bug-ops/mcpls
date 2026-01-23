//! LSP server lifecycle management.
//!
//! This module handles the complete lifecycle of an LSP server:
//! 1. Spawn server process
//! 2. Initialize → initialized handshake
//! 3. Capability negotiation
//! 4. Active request handling
//! 5. Graceful shutdown sequence

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
use crate::error::{Error, Result};
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

impl std::fmt::Display for ServerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Uninitialized => "uninitialized",
            Self::Initializing => "initializing",
            Self::Ready => "ready",
            Self::ShuttingDown => "shutting_down",
            Self::Shutdown => "shutdown",
        };
        write!(f, "{s}")
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
    fn test_server_state_display_ready() {
        assert_eq!(format!("{}", ServerState::Ready), "ready");
    }

    #[test]
    fn test_server_state_display_initializing() {
        assert_eq!(format!("{}", ServerState::Initializing), "initializing");
    }

    #[test]
    fn test_server_state_display_uninitialized() {
        assert_eq!(format!("{}", ServerState::Uninitialized), "uninitialized");
    }

    #[test]
    fn test_server_state_display_shutting_down() {
        assert_eq!(format!("{}", ServerState::ShuttingDown), "shutting_down");
    }

    #[test]
    fn test_server_state_display_shutdown() {
        assert_eq!(format!("{}", ServerState::Shutdown), "shutdown");
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
}
