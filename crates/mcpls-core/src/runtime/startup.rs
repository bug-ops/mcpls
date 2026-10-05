//! Background LSP startup: starts the configured servers with bounded
//! concurrency, registers each as it settles, and supervises their
//! diagnostics pumps until shutdown.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;

use futures::{FutureExt as _, Stream, StreamExt as _};
use tokio::sync::Mutex;
use tokio::task::{JoinHandle, JoinSet};
use tracing::{error, info, warn};

use super::pump::{PumpShared, PumpWiring, degrade_after_pump_panic, diagnostics_pump};
use crate::bridge::{DiagnosticsRole, NotificationCache, Translator, WorkspaceRoots};
use crate::config::{LanguageId, ServerConfig, ServerId, ServerStartConcurrency};
use crate::error::ServerSpawnFailure;
use crate::lsp::{self, LspServer, ServerInitConfig, ServerStartOutcome};
use crate::mcp::SubscriptionRegistry;
use crate::redaction::Redactions;
use crate::util::panic_message;

/// The servers worth starting for this run: every configured server whose
/// project markers are found under at least one workspace root, paired with
/// the roots, position encodings and (for the TypeScript server) the pinned
/// `tsserver` it is initialized with.
pub fn plan_server_starts(
    config: &ServerConfig,
    roots: &WorkspaceRoots,
    redactions: &Arc<Redactions>,
) -> Vec<ServerInitConfig> {
    let max_depth = Some(config.workspace.heuristics_max_depth);
    config
        .lsp_servers
        .iter()
        .filter_map(|lsp_config| {
            let should_spawn = roots
                .canonical()
                .iter()
                .any(|root| lsp_config.should_spawn(root, max_depth));

            if !should_spawn {
                info!(
                    "Skipping LSP server '{}' ({}): no project markers found",
                    lsp_config.language_id, lsp_config.command
                );
                return None;
            }

            Some(ServerInitConfig {
                server_config: lsp_config.clone(),
                workspace_roots: roots.canonical().to_vec(),
                initialization_options: lsp::tsserver_pin::pinned_initialization_options(
                    lsp_config,
                    roots,
                    |key| std::env::var_os(key),
                ),
                position_encodings: config.workspace.position_encodings.clone(),
                redactions: Arc::clone(redactions),
            })
        })
        .collect()
}

/// Spawn the applicable LSP servers in a background task and register them into
/// the shared `translator` once ready.
///
/// This intentionally does NOT block the caller: `serve_with` starts the MCP
/// server immediately so its `initialize` handshake returns before slow language
/// servers (e.g. `OmniSharp` on a large Unity solution, which can take minutes to
/// load) finish initializing. Tool calls that arrive before a server has
/// registered return a `ServerInitializing` error telling the caller to wait and
/// retry. Once initialization settles, servers that failed to start are
/// reported through `Error::ServerFailedToStart` instead.
///
/// Returns the task's `JoinHandle` so [`shutdown`](super::shutdown::shutdown) can await it. A panic in the
/// task is contained by [`run_init_supervised`] rather than leaving every
/// affected tool on `ServerInitializing` forever (#528).
pub fn spawn_lsp_servers_background(
    applicable_configs: Vec<ServerInitConfig>,
    translator: Arc<Translator>,
    notification_cache: Arc<Mutex<NotificationCache>>,
    subscription_registry: SubscriptionRegistry,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    workspace_roots: WorkspaceRoots,
    max_concurrent_server_starts: ServerStartConcurrency,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let settle_registry = subscription_registry.clone();
        let body = init_lsp_servers(
            &applicable_configs,
            &translator,
            notification_cache,
            subscription_registry,
            cancel_rx,
            workspace_roots,
            max_concurrent_server_starts,
        );
        if Box::pin(run_init_supervised(&translator, &applicable_configs, body)).await
            == InitOutcome::Panicked
        {
            publish_startup_failures(&translator, &settle_registry).await;
        }
    })
}

/// How [`run_init_supervised`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitOutcome {
    /// The init body ran to completion.
    Completed,
    /// The init body panicked and the translator was settled after it.
    Panicked,
}

/// Tells subscribers of files whose server failed to start that a re-read now
/// returns the error.
///
/// Called once initialization has settled, after the translator state it reads
/// (recorded failures, router, expected set) is final.
pub async fn publish_startup_failures(translator: &Translator, registry: &SubscriptionRegistry) {
    if translator.startup_failures().is_empty() {
        return;
    }
    registry
        .publish_matching(|uri| translator.diagnostics_route_for_uri(uri).is_failed())
        .await;
}

/// Drives `body` (the LSP init sequence) to completion on the current task,
/// turning a panic in it into a settled translator state.
///
/// The body runs on the same task rather than an inner spawn so that aborting
/// the outer task (see [`await_lsp_init_handle`](super::shutdown::await_lsp_init_handle)) still drops every
/// not-yet-registered `Child` it owns. On a panic, every config that never
/// registered is recorded as `StartupFailure::InitTaskPanicked` and the
/// router and expected-server set are settled, so tools return a terminal
/// error instead of `ServerInitializing` indefinitely.
async fn run_init_supervised(
    translator: &Translator,
    configs: &[ServerInitConfig],
    body: impl std::future::Future<Output = ()>,
) -> InitOutcome {
    if let Err(payload) = AssertUnwindSafe(body).catch_unwind().await {
        error!(
            "Background LSP initialization task panicked: {}",
            panic_message(payload.as_ref())
        );
        translator.settle_after_init_panic(configs).await;
        InitOutcome::Panicked
    } else {
        InitOutcome::Completed
    }
}

/// Outcomes of starting a batch of servers, in completion order.
type StartStream<'a> = Pin<Box<dyn Stream<Item = ServerStartOutcome> + Send + 'a>>;

/// Starts `configs` with at most `limit` in flight at once.
fn start_servers(configs: &[ServerInitConfig], limit: ServerStartConcurrency) -> StartStream<'_> {
    if configs.len() > limit.get() {
        info!(
            "Starting {} LSP server(s), at most {} at a time",
            configs.len(),
            limit.get()
        );
    }
    Box::pin(
        futures::stream::iter(configs.iter().map(LspServer::start_contained))
            .buffer_unordered(limit.get()),
    )
}

/// The next settled start, or never while there is no start stream.
///
/// `tokio::select!` still evaluates the future of a disabled branch, so an
/// absent stream has to be a future that never completes rather than a guard.
async fn next_start(pending: &mut Option<StartStream<'_>>) -> Option<ServerStartOutcome> {
    match pending {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

/// Starts `configs` with at most `max_concurrent_starts` in flight, registers
/// each server the moment its own `initialize` settles, and runs their
/// diagnostics pumps until shutdown.
///
/// The start futures are polled on this (the supervised) task, so aborting it
/// drops every not-yet-registered `Child`. Cancellation abandons whatever is
/// still starting and registers nothing further.
async fn init_lsp_servers(
    configs: &[ServerInitConfig],
    translator: &Translator,
    notification_cache: Arc<Mutex<NotificationCache>>,
    subscription_registry: SubscriptionRegistry,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    workspace_roots: WorkspaceRoots,
    max_concurrent_starts: ServerStartConcurrency,
) {
    if configs.is_empty() {
        return;
    }
    let pump_shared = PumpShared {
        notification_cache: Arc::clone(&notification_cache),
        subs: subscription_registry,
        workspace_roots,
    };
    // Installed before the first server settles, so every settled server can
    // be restarted and no init path leaves a registered server unwired.
    let mut startup = Some(translator.begin_startup());
    translator.install_wiring(Arc::new(PumpWiring::new(
        pump_shared.clone(),
        cancel_rx.clone(),
    )));
    let mut settler = StartupSettler {
        translator,
        notification_cache,
        pump_shared,
        cancel_rx: cancel_rx.clone(),
        configured: configs
            .iter()
            .map(|config| {
                (
                    config.server_config.id(),
                    config.server_config.language_id.clone(),
                )
            })
            .collect(),
        roles: HashMap::new(),
        pumps: JoinSet::new(),
        pump_servers: HashMap::new(),
        tally: StartupTally::default(),
    };

    let mut cancel_rx = cancel_rx;
    let mut cancelled = *cancel_rx.borrow();
    let mut pending = (!cancelled).then(|| start_servers(configs, max_concurrent_starts));
    loop {
        if pending.is_none() {
            drop(startup.take());
            if settler.pumps.is_empty() {
                break;
            }
        }
        tokio::select! {
            biased;
            _ = cancel_rx.changed(), if !cancelled => {
                cancelled = true;
                pending = None;
            }
            outcome = next_start(&mut pending) => {
                if let Some(outcome) = outcome {
                    settler.settle(outcome).await;
                } else {
                    pending = None;
                    if !cancelled {
                        settler.tally.log_summary();
                    }
                }
            }
            Some(joined) = settler.pumps.join_next_with_id(), if !settler.pumps.is_empty() => {
                handle_pump_exit(joined, &settler.pump_servers, &settler.notification_cache).await;
            }
        }
    }
}

/// How many servers have registered and failed so far.
#[derive(Debug, Default, Clone, Copy)]
struct StartupTally {
    registered: usize,
    failed: usize,
}

impl StartupTally {
    /// Logs the aggregate outcome; called once, when the last server settles.
    fn log_summary(self) {
        let Self { registered, failed } = self;
        if registered == 0 {
            error!("All {failed} configured LSP server(s) failed to initialize");
            return;
        }
        if failed > 0 {
            warn!("Partial server initialization: {registered} succeeded, {failed} failed");
        }
        info!("Proceeding with {registered} LSP server(s)");
    }
}

/// Settles servers one at a time for [`init_lsp_servers`].
struct StartupSettler<'a> {
    translator: &'a Translator,
    notification_cache: Arc<Mutex<NotificationCache>>,
    pump_shared: PumpShared,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    /// `(id, language)` of every configured server, settled or not.
    configured: Vec<(ServerId, LanguageId)>,
    roles: HashMap<ServerId, (LanguageId, tokio::sync::watch::Sender<DiagnosticsRole>)>,
    pumps: JoinSet<()>,
    pump_servers: HashMap<tokio::task::Id, ServerId>,
    tally: StartupTally,
}

impl StartupSettler<'_> {
    async fn settle(&mut self, outcome: ServerStartOutcome) {
        match outcome {
            ServerStartOutcome::Started(server) => self.settle_started(*server).await,
            ServerStartOutcome::Failed(failure) => self.settle_failed(failure).await,
        }
    }

    /// Receivers and indexing policy are taken before the server is visible;
    /// cancel is re-checked right before registering.
    async fn settle_started(&mut self, mut server: LspServer) {
        if *self.cancel_rx.borrow() {
            return;
        }
        let notification_rx = server.take_notification_rx();
        let lifecycle_rx = server.take_lifecycle_rx();
        let pinned_tsserver = lsp::tsserver_pin::configured_tsserver_path(
            server.init_config().initialization_options.as_ref(),
        );
        let policy = server.init_config().server_config.indexing;
        let config_id = server.init_config().server_config.id();
        self.notification_cache
            .lock()
            .await
            .set_indexing_policy(config_id, policy);
        if *self.cancel_rx.borrow() {
            return;
        }

        // A restart waits on this lock, so it never runs between the server
        // becoming visible and its initial pump being registered.
        let respawn_lock = self
            .translator
            .respawn_lock(&server.init_config().server_config.id());
        let serialized = respawn_lock.lock().await;
        let (id, language) = self.translator.settle_started(server);
        let (role_tx, role_rx) =
            tokio::sync::watch::channel(self.diagnostics_role(language.as_str(), &id));
        self.roles.insert(id.clone(), (language, role_tx));
        self.recompute_roles().await;
        let pump = self.pumps.spawn(diagnostics_pump(
            id.clone(),
            notification_rx,
            lifecycle_rx,
            self.cancel_rx.clone(),
            role_rx,
            pinned_tsserver,
            self.pump_shared.clone(),
        ));
        self.translator.set_notification_task(&id, pump.clone());
        drop(serialized);
        self.pump_servers.insert(pump.id(), id.clone());
        self.tally.registered = self.tally.registered.saturating_add(1);
        self.publish_routes_served_by(&id).await;
    }

    /// Notifies subscribers of files now served by `id`, which may have been
    /// told "starting" while the route was unbound.
    async fn publish_routes_served_by(&self, id: &ServerId) {
        self.pump_shared
            .subs
            .publish_matching(|uri| {
                self.translator.diagnostics_route_for_uri(uri).server_id() == Some(id)
            })
            .await;
    }

    /// Records the failure, re-evaluates roles, notifies failed routes.
    async fn settle_failed(&mut self, failure: ServerSpawnFailure) {
        error!("Server initialization failed: {failure}");
        self.translator.settle_failed(&failure);
        self.tally.failed = self.tally.failed.saturating_add(1);
        self.recompute_roles().await;
        publish_startup_failures(self.translator, &self.pump_shared.subs).await;
    }

    fn diagnostics_role(&self, language: &str, id: &ServerId) -> DiagnosticsRole {
        DiagnosticsRole::from_route(self.translator.is_diagnostics_route(language, id))
    }

    /// Pending servers count toward the route count, so it equals the batch
    /// value once every server has settled.
    async fn recompute_roles(&self) {
        for (id, (language, role_tx)) in &self.roles {
            let role = self.diagnostics_role(language.as_str(), id);
            role_tx.send_if_modified(|current| {
                let changed = *current != role;
                *current = role;
                changed
            });
        }
        let route_count = self
            .configured
            .iter()
            .filter(|(id, language)| {
                self.translator.startup_failure(id).is_none()
                    && self.translator.is_diagnostics_route(language.as_str(), id)
            })
            .count();
        self.notification_cache
            .lock()
            .await
            .set_diagnostics_route_count(route_count);
    }
}

/// A panicked pump stops caching its server's pushes: mark it push-degraded.
async fn handle_pump_exit(
    joined: Result<(tokio::task::Id, ()), tokio::task::JoinError>,
    pump_servers: &HashMap<tokio::task::Id, ServerId>,
    notification_cache: &Mutex<NotificationCache>,
) {
    let Err(join_error) = joined else { return };
    if !join_error.is_panic() {
        return;
    }
    let Some(server_id) = pump_servers.get(&join_error.id()) else {
        return;
    };
    error!("Diagnostics pump for LSP server '{server_id}' panicked: {join_error}");
    degrade_after_pump_panic(notification_cache, server_id).await;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod settler_tests {
    use super::*;
    use crate::bridge::WorkspaceRoots;
    use crate::config::{LspServerConfig, ToolKind, ToolRouter};
    use crate::error::StartupFailure;

    fn settler_for<'a>(
        translator: &'a Translator,
        cache: &Arc<Mutex<NotificationCache>>,
        subs: SubscriptionRegistry,
        configs: &[&LspServerConfig],
    ) -> StartupSettler<'a> {
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        StartupSettler {
            translator,
            notification_cache: Arc::clone(cache),
            pump_shared: PumpShared {
                notification_cache: Arc::clone(cache),
                subs,
                workspace_roots: WorkspaceRoots::default(),
            },
            cancel_rx,
            configured: configs
                .iter()
                .map(|c| (c.id(), c.language_id.clone()))
                .collect(),
            roles: HashMap::new(),
            pumps: JoinSet::new(),
            pump_servers: HashMap::new(),
            tally: StartupTally::default(),
        }
    }

    fn failure_of(config: &LspServerConfig) -> ServerStartOutcome {
        ServerStartOutcome::Failed(ServerSpawnFailure {
            server_id: config.id(),
            language_id: config.language_id.clone(),
            command: config.command.clone(),
            reason: StartupFailure::InitTaskPanicked,
        })
    }

    fn explicit_and_catch_all_translator() -> (Translator, LspServerConfig, LspServerConfig) {
        let mut explicit = LspServerConfig::rust_analyzer();
        explicit.name = Some("explicit".to_string());
        explicit.handles = Some(vec![ToolKind::Diagnostics]);
        let mut catch_all = LspServerConfig::rust_analyzer();
        catch_all.name = Some("catch-all".to_string());
        let translator = Translator::new()
            .with_extensions(crate::test_lsp::test_extensions())
            .with_router(ToolRouter::from_configs([&explicit, &catch_all]).unwrap());
        translator.set_expected_servers([explicit.id(), catch_all.id()].into_iter().collect());
        (translator, explicit, catch_all)
    }

    async fn subscribe_to_main_rs(
        registry: &SubscriptionRegistry,
    ) -> (
        crate::mcp::SessionHandle,
        tokio::sync::mpsc::Receiver<String>,
        crate::bridge::resources::DiagnosticsResourceUri,
    ) {
        use crate::bridge::resources::make_uri;
        use crate::mcp::{SessionHandle, Target};

        let session = SessionHandle::new(registry.clone());
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let uri = crate::test_lsp::diagnostics_uri(
            &make_uri(&crate::test_lsp::absolute_path("main.rs")).unwrap(),
        );
        session
            .subscribe_for_test(&uri, Target::Channel(tx))
            .await
            .unwrap();
        (session, rx, uri)
    }

    /// FR-016: while the explicit diagnostics server is failed and the
    /// catch-all still starting, subscribers hear nothing (the route is
    /// merely starting); they are told once the catch-all fails too.
    #[tokio::test]
    async fn startup_failure_publish_reaches_routes_attributed_via_configured_router() {
        let (translator, explicit, catch_all) = explicit_and_catch_all_translator();
        let registry = SubscriptionRegistry::new();
        let (_session, mut rx, uri) = subscribe_to_main_rs(&registry).await;
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let mut settler = settler_for(&translator, &cache, registry, &[&explicit, &catch_all]);

        settler.settle(failure_of(&explicit)).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "the route is only starting while the catch-all is pending"
        );

        settler.settle(failure_of(&catch_all)).await;
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("notified once the catch-all failed")
            .unwrap();
        assert_eq!(got, uri.as_str());
    }

    /// A subscriber of a file whose explicit server failed is told to
    /// re-read once the catch-all registers and serves the route.
    #[tokio::test]
    async fn catch_all_registration_notifies_subscribers_of_recovered_routes() {
        let (translator, explicit, catch_all) = explicit_and_catch_all_translator();
        let registry = SubscriptionRegistry::new();
        let (_session, mut rx, uri) = subscribe_to_main_rs(&registry).await;
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let mut settler = settler_for(&translator, &cache, registry, &[&explicit, &catch_all]);

        settler.settle(failure_of(&explicit)).await;
        settler
            .settle(ServerStartOutcome::Started(Box::new(
                crate::lsp::fake_lsp_server_with_config(catch_all.clone()),
            )))
            .await;

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("notified when the catch-all registered")
            .unwrap();
        assert_eq!(got, uri.as_str());
    }

    /// Cancellation that lands while the cache lock is awaited also drops it.
    #[tokio::test]
    async fn settle_started_cancelled_during_cache_wait_does_not_register() {
        let config = LspServerConfig::rust_analyzer();
        let translator = Translator::new();
        translator.set_expected_servers(std::iter::once(config.id()).collect());
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let mut settler = settler_for(&translator, &cache, SubscriptionRegistry::new(), &[&config]);
        settler.cancel_rx = cancel_rx;
        let guard = cache.lock().await;

        let settle = settler.settle(ServerStartOutcome::Started(Box::new(
            crate::lsp::fake_lsp_server_with_config(config.clone()),
        )));
        let release = async {
            tokio::task::yield_now().await;
            cancel_tx.send(true).unwrap();
            drop(guard);
        };
        tokio::join!(settle, release);

        assert_eq!(translator.registered_server_count(), 0);
    }

    /// Cancellation that lands while a server finishes starting drops it
    /// instead of registering it after shutdown drained the registry.
    #[tokio::test]
    async fn settle_started_after_cancel_does_not_register() {
        let config = LspServerConfig::rust_analyzer();
        let translator = Translator::new();
        translator.set_expected_servers(std::iter::once(config.id()).collect());
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let mut settler = settler_for(&translator, &cache, SubscriptionRegistry::new(), &[&config]);
        settler.cancel_rx = cancel_rx;
        cancel_tx.send(true).unwrap();

        settler
            .settle(ServerStartOutcome::Started(Box::new(
                crate::lsp::fake_lsp_server_with_config(config.clone()),
            )))
            .await;

        assert_eq!(translator.registered_server_count(), 0);
    }

    /// M4/M6: the catch-all registers as `Secondary` while the explicit
    /// diagnostics server is pending, becomes `Authoritative` once that
    /// server fails, and the route count equals the batch value.
    #[tokio::test]
    async fn catch_all_role_flips_when_explicit_diagnostics_server_fails() {
        let mut explicit = LspServerConfig::rust_analyzer();
        explicit.name = Some("explicit".to_string());
        explicit.handles = Some(vec![ToolKind::Diagnostics]);
        let mut catch_all = LspServerConfig::rust_analyzer();
        catch_all.name = Some("catch-all".to_string());
        let router = ToolRouter::from_configs([&explicit, &catch_all]).unwrap();
        let translator = Translator::new().with_router(router);
        translator.set_expected_servers([explicit.id(), catch_all.id()].into_iter().collect());
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let mut settler = settler_for(
            &translator,
            &cache,
            SubscriptionRegistry::new(),
            &[&explicit, &catch_all],
        );

        settler
            .settle(ServerStartOutcome::Started(Box::new(
                crate::lsp::fake_lsp_server_with_config(catch_all.clone()),
            )))
            .await;
        let role = |settler: &StartupSettler<'_>| *settler.roles[&catch_all.id()].1.borrow();
        assert_eq!(role(&settler), DiagnosticsRole::Secondary);
        assert_eq!(cache.lock().await.configured_route_count(), Some(1));

        settler
            .settle(ServerStartOutcome::Failed(ServerSpawnFailure {
                server_id: explicit.id(),
                language_id: LanguageId::from_static("rust"),
                command: explicit.command.clone(),
                reason: StartupFailure::InitTaskPanicked,
            }))
            .await;

        assert_eq!(role(&settler), DiagnosticsRole::Authoritative);
        assert_eq!(cache.lock().await.configured_route_count(), Some(1));
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod startup_tests {
    use std::path::Path;

    use super::*;
    use crate::bridge::{RouteSupport, WorkspaceRoots};
    use crate::config::{ToolKind, ToolRouter};
    use crate::error::StartupFailure;
    use crate::test_lsp::{answer_initialize_script, named_sh_init_config};

    struct Startup {
        translator: Arc<Translator>,
        task: JoinHandle<()>,
        cancel_tx: tokio::sync::watch::Sender<bool>,
    }

    fn start(configs: Vec<ServerInitConfig>) -> Startup {
        start_limited(configs, ServerStartConcurrency::DEFAULT)
    }

    fn start_limited(configs: Vec<ServerInitConfig>, limit: ServerStartConcurrency) -> Startup {
        let router = ToolRouter::from_configs(configs.iter().map(|c| &c.server_config)).unwrap();
        let translator = Arc::new(
            Translator::new()
                .with_extensions(crate::test_lsp::test_extensions())
                .with_router(router),
        );
        translator.set_expected_servers(configs.iter().map(|c| c.server_config.id()).collect());
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let task = spawn_lsp_servers_background(
            configs,
            Arc::clone(&translator),
            cache,
            SubscriptionRegistry::new(),
            cancel_rx,
            WorkspaceRoots::default(),
            limit,
        );
        Startup {
            translator,
            task,
            cancel_tx,
        }
    }

    fn support(translator: &Translator, language: &str) -> RouteSupport {
        translator
            .tool_support_snapshot()
            .document_support(language, ToolKind::Hover)
    }

    async fn wait_until(what: &str, condition: impl Fn() -> bool + Send + Sync) {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while !condition() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    /// US-001/US-002: the fast server is usable while the slow one is still
    /// initializing, whichever is listed first.
    #[tokio::test]
    async fn fast_server_registers_while_slow_sibling_initializes() {
        for slow_first in [true, false] {
            let dir = tempfile::TempDir::new().unwrap();
            let gate = dir.path().join("gate");
            let fast = named_sh_init_config(
                dir.path(),
                "fast",
                "rust",
                &answer_initialize_script(None, None),
            );
            let slow = named_sh_init_config(
                dir.path(),
                "slow",
                "typescriptreact",
                &answer_initialize_script(None, Some(&gate)),
            );
            let configs = if slow_first {
                vec![slow, fast]
            } else {
                vec![fast, slow]
            };
            let startup = start(configs);

            wait_until("the fast server to register", || {
                !matches!(
                    support(&startup.translator, "rust"),
                    RouteSupport::Initializing
                )
            })
            .await;
            assert_eq!(
                support(&startup.translator, "typescriptreact"),
                RouteSupport::Initializing,
                "slow_first: {slow_first}"
            );

            std::fs::write(&gate, "").unwrap();
            wait_until("the slow server to register", || {
                !matches!(
                    support(&startup.translator, "typescriptreact"),
                    RouteSupport::Initializing
                )
            })
            .await;
            startup.cancel_tx.send(true).unwrap();
            startup.task.await.unwrap();
        }
    }

    /// US-003: a failed server reports its failure at once while its sibling
    /// still initializes.
    #[tokio::test]
    async fn failed_server_reports_failure_while_sibling_initializes() {
        let dir = tempfile::TempDir::new().unwrap();
        let gate = dir.path().join("gate");
        let mut broken = named_sh_init_config(dir.path(), "broken", "rust", "exit 1\n");
        broken.server_config.command = "mcpls-no-such-language-server".to_string();
        let slow = named_sh_init_config(
            dir.path(),
            "slow",
            "typescriptreact",
            &answer_initialize_script(None, Some(&gate)),
        );
        let startup = start(vec![broken, slow]);

        wait_until("the broken server to settle", || {
            startup
                .translator
                .diagnostics_route_for_path(Path::new("/ws/main.rs"))
                .is_failed()
        })
        .await;
        assert_eq!(
            support(&startup.translator, "typescriptreact"),
            RouteSupport::Initializing
        );
        let failure = startup
            .translator
            .startup_failure(&ServerId::from("broken"))
            .unwrap();
        std::assert_matches!(failure.reason, StartupFailure::Spawn(_));

        startup.cancel_tx.send(true).unwrap();
        startup.task.await.unwrap();
    }

    /// With fewer start slots than servers, the last failure still ends
    /// the task and logs the aggregate outcome.
    #[tokio::test]
    async fn failures_beyond_the_start_limit_still_settle_and_end_the_task() {
        use tracing_subscriber::prelude::*;

        use crate::test_lsp::CapturedLogs;

        let dir = tempfile::TempDir::new().unwrap();
        let configs = ["rust", "python", "go"]
            .map(|language| {
                let mut config = named_sh_init_config(dir.path(), language, language, "exit 1\n");
                config.server_config.command = "mcpls-no-such-language-server".to_string();
                config
            })
            .to_vec();
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(logs.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        let startup = start_limited(configs, ServerStartConcurrency::new(2).unwrap());

        tokio::time::timeout(std::time::Duration::from_secs(10), startup.task)
            .await
            .unwrap()
            .unwrap();
        drop(guard);

        assert_eq!(startup.translator.startup_failures().len(), 3);
        assert!(
            logs.messages()
                .iter()
                .any(|m| m.contains("All 3 configured LSP server(s) failed")),
            "{:?}",
            logs.messages()
        );
    }

    /// With one start slot, the second server starts only after the first
    /// has settled.
    #[tokio::test]
    async fn start_limit_holds_back_servers_until_a_slot_frees() {
        let dir = tempfile::TempDir::new().unwrap();
        let (a_up, b_up, gate) = (
            dir.path().join("a_up"),
            dir.path().join("b_up"),
            dir.path().join("gate"),
        );
        let first = named_sh_init_config(
            dir.path(),
            "first",
            "rust",
            &answer_initialize_script(Some(&a_up), Some(&gate)),
        );
        let second = named_sh_init_config(
            dir.path(),
            "second",
            "python",
            &answer_initialize_script(Some(&b_up), None),
        );
        let startup = start_limited(vec![first, second], ServerStartConcurrency::new(1).unwrap());

        wait_until("the first server to start", || a_up.exists()).await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            !b_up.exists(),
            "second server started while the slot was taken"
        );

        std::fs::write(&gate, "").unwrap();
        wait_until("the second server to start", || b_up.exists()).await;
        startup.cancel_tx.send(true).unwrap();
        startup.task.await.unwrap();
    }

    /// FR-014/FR-015 and review N5: cancellation abandons servers that are
    /// still starting without registering them, recording a failure for
    /// them or logging an aggregate outcome.
    #[tokio::test]
    async fn cancel_during_startup_stops_registering_pending_servers() {
        use tracing_subscriber::prelude::*;

        use crate::test_lsp::CapturedLogs;

        let dir = tempfile::TempDir::new().unwrap();
        let never = dir.path().join("never");
        let up = dir.path().join("up");
        let stuck = named_sh_init_config(
            dir.path(),
            "stuck",
            "rust",
            &answer_initialize_script(Some(&up), Some(&never)),
        );
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(logs.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        let startup = start(vec![stuck]);

        wait_until("the stuck server to start", || up.exists()).await;
        startup.cancel_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), startup.task)
            .await
            .unwrap()
            .unwrap();
        drop(guard);

        assert_eq!(startup.translator.registered_server_count(), 0);
        assert!(startup.translator.startup_failures().is_empty());
        let aggregate: Vec<String> = logs
            .messages()
            .into_iter()
            .filter(|m| m.contains("Proceeding with") || m.contains("failed to initialize"))
            .collect();
        assert!(aggregate.is_empty(), "{aggregate:?}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod init_supervision_tests {
    use std::assert_matches;
    use std::collections::HashSet;

    use super::*;
    use crate::Error;
    use crate::bridge::IndexingState;
    use crate::config::LspServerConfig;
    use crate::error::StartupFailure;

    fn mark_ready(cache: &mut NotificationCache, id: &ServerId) {
        cache.observe_indexing_signal(
            id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        assert_eq!(cache.indexing_state(id), IndexingState::Ready);
    }

    #[tokio::test]
    async fn test_run_init_supervised_records_panic_for_unregistered_servers() {
        let translator = Translator::new();
        let config = crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer());
        let id = config.server_config.id();

        run_init_supervised(&translator, &[config], async {
            panic!("init boom");
        })
        .await;

        let failure = translator.startup_failure(&id).unwrap();
        assert_matches!(failure.reason, StartupFailure::InitTaskPanicked);
    }

    #[tokio::test]
    async fn test_run_init_supervised_reports_outcome() {
        let translator = Translator::new();
        let config = crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer());

        let completed =
            run_init_supervised(&translator, std::slice::from_ref(&config), async {}).await;
        let panicked = run_init_supervised(&translator, &[config], async {
            panic!("init boom");
        })
        .await;

        assert_eq!(completed, InitOutcome::Completed);
        assert_eq!(panicked, InitOutcome::Panicked);
    }

    /// #535: once settling recorded a failure, subscribers of a file the
    /// failed server would have served are told to re-read; before that
    /// nothing publishes.
    #[tokio::test]
    async fn test_publish_startup_failures_notifies_failed_uris_after_settle() {
        use crate::bridge::resources::make_uri;
        use crate::config::ToolRouter;
        use crate::mcp::{SessionHandle, Target};

        let config = crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer());
        let id = config.server_config.id();
        let translator = Translator::new()
            .with_extensions(crate::test_lsp::test_extensions())
            .with_router(ToolRouter::catch_all([(
                id,
                LanguageId::from_static("rust"),
            )]));
        translator.set_expected_servers(HashSet::from([config.server_config.id()]));
        let registry = SubscriptionRegistry::new();
        let session = SessionHandle::new(registry.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let failed = crate::test_lsp::diagnostics_uri(
            &make_uri(&crate::test_lsp::absolute_path("main.rs")).unwrap(),
        );
        session
            .subscribe_for_test(&failed, Target::Channel(tx))
            .await
            .unwrap();

        publish_startup_failures(&translator, &registry).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "no failure is recorded yet, so nothing may publish"
        );

        run_init_supervised(&translator, &[config], async {
            panic!("init boom");
        })
        .await;
        publish_startup_failures(&translator, &registry).await;

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, failed.as_str());
    }

    #[tokio::test]
    async fn test_run_init_supervised_leaves_translator_alone_without_panic() {
        let translator = Translator::new();
        let config = crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer());
        let id = config.server_config.id();

        run_init_supervised(&translator, &[config], async {}).await;

        assert!(translator.startup_failure(&id).is_none());
    }

    #[tokio::test]
    async fn test_run_init_supervised_keeps_existing_spawn_failure() {
        let translator = Translator::new();
        let config = crate::test_lsp::init_config_for(LspServerConfig::rust_analyzer());
        let id = config.server_config.id();
        translator.record_startup_failures(&[crate::error::ServerSpawnFailure {
            server_id: id.clone(),
            language_id: LanguageId::from_static("rust"),
            command: "rust-analyzer".to_string(),
            reason: StartupFailure::Spawn(Arc::new(Error::ServerTerminated)),
        }]);

        run_init_supervised(&translator, &[config], async {
            panic!("init boom");
        })
        .await;

        let failure = translator.startup_failure(&id).unwrap();
        assert_matches!(failure.reason, StartupFailure::Spawn(_));
    }

    #[tokio::test]
    async fn test_run_init_supervised_degrades_registered_servers_after_panic() {
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let translator = Translator::new().with_notification_cache(Arc::clone(&cache));
        let server_config = LspServerConfig::rust_analyzer();
        let id = server_config.id();
        translator.register_server_complete(crate::lsp::fake_lsp_server_with_config(
            server_config.clone(),
        ));
        mark_ready(&mut *cache.lock().await, &id);

        run_init_supervised(
            &translator,
            &[crate::test_lsp::init_config_for(server_config)],
            async {
                panic!("init boom");
            },
        )
        .await;

        let guard = cache.lock().await;
        assert!(guard.is_push_degraded(&id));
        assert_eq!(guard.indexing_state(&id), IndexingState::Unknown);
        drop(guard);
        assert!(translator.startup_failure(&id).is_none());
    }

    #[tokio::test]
    async fn test_drain_pumps_degrades_server_of_panicked_pump() {
        let cache = Mutex::new(NotificationCache::new());
        let id = ServerId::from("rust");
        mark_ready(&mut *cache.lock().await, &id);

        let mut pumps = JoinSet::new();
        let pump = pumps.spawn(async {
            panic!("pump boom");
        });
        let pump_servers = HashMap::from([(pump.id(), id.clone())]);

        while let Some(joined) = pumps.join_next_with_id().await {
            handle_pump_exit(joined, &pump_servers, &cache).await;
        }

        let guard = cache.lock().await;
        assert!(guard.is_push_degraded(&id));
        assert_eq!(guard.indexing_state(&id), IndexingState::Unknown);
    }

    #[tokio::test]
    async fn test_drain_pumps_ignores_pump_that_finished_normally() {
        let cache = Mutex::new(NotificationCache::new());
        let id = ServerId::from("rust");

        let mut pumps = JoinSet::new();
        let pump = pumps.spawn(async {});
        let pump_servers = HashMap::from([(pump.id(), id.clone())]);

        while let Some(joined) = pumps.join_next_with_id().await {
            handle_pump_exit(joined, &pump_servers, &cache).await;
        }

        assert!(!cache.lock().await.is_push_degraded(&id));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod plan_tests {
    use super::*;
    use crate::config::LspServerConfig;

    fn config_with_rust_analyzer() -> ServerConfig {
        ServerConfig {
            lsp_servers: vec![LspServerConfig::rust_analyzer()],
            ..ServerConfig::default()
        }
    }

    fn plan(config: &ServerConfig, roots: &WorkspaceRoots) -> Vec<ServerInitConfig> {
        let redactions = Arc::new(Redactions::for_servers(
            &config.lsp_servers,
            lsp::current_environment(),
        ));
        plan_server_starts(config, roots, &redactions)
    }

    #[test]
    fn plan_skips_server_without_project_markers() {
        let dir = tempfile::TempDir::new().unwrap();
        let roots = WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap();

        assert!(plan(&config_with_rust_analyzer(), &roots).is_empty());
    }

    #[test]
    fn plan_keeps_server_with_project_markers() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let roots = WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap();

        let plan = plan(&config_with_rust_analyzer(), &roots);

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].workspace_roots, roots.canonical());
    }
}
