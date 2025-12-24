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
    let workspace_roots = if config.workspace.roots.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        config.workspace.roots.clone()
    };

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
