//! MCPLS - Universal MCP to LSP Bridge
//!
//! This binary provides an MCP server that exposes LSP capabilities as tools,
//! enabling AI agents to access semantic code intelligence.

use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use mcpls_core::ProjectConfigTrust;
use tokio::runtime::Runtime;

mod args;
mod logging;

use args::Args;

/// Grace period for the blocking pool to wind down after a panic; bounds the
/// wait on the stdin reader thread, which cannot be cancelled.
const PANIC_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// How a run of the server ended, and therefore the process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Success,
    Failure,
    Panicked,
}

impl Outcome {
    const fn exit_code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Panicked => 101,
        }
    }
}

fn main() {
    let args = Args::parse();

    // Initialize logging. No subscriber is installed yet, so failures here
    // must go straight to stderr.
    if let Err(err) = logging::init(&args.log_level, args.log_json) {
        eprintln!("failed to initialize logging: {err:?}");
        std::process::exit(1);
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("failed to build the async runtime: {err:?}");
            std::process::exit(1);
        }
    };

    // Route fatal errors through the tracing subscriber (rather than the
    // default `Result` `Termination` printer) so they honor --log-json too.
    let outcome = block_on_guarded(runtime, async {
        match run(args).await {
            Ok(()) => Outcome::Success,
            Err(err) => {
                tracing::error!(error = ?err, "mcpls exited with an error");
                Outcome::Failure
            }
        }
    });

    std::process::exit(outcome.exit_code());
}

/// Drives `future` to completion on `runtime`, turning a panic into
/// [`Outcome::Panicked`] instead of unwinding out of `main`.
///
/// On a panic the runtime is shut down with a bounded grace period, which
/// drops every task and with it every LSP child (`kill_on_drop`); the
/// lifeline/job binding covers exits that skip this path.
fn block_on_guarded(runtime: Runtime, future: impl Future<Output = Outcome>) -> Outcome {
    if let Ok(outcome) = panic::catch_unwind(AssertUnwindSafe(|| runtime.block_on(future))) {
        // Dropping the runtime would block on the uncancellable stdin read. See #308.
        runtime.shutdown_background();
        outcome
    } else {
        tracing::error!("mcpls panicked, shutting down the runtime to reap LSP servers");
        runtime.shutdown_timeout(PANIC_SHUTDOWN_GRACE);
        Outcome::Panicked
    }
}

/// The HTTP transport configuration selected by the command line.
#[cfg(feature = "transport-http")]
fn http_config(args: &Args, bind: std::net::SocketAddr) -> mcpls_core::HttpConfig {
    mcpls_core::HttpConfig::new(bind)
        .with_path(args.http_path.clone())
        .with_stream_liveness(args.http_stream_liveness.into())
        .with_allowed_origins(args.allowed_origins())
        .with_allowed_hosts(args.allowed_hosts())
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
            args.listen.map_or(mcpls_core::Transport::Stdio, |bind| {
                mcpls_core::Transport::Http(http_config(&args, bind))
            })
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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn test_runtime() -> Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    #[cfg(feature = "transport-http")]
    #[test]
    fn test_http_config_carries_the_allowed_origins_of_the_command_line() {
        use clap::Parser as _;

        let args = Args::parse_from([
            "mcpls",
            "--http-allowed-origin",
            "https://app.example.com, http://[::1]:8080",
            "--http-path",
            "/api/mcp",
        ]);

        let cfg = http_config(&args, "127.0.0.1:3000".parse().unwrap());

        let origins: Vec<String> = cfg
            .allowed_origins
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            origins,
            ["https://app.example.com:443", "http://[::1]:8080"]
        );
        assert_eq!(cfg.path.as_str(), "/api/mcp");
    }

    #[cfg(feature = "transport-http")]
    #[test]
    fn test_http_config_carries_the_allowed_hosts_of_the_command_line() {
        use clap::Parser as _;

        let args = Args::parse_from([
            "mcpls",
            "--http-allowed-host",
            "Mcp.Example.com, [::1]:8080",
        ]);

        let cfg = http_config(&args, "0.0.0.0:3000".parse().unwrap());

        let hosts: Vec<String> = cfg.allowed_hosts.iter().map(ToString::to_string).collect();
        assert_eq!(hosts, ["mcp.example.com", "[::1]:8080"]);
    }

    #[test]
    fn test_outcome_exit_codes() {
        assert_eq!(Outcome::Success.exit_code(), 0);
        assert_eq!(Outcome::Failure.exit_code(), 1);
        assert_eq!(Outcome::Panicked.exit_code(), 101);
    }

    #[test]
    fn test_block_on_guarded_passes_through_outcome() {
        let outcome = block_on_guarded(test_runtime(), async { Outcome::Failure });
        assert_eq!(outcome, Outcome::Failure);
    }

    /// A panic must yield `Panicked` promptly even while a blocking thread is
    /// parked forever, standing in for the stdin reader.
    #[test]
    fn test_block_on_guarded_reports_panic_despite_parked_blocking_thread() {
        let (_keep_open, parked_rx) = std::sync::mpsc::channel::<()>();
        let started = Instant::now();

        let outcome = block_on_guarded(test_runtime(), async move {
            tokio::task::spawn_blocking(move || {
                let _ = parked_rx.recv();
            });
            tokio::task::yield_now().await;
            panic!("simulated handler panic");
        });

        assert_eq!(outcome, Outcome::Panicked);
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
