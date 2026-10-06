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
use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use super::Translator;
use super::respawn::BackoffPolicy;
use super::servers::{Backend, Phase};
use crate::bridge::indexing::IndexingReset;
use crate::bridge::{DiagnosticsKey, IndexingState};
use crate::config::{LanguageId, ServerId};
use crate::error::{Error, Result};
use crate::lsp::{ExitGrace, LspNotification, ServerInitConfig};
use crate::redaction::{Redactions, ServerText};
use crate::util::lock_std;

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

/// Why a list of server ids was rejected by [`ServerIds::try_new`] or [`ServerIds::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ServerIdsError {
    /// No ids were given.
    #[error("`servers` must not be empty")]
    Empty,
    /// More than [`MAX_RESTART_SERVER_IDS`] ids were given.
    #[error("`servers` may name at most {MAX_RESTART_SERVER_IDS} servers")]
    TooMany,
    /// An id was empty or whitespace; only [`ServerIds::parse`] returns it.
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
/// let ids = ServerIds::try_new(vec![ServerId::from_static("rust"), ServerId::from_static("rust")]).unwrap();
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
    /// [`MAX_RESTART_SERVER_IDS`] entries, or holds an oversized id.
    pub fn try_new(ids: Vec<ServerId>) -> std::result::Result<Self, ServerIdsError> {
        if ids.len() > MAX_RESTART_SERVER_IDS {
            return Err(ServerIdsError::TooMany);
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

    /// Parse the ids of a request, checking the count, blankness and length
    /// in that order, then drop repeats like [`Self::try_new`].
    ///
    /// # Errors
    ///
    /// [`ServerIdsError`] when `ids` is empty, has more than
    /// [`MAX_RESTART_SERVER_IDS`] entries, or holds a blank or oversized id.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::bridge::{ServerIds, ServerIdsError};
    ///
    /// assert_eq!(ServerIds::parse(vec![" ".to_owned()]), Err(ServerIdsError::Blank));
    /// assert_eq!(ServerIds::parse(vec!["rust".to_owned()]).unwrap().as_slice().len(), 1);
    /// ```
    pub fn parse(ids: Vec<String>) -> std::result::Result<Self, ServerIdsError> {
        if ids.len() > MAX_RESTART_SERVER_IDS {
            return Err(ServerIdsError::TooMany);
        }
        let ids = ids
            .into_iter()
            .map(|id| ServerId::new(id).map_err(|_| ServerIdsError::Blank))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Self::try_new(ids)
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
/// let one = RestartTarget::Servers(ServerIds::try_new(vec![ServerId::from_static("rust")]).unwrap());
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
            | Error::NoServerForLanguage { .. }
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
            | Error::ConfigInsideWorkspace { .. }
            | Error::Config(_)
            | Error::TomlDe(_)
            | Error::TomlSer(_)
            | Error::NoResolvableListenUris
            | Error::ResourceUri(_)
            | Error::InvalidPositionInput(_)
            | Error::InvalidRangeInput(_)
            | Error::InvalidHierarchyItemInput(_)
            | Error::PositionBeyondDocument { .. }
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

/// Whether a server's `publishDiagnostics` pushes are the ones the cache
/// keeps for its language -- see #174 section 8.
///
/// The role is re-evaluated whenever a server settles: a catch-all that
/// registered as `Secondary` becomes `Authoritative` when the explicit
/// diagnostics server for its language fails afterwards. Pushes the catch-all
/// made while it was `Secondary` are not replayed; the cache fills with its
/// next publish for each file (the next `didOpen`/`didChange`). A file nobody
/// opens may never get one, since mcpls sends no `didSave`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticsRole {
    /// The server is the language's diagnostics route: its pushes are cached
    /// and subscribers are notified.
    Authoritative,
    /// Another server owns the language's diagnostics (or none does): pushes
    /// are skipped so they cannot overwrite or spuriously notify about the
    /// owner's entries.
    Secondary,
}

impl DiagnosticsRole {
    /// The role of a server that is, or is not, the language's diagnostics route.
    pub(crate) const fn from_route(is_diagnostics_route: bool) -> Self {
        if is_diagnostics_route {
            Self::Authoritative
        } else {
            Self::Secondary
        }
    }
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
    /// re-read them: after a server's entries were cleared, and after a pull
    /// write evicted other files' entries.
    fn publish_invalidated<'a>(&'a self, cleared: &'a [DiagnosticsKey]) -> BoxFuture<'a, ()>;

    /// Whether any session holds a subscription at all, so a caller can skip
    /// work that only matters to subscribers. Defaults to `true`, which only
    /// forgoes the saving.
    fn has_subscriptions(&self) -> BoxFuture<'_, bool> {
        Box::pin(async { true })
    }

    /// Tell subscribers of the resource behind `file`, the canonical URI of a
    /// tracked file, that a `textDocument/diagnostic` report changed what a
    /// read of it returns.
    fn publish_changed<'a>(&'a self, file: &'a lsp_types::Uri) -> BoxFuture<'a, ()>;
}

/// Keeps restarts reporting `initializing` until dropped, so an init panic,
/// task abort or early return can never leave the flag set.
#[derive(Debug)]
pub struct StartupGuard<'a>(&'a Translator);

impl Drop for StartupGuard<'_> {
    fn drop(&mut self) {
        lock_std(&self.0.phase).finish_startup();
    }
}

/// A server taken out of its slot for termination, restored on every exit
/// path (error, early return, panic, cancelled future).
struct Deregistered<'a> {
    translator: &'a Translator,
    id: ServerId,
    backend: Option<Backend>,
}

impl<'a> Deregistered<'a> {
    /// Marks `id` restarting while taking its backend, in one step, so no
    /// caller ever finds it in neither state (which would read as a missing
    /// server).
    fn take(translator: &'a Translator, id: &ServerId) -> Option<Self> {
        let backend = lock_std(&translator.servers).take_for_restart(id)?;
        let held = Self {
            translator,
            id: id.clone(),
            backend: Some(backend),
        };
        held.backend
            .as_ref()
            .is_some_and(|backend| backend.server().is_some())
            .then_some(held)
    }

    /// Stop the held server and return the config to respawn it from.
    /// Termination errors (a wedged server times out) are not fatal.
    async fn terminate(&mut self) -> Option<ServerInitConfig> {
        let backend = self.backend.as_mut()?;
        backend.client().mark_restarted(self.id.clone());
        let server = backend.server_mut()?;
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

    /// Put the held backend back; idempotent.
    fn restore(&mut self) {
        if let Some(backend) = self.backend.take() {
            let refused = lock_std(&self.translator.servers).restore(&self.id, backend);
            if refused.is_some() {
                tracing::warn!(id = %self.id, "restored server dropped: its slot was settled meanwhile");
            }
            drop(refused);
        }
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
        let mut ids: Vec<ServerId> = lock_std(&self.servers).ids().cloned().collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids
    }

    fn restart_generation(&self, id: &ServerId) -> RestartGeneration {
        lock_std(&self.servers)
            .get(id)
            .map_or_default(|slot| slot.restart.generation)
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
    pub(crate) fn set_notification_task(&self, id: &ServerId, handle: AbortHandle) {
        if let Some(slot) = lock_std(&self.servers).get_mut(id) {
            slot.notification_task = Some(handle);
        }
    }

    /// Declare that the initial server startup is still settling; restarts
    /// report `initializing` until the returned guard is dropped, because a
    /// restarted pump's diagnostics role is fixed at spawn and routes still
    /// change while servers settle.
    pub(crate) fn begin_startup(&self) -> StartupGuard<'_> {
        lock_std(&self.phase).begin_startup();
        StartupGuard(self)
    }

    /// Declare that shutdown has begun; no restart starts a server after this.
    pub(crate) fn begin_shutdown(&self) {
        lock_std(&self.phase).begin_shutdown();
    }

    fn is_shutting_down(&self) -> bool {
        *lock_std(&self.phase) == Phase::ShuttingDown
    }

    /// The cooldown still to wait before `id` may be restarted again.
    fn restart_cooldown_remaining(&self, id: &ServerId) -> Option<Duration> {
        let last = lock_std(&self.servers).get(id)?.restart.last_attempt?;
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

    /// The notification wiring a restart of `id` needs, or why it cannot be
    /// restarted right now, in classification order: shutting down, never
    /// started or failed to start, init panicked, startup still settling or not
    /// yet wired, then the cooldown.
    fn restart_permit(
        &self,
        id: &ServerId,
    ) -> std::result::Result<&Arc<dyn NotificationWiring>, RestartOutcome> {
        let phase = *lock_std(&self.phase);
        if phase == Phase::ShuttingDown {
            return Err(RestartOutcome::Failed {
                reason: RestartFailure::ShuttingDown,
            });
        }
        let not_running = {
            let servers = lock_std(&self.servers);
            let outcome = servers
                .server(id)
                .is_none()
                .then(|| match servers.failure(id) {
                    Some(failure) => RestartOutcome::NotRunning {
                        message: Error::ServerFailedToStart(Box::new(failure.clone())).to_string(),
                    },
                    None if servers.is_expected(id) => RestartOutcome::Initializing,
                    None => RestartOutcome::NotRunning {
                        message: format!("LSP server '{id}' is not running"),
                    },
                });
            drop(servers);
            outcome
        };
        if let Some(outcome) = not_running {
            return Err(outcome);
        }
        if phase == Phase::InitPanicked {
            return Err(RestartOutcome::NotRunning {
                message: "startup was interrupted by a panic, so servers cannot be restarted; restart mcpls".to_string(),
            });
        }
        let wiring = match (phase, self.wiring.get()) {
            (Phase::Settling, _) | (_, None) => return Err(RestartOutcome::Initializing),
            (_, Some(wiring)) => wiring,
        };
        self.restart_cooldown_remaining(id)
            .map_or(Ok(wiring), |remaining| {
                Err(RestartOutcome::Throttled {
                    retry_in_ms: u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
                })
            })
    }

    async fn restart_one(&self, id: &ServerId, seen: RestartGeneration) -> RestartOutcome {
        let lock = self.respawn_lock(id);
        let _serialized = lock.lock().await;

        if self.restart_generation(id) != seen {
            return self.restarted_outcome(id, true).await;
        }
        let wiring = match self.restart_permit(id) {
            Ok(wiring) => wiring,
            Err(outcome) => return outcome,
        };

        self.note_restart_attempt(id);
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
        let language_id = config.server_config().language_id.clone();

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
        self.note_restart_attempt(id);
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
        if let Some(slot) = lock_std(&self.servers).get_mut(id) {
            slot.restart.generation.bump();
        }
        tracing::info!(%id, "LSP server restarted");
        self.restarted_outcome(id, false).await
    }

    fn note_restart_attempt(&self, id: &ServerId) {
        if let Some(slot) = lock_std(&self.servers).get_mut(id) {
            slot.restart.last_attempt = Some(self.clock.now());
        }
    }

    /// The old process is gone and no replacement runs: drop what it cached
    /// and say so, so its diagnostics are never served as live.
    async fn invalidate_stopped_server(&self, id: &ServerId, language_id: &LanguageId) {
        // Best effort: the old pump goes first, and an aborted task stops at its
        // next await, so it cannot re-cache what the dead server buffered
        // once the cache below is cleared.
        let stale_pump = lock_std(&self.servers)
            .get_mut(id)
            .and_then(|slot| slot.notification_task.take());
        if let Some(pump) = stale_pump {
            pump.abort();
            tokio::task::yield_now().await;
        }
        let mut cleared = Vec::new();
        if let Some(cache) = &self.notification_cache {
            let mut cache = cache.lock().await;
            cache.reset_indexing_state(id, IndexingReset::Forget);
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
        let server = lock_std(&self.servers).remove_server(id);
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
mod tests {
    use std::assert_matches;

    use super::*;

    #[test]
    fn server_ids_drop_repeats_keep_order_and_reject_empty() {
        let ids = ServerIds::try_new(vec![
            ServerId::from_static("b"),
            ServerId::from_static("a"),
            ServerId::from_static("b"),
        ])
        .unwrap();
        assert_eq!(
            ids.as_slice(),
            [ServerId::from_static("b"), ServerId::from_static("a")]
        );
        assert_eq!(ServerIds::try_new(Vec::new()), Err(ServerIdsError::Empty));
        let ids = |count: usize| {
            (0..count)
                .map(|i| ServerId::new(format!("s{i}")).unwrap())
                .collect()
        };
        assert!(ServerIds::try_new(ids(MAX_RESTART_SERVER_IDS)).is_ok());
        assert_eq!(
            ServerIds::try_new(ids(MAX_RESTART_SERVER_IDS + 1)),
            Err(ServerIdsError::TooMany)
        );
        assert_eq!(
            ServerIds::parse(vec![" ".to_owned()]),
            Err(ServerIdsError::Blank)
        );
        assert_eq!(
            ServerIds::try_new(vec![
                ServerId::new("x".repeat(MAX_SERVER_ID_BYTES + 1)).unwrap()
            ]),
            Err(ServerIdsError::TooLong)
        );
        assert_eq!(
            ServerIds::parse(vec![" ".to_owned(); MAX_RESTART_SERVER_IDS + 1]),
            Err(ServerIdsError::TooMany)
        );
        assert_eq!(
            ServerIds::parse(vec!["x".repeat(MAX_SERVER_ID_BYTES + 1), " ".to_owned()]),
            Err(ServerIdsError::Blank)
        );
    }

    #[test]
    fn restart_entry_flattens_the_outcome_next_to_the_server_id() {
        let entry = ServerRestartEntry {
            server_id: ServerId::from_static("rust"),
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
        use crate::bridge::translator::clock::{Clock, FakeClock};
        use crate::bridge::translator::testing::{
            stub_server_config, write_crash_after_init_script, write_protocol_server_script,
            write_responder_script, write_slow_exit_server_script,
        };
        use crate::bridge::{NotificationCache, WorkspaceRoots};
        use crate::config::{LanguageId, ServerCommand, ToolRouter};
        use crate::error::{ServerSpawnFailure, StartupFailure};
        use crate::mcp::SubscriptionRegistry;
        use crate::runtime::pump::{PumpShared, PumpWiring};

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
                ServerIds::try_new(names.iter().map(|n| ServerId::new(*n).unwrap()).collect())
                    .unwrap(),
            )
        }

        async fn fixture(dir: TempDir, script: &Path, wired: bool) -> Fixture {
            let id = ServerId::from_static("rust");
            let cache = Arc::new(Mutex::new(NotificationCache::new()));
            let clock = Arc::new(FakeClock::new());
            let mut translator = Translator::new()
                .with_router(ToolRouter::catch_all([(
                    id.clone(),
                    LanguageId::from_static("rust"),
                )]))
                .with_notification_cache(Arc::clone(&cache))
                .with_clock(clock.clone());
            translator.set_workspace_roots(
                WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap(),
            );

            let mut server = crate::lsp::LspServer::spawn(stub_server_config("rust", script))
                .await
                .unwrap();
            let receivers = NotificationReceivers {
                notifications: server.take_notification_rx(),
                lifecycle: server.take_lifecycle_rx(),
                pinned_tsserver: None,
            };
            translator.register_server_complete(server);

            let (cancel, cancel_rx) = watch::channel(false);
            let wiring = PumpWiring::new(
                PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs: SubscriptionRegistry::new(),
                    workspace_roots: translator.workspace_roots.clone(),
                },
                cancel_rx,
            );
            let pump = wiring.spawn_pump(id.clone(), receivers, DiagnosticsRole::Authoritative);
            translator.set_notification_task(&id, pump);
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
                let servers = lock_std(&self.translator.servers);
                servers.server(&self.id).is_some() && servers.client(&self.id).is_some()
            }

            fn expected(&self) -> bool {
                lock_std(&self.translator.servers).is_expected(&self.id)
            }

            fn is_dead(&self) -> bool {
                lock_std(&self.translator.servers)
                    .server_mut(&self.id)
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
            let id = ServerId::from_static("rust");
            let translator = Translator::new().with_router(ToolRouter::catch_all([(
                id.clone(),
                LanguageId::from_static("rust"),
            )]));
            translator.record_startup_failures(&[ServerSpawnFailure {
                server_id: id.clone(),
                language_id: LanguageId::from_static("rust"),
                command: ServerCommand::from_static("missing"),
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
        async fn test_restart_all_reports_a_refused_server_as_not_running() {
            let config = crate::config::LspServerConfig::rust_analyzer();
            let translator =
                Translator::new().with_router(ToolRouter::from_configs([&config]).unwrap());
            translator.record_refusals(&[ServerSpawnFailure {
                server_id: config.id(),
                language_id: config.language_id.clone(),
                command: config.command.server_command().clone(),
                reason: StartupFailure::RefusedUntrustedWorkspace(
                    crate::error::UntrustedRefusal::NotAllowed { builtin: None },
                ),
            }]);

            let result = translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            let outcome = only_outcome(&result);
            assert_matches!(
                outcome,
                RestartOutcome::NotRunning { message } if message.contains("--allow-server rust"),
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

        #[tokio::test]
        async fn test_restart_permit_classifies_in_order() {
            let id = ServerId::from_static("rust");
            let translator = Translator::new();
            assert_matches!(
                translator.restart_permit(&id),
                Err(RestartOutcome::NotRunning { .. })
            );

            translator.set_expected_servers(HashSet::from([id.clone()]));
            assert_matches!(
                translator.restart_permit(&id),
                Err(RestartOutcome::Initializing)
            );

            translator.clear_expected_servers();
            translator.record_startup_failures(&[ServerSpawnFailure {
                server_id: id.clone(),
                language_id: crate::config::LanguageId::from_static("rust"),
                command: ServerCommand::from_static("missing"),
                reason: StartupFailure::InitTaskPanicked,
            }]);
            assert_matches!(
                translator.restart_permit(&id),
                Err(RestartOutcome::NotRunning { .. })
            );

            let config = crate::config::LspServerConfig::rust_analyzer();
            let running = Translator::new();
            running
                .register_server_complete(crate::lsp::fake_lsp_server_with_config(config.clone()));
            let rid = config.id();
            assert_matches!(
                running.restart_permit(&rid),
                Err(RestartOutcome::Initializing)
            );

            lock_std(&running.phase).init_panicked();
            assert_matches!(
                running.restart_permit(&rid),
                Err(RestartOutcome::NotRunning { .. })
            );

            running.begin_shutdown();
            assert_matches!(
                running.restart_permit(&rid),
                Err(RestartOutcome::Failed {
                    reason: RestartFailure::ShuttingDown
                })
            );
        }

        #[test]
        fn test_dropping_the_startup_guard_clears_the_flag() {
            let translator = Translator::new();
            let startup = translator.begin_startup();
            assert_eq!(*lock_std(&translator.phase), Phase::Settling);
            drop(startup);
            assert_eq!(*lock_std(&translator.phase), Phase::Settled);
        }

        #[tokio::test]
        async fn test_restart_of_a_startup_failed_server_while_startup_settles_reports_not_running()
        {
            let id = ServerId::from_static("rust");
            let translator = Translator::new().with_router(ToolRouter::catch_all([(
                id.clone(),
                LanguageId::from_static("rust"),
            )]));
            translator.record_startup_failures(&[ServerSpawnFailure {
                server_id: id,
                language_id: LanguageId::from_static("rust"),
                command: ServerCommand::from_static("missing"),
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
            let id = ServerId::from_static("rust");
            let translator = Translator::new().with_router(ToolRouter::catch_all([(
                id.clone(),
                LanguageId::from_static("rust"),
            )]));
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
            assert_eq!(unknown, [ServerId::from_static("nope")]);
            assert_eq!(configured, [ServerId::from_static("rust")]);
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
            let client = lock_std(&fx.translator.servers).client(&fx.id).unwrap();
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
            broken.server_config_mut().command =
                ServerCommand::from_static("mcpls-test-missing-server").into();
            lock_std(&fx.translator.servers)
                .server_mut(&fx.id)
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
        async fn test_take_leaves_the_server_expected_in_the_same_step_and_restore_runs_it_again() {
            let config = crate::config::LspServerConfig::rust_analyzer();
            let id = config.id();
            let translator = Translator::new();
            translator.register_server_complete(crate::lsp::fake_lsp_server_with_config(config));

            let held = Deregistered::take(&translator, &id).unwrap();
            {
                let servers = lock_std(&translator.servers);
                assert!(
                    servers.is_expected(&id),
                    "the id must never be in neither state"
                );
                assert!(servers.server(&id).is_none());
                drop(servers);
            }
            drop(held);

            let servers = lock_std(&translator.servers);
            assert!(!servers.is_expected(&id));
            assert!(servers.server(&id).is_some());
            drop(servers);
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
                WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap(),
            );
            let (_cancel, cancel_rx) = watch::channel(false);
            let wiring = PumpWiring::new(
                PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs: SubscriptionRegistry::new(),
                    workspace_roots: translator.workspace_roots.clone(),
                },
                cancel_rx,
            );
            let mut logs = Vec::new();
            for index in 0..6 {
                let name = format!("s{index}");
                let log = dir.path().join(format!("{name}.log"));
                let script_dir = dir.path().join(&name);
                fs::create_dir(&script_dir).unwrap();
                let script = write_protocol_server_script(&script_dir, &log, None);
                let mut server = crate::lsp::LspServer::spawn(stub_server_config(&name, &script))
                    .await
                    .unwrap();
                let receivers = NotificationReceivers {
                    notifications: server.take_notification_rx(),
                    lifecycle: server.take_lifecycle_rx(),
                    pinned_tsserver: None,
                };
                translator.register_server_complete(server);
                let id = ServerId::new(name.as_str()).unwrap();
                let pump = wiring.spawn_pump(id.clone(), receivers, DiagnosticsRole::Secondary);
                translator.set_notification_task(&id, pump);
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
            let id = ServerId::from_static("rust");
            let clock = Arc::new(FakeClock::new());
            let translator = Translator::new().with_clock(clock.clone());
            translator.set_expected_servers(HashSet::from([id.clone()]));
            lock_std(&translator.servers)
                .get_mut(&id)
                .unwrap()
                .restart
                .last_attempt = Some(clock.now());
            let starting = translator
                .restart_servers(server_ids(&["rust"]))
                .await
                .unwrap();
            assert_eq!(*only_outcome(&starting), RestartOutcome::Initializing);

            translator.clear_expected_servers();
            translator.record_startup_failures(&[ServerSpawnFailure {
                server_id: id,
                language_id: LanguageId::from_static("rust"),
                command: ServerCommand::from_static("missing"),
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
            broken.server_config_mut().command =
                ServerCommand::from_static("mcpls-test-missing-server").into();
            lock_std(&fx.translator.servers)
                .server_mut(&fx.id)
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
                lock_std(&fx.translator.servers)
                    .get(&fx.id)
                    .is_none_or(|slot| slot.notification_task.is_none()),
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

        /// #667: right after a restart of a server that reported readiness
        /// before, the result says `loading` and a gated call holds until the
        /// replacement's own signal arrives instead of reading an empty index.
        #[tokio::test]
        async fn test_restart_of_a_signalling_server_gates_until_its_signal() {
            let (fx, _log) = protocol_fixture(None).await;
            fx.cache.lock().await.observe_indexing_signal(
                &fx.id,
                "experimental/serverStatus",
                Some(&serde_json::json!({"quiescent": true})),
            );

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();
            assert_matches!(
                only_outcome(&result),
                RestartOutcome::Restarted {
                    indexing_state: IndexingState::Loading,
                    ..
                },
                "{result:?}"
            );

            let translator = Arc::clone(&fx.translator);
            let id = fx.id.clone();
            let gated = tokio::spawn(async move { translator.wait_for_indexing_ready(&id).await });
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert!(!gated.is_finished(), "the gate must hold before the signal");

            fx.cache.lock().await.observe_indexing_signal(
                &fx.id,
                "experimental/serverStatus",
                Some(&serde_json::json!({"quiescent": true})),
            );
            tokio::time::timeout(Duration::from_secs(5), gated)
                .await
                .expect("the gate releases once the replacement is ready")
                .unwrap()
                .unwrap();
        }

        /// #667: a server that never reported a signal is restarted without a
        /// gate (no new fixed delay).
        #[tokio::test]
        async fn test_restart_of_a_silent_server_reports_unknown() {
            let (fx, _log) = protocol_fixture(None).await;

            let result = fx
                .translator
                .restart_servers(RestartTarget::All)
                .await
                .unwrap();

            assert_matches!(
                only_outcome(&result),
                RestartOutcome::Restarted {
                    indexing_state: IndexingState::Unknown,
                    ..
                },
                "{result:?}"
            );
            fx.translator.wait_for_indexing_ready(&fx.id).await.unwrap();
        }
    }
}

#[cfg(test)]
mod server_text_tests {
    use super::*;

    #[test]
    fn test_restart_messages_are_redacted() {
        let secret = "SuperSecretValue123";
        let set = Redactions::new([("API_TOKEN".to_owned(), secret.to_owned())]);
        let mut result = RestartServerResult {
            servers: vec![
                ServerRestartEntry {
                    server_id: ServerId::from_static("a"),
                    outcome: RestartOutcome::Failed {
                        reason: RestartFailure::SpawnFailed {
                            message: format!("spawn {secret}"),
                        },
                    },
                },
                ServerRestartEntry {
                    server_id: ServerId::from_static("b"),
                    outcome: RestartOutcome::Failed {
                        reason: RestartFailure::InitializeFailed {
                            message: format!("init {secret}"),
                        },
                    },
                },
                ServerRestartEntry {
                    server_id: ServerId::from_static("c"),
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
