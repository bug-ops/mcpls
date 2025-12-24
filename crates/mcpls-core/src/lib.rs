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

pub use config::ServerConfig;
pub use error::Error;

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
pub async fn serve(_config: ServerConfig) -> Result<(), Error> {
    // TODO: Implement server initialization
    // 1. Initialize LSP clients based on configuration
    // 2. Set up MCP server with rmcp
    // 3. Register MCP tools
    // 4. Start serving requests
    tracing::info!("MCPLS server starting...");
    Ok(())
}
