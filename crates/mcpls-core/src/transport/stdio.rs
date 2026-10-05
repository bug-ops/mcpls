//! The stdio transport runner.

use rmcp::ServiceExt as _;

use super::shutdown::ShutdownSignal;

/// Run the MCP server over stdio.
///
/// Serves the given `mcp_server` using stdin/stdout. Returns as soon as either
/// the stdio transport closes (client disconnect / stdin EOF) or a `SIGTERM`/
/// `SIGINT` is received, so callers can run orderly cleanup — such as
/// [`crate::bridge::Translator::shutdown_servers`] — before the process
/// exits. `shutdown_signal` is dropped when this function returns, and this
/// function does no draining of its own after a signal arrives — so a
/// repeat signal during the post-return cleanup in [`shutdown`](crate::runtime::shutdown::shutdown) is
/// caught only by the second `ShutdownSignal` that function registers for
/// itself, not by this one (see [`ShutdownSignal`]'s docs and #329).
///
/// `shutdown_signal` is constructed by [`crate::serve_with`] *before* any
/// startup work runs (see [`ShutdownSignal`]'s docs) and is raced here
/// against both the MCP handshake and, once it completes, the
/// post-handshake serve loop. `serve(..)` awaits the full MCP `initialize`
/// handshake internally (reading the client's request and writing the
/// response) before resolving, so a signal arriving during that wait — which
/// can be indefinite if the client is slow to send `initialize` — must be
/// caught there too, not only after the handshake finishes. On signal, the
/// in-flight handshake or `RunningService` is dropped rather than awaited to
/// completion; `rmcp` closes it asynchronously in that case, which is
/// acceptable here since the process exits shortly after -- callers must
/// exit via `std::process::exit` rather than returning normally from `main`,
/// or an uncancellable `tokio::io::stdin()` blocking thread can stall
/// runtime shutdown indefinitely (see `mcpls-cli`'s `main.rs` and #308).
pub async fn run_stdio(
    mcp_server: crate::mcp::McplsServer,
    mut shutdown_signal: ShutdownSignal,
) -> Result<(), crate::Error> {
    let service = tokio::select! {
        result = mcp_server.serve(rmcp::transport::stdio()) => {
            result.map_err(|e| crate::Error::McpServerStart(Box::new(e)))?
        }
        () = shutdown_signal.recv() => {
            tracing::info!("shutdown signal received during handshake, stopping stdio transport");
            return Ok(());
        }
    };

    tokio::select! {
        result = service.waiting() => result
            .map(|_| ())
            .map_err(|source| crate::Error::TaskFailed {
                task: crate::error::BackgroundTask::McpService,
                source,
            }),
        () = shutdown_signal.recv() => {
            tracing::info!("shutdown signal received, stopping stdio transport");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use crate::bridge::WorkspaceRoots;

    /// #241: `run_stdio` must not hang when the transport never even
    /// establishes — it must surface the failure promptly.
    ///
    /// This is the closest portable coverage of `run_stdio`'s non-signal
    /// path achievable here: `run_stdio` is hardcoded to the process's real
    /// stdin/stdout (no injectable transport), and this crate is
    /// `forbid(unsafe_code)`, so a test can't redirect the fd to simulate "the
    /// MCP handshake completes, *then* stdin closes" — the specific
    /// scenario that would drive `service.waiting()` to resolve inside the
    /// `tokio::select!` and hit its `Ok(())` arm. What a test *can* rely on:
    /// under `cargo nextest`, each test's stdin is already closed before the
    /// test body runs, so `mcp_server.serve(...)` fails during the initial
    /// `initialize` handshake — before `run_stdio` ever reaches the
    /// `select!`. That still exercises real production code (the `.serve()`
    /// call and its error mapping) and proves `run_stdio` returns promptly
    /// rather than hanging, which is what a broken `select!` (e.g. one
    /// missing a branch, or awaiting the wrong future) would look like.
    #[tokio::test]
    async fn test_run_stdio_returns_promptly_when_stdin_is_already_closed() {
        use std::sync::Arc;

        use tokio::sync::Mutex;

        use crate::bridge::{NotificationCache, Translator};
        use crate::config::McpConfig;
        use crate::mcp::{McplsServer, SubscriptionRegistry};

        let translator = Arc::new(Translator::new());
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let workspace_roots = WorkspaceRoots::default();
        let subs = SubscriptionRegistry::new();
        let server = McplsServer::new(
            translator,
            notification_cache,
            workspace_roots,
            subs,
            crate::ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::run_stdio(server, super::ShutdownSignal::new()),
        )
        .await;

        assert!(
            outcome.is_ok(),
            "run_stdio must not hang when stdin is already closed"
        );
        let result = outcome.unwrap();
        assert_matches!(
            result,
            Err(crate::Error::McpServerStart(_)),
            "expected a McpServerStart error from the failed handshake, got: {result:?}"
        );
    }
}
