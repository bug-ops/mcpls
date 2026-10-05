//! Manual restart of LSP servers on request (`restart_server`).
//!
//! A restart is kill-then-respawn: the old process tree is terminated first,
//! then a replacement is spawned through the same body automatic respawn uses
//! ([`Translator::respawn_locked`]), with the diagnostics pump re-wired so the
//! restarted server returns to live push diagnostics. While the old process is
//! being stopped the server is deregistered and reported as initializing, so
//! concurrent tool calls get a retryable error instead of a dead client.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use super::Translator;
use super::respawn::BackoffPolicy;
use crate::DiagnosticsRole;
use crate::bridge::{DiagnosticsKey, IndexingState, lock_std};
use crate::config::ServerId;
use crate::error::{Error, Result};
use crate::lsp::{ExitGrace, LspClient, LspNotification, LspServer, ServerInitConfig};
use crate::redaction::{Redactions, ServerText};

/// Minimum interval between two manual restart attempts of the same server.
const RESTART_COOLDOWN: Duration = Duration::from_secs(5);

/// How long the old server gets to answer `shutdown` on a restart. Short
/// because a server that does not answer is killed anyway and the caller is
/// waiting.
const RESTART_HANDSHAKE_BUDGET: Duration = Duration::from_secs(3);

/// How long a server that did answer `shutdown` may take to exit on its own
/// on a restart (rust-analyzer and jdtls flush caches), before its process
/// tree is killed.
const RESTART_EXIT_GRACE: Duration = crate::lsp::SHUTDOWN_TIMEOUT;

/// Most server ids one request may name.
pub const MAX_RESTART_SERVER_IDS: usize = 64;

/// How many servers one `restart_server` call restarts concurrently.
const RESTART_CONCURRENCY: usize = 4;

/// Longest accepted server id, in bytes.
pub const MAX_SERVER_ID_BYTES: usize = 256;

/// Why a list of server ids was rejected by [`ServerIds::try_new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ServerIdsError {
    /// No ids were given.
    #[error("`servers` must not be empty")]
    Empty,
    /// More than [`MAX_RESTART_SERVER_IDS`] ids were given.
    #[error("`servers` may name at most {MAX_RESTART_SERVER_IDS} servers")]
    TooMany,
    /// An id was empty or whitespace.
    #[error("server ids must not be blank")]
    Blank,
    /// An id was longer than [`MAX_SERVER_ID_BYTES`].
    #[error("server ids must be at most {MAX_SERVER_ID_BYTES} bytes")]
    TooLong,
}

/// A non-empty, duplicate-free, bounded list of server ids: at most
/// [`MAX_RESTART_SERVER_IDS`] ids of [`MAX_SERVER_ID_BYTES`] bytes each.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::{ServerIds, ServerIdsError};
/// use mcpls_core::config::ServerId;
///
/// let ids = ServerIds::try_new(vec![ServerId::from("rust"), ServerId::from("rust")]).unwrap();
/// assert_eq!(ids.as_slice().len(), 1);
/// assert_eq!(ServerIds::try_new(Vec::new()), Err(ServerIdsError::Empty));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerIds(Vec<ServerId>);

impl ServerIds {
    /// Build the list, dropping repeated ids.
    ///
    /// # Errors
    ///
    /// [`ServerIdsError`] when `ids` is empty, has more than
    /// [`MAX_RESTART_SERVER_IDS`] entries, or holds a blank or oversized id.
    pub fn try_new(ids: Vec<ServerId>) -> std::result::Result<Self, ServerIdsError> {
        if ids.len() > MAX_RESTART_SERVER_IDS {
            return Err(ServerIdsError::TooMany);
        }
        if ids.iter().any(|id| id.as_str().trim().is_empty()) {
            return Err(ServerIdsError::Blank);
        }
        if ids.iter().any(|id| id.as_str().len() > MAX_SERVER_ID_BYTES) {
            return Err(ServerIdsError::TooLong);
        }
        let mut seen = HashSet::new();
        let unique: Vec<ServerId> = ids
            .into_iter()
            .filter(|id| seen.insert(id.clone()))
            .collect();
        if unique.is_empty() {
            Err(ServerIdsError::Empty)
        } else {
            Ok(Self(unique))
        }
    }

    /// The ids, in request order.
    #[must_use]
    pub fn as_slice(&self) -> &[ServerId] {
        &self.0
    }
}

/// Which servers a restart applies to.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::{RestartTarget, ServerIds};
/// use mcpls_core::config::ServerId;
///
/// let one = RestartTarget::Servers(ServerIds::try_new(vec![ServerId::from("rust")]).unwrap());
/// assert_ne!(one, RestartTarget::All);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartTarget {
    /// Every configured server.
    All,
    /// Exactly these servers.
    Servers(ServerIds),
}

/// Why a restart failed after the old process was already stopped.
///
/// The server stays registered as dead; the next tool call retries the spawn
/// under the normal crash-loop backoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RestartFailure {
    /// The replacement process could not be started.
    SpawnFailed {
        /// Why the spawn failed.
        message: String,
    },
    /// The replacement started but did not complete the `initialize` handshake.
    InitializeFailed {
        /// Why initialization failed.
        message: String,
    },
    /// mcpls is shutting down, so no replacement was started.
    ShuttingDown,
}

impl RestartFailure {
    fn from_error(error: &Error) -> Self {
        let message = error.to_string();
        match error {
            Error::ServerNotFound { .. } | Error::ServerSpawnFailed { .. } => {
                Self::SpawnFailed { message }
            }
            Error::LspInitFailed { .. }
            | Error::ServerExitedDuringInit { .. }
            | Error::ServerTerminated
            | Error::TaskFailed { .. }
            | Error::StdioCapture(_)
            | Error::Timeout(_)
            | Error::LspServerError { .. }
            | Error::LspProtocolError(_)
            | Error::ShutdownTimeout
            | Error::Io(_)
            | Error::Json(_)
            // Spawning a replacement never yields the variants below; they are
            // reported as an initialization failure rather than a new reason.
            | Error::McpServerStart(_)
            | Error::PathToUri(_)
            | Error::HttpBind { .. }
            | Error::InvalidClientPath(_)
            | Error::MalformedPath { .. }
            | Error::DocumentNotFound(_)
            | Error::NoServerForLanguage(_)
            | Error::NoServerForTool { .. }
            | Error::ServerFailedToStart(_)
            | Error::ServerInitializing { .. }
            | Error::ServerRestarted { .. }
            | Error::SymbolResolution(_)
            | Error::UnknownServers { .. }
            | Error::WorkspaceServersInitializing
            | Error::NoServerConfigured
            | Error::NoServerForWorkspaceTool { .. }
            | Error::ConfigNotFound(_)
            | Error::InvalidConfig(_)
            | Error::TomlDe(_)
            | Error::TomlSer(_)
            | Error::InvalidUri(_)
            | Error::ResourceUri(_)
            | Error::InvalidPositionInput(_)
            | Error::InvalidRangeInput(_)
            | Error::ServerUnavailable { .. }
            | Error::InvalidToolParams(_)
            | Error::FileIo { .. }
            | Error::PathOutsideWorkspace(_)
            | Error::NoWorkspaceRoots(_)
            | Error::DocumentLimitExceeded { .. }
            | Error::SubscriptionLimitReached { .. }
            | Error::ListenStreamsExhausted { .. }
            | Error::ListenFilterTooLarge { .. }
            | Error::FileSizeLimitExceeded { .. }
            | Error::NotARegularFile(_)
            | Error::AllServersFailedToInit { .. }
            | Error::CapabilityNotSupported { .. }
            | Error::WorkspaceIndexing { .. } => Self::InitializeFailed { message },
        }
    }
}

/// What happened to one server in a restart request.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::RestartOutcome;
///
/// let value = serde_json::to_value(RestartOutcome::Throttled { retry_in_ms: 1500 }).unwrap();
/// assert_eq!(value["status"], "throttled");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RestartOutcome {
    /// The server runs a freshly initialized process.
    Restarted {
        /// Another restart of the same server finished while this request
        /// waited, so no second restart was performed.
        coalesced: bool,
        /// Indexing readiness of the new process.
        indexing_state: IndexingState,
        /// Push diagnostics for this server are still not live. A restart
        /// clears this; it stays set if a crash respawn raced in afterwards
        /// and restarting again is needed.
        push_notifications_degraded: bool,
    },
    /// The restart failed; see [`RestartFailure`].
    Failed {
        /// Why it failed.
        reason: RestartFailure,
    },
    /// The server was restarted too recently; retry after `retry_in_ms`.
    Throttled {
        /// Milliseconds until a restart is allowed again.
        retry_in_ms: u64,
    },
    /// The server has not finished its initial startup; retry shortly.
    Initializing,
    /// The server never started (or failed at startup), so there is no
    /// process to restart. Fix the cause and restart mcpls.
    NotRunning {
        /// Why the server is not running.
        message: String,
    },
}

/// One server's entry in a [`RestartServerResult`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ServerRestartEntry {
    /// The server this entry reports on.
    pub server_id: ServerId,
    /// What happened to it.
    #[serde(flatten)]
    pub outcome: RestartOutcome,
}

/// Result of a restart request: one entry per targeted server, sorted by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RestartServerResult {
    /// Per-server outcomes.
    pub servers: Vec<ServerRestartEntry>,
}

/// Count of manual restarts completed for one server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct RestartGeneration(u64);

impl RestartGeneration {
    const fn bump(&mut self) {
        self.0 = self.0.saturating_add(1);
    }
}

/// The two notification lanes of a freshly spawned server.
#[derive(Debug)]
pub struct NotificationReceivers {
    /// Diagnostics, log and `showMessage` lane.
    pub(crate) notifications: mpsc::Receiver<LspNotification>,
    /// `$/progress` and unrecognized notifications lane.
    pub(crate) lifecycle: mpsc::Receiver<LspNotification>,
    /// The `tsserver` path mcpls pinned for this server, so the lifecycle lane
    /// can warn when the server reports it did not take effect.
    pub(crate) pinned_tsserver: Option<std::path::PathBuf>,
}

/// How a respawned server's notifications are consumed.
#[derive(Clone, Copy)]
pub(super) enum NotificationRouting<'a> {
    /// Drain and discard the notification lane; the diagnostics-route server
    /// is flagged push-degraded (automatic crash respawn).
    Discard,
    /// Re-wire the diagnostics pump (manual restart).
    Pump(&'a dyn NotificationWiring),
}

/// The part of notification handling that lives in `serve_with`'s scope
/// (shutdown watch, subscription registry) and so cannot be built by the
/// translator itself.
pub trait NotificationWiring: std::fmt::Debug + Send + Sync {
    /// Start a diagnostics pump over `receivers` for `id`, returning a handle
    /// that stops it. A panicking pump must mark `id` push-degraded.
    fn spawn_pump(
        &self,
        id: ServerId,
        receivers: NotificationReceivers,
        role: DiagnosticsRole,
    ) -> AbortHandle;

    /// Tell subscribers of the resources behind the `cleared` cache keys to
    /// re-read them.
    fn publish_invalidated<'a>(&'a self, cleared: &'a [DiagnosticsKey]) -> BoxFuture<'a, ()>;
}

/// Keeps restarts reporting `initializing` until dropped, so an init panic,
/// task abort or early return can never leave the flag set.
#[derive(Debug)]
pub struct StartupGuard<'a>(&'a Translator);

impl Drop for StartupGuard<'_> {
    fn drop(&mut self) {
        self.0.startup_settling.store(false, Ordering::SeqCst);
    }
}

/// A server taken out of the registries for termination, restored on every
/// exit path (error, early return, panic, cancelled future).
struct Deregistered<'a> {
    translator: &'a Translator,
    id: ServerId,
    server: Option<LspServer>,
    client: Option<LspClient>,
}

impl<'a> Deregistered<'a> {
    /// Mark `id` as expected before removing it from the maps, so no caller
    /// ever finds it in neither (which would read as a missing server).
    fn take(translator: &'a Translator, id: &ServerId) -> Option<Self> {
        lock_std(&translator.expected_servers).insert(id.clone());
        let server = lock_std(&translator.lsp_servers).remove(id);
        let client = lock_std(&translator.lsp_clients).remove(id);
        let held = Self {
            translator,
            id: id.clone(),
            server,
            client,
        };
        held.server.is_some().then_some(held)
    }

    /// Stop the held server and return the config to respawn it from.
    /// Termination errors (a wedged server times out) are not fatal.
    async fn terminate(&mut self) -> Option<ServerInitConfig> {
        let server = self.server.as_mut()?;
        if let Some(client) = &self.client {
            client.mark_restarted(self.id.clone());
        }
        let config = server.init_config().clone();
        let stopped = server
            .terminate(
                RESTART_HANDSHAKE_BUDGET,
                ExitGrace::IfAnswered(RESTART_EXIT_GRACE),
            )
            .await;
        if let Err(error) = stopped {
            tracing::warn!(id = %self.id, %error, "old LSP server did not stop cleanly; continuing with restart");
        }
        Some(config)
    }

    /// Put the held parts back and clear the expectation; idempotent.
    fn restore(&mut self) {
        if let Some(server) = self.server.take() {
            lock_std(&self.translator.lsp_servers).insert(self.id.clone(), server);
        }
        if let Some(client) = self.client.take() {
            lock_std(&self.translator.lsp_clients).insert(self.id.clone(), client);
        }
        lock_std(&self.translator.expected_servers).remove(&self.id);
    }
}

impl Drop for Deregistered<'_> {
    fn drop(&mut self) {
        self.restore();
    }
}

impl Translator {
    /// Restart the servers named by `target`, each independently.
    ///
    /// The old process tree is stopped (graceful `shutdown`/`exit` bounded by
    /// the LSP shutdown deadline, then the whole process group is killed) and a
    /// replacement is spawned and initialized from the server's existing
    /// config. Requests in flight on the old process fail with a retryable
    /// [`Error::ServerRestarted`]; documents are re-opened on the new process
    /// on next access. Servers not named are untouched. Restarts of the same
    /// server are serialized, and a request that queued behind another restart
    /// of the same server is reported as coalesced.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownServers`] when `target` names a server that is not
    /// configured; nothing is restarted then. Per-server failures are reported
    /// in the result, not as errors.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use mcpls_core::bridge::{RestartTarget, Translator};
    ///
    /// # async fn run(translator: &Translator) -> mcpls_core::error::Result<()> {
    /// let result = translator.restart_servers(RestartTarget::All).await?;
    /// for entry in &result.servers {
    ///     println!("{}: {:?}", entry.server_id, entry.outcome);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn restart_servers(&self, target: RestartTarget) -> Result<RestartServerResult> {
        let known = self.known_server_ids();
        let ids = match target {
            RestartTarget::All => known,
            RestartTarget::Servers(requested) => {
                let unknown: Vec<ServerId> = requested
                    .as_slice()
                    .iter()
                    .filter(|id| !known.contains(id))
                    .cloned()
                    .collect();
                if !unknown.is_empty() {
                    return Err(Error::UnknownServers {
                        unknown,
                        configured: known,
                    });
                }
                requested.0
            }
        };

        // Snapshotted for every id up front: ids queued behind the
        // concurrency limit must not observe a restart of their own request.
        let requests: Vec<(ServerId, RestartGeneration)> = ids
            .into_iter()
            .map(|id| {
                let generation = self.restart_generation(&id);
                (id, generation)
            })
            .collect();
        let mut servers: Vec<ServerRestartEntry> = futures::stream::iter(requests)
            .map(|(id, generation)| async move {
                let outcome = Box::pin(self.restart_one(&id, generation)).await;
                ServerRestartEntry {
                    server_id: id,
                    outcome,
                }
            })
            .buffer_unordered(RESTART_CONCURRENCY)
            .collect()
            .await;
        servers.sort_by(|a, b| a.server_id.as_str().cmp(b.server_id.as_str()));
        Ok(RestartServerResult { servers })
    }

    /// Every server a restart may address: registered, still expected, or
    /// failed at startup. Sorted by id.
    fn known_server_ids(&self) -> Vec<ServerId> {
        let mut ids: HashSet<ServerId> = lock_std(&self.lsp_servers).keys().cloned().collect();
        ids.extend(lock_std(&self.lsp_clients).keys().cloned());
        ids.extend(lock_std(&self.expected_servers).iter().cloned());
        ids.extend(lock_std(&self.startup_failures).keys().cloned());
        let mut ids: Vec<ServerId> = ids.into_iter().collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids
    }

    fn restart_generation(&self, id: &ServerId) -> RestartGeneration {
        lock_std(&self.restart_generations)
            .get(id)
            .copied()
            .unwrap_or_default()
    }

    /// Install how restarted servers get their diagnostics pump back.
    ///
    /// Called once, before the first server settles, so each initial pump is
    /// registered with [`Self::set_notification_task`] as its server
    /// registers; a repeat call keeps the first wiring. Restarts stay blocked
    /// until startup has settled (see [`Self::begin_startup`]).
    pub(crate) fn install_wiring(&self, wiring: Arc<dyn NotificationWiring>) {
        if self.wiring.set(wiring).is_err() {
            tracing::debug!("notification wiring already installed");
        }
    }

    /// Record the task consuming `id`'s notification lanes, so a restart can
    /// stop it before starting its replacement.
    pub(crate) fn set_notification_task(&self, id: ServerId, handle: AbortHandle) {
        lock_std(&self.notification_tasks).insert(id, handle);
    }

    /// Declare that the initial server startup is still settling; restarts
    /// report `initializing` until the returned guard is dropped, because a
    /// restarted pump's diagnostics role is fixed at spawn and routes still
    /// change while servers settle.
    pub(crate) fn begin_startup(&self) -> StartupGuard<'_> {
        self.startup_settling.store(true, Ordering::SeqCst);
        StartupGuard(self)
    }

    /// Declare that shutdown has begun; no restart starts a server after this.
    pub(crate) fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// The cooldown still to wait before `id` may be restarted again.
    fn restart_cooldown_remaining(&self, id: &ServerId) -> Option<Duration> {
        let last = lock_std(&self.restart_attempts).get(id).copied()?;
        let elapsed = self.clock.now().saturating_duration_since(last);
        RESTART_COOLDOWN.checked_sub(elapsed)
    }

    async fn restarted_outcome(&self, id: &ServerId, coalesced: bool) -> RestartOutcome {
        let (indexing_state, push_notifications_degraded) = match &self.notification_cache {
            Some(cache) => {
                let cache = cache.lock().await;
                (cache.indexing_state(id), cache.is_push_degraded(id))
            }
            None => (IndexingState::Unknown, false),
        };
        RestartOutcome::Restarted {
            coalesced,
            indexing_state,
            push_notifications_degraded,
        }
    }

    /// Why `id` cannot be restarted right now, in classification order:
    /// shutting down, never started or failed to start, init panicked, startup
    /// still settling or not yet wired, then the cooldown.
    fn restart_blocker(&self, id: &ServerId) -> Option<RestartOutcome> {
        if self.is_shutting_down() {
            return Some(RestartOutcome::Failed {
                reason: RestartFailure::ShuttingDown,
            });
        }
        if !lock_std(&self.lsp_servers).contains_key(id) {
            return Some(match self.startup_failure(id) {
                Some(failure) => RestartOutcome::NotRunning {
                    message: Error::ServerFailedToStart(Box::new(failure)).to_string(),
                },
                None if lock_std(&self.expected_servers).contains(id) => {
                    RestartOutcome::Initializing
                }
                None => RestartOutcome::NotRunning {
                    message: format!("LSP server '{id}' is not running"),
                },
            });
        }
        if self.init_panicked.load(Ordering::SeqCst) {
            return Some(RestartOutcome::NotRunning {
                message: "startup was interrupted by a panic, so servers cannot be restarted; restart mcpls".to_string(),
            });
        }
        if self.startup_settling.load(Ordering::SeqCst) || self.wiring.get().is_none() {
            return Some(RestartOutcome::Initializing);
        }
        self.restart_cooldown_remaining(id)
            .map(|remaining| RestartOutcome::Throttled {
                retry_in_ms: u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
            })
    }

    async fn restart_one(&self, id: &ServerId, seen: RestartGeneration) -> RestartOutcome {
        let lock = self.respawn_lock(id);
        let _serialized = lock.lock().await;

        if self.restart_generation(id) != seen {
            return self.restarted_outcome(id, true).await;
        }
        if let Some(blocker) = self.restart_blocker(id) {
            return blocker;
        }
        let Some(wiring) = self.wiring.get() else {
            return RestartOutcome::Initializing;
        };

        lock_std(&self.restart_attempts).insert(id.clone(), self.clock.now());
        tracing::info!(%id, "restarting LSP server on request");

        let Some(mut old) = Deregistered::take(self, id) else {
            return RestartOutcome::NotRunning {
                message: format!("LSP server '{id}' is not running"),
            };
        };
        let config = old.terminate().await;
        drop(old);
        let Some(config) = config else {
            return RestartOutcome::NotRunning {
                message: format!("LSP server '{id}' is not running"),
            };
        };
        let language_id = config.server_config.language_id.clone();

        if self.is_shutting_down() {
            self.invalidate_stopped_server(id, &language_id).await;
            return RestartOutcome::Failed {
                reason: RestartFailure::ShuttingDown,
            };
        }
        let respawned = self
            .respawn_locked(
                id,
                config,
                BackoffPolicy::Bypass,
                NotificationRouting::Pump(wiring.as_ref()),
            )
            .await;
        // Counted from completion too, so a slow restart cannot be chained at once.
        lock_std(&self.restart_attempts).insert(id.clone(), self.clock.now());
        if let Err(error) = respawned {
            tracing::warn!(%id, %error, "LSP server restart failed");
            self.invalidate_stopped_server(id, &language_id).await;
            return RestartOutcome::Failed {
                reason: RestartFailure::from_error(&error),
            };
        }

        if self.is_shutting_down() {
            self.discard_registered(id).await;
            return RestartOutcome::Failed {
                reason: RestartFailure::ShuttingDown,
            };
        }
        lock_std(&self.restart_generations)
            .entry(id.clone())
            .or_default()
            .bump();
        tracing::info!(%id, "LSP server restarted");
        self.restarted_outcome(id, false).await
    }

    /// The old process is gone and no replacement runs: drop what it cached
    /// and say so, so its diagnostics are never served as live.
    async fn invalidate_stopped_server(&self, id: &ServerId, language_id: &str) {
        // Best effort: the old pump goes first, and an aborted task stops at its
        // next await, so it cannot re-cache what the dead server buffered
        // once the cache below is cleared.
        let stale_pump = lock_std(&self.notification_tasks).remove(id);
        if let Some(pump) = stale_pump {
            pump.abort();
            tokio::task::yield_now().await;
        }
        let mut cleared = Vec::new();
        if let Some(cache) = &self.notification_cache {
            let mut cache = cache.lock().await;
            cache.reset_indexing_state(id);
            if self.is_diagnostics_route(language_id, id) {
                cleared = cache.clear_server_diagnostics(id);
                cache.mark_push_degraded(id);
            }
        }
        if let Some(wiring) = self.wiring.get() {
            wiring.publish_invalidated(&cleared).await;
        }
    }

    /// Shut down and deregister the server just registered under `id`, after
    /// shutdown began while its replacement was starting.
    async fn discard_registered(&self, id: &ServerId) {
        let server = lock_std(&self.lsp_servers).remove(id);
        lock_std(&self.lsp_clients).remove(id);
        if let Some(server) = server
            && let Err(error) = server.shutdown().await
        {
            tracing::warn!(%id, %error, "replacement LSP server shutdown failed");
        }
    }
}

impl ServerText for RestartFailure {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        match self {
            Self::SpawnFailed { message } | Self::InitializeFailed { message } => {
                redactions.redact_in_place(message);
            }
            Self::ShuttingDown => {}
        }
    }
}

impl ServerText for RestartOutcome {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        match self {
            Self::Restarted {
                coalesced: _,
                indexing_state: _,
                push_notifications_degraded: _,
            }
            | Self::Throttled { retry_in_ms: _ }
            | Self::Initializing => {}
            Self::Failed { reason } => reason.redact_server_text(redactions),
            Self::NotRunning { message } => redactions.redact_in_place(message),
        }
    }
}

impl ServerText for ServerRestartEntry {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            server_id: _,
            outcome,
        } = self;
        outcome.redact_server_text(redactions);
    }
}

impl ServerText for RestartServerResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self { servers } = self;
        servers.redact_server_text(redactions);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::assert_matches;

    use super::*;

    #[test]
    fn server_ids_drop_repeats_keep_order_and_reject_empty() {
        let ids = ServerIds::try_new(vec![
            ServerId::from("b"),
            ServerId::from("a"),
            ServerId::from("b"),
        ])
        .unwrap();
        assert_eq!(ids.as_slice(), [ServerId::from("b"), ServerId::from("a")]);
        assert_eq!(ServerIds::try_new(Vec::new()), Err(ServerIdsError::Empty));
        let ids = |count: usize| {
            (0..count)
                .map(|i| ServerId::from(format!("s{i}")))
                .collect()
        };
        assert!(ServerIds::try_new(ids(MAX_RESTART_SERVER_IDS)).is_ok());
        assert_eq!(
            ServerIds::try_new(ids(MAX_RESTART_SERVER_IDS + 1)),
            Err(ServerIdsError::TooMany)
        );
        assert_eq!(
            ServerIds::try_new(vec![ServerId::from(" ")]),
            Err(ServerIdsError::Blank)
        );
        assert_eq!(
            ServerIds::try_new(vec![ServerId::from("x".repeat(MAX_SERVER_ID_BYTES + 1))]),
            Err(ServerIdsError::TooLong)
        );
    }

    #[test]
    fn restart_entry_flattens_the_outcome_next_to_the_server_id() {
        let entry = ServerRestartEntry {
            server_id: ServerId::from("rust"),
            outcome: RestartOutcome::Failed {
                reason: RestartFailure::SpawnFailed {
                    message: "gone".to_string(),
                },
            },
        };
        assert_eq!(
            serde_json::to_value(entry).unwrap(),
            serde_json::json!({
                "server_id": "rust",
                "status": "failed",
                "reason": {"kind": "spawn_failed", "message": "gone"},
            })
        );
    }

    #[test]
    fn spawn_errors_map_to_spawn_failed_and_the_rest_to_initialize_failed() {
        let not_found = Error::ServerNotFound {
            command: "x".to_string(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        assert_matches!(
            RestartFailure::from_error(&not_found),
            RestartFailure::SpawnFailed { .. }
        );
        assert_matches!(
            RestartFailure::from_error(&Error::Timeout(Duration::from_secs(5))),
            RestartFailure::InitializeFailed { .. }
        );
    }

    #[cfg(unix)]
    mod restart_tests {
        use std::fs;
        use std::path::{Path, PathBuf};

        use tempfile::TempDir;
        use tokio::sync::{Mutex, watch};

        use super::*;
        use crate::PumpShared;
        use crate::bridge::translator::clock::{Clock, FakeClock};
        use crate::bridge::translator::testing::{
            stub_server_config, write_crash_after_init_script, write_protocol_server_script,
            write_responder_script, write_slow_exit_server_script,
        };
        use crate::bridge::{NotificationCache, WorkspaceRoots};
        use crate::config::ToolRouter;
        use crate::error::{ServerSpawnFailure, StartupFailure};
        use crate::mcp::SubscriptionRegistry;

        struct Fixture {
            translator: Arc<Translator>,
            cache: Arc<Mutex<NotificationCache>>,
            clock: Arc<FakeClock>,
            id: ServerId,
            _dir: TempDir,
            _cancel: watch::Sender<bool>,
        }

        fn server_ids(names: &[&str]) -> RestartTarget {
            RestartTarget::Servers(
                ServerIds::try_new(names.iter().map(|n| ServerId::from(*n)).collect()).unwrap(),
            )
        }

        async fn fixture(dir: TempDir, script: &Path, wired: bool) -> Fixture {
            let id = ServerId::from("rust");
            let cache = Arc::new(Mutex::new(NotificationCache::new()));
            let clock = Arc::new(FakeClock::new());
            let mut translator = Translator::new()
                .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]))
                .with_notification_cache(Arc::clone(&cache))
                .with_clock(clock.clone());
            translator.set_workspace_roots(
                WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap(),
            );

            let mut server = LspServer::spawn(stub_server_config("rust", script))
                .await
                .unwrap();
            let receivers = NotificationReceivers {
                notifications: server.take_notification_rx(),
                lifecycle: server.take_lifecycle_rx(),
                pinned_tsserver: None,
            };
            translator.register_server_complete(server);

            let (cancel, cancel_rx) = watch::channel(false);
            let wiring = crate::PumpWiring {
                shared: PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs: SubscriptionRegistry::new(),
                    workspace_roots: translator.workspace_roots.clone(),
                },
                cancel_rx,
            };
            let pump = wiring.spawn_pump(id.clone(), receivers, DiagnosticsRole::Authoritative);
            translator.set_notification_task(id.clone(), pump);
            if wired {
                translator.install_wiring(Arc::new(wiring));
            }
            Fixture {
                translator: Arc::new(translator),
                cache,
                clock,
                id,
                _dir: dir,
                _cancel: cancel,
            }
        }

        async fn protocol_fixture(publish: Option<&str>) -> (Fixture, PathBuf) {
            let dir = TempDir::new().unwrap();
            let log = dir.path().join("server.log");
            let script = write_protocol_server_script(dir.path(), &log, publish);
            (fixture(dir, &script, true).await, log)
        }

        fn read_log(log: &Path) -> String {
            fs::read_to_string(log).unwrap_or_default()
        }

        fn only_outcome(result: &RestartServerResult) -> &RestartOutcome {
            assert_eq!(result.servers.len(), 1, "{result:?}");
            &result.servers[0].outcome
        }

        impl Fixture {
            fn registered(&self) -> bool {
                lock_std(&self.translator.lsp_servers).contains_key(&self.id)
                    && lock_std(&self.translator.lsp_clients).contains_key(&self.id)
            }

            fn expected(&self) -> bool {
                lock_std(&self.translator.expected_servers).contains(&self.id)
            }

            fn is_dead(&self) -> bool {
                lock_std(&self.translator.lsp_servers)
                    .get_mut(&self.id)
                    .unwrap()
                    .is_dead()
                    .unwrap()
            }

            async fn wait_until_deregistered(&self) {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !self.expected() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("the restart must deregister the server");
            }
        }

        #[tokio::test]
        async fn test_restart_replaces_the_process_after_a_graceful_stop() {
            let (fx, log) = protocol_fixture(None).await;

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_matches!(
                only_outcome(&result),
                RestartOutcome::Restarted {
                    coalesced: false,
                    ..
                },
                "{result:?}"
            );
            let log = read_log(&log);
            assert_eq!(log.matches("started").count(), 2, "{log}");
            assert!(log.contains(r#""method":"shutdown""#), "{log}");
            assert!(log.contains(r#""method":"exit""#), "{log}");
            assert!(fx.registered() && !fx.expected() && !fx.is_dead());
        }

        #[tokio::test]
        async fn test_restart_of_a_wedged_server_completes_despite_the_terminate_error() {
            let dir = TempDir::new().unwrap();
            let script = write_responder_script(dir.path(), 600);
            let fx = fixture(dir, &script, true).await;

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_matches!(
                only_outcome(&result),
                RestartOutcome::Restarted { .. },
                "{result:?}"
            );
            assert!(fx.registered() && !fx.expected() && !fx.is_dead());
        }

        #[tokio::test]
        async fn test_cancelled_restart_restores_the_registry() {
            let dir = TempDir::new().unwrap();
            let script = write_responder_script(dir.path(), 600);
            let fx = fixture(dir, &script, true).await;

            let translator = Arc::clone(&fx.translator);
            let task =
                tokio::spawn(async move { translator.restart_servers(RestartTarget::All).await });
            fx.wait_until_deregistered().await;
            assert!(!fx.registered());

            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());

            assert!(fx.registered() && !fx.expected());
        }

        #[tokio::test]
        async fn test_restart_of_a_startup_failed_server_reports_not_running() {
            let id = ServerId::from("rust");
            let translator = Translator::new()
                .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]));
            translator.record_startup_failures(&[ServerSpawnFailure {
                server_id: id.clone(),
                language_id: "rust".to_string(),
                command: "missing".to_string(),
                reason: StartupFailure::InitTaskPanicked,
            }]);
            translator.rebind_router(&HashSet::new());
            translator.clear_expected_servers();

            let result = translator
                .restart_servers(server_ids(&["rust"]))
                .await
                .unwrap();

            assert_matches!(
                only_outcome(&result),
                RestartOutcome::NotRunning { .. },
                "{result:?}"
            );
        }

        #[tokio::test]
        async fn test_restart_of_a_settled_server_while_startup_settles_reports_initializing() {
            let dir = TempDir::new().unwrap();
            let log = dir.path().join("server.log");
            let script = write_protocol_server_script(dir.path(), &log, None);
            let fx = fixture(dir, &script, true).await;
            let startup = fx.translator.begin_startup();

            let during = fx
                .translator
                .restart_servers(server_ids(&["rust"]))
                .await
                .unwrap();
            assert_eq!(*only_outcome(&during), RestartOutcome::Initializing);

            drop(startup);
            let after = fx
                .translator
                .restart_servers(server_ids(&["rust"]))
                .await
                .unwrap();
            assert_matches!(
                only_outcome(&after),
                RestartOutcome::Restarted { .. },
                "{after:?}"
            );
        }

        #[test]
        fn test_dropping_the_startup_guard_clears_the_flag() {
            let translator = Translator::new();
            let startup = translator.begin_startup();
            assert!(translator.startup_settling.load(Ordering::SeqCst));
            drop(startup);
            assert!(!translator.startup_settling.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn test_restart_of_a_startup_failed_server_while_startup_settles_reports_not_running()
        {
            let id = ServerId::from("rust");
            let translator = Translator::new()
                .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]));
            translator.record_startup_failures(&[ServerSpawnFailure {
                server_id: id,
                language_id: "rust".to_string(),
                command: "missing".to_string(),
                reason: StartupFailure::InitTaskPanicked,
            }]);
            let _startup = translator.begin_startup();

            let result = translator
                .restart_servers(server_ids(&["rust"]))
                .await
                .unwrap();

            assert_matches!(
                only_outcome(&result),
                RestartOutcome::NotRunning { .. },
                "{result:?}"
            );
        }

        #[tokio::test]
        async fn test_restart_of_a_still_expected_server_reports_initializing() {
            let id = ServerId::from("rust");
            let translator = Translator::new()
                .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]));
            translator.set_expected_servers(HashSet::from([id]));

            let result = translator
                .restart_servers(server_ids(&["rust"]))
                .await
                .unwrap();

            assert_eq!(*only_outcome(&result), RestartOutcome::Initializing);
        }

        #[tokio::test]
        async fn test_restart_before_the_wiring_is_installed_reports_initializing() {
            let dir = TempDir::new().unwrap();
            let log = dir.path().join("server.log");
            let script = write_protocol_server_script(dir.path(), &log, None);
            let fx = fixture(dir, &script, false).await;

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_eq!(*only_outcome(&result), RestartOutcome::Initializing);
            assert_eq!(read_log(&log).matches("started").count(), 1);
            assert!(fx.registered());
        }

        #[tokio::test]
        async fn test_restart_rejects_unknown_ids_and_restarts_nothing() {
            let (fx, log) = protocol_fixture(None).await;

            let err = fx
                .translator
                .restart_servers(server_ids(&["rust", "nope"]))
                .await
                .unwrap_err();

            let Error::UnknownServers {
                unknown,
                configured,
            } = err
            else {
                panic!("expected UnknownServers, got {err:?}");
            };
            assert_eq!(unknown, [ServerId::from("nope")]);
            assert_eq!(configured, [ServerId::from("rust")]);
            assert_eq!(read_log(&log).matches("started").count(), 1);
        }

        #[tokio::test]
        async fn test_restart_is_throttled_until_the_cooldown_elapses() {
            let (fx, _log) = protocol_fixture(None).await;
            let first = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();
            assert_matches!(only_outcome(&first), RestartOutcome::Restarted { .. });

            let second = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();
            let RestartOutcome::Throttled { retry_in_ms } = only_outcome(&second) else {
                panic!("expected Throttled, got {second:?}");
            };
            assert!((1..=5000).contains(retry_in_ms));

            fx.clock.advance(RESTART_COOLDOWN + Duration::from_secs(1));
            let third = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();
            assert_matches!(only_outcome(&third), RestartOutcome::Restarted { .. });
        }

        #[tokio::test]
        async fn test_concurrent_restarts_of_one_server_coalesce() {
            let (fx, log) = protocol_fixture(None).await;

            let (a, b) = tokio::join!(
                fx.translator.restart_servers(RestartTarget::All),
                fx.translator.restart_servers(RestartTarget::All),
            );

            let outcomes = [a.unwrap(), b.unwrap()];
            let coalesced = outcomes
                .iter()
                .filter(|result| {
                    matches!(
                        only_outcome(result),
                        RestartOutcome::Restarted {
                            coalesced: true,
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(coalesced, 1, "{outcomes:?}");
            assert_eq!(read_log(&log).matches("started").count(), 2);
        }

        #[tokio::test]
        async fn test_restart_clears_degradation_and_keeps_the_early_diagnostics_push() {
            let dir = TempDir::new().unwrap();
            let file = dunce::canonicalize(dir.path()).unwrap().join("main.rs");
            fs::write(&file, "fn main() {}").unwrap();
            let uri = crate::bridge::try_path_to_uri(&file).unwrap();
            let log = dir.path().join("server.log");
            let script = write_protocol_server_script(dir.path(), &log, Some(uri.as_ref()));
            let fx = fixture(dir, &script, true).await;
            {
                let mut cache = fx.cache.lock().await;
                cache.mark_push_degraded(&fx.id);
                cache.clear_server_diagnostics(&fx.id);
            }

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_matches!(
                only_outcome(&result),
                RestartOutcome::Restarted {
                    push_notifications_degraded: false,
                    ..
                },
                "{result:?}"
            );
            tokio::time::timeout(Duration::from_secs(5), async {
                while !fx.cache.lock().await.has_diagnostics(&uri) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the diagnostics pushed during the new handshake must reach the cache");
        }

        #[tokio::test]
        async fn test_restart_fails_requests_in_flight_with_server_restarted() {
            let (fx, _log) = protocol_fixture(None).await;
            let client = lock_std(&fx.translator.lsp_clients)
                .get(&fx.id)
                .cloned()
                .unwrap();
            let in_flight = tokio::spawn(async move {
                client
                    .request::<_, serde_json::Value>(
                        "textDocument/hover",
                        serde_json::json!({}),
                        Duration::from_secs(30),
                    )
                    .await
            });
            tokio::time::sleep(Duration::from_millis(300)).await;

            fx.translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            let failed = tokio::time::timeout(Duration::from_secs(5), in_flight)
                .await
                .expect("the in-flight request must fail promptly")
                .unwrap();
            assert_matches!(&failed, Err(Error::ServerRestarted { server_id }) if *server_id == fx.id, "{failed:?}");
        }

        #[tokio::test]
        async fn test_restart_during_shutdown_starts_nothing() {
            let (fx, log) = protocol_fixture(None).await;
            fx.translator.begin_shutdown();

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_eq!(
                *only_outcome(&result),
                RestartOutcome::Failed {
                    reason: RestartFailure::ShuttingDown
                }
            );
            assert_eq!(read_log(&log).matches("started").count(), 1);
            assert!(fx.registered());
        }

        #[tokio::test]
        async fn test_shutdown_beginning_while_the_old_server_stops_prevents_the_respawn() {
            let dir = TempDir::new().unwrap();
            let script = write_responder_script(dir.path(), 600);
            let fx = fixture(dir, &script, true).await;

            let translator = Arc::clone(&fx.translator);
            let task =
                tokio::spawn(async move { translator.restart_servers(RestartTarget::All).await });
            fx.wait_until_deregistered().await;
            fx.translator.begin_shutdown();

            let result = task.await.unwrap().unwrap();

            assert_eq!(
                *only_outcome(&result),
                RestartOutcome::Failed {
                    reason: RestartFailure::ShuttingDown
                }
            );
            assert!(fx.registered() && !fx.expected() && fx.is_dead());
        }

        #[tokio::test]
        async fn test_failed_replacement_leaves_the_server_registered_and_reports_spawn_failed() {
            let dir = TempDir::new().unwrap();
            let log = dir.path().join("server.log");
            let script = write_protocol_server_script(dir.path(), &log, None);
            let fx = fixture(dir, &script, true).await;
            let mut broken = stub_server_config("rust", &script);
            broken.server_config.command = "mcpls-test-missing-server".to_string();
            lock_std(&fx.translator.lsp_servers)
                .get_mut(&fx.id)
                .unwrap()
                .set_init_config(broken);

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_matches!(
                only_outcome(&result),
                RestartOutcome::Failed {
                    reason: RestartFailure::SpawnFailed { .. }
                },
                "{result:?}"
            );
            assert!(fx.registered() && !fx.expected() && fx.is_dead());
        }

        #[tokio::test]
        async fn test_take_marks_the_server_expected_before_removing_it_from_the_maps() {
            let config = crate::config::LspServerConfig::rust_analyzer();
            let id = config.id();
            let translator = Translator::new();
            translator.register_server_complete(crate::lsp::fake_lsp_server_with_config(config));

            // While the servers map is locked, `take` is blocked on removing the server;
            // by then the id must already be expected, or callers would find it nowhere.
            let servers = lock_std(&translator.lsp_servers);
            std::thread::scope(|scope| {
                let taker = scope.spawn(|| Deregistered::take(&translator, &id).is_some());
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while !lock_std(&translator.expected_servers).contains(&id) {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "take must insert into expected_servers before removing from the maps"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                drop(servers);
                assert!(taker.join().unwrap());
            });
            assert!(!lock_std(&translator.expected_servers).contains(&id));
            assert!(lock_std(&translator.lsp_servers).contains_key(&id));
        }

        /// Six servers, two overlapping `All` requests: ids queued behind the
        /// concurrency limit must snapshot their generation up front, so each
        /// server is restarted exactly once and the other request coalesces.
        #[tokio::test]
        async fn test_overlapping_requests_over_more_than_four_servers_restart_each_once() {
            let dir = TempDir::new().unwrap();
            let cache = Arc::new(Mutex::new(NotificationCache::new()));
            let mut translator = Translator::new().with_notification_cache(Arc::clone(&cache));
            translator.set_workspace_roots(
                WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap(),
            );
            let (_cancel, cancel_rx) = watch::channel(false);
            let wiring = crate::PumpWiring {
                shared: PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs: SubscriptionRegistry::new(),
                    workspace_roots: translator.workspace_roots.clone(),
                },
                cancel_rx,
            };
            let mut logs = Vec::new();
            for index in 0..6 {
                let name = format!("s{index}");
                let log = dir.path().join(format!("{name}.log"));
                let script_dir = dir.path().join(&name);
                fs::create_dir(&script_dir).unwrap();
                let script = write_protocol_server_script(&script_dir, &log, None);
                let mut server = LspServer::spawn(stub_server_config(&name, &script))
                    .await
                    .unwrap();
                let receivers = NotificationReceivers {
                    notifications: server.take_notification_rx(),
                    lifecycle: server.take_lifecycle_rx(),
                    pinned_tsserver: None,
                };
                translator.register_server_complete(server);
                let id = ServerId::from(name.as_str());
                translator.set_notification_task(
                    id.clone(),
                    wiring.spawn_pump(id, receivers, DiagnosticsRole::Secondary),
                );
                logs.push(log);
            }
            translator.install_wiring(Arc::new(wiring));

            let (a, b) = tokio::join!(
                translator.restart_servers(RestartTarget::All),
                translator.restart_servers(RestartTarget::All),
            );
            let (a, b) = (a.unwrap(), b.unwrap());

            assert_eq!((a.servers.len(), b.servers.len()), (6, 6));
            for (first, second) in a.servers.iter().zip(&b.servers) {
                let coalesced = |entry: &ServerRestartEntry| match entry.outcome {
                    RestartOutcome::Restarted { coalesced, .. } => coalesced,
                    ref other => panic!("{}: unexpected outcome {other:?}", entry.server_id),
                };
                assert_ne!(
                    coalesced(first),
                    coalesced(second),
                    "{}: exactly one request restarts it",
                    first.server_id
                );
            }
            for log in logs {
                assert_eq!(read_log(&log).matches("started").count(), 2, "{log:?}");
            }
        }

        #[tokio::test]
        async fn test_an_automatic_respawn_does_not_count_towards_coalescing() {
            let dir = TempDir::new().unwrap();
            let script = write_crash_after_init_script(dir.path());
            let fx = fixture(dir, &script, true).await;
            tokio::time::timeout(Duration::from_secs(3), async {
                while !fx.is_dead() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the seed server crashes after initialize");
            let before = fx.translator.restart_generation(&fx.id);
            fx.translator.respawn_if_dead(&fx.id).await.unwrap();
            assert_eq!(fx.translator.restart_generation(&fx.id), before);

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_matches!(
                only_outcome(&result),
                RestartOutcome::Restarted {
                    coalesced: false,
                    ..
                },
                "{result:?}"
            );
        }

        #[tokio::test]
        async fn test_an_unregistered_server_is_never_throttled() {
            let id = ServerId::from("rust");
            let clock = Arc::new(FakeClock::new());
            let translator = Translator::new().with_clock(clock.clone());
            lock_std(&translator.restart_attempts).insert(id.clone(), clock.now());

            translator.set_expected_servers(HashSet::from([id.clone()]));
            let starting = translator
                .restart_servers(server_ids(&["rust"]))
                .await
                .unwrap();
            assert_eq!(*only_outcome(&starting), RestartOutcome::Initializing);

            translator.clear_expected_servers();
            translator.record_startup_failures(&[ServerSpawnFailure {
                server_id: id,
                language_id: "rust".to_string(),
                command: "missing".to_string(),
                reason: StartupFailure::InitTaskPanicked,
            }]);
            let failed = translator
                .restart_servers(server_ids(&["rust"]))
                .await
                .unwrap();
            assert_matches!(
                only_outcome(&failed),
                RestartOutcome::NotRunning { .. },
                "{failed:?}"
            );
        }

        #[tokio::test]
        async fn test_restart_after_an_init_panic_without_wiring_reports_not_running() {
            let dir = TempDir::new().unwrap();
            let log = dir.path().join("server.log");
            let script = write_protocol_server_script(dir.path(), &log, None);
            let fx = fixture(dir, &script, false).await;
            fx.translator.settle_after_init_panic(&[]).await;

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_matches!(
                only_outcome(&result),
                RestartOutcome::NotRunning { .. },
                "{result:?}"
            );
        }

        #[tokio::test]
        async fn test_restart_after_an_init_panic_with_wiring_installed_reports_not_running() {
            let dir = TempDir::new().unwrap();
            let log = dir.path().join("server.log");
            let script = write_protocol_server_script(dir.path(), &log, None);
            let fx = fixture(dir, &script, true).await;
            fx.translator.settle_after_init_panic(&[]).await;

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_matches!(
                only_outcome(&result),
                RestartOutcome::NotRunning { .. },
                "{result:?}"
            );
        }

        #[tokio::test]
        async fn test_a_failed_restart_invalidates_the_stopped_servers_cached_diagnostics() {
            let dir = TempDir::new().unwrap();
            let file = dunce::canonicalize(dir.path()).unwrap().join("main.rs");
            fs::write(&file, "fn main() {}").unwrap();
            let uri = crate::bridge::try_path_to_uri(&file).unwrap();
            let log = dir.path().join("server.log");
            let script = write_protocol_server_script(dir.path(), &log, Some(uri.as_ref()));
            let fx = fixture(dir, &script, true).await;
            tokio::time::timeout(Duration::from_secs(5), async {
                while !fx.cache.lock().await.has_diagnostics(&uri) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the seed server's diagnostics reach the cache");
            let mut broken = stub_server_config("rust", &script);
            broken.server_config.command = "mcpls-test-missing-server".to_string();
            lock_std(&fx.translator.lsp_servers)
                .get_mut(&fx.id)
                .unwrap()
                .set_init_config(broken);

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_matches!(only_outcome(&result), RestartOutcome::Failed { .. });
            let (stale, degraded) = {
                let cache = fx.cache.lock().await;
                (cache.has_diagnostics(&uri), cache.is_push_degraded(&fx.id))
            };
            assert!(!stale, "stale diagnostics must go");
            assert!(degraded);
            assert!(
                !lock_std(&fx.translator.notification_tasks).contains_key(&fx.id),
                "the dead server's pump handle must be dropped"
            );
        }

        #[tokio::test]
        async fn test_cooldown_counts_from_the_completion_of_a_slow_restart() {
            let dir = TempDir::new().unwrap();
            let script = write_responder_script(dir.path(), 600);
            let fx = fixture(dir, &script, true).await;
            let translator = Arc::clone(&fx.translator);
            let restart =
                tokio::spawn(async move { translator.restart_servers(RestartTarget::All).await });
            fx.wait_until_deregistered().await;
            fx.clock.advance(Duration::from_secs(30));

            restart.await.unwrap().unwrap();
            let again = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_matches!(
                only_outcome(&again),
                RestartOutcome::Throttled { .. },
                "{again:?}"
            );
        }

        #[tokio::test]
        async fn test_a_request_queued_behind_a_discarded_restart_is_not_reported_coalesced() {
            let dir = TempDir::new().unwrap();
            let script = write_responder_script(dir.path(), 600);
            let fx = fixture(dir, &script, true).await;
            let (first_tx, second_tx) = (Arc::clone(&fx.translator), Arc::clone(&fx.translator));
            let first =
                tokio::spawn(async move { first_tx.restart_servers(RestartTarget::All).await });
            fx.wait_until_deregistered().await;
            let second =
                tokio::spawn(async move { second_tx.restart_servers(RestartTarget::All).await });
            tokio::time::sleep(Duration::from_millis(100)).await;
            fx.translator.begin_shutdown();

            for result in [
                first.await.unwrap().unwrap(),
                second.await.unwrap().unwrap(),
            ] {
                assert_eq!(
                    *only_outcome(&result),
                    RestartOutcome::Failed {
                        reason: RestartFailure::ShuttingDown
                    }
                );
            }
        }

        /// A server that answered `shutdown` but needs longer than the
        /// handshake budget to exit is not killed at the budget.
        #[tokio::test]
        async fn test_a_healthy_server_slow_to_exit_is_not_killed_at_the_handshake_budget() {
            let dir = TempDir::new().unwrap();
            let log = dir.path().join("server.log");
            let delay = u32::try_from(RESTART_HANDSHAKE_BUDGET.as_secs()).unwrap() + 1;
            let script = write_slow_exit_server_script(dir.path(), &log, delay);
            let fx = fixture(dir, &script, true).await;

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_matches!(
                only_outcome(&result),
                RestartOutcome::Restarted { .. },
                "{result:?}"
            );
            assert!(
                read_log(&log).contains("exiting"),
                "the old server must have exited by itself, not been killed"
            );
        }

        #[tokio::test]
        async fn test_restart_recovers_a_crashed_server() {
            let dir = TempDir::new().unwrap();
            let script = write_crash_after_init_script(dir.path());
            let fx = fixture(dir, &script, true).await;
            tokio::time::timeout(Duration::from_secs(3), async {
                while !fx.is_dead() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the seed server crashes after initialize");

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_matches!(
                only_outcome(&result),
                RestartOutcome::Restarted { .. },
                "{result:?}"
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod server_text_tests {
    use super::*;

    #[test]
    fn test_restart_messages_are_redacted() {
        let secret = "SuperSecretValue123";
        let set = Redactions::new([("API_TOKEN".to_owned(), secret.to_owned())]);
        let mut result = RestartServerResult {
            servers: vec![
                ServerRestartEntry {
                    server_id: ServerId::from("a"),
                    outcome: RestartOutcome::Failed {
                        reason: RestartFailure::SpawnFailed {
                            message: format!("spawn {secret}"),
                        },
                    },
                },
                ServerRestartEntry {
                    server_id: ServerId::from("b"),
                    outcome: RestartOutcome::Failed {
                        reason: RestartFailure::InitializeFailed {
                            message: format!("init {secret}"),
                        },
                    },
                },
                ServerRestartEntry {
                    server_id: ServerId::from("c"),
                    outcome: RestartOutcome::NotRunning {
                        message: format!("gone {secret}"),
                    },
                },
            ],
        };

        result.redact_server_text(&set);

        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains(secret), "{json}");
        assert_eq!(json.matches("[redacted:API_TOKEN]").count(), 3, "{json}");
    }
}
