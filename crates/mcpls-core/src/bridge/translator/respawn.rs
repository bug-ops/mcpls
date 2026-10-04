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
use super::restart::{NotificationReceivers, NotificationRouting};
use crate::DiagnosticsRole;
use crate::bridge::lock_std;
use crate::config::ServerId;
use crate::error::{Error, Result};
use crate::lsp::{LspServer, ServerInitConfig};

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
    /// The respawn config of the server tracked under `id`, if it is
    /// registered and dead: its process has exited, or its message loop has
    /// stopped while the process is still running.
    ///
    /// The config is read from the registered [`LspServer`] itself, under the
    /// same `lsp_servers` lock as the liveness check, so the two cannot
    /// disagree. Returns `None` ("not dead") for an `id` that isn't
    /// registered at all -- that's the separate `ServerInitializing`/
    /// `NoServerForTool` concern callers already handle, not something the
    /// respawn path should react to -- and for any `try_wait` error, on the
    /// conservative assumption that a health check that itself failed should
    /// not trigger a respawn.
    fn dead_server_config(&self, id: &ServerId) -> Option<ServerInitConfig> {
        let mut servers = lock_std(&self.lsp_servers);
        let server = servers.get_mut(id)?;
        let config = server.is_dead().ok()?.then(|| server.init_config().clone());
        drop(servers);
        config
    }

    /// Return the shared single-flight lock for `id`, creating it on first
    /// use.
    ///
    /// Two concurrent callers racing to respawn the same server both get a
    /// clone of the *same* underlying `Mutex`, so awaiting it actually
    /// serializes them instead of letting both proceed independently.
    pub(crate) fn respawn_lock(&self, id: &ServerId) -> Arc<Mutex<()>> {
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
    /// Notifications from the replacement are discarded rather than wired
    /// into the existing pump (see [`NotificationRouting::Discard`]), and the
    /// diagnostics-route server is flagged push-degraded (#359); a manual
    /// `restart_server` re-wires the pump and clears the flag. See
    /// [`Self::respawn_locked`] for the state reset both paths share.
    ///
    /// A crash-looping server (repeated respawn failures) backs off
    /// exponentially (`RESPAWN_BACKOFF_BASE` up to `RESPAWN_BACKOFF_MAX`)
    /// instead of retrying on every single tool call, each of which would
    /// otherwise cost up to a full `timeout_seconds` inside `initialize`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ServerUnavailable`] if `id` is currently within its
    /// backoff window. Returns whatever error `LspServer::spawn` produced (e.g. its
    /// command is no longer on `PATH`, or `initialize` fails again) if an
    /// actual respawn attempt failed.
    pub(crate) async fn respawn_if_dead(&self, id: &ServerId) -> Result<()> {
        if self.dead_server_config(id).is_none() {
            return Ok(());
        }

        let lock = self.respawn_lock(id);
        let _guard = lock.lock().await;

        // Another caller may have already respawned it while we waited.
        let Some(config) = self.dead_server_config(id) else {
            return Ok(());
        };

        // A panicked message loop never drains its own pending requests.
        let dead_client = lock_std(&self.lsp_clients).get(id).cloned();
        if let Some(client) = dead_client {
            client.fail_pending_requests().await;
        }

        self.reconcile_respawn_stability(id);

        tracing::warn!("LSP server '{id}' has crashed, respawning");
        self.respawn_locked(
            id,
            config,
            BackoffPolicy::Honor,
            NotificationRouting::Discard,
        )
        .await
    }

    /// Spawn a replacement for `id` from `config` and swap it in for whatever
    /// is registered, with the caller already holding [`Self::respawn_lock`].
    ///
    /// Shared by the crash path ([`Self::respawn_if_dead`]) and the manual
    /// restart. Ordering matters:
    ///
    /// 1. Backoff (when honoured), then spawn, recorded in the backoff
    ///    bookkeeping.
    /// 2. The previous notification task for `id` is aborted. Abort is
    ///    asynchronous: it may still finish one cache write already in
    ///    progress.
    /// 3. `id`'s indexing state is reset, and for the diagnostics-route
    ///    server its cached diagnostics are cleared and its push-degraded
    ///    flag updated per `routing`. This precedes step 4 so a readiness
    ///    signal or diagnostics push the replacement emitted during its own
    ///    handshake, already buffered in its channels, is the last write.
    /// 4. The consumer for the replacement's notification lanes is started.
    /// 5. The replacement replaces the registered client and server,
    ///    `document_tracker` forgets `id` (after the swap: a caller still on
    ///    the old client must not record sync against the new generation),
    ///    the old client's pending requests fail, and subscribers of the
    ///    cleared diagnostics are told to re-read.
    ///
    /// # Errors
    ///
    /// [`Error::ServerUnavailable`] when backoff is honoured and active, or
    /// the spawn error of the replacement.
    pub(super) async fn respawn_locked(
        &self,
        id: &ServerId,
        config: ServerInitConfig,
        backoff: BackoffPolicy,
        routing: NotificationRouting<'_>,
    ) -> Result<()> {
        if backoff == BackoffPolicy::Honor
            && let Some(remaining) = self.respawn_backoff_remaining(id)
        {
            tracing::warn!(
                "LSP server '{id}' is crash-looping, backing off for {remaining:?} \
                 before the next respawn attempt"
            );
            return Err(Error::ServerUnavailable {
                server_id: id.clone(),
                retry_in: remaining,
            });
        }

        let language_id = config.server_config.language_id.clone();

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
        let receivers = NotificationReceivers {
            notifications: new_server.take_notification_rx(),
            lifecycle: new_server.take_lifecycle_rx(),
        };

        let stale_task = lock_std(&self.notification_tasks).remove(id);
        if let Some(handle) = stale_task {
            handle.abort();
        }

        // Only the diagnostics-route server ever wrote push notifications to
        // the cache, and `clear_server_diagnostics` is scoped to one server's
        // own entries (#266), so a crashed rust-analyzer never wipes a
        // healthy pyright's cached diagnostics.
        let diagnostics_route = self.is_diagnostics_route(&language_id, id);
        let mut cleared = Vec::new();
        if let Some(cache) = &self.notification_cache {
            let mut cache = cache.lock().await;
            cache.reset_indexing_state(id);
            if diagnostics_route {
                cleared = cache.clear_server_diagnostics(id);
                match routing {
                    NotificationRouting::Pump(_) => cache.clear_push_degraded(id),
                    NotificationRouting::Discard => {
                        tracing::warn!(
                            "LSP server '{id}' (language '{language_id}') respawned; its diagnostics \
                             push notifications are discarded rather than cached until it is \
                             restarted with `restart_server` or mcpls is restarted"
                        );
                        cache.mark_push_degraded(id);
                    }
                }
            }
        }

        let consumer = match routing {
            NotificationRouting::Pump(wiring) => wiring.spawn_pump(
                id.clone(),
                receivers,
                DiagnosticsRole::from_route(diagnostics_route),
            ),
            NotificationRouting::Discard => self.spawn_discard_consumer(id, receivers),
        };
        lock_std(&self.notification_tasks).insert(id.clone(), consumer);

        let old_client = lock_std(&self.lsp_clients).insert(id.clone(), new_client);
        let old_server = lock_std(&self.lsp_servers).insert(id.clone(), new_server);
        drop(old_server); // dropped after the `lsp_servers` guard, not under it

        self.document_tracker.forget_server(id);

        if let Some(old_client) = old_client {
            old_client.fail_pending_requests().await;
        }

        if let Some(wiring) = self.wiring.get() {
            wiring.publish_invalidated(&cleared).await;
        }

        tracing::info!("LSP server '{id}' respawned successfully");
        Ok(())
    }

    /// Drain the replacement's notification lane and forward its lifecycle
    /// lane into the cache, returning the lifecycle forwarder's handle.
    ///
    /// The forwarder has no cancellation hook (the shutdown watch lives in
    /// `serve_with`'s scope), so it writes until its channel closes.
    fn spawn_discard_consumer(
        &self,
        id: &ServerId,
        receivers: NotificationReceivers,
    ) -> tokio::task::AbortHandle {
        let NotificationReceivers {
            mut notifications,
            mut lifecycle,
        } = receivers;
        tokio::spawn(async move { while notifications.recv().await.is_some() {} });
        let forwarder = match self.notification_cache.clone() {
            Some(cache) => {
                let lifecycle_id = id.clone();
                tokio::spawn(async move {
                    while let Some(notif) = lifecycle.recv().await {
                        crate::bridge::apply_lifecycle_notification(
                            &mut *cache.lock().await,
                            &lifecycle_id,
                            notif,
                        );
                    }
                })
            }
            None => tokio::spawn(async move { while lifecycle.recv().await.is_some() {} }),
        };
        forwarder.abort_handle()
    }
}

/// Whether [`Translator::respawn_locked`] honours the crash-loop backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackoffPolicy {
    /// Refuse while `id` is inside its backoff window (automatic respawn).
    Honor,
    /// Attempt immediately; the outcome still feeds the bookkeeping (manual restart).
    Bypass,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::bridge::translator::clock::{Clock, FakeClock};
    use crate::config::ServerId;
    #[cfg(unix)]
    use crate::test_lsp::client_path;

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
    fn test_dead_server_config_none_when_not_registered() {
        let translator = Translator::new();
        assert!(
            translator
                .dead_server_config(&ServerId::from("rust"))
                .is_none()
        );
    }

    // Gated `#[cfg(unix)]`: this module's fake-LSP-server test double is a
    // hand-written `sh` script (POSIX parameter expansion, `printf`-framed
    // LSP responses, file-based invocation counters), which has no
    // equivalent on Windows. CI's "Test (unit)" job matrix includes
    // `windows-latest`.
    #[cfg(unix)]
    mod respawn_tests {
        use std::collections::HashMap;
        use std::{assert_matches, fs};

        use tempfile::TempDir;
        use tokio::time::Duration;

        use super::*;
        use crate::bridge::WorkspaceRoots;
        use crate::bridge::translator::testing::{
            pid_is_running, stub_server_config, write_crash_after_init_script,
            write_responder_script,
        };
        use crate::config::{ToolKind, ToolRouter};
        use crate::lsp::ServerInitConfig;
        use crate::test_lsp::with_read_preamble;

        /// Replaces the registered server's `init_config`, i.e. the config
        /// the next respawn will use.
        fn set_respawn_config(translator: &Translator, id: &ServerId, config: ServerInitConfig) {
            lock_std(&translator.lsp_servers)
                .get_mut(id)
                .unwrap()
                .set_init_config(config);
        }

        /// Polls `dead_server_config` until it reports a dead server, bounding the wait
        /// so a broken script fails the test instead of hanging it.
        async fn wait_until_dead(translator: &Translator, id: &ServerId) {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if translator.dead_server_config(id).is_some() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("seed server never reported as exited");
        }

        /// #542: respawning a crashed server kills the descendants it left
        /// behind in its process group.
        #[tokio::test]
        async fn test_respawn_if_dead_kills_descendants_of_the_crashed_server() {
            let dir = TempDir::new().unwrap();
            let pid_file = dir.path().join("grandchild.pid");
            let crashing = dir.path().join("crashing.sh");
            let body = with_read_preamble(&format!(
                r#"sleep 600 &
echo $! > '{}'
body='{{"jsonrpc":"2.0","id":1,"result":{{"capabilities":{{}}}}}}'
printf 'Content-Length: %d\r\n\r\n%s' ${{#body}} "$body"
sleep 0.3
"#,
                pid_file.display()
            ));
            fs::write(&crashing, body).unwrap();
            let id = ServerId::from("rust");

            let server = LspServer::spawn(stub_server_config("rust", &crashing))
                .await
                .unwrap();
            let translator = Translator::new();
            translator.register_server_complete(server);
            wait_until_dead(&translator, &id).await;
            let grandchild: u32 = fs::read_to_string(&pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(pid_is_running(grandchild));

            let responder = write_responder_script(dir.path(), 5);
            set_respawn_config(&translator, &id, stub_server_config("rust", &responder));
            translator.respawn_if_dead(&id).await.unwrap();

            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while pid_is_running(grandchild) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "descendant of the crashed server survived the respawn"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        #[tokio::test]
        async fn test_respawn_if_dead_noop_when_server_alive() {
            let dir = TempDir::new().unwrap();
            let script = write_responder_script(dir.path(), 1);
            let id = ServerId::from("rust");
            let config = stub_server_config("rust", &script);

            let server = LspServer::spawn(config).await.unwrap();
            let translator = Translator::new();
            translator.register_server_complete(server);

            assert!(translator.dead_server_config(&id).is_none());
            assert!(translator.respawn_if_dead(&id).await.is_ok());
        }

        /// A live child behind a stopped message loop (e.g. a panicked loop)
        /// is dead for routing purposes and must be respawned.
        #[cfg(unix)]
        #[tokio::test]
        async fn test_respawn_if_dead_replaces_server_with_stopped_message_loop() {
            let dir = TempDir::new().unwrap();
            let script = write_responder_script(dir.path(), 5);
            let id = ServerId::from("rust");

            let seed = crate::lsp::fake_lsp_server_with_dead_loop_and_live_child();
            let translator = Translator::new();
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            set_respawn_config(&translator, &id, stub_server_config("rust", &script));
            wait_until_dead(&translator, &id).await;

            translator.respawn_if_dead(&id).await.unwrap();

            assert!(translator.dead_server_config(&id).is_none());
        }

        /// #529: the respawn config is the registered server's own
        /// `init_config`, with no separately stored copy.
        #[tokio::test]
        async fn test_respawn_if_dead_uses_config_of_registered_server() {
            let dir = TempDir::new().unwrap();
            let script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let config = stub_server_config("rust", &script);

            let server = LspServer::spawn(config.clone()).await.unwrap();
            let translator = Translator::new();
            translator.register_server_complete(server);
            wait_until_dead(&translator, &id).await;

            let dead = translator.dead_server_config(&id).unwrap();
            assert_eq!(dead.server_config.args, config.server_config.args);

            translator.respawn_if_dead(&id).await.unwrap();
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
            set_respawn_config(&translator, &id, broken);

            let err = translator.respawn_if_dead(&id).await.unwrap_err();
            assert_matches!(err, Error::ServerNotFound { .. }, "got {err:?}");
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
            let template = with_read_preamble(
                r#"echo x >> "__COUNTER__"
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
"#,
            );
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
            set_respawn_config(&translator, &id, config);
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
        /// (`Error::ServerNotFound`, from a command that does not
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
            set_respawn_config(&translator, &id, broken);

            let err1 = translator.respawn_if_dead(&id).await.unwrap_err();
            assert_matches!(
                err1,
                Error::ServerNotFound { .. },
                "first attempt should be a real (failed) spawn, got {err1:?}"
            );

            let err2 = translator.respawn_if_dead(&id).await.unwrap_err();
            assert_matches!(
                err2,
                Error::ServerUnavailable { .. },
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
            set_respawn_config(&translator, &id, broken);

            let err1 = translator.respawn_if_dead(&id).await.unwrap_err();
            assert_matches!(
                err1,
                Error::ServerNotFound { .. },
                "first attempt should be a real (failed) spawn, got {err1:?}"
            );

            let err2 = translator.respawn_if_dead(&id).await.unwrap_err();
            assert_matches!(
                err2,
                Error::ServerUnavailable { .. },
                "second call within the backoff window must still fail fast, got {err2:?}"
            );

            // Advance well past the computed backoff delay and swap in a
            // config that will actually succeed this time.
            clock.advance(RESPAWN_BACKOFF_MAX);
            let working_script = write_crash_after_init_script(dir.path());
            set_respawn_config(
                &translator,
                &id,
                stub_server_config("rust", &working_script),
            );

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
            set_respawn_config(&translator, &id, stub_server_config("rust", &seed_script));

            translator
                .respawn_if_dead(&id)
                .await
                .expect("the replacement completes initialize, so this attempt succeeds");
            wait_until_dead(&translator, &id).await;

            let err = translator.respawn_if_dead(&id).await.unwrap_err();
            assert_matches!(
                err,
                Error::ServerUnavailable { .. },
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

            let synced_uri: lsp_types::Uri = lsp_types::Uri::from("file:///workspace/opened.rs");
            let never_opened_uri: lsp_types::Uri =
                lsp_types::Uri::from("file:///workspace/never_opened.rs");
            let other_language_uri: lsp_types::Uri =
                lsp_types::Uri::from("file:///workspace/main.py");
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
            set_respawn_config(
                &translator,
                &id,
                stub_server_config("rust", &respawn_script),
            );

            translator.respawn_if_dead(&id).await.unwrap();

            let guard = cache.lock().await;
            assert!(
                guard.diagnostics(&synced_uri).is_none(),
                "diagnostics attributed to the crashed connection must be \
                 invalidated on respawn, not served as current"
            );
            assert!(
                guard.diagnostics(&never_opened_uri).is_none(),
                "workspace-wide diagnostics for a file mcpls never opened \
                 must also be invalidated, not just synced documents"
            );
            assert!(
                guard.diagnostics(&other_language_uri).is_some(),
                "a different diagnostics-route server's entries must survive \
                 an unrelated server's respawn-triggered cache clear"
            );
            assert!(
                guard.is_push_degraded(&id),
                "#359: the respawned diagnostics-route server must be marked as \
                 push-degraded, since its replacement's notifications are discarded"
            );
            assert!(
                !guard.is_push_degraded(&ServerId::from("python")),
                "an unrelated, never-respawned server must not be marked degraded"
            );
            drop(guard);
        }

        /// A respawned server's tracked `IndexingState` must reset
        /// to `Unknown`, not carry over stale state from the crashed
        /// connection -- the replacement process has indexed nothing yet,
        /// and its own `experimental/serverStatus` notifications are
        /// discarded (see this method's doc), so a stale `Ready` would let
        /// whole-workspace queries through against an empty index. Unlike
        /// diagnostics-cache clearing, this must happen regardless of
        /// diagnostics-route status -- indexing readiness gates every
        /// routed server's whole-workspace tools, not just the one that
        /// owns diagnostics.
        #[tokio::test]
        async fn test_respawn_if_dead_resets_indexing_state() {
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

            cache.lock().await.observe_indexing_signal(
                &id,
                "experimental/serverStatus",
                Some(&serde_json::json!({"quiescent": true})),
            );
            assert_eq!(
                cache.lock().await.indexing_state(&id),
                crate::bridge::IndexingState::Ready
            );

            wait_until_dead(&translator, &id).await;

            let respawn_script = write_responder_script(dir.path(), 1);
            set_respawn_config(
                &translator,
                &id,
                stub_server_config("rust", &respawn_script),
            );

            translator.respawn_if_dead(&id).await.unwrap();

            assert_eq!(
                cache.lock().await.indexing_state(&id),
                crate::bridge::IndexingState::Unknown,
                "a respawned server must not carry over a stale Ready/Loading state \
                 from the crashed connection"
            );
        }

        /// #425 regression: the *replacement* process's own lifecycle-lane
        /// notifications (`$/progress` and `experimental/serverStatus`) must
        /// be wired into the shared `NotificationCache`, not drained and
        /// discarded like before -- otherwise a respawned server can never
        /// re-acquire indexing readiness and stays `Unknown`/fail-open for
        /// the rest of the process's life. The respawn script answers
        /// `initialize` and *immediately* (no delay) emits its own
        /// `experimental/serverStatus` notification, so it is already
        /// buffered in the lifecycle channel by the time `respawn_if_dead`
        /// gets to wiring it up -- exactly like a real rust-analyzer
        /// replacement reporting readiness during its own handshake. Runs on
        /// a multi-thread runtime (matching `crates/mcpls-cli/src/main.rs`'s
        /// production `#[tokio::main]`) so the forwarding task can race
        /// `respawn_if_dead`'s own continuation for real, which is what
        /// exposed the S1 ordering bug this test is meant to catch: with
        /// `reset_indexing_state` (wrongly) called *after* the forwarder was
        /// spawned, the buffered signal could be drained and immediately
        /// erased before this test ever observes it.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn test_respawn_if_dead_reacquires_indexing_state_from_replacement() {
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
            wait_until_dead(&translator, &id).await;

            let respawn_script_path = dir.path().join("respawn_with_status.sh");
            let respawn_script_body = with_read_preamble(
                r#"body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
notif='{"jsonrpc":"2.0","method":"experimental/serverStatus","params":{"quiescent":true}}'
printf 'Content-Length: %d\r\n\r\n%sContent-Length: %d\r\n\r\n%s' ${#body} "$body" ${#notif} "$notif"
sleep 1
"#,
            );
            fs::write(&respawn_script_path, respawn_script_body).unwrap();
            set_respawn_config(
                &translator,
                &id,
                stub_server_config("rust", &respawn_script_path),
            );

            translator.respawn_if_dead(&id).await.unwrap();

            let observed_ready = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if cache.lock().await.indexing_state(&id) == crate::bridge::IndexingState::Ready
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;

            assert!(
                observed_ready.is_ok(),
                "the replacement's own experimental/serverStatus notification must reach \
                 the NotificationCache, not be discarded"
            );
        }

        /// Code-review regression: `respawn_if_dead` must actually *abort*
        /// any previously-registered lifecycle-forwarder handle for `id`,
        /// not merely replace the map entry (which an earlier version of
        /// this test could not tell apart from a forwarder that happened to
        /// finish on its own -- e.g. because its channel closed naturally).
        /// Pre-registers a handle to a task built from [`std::future::pending`],
        /// which by construction can *never* complete except via
        /// cancellation, so observing it end after a respawn is unambiguous
        /// proof that `respawn_if_dead` called `.abort()` on it.
        #[tokio::test]
        async fn test_respawn_if_dead_aborts_previous_forwarder_handle() {
            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();
            let translator = Translator::new()
                .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]));
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            wait_until_dead(&translator, &id).await;

            let never_completes = tokio::spawn(std::future::pending::<()>());
            lock_std(&translator.notification_tasks)
                .insert(id.clone(), never_completes.abort_handle());

            let respawn_script = write_responder_script(dir.path(), 1);
            set_respawn_config(
                &translator,
                &id,
                stub_server_config("rust", &respawn_script),
            );
            translator.respawn_if_dead(&id).await.unwrap();

            let outcome = tokio::time::timeout(Duration::from_secs(2), never_completes)
                .await
                .expect("respawn_if_dead must abort the stale handle promptly, not hang it");
            assert!(
                outcome.is_err_and(|join_err| join_err.is_cancelled()),
                "a task that can never complete on its own must have been aborted \
                 by the later respawn, not merely replaced in the map"
            );

            assert!(
                lock_std(&translator.notification_tasks)
                    .get(&id)
                    .is_some_and(|current| !current.is_finished()),
                "the new respawn's own forwarder must be registered and still running"
            );
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
                    indexing: crate::bridge::IndexingPolicy::Auto,
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
                    indexing: crate::bridge::IndexingPolicy::Auto,
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
                lsp_types::Uri::from("file:///workspace/still_healthy.rs");
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
            set_respawn_config(&translator, &hover_id, respawn_config);

            translator.respawn_if_dead(&hover_id).await.unwrap();

            assert!(
                cache
                    .lock()
                    .await
                    .diagnostics(&owned_by_healthy_server)
                    .is_some(),
                "respawning a non-diagnostics-route server must not clear \
                 the diagnostics-route server's cache entries"
            );
            assert!(
                !cache.lock().await.is_push_degraded(&hover_id),
                "#359: respawning a server that is not the diagnostics route \
                 must not mark it push-degraded either"
            );
        }

        /// #249 test-gap closure: proves `resolve_client_for_file`'s
        /// dead-server branch is actually reached through the shared
        /// entry point every public tool handler (`handle_hover`,
        /// `handle_definition`, ...) funnels through -- not just through
        /// the private `respawn_if_dead`/`dead_server_config` calls the other
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
            translator.set_workspace_roots(
                WorkspaceRoots::from_configured(&[workspace.to_path_buf()]).unwrap(),
            );
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            wait_until_dead(&translator, &id).await;

            let respawn_script = write_responder_script(dir.path(), 1);
            set_respawn_config(
                &translator,
                &id,
                stub_server_config("rust", &respawn_script),
            );

            let result = translator
                .prepare_document(
                    &client_path(file_path.to_string_lossy().into_owned()),
                    ToolKind::Hover,
                )
                .await;
            assert!(result.is_ok(), "got {result:?}");

            assert!(
                translator.dead_server_config(&id).is_none(),
                "the respawned replacement should be alive"
            );
        }
    }
}
