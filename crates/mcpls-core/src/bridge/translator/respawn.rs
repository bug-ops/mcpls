//! Dead-server detection and respawn-backoff bookkeeping.
//!
//! Tracks consecutive respawn failures per server so a crash-looping
//! process backs off exponentially instead of eating a fresh
//! `timeout_seconds` on every tool call that arrives while it is down.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use tokio::time::Duration;

use super::Translator;
use crate::bridge::lock_std;
use crate::config::ServerId;
use crate::error::{Error, Result};
use crate::lsp::LspServer;

/// Tracks respawn attempts for one server, so [`Translator::respawn_if_dead`]
/// can back off a crash-looping process instead of retrying it on every
/// single tool call.
#[derive(Debug, Clone, Copy)]
pub(super) struct RespawnBackoff {
    /// Number of consecutive attempts that have not produced a server which
    /// stayed alive for at least [`RESPAWN_BACKOFF_BASE`]. A spawn failure
    /// counts immediately; a spawn that succeeds but is found dead again
    /// within that window counts too, once that is discovered -- see
    /// [`Translator::reconcile_respawn_stability`]. Without this, a server
    /// that starts, completes `initialize`, and then crashes a second later
    /// (a common real crash-loop shape) would bypass backoff entirely: each
    /// "success" would otherwise look like a fresh, unbacked-off start.
    consecutive_failures: u32,
    /// When the most recent attempt was made, or (if `last_attempt_succeeded`)
    /// when that success was last found to have not held up.
    last_attempt: Instant,
    /// Whether the most recent attempt completed `initialize` successfully.
    /// `false` for an outright spawn failure. Also reset to `false` once a
    /// "successful" respawn is found to have died again within the
    /// stability window, so that discovery is applied only once.
    last_attempt_succeeded: bool,
}

/// Base delay before the first backed-off retry after a respawn failure.
const RESPAWN_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Upper bound on the exponential backoff delay between respawn attempts.
const RESPAWN_BACKOFF_MAX: Duration = Duration::from_secs(30);

impl Translator {
    /// Whether the server tracked under `id` is registered and has exited.
    ///
    /// Returns `false` ("not dead") for an `id` that isn't registered at
    /// all -- that's the separate `ServerInitializing`/`NoServerForTool`
    /// concern callers already handle, not something the respawn path
    /// should react to -- and for any `try_wait` error, on the conservative
    /// assumption that a health check that itself failed should not trigger
    /// a respawn.
    fn is_server_dead(&self, id: &ServerId) -> bool {
        lock_std(&self.lsp_servers)
            .get_mut(id)
            .and_then(|server| server.has_exited().ok())
            .unwrap_or(false)
    }

    /// Return the shared single-flight lock for `id`, creating it on first
    /// use.
    ///
    /// Two concurrent callers racing to respawn the same server both get a
    /// clone of the *same* underlying `Mutex`, so awaiting it actually
    /// serializes them instead of letting both proceed independently.
    fn respawn_lock(&self, id: &ServerId) -> Arc<Mutex<()>> {
        Arc::clone(
            lock_std(&self.respawn_locks)
                .entry(id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Remaining backoff delay before `id` may be respawned again, or
    /// `None` if it may be attempted right now.
    ///
    /// Only consults recorded *failures* -- a server with no recorded
    /// attempt is never backed off. A server whose last attempt "succeeded"
    /// is reconciled by [`Self::reconcile_respawn_stability`] (called by
    /// [`Self::respawn_if_dead`] before this) into either a failure (died
    /// again too soon) or removed entirely (proven stable), so by the time
    /// this runs, a lingering "succeeded" entry never reaches here.
    fn respawn_backoff_remaining(&self, id: &ServerId) -> Option<Duration> {
        let (consecutive_failures, last_attempt) = {
            let entry = lock_std(&self.respawn_backoffs).get(id).copied()?;
            (entry.consecutive_failures, entry.last_attempt)
        };
        if consecutive_failures == 0 {
            return None;
        }
        let shift = consecutive_failures.saturating_sub(1).min(5);
        let delay = RESPAWN_BACKOFF_BASE
            .saturating_mul(1 << shift)
            .min(RESPAWN_BACKOFF_MAX);
        let elapsed = self.clock.now().saturating_duration_since(last_attempt);
        (elapsed < delay).then(|| delay.saturating_sub(elapsed))
    }

    /// Records a failed respawn attempt for `id`, extending its backoff.
    fn record_respawn_failure(&self, id: &ServerId) {
        let mut backoffs = lock_std(&self.respawn_backoffs);
        let entry = backoffs
            .entry(id.clone())
            .or_insert_with(|| RespawnBackoff {
                consecutive_failures: 0,
                last_attempt: self.clock.now(),
                last_attempt_succeeded: false,
            });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.last_attempt = self.clock.now();
        entry.last_attempt_succeeded = false;
        drop(backoffs);
    }

    /// Records that a respawn attempt for `id` completed `initialize`
    /// successfully.
    ///
    /// Does *not* clear `consecutive_failures`: whether this attempt
    /// actually broke the crash loop is only known once the server either
    /// stays alive for a while or is found dead again -- see
    /// [`Self::reconcile_respawn_stability`], which is what acts on this
    /// entry.
    fn record_respawn_success(&self, id: &ServerId) {
        let mut backoffs = lock_std(&self.respawn_backoffs);
        let entry = backoffs
            .entry(id.clone())
            .or_insert_with(|| RespawnBackoff {
                consecutive_failures: 0,
                last_attempt: self.clock.now(),
                last_attempt_succeeded: true,
            });
        entry.last_attempt = self.clock.now();
        entry.last_attempt_succeeded = true;
        drop(backoffs);
    }

    /// Reconciles `id`'s backoff state against a *newly observed* death,
    /// before deciding whether to back off this respawn attempt.
    ///
    /// A no-op unless the last recorded attempt "succeeded" ([`Self::record_respawn_success`]):
    /// - If it has since survived at least [`RESPAWN_BACKOFF_BASE`], it is
    ///   treated as proven stable and its backoff state is cleared -- a
    ///   later, unrelated crash starts a fresh backoff sequence rather than
    ///   inheriting history from a long-resolved incident.
    /// - Otherwise, the server died again before proving itself: this
    ///   counts as a failure (extending `consecutive_failures`) instead of
    ///   being silently forgotten. Without this, a server that starts,
    ///   completes `initialize`, and crashes again a moment later would
    ///   bypass backoff entirely -- every such cycle would look like a
    ///   fresh, unbacked-off start, spawning one child process per tool
    ///   call forever.
    fn reconcile_respawn_stability(&self, id: &ServerId) {
        let Some(entry) = lock_std(&self.respawn_backoffs).get(id).copied() else {
            return;
        };
        if !entry.last_attempt_succeeded {
            return;
        }
        if self
            .clock
            .now()
            .saturating_duration_since(entry.last_attempt)
            >= RESPAWN_BACKOFF_BASE
        {
            lock_std(&self.respawn_backoffs).remove(id);
        } else {
            let mut backoffs = lock_std(&self.respawn_backoffs);
            if let Some(current) = backoffs.get_mut(id) {
                current.consecutive_failures = current.consecutive_failures.saturating_add(1);
                current.last_attempt = self.clock.now();
                current.last_attempt_succeeded = false;
            }
        }
    }

    /// Detect whether the server routed to `id` has crashed and, if so,
    /// eagerly respawn and re-initialize it before returning.
    ///
    /// A no-op if `id` names a server that was never registered (routing
    /// resolved to it, but it hasn't started yet or never will) or is still
    /// alive.
    ///
    /// # Concurrency
    ///
    /// Multiple callers can race in here for the same `id` -- e.g. two tool
    /// calls landing back-to-back right after the process dies. They
    /// single-flight on [`Self::respawn_lock`]: the first to acquire it
    /// performs the actual respawn; everyone else waits for that attempt to
    /// finish (or fail), rechecks, and finds nothing left to do.
    ///
    /// Requests still parked in the dead client's `pending_requests` are
    /// failed immediately via [`LspClient::fail_pending_requests`] instead
    /// of being left to time out on their own.
    ///
    /// The respawned process has no memory of any document the old one had
    /// open, so this also clears `document_tracker`'s per-server sync
    /// history for `id` -- otherwise `ensure_open` would send `didChange`
    /// instead of `didOpen` for a document the new process never saw. Any
    /// diagnostics cached from the old connection are invalidated (see
    /// [`Self::with_notification_cache`]) rather than left to be merged into
    /// fresh pulls as if still current.
    ///
    /// Diagnostics and other push notifications from the new process itself
    /// are drained and discarded rather than wired into the existing pump
    /// task: the pump's remaining dependencies (resource subscriptions, peer
    /// handle) live in `serve_with`'s scope, not the translator's, so
    /// reconnecting live push for a respawned server is out of scope for
    /// this fix -- it does not resume until the whole mcpls process
    /// restarts, but stale data is no longer served as current.
    ///
    /// A crash-looping server (repeated respawn failures) backs off
    /// exponentially (`RESPAWN_BACKOFF_BASE` up to `RESPAWN_BACKOFF_MAX`)
    /// instead of retrying on every single tool call, each of which would
    /// otherwise cost up to a full `timeout_seconds` inside `initialize`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ServerUnavailable`] if no respawn config was ever
    /// registered for `id`, or if it is currently within its backoff
    /// window. Returns whatever error `LspServer::spawn` produced (e.g. its
    /// command is no longer on `PATH`, or `initialize` fails again) if an
    /// actual respawn attempt failed.
    pub(super) async fn respawn_if_dead(&self, id: &ServerId) -> Result<()> {
        if !self.is_server_dead(id) {
            return Ok(());
        }

        let lock = self.respawn_lock(id);
        let _guard = lock.lock().await;

        // Another caller may have already respawned it while we waited.
        if !self.is_server_dead(id) {
            return Ok(());
        }

        self.reconcile_respawn_stability(id);

        if let Some(remaining) = self.respawn_backoff_remaining(id) {
            tracing::warn!(
                "LSP server '{id}' is crash-looping, backing off for {remaining:?} \
                 before the next respawn attempt"
            );
            return Err(Error::ServerUnavailable {
                server_id: id.clone(),
                reason: format!("crash-looping, retry in {remaining:?}"),
            });
        }

        let Some(config) = lock_std(&self.server_configs).get(id).cloned() else {
            return Err(Error::ServerUnavailable {
                server_id: id.clone(),
                reason: "no respawn config registered for this server".to_string(),
            });
        };
        let language_id = config.server_config.language_id.clone();

        tracing::warn!("LSP server '{id}' has crashed, respawning");
        let mut new_server = match LspServer::spawn(config).await {
            Ok(server) => {
                self.record_respawn_success(id);
                server
            }
            Err(err) => {
                self.record_respawn_failure(id);
                return Err(err);
            }
        };
        let new_client = new_server.client().clone();
        let mut notification_rx = new_server.take_notification_rx();
        tokio::spawn(async move { while notification_rx.recv().await.is_some() {} });

        let old_client = lock_std(&self.lsp_clients).insert(id.clone(), new_client);
        let old_server = lock_std(&self.lsp_servers).insert(id.clone(), new_server);
        drop(old_server); // dropped after the `lsp_servers` guard, not under it

        self.document_tracker.forget_server(id);

        // Only the diagnostics-route server for this language ever writes
        // to the cache (see `diagnostics_pump`'s `caches_diagnostics` gate
        // in the crate root) -- clearing a non-route server's synced URIs
        // would delete the *healthy* diagnostics server's valid entries for
        // those same files instead. And the route server's own cache
        // entries are not limited to documents mcpls ever opened (it
        // publishes workspace-wide, e.g. `cargo check` diagnostics), so a
        // per-URI clear scoped to synced documents would miss most of what
        // needs invalidating.
        //
        // `clear_server_diagnostics` scopes the clear to just this server's
        // own entries, tracked via `NotificationCache`'s per-server
        // ownership map (#266) -- a crashed rust-analyzer no longer wipes a
        // healthy pyright's cached diagnostics for Python files in the same
        // workspace.
        //
        // This clear is not atomic with the swap above: a caller that reads
        // `lsp_clients` between the swap and this point sees the new client
        // and could read a not-yet-cleared cache entry. In practice
        // `handle_diagnostics` only reads the cache after a full LSP pull
        // round-trip, so this window is negligible.
        if self.is_diagnostics_route(&language_id, id)
            && let Some(cache) = &self.notification_cache
        {
            cache.lock().await.clear_server_diagnostics(id);
        }

        if let Some(old_client) = old_client {
            old_client.fail_pending_requests().await;
        }

        tracing::info!("LSP server '{id}' respawned successfully");
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::bridge::translator::clock::{Clock, FakeClock};
    use crate::config::ServerId;

    #[test]
    fn test_respawn_backoff_remaining_returns_none_once_delay_elapsed() {
        let clock = Arc::new(FakeClock::new());
        let translator = Translator::new().with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        let id = ServerId::from("rust");

        translator.record_respawn_failure(&id);
        assert!(
            translator.respawn_backoff_remaining(&id).is_some(),
            "immediately after a failure, the backoff window must still be active"
        );

        clock.advance(RESPAWN_BACKOFF_MAX);
        assert!(
            translator.respawn_backoff_remaining(&id).is_none(),
            "once the fake clock has advanced past the computed delay, \
             the backoff window must be reported as elapsed"
        );
    }

    #[test]
    fn test_reconcile_respawn_stability_clears_backoff_after_proven_stable() {
        let clock = Arc::new(FakeClock::new());
        let translator = Translator::new().with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        let id = ServerId::from("rust");

        translator.record_respawn_failure(&id);
        translator.record_respawn_success(&id);
        assert!(
            lock_std(&translator.respawn_backoffs).contains_key(&id),
            "a recorded success must still leave a backoff entry pending reconciliation"
        );

        clock.advance(RESPAWN_BACKOFF_BASE);
        translator.reconcile_respawn_stability(&id);

        assert!(
            !lock_std(&translator.respawn_backoffs).contains_key(&id),
            "once proven stable (survived at least RESPAWN_BACKOFF_BASE), \
             the backoff entry must be cleared entirely"
        );
    }

    // These three are pure logic (no process spawning), so they run on
    // every platform rather than being swept under `respawn_tests`'s
    // `#[cfg(unix)]` gate below -- otherwise Windows CI would have zero
    // #249 coverage at all.
    #[test]
    fn test_respawn_lock_is_shared_across_lookups_for_same_id() {
        let translator = Translator::new();
        let id = ServerId::from("rust");

        let first = translator.respawn_lock(&id);
        let second = translator.respawn_lock(&id);

        assert!(
            Arc::ptr_eq(&first, &second),
            "two lookups for the same id must return the same underlying lock, \
             otherwise concurrent respawns would not actually be serialized"
        );
    }

    #[test]
    fn test_respawn_lock_differs_across_ids() {
        let translator = Translator::new();

        let rust_lock = translator.respawn_lock(&ServerId::from("rust"));
        let python_lock = translator.respawn_lock(&ServerId::from("python"));

        assert!(!Arc::ptr_eq(&rust_lock, &python_lock));
    }

    #[test]
    fn test_is_server_dead_false_when_not_registered() {
        let translator = Translator::new();
        assert!(!translator.is_server_dead(&ServerId::from("rust")));
    }

    // Gated `#[cfg(unix)]`: this module's fake-LSP-server test double is a
    // hand-written `sh` script (POSIX parameter expansion, `printf`-framed
    // LSP responses, file-based invocation counters), which has no
    // equivalent on Windows. CI's "Test (unit)" job matrix includes
    // `windows-latest`.
    #[cfg(unix)]
    mod respawn_tests {
        use std::collections::HashMap;
        use std::fs;
        use std::path::{Path, PathBuf};

        use tempfile::TempDir;
        use tokio::time::Duration;

        use super::*;
        use crate::config::{LspServerConfig, ToolKind, ToolRouter};
        use crate::lsp::ServerInitConfig;

        /// Writes a `sh` script that answers the LSP `initialize` handshake
        /// with a canned response -- request id `1`, since a freshly spawned
        /// `LspClient`'s request counter always starts there -- and then
        /// exits shortly after, so `LspServer::spawn` succeeds but the
        /// process is already dead moments later. Stands in for "the server
        /// was alive, then crashed" without needing a real language server
        /// binary.
        ///
        /// The brief sleep before exiting matters: `LspServer::spawn` sends
        /// the `initialized` notification right after the `initialize`
        /// response arrives, and without it the process can (racily) have
        /// already exited by the time that notification is written to its
        /// stdin, failing the spawn itself instead of the respawn this is
        /// meant to seed.
        fn write_crash_after_init_script(dir: &Path) -> PathBuf {
            let script_path = dir.join("crash_after_init.sh");
            let body = r#"body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
sleep 0.3
"#;
            fs::write(&script_path, body).unwrap();
            script_path
        }

        /// Like [`write_crash_after_init_script`], but stays alive for
        /// `sleep_secs` after responding instead of exiting immediately.
        fn write_responder_script(dir: &Path, sleep_secs: u64) -> PathBuf {
            let script_path = dir.join("responder.sh");
            let template = r#"body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
sleep __SLEEP__
"#;
            fs::write(
                &script_path,
                template.replace("__SLEEP__", &sleep_secs.to_string()),
            )
            .unwrap();
            script_path
        }

        fn stub_server_config(id: &str, script: &Path) -> ServerInitConfig {
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: id.to_string(),
                    command: "sh".to_string(),
                    args: vec![script.to_string_lossy().to_string()],
                    env: HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 5,
                    request_timeout_seconds: 5,
                    heuristics: None,
                    name: Some(id.to_string()),
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
                position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                notification_tx: None,
            }
        }

        /// Polls `is_server_dead` until it reports `true`, bounding the wait
        /// so a broken script fails the test instead of hanging it.
        async fn wait_until_dead(translator: &Translator, id: &ServerId) {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if translator.is_server_dead(id) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("seed server never reported as exited");
        }

        #[tokio::test]
        async fn test_respawn_if_dead_noop_when_server_alive() {
            let dir = TempDir::new().unwrap();
            let script = write_responder_script(dir.path(), 1);
            let id = ServerId::from("rust");
            let config = stub_server_config("rust", &script);

            let server = LspServer::spawn(config).await.unwrap();
            let translator = Translator::new();
            translator.register_client(id.clone(), server.client().clone());
            translator.register_server(id.clone(), server);
            // Deliberately no `register_server_config`: if a respawn were
            // (wrongly) attempted despite the server being alive, the
            // missing config would surface as `Error::ServerUnavailable`
            // instead of quietly succeeding -- so `Ok(())` here is proof
            // the alive fast path skipped respawning entirely.

            assert!(translator.respawn_if_dead(&id).await.is_ok());
        }

        #[tokio::test]
        async fn test_respawn_if_dead_errors_when_no_config_registered() {
            let dir = TempDir::new().unwrap();
            let script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let config = stub_server_config("rust", &script);

            let server = LspServer::spawn(config).await.unwrap();
            let translator = Translator::new();
            translator.register_client(id.clone(), server.client().clone());
            translator.register_server(id.clone(), server);
            wait_until_dead(&translator, &id).await;

            let err = translator.respawn_if_dead(&id).await.unwrap_err();
            assert!(
                matches!(err, Error::ServerUnavailable { .. }),
                "got {err:?}"
            );
        }

        #[tokio::test]
        async fn test_respawn_if_dead_propagates_spawn_failure() {
            let dir = TempDir::new().unwrap();
            let script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &script);

            let server = LspServer::spawn(seed_config).await.unwrap();
            let translator = Translator::new();
            translator.register_client(id.clone(), server.client().clone());
            translator.register_server(id.clone(), server);
            wait_until_dead(&translator, &id).await;

            let mut broken = stub_server_config("rust", &script);
            broken.server_config.command = "nonexistent-lsp-cmd-xyz".to_string();
            translator.register_server_config(id.clone(), broken);

            let err = translator.respawn_if_dead(&id).await.unwrap_err();
            assert!(
                matches!(err, Error::ServerSpawnFailed { .. }),
                "got {err:?}"
            );
        }

        /// #249: two concurrent tool calls that both observe the same dead
        /// server must not each perform their own respawn -- only one
        /// replacement process should ever be spawned, and both callers
        /// must still resolve successfully.
        ///
        /// The fake server script counts every invocation and, on its
        /// first run only, exits right after answering `initialize`
        /// (simulating "was alive, then crashed"); every later invocation
        /// answers and then sleeps, standing in for a healthy replacement.
        /// If single-flighting were broken, both concurrent callers would
        /// spawn their own replacement and the invocation count would be
        /// 3 (seed + two independent respawns) instead of 2 (seed + one
        /// shared respawn).
        #[tokio::test]
        async fn test_respawn_if_dead_single_flights_concurrent_callers() {
            let dir = TempDir::new().unwrap();
            let marker = dir.path().join("marker");
            let counter = dir.path().join("invocations");
            let script_path = dir.path().join("flaky.sh");
            let template = r#"echo x >> "__COUNTER__"
if [ -f "__MARKER__" ]; then
  body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
  printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
  sleep 1
else
  touch "__MARKER__"
  body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
  printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
  sleep 0.3
fi
"#;
            let script_body = template
                .replace("__COUNTER__", &counter.display().to_string())
                .replace("__MARKER__", &marker.display().to_string());
            fs::write(&script_path, script_body).unwrap();

            let id = ServerId::from("rust");
            let config = stub_server_config("rust", &script_path);

            let seed = LspServer::spawn(config.clone()).await.unwrap();
            let translator = Arc::new(Translator::new());
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            translator.register_server_config(id.clone(), config);
            wait_until_dead(&translator, &id).await;

            let (t1, id1) = (Arc::clone(&translator), id.clone());
            let (t2, id2) = (Arc::clone(&translator), id.clone());
            let (r1, r2) = tokio::join!(
                tokio::spawn(async move { t1.respawn_if_dead(&id1).await }),
                tokio::spawn(async move { t2.respawn_if_dead(&id2).await }),
            );
            assert!(r1.unwrap().is_ok());
            assert!(r2.unwrap().is_ok());

            let invocations = fs::read_to_string(&counter).unwrap();
            assert_eq!(
                invocations.lines().count(),
                2,
                "expected exactly one seed spawn + one single-flighted \
                 respawn, got:\n{invocations}"
            );
        }

        /// #249 S2 regression: a second `respawn_if_dead` call within the
        /// backoff window must fail fast via `Error::ServerUnavailable`
        /// instead of repeating a real spawn attempt -- proven by the
        /// *kind* of error changing between the two calls, not by timing:
        /// the first call's failure is the genuine `LspServer::spawn` error
        /// (`Error::ServerSpawnFailed`, from a command that does not
        /// exist), and the second, immediately following, is the distinct
        /// backoff error.
        #[tokio::test]
        async fn test_respawn_if_dead_backs_off_after_repeated_failure() {
            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();
            let clock = Arc::new(FakeClock::new());
            let translator = Translator::new().with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            wait_until_dead(&translator, &id).await;

            let mut broken = stub_server_config("rust", &seed_script);
            broken.server_config.command = "nonexistent-lsp-cmd-xyz".to_string();
            translator.register_server_config(id.clone(), broken);

            let err1 = translator.respawn_if_dead(&id).await.unwrap_err();
            assert!(
                matches!(err1, Error::ServerSpawnFailed { .. }),
                "first attempt should be a real (failed) spawn, got {err1:?}"
            );

            let err2 = translator.respawn_if_dead(&id).await.unwrap_err();
            assert!(
                matches!(err2, Error::ServerUnavailable { .. }),
                "second call within the backoff window must fail fast \
                 without attempting another real spawn, got {err2:?}"
            );
        }

        /// #292 regression: once the backoff window has elapsed, the next
        /// `respawn_if_dead` call must actually attempt a fresh respawn
        /// instead of continuing to fail fast -- proven by swapping in a
        /// config that succeeds and observing `Ok(())`, not merely a
        /// different error kind.
        #[tokio::test]
        async fn test_respawn_if_dead_reattempts_once_backoff_window_elapses() {
            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();
            let clock = Arc::new(FakeClock::new());
            let translator = Translator::new().with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            wait_until_dead(&translator, &id).await;

            let mut broken = stub_server_config("rust", &seed_script);
            broken.server_config.command = "nonexistent-lsp-cmd-xyz".to_string();
            translator.register_server_config(id.clone(), broken);

            let err1 = translator.respawn_if_dead(&id).await.unwrap_err();
            assert!(
                matches!(err1, Error::ServerSpawnFailed { .. }),
                "first attempt should be a real (failed) spawn, got {err1:?}"
            );

            let err2 = translator.respawn_if_dead(&id).await.unwrap_err();
            assert!(
                matches!(err2, Error::ServerUnavailable { .. }),
                "second call within the backoff window must still fail fast, got {err2:?}"
            );

            // Advance well past the computed backoff delay and swap in a
            // config that will actually succeed this time.
            clock.advance(RESPAWN_BACKOFF_MAX);
            let working_script = write_crash_after_init_script(dir.path());
            translator
                .register_server_config(id.clone(), stub_server_config("rust", &working_script));

            let result = translator.respawn_if_dead(&id).await;
            assert!(
                result.is_ok(),
                "once the backoff window has elapsed, respawn_if_dead must actually \
                 reattempt a respawn instead of continuing to short-circuit, got {result:?}"
            );
        }

        /// #249 R3 regression: a respawn that *succeeds* (completes
        /// `initialize`) but dies again almost immediately must still
        /// engage backoff -- this is the more realistic crash-loop shape
        /// (start, initialize, then OOM-die a second later) than an
        /// outright spawn failure, and without this fix every such cycle
        /// looked like a fresh, unbacked-off start, spawning one child
        /// process per tool call forever.
        #[tokio::test]
        async fn test_respawn_if_dead_backs_off_after_quick_recrash_following_success() {
            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();
            let clock = Arc::new(FakeClock::new());
            let translator = Translator::new().with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            wait_until_dead(&translator, &id).await;

            // Reuse the same crash-after-init script as the respawn target:
            // every attempt completes `initialize` successfully, then dies
            // ~0.3s later -- a post-init crash loop, not a spawn failure.
            translator.register_server_config(id.clone(), stub_server_config("rust", &seed_script));

            translator
                .respawn_if_dead(&id)
                .await
                .expect("the replacement completes initialize, so this attempt succeeds");
            wait_until_dead(&translator, &id).await;

            let err = translator.respawn_if_dead(&id).await.unwrap_err();
            assert!(
                matches!(err, Error::ServerUnavailable { .. }),
                "a respawn that dies again within the stability window must \
                 back off instead of being treated as a fresh attempt, got {err:?}"
            );
        }

        /// #249 C1 regression: respawning the *diagnostics-route* server
        /// for a language must invalidate that server's diagnostics cache
        /// entries, rather than leaving stale entries to be merged into
        /// fresh pull results as if still current -- the crashed process's
        /// pump is gone and will never update or clear them itself.
        ///
        /// Covers the "under-clear" failure mode a scoped-to-synced-URIs
        /// clear has: a real diagnostics-route server (e.g. rust-analyzer)
        /// publishes workspace-wide (`cargo check` results for files never
        /// opened through mcpls), so `never_opened_uri` below stands in for
        /// an entry that must still be cleared despite never having gone
        /// through `ensure_open`.
        ///
        /// #266 S2 regression (over-clear direction, multi-language case):
        /// `other_language_uri` is owned by a *different* diagnostics-route
        /// server (e.g. pyright for Python, in the same workspace as the
        /// rust-analyzer under test here) and must survive -- `clear_server_diagnostics`
        /// replaced a workspace-wide `clear_all_diagnostics` that used to
        /// wipe every language's cache on any single server's respawn.
        #[tokio::test]
        async fn test_respawn_if_dead_clears_diagnostics_cache_when_diagnostics_route() {
            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();

            let cache = Arc::new(Mutex::new(crate::bridge::NotificationCache::new()));
            let translator = Translator::new()
                .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]))
                .with_notification_cache(Arc::clone(&cache));
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);

            let synced_uri: lsp_types::Uri = "file:///workspace/opened.rs".parse().unwrap();
            let never_opened_uri: lsp_types::Uri =
                "file:///workspace/never_opened.rs".parse().unwrap();
            let other_language_uri: lsp_types::Uri = "file:///workspace/main.py".parse().unwrap();
            cache
                .lock()
                .await
                .store_diagnostics(&id, &synced_uri, None, vec![]);
            cache
                .lock()
                .await
                .store_diagnostics(&id, &never_opened_uri, None, vec![]);
            cache.lock().await.store_diagnostics(
                &ServerId::from("python"),
                &other_language_uri,
                None,
                vec![],
            );

            wait_until_dead(&translator, &id).await;

            let respawn_script = write_responder_script(dir.path(), 1);
            translator
                .register_server_config(id.clone(), stub_server_config("rust", &respawn_script));

            translator.respawn_if_dead(&id).await.unwrap();

            let guard = cache.lock().await;
            assert!(
                guard.get_diagnostics(synced_uri.as_str()).is_none(),
                "diagnostics attributed to the crashed connection must be \
                 invalidated on respawn, not served as current"
            );
            assert!(
                guard.get_diagnostics(never_opened_uri.as_str()).is_none(),
                "workspace-wide diagnostics for a file mcpls never opened \
                 must also be invalidated, not just synced documents"
            );
            assert!(
                guard.get_diagnostics(other_language_uri.as_str()).is_some(),
                "a different diagnostics-route server's entries must survive \
                 an unrelated server's respawn-triggered cache clear"
            );
            drop(guard);
        }

        /// #249 C1 regression (over-clear direction): respawning a server
        /// that is *not* the diagnostics route for its language must not
        /// touch the cache at all -- otherwise a crashed hover-only server
        /// would wipe out a healthy, still-running diagnostics server's
        /// valid entries for the same files.
        #[tokio::test]
        async fn test_respawn_if_dead_does_not_clear_cache_when_not_diagnostics_route() {
            use crate::config::LspServerConfig;

            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let hover_id = ServerId::from("hover-only");
            let hover_seed_config = stub_server_config("hover-only", &seed_script);

            let seed = LspServer::spawn(hover_seed_config).await.unwrap();

            // `hover_id` handles only Hover; a separate (never-registered
            // here, purely routing-table) server is the catch-all and thus
            // the diagnostics route.
            let configs = [
                LspServerConfig {
                    language_id: "rust".to_string(),
                    command: "sh".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 5,
                    request_timeout_seconds: 5,
                    heuristics: None,
                    name: Some("hover-only".to_string()),
                    handles: Some(vec![ToolKind::Hover]),
                },
                LspServerConfig {
                    language_id: "rust".to_string(),
                    command: "sh".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 5,
                    request_timeout_seconds: 5,
                    heuristics: None,
                    name: Some("diag-catchall".to_string()),
                    handles: None,
                },
            ];
            let router = ToolRouter::from_configs(configs.iter()).unwrap();

            let cache = Arc::new(Mutex::new(crate::bridge::NotificationCache::new()));
            let translator = Translator::new()
                .with_router(router)
                .with_notification_cache(Arc::clone(&cache));
            translator.register_client(hover_id.clone(), seed.client().clone());
            translator.register_server(hover_id.clone(), seed);

            let owned_by_healthy_server: lsp_types::Uri =
                "file:///workspace/still_healthy.rs".parse().unwrap();
            cache
                .lock()
                .await
                .store_diagnostics(&hover_id, &owned_by_healthy_server, None, vec![]);

            wait_until_dead(&translator, &hover_id).await;

            let respawn_script = write_responder_script(dir.path(), 1);
            // `language_id` must match the router's ("rust"), not the
            // routing identity ("hover-only"): otherwise `is_diagnostics_route`
            // returns `false` because of a language mismatch rather than
            // because of the `handles: Some([Hover])` restriction this test
            // means to exercise, which would pass for the wrong reason.
            let mut respawn_config = stub_server_config("hover-only", &respawn_script);
            respawn_config.server_config.language_id = "rust".to_string();
            translator.register_server_config(hover_id.clone(), respawn_config);

            translator.respawn_if_dead(&hover_id).await.unwrap();

            assert!(
                cache
                    .lock()
                    .await
                    .get_diagnostics(owned_by_healthy_server.as_str())
                    .is_some(),
                "respawning a non-diagnostics-route server must not clear \
                 the diagnostics-route server's cache entries"
            );
        }

        /// #249 test-gap closure: proves `resolve_client_for_file`'s
        /// dead-server branch is actually reached through the shared
        /// entry point every public tool handler (`handle_hover`,
        /// `handle_definition`, ...) funnels through -- not just through
        /// the private `respawn_if_dead`/`is_server_dead` calls the other
        /// tests in this module make directly.
        #[tokio::test]
        async fn test_prepare_document_respawns_dead_server_through_shared_entry_point() {
            let dir = TempDir::new().unwrap();
            let workspace = dir.path();
            let file_path = workspace.join("main.rs");
            fs::write(&file_path, "fn main() {}").unwrap();

            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();
            let mut translator = Translator::new()
                .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]))
                .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
            translator.set_workspace_roots(vec![workspace.to_path_buf()]);
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            wait_until_dead(&translator, &id).await;

            let respawn_script = write_responder_script(dir.path(), 1);
            translator
                .register_server_config(id.clone(), stub_server_config("rust", &respawn_script));

            let result = translator
                .prepare_document(&file_path.to_string_lossy(), ToolKind::Hover)
                .await;
            assert!(result.is_ok(), "got {result:?}");

            assert!(
                !translator.is_server_dead(&id),
                "the respawned replacement should be alive"
            );
        }
    }
}
