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

use bridge::Translator;
pub use config::ServerConfig;
pub use error::Error;
use lsp::{LspServer, ServerInitConfig};
use rmcp::ServiceExt;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

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
    let extension_map = config.workspace.build_extension_map();

    let mut translator = Translator::new().with_extensions(extension_map);
    translator.set_workspace_roots(workspace_roots.clone());

    // Build configurations for batch spawning
    let server_configs: Vec<ServerInitConfig> = config
        .lsp_servers
        .iter()
        .map(|lsp_config| ServerInitConfig {
            server_config: lsp_config.clone(),
            workspace_roots: workspace_roots.clone(),
            initialization_options: lsp_config.initialization_options.clone(),
        })
        .collect();

    info!(
        "Attempting to spawn {} LSP server(s)...",
        server_configs.len()
    );

    // Spawn all servers with graceful degradation
    let result = LspServer::spawn_batch(&server_configs).await;

    // Handle the three possible outcomes
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

    // Check if at least one server successfully initialized
    if !result.has_servers() {
        return Err(Error::NoServersAvailable(
            "none configured or all failed to initialize".to_string(),
        ));
    }

    // Register all successfully initialized servers
    let server_count = result.server_count();
    for (language_id, server) in result.servers {
        let client = server.client().clone();
        translator.register_client(language_id.clone(), client);
        translator.register_server(language_id.clone(), server);
    }

    info!("Proceeding with {} LSP server(s)", server_count);

    let translator = Arc::new(Mutex::new(translator));

    info!("Starting MCP server with rmcp...");
    let mcp_server = mcp::McplsServer::new(translator);

    info!("MCPLS server initialized successfully");
    info!("Listening for MCP requests on stdio...");

    let service = mcp_server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| Error::McpServer(format!("Failed to start MCP server: {e}")))?;

    service
        .waiting()
        .await
        .map_err(|e| Error::McpServer(format!("MCP server error: {e}")))?;

    info!("MCPLS server shutting down");
    Ok(())
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
                },
                lsp_servers: vec![LspServerConfig {
                    language_id: "rust".to_string(),
                    command: "nonexistent-command-that-will-fail-12345".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec!["**/*.rs".to_string()],
                    initialization_options: None,
                    timeout_seconds: 10,
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
        async fn test_serve_fails_with_empty_config() {
            use crate::config::WorkspaceConfig;

            // Create a config with no servers
            let config = ServerConfig {
                workspace: WorkspaceConfig {
                    roots: vec![PathBuf::from("/tmp/test-workspace")],
                    position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                    language_extensions: vec![],
                },
                lsp_servers: vec![],
            };

            let result = serve(config).await;

            assert!(result.is_err());
            let err = result.unwrap_err();

            // Should return NoServersAvailable because no servers were configured
            assert!(
                matches!(err, Error::NoServersAvailable(_)),
                "Expected NoServersAvailable error, got: {err:?}"
            );

            if let Error::NoServersAvailable(msg) = err {
                assert!(msg.contains("none configured"));
            }
        }
    }
}
