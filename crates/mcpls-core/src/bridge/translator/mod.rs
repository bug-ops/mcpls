//! MCP to LSP translation layer.
//!
//! `Translator` owns the LSP client/server registries and dispatches MCP
//! tool calls to per-domain handler modules. This module defines the
//! `Translator` struct itself plus setup/lifecycle methods (construction,
//! registration, shutdown); actual tool-call handling lives in the sibling
//! modules below, grouped by domain.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use tokio::sync::Mutex;

use self::clock::{Clock, SystemClock};
use self::encoding_ctx::EncodingCtx;
use self::servers::{Backend, Phase, Servers};
use crate::bridge::encoding::PositionEncoding;
use crate::bridge::indexing::IndexingReset;
use crate::bridge::state::ResourceLimits;
use crate::bridge::{DocumentTracker, NotificationCache, WorkspaceRoots};
use crate::config::{
    FileExtension, FilePattern, IndexingReadyTimeoutSecs, LanguageId, ServerId, ServerSettlement,
    ToolKind, ToolRouter,
};
use crate::error::{ServerSpawnFailure, StartupFailure};
use crate::lsp::{LspServer, ServerInitConfig};
use crate::redaction::Redactions;
use crate::util::lock_std;

mod addressing;
mod assist;
mod availability;
mod call_hierarchy;
#[cfg(test)]
mod characterization;
mod clock;
mod diagnostics;
mod dto;
mod edits;
mod enclosing;
#[cfg(test)]
mod enclosing_tests;
mod encoding_ctx;
mod folding_range;
mod hierarchy;
mod highlights;
mod kind_filter;
mod navigation;
#[cfg(test)]
mod prepare_range_tests;
mod pull_support;
mod respawn;
mod restart;
mod routing;
mod selection_range;
mod servers;
mod support;
mod symbols;
#[cfg(test)]
mod testing;
mod type_hierarchy;

pub use addressing::{
    AddressableTool, Addressed, MAX_SYMBOL_NAME_BYTES, PositionSource, ResolvedSymbol,
    ResolvedTarget, SymbolName, SymbolNameError, SymbolQuery, SymbolTarget,
};
pub use availability::{DiagnosticsAnswer, DiagnosticsAvailability, DiagnosticsOrigin};
pub use dto::*;
pub use enclosing::{
    Contextual, ContextualDiagnostic, ContextualLocation, EnclosingSymbol, EnclosingSymbolOutcome,
    EnrichmentSummary, NotComputedReason, ResultContext, SymbolFidelity, UnavailableReason,
};
pub use kind_filter::{
    CodeActionKindFilter, KindFilter, KindFilterInput, RejectedKindFilter, SymbolKindFilter,
};
pub use restart::{
    DiagnosticsRole, MAX_RESTART_SERVER_IDS, MAX_SERVER_ID_BYTES, NotificationReceivers,
    NotificationWiring, RestartFailure, RestartOutcome, RestartServerResult, RestartTarget,
    ServerIds, ServerIdsError, ServerRestartEntry,
};
pub use routing::Capability;
pub use support::{RouteSupport, ToolSupportSnapshot};

/// Translator handles MCP tool calls by converting them to LSP requests.
///
/// All fields use interior mutability so `Translator` can be shared via a
/// plain `Arc<Translator>` with no outer lock: every LSP tool call would
/// otherwise serialize behind a single mutex for its entire round trip
/// (including the LSP request timeout), which is the root cause fixed here.
/// Each field is locked independently and only for the short, synchronous
/// section that touches it. In particular, the actual LSP request/response
/// round trip (`client.request(...)`) always runs with no lock held.
///
/// `document_tracker` is no exception: `DocumentTracker` locks its own state
/// per-path internally (see its docs), so `prepare_document`'s call into
/// `ensure_open` never holds a lock shared across unrelated paths or
/// languages while it does that document's disk I/O and
/// `textDocument/didOpen`/`didChange` notify.
#[derive(Debug)]
pub struct Translator {
    /// Per-server state, one slot per routing identity. Locked only for a
    /// short synchronous section, never across an LSP request, and never
    /// together with `router`.
    servers: StdMutex<Servers>,
    /// The translator-wide lifecycle.
    phase: StdMutex<Phase>,
    /// Secrets of every configured server, known from startup, so error
    /// text is redacted even while no server is live.
    startup_redactions: Arc<Redactions>,
    /// Union of the startup set and the live clients' sets, rebuilt on change.
    merged_redactions: StdMutex<Option<MergedRedactions>>,
    /// Document state tracker. Locks its own state internally, per path.
    document_tracker: Arc<DocumentTracker>,
    /// Resource limits `document_tracker` was last built with. Kept
    /// alongside `document_tracker` so [`Self::with_extensions`] and
    /// [`Self::with_resource_limits`] can each rebuild the tracker from
    /// whichever of (limits, extension map) the other has already set,
    /// regardless of call order -- see [`Self::with_resource_limits`].
    resource_limits: ResourceLimits,
    /// Allowed workspace roots for path validation. Read-only after `serve()`
    /// setup, so no lock is needed.
    workspace_roots: WorkspaceRoots,
    /// Custom file extension to language ID mappings. Read-only after
    /// `serve()` setup, so no lock is needed.
    extension_map: Arc<HashMap<FileExtension, LanguageId>>,
    /// The `file_patterns` configured across all servers, reported when a
    /// file's language has no server. Read-only after `serve()` setup.
    file_patterns: Arc<[FilePattern]>,
    /// Per-tool routing table: resolves `(language, tool)` to a `ServerId`.
    /// Locked independently so a rebind (called from the background init task
    /// as each server settles) never contends with an in-flight LSP round
    /// trip.
    router: Arc<StdMutex<Arc<ToolRouter>>>,
    /// The routing table as installed by [`Self::with_router`], before any
    /// route to a server that failed to start is dropped. Read-only
    /// afterwards: the active `router` is derived from it on every
    /// settlement, and it lets a failed lookup be traced back to the server
    /// that would have served it, to report that server's [`StartupFailure`].
    configured_router: Arc<ToolRouter>,
    /// Diagnostics cache, shared with `serve_with`'s notification pump.
    ///
    /// `None` for a `Translator` built without [`Self::with_notification_cache`]
    /// (e.g. most unit tests). When present, [`Self::respawn_if_dead`] uses
    /// it to invalidate a respawned server's stale cached diagnostics --
    /// see that method's docs for why that matters.
    notification_cache: Option<Arc<Mutex<NotificationCache>>>,
    /// How to re-start a diagnostics pump for a restarted server. Installed
    /// once initialization has registered the initial pumps; until then a
    /// manual restart reports the server as still initializing.
    wiring: OnceLock<Arc<dyn NotificationWiring>>,
    /// Time source for respawn-backoff bookkeeping ([`respawn`](self::respawn)).
    /// Always [`SystemClock`] in production; overridden via
    /// [`Self::with_clock`] in tests so backoff-window tests can advance
    /// time deterministically instead of sleeping in real time.
    clock: Arc<dyn Clock>,
    /// Bound `Self::wait_for_indexing_ready` waits for a routed server to
    /// report indexing readiness. Defaults to `navigation::INDEXING_READY_TIMEOUT`;
    /// overridable via [`Self::with_indexing_ready_timeout`], wired from
    /// `workspace.indexing_ready_timeout_seconds` in `mcpls.toml` (#424).
    indexing_ready_timeout: std::time::Duration,
}

impl Translator {
    /// Create a new translator.
    ///
    /// Starts with an empty router: nothing is routable until [`Self::with_router`]
    /// installs one, which matches having no servers registered.
    ///
    /// Also starts with no workspace roots, which makes every path-taking
    /// operation fail closed with `Error::NoWorkspaceRoots` -- embedders
    /// MUST call [`Self::set_workspace_roots`] before serving any
    /// path-taking request.
    #[must_use]
    pub fn new() -> Self {
        Self {
            servers: StdMutex::new(Servers::default()),
            phase: StdMutex::new(Phase::default()),
            startup_redactions: Arc::default(),
            merged_redactions: StdMutex::new(None),
            document_tracker: Arc::new(DocumentTracker::new(
                ResourceLimits::default(),
                HashMap::new(),
            )),
            resource_limits: ResourceLimits::default(),
            workspace_roots: WorkspaceRoots::default(),
            extension_map: Arc::new(HashMap::new()),
            file_patterns: Arc::default(),
            router: Arc::new(StdMutex::new(Arc::new(ToolRouter::default()))),
            configured_router: Arc::new(ToolRouter::default()),
            notification_cache: None,
            wiring: OnceLock::new(),
            clock: Arc::new(SystemClock),
            indexing_ready_timeout: navigation::INDEXING_READY_TIMEOUT,
        }
    }

    /// Override the time source used by respawn-backoff bookkeeping.
    ///
    /// Test-only: production always uses [`SystemClock`]. Lets
    /// backoff-window tests advance a `FakeClock` deterministically instead
    /// of sleeping in real time.
    #[cfg(test)]
    #[must_use]
    fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Set the workspace roots for path validation.
    ///
    /// Only called during single-owner setup, before the translator is
    /// shared, so this replaces the roots wholesale rather than locking.
    ///
    /// Mandatory for any embedder that will serve path-taking requests:
    /// leaving `roots` empty (or never calling this) makes every such
    /// operation reject with `Error::NoWorkspaceRoots` instead of allowing
    /// unrestricted access.
    pub fn set_workspace_roots(&mut self, roots: WorkspaceRoots) {
        self.workspace_roots = roots;
    }

    /// Give the translator a handle to the shared diagnostics cache, so the
    /// respawn path can invalidate a respawned server's stale entries.
    ///
    /// Only called during single-owner setup (mirrors [`Self::with_router`]),
    /// before the translator is shared -- `serve_with` passes the same
    /// `Arc<Mutex<NotificationCache>>` used by the notification pump tasks.
    #[must_use]
    pub fn with_notification_cache(mut self, cache: Arc<Mutex<NotificationCache>>) -> Self {
        self.notification_cache = Some(cache);
        self
    }

    /// Give the translator the secrets of every configured server.
    ///
    /// Only called during single-owner setup, before the translator is shared.
    #[must_use]
    pub(crate) fn with_startup_redactions(mut self, redactions: Arc<Redactions>) -> Self {
        self.startup_redactions = redactions;
        self
    }

    /// Override the bound `Self::wait_for_indexing_ready` waits for a routed
    /// server to report indexing readiness, in place of the built-in
    /// `navigation::INDEXING_READY_TIMEOUT` default.
    ///
    /// Only called during single-owner setup (mirrors [`Self::with_notification_cache`]),
    /// before the translator is shared. `serve()` wires this from
    /// `workspace.indexing_ready_timeout_seconds`, whose type already bounds it.
    #[must_use]
    pub const fn with_indexing_ready_timeout(mut self, timeout: IndexingReadyTimeoutSecs) -> Self {
        self.indexing_ready_timeout = timeout.as_duration();
        self
    }

    /// Mark the set of servers that are expected (configured + applicable)
    /// but may still be initializing in the background.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "public API: callers hand over the set they built"
    )]
    pub fn set_expected_servers(&self, servers: HashSet<ServerId>) {
        lock_std(&self.servers).set_expected(&servers);
    }

    /// Clear the expected-servers set (e.g. after background init failed).
    pub fn clear_expected_servers(&self) {
        lock_std(&self.servers).clear_expected();
    }

    /// Install the per-tool routing table built from the applicable configs.
    ///
    /// Only called during single-owner setup, before the translator is
    /// shared, so this replaces the `Arc`-wrapped router wholesale.
    #[must_use]
    pub fn with_router(mut self, router: ToolRouter) -> Self {
        let router = Arc::new(router);
        self.configured_router = Arc::clone(&router);
        self.router = Arc::new(StdMutex::new(router));
        self
    }

    /// Remember why each of `failures` never registered, so later tool calls
    /// can report it instead of a generic "no server configured".
    ///
    /// A server that is already running keeps running: its failure is stale.
    #[cfg(test)]
    pub(crate) fn record_startup_failures(&self, failures: &[ServerSpawnFailure]) {
        let mut servers = lock_std(&self.servers);
        for failure in failures {
            servers.record_failure(failure);
        }
    }

    /// The recorded startup failure of the server `id`, if it failed to start.
    pub(crate) fn startup_failure(&self, id: &ServerId) -> Option<ServerSpawnFailure> {
        lock_std(&self.servers).failure(id).cloned()
    }

    /// Every recorded startup failure, ordered by routing identity.
    pub(crate) fn startup_failures(&self) -> Vec<ServerSpawnFailure> {
        lock_std(&self.servers).failures()
    }

    /// Records the servers untrusted-workspace mode refused to start and
    /// rebinds the router away from them.
    ///
    /// Does nothing for an empty list, so a trusted workspace is untouched.
    /// Not [`Self::settle_failed`]: a refused server was never expected, so
    /// its "settled twice or was never expected" error would be wrong.
    pub(crate) fn record_refusals(&self, refused: &[ServerSpawnFailure]) {
        if refused.is_empty() {
            return;
        }
        {
            let mut servers = lock_std(&self.servers);
            for failure in refused {
                servers.record_failure(failure);
            }
        }
        self.rebind_router_to_settled();
        for failure in refused {
            self.warn_failed_routes(&failure.server_id, &RouteLoss::of(failure));
        }
    }

    /// Settle the translator after the background init task panicked.
    ///
    /// Every config that never registered is recorded as
    /// [`StartupFailure::InitTaskPanicked`] (unless it already has a recorded
    /// failure), the router is rebound to what did register, and the
    /// expected-server set is cleared, so tools return a terminal error
    /// instead of `ServerInitializing` forever. The notification receivers and
    /// pumps of registered servers died with the task, so those servers are
    /// marked push-degraded and their indexing state is reset.
    pub(crate) async fn settle_after_init_panic(&self, configs: &[ServerInitConfig]) {
        lock_std(&self.phase).init_panicked();
        let registered: HashSet<ServerId> = {
            let mut servers = lock_std(&self.servers);
            servers.fail_unsettled(configs.iter().map(|config| {
                let server_config = &config.server_config();
                ServerSpawnFailure {
                    server_id: server_config.id(),
                    language_id: server_config.language_id.clone(),
                    command: server_config.command.to_string(),
                    reason: StartupFailure::InitTaskPanicked,
                }
            }));
            servers
                .ids()
                .filter(|id| servers.client(id).is_some())
                .cloned()
                .collect()
        };
        self.rebind_router_to_settled();
        self.clear_expected_servers();

        if let Some(cache) = &self.notification_cache {
            let mut cache = cache.lock().await;
            for id in &registered {
                cache.mark_push_degraded(id);
                cache.reset_indexing_state(id, IndexingReset::Forget);
            }
        }
    }

    /// Rebind the routing table to the set of servers that actually
    /// registered, dropping or redirecting routes to every other server as if
    /// it had failed to spawn. See [`ToolRouter::rebind`] for the semantics.
    pub fn rebind_router(&self, registered: &HashSet<ServerId>) {
        self.install_router(|id| {
            if registered.contains(id) {
                ServerSettlement::Registered
            } else {
                ServerSettlement::Failed
            }
        });
    }

    /// Re-derive the routing table from the configured one and what has
    /// settled so far: servers with a registered client are `Registered`,
    /// servers with a recorded startup failure are `Failed`, all others are
    /// still `Pending` and keep their routes. A pure function of that state,
    /// so it does not matter in which order servers settled.
    pub(crate) fn rebind_router_to_settled(&self) {
        let settlements = lock_std(&self.servers).settlements();
        self.install_router(|id| {
            settlements
                .get(id)
                .copied()
                .unwrap_or(ServerSettlement::Pending)
        });
    }

    fn install_router(&self, settlement: impl Fn(&ServerId) -> ServerSettlement) {
        let mut router = (*self.configured_router).clone();
        router.rebind(settlement);
        *lock_std(&self.router) = Arc::new(router);
    }

    /// The routing table as of now. The `router` lock is released before it
    /// returns, so callers can then lock `servers` without holding both.
    pub(super) fn router_snapshot(&self) -> Arc<ToolRouter> {
        Arc::clone(&lock_std(&self.router))
    }

    /// Registers the server in one step, then re-derives routes.
    pub(crate) fn settle_started(&self, server: LspServer) -> (ServerId, LanguageId) {
        let id = server.init_config().server_config().id();
        let language = server.client().language_id().clone();
        let registered = lock_std(&self.servers).register(id.clone(), Backend::Process(server));
        drop(registered.displaced);
        if !registered.was_expected {
            tracing::error!("LSP server '{id}' settled twice or was never expected");
        }
        self.rebind_router_to_settled();
        (id, language)
    }

    /// As [`Self::settle_started`], with the failure recorded.
    pub(crate) fn settle_failed(&self, failure: &ServerSpawnFailure) {
        let id = failure.server_id.clone();
        // The router moves off the failed server before it stops being
        // expected, so no lookup reports a stale `ServerFailedToStart`.
        lock_std(&self.servers).announce_failure(failure);
        self.rebind_router_to_settled();
        let was_expected = lock_std(&self.servers).record_failure(failure);
        if !was_expected {
            tracing::error!("LSP server '{id}' settled twice or was never expected");
        }
        self.warn_failed_routes(&id, &RouteLoss::FailedToStart);
    }

    /// Logs which languages lost a route to the failed server `id` and what
    /// the state of each language's catch-all is.
    fn warn_failed_routes(&self, id: &ServerId, loss: &RouteLoss<'_>) {
        let languages = self.configured_router.languages_routed_to(id);
        let catch_all_states: Vec<String> = languages
            .iter()
            .map(|language| {
                let state = match self.configured_router.catch_all_for(language.as_str()) {
                    None => "no catch-all".to_string(),
                    Some(catch_all) if catch_all == id => "it was the catch-all".to_string(),
                    Some(catch_all) if lock_std(&self.servers).client(catch_all).is_some() => {
                        format!("catch-all '{catch_all}' registered")
                    }
                    Some(catch_all) if self.startup_failure(catch_all).is_some() => {
                        format!("catch-all '{catch_all}' failed")
                    }
                    Some(catch_all) => format!("catch-all '{catch_all}' pending"),
                };
                format!("{language}: {state}")
            })
            .collect();
        tracing::warn!(
            "server '{id}' {loss}; routes of [{}] are rebound or dropped",
            catch_all_states.join(", ")
        );
    }

    /// Whether `id` is the server the router currently resolves
    /// `ToolKind::Diagnostics` to for `language_id`.
    ///
    /// Purpose-built for the startup settlement, which needs this to compute
    /// each pump's diagnostics role and the diagnostics-route count, without
    /// exposing the router's lock guard outside this module.
    #[must_use]
    pub fn is_diagnostics_route(&self, language_id: &LanguageId, id: &ServerId) -> bool {
        lock_std(&self.router).resolve(language_id.as_str(), ToolKind::Diagnostics) == Some(id)
    }

    /// Negotiated [`PositionEncoding`] of the registered server `id`, or the
    /// LSP spec's own default (UTF-16) if `id` is not currently registered.
    ///
    /// Note this falls back to UTF-16, not [`PositionEncoding::default`]
    /// (UTF-8): UTF-16 is what an absent/unrecognized negotiation means per
    /// the LSP spec and what [`crate::lsp::LspServer::spawn`] itself falls
    /// back to, so this must match rather than use the bridge type's own
    /// default, which exists only for `PositionEncoding`'s own internal use.
    #[must_use]
    pub(crate) fn position_encoding_for(&self, server_id: &ServerId) -> PositionEncoding {
        lock_std(&self.servers)
            .server(server_id)
            .and_then(|server| PositionEncoding::from_lsp(server.position_encoding().as_str()))
            .unwrap_or(PositionEncoding::Utf16)
    }

    /// Build the [`EncodingCtx`] for converting positions/ranges in
    /// responses from the registered server `id`.
    fn encoding_ctx(&self, server_id: &ServerId) -> EncodingCtx {
        EncodingCtx::new(
            self.position_encoding_for(server_id),
            self.document_tracker.clone(),
            self.workspace_roots.clone(),
        )
    }

    /// Rebuilds `document_tracker` from `self.resource_limits` and
    /// `self.extension_map`, whatever the two are currently set to.
    ///
    /// Called by every builder that touches either input ([`Self::with_extensions`],
    /// [`Self::with_resource_limits`]), so each one only needs to set its own
    /// field and call this -- it always reads *both* current values, so the
    /// builders remain order-independent (see [`Self::with_resource_limits`])
    /// without each one needing to know the other's field. A future builder
    /// that adds a third tracker input should follow the same pattern:
    /// update its own field, then call this.
    fn rebuild_document_tracker(&mut self) {
        self.document_tracker = Arc::new(DocumentTracker::new(
            self.resource_limits,
            (*self.extension_map).clone(),
        ));
    }

    /// Configure the `file_patterns` reported by no-server-for-language errors.
    ///
    /// Only called during single-owner setup, before the translator is shared.
    #[must_use]
    pub fn with_file_patterns(mut self, patterns: impl IntoIterator<Item = FilePattern>) -> Self {
        self.file_patterns = patterns.into_iter().collect();
        self
    }

    /// Configure custom file extension mappings.
    ///
    /// This method sets the extension map and updates the document tracker
    /// to use the same mappings for language detection.
    ///
    /// Only called during single-owner setup, before the translator is
    /// shared, so this replaces the `Arc`-wrapped fields wholesale.
    #[must_use]
    pub fn with_extensions(mut self, extension_map: HashMap<FileExtension, LanguageId>) -> Self {
        self.extension_map = Arc::new(extension_map);
        self.rebuild_document_tracker();
        self
    }

    /// Configure resource limits (max open documents, max file size) for the
    /// document tracker.
    ///
    /// Only called during single-owner setup, before the translator is
    /// shared. This builder and [`Self::with_extensions`] may be called in
    /// either order -- each rebuilds `document_tracker` from *both* of
    /// `self.resource_limits`/`self.extension_map`'s current values,
    /// instead of one of them starting fresh from
    /// `ResourceLimits::default()`/an empty extension map, which previously
    /// meant whichever builder ran last silently discarded the other's
    /// effect.
    #[must_use]
    pub fn with_resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.resource_limits = limits;
        self.rebuild_document_tracker();
        self
    }

    /// Test-only: register a bare LSP client under its routing identity, next
    /// to the server already registered for it, if any.
    #[cfg(test)]
    pub(crate) fn register_client(&self, id: ServerId, client: crate::lsp::LspClient) {
        lock_std(&self.servers).register_test_client(id, client);
    }

    /// The secrets of every live server's client, for hiding them in tool
    /// results. The merged set is rebuilt only when the set of client
    /// redaction sets changes (registration, restart, respawn), outside the
    /// `servers` lock.
    pub(crate) fn server_text_redactions(&self) -> Arc<Redactions> {
        let sources: Vec<Arc<Redactions>> = std::iter::once(Arc::clone(&self.startup_redactions))
            .chain(
                lock_std(&self.servers)
                    .clients()
                    .map(|client| Arc::clone(client.redactions())),
            )
            .filter(|set| !set.is_empty())
            .collect();
        let mut cache = lock_std(&self.merged_redactions);
        if let Some(cached) = cache
            .as_ref()
            .filter(|cached| cached.is_built_from(&sources))
        {
            return Arc::clone(&cached.merged);
        }
        let merged = Arc::new(Redactions::union(sources.iter().map(Arc::as_ref)));
        *cache = Some(MergedRedactions {
            sources,
            merged: Arc::clone(&merged),
        });
        merged
    }

    /// Test-only: register a bare LSP server under its routing identity, next
    /// to the client already registered for it, if any.
    #[cfg(test)]
    pub(crate) fn register_server(&self, id: ServerId, server: LspServer) {
        lock_std(&self.servers).register_test_server(id, server);
    }

    /// Register a spawned server under its routing identity, in one step.
    ///
    /// The routing identity and client are both derived from `server`
    /// itself, so they cannot be registered out of sync. The server also owns
    /// the config a respawn would use.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use mcpls_core::bridge::Translator;
    /// use mcpls_core::lsp::LspServer;
    ///
    /// fn register(translator: &Translator, server: LspServer) {
    ///     translator.register_server_complete(server);
    /// }
    /// ```
    pub fn register_server_complete(&self, server: LspServer) {
        let id = server.init_config().server_config().id();
        let registered = lock_std(&self.servers).register(id, Backend::Process(server));
        drop(registered.displaced);
    }

    /// Number of currently registered LSP servers.
    ///
    /// Test-only: the server slots are private, so this is the one way a test
    /// outside this module (e.g. `crate::tests`, exercising
    /// [`Translator::shutdown_servers`] indirectly through `serve_with`'s
    /// shutdown sequence) can observe that a registered server was actually
    /// drained.
    #[cfg(test)]
    pub(crate) fn registered_server_count(&self) -> usize {
        lock_std(&self.servers).running_servers().count()
    }

    /// Snapshot of currently open document paths, used for MCP resource listing.
    #[must_use]
    pub fn open_document_paths(&self) -> Vec<PathBuf> {
        self.document_tracker.open_paths()
    }

    /// Whether a document is currently tracked as open.
    #[must_use]
    pub fn is_document_open(&self, path: &Path) -> bool {
        self.document_tracker.is_open(path)
    }

    /// The document tracker, shared with [`EncodingCtx`] so a cache-only
    /// caller (e.g. `get_cached_diagnostics`) can still prefer tracked
    /// in-memory content over a disk read when converting positions.
    #[must_use]
    pub(crate) const fn document_tracker(&self) -> &Arc<DocumentTracker> {
        &self.document_tracker
    }

    /// Gracefully shut down every registered LSP server.
    ///
    /// Drains the registered LSP servers and, for each one concurrently,
    /// sends the LSP `shutdown` request and `exit` notification via
    /// [`LspServer::shutdown`], whose handshake is bounded by
    /// [`crate::lsp::SHUTDOWN_TIMEOUT`] and which then sweeps the server's
    /// whole process tree, for up to [`crate::lsp::LIFELINE_SWEEP_BUDGET`]
    /// more. A server that errors or fails to respond in time has its tree
    /// killed instead. No outer timeout is applied, so when this returns every
    /// tree is gone. Call this once, from the
    /// top-level shutdown path, after the MCP transport has stopped
    /// accepting new requests.
    ///
    /// # Limitations
    ///
    /// This only runs on the normal shutdown path (stdio EOF, `SIGTERM`/
    /// `SIGINT`, or the HTTP transport's own graceful shutdown). A panic is
    /// covered separately: release builds unwind, and the binary's `main`
    /// shuts the runtime down so every task, and with it every LSP child
    /// (`kill_on_drop`), is dropped. When no Rust code runs (`SIGKILL`, the
    /// OOM killer, the forced `exit(1)` on a second signal during shutdown,
    /// or a panic inside a `Drop` during unwinding) the children are still
    /// killed by the lifeline watchdog (Unix) or Job Object (Windows), see
    /// `specs/lsp/007-lsp-child-process-lifetime`, which also sweeps
    /// descendants that left the process group (`setsid`/`setpgid`).
    ///
    /// `pub(crate)` rather than `pub`: this is meant for exactly one call
    /// site (`serve_with`'s post-transport shutdown sequence), after the MCP
    /// transport is already down. An external caller invoking it mid-session
    /// would stop the servers while their slots (routing table) still
    /// points at the now-shut-down servers, so in-flight tool calls would
    /// resolve to a client whose server is gone.
    pub(crate) async fn shutdown_servers(&self) {
        self.begin_shutdown();
        let servers: Vec<(ServerId, LspServer)> = lock_std(&self.servers).drain_servers();
        if servers.is_empty() {
            return;
        }

        let mut tasks = tokio::task::JoinSet::new();
        let mut ids = HashMap::new();
        for (id, server) in servers {
            let task_id = id.clone();
            let handle = tasks.spawn(async move {
                match server.shutdown().await {
                    Ok(()) => tracing::debug!(%id, "LSP server shut down gracefully"),
                    Err(e) => tracing::warn!(
                        %id, error = %e,
                        "LSP server shutdown failed, killing process instead"
                    ),
                }
            });
            ids.insert(handle.id(), task_id);
        }
        join_shutdown_tasks(tasks, &ids).await;
    }
}

/// Awaits every shutdown task, logging a panicked or cancelled one with its
/// server id instead of re-raising it into the caller.
async fn join_shutdown_tasks(
    mut tasks: tokio::task::JoinSet<()>,
    ids: &HashMap<tokio::task::Id, ServerId>,
) {
    while let Some(joined) = tasks.join_next_with_id().await {
        if let Err(e) = joined {
            let id = ids.get(&e.id()).map_or("unknown", ServerId::as_str);
            tracing::error!(%id, error = %e, "LSP server shutdown task failed");
        }
    }
}

impl Default for Translator {
    /// Same as [`Translator::new`]: no workspace roots configured, so every
    /// path-taking operation fails closed with `Error::NoWorkspaceRoots`
    /// until [`Translator::set_workspace_roots`] is called.
    fn default() -> Self {
        Self::new()
    }
}

/// A merged redaction set and the per-client sets it was built from.
#[derive(Debug)]
struct MergedRedactions {
    sources: Vec<Arc<Redactions>>,
    merged: Arc<Redactions>,
}

impl MergedRedactions {
    /// Whether `sources` are the same sets (by identity, in any order).
    fn is_built_from(&self, sources: &[Arc<Redactions>]) -> bool {
        self.sources.len() == sources.len()
            && sources
                .iter()
                .all(|set| self.sources.iter().any(|known| Arc::ptr_eq(known, set)))
    }
}

/// Why a server's routes were lost, as the startup log line words it.
enum RouteLoss<'a> {
    FailedToStart,
    Refused(&'a crate::error::UntrustedRefusal),
}

impl<'a> RouteLoss<'a> {
    const fn of(failure: &'a ServerSpawnFailure) -> Self {
        match &failure.reason {
            StartupFailure::RefusedUntrustedWorkspace(refusal) => Self::Refused(refusal),
            StartupFailure::Spawn(_) | StartupFailure::InitTaskPanicked => Self::FailedToStart,
        }
    }
}

impl std::fmt::Display for RouteLoss<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FailedToStart => f.write_str("failed to start"),
            Self::Refused(refusal) => write!(f, "refused (untrusted workspace: {refusal})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    use tempfile::TempDir;
    use tokio::time::Duration;

    use super::*;
    use crate::bridge::state::detect_language;
    use crate::config::{DocumentLimit, ServerId, SizeLimit, ToolKind, ToolRouter, ToolSet};
    use crate::error::Error;
    use crate::test_lsp::fake_lsp_client;

    #[tokio::test]
    async fn test_server_text_redactions_follow_live_clients_across_respawn() {
        let translator = Translator::new();
        assert!(translator.server_text_redactions().is_empty());

        let (a, _fake_a, _lanes_a) = crate::test_lsp::fake_lsp_client_with_redactions(
            Redactions::new([("A_TOKEN".to_owned(), "alpha-secret-111".to_owned())]),
        );
        let (b, _fake_b, _lanes_b) = crate::test_lsp::fake_lsp_client_with_redactions(
            Redactions::new([("B_TOKEN".to_owned(), "bravo-secret-222".to_owned())]),
        );
        translator.register_client(ServerId::from_static("a"), a);
        translator.register_client(ServerId::from_static("b"), b);
        let both = translator.server_text_redactions();
        assert_eq!(both.apply("alpha-secret-111"), "[redacted:A_TOKEN]");
        assert_eq!(both.apply("bravo-secret-222"), "[redacted:B_TOKEN]");

        let (respawned, _fake_c, _lanes_c) = crate::test_lsp::fake_lsp_client_with_redactions(
            Redactions::new([("C_TOKEN".to_owned(), "charlie-secret-333".to_owned())]),
        );
        translator.register_client(ServerId::from_static("a"), respawned);
        let after = translator.server_text_redactions();
        assert_eq!(after.apply("alpha-secret-111"), "alpha-secret-111");
        assert_eq!(after.apply("charlie-secret-333"), "[redacted:C_TOKEN]");
        assert_eq!(after.apply("bravo-secret-222"), "[redacted:B_TOKEN]");
    }

    #[tokio::test]
    async fn test_server_text_redactions_are_cached_and_do_not_relog() {
        use tracing_subscriber::prelude::*;

        let translator = Translator::new();
        let (a, _fake_a, _lanes_a) = crate::test_lsp::fake_lsp_client_with_redactions(
            Redactions::new([("A_TOKEN".to_owned(), "alpha-secret-111".to_owned())]),
        );
        let (b, _fake_b, _lanes_b) = crate::test_lsp::fake_lsp_client_with_redactions(
            Redactions::new([("B_TOKEN".to_owned(), "A_TOKEN]".to_owned())]),
        );
        translator.register_client(ServerId::from_static("a"), a);
        translator.register_client(ServerId::from_static("b"), b);

        let logs = crate::test_lsp::CapturedLogs::default();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::WARN)
            .with(logs.clone());
        let first = tracing::subscriber::with_default(subscriber, || {
            let first = translator.server_text_redactions();
            let second = translator.server_text_redactions();
            let third = translator.server_text_redactions();
            assert!(Arc::ptr_eq(&first, &second) && Arc::ptr_eq(&second, &third));
            first
        });

        assert_eq!(logs.messages().len(), 1, "{:?}", logs.messages());
        assert_eq!(first.apply("alpha-secret-111"), "[redacted:A_TOKEN]");
    }

    #[tokio::test]
    async fn test_register_server_complete_fills_all_maps_under_init_config_id() {
        let translator = Translator::new();
        let config = crate::config::LspServerConfig::rust_analyzer();
        let id = config.id();
        translator.register_server_complete(crate::lsp::fake_lsp_server_with_config(config));

        let servers = lock_std(&translator.servers);
        assert!(servers.client(&id).is_some());
        assert!(servers.server(&id).is_some());
        drop(servers);
    }

    fn named_config(
        name: &str,
        language: &str,
        handles: Option<Vec<ToolKind>>,
    ) -> crate::config::LspServerConfig {
        let mut config = crate::config::LspServerConfig::rust_analyzer();
        config.name = Some(ServerId::new(name).unwrap());
        config.language_id = LanguageId::new(language).unwrap();
        config.handles = handles.map(|tools| ToolSet::new(tools).unwrap());
        config
    }

    fn spawn_failure(config: &crate::config::LspServerConfig) -> ServerSpawnFailure {
        ServerSpawnFailure {
            server_id: config.id(),
            language_id: config.language_id.clone(),
            command: config.command.to_string(),
            reason: StartupFailure::InitTaskPanicked,
        }
    }

    fn settle_all(
        translator: &Translator,
        configs: &[crate::config::LspServerConfig],
        up: &[bool],
    ) {
        for (config, up) in configs.iter().zip(up) {
            if *up {
                translator.settle_started(crate::lsp::fake_lsp_server_with_config(config.clone()));
            } else {
                translator.settle_failed(&spawn_failure(config));
            }
        }
    }

    /// FR-006 / NFR-005: whatever order servers settle in, the final routes,
    /// failure set and expected set equal the batch result.
    #[tokio::test]
    async fn settle_in_any_completion_order_yields_the_batch_router_and_failures() {
        let configs = [
            named_config("x", "rust", Some(vec![ToolKind::Hover])),
            named_config("c", "rust", None),
            named_config("y", "python", None),
        ];
        let orders = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        for outcome in 0..8u8 {
            let up: Vec<bool> = (0..3).map(|bit| outcome & (1 << bit) != 0).collect();
            let registered: HashSet<ServerId> = configs
                .iter()
                .zip(&up)
                .filter(|(_, up)| **up)
                .map(|(config, _)| config.id())
                .collect();
            let mut batch = ToolRouter::from_configs(&configs).unwrap();
            batch.rebind_to_registered(&registered);

            for order in orders {
                let translator =
                    Translator::new().with_router(ToolRouter::from_configs(&configs).unwrap());
                translator.set_expected_servers(
                    configs
                        .iter()
                        .map(crate::config::LspServerConfig::id)
                        .collect(),
                );
                for index in order {
                    settle_all(
                        &translator,
                        std::slice::from_ref(&configs[index]),
                        &[up[index]],
                    );
                }

                let router = lock_std(&translator.router).clone();
                for language in ["rust", "python"] {
                    for tool in ToolKind::ALL.iter().copied() {
                        assert_eq!(
                            router.resolve(language, tool),
                            batch.resolve(language, tool),
                            "{language}/{tool:?}, up {up:?}, order {order:?}"
                        );
                    }
                }
                for tool in ToolKind::ALL.iter().copied() {
                    assert_eq!(router.resolve_any(tool), batch.resolve_any(tool));
                }
                let failed: HashSet<ServerId> = translator
                    .startup_failures()
                    .into_iter()
                    .map(|failure| failure.server_id)
                    .collect();
                let expected_failed: HashSet<ServerId> = configs
                    .iter()
                    .map(crate::config::LspServerConfig::id)
                    .filter(|id| !registered.contains(id))
                    .collect();
                assert_eq!(failed, expected_failed);
                assert!(!lock_std(&translator.servers).any_expected());
            }
        }
    }

    /// FR-007 window: the explicit server failed, its catch-all is still
    /// initializing. Nothing is bound to the catch-all, but the lookup reports
    /// the retryable `ServerInitializing` naming it (not a terminal failure
    /// and not a dangling-route error); once the catch-all registers the route
    /// is served by it.
    #[tokio::test]
    async fn dead_explicit_route_is_retryable_while_the_catch_all_initializes() {
        use tracing_subscriber::prelude::*;

        use crate::test_lsp::CapturedLogs;

        let x = named_config(
            "x",
            "rust",
            Some(vec![ToolKind::Hover, ToolKind::Diagnostics]),
        );
        let c = named_config("c", "rust", None);
        let translator = Translator::new()
            .with_extensions(crate::test_lsp::test_extensions())
            .with_router(ToolRouter::from_configs([&x, &c]).unwrap());
        translator.set_expected_servers([x.id(), c.id()].into_iter().collect());
        let path = PathBuf::from("/ws/main.rs");
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(logs.clone());

        tracing::subscriber::with_default(subscriber, || {
            translator.settle_failed(&spawn_failure(&x));

            let hover = translator
                .client_for_file(&path, ToolKind::Hover)
                .unwrap_err();
            std::assert_matches!(
                hover,
                Error::ServerInitializing { server_id } if server_id == c.id()
            );
            std::assert_matches!(
                translator.diagnostics_route_for_path(&path),
                routing::DiagnosticsRoute::Initializing(id) if id == c.id()
            );
            assert_eq!(
                translator
                    .tool_support_snapshot()
                    .document_support(&LanguageId::from_static("rust"), ToolKind::Hover),
                RouteSupport::Initializing
            );
            assert!(
                logs.entries()
                    .iter()
                    .all(|(level, _)| *level != tracing::Level::ERROR),
                "{:?}",
                logs.messages()
            );
        });

        translator.settle_started(crate::lsp::fake_lsp_server_with_config(c.clone()));
        let (served_by, _) = translator.client_for_file(&path, ToolKind::Hover).unwrap();
        assert_eq!(served_by, c.id());
    }

    /// The window inside `settle_failed`: once the failure is noted but the
    /// router has not moved yet, a lookup still reads the server as
    /// initializing (retryable), never as a failed start, although a
    /// catch-all is registered and serves the call right after the rebind.
    #[tokio::test]
    async fn test_settle_failed_window_reads_initializing_not_failed() {
        let configs = [
            named_config("a", "rust", Some(vec![ToolKind::Hover])),
            named_config("b", "rust", None),
        ];
        let translator = Translator::new()
            .with_extensions(crate::test_lsp::test_extensions())
            .with_router(ToolRouter::from_configs(&configs).unwrap());
        translator.set_expected_servers(
            configs
                .iter()
                .map(crate::config::LspServerConfig::id)
                .collect(),
        );
        translator.settle_started(crate::lsp::fake_lsp_server_with_config(configs[1].clone()));
        let path = PathBuf::from("/ws/main.rs");

        lock_std(&translator.servers).announce_failure(&spawn_failure(&configs[0]));

        let err = translator
            .client_for_file(&path, ToolKind::Hover)
            .unwrap_err();
        std::assert_matches!(&err, Error::ServerInitializing { server_id } if *server_id == configs[0].id());
        assert_eq!(
            translator
                .tool_support_snapshot()
                .document_support(&LanguageId::from_static("rust"), ToolKind::Hover),
            RouteSupport::Initializing
        );

        translator.settle_failed(&spawn_failure(&configs[0]));
        let (served_by, _) = translator.client_for_file(&path, ToolKind::Hover).unwrap();
        assert_eq!(served_by, configs[1].id());
    }

    /// The registry is read before the router, so a router already moved off
    /// a failing server never pairs with the registry from before the move.
    #[tokio::test]
    async fn test_snapshot_between_rebind_and_failure_record_still_routes_to_the_catch_all() {
        let configs = [
            named_config("a", "rust", Some(vec![ToolKind::Hover])),
            named_config("b", "rust", None),
        ];
        let translator = Translator::new()
            .with_extensions(crate::test_lsp::test_extensions())
            .with_router(ToolRouter::from_configs(&configs).unwrap());
        translator.set_expected_servers(
            configs
                .iter()
                .map(crate::config::LspServerConfig::id)
                .collect(),
        );
        translator.settle_started(crate::lsp::fake_lsp_server_with_config(configs[1].clone()));

        lock_std(&translator.servers).announce_failure(&spawn_failure(&configs[0]));
        translator.rebind_router_to_settled();

        assert_matches!(
            translator
                .tool_support_snapshot()
                .document_support(&LanguageId::from_static("rust"), ToolKind::Hover),
            RouteSupport::Supported { server } | RouteSupport::CapabilityNotAdvertised { server, .. }
                if server == configs[1].id()
        );
    }

    /// An init-task panic while a server is mid-restart leaves its slot alone:
    /// the restart's backend is restored to a running server, and the slot's
    /// bookkeeping survives.
    #[tokio::test]
    async fn test_init_panic_during_a_restart_does_not_fail_or_drop_the_restarting_server() {
        let config = crate::config::LspServerConfig::rust_analyzer();
        let id = config.id();
        let translator = Translator::new();
        translator
            .register_server_complete(crate::lsp::fake_lsp_server_with_config(config.clone()));
        let lock_before = translator.respawn_lock(&id);
        let held = lock_std(&translator.servers).take_for_restart(&id).unwrap();

        translator
            .settle_after_init_panic(&[crate::test_lsp::init_config_for(config)])
            .await;

        assert!(translator.startup_failure(&id).is_none());
        assert!(Arc::ptr_eq(&lock_before, &translator.respawn_lock(&id)));
        let refused = lock_std(&translator.servers).restore(&id, held);
        assert!(refused.is_none());
        assert_eq!(translator.registered_server_count(), 1);
    }

    /// A settlement that finds an earlier failure keeps it and records only
    /// the servers that never settled.
    #[tokio::test]
    async fn settle_after_init_panic_after_partial_settlement_records_only_unsettled() {
        let configs = [
            named_config("a", "rust", Some(vec![ToolKind::Hover])),
            named_config("b", "python", None),
            named_config("c", "typescript", None),
        ];
        let translator = Translator::new().with_router(ToolRouter::from_configs(&configs).unwrap());
        translator.set_expected_servers(
            configs
                .iter()
                .map(crate::config::LspServerConfig::id)
                .collect(),
        );
        translator.settle_started(crate::lsp::fake_lsp_server_with_config(configs[0].clone()));
        translator.settle_failed(&ServerSpawnFailure {
            reason: StartupFailure::Spawn(std::sync::Arc::new(Error::ServerNotFound {
                command: "b".to_string(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            })),
            ..spawn_failure(&configs[1])
        });
        let init_configs: Vec<_> = configs
            .iter()
            .cloned()
            .map(crate::test_lsp::init_config_for)
            .collect();

        translator.settle_after_init_panic(&init_configs).await;

        assert!(translator.startup_failure(&configs[0].id()).is_none());
        std::assert_matches!(
            translator.startup_failure(&configs[1].id()).unwrap().reason,
            StartupFailure::Spawn(_)
        );
        std::assert_matches!(
            translator.startup_failure(&configs[2].id()).unwrap().reason,
            StartupFailure::InitTaskPanicked
        );
    }

    /// #528: after an init-task panic, a configured server that never
    /// registered gets a terminal `ServerFailedToStart` instead of
    /// `ServerInitializing` forever.
    #[tokio::test]
    async fn test_settle_after_init_panic_turns_initializing_into_startup_failure() {
        let config = crate::config::LspServerConfig::rust_analyzer();
        let id = config.id();
        let translator = Translator::new()
            .with_extensions(crate::test_lsp::test_extensions())
            .with_router(ToolRouter::catch_all([(
                id.clone(),
                LanguageId::from_static("rust"),
            )]));
        translator.set_expected_servers(HashSet::from([id.clone()]));
        let path = PathBuf::from("/ws/main.rs");
        let before = translator
            .client_for_file(&path, ToolKind::Hover)
            .unwrap_err();
        assert_matches!(before, Error::ServerInitializing { .. }, "got {before:?}");

        translator
            .settle_after_init_panic(&[crate::test_lsp::init_config_for(config)])
            .await;

        let after = translator
            .client_for_file(&path, ToolKind::Hover)
            .unwrap_err();
        assert_matches!(
                &after,
                Error::ServerFailedToStart(f)
                    if f.server_id == id && matches!(f.reason, StartupFailure::InitTaskPanicked),
            "got {after:?}"
        );
    }

    /// A server that registered before the panic keeps serving.
    #[tokio::test]
    async fn test_settle_after_init_panic_keeps_registered_server_routable() {
        let config = crate::config::LspServerConfig::rust_analyzer();
        let id = config.id();
        let translator = Translator::new()
            .with_extensions(crate::test_lsp::test_extensions())
            .with_router(ToolRouter::catch_all([(
                id.clone(),
                LanguageId::from_static("rust"),
            )]));
        translator.set_expected_servers(HashSet::from([id.clone()]));
        translator
            .register_server_complete(crate::lsp::fake_lsp_server_with_config(config.clone()));

        translator
            .settle_after_init_panic(&[crate::test_lsp::init_config_for(config)])
            .await;

        let (routed, _client) = translator
            .client_for_file(Path::new("/ws/main.rs"), ToolKind::Hover)
            .unwrap();
        assert_eq!(routed, id);
        assert!(translator.startup_failure(&id).is_none());
    }

    #[test]
    fn test_record_startup_failures_orders_listing_by_server_id() {
        let translator = Translator::new();
        let failure = |id: &str| ServerSpawnFailure {
            server_id: ServerId::new(id).unwrap(),
            language_id: LanguageId::new(id).unwrap(),
            command: id.to_string(),
            reason: StartupFailure::InitTaskPanicked,
        };
        translator.record_startup_failures(&[failure("zls"), failure("clangd"), failure("gopls")]);

        let ids: Vec<_> = translator
            .startup_failures()
            .into_iter()
            .map(|f| f.server_id.as_str().to_string())
            .collect();
        assert_eq!(ids, ["clangd", "gopls", "zls"]);
    }

    /// A panicking shutdown task must be logged, not re-raised into the caller.
    #[tokio::test]
    async fn test_join_shutdown_tasks_survives_panicking_task() {
        let mut tasks = tokio::task::JoinSet::new();
        let handle = tasks.spawn(async { panic!("shutdown task boom") });
        let mut ids = HashMap::new();
        ids.insert(handle.id(), ServerId::from_static("rust"));
        tasks.spawn(async {});

        join_shutdown_tasks(tasks, &ids).await;
    }

    #[test]
    fn test_translator_new() {
        let translator = Translator::new();
        assert!(translator.workspace_roots.is_empty());
        assert_eq!(translator.registered_server_count(), 0);
    }

    #[test]
    fn test_set_workspace_roots() {
        let mut translator = Translator::new();
        let roots = vec![PathBuf::from("/test/root1"), PathBuf::from("/test/root2")];
        translator.set_workspace_roots(WorkspaceRoots::for_test(roots.clone(), Vec::new()));
        assert_eq!(translator.workspace_roots.canonical(), roots);
    }

    #[test]
    fn test_register_server() {
        let translator = Translator::new();

        // Initial state: no servers registered
        assert_eq!(translator.registered_server_count(), 0);

        // The register_server method exists and is callable
        // Full integration testing with real LspServer is done in integration tests
        // This unit test verifies the method signature and basic functionality

        // Note: We can't easily construct an LspServer in a unit test without async
        // and a real LSP server process. The actual registration functionality is
        // tested in integration tests (see rust_analyzer_tests.rs).
        // This test verifies the data structure is properly initialized.
    }

    /// #241: `shutdown_servers` on an empty registry must return immediately
    /// rather than blocking (e.g. on a `JoinSet` that's never populated).
    #[tokio::test]
    async fn test_shutdown_servers_empty_registry_returns_promptly() {
        let translator = Translator::new();

        let result =
            tokio::time::timeout(Duration::from_secs(1), translator.shutdown_servers()).await;

        assert!(
            result.is_ok(),
            "shutdown_servers must return promptly when no servers are registered"
        );
    }

    /// #241: `shutdown_servers` must drain every registered `LspServer` —
    /// this is the core behavior the issue is about (orphaned LSP children
    /// on shutdown). Uses `fake_lsp_server()` (an inert in-memory
    /// transport plus a real `LspServer`, see `lsp::lifecycle`), which
    /// won't answer the LSP `shutdown` handshake — proving the drain
    /// completes, via the error fallback path, without hanging on
    /// non-responsive servers.
    #[tokio::test]
    async fn test_shutdown_servers_drains_registered_servers() {
        let translator = Translator::new();
        translator.register_server(
            ServerId::from_static("server-a"),
            crate::lsp::fake_lsp_server(),
        );
        translator.register_server(
            ServerId::from_static("server-b"),
            crate::lsp::fake_lsp_server(),
        );
        assert_eq!(translator.registered_server_count(), 2);

        // Bounded well above `lsp::SHUTDOWN_TIMEOUT` (10s) so a genuine
        // regression (a hang) still fails the test instead of the harness
        // itself timing out ambiguously.
        let result =
            tokio::time::timeout(Duration::from_secs(20), translator.shutdown_servers()).await;

        assert!(
            result.is_ok(),
            "shutdown_servers must not hang against non-responsive mock servers"
        );
        assert_eq!(
            translator.registered_server_count(),
            0,
            "all registered servers must be drained"
        );
    }

    #[test]
    fn test_clear_expected_servers_reverts_to_no_server_after_all_routes_dropped() {
        // Mirrors the settled state of the real `serve_with` flow: the router
        // has dropped routes to servers that never registered and the
        // expected set is empty. Subsequent lookups must fall back to
        // NoServerForLanguage rather than keep implying the server is still
        // on its way.
        let path = PathBuf::from("/ws/Assets/Scripts/Player.cs");
        let lang = detect_language(&path, &HashMap::new());
        let id = ServerId::from(lang.clone());

        let translator = Translator::new().with_router(ToolRouter::catch_all([(
            id.clone(),
            LanguageId::new(lang).unwrap(),
        )]));
        let mut expected = HashSet::new();
        expected.insert(id);
        translator.set_expected_servers(expected);

        translator.rebind_router(&HashSet::new());
        translator.clear_expected_servers();

        let err = translator
            .client_for_file(&path, ToolKind::Hover)
            .unwrap_err();
        assert_matches!(err, Error::NoServerForLanguage { .. });
    }

    #[test]
    fn test_translator_with_custom_extensions() {
        let mut extension_map = HashMap::new();
        extension_map.insert(
            FileExtension::from_static("nu"),
            LanguageId::from_static("nushell"),
        );
        extension_map.insert(
            FileExtension::from_static("customext"),
            LanguageId::from_static("customlang"),
        );

        let translator = Translator::new().with_extensions(extension_map.clone());

        assert_eq!(translator.extension_map.len(), 2);
        assert_eq!(
            translator.extension_map.get("nu"),
            Some(&LanguageId::from_static("nushell"))
        );
        assert_eq!(
            translator.extension_map.get("customext"),
            Some(&LanguageId::from_static("customlang"))
        );
    }

    /// `with_resource_limits` called before `with_extensions` (the order
    /// `serve()` uses) must reach `document_tracker`. With `max_documents:
    /// 1` and neither document locked, the second `ensure_open` evicts the
    /// first (#495) rather than failing -- `document_tracker.len()` staying
    /// at 1 is what proves the limit actually reached the tracker. Goes
    /// through `ensure_open` (not the raw `open`) so the first document is
    /// disk-verified and therefore actually evictable (#495 S4).
    #[tokio::test]
    async fn test_with_resource_limits_applies_before_with_extensions() {
        let limits = ResourceLimits {
            max_documents: DocumentLimit::new(1),
            max_file_size: SizeLimit::UNLIMITED,
        };
        let translator = Translator::new()
            .with_resource_limits(limits)
            .with_extensions(HashMap::new());

        let dir = TempDir::new().unwrap();
        let path_a = dir.path().join("a.rs");
        std::fs::write(&path_a, "a").unwrap();
        let path_b = dir.path().join("b.rs");
        std::fs::write(&path_b, "b").unwrap();

        let (client, _server) = fake_lsp_client();
        let server_id = ServerId::from_static("rust");

        translator
            .document_tracker
            .ensure_open(&path_a, &server_id, &client)
            .await
            .unwrap();
        translator
            .document_tracker
            .ensure_open(&path_b, &server_id, &client)
            .await
            .unwrap();
        assert_eq!(translator.document_tracker.len(), 1);
        assert!(!translator.document_tracker.is_open(&path_a));
        assert!(translator.document_tracker.is_open(&path_b));
    }

    /// `with_resource_limits` called *after* `with_extensions` (the reverse
    /// of `serve()`'s order) must still reach `document_tracker` -- the two
    /// builders must not clobber each other regardless of call order. See
    /// `Translator::with_resource_limits`'s docs.
    ///
    /// Uses a non-empty extension map (unlike the "before" test above) and
    /// asserts it survived `with_resource_limits`'s rebuild by checking the
    /// tracked document's resolved `language_id` -- a bug that dropped the
    /// extension map (e.g. rebuilding from `HashMap::new()` instead of
    /// `self.extension_map`) would leave `max_documents` correct but the
    /// extension map silently empty, which the "before" test alone cannot
    /// detect.
    #[tokio::test]
    async fn test_with_resource_limits_applies_after_with_extensions() {
        let limits = ResourceLimits {
            max_documents: DocumentLimit::new(1),
            max_file_size: SizeLimit::UNLIMITED,
        };
        let translator = Translator::new()
            .with_extensions(HashMap::from([(
                FileExtension::from_static("rs"),
                LanguageId::from_static("rust"),
            )]))
            .with_resource_limits(limits);

        let dir = TempDir::new().unwrap();
        let path_a = dir.path().join("a.rs");
        std::fs::write(&path_a, "a").unwrap();
        let path_b = dir.path().join("b.rs");
        std::fs::write(&path_b, "b").unwrap();

        let (client, _server) = fake_lsp_client();
        let server_id = ServerId::from_static("rust");

        translator
            .document_tracker
            .ensure_open(&path_a, &server_id, &client)
            .await
            .unwrap();
        translator
            .document_tracker
            .ensure_open(&path_b, &server_id, &client)
            .await
            .unwrap();
        assert_eq!(translator.document_tracker.len(), 1);
        assert!(!translator.document_tracker.is_open(&path_a));

        let state = translator.document_tracker.close(&path_b).unwrap();
        assert_eq!(
            state.language_id(),
            "rust",
            "extension map must have survived with_resource_limits's rebuild"
        );
    }

    fn refusal_of(config: &crate::config::LspServerConfig) -> ServerSpawnFailure {
        ServerSpawnFailure {
            reason: StartupFailure::RefusedUntrustedWorkspace(
                crate::error::UntrustedRefusal::NotAllowed {
                    builtin: Some(crate::config::BuiltinServer::RustAnalyzer),
                },
            ),
            ..spawn_failure(config)
        }
    }

    #[test]
    fn test_record_refusals_without_refusals_leaves_translator_untouched() {
        let config = crate::config::LspServerConfig::rust_analyzer();
        let translator =
            Translator::new().with_router(ToolRouter::from_configs([&config]).unwrap());
        let before = translator.router_snapshot();

        translator.record_refusals(&[]);

        assert!(translator.startup_failures().is_empty());
        assert!(Arc::ptr_eq(&before, &translator.router_snapshot()));
    }

    #[test]
    fn test_refused_server_without_catch_all_reports_the_refusal() {
        let config = crate::config::LspServerConfig::rust_analyzer();
        let translator = Translator::new()
            .with_extensions(crate::test_lsp::test_extensions())
            .with_router(ToolRouter::from_configs([&config]).unwrap());

        translator.record_refusals(&[refusal_of(&config)]);

        let err = translator
            .client_for_file(Path::new("/ws/main.rs"), ToolKind::Hover)
            .unwrap_err();
        assert_matches!(&err, Error::ServerFailedToStart(f) if f.server_id == config.id());
        let text = err.to_string();
        assert!(text.contains("'rust'"), "{text}");
        assert!(text.contains("--allow-server rust"), "{text}");
        assert!(!text.contains("not retried"), "{text}");
    }

    #[tokio::test]
    async fn test_refused_explicit_server_leaves_allowed_catch_all_serving() {
        let explicit = named_config("explicit", "rust", Some(vec![ToolKind::Hover]));
        let catch_all = named_config("allowed", "rust", None);
        let translator = Translator::new()
            .with_extensions(crate::test_lsp::test_extensions())
            .with_router(ToolRouter::from_configs([&explicit, &catch_all]).unwrap());
        translator.set_expected_servers(HashSet::from([catch_all.id()]));

        translator.record_refusals(&[refusal_of(&explicit)]);
        translator.settle_started(crate::lsp::fake_lsp_server_with_config(catch_all.clone()));

        let (routed, _client) = translator
            .client_for_file(Path::new("/ws/main.rs"), ToolKind::Hover)
            .unwrap();
        assert_eq!(routed, catch_all.id());
    }
}
