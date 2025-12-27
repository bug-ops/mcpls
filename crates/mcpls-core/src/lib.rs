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
//! - [`error`] - Error types for the library
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
use tracing::{info, warn};

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
            Ok(cwd) => match cwd.canonicalize() {
                Ok(canonical) => {
                    info!(
                        "Using current directory as workspace root: {}",
                        canonical.display()
                    );
                    vec![canonical]
                }
                Err(e) => {
                    warn!("Failed to canonicalize current directory: {e}");
                    vec![PathBuf::from(".")]
                }
            },
            Err(e) => {
                warn!("Failed to get current directory: {e}");
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
///
/// # Errors
///
/// Returns an error if:
/// - LSP server initialization fails
/// - MCP server setup fails
/// - Configuration is invalid
pub async fn serve(config: ServerConfig) -> Result<(), Error> {
    tracing::info!("Starting MCPLS server...");

    let mut translator = Translator::new();
    let workspace_roots = resolve_workspace_roots(&config.workspace.roots);

    translator.set_workspace_roots(workspace_roots.clone());

    for lsp_config in config.lsp_servers {
        tracing::info!(
            "Spawning LSP server for language '{}': {} {:?}",
            lsp_config.language_id,
            lsp_config.command,
            lsp_config.args
        );

        let server_init_config = ServerInitConfig {
            server_config: lsp_config.clone(),
            workspace_roots: workspace_roots.clone(),
            initialization_options: lsp_config.initialization_options.clone(),
        };

        let server = LspServer::spawn(server_init_config).await?;
        let client = server.client().clone();

        translator.register_client(lsp_config.language_id.clone(), client);
        translator.register_server(lsp_config.language_id.clone(), server);
    }

    let translator = Arc::new(Mutex::new(translator));

    tracing::info!("Starting MCP server with rmcp...");
    let mcp_server = mcp::McplsServer::new(translator);

    tracing::info!("MCPLS server initialized successfully");
    tracing::info!("Listening for MCP requests on stdio...");

    let service = mcp_server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| Error::McpServer(format!("Failed to start MCP server: {e}")))?;

    service
        .waiting()
        .await
        .map_err(|e| Error::McpServer(format!("MCP server error: {e}")))?;

    tracing::info!("MCPLS server shutting down");
    Ok(())
}

#[cfg(test)]
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
}
