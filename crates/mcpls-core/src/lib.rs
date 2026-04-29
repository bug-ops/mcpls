//! # mcpls-core
//!
//! Core library for MCP (Model Context Protocol) to LSP (Language Server Protocol) translation.
//!
//! This crate provides the fundamental building blocks for bridging AI agents with
//! language servers, enabling semantic code intelligence through MCP tools.
//!
//! ## Architecture
//!
//! The library is organized into several modules:
//!
//! - [`lsp`] - LSP client implementation for communicating with language servers
//! - [`mcp`] - MCP tool definitions and handlers
//! - [`bridge`] - Translation layer between MCP and LSP protocols
//! - [`config`] - Configuration types and loading
//! - [`mod@error`] - Error types for the library
//!
//! ## Example
//!
//! ```rust,ignore
//! use mcpls_core::{serve, ServerConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), mcpls_core::Error> {
//!     let config = ServerConfig::load()?;
//!     serve(config).await
//! }
//! ```

pub mod bridge;
pub mod config;
pub mod error;
pub mod lsp;
pub mod mcp;

use std::path::PathBuf;
use std::sync::Arc;

use bridge::resources::make_uri;
use bridge::{ResourceSubscriptions, Translator};
pub use config::ServerConfig;
pub use error::Error;
use lsp::{LspNotification, LspServer, ServerInitConfig};
use rmcp::ServiceExt;
use rmcp::model::ResourceUpdatedNotificationParam;
use tokio::sync::{Mutex, OnceCell};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

/// Background task that drains LSP notifications, writes them to the cache,
/// and forwards `resources/updated` to the MCP peer when subscribed.
///
/// The task operates in two phases without explicit state:
/// - **Phase A** (before peer is set): caches every notification, skips peer notify.
/// - **Phase B** (after peer is set): additionally fires `notify_resource_updated`
///   for subscribed `PublishDiagnostics` URIs.
///
/// The task exits when:
/// - The LSP notification channel closes (`rx.recv()` returns `None`).
/// - The cancellation watch fires (or the sender is dropped).
/// - `notify_resource_updated` returns an error (peer disconnect / transport closed).
///
/// # Note on lock contention (TODO critic-S4)
/// All cache writes acquire `Arc<Mutex<Translator>>`, which is the same lock used
/// by every MCP tool call. Splitting `NotificationCache` into its own `Arc<RwLock>`
/// would eliminate this contention. Tracked as a P2 follow-up.
pub(crate) async fn diagnostics_pump(
    _lang: String,
    mut rx: tokio::sync::mpsc::Receiver<LspNotification>,
    translator: Arc<Mutex<Translator>>,
    subs: Arc<ResourceSubscriptions>,
    peer_cell: Arc<OnceCell<rmcp::Peer<rmcp::RoleServer>>>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            // Exit when cancellation is requested or the sender is dropped.
            result = cancel_rx.changed() => {
                // Err means the sender was dropped; treat as cancellation.
                if result.is_err() || *cancel_rx.borrow() {
                    break;
                }
            }
            msg = rx.recv() => {
                let Some(notif) = msg else { break };
                match notif {
                    LspNotification::PublishDiagnostics(p) => {
                        // Always cache unconditionally.
                        {
                            let mut t = translator.lock().await;
                            t.notification_cache_mut()
                                .store_diagnostics(&p.uri, p.version, p.diagnostics);
                        }

                        // Fast path: skip URI construction when nothing is subscribed.
                        if subs.is_empty().await {
                            continue;
                        }

                        // Notify only when peer is ready and URI is subscribed.
                        let Some(peer) = peer_cell.get() else { continue };
                        let Some(path) = bridge::uri_to_path(&p.uri) else { continue };
                        let Ok(mcp_uri) = make_uri(&path) else { continue };

                        // TODO(critic-S3): on subscribe, replay cached diagnostics once
                        // so clients that subscribe after the first PublishDiagnostics
                        // do not have to wait for the next LSP push.
                        if !subs.contains(&mcp_uri).await {
                            continue;
                        }

                        if peer
                            .notify_resource_updated(ResourceUpdatedNotificationParam::new(
                                mcp_uri,
                            ))
                            .await
                            .is_err()
                        {
                            // Peer disconnected; stop the pump.
                            break;
                        }
                    }
                    LspNotification::LogMessage(m) => {
                        let mut t = translator.lock().await;
                        t.notification_cache_mut()
                            .store_log(m.typ.into(), m.message);
                    }
                    LspNotification::ShowMessage(m) => {
                        let mut t = translator.lock().await;
                        t.notification_cache_mut()
                            .store_message(m.typ.into(), m.message);
                    }
                    LspNotification::Progress { .. } | LspNotification::Other { .. } => {}
                }
            }
        }
    }
}

/// Register initialized LSP servers with the translator and extract notification receivers.
///
/// Takes ownership of the `ServerInitResult`, extracts `notification_rx` from each server
/// before registration, and returns a map of language-id to receiver for the pump tasks.
fn register_servers(
    mut result: lsp::ServerInitResult,
    translator: &mut bridge::Translator,
) -> std::collections::HashMap<String, tokio::sync::mpsc::Receiver<lsp::LspNotification>> {
    let mut receivers = std::collections::HashMap::new();
    for (lang, server) in &mut result.servers {
        receivers.insert(lang.clone(), server.take_notification_rx());
    }
    for (language_id, server) in result.servers {
        let client = server.client().clone();
        translator.register_client(language_id.clone(), client);
        translator.register_server(language_id.clone(), server);
    }
    receivers
}

/// Resolve workspace roots from config or current directory.
///
/// If no workspace roots are provided in the configuration, this function
/// will use the current working directory, canonicalized for security.
///
/// # Returns
///
/// A vector of workspace root paths. If config roots are provided, they are
/// returned as-is. Otherwise, returns the canonicalized current directory,
/// falling back to relative "." if canonicalization fails.
fn resolve_workspace_roots(config_roots: &[PathBuf]) -> Vec<PathBuf> {
    if config_roots.is_empty() {
        match std::env::current_dir() {
            Ok(cwd) => {
                // current_dir() always returns an absolute path
                match cwd.canonicalize() {
                    Ok(canonical) => {
                        info!(
                            "Using current directory as workspace root: {}",
                            canonical.display()
                        );
                        vec![canonical]
                    }
                    Err(e) => {
                        // Canonicalization can fail if directory was deleted or permissions changed
                        // but cwd itself is still absolute
                        warn!(
                            "Failed to canonicalize current directory: {e}, using non-canonical path"
                        );
                        vec![cwd]
                    }
                }
            }
            Err(e) => {
                // This is extremely rare - only happens if cwd was deleted or unlinked
                // In this case, we have no choice but to use a relative path
                warn!("Failed to get current directory: {e}, using fallback");
                vec![PathBuf::from(".")]
            }
        }
    } else {
        config_roots.to_vec()
    }
}

/// Start the MCPLS server with the given configuration.
///
/// This is the primary entry point for running the MCP-LSP bridge.
/// Implements graceful degradation: if some but not all LSP servers fail
/// to initialize, the service continues with available servers.
///
/// # Errors
///
/// Returns an error if:
/// - All LSP servers fail to initialize
/// - MCP server setup fails
/// - Configuration is invalid
///
/// # Graceful Degradation
///
/// - **All servers succeed**: Service runs normally
/// - **Partial success**: Logs warnings for failures, continues with available servers
/// - **All servers fail**: Returns `Error::AllServersFailedToInit` with details
pub async fn serve(config: ServerConfig) -> Result<(), Error> {
    info!("Starting MCPLS server...");

    let workspace_roots = resolve_workspace_roots(&config.workspace.roots);
    let extension_map = config.build_effective_extension_map();
    let max_depth = Some(config.workspace.heuristics_max_depth);

    let mut translator = Translator::new().with_extensions(extension_map);
    translator.set_workspace_roots(workspace_roots.clone());

    let applicable_configs: Vec<ServerInitConfig> = config
        .lsp_servers
        .iter()
        .filter_map(|lsp_config| {
            let should_spawn = workspace_roots
                .iter()
                .any(|root| lsp_config.should_spawn(root, max_depth));

            if !should_spawn {
                info!(
                    "Skipping LSP server '{}' ({}): no project markers found",
                    lsp_config.language_id, lsp_config.command
                );
                return None;
            }

            Some(ServerInitConfig {
                server_config: lsp_config.clone(),
                workspace_roots: workspace_roots.clone(),
                initialization_options: lsp_config.initialization_options.clone(),
                notification_tx: None,
            })
        })
        .collect();

    info!(
        "Attempting to spawn {} applicable LSP server(s)...",
        applicable_configs.len()
    );

    // notification_receivers collects per-language mpsc receivers used by the pump tasks.
    let mut notification_receivers = std::collections::HashMap::new();

    if applicable_configs.is_empty() {
        warn!("No applicable LSP servers configured — starting in protocol-only mode");
    } else {
        // Spawn all servers with graceful degradation.
        let result = LspServer::spawn_batch(&applicable_configs).await;

        // Handle the three possible outcomes.
        if result.all_failed() {
            return Err(Error::AllServersFailedToInit {
                count: result.failure_count(),
                failures: result.failures,
            });
        }

        if result.partial_success() {
            warn!(
                "Partial server initialization: {} succeeded, {} failed",
                result.server_count(),
                result.failure_count()
            );
            for failure in &result.failures {
                error!("Server initialization failed: {}", failure);
            }
        }

        // Register servers and extract their notification receivers.
        let server_count = result.server_count();
        notification_receivers = register_servers(result, &mut translator);
        info!("Proceeding with {} LSP server(s)", server_count);
    }

    let translator = Arc::new(Mutex::new(translator));
    let subscriptions = Arc::new(ResourceSubscriptions::new());
    // Peer cell is populated after the MCP transport is established (Phase B).
    let peer_cell = Arc::new(OnceCell::new());

    // Cancellation for pump tasks: send `true` to request shutdown.
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let mut pumps: JoinSet<()> = JoinSet::new();

    // Phase A: start pump tasks before MCP serve so no notifications are dropped
    // while the transport is being established.
    for (lang, rx) in notification_receivers {
        pumps.spawn(diagnostics_pump(
            lang,
            rx,
            Arc::clone(&translator),
            Arc::clone(&subscriptions),
            Arc::clone(&peer_cell),
            cancel_rx.clone(),
        ));
    }

    info!("Starting MCP server with rmcp...");
    let mcp_server = mcp::McplsServer::new(Arc::clone(&translator), Arc::clone(&subscriptions));

    info!("MCPLS server initialized successfully");
    info!("Listening for MCP requests on stdio...");

    let service = mcp_server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| Error::McpServer(format!("Failed to start MCP server: {e}")))?;

    // Phase B: peer is now available; pump tasks will start sending notifications.
    if let Err(e) = peer_cell.set(service.peer().clone()) {
        // The cell can only be set once; a second set means a logic error.
        debug!("Peer cell already set ({}), ignoring", e);
    }

    let result = service
        .waiting()
        .await
        .map(|_| ())
        .map_err(|e| Error::McpServer(format!("MCP server error: {e}")));

    // Signal pump tasks to exit and wait for them.
    let _ = cancel_tx.send(true);
    while pumps.join_next().await.is_some() {}

    info!("MCPLS server shutting down");
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_workspace_roots_empty_config() {
        let roots = resolve_workspace_roots(&[]);
        assert_eq!(roots.len(), 1);
        assert!(
            roots[0].is_absolute(),
            "Workspace root should be absolute path"
        );
    }

    #[test]
    fn test_resolve_workspace_roots_with_config() {
        let config_roots = vec![PathBuf::from("/test/root")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots, config_roots);
    }

    #[test]
    fn test_resolve_workspace_roots_multiple_paths() {
        let config_roots = vec![PathBuf::from("/test/root1"), PathBuf::from("/test/root2")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots, config_roots);
        assert_eq!(roots.len(), 2);
    }

    #[test]
    fn test_resolve_workspace_roots_preserves_order() {
        let config_roots = vec![
            PathBuf::from("/workspace/alpha"),
            PathBuf::from("/workspace/beta"),
            PathBuf::from("/workspace/gamma"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots[0], PathBuf::from("/workspace/alpha"));
        assert_eq!(roots[1], PathBuf::from("/workspace/beta"));
        assert_eq!(roots[2], PathBuf::from("/workspace/gamma"));
    }

    #[test]
    fn test_resolve_workspace_roots_single_path() {
        let config_roots = vec![PathBuf::from("/single/workspace")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from("/single/workspace"));
    }

    #[test]
    fn test_resolve_workspace_roots_empty_returns_cwd() {
        let roots = resolve_workspace_roots(&[]);
        assert!(
            !roots.is_empty(),
            "Should return at least one workspace root"
        );
    }

    #[test]
    fn test_resolve_workspace_roots_relative_paths() {
        let config_roots = vec![
            PathBuf::from("relative/path1"),
            PathBuf::from("relative/path2"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("relative/path1"));
        assert_eq!(roots[1], PathBuf::from("relative/path2"));
    }

    #[test]
    fn test_resolve_workspace_roots_mixed_paths() {
        let config_roots = vec![
            PathBuf::from("/absolute/path"),
            PathBuf::from("relative/path"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/absolute/path"));
        assert_eq!(roots[1], PathBuf::from("relative/path"));
    }

    #[test]
    fn test_resolve_workspace_roots_with_dot_path() {
        let config_roots = vec![PathBuf::from(".")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots, config_roots);
    }

    #[test]
    fn test_resolve_workspace_roots_with_parent_path() {
        let config_roots = vec![PathBuf::from("..")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from(".."));
    }

    #[test]
    fn test_resolve_workspace_roots_unicode_paths() {
        let config_roots = vec![
            PathBuf::from("/workspace/テスト"),
            PathBuf::from("/workspace/тест"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/workspace/テスト"));
        assert_eq!(roots[1], PathBuf::from("/workspace/тест"));
    }

    #[test]
    fn test_resolve_workspace_roots_spaces_in_paths() {
        let config_roots = vec![
            PathBuf::from("/workspace/path with spaces"),
            PathBuf::from("/another path/workspace"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/workspace/path with spaces"));
    }

    // Tests for graceful degradation behavior
    mod graceful_degradation_tests {
        use super::*;
        use crate::error::ServerSpawnFailure;
        use crate::lsp::ServerInitResult;

        #[test]
        fn test_all_servers_failed_error_handling() {
            let mut result = ServerInitResult::new();
            result.add_failure(ServerSpawnFailure {
                language_id: "rust".to_string(),
                command: "rust-analyzer".to_string(),
                message: "not found".to_string(),
            });
            result.add_failure(ServerSpawnFailure {
                language_id: "python".to_string(),
                command: "pyright".to_string(),
                message: "not found".to_string(),
            });

            assert!(result.all_failed());
            assert_eq!(result.failure_count(), 2);
            assert_eq!(result.server_count(), 0);
        }

        #[test]
        fn test_partial_success_detection() {
            use std::collections::HashMap;

            let mut result = ServerInitResult::new();
            // Simulate one success and one failure
            result.servers = HashMap::new(); // Would have a real server in production
            result.add_failure(ServerSpawnFailure {
                language_id: "python".to_string(),
                command: "pyright".to_string(),
                message: "not found".to_string(),
            });

            // Without actual servers, we can verify the failure was recorded
            assert_eq!(result.failure_count(), 1);
            assert_eq!(result.server_count(), 0);
        }

        #[test]
        fn test_all_servers_succeeded_detection() {
            use std::collections::HashMap;

            let mut result = ServerInitResult::new();
            result.servers = HashMap::new(); // Would have real servers in production

            assert_eq!(result.failure_count(), 0);
            assert!(!result.all_failed());
            assert!(!result.partial_success());
        }

        #[test]
        fn test_all_servers_failed_to_init_error() {
            let failures = vec![
                ServerSpawnFailure {
                    language_id: "rust".to_string(),
                    command: "rust-analyzer".to_string(),
                    message: "command not found".to_string(),
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

            // Verify failures are preserved
            if let Error::AllServersFailedToInit { count, failures: f } = err {
                assert_eq!(count, 2);
                assert_eq!(f.len(), 2);
                assert_eq!(f[0].language_id, "rust");
                assert_eq!(f[1].language_id, "python");
            } else {
                panic!("Expected AllServersFailedToInit error");
            }
        }

        #[test]
        fn test_graceful_degradation_with_empty_config() {
            let result = ServerInitResult::new();

            // Empty config means no servers configured
            assert!(!result.all_failed());
            assert!(!result.partial_success());
            assert!(!result.has_servers());
            assert_eq!(result.server_count(), 0);
            assert_eq!(result.failure_count(), 0);
        }

        #[test]
        fn test_server_spawn_failure_display() {
            let failure = ServerSpawnFailure {
                language_id: "typescript".to_string(),
                command: "tsserver".to_string(),
                message: "executable not found in PATH".to_string(),
            };

            let display = failure.to_string();
            assert!(display.contains("typescript"));
            assert!(display.contains("tsserver"));
            assert!(display.contains("executable not found"));
        }

        #[test]
        fn test_result_helpers_consistency() {
            let mut result = ServerInitResult::new();

            // Initially empty
            assert!(!result.has_servers());
            assert!(!result.all_failed());
            assert!(!result.partial_success());

            // Add a failure
            result.add_failure(ServerSpawnFailure {
                language_id: "go".to_string(),
                command: "gopls".to_string(),
                message: "error".to_string(),
            });

            assert!(result.all_failed());
            assert!(!result.has_servers());
            assert!(!result.partial_success());
        }

        #[tokio::test]
        async fn test_serve_fails_with_no_servers_available() {
            use crate::config::{LspServerConfig, WorkspaceConfig};

            // Create a config with an invalid server (guaranteed to fail)
            let config = ServerConfig {
                workspace: WorkspaceConfig {
                    roots: vec![PathBuf::from("/tmp/test-workspace")],
                    position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                    language_extensions: vec![],
                    heuristics_max_depth: 10,
                },
                lsp_servers: vec![LspServerConfig {
                    language_id: "rust".to_string(),
                    command: "nonexistent-command-that-will-fail-12345".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec!["**/*.rs".to_string()],
                    initialization_options: None,
                    timeout_seconds: 10,
                    heuristics: None,
                }],
            };

            let result = serve(config).await;

            assert!(result.is_err());
            let err = result.unwrap_err();

            // The serve function should now return NoServersAvailable error
            // because all servers failed, but has_servers() returned false
            assert!(
                matches!(err, Error::NoServersAvailable(_))
                    || matches!(err, Error::AllServersFailedToInit { .. }),
                "Expected NoServersAvailable or AllServersFailedToInit error, got: {err:?}"
            );
        }

        #[tokio::test]
        async fn test_serve_starts_with_empty_config() {
            use crate::config::WorkspaceConfig;

            // Server starts in protocol-only mode when no LSP servers are configured.
            // serve() blocks until the MCP transport closes, so it will error with a
            // connection/transport error — not NoServersAvailable.
            let config = ServerConfig {
                workspace: WorkspaceConfig {
                    roots: vec![PathBuf::from("/tmp/test-workspace")],
                    position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                    language_extensions: vec![],
                    heuristics_max_depth: 10,
                },
                lsp_servers: vec![],
            };

            let result = serve(config).await;

            // serve() may succeed or fail with a transport error, but must NOT
            // return NoServersAvailable when the config simply has no servers.
            if let Err(ref err) = result {
                assert!(
                    !matches!(err, Error::NoServersAvailable(_)),
                    "serve() must not return NoServersAvailable for empty lsp_servers config"
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // diagnostics_pump unit tests
    // ------------------------------------------------------------------

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    mod pump_tests {
        use lsp_types::{PublishDiagnosticsParams, Uri};
        use tokio::sync::{mpsc, watch};

        use super::*;

        fn make_translator() -> Arc<Mutex<Translator>> {
            Arc::new(Mutex::new(Translator::new()))
        }

        fn make_subs() -> Arc<ResourceSubscriptions> {
            Arc::new(ResourceSubscriptions::new())
        }

        type PeerCell = Arc<OnceCell<rmcp::Peer<rmcp::RoleServer>>>;

        fn make_peer_cell() -> PeerCell {
            Arc::new(OnceCell::new())
        }

        /// `PublishDiagnostics` is cached even when the peer is not yet connected.
        #[tokio::test]
        async fn test_pump_caches_before_peer_set() {
            let translator = make_translator();
            let subs = make_subs();
            let peer_cell = make_peer_cell();
            let (tx, rx) = mpsc::channel(8);
            // Keep _cancel_tx alive: dropping it causes cancel_rx.changed() to return Err,
            // which makes the pump exit before processing any messages.
            let (_cancel_tx, cancel_rx) = watch::channel(false);

            let t = Arc::clone(&translator);
            tokio::spawn(diagnostics_pump(
                "rust".to_string(),
                rx,
                t,
                Arc::clone(&subs),
                Arc::clone(&peer_cell),
                cancel_rx,
            ));

            let uri: Uri = "file:///test/main.rs".parse().unwrap();
            tx.send(LspNotification::PublishDiagnostics(
                PublishDiagnosticsParams {
                    uri: uri.clone(),
                    diagnostics: vec![],
                    version: None,
                },
            ))
            .await
            .unwrap();
            drop(tx);

            // Poll until the pump processes the message or we time out.
            let cached = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    tokio::task::yield_now().await;
                    let found = {
                        let guard = translator.lock().await;
                        guard
                            .notification_cache()
                            .get_diagnostics(uri.as_str())
                            .is_some()
                    };
                    if found {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("pump did not cache diagnostics within 5 s");
            assert!(cached, "diagnostics should be cached before peer is set");
        }

        /// Pump exits cleanly when the cancel watch sends `true`.
        #[tokio::test]
        async fn test_pump_exits_on_cancel() {
            let translator = make_translator();
            let subs = make_subs();
            let peer_cell = make_peer_cell();
            let (_tx, rx) = mpsc::channel::<LspNotification>(8);
            let (cancel_tx, cancel_rx) = watch::channel(false);

            let handle = tokio::spawn(diagnostics_pump(
                "rust".to_string(),
                rx,
                translator,
                subs,
                peer_cell,
                cancel_rx,
            ));

            cancel_tx.send(true).unwrap();
            // Pump must finish within a short time after cancellation.
            tokio::time::timeout(std::time::Duration::from_millis(200), handle)
                .await
                .expect("pump did not exit within timeout")
                .unwrap();
        }

        /// Pump exits when the cancel sender is dropped (Err branch).
        #[tokio::test]
        async fn test_pump_exits_when_cancel_sender_dropped() {
            let translator = make_translator();
            let subs = make_subs();
            let peer_cell = make_peer_cell();
            let (_tx, rx) = mpsc::channel::<LspNotification>(8);
            let (cancel_tx, cancel_rx) = watch::channel(false);

            let handle = tokio::spawn(diagnostics_pump(
                "rust".to_string(),
                rx,
                translator,
                subs,
                peer_cell,
                cancel_rx,
            ));

            drop(cancel_tx); // triggers Err in cancel_rx.changed()
            tokio::time::timeout(std::time::Duration::from_millis(200), handle)
                .await
                .expect("pump did not exit within timeout")
                .unwrap();
        }
    }
}
