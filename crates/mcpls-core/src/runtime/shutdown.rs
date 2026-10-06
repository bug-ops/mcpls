//! Post-transport shutdown: stops the pumps, drains every registered LSP
//! server, and bounds the wait for the background init task.

use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::bridge::Translator;
use crate::transport::ShutdownSignal;
use crate::util::AbortOnDrop;

/// Bounds how long [`shutdown`] waits for the background LSP init task
/// (see [`spawn_lsp_servers_background`](super::startup::spawn_lsp_servers_background)) to finish after cancellation is
/// signaled. Deliberately shorter than [`Translator`]'s own per-server
/// shutdown timeout: by the time `shutdown_servers` returns, every
/// registered server's notification channel has closed, so the init task's
/// diagnostics pumps should already be draining. This bound only matters
/// for the rarer case where the init task is still mid-`initialize` (never
/// registered anything for `shutdown_servers` to act on).
const LSP_INIT_TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounds the second wait, after `abort()`, for the init task's locals to drop.
const LSP_INIT_TASK_ABORT_GRACE: Duration = Duration::from_secs(1);

/// Awaits the background LSP init task's `JoinHandle` with a bounded
/// `timeout`, logging a panic at `error` level (previously dropped
/// silently, see #196) or an unresponsive task at `warn` level instead of
/// letting either go unnoticed.
///
/// `timeout` is a parameter (rather than always
/// [`LSP_INIT_TASK_SHUTDOWN_TIMEOUT`]) so tests can exercise the timeout
/// branch without waiting out the real bound. Awaits `handle` by `&mut`
/// (not by value): dropping an *owned* `JoinHandle` on timeout would only
/// detach the task — it keeps running rather than stopping, contradicting
/// the warning logged below. Retaining ownership lets `abort()` make that
/// message true.
///
/// `abort()` only *requests* cancellation; the task's locals (which may own
/// not-yet-registered `tokio::process::Child` handles for LSP servers
/// [`spawn_lsp_servers_background`](super::startup::spawn_lsp_servers_background) is still starting servers,
/// relying entirely on `kill_on_drop` to terminate them) are only actually
/// dropped once the runtime polls the task to completion. `mcpls-cli`'s
/// `main` calls `std::process::exit` right after `serve_with` returns (see
/// #308), which skips the executor's own task teardown that used to do this
/// polling implicitly — so this function awaits the aborted handle again,
/// bounded, to drive that drop here instead of leaving it to chance.
/// Otherwise a `SIGTERM` arriving mid-startup could orphan those LSP
/// child processes, the exact failure mode #270 was filed to prevent.
pub(super) async fn await_lsp_init_handle(handle: JoinHandle<()>, timeout: Duration) {
    await_lsp_init_handle_within(handle, timeout, LSP_INIT_TASK_ABORT_GRACE).await;
}

/// [`await_lsp_init_handle`] with the post-abort grace given, so a test can
/// shorten it.
async fn await_lsp_init_handle_within(
    mut handle: JoinHandle<()>,
    timeout: Duration,
    abort_grace: Duration,
) {
    match tokio::time::timeout(timeout, &mut handle).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => error!("Background LSP initialization task failed: {err}"),
        Err(_) => {
            warn!("Timed out waiting for background LSP initialization task to stop");
            handle.abort();
            if tokio::time::timeout(abort_grace, handle).await.is_err() {
                warn!("Background LSP initialization task did not stop after abort");
            }
        }
    }
}

/// Post-transport shutdown sequence, run once the transport future
/// (`run_stdio`/`run_http`) returns — whether that's because of a
/// `SIGTERM`/`SIGINT`, stdio EOF, or (for HTTP) its own graceful shutdown.
///
/// Signals background pump tasks to exit, then gracefully shuts down every
/// LSP server registered on `translator` (see
/// [`Translator::shutdown_servers`] for what "gracefully" bounds and falls
/// back to). Finally, if the background LSP init task (see
/// [`spawn_lsp_servers_background`](super::startup::spawn_lsp_servers_background)) is still running, awaits it via
/// [`await_lsp_init_handle`], giving its diagnostics pump tasks a chance to
/// finish draining before `serve_with` returns. Extracted from
/// [`serve_with`](crate::serve_with) so this sequence is exercised directly in tests without
/// needing a full stdio/HTTP transport round trip.
///
/// # Signal handling during cleanup (#329)
///
/// The OS-level `SIGTERM`/`SIGINT` handler installed by [`ShutdownSignal::new`]
/// stays installed for the rest of the process's life once registered —
/// `tokio::signal` never uninstalls it, regardless of how many [`ShutdownSignal`]
/// values are constructed or dropped. So dropping the instance built in
/// `serve_with` and moved into `run_stdio`/`run_http` (which happens as soon
/// as that transport function returns, right before this function runs)
/// does *not* reopen a window where a repeat signal could hit the OS's
/// default disposition. What it does instead: with no live [`ShutdownSignal`]
/// subscribed, a signal delivered during `shutdown_servers`/
/// `await_lsp_init_handle` (bounded by [`crate::lsp::SHUTDOWN_TIMEOUT`] and
/// [`LSP_INIT_TASK_SHUTDOWN_TIMEOUT`], ~15s worst case) is recorded and then
/// silently discarded — there is no receiver to broadcast it to. Before this
/// fix, that made cleanup **uninterruptible**: an operator's repeat
/// `Ctrl-C`/`SIGTERM` during that window was a no-op short of `SIGKILL`.
///
/// This function re-registers a fresh `ShutdownSignal` first thing to give
/// cleanup a listener again, restoring the ability to force-quit a stuck
/// cleanup on request. A brief gap remains between the old registration's
/// last live receiver dropping and this one subscribing, in which a signal
/// can still be discarded the same way as before the fix — see the
/// escalation behavior below for how that's bounded.
///
/// A signal caught here means "the operator wants out": the first one during
/// cleanup is logged and forces an immediate `std::process::exit(1)`, since
/// the graceful default (waiting out `shutdown_servers`'s bounded timeouts)
/// already had its chance before the operator intervened. This is
/// deliberately not lenient — because a signal in the re-registration gap
/// above is silently dropped rather than counted, requiring a second repeat
/// before acting would let an unlucky operator's second press go unnoticed
/// too. `exit(1)` skips unwinding, so
/// it forfeits `Drop` (`kill_on_drop`) and the graceful LSP `exit`, but any
/// still-running LSP child is killed by the lifeline/job binding (see
/// [`Translator::shutdown_servers`]'s "Limitations" section) — an explicit
/// trade the operator is asking for, not a case this fix silently regresses.
pub async fn shutdown(
    cancel_tx: &tokio::sync::watch::Sender<bool>,
    translator: &Translator,
    lsp_init_handle: Option<JoinHandle<()>>,
) {
    translator.begin_shutdown();
    let _ = cancel_tx.send(true);

    let mut cleanup_signal = ShutdownSignal::new();
    let force_exit_on_signal = tokio::spawn(async move {
        cleanup_signal.recv().await;
        error!("shutdown signal received during cleanup, forcing immediate exit");
        std::process::exit(1);
    });
    // Aborts `force_exit_on_signal` on every exit from this scope, including
    // an unwind out of `shutdown_servers().await` below — otherwise that path
    // would merely detach the task instead of stopping it, unlike the
    // equivalent abort-on-timeout handling in `await_lsp_init_handle`.
    let _abort_force_exit_on_signal = AbortOnDrop(&force_exit_on_signal);

    info!("Shutting down LSP servers...");
    translator.shutdown_servers().await;

    if let Some(handle) = lsp_init_handle {
        await_lsp_init_handle(handle, LSP_INIT_TASK_SHUTDOWN_TIMEOUT).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::bridge::Translator;

    /// #241: `serve_with`'s post-transport shutdown sequence must drain
    /// registered LSP servers rather than orphaning them. Exercises
    /// `shutdown()` directly (the exact code `serve_with` runs after its
    /// transport future returns) against a `Translator` with a real,
    /// registered `LspServer` — `serve_with` itself can't be driven
    /// through this path in a portable unit test, since it only
    /// registers a server after a successful LSP `initialize` handshake,
    /// which requires a real language server binary.
    #[tokio::test]
    async fn test_shutdown_drains_registered_lsp_server() {
        let translator = Translator::new();
        translator.register_server("fake-server", crate::lsp::fake_lsp_server());
        assert_eq!(translator.registered_server_count(), 1);

        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            super::shutdown(&cancel_tx, &translator, None),
        )
        .await;

        assert!(
            result.is_ok(),
            "shutdown must not hang against a non-responsive mock LSP server"
        );
        assert_eq!(
            translator.registered_server_count(),
            0,
            "shutdown must drain every registered LSP server"
        );
        assert!(
            *cancel_rx.borrow(),
            "shutdown must signal background pump tasks to exit"
        );
    }

    /// #196: `shutdown` must await the background LSP init task's
    /// `JoinHandle` (rather than leaving it detached) so a panic inside
    /// it surfaces as an `error!` log instead of being silently dropped.
    #[tokio::test]
    async fn test_shutdown_awaits_background_init_task() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let translator = Translator::new();
        let (cancel_tx, _cancel_rx) = tokio::sync::watch::channel(false);

        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = Arc::clone(&completed);
        let handle = tokio::spawn(async move {
            completed_clone.store(true, Ordering::SeqCst);
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            super::shutdown(&cancel_tx, &translator, Some(handle)),
        )
        .await;

        assert!(result.is_ok(), "shutdown must not hang on a live handle");
        assert!(
            completed.load(Ordering::SeqCst),
            "shutdown must await the background init task before returning"
        );
    }

    /// A timed-out background init task must actually be stopped
    /// (`JoinHandle::abort`), not merely detached: awaiting the handle
    /// *by value* inside `tokio::time::timeout` would drop only the
    /// `JoinHandle` on timeout, which detaches the task without
    /// cancelling it — it keeps running (and its future is never
    /// dropped) despite the "timed out waiting ... to stop" log.
    ///
    /// Tests `await_lsp_init_handle` directly with a millisecond-scale
    /// `timeout` (rather than going through `shutdown` with the real
    /// multi-second `LSP_INIT_TASK_SHUTDOWN_TIMEOUT`) so this stays
    /// fast. A `completed`-style flag set at the end of the task
    /// couldn't tell "aborted" from "merely detached" apart here either
    /// way, since the task hasn't finished its (deliberately long)
    /// sleep yet in both cases — so this uses a `Drop`-signaling guard
    /// held across the `.await` instead: `abort()` drops the task's
    /// future promptly (well inside the grace period below), while a
    /// detached-but-still-running task would only drop it once its
    /// sleep actually finishes.
    #[tokio::test]
    async fn test_await_lsp_init_handle_aborts_on_timeout() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let future_dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(Arc::clone(&future_dropped));
        let handle = tokio::spawn(async move {
            let _guard = guard;
            // Far longer than the timeout below, so it only elapses if
            // the task is genuinely aborted rather than left running.
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });

        super::await_lsp_init_handle(handle, std::time::Duration::from_millis(20)).await;

        // Give the just-aborted task's cancellation a moment to land.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            future_dropped.load(Ordering::SeqCst),
            "timed-out background init task's future must be dropped via abort(), \
             not left running detached until its own sleep completes"
        );
    }

    /// An init task that ignores `abort()` (a blocking task cannot be
    /// cancelled) must be reported, not forgotten after the second wait.
    #[tokio::test]
    async fn test_await_lsp_init_handle_logs_task_that_ignores_abort() {
        use tracing_subscriber::layer::SubscriberExt as _;

        use crate::test_lsp::CapturedLogs;

        let (release, released) = std::sync::mpsc::channel::<()>();
        let handle = tokio::task::spawn_blocking(move || {
            let _ = released.recv();
        });

        let captured = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);

        let grace = Duration::from_millis(20);
        super::await_lsp_init_handle_within(handle, grace, grace).await;

        drop(guard);
        drop(release);

        assert!(
            captured
                .messages()
                .iter()
                .any(|m| m.contains("did not stop after abort")),
            "expected a warn! log for the task that outlived the abort grace"
        );
    }

    /// #196: a panicking background init task must not hang or crash
    /// `shutdown`, and the panic must actually be logged (not merely
    /// swallowed while `shutdown` happens not to hang for other
    /// reasons) — asserted via a captured `tracing` event rather than
    /// just checking completion.
    #[tokio::test]
    async fn test_await_lsp_init_handle_logs_panic() {
        use tracing_subscriber::layer::SubscriberExt as _;

        use crate::test_lsp::CapturedLogs;

        let handle = tokio::spawn(async {
            panic!("simulated background LSP init panic");
        });

        let captured = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);

        super::await_lsp_init_handle(handle, std::time::Duration::from_secs(5)).await;

        drop(guard);

        let messages = captured.messages();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("Background LSP initialization task failed")),
            "expected an error! log for the panicking background init task, got: {messages:?}"
        );
    }
}
