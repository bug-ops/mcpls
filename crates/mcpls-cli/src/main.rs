//! MCPLS - Universal MCP to LSP Bridge
//!
//! This binary provides an MCP server that exposes LSP capabilities as tools,
//! enabling AI agents to access semantic code intelligence.

use std::future::Future;
use std::io::Write;
use std::panic::{self, AssertUnwindSafe, Location, PanicHookInfo};
use std::time::Duration;

use anyhow::{Context, Result};
use mcpls_core::config::ConfigOrigin;
use mcpls_core::{PanicMessage, ProjectConfigTrust, WorkspaceTrust};
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
    /// Logs `err` through the tracing subscriber and returns [`Self::Failure`].
    fn failed(err: &impl std::fmt::Debug) -> Self {
        tracing::error!(error = ?err, "mcpls exited with an error");
        Self::Failure
    }

    const fn exit_code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Panicked => 101,
        }
    }
}

fn main() {
    let (args, config_origin) = Args::parse_with_config_origin();
    let trust = args.workspace_trust().unwrap_or_else(|err| err.exit());

    // Initialize logging. No subscriber is installed yet, so failures here
    // must go straight to stderr.
    if let Err(err) = logging::init(&args.log_level, args.log_format()) {
        eprintln!("failed to initialize logging: {err:?}");
        std::process::exit(Outcome::Failure.exit_code());
    }

    install_panic_hook();

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting mcpls");

    // Loaded before the runtime exists: it reads and may create files, which
    // must not run on an async worker.
    let config = match load_config(&args, config_origin, &trust) {
        Ok(config) => config,
        Err(err) => std::process::exit(Outcome::failed(&err).exit_code()),
    };

    let transport = transport(&args);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("failed to build the async runtime: {err:?}");
            std::process::exit(Outcome::Failure.exit_code());
        }
    };

    // Route fatal errors through the tracing subscriber (rather than the
    // default `Result` `Termination` printer) so they honor --log-json too.
    let outcome = block_on_guarded(runtime, async {
        match run(config, transport).await {
            Ok(()) => Outcome::Success,
            Err(err) => Outcome::failed(&err),
        }
    });

    std::process::exit(outcome.exit_code());
}

/// Replaces the default panic hook, which prints the raw payload to stderr
/// and so can forge a log line (CWE-117).
///
/// The record is one line carrying the location and the escaped, bounded
/// payload. It goes through tracing, or, when the active filter or a missing
/// subscriber would drop it (`--log-level off`), straight to stderr. Panics
/// before logging is initialised keep the default hook; their input is the
/// operator's own command line.
///
/// The hook prints no `RUST_BACKTRACE` output, and the stderr fallback line is
/// plain text even under `--log-format json`.
fn install_panic_hook() {
    panic::set_hook(Box::new(log_panic));
}

fn log_panic(info: &PanicHookInfo<'_>) {
    let message = PanicMessage::from_payload(info.payload());
    report_panic(
        tracing::enabled!(tracing::Level::ERROR),
        info.location(),
        &message,
        &mut std::io::stderr().lock(),
    );
}

/// Logs the panic through tracing when `tracing_enabled`, otherwise writes one
/// line to `fallback`. A failed write is ignored: the hook must never panic.
fn report_panic(
    tracing_enabled: bool,
    location: Option<&Location<'_>>,
    message: &PanicMessage,
    fallback: &mut impl Write,
) {
    if tracing_enabled {
        tracing::error!(
            file = location.map(Location::file),
            line = location.map(Location::line),
            column = location.map(Location::column),
            payload = %message,
            "mcpls panicked"
        );
    } else {
        let _ = writeln!(fallback, "{}", panic_line(location, message));
    }
}

fn panic_line(location: Option<&Location<'_>>, message: &PanicMessage) -> String {
    location.map_or_else(
        || format!("mcpls panicked: {message}"),
        |at| format!("mcpls panicked at {at}: {message}"),
    )
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

/// Loads the configuration. In untrusted mode the file that was actually loaded
/// must lie outside the workspace, whichever way it was found.
fn load_config(
    args: &Args,
    config_origin: ConfigOrigin,
    trust: &WorkspaceTrust,
) -> Result<mcpls_core::ServerConfig> {
    args.config.as_ref().map_or_else(
        || {
            let project_trust = if args.trust_project_config {
                ProjectConfigTrust::Trusted
            } else {
                ProjectConfigTrust::Untrusted
            };
            mcpls_core::ServerConfig::load_discovered(project_trust, trust)
                .context("failed to load configuration")
        },
        |config_path| {
            mcpls_core::ServerConfig::load_explicit(config_path, config_origin, trust)
                .with_context(|| format!("failed to load config from {}", config_path.display()))
        },
    )
}

/// The transport selected by the command line.
#[cfg(feature = "transport-http")]
fn transport(args: &Args) -> mcpls_core::Transport {
    args.listen.map_or(mcpls_core::Transport::Stdio, |bind| {
        mcpls_core::Transport::Http(http_config(args, bind))
    })
}

/// The transport selected by the command line.
#[cfg(not(feature = "transport-http"))]
const fn transport(_args: &Args) -> mcpls_core::Transport {
    mcpls_core::Transport::Stdio
}

async fn run(config: mcpls_core::ServerConfig, transport: mcpls_core::Transport) -> Result<()> {
    tracing::debug!(
        lsp_servers = config.lsp_servers.len(),
        "configuration loaded"
    );

    mcpls_core::serve_with(config, transport)
        .await
        .context("server error")?;

    tracing::info!("mcpls shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::*;

    static HOOK_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Panics with `payload` under the installed hook and a plain (non-escaping)
    /// fmt layer, so only the hook can make the output safe.
    fn hooked_panic_output(payload: &str) -> String {
        let buf = SharedBuf::default();
        let writer = buf.clone();
        let builder = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false);
        hooked_panic_with(builder.finish(), &buf, payload)
    }

    fn hooked_panic_with(
        subscriber: impl tracing::Subscriber + Send + Sync,
        buf: &SharedBuf,
        payload: &str,
    ) -> String {
        let _serial = HOOK_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = panic::take_hook();
        install_panic_hook();
        tracing::subscriber::with_default(subscriber, || {
            let _ = panic::catch_unwind(|| panic!("{payload}"));
        });
        panic::set_hook(previous);
        String::from_utf8(buf.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn test_panic_hook_json_has_a_single_escaped_bounded_payload_field() {
        let buf = SharedBuf::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(move || writer.clone())
            .finish();

        let output = hooked_panic_with(subscriber, &buf, &format!("a\nforged{}", "x".repeat(5000)));

        assert_eq!(output.lines().count(), 1, "got {output:?}");
        assert_eq!(output.matches("\"payload\"").count(), 1, "got {output:?}");
        assert_eq!(output.matches("\"message\"").count(), 1, "got {output:?}");
        assert!(output.contains("a\\\\nforged"), "got {output:?}");
        assert!(output.contains("... (truncated)"), "got {output:?}");
        assert!(output.len() < 1000, "got {} bytes", output.len());
    }

    struct BrokenPipe;

    impl Write for BrokenPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_report_panic_without_tracing_writes_one_escaped_line_to_the_fallback() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("c\nERROR forged");
        let message = PanicMessage::from_payload(payload.as_ref());
        let mut sink = Vec::new();

        report_panic(false, Some(Location::caller()), &message, &mut sink);

        let line = String::from_utf8(sink).unwrap();
        assert_eq!(line.lines().count(), 1, "got {line:?}");
        assert!(line.ends_with("c\\nERROR forged\n"), "got {line:?}");
    }

    #[test]
    fn test_report_panic_with_tracing_leaves_the_fallback_untouched() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("d");
        let message = PanicMessage::from_payload(payload.as_ref());
        let mut sink = Vec::new();

        report_panic(true, None, &message, &mut sink);

        assert_eq!(sink, Vec::<u8>::new());
    }

    #[test]
    fn test_report_panic_ignores_a_broken_fallback_pipe() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("e");
        let message = PanicMessage::from_payload(payload.as_ref());

        report_panic(false, None, &message, &mut BrokenPipe);
    }

    #[test]
    fn test_panic_hook_logs_one_escaped_line_with_location() {
        let output = hooked_panic_output("a\nERROR forged");

        assert_eq!(output.lines().count(), 1, "got {output:?}");
        assert!(output.contains("a\\nERROR forged"), "got {output:?}");
        assert!(output.contains("main.rs"), "got {output:?}");
    }

    #[test]
    fn test_panic_hook_bounds_a_long_payload() {
        let output = hooked_panic_output(&"x".repeat(5000));

        assert_eq!(output.lines().count(), 1, "got {output:?}");
        assert!(output.contains("... (truncated)"), "got {output:?}");
        assert!(output.len() < 1000, "got {} bytes", output.len());
    }

    #[test]
    fn test_panic_line_is_one_escaped_line() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("b\r\nERROR forged");
        let message = PanicMessage::from_payload(payload.as_ref());

        let line = panic_line(Some(Location::caller()), &message);

        assert_eq!(line.lines().count(), 1, "got {line:?}");
        assert!(line.ends_with("b\\r\\nERROR forged"), "got {line:?}");
        assert_eq!(
            panic_line(None, &message),
            "mcpls panicked: b\\r\\nERROR forged"
        );
    }

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
