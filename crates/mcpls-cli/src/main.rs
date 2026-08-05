//! MCPLS - Universal MCP to LSP Bridge
//!
//! This binary provides an MCP server that exposes LSP capabilities as tools,
//! enabling AI agents to access semantic code intelligence.

use anyhow::{Context, Result};
use clap::Parser;
use mcpls_core::ProjectConfigTrust;

mod args;
mod logging;

use args::Args;

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Initialize logging. No subscriber is installed yet, so failures here
    // must go straight to stderr.
    if let Err(err) = logging::init(&args.log_level, args.log_json) {
        eprintln!("failed to initialize logging: {err:?}");
        std::process::exit(1);
    }

    // Route fatal errors through the tracing subscriber (rather than the
    // default `Result` `Termination` printer) so they honor --log-json too.
    let exit_code = if let Err(err) = run(args).await {
        tracing::error!(error = ?err, "mcpls exited with an error");
        1
    } else {
        0
    };

    // `#[tokio::main]`'s generated wrapper blocks in `Runtime::drop` ->
    // `BlockingPool::shutdown` after this function returns, waiting for
    // every outstanding spawn_blocking thread -- including the one
    // `rmcp::transport::stdio()` (== `tokio::io::stdin()`) parks in a raw,
    // uncancellable `read()` on the real stdin fd. That read only returns on
    // more input or EOF, so if the MCP client's write end of stdin is still
    // open, the wait never completes even though `run()` above (which
    // includes LSP server shutdown and all shutdown logging) has already
    // finished. `process::exit` terminates immediately, bypassing that wait
    // -- safe here because everything that matters has already completed
    // above. See #308.
    std::process::exit(exit_code);
}

async fn run(args: Args) -> Result<()> {
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting mcpls");

    // Load configuration
    let config = if let Some(config_path) = &args.config {
        mcpls_core::ServerConfig::load_from(config_path)
            .with_context(|| format!("failed to load config from {}", config_path.display()))?
    } else {
        let trust = if args.trust_project_config {
            ProjectConfigTrust::Trusted
        } else {
            ProjectConfigTrust::Untrusted
        };
        mcpls_core::ServerConfig::load_with_trust(trust).context("failed to load configuration")?
    };

    tracing::debug!(
        lsp_servers = config.lsp_servers.len(),
        "configuration loaded"
    );

    // Select transport based on CLI flags.
    let transport = {
        #[cfg(feature = "transport-http")]
        {
            match args.listen {
                Some(bind) => mcpls_core::Transport::Http(mcpls_core::HttpConfig::new(
                    bind,
                    args.http_path.clone(),
                )),
                None => mcpls_core::Transport::Stdio,
            }
        }
        #[cfg(not(feature = "transport-http"))]
        {
            mcpls_core::Transport::Stdio
        }
    };

    mcpls_core::serve_with(config, transport)
        .await
        .context("server error")?;

    tracing::info!("mcpls shutdown complete");
    Ok(())
}
