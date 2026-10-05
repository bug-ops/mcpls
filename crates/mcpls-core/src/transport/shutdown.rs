//! Process shutdown-signal registration shared by the stdio and HTTP transports.

/// A registered handle for waiting on a shutdown signal: `SIGTERM`/`SIGINT`
/// on Unix (as sent by containers, systemd, and `Ctrl-C`) or `Ctrl-C` on
/// Windows.
///
/// Constructed once by [`crate::serve_with`], *before* any startup work
/// (LSP-server discovery heuristics, `spawn_lsp_servers_background`) runs,
/// and moved by value into whichever transport (`run_stdio`/`run_http`) ends
/// up serving. Registering this early — rather than inside the transport
/// function itself — closes the startup window between process start and the
/// transport loop, during which a signal would otherwise hit the OS's
/// default disposition (immediate termination, bypassing
/// [`crate::bridge::Translator::shutdown_servers`] and risking an orphaned
/// LSP child process that `spawn_lsp_servers_background` is mid-spawning;
/// see #270).
///
/// Every signal kind is held as its own persistent stream
/// (`tokio::signal::unix::Signal` / `tokio::signal::windows::CtrlC`) for the
/// lifetime of this value, rather than re-registered on every
/// [`ShutdownSignal::recv`] call via `tokio::signal::ctrl_c()`: a signal
/// delivered while a *specific* listener isn't being polled is only observed
/// by that same listener's next poll — a freshly (re-)subscribed one starts
/// at the broadcast's current version and never sees it (tokio
/// `signal/registry.rs`). Since [`recv`](ShutdownSignal::recv) is awaited
/// from more than one call site — both by [`run_stdio`](super::stdio::run_stdio), which races it
/// against the MCP handshake and then the post-handshake serve loop, and
/// across the gap between construction in `serve_with` and the first await
/// inside the transport — a fresh registration per call would risk losing a
/// signal delivered in between.
///
/// This instance is dropped as soon as the transport function it was moved
/// into returns — but that does *not* deregister the OS-level handler:
/// `tokio::signal` installs it once per process and never uninstalls it, no
/// matter how many `ShutdownSignal`s are constructed or dropped. What
/// dropping the last live instance actually does is remove the only
/// receiver a delivered signal could be broadcast to, so until a new one
/// subscribes, a signal is recorded and then silently discarded rather than
/// observed by anything — making that stretch of code uninterruptible
/// rather than unsafe. [`shutdown`](crate::runtime::shutdown::shutdown) (the post-transport cleanup run
/// immediately after) registers a *second* `ShutdownSignal` of its own so a
/// repeat signal during cleanup has a receiver again and can force an exit;
/// see #329.
pub struct ShutdownSignal {
    #[cfg(unix)]
    sigterm: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    sigint: Option<tokio::signal::unix::Signal>,
    #[cfg(windows)]
    ctrl_c: Option<tokio::signal::windows::CtrlC>,
}

impl ShutdownSignal {
    /// Registers the process's shutdown signal handler(s) up front.
    pub fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let sigterm = match signal(SignalKind::terminate()) {
                Ok(sigterm) => Some(sigterm),
                Err(e) => {
                    tracing::warn!(
                        "SIGTERM handler registration failed ({e}), SIGTERM will not be caught"
                    );
                    None
                }
            };
            let sigint = match signal(SignalKind::interrupt()) {
                Ok(sigint) => Some(sigint),
                Err(e) => {
                    tracing::warn!(
                        "SIGINT handler registration failed ({e}), SIGINT will not be caught"
                    );
                    None
                }
            };
            Self { sigterm, sigint }
        }
        #[cfg(windows)]
        {
            let ctrl_c = match tokio::signal::windows::ctrl_c() {
                Ok(ctrl_c) => Some(ctrl_c),
                Err(e) => {
                    tracing::warn!("Ctrl-C handler registration failed ({e})");
                    None
                }
            };
            Self { ctrl_c }
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self {}
        }
    }

    /// Waits for the next shutdown signal. May be awaited repeatedly.
    pub async fn recv(&mut self) {
        #[cfg(unix)]
        {
            match (self.sigterm.as_mut(), self.sigint.as_mut()) {
                (Some(sigterm), Some(sigint)) => {
                    tokio::select! {
                        _ = sigterm.recv() => {},
                        _ = sigint.recv() => {},
                    }
                }
                (Some(sigterm), None) => {
                    sigterm.recv().await;
                }
                (None, Some(sigint)) => {
                    sigint.recv().await;
                }
                (None, None) => {
                    // Both registrations failed above; fall back to a
                    // one-shot listener so shutdown is still possible, even
                    // though it doesn't carry the same across-calls
                    // durability the held streams above do (see the struct
                    // docs).
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
        #[cfg(windows)]
        {
            match self.ctrl_c.as_mut() {
                Some(ctrl_c) => {
                    ctrl_c.recv().await;
                }
                None => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            // No persistent listener is available on this platform; same
            // caveat as the Unix double-registration-failure fallback above.
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::ShutdownSignal;

    /// #329 regression: `crate::shutdown`'s cleanup-window fix hinges on a
    /// freshly constructed `ShutdownSignal` still receiving real OS signals
    /// after an *earlier* `ShutdownSignal` (e.g. the one `run_stdio`/
    /// `run_http` held) has already been dropped — proving there is no
    /// window in which the OS handler itself gets deregistered (per
    /// `ShutdownSignal`'s corrected struct doc: `tokio::signal` never
    /// uninstalls it, regardless of how many instances are constructed or
    /// dropped). Exercises this with a real self-sent `SIGTERM` via the
    /// external `kill` binary rather than mocking `ShutdownSignal`, since
    /// that's the exact mechanism `crate::shutdown`'s force-exit task relies
    /// on. Safe under `cargo nextest`'s one-process-per-test model, so no
    /// other test's signal disposition is affected.
    ///
    /// Deliberately does not go through `crate::shutdown` itself: a signal
    /// caught there unconditionally calls `std::process::exit(1)`, which
    /// would kill this test's own process for real rather than fail an
    /// assertion — see the #329 regression-test handoff for why a test
    /// triggering `std::process::exit(1)` isn't attempted here.
    ///
    /// Unix-only: `SIGTERM` and the external `kill` binary this test relies
    /// on don't exist on Windows, where `ShutdownSignal` listens for
    /// Ctrl-C instead (see the struct's `#[cfg(windows)]` arm above).
    #[tokio::test]
    async fn test_fresh_shutdown_signal_still_receives_sigterm_after_prior_instance_dropped() {
        let earlier = ShutdownSignal::new();
        drop(earlier);

        let mut cleanup_signal = ShutdownSignal::new();

        let pid = std::process::id();
        let signal_sender = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let status = std::process::Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status()
                .unwrap();
            assert!(status.success(), "`kill -TERM {pid}` must succeed");
        });

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), cleanup_signal.recv()).await;
        signal_sender.await.unwrap();

        assert!(
            result.is_ok(),
            "a freshly constructed ShutdownSignal must still receive a real SIGTERM sent after \
             an earlier instance was dropped — this is the exact mechanism crate::shutdown's \
             cleanup-window force-exit task depends on"
        );
    }
}
