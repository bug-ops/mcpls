//! MCPLS - Universal MCP to LSP Bridge
//!
//! This binary provides an MCP server that exposes LSP capabilities as tools,
//! enabling AI agents to access semantic code intelligence.

use anyhow::{Context, Result};
use clap::Parser;

mod args;
mod logging;

use args::Args;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    logging::init(args.log_level)?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting mcpls"
    );

    // Load configuration
    let config = if let Some(config_path) = &args.config {
        mcpls_core::ServerConfig::load_from(config_path)
            .with_context(|| format!("failed to load config from {}", config_path.display()))?
    } else {
        mcpls_core::ServerConfig::load()
            .context("failed to load configuration")?
    };

    tracing::debug!(
        lsp_servers = config.lsp_servers.len(),
        "configuration loaded"
    );

    // Start the server
    mcpls_core::serve(config)
        .await
        .context("server error")?;

    tracing::info!("mcpls shutdown complete");
    Ok(())
}
