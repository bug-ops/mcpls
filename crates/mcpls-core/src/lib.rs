//! # mcpls-core
//!
//! Core library for MCP (Model Context Protocol) to LSP (Language Server Protocol) translation.
//!
//! This crate provides the fundamental building blocks for bridging AI agents with
//! language servers, enabling semantic code intelligence through MCP tools.
//!
//! ## Architecture
//!
//! The library is organized into several modules:
//!
//! - [`lsp`] - LSP client implementation for communicating with language servers
//! - [`mcp`] - MCP tool definitions and handlers
//! - [`bridge`] - Translation layer between MCP and LSP protocols
//! - [`config`] - Configuration types and loading
//! - [`mod@error`] - Error types for the library
//!
//! ## Example
//!
//! ```rust,ignore
//! use mcpls_core::{serve, serve_with, Transport, ServerConfig};
//!
//! #[tokio::main]
//! async fn main() {
//!     let config = ServerConfig::load().expect("failed to load config");
//!     // Stdio (default):
//!     let result = serve(config).await;
//!     // HTTP (requires `transport-http` feature):
//!     // let http = mcpls_core::HttpConfig::new("127.0.0.1:3000".parse().unwrap());
//!     // let result = serve_with(config, Transport::Http(http)).await;
//!
//!     // See `serve`/`serve_with`'s "Shutdown" docs: process::exit avoids a
//!     // runtime-shutdown hang under the stdio transport.
//!     std::process::exit(if result.is_ok() { 0 } else { 1 });
//! }
//! ```

#![cfg_attr(
    not(test),
    warn(clippy::arithmetic_side_effects, clippy::indexing_slicing)
)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod bridge;
pub mod config;
pub mod error;
pub mod lsp;
pub mod mcp;
mod redaction;
pub mod transport;
mod util;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod test_lsp;

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bridge::resources::{DiagnosticsResourceUri, PublishedDiagnosticsUri};
use bridge::{
    NotificationCache, Publication, PublicationKind, PublishedPathResolver, Translator,
    WorkspaceRoots,
};
use config::{LanguageId, ServerId, ServerStartConcurrency, ToolRouter};
pub use config::{ProjectConfigStatus, ProjectConfigTrust, ServerConfig};
pub use error::Error;
use error::ServerSpawnFailure;
use futures::{FutureExt as _, Stream, StreamExt as _};
use lsp::tsserver_pin::warn_if_pin_ignored;
use lsp::{LspNotification, LspServer, ServerInitConfig, ServerStartOutcome};
use mcp::SubscriptionRegistry;
use tokio::sync::Mutex;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, error, info, warn};
pub use transport::Transport;
#[cfg(feature = "transport-http")]
use transport::run_http;
#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
pub use transport::{
    AllowedHost, AllowedOrigin, ConnectionLimit, HeaderReadTimeout, HttpConfig, HttpPath,
    InvalidAllowedHost, InvalidAllowedOrigin, InvalidHttpPath, LeaseWindow, ListenLease,
    ProbeDeadline, ProbeInterval, RequestBodyLimit, ResponseStreamDeadline, SessionLimit,
    StreamLiveness, WriteStallTimeout,
};
use transport::{ShutdownSignal, run_stdio};
pub use util::{escape_control, needs_control_escape};

/// `Arc`-backed state shared by every `diagnostics_pump` task spawned for one
/// `serve_with` run, factored out of `diagnostics_pump`'s parameter list to
/// keep it under clippy's argument-count lint. `Clone` is cheap (`Arc`
/// clones only).
#[derive(Clone)]
pub(crate) struct PumpShared {
    pub(crate) notification_cache: Arc<Mutex<NotificationCache>>,
    /// Every session that has subscribed to anything (one per HTTP session,
    /// or the single stdio session); the pump hands each one the URI and the
    /// session decides whether it is subscribed -- see [`SubscriptionRegistry`].
    pub(crate) subs: SubscriptionRegistry,
    /// Used to reject diagnostics for out-of-workspace URIs (see #234): a
    /// misbehaving server could otherwise flush the FIFO-capped cache with
    /// fabricated URIs.
    pub(crate) workspace_roots: WorkspaceRoots,
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
pub(crate) enum DiagnosticsRole {
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

/// Background task that drains LSP notifications, writes them to the cache,
/// and queues `resources/updated` for each MCP session subscribed to the URI.
///
/// Selects over two independent lanes (P3) rather than one: `rx` carries
/// diagnostics/log/showMessage, `lifecycle_rx` carries `$/progress`
/// `begin`/`end` frames and `Other` (which carries e.g. rust-analyzer's
/// `experimental/serverStatus`). Splitting them means a high-volume
/// diagnostics publisher (rust-analyzer republishing whole-workspace
/// diagnostics on every save) can never starve out a low-volume readiness
/// signal, or vice versa -- see `lsp::client::LspClient::message_loop_inner`
/// for where each notification is classified onto its lane.
///
/// Delivery is per session and never awaits a peer: the pump only calls
/// `SessionState::publish_if_subscribed` on each registered session, whose own
/// task notifies that session's peer. A session that has not subscribed yet is
/// not registered, so notifications arriving before the first subscribe are
/// only cached.
///
/// The task exits when:
/// - **Both** lanes have closed (`rx.recv()` and `lifecycle_rx.recv()` both
///   returned `None`) -- in practice both senders live inside the same
///   `LspClient` and close together, but each lane is tracked independently
///   so one closing early can never stop the other from still being drained.
/// - The cancellation watch fires (or the sender is dropped).
///
/// # Lock independence
/// Cache writes acquire only `Arc<Mutex<NotificationCache>>`, a lock entirely
/// separate from `translator`'s own internal locks (`Arc<Translator>` has no
/// outer mutex; each field manages its own short-lived, independent lock).
/// Neither an in-flight LSP round-trip (e.g. `textDocument/diagnostic`) nor
/// any other translator-side work holds the notification-cache lock, so this
/// pump is never blocked by tool-call activity: a `publishDiagnostics`
/// notification arriving mid-request is cached immediately instead of being
/// silently dropped. This matters because the LSP transport forwards
/// notifications via `mpsc::Sender::try_send`, which drops on a full channel
/// rather than blocking — a pump stalled behind someone else's lock would
/// previously lose notifications under sustained push traffic.
pub(crate) async fn diagnostics_pump(
    server_id: ServerId,
    rx: tokio::sync::mpsc::Receiver<LspNotification>,
    lifecycle_rx: tokio::sync::mpsc::Receiver<LspNotification>,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    role_rx: tokio::sync::watch::Receiver<DiagnosticsRole>,
    pinned_tsserver: Option<PathBuf>,
    shared: PumpShared,
) {
    diagnostics_pump_with_resolver(
        server_id,
        rx,
        lifecycle_rx,
        cancel_rx,
        role_rx,
        pinned_tsserver,
        shared,
        PublishedPathResolver::new(),
    )
    .await;
}

/// [`diagnostics_pump`] over a caller-supplied path resolver, so tests can
/// inject a slow or hanging canonicalizer.
#[allow(
    clippy::too_many_arguments,
    reason = "internal test seam; each parameter is a distinct per-server input"
)]
async fn diagnostics_pump_with_resolver(
    server_id: ServerId,
    mut rx: tokio::sync::mpsc::Receiver<LspNotification>,
    mut lifecycle_rx: tokio::sync::mpsc::Receiver<LspNotification>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    role_rx: tokio::sync::watch::Receiver<DiagnosticsRole>,
    pinned_tsserver: Option<PathBuf>,
    shared: PumpShared,
    mut resolver: PublishedPathResolver,
) {
    let PumpShared {
        notification_cache,
        subs,
        workspace_roots,
    } = shared;
    let mut notification_closed = false;
    let mut lifecycle_closed = false;
    'pump: loop {
        if notification_closed && lifecycle_closed {
            break;
        }
        tokio::select! {
            () = cancelled(&mut cancel_rx) => break,
            msg = rx.recv(), if !notification_closed => {
                let Some(first) = msg else {
                    notification_closed = true;
                    continue;
                };
                let mut batch = vec![first];
                while batch.len() < PUMP_BATCH {
                    match rx.try_recv() {
                        Ok(notif) => batch.push(notif),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            notification_closed = true;
                            break;
                        }
                    }
                }

                // A cold burst resolves its paths in parallel but is applied
                // strictly in arrival order, so a clear is never reordered.
                let admission_role = *role_rx.borrow();
                let admitted: Vec<bool> = batch
                    .iter()
                    .map(|n| publication_admitted(n, admission_role, &workspace_roots))
                    .collect();
                let uris: Vec<Publication<'_>> = batch
                    .iter()
                    .zip(&admitted)
                    .filter_map(|(n, admitted)| match n {
                        LspNotification::PublishDiagnostics(p) if *admitted => Some(Publication {
                            uri: &p.uri,
                            kind: if p.diagnostics.is_empty() {
                                PublicationKind::Clear
                            } else {
                                PublicationKind::Diagnostics
                            },
                        }),
                        _ => None,
                    })
                    .collect();
                // The lifecycle lane keeps being serviced while paths resolve;
                // the diagnostics lane waits, which keeps its order.
                let publications = {
                    let mut resolving =
                        std::pin::pin!(resolver.resolve_batch(&uris, &workspace_roots));
                    loop {
                        tokio::select! {
                            () = cancelled(&mut cancel_rx) => break 'pump,
                            done = &mut resolving => break done,
                            msg = lifecycle_rx.recv(), if !lifecycle_closed => match msg {
                                Some(notif) => {
                                    bridge::apply_lifecycle_notification(
                                        &mut *notification_cache.lock().await,
                                        &server_id,
                                        notif,
                                    );
                                }
                                None => lifecycle_closed = true,
                            },
                        }
                    }
                };
                drop(uris);
                let mut publications = publications.into_iter();

                for (notif, admitted) in batch.into_iter().zip(admitted) {
                    let published = if admitted {
                        publications.next().flatten()
                    } else {
                        None
                    };
                    // Re-read after the (possibly slow) resolve: a demotion in
                    // the meantime must stop caching and fan-out (#174 s8).
                    let role = *role_rx.borrow();
                    apply_notification(&server_id, notif, published, role, &notification_cache, &subs).await;
                }
            }
            msg = lifecycle_rx.recv(), if !lifecycle_closed => {
                let Some(notif) = msg else {
                    lifecycle_closed = true;
                    continue;
                };
                warn_if_pin_ignored(pinned_tsserver.as_deref(), &notif, server_id.as_str());
                bridge::apply_lifecycle_notification(
                    &mut *notification_cache.lock().await,
                    &server_id,
                    notif,
                );
            }
        }
    }
}

/// Notifications drained from the diagnostics lane per resolve round.
const PUMP_BATCH: usize = 64;

/// Completes when cancellation is requested or its sender is dropped.
async fn cancelled(cancel_rx: &mut tokio::sync::watch::Receiver<bool>) {
    drop(cancel_rx.wait_for(|cancelled| *cancelled).await);
}

/// Whether `notif` is a publication this pump should canonicalize and cache.
fn publication_admitted(
    notif: &LspNotification,
    role: DiagnosticsRole,
    workspace_roots: &WorkspaceRoots,
) -> bool {
    match notif {
        LspNotification::PublishDiagnostics(p) => {
            role == DiagnosticsRole::Authoritative && workspace_roots.admits_uri(&p.uri)
        }
        _ => false,
    }
}

/// Applies one drained notification; `published` is the resolution of a
/// publication that passed [`publication_admitted`], `None` when it resolved
/// outside the workspace or could not be resolved.
async fn apply_notification(
    server_id: &ServerId,
    notif: LspNotification,
    published: Option<PublishedDiagnosticsUri>,
    role: DiagnosticsRole,
    notification_cache: &Mutex<NotificationCache>,
    subs: &SubscriptionRegistry,
) {
    match notif {
        LspNotification::PublishDiagnostics(p) => {
            // Only the server the router resolves `Diagnostics` to for this
            // notification's language caches (and notifies subscribers of) it
            // -- see #174 §8.
            if role == DiagnosticsRole::Secondary {
                return;
            }
            let Some(published) = published else {
                debug!(
                    "dropping diagnostics for URI outside the workspace or unresolvable: {}",
                    p.uri.as_ref()
                );
                return;
            };
            {
                let mut cache = notification_cache.lock().await;
                cache.store_published_diagnostics(server_id, &published, p.version, p.diagnostics);
            }

            let sessions = subs.live_sessions();

            // Fast path: skip URI construction when nothing is subscribed.
            let mut any_subscribed = false;
            for session in &sessions {
                if !session.is_empty().await {
                    any_subscribed = true;
                    break;
                }
            }
            if !any_subscribed {
                return;
            }

            let Some(mcp_uri) = DiagnosticsResourceUri::for_published(&published) else {
                return;
            };
            for session in &sessions {
                session.publish_if_subscribed(&mcp_uri).await;
            }
        }
        LspNotification::LogMessage(m) => {
            notification_cache
                .lock()
                .await
                .store_log(m.kind.into(), m.message);
        }
        LspNotification::ShowMessage(m) => {
            notification_cache
                .lock()
                .await
                .store_message(m.kind.into(), m.message);
        }
        // Never classified onto this lane -- see `LspClient::message_loop_inner`'s routing.
        LspNotification::Progress(_) | LspNotification::Other { .. } => {}
    }
}

/// Re-starts diagnostics pumps for manually restarted servers over the same
/// shared state and shutdown watch the initial pumps use.
#[derive(Clone)]
pub(crate) struct PumpWiring {
    shared: PumpShared,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
}

impl std::fmt::Debug for PumpWiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PumpWiring").finish_non_exhaustive()
    }
}

impl bridge::NotificationWiring for PumpWiring {
    fn spawn_pump(
        &self,
        id: ServerId,
        receivers: bridge::NotificationReceivers,
        role: DiagnosticsRole,
    ) -> tokio::task::AbortHandle {
        let shared = self.shared.clone();
        let cancel_rx = self.cancel_rx.clone();
        let (_role_tx, role_rx) = tokio::sync::watch::channel(role);
        let cache = Arc::clone(&shared.notification_cache);
        tokio::spawn(async move {
            let pump = diagnostics_pump(
                id.clone(),
                receivers.notifications,
                receivers.lifecycle,
                cancel_rx,
                role_rx,
                receivers.pinned_tsserver,
                shared,
            );
            if let Err(payload) = AssertUnwindSafe(pump).catch_unwind().await {
                error!(
                    "Diagnostics pump for LSP server '{id}' panicked: {}",
                    panic_message(payload.as_ref())
                );
                let mut cache = cache.lock().await;
                cache.mark_push_degraded(&id);
                cache.reset_indexing_state(&id);
            }
        })
        .abort_handle()
    }

    fn publish_invalidated<'a>(
        &'a self,
        cleared: &'a [bridge::DiagnosticsKey],
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            if cleared.is_empty() {
                return;
            }
            let cleared: HashSet<&bridge::DiagnosticsKey> = cleared.iter().collect();
            self.shared
                .subs
                .publish_matching(|uri| {
                    bridge::diagnostics_cache_key(uri).is_some_and(|key| cleared.contains(&key))
                })
                .await;
        })
    }
}

/// Start the MCPLS server with the given configuration over stdio.
///
/// This is the backward-compatible entry point. It is equivalent to calling
/// `serve_with(config, Transport::Stdio)`.
///
/// # Errors
///
/// Returns an error if:
/// - MCP server setup fails
/// - Configuration is invalid
///
/// # Graceful Degradation
///
/// - **All servers succeed**: Service runs normally
/// - **Partial success**: Logs warnings for failures, continues with available servers
/// - **All servers fail**: Keeps serving; tools for the affected languages return `Error::ServerFailedToStart` with the spawn failure
///
/// # Shutdown
///
/// See [`serve_with`]'s "Shutdown" section — this function uses
/// [`Transport::Stdio`], so the same `std::process::exit` requirement
/// applies to callers.
pub async fn serve(config: ServerConfig) -> Result<(), Error> {
    serve_with(config, Transport::Stdio).await
}

/// Start the MCPLS server with an explicit transport.
///
/// Performs all shared setup (workspace discovery, LSP spawning, translator
/// initialization, diagnostic pump tasks) and then delegates to the
/// appropriate transport runner.
///
/// # Errors
///
/// Returns an error if:
/// - The MCP server or transport fails to start
/// - Configuration is invalid, including two applicable `[[lsp_servers]]`
///   entries whose per-tool routing is ambiguous in this workspace (shared
///   routing identity, two catch-alls, or the same tool claimed by both) --
///   see `config::ToolRouter::from_configs`
///
/// # DNS rebinding protection (HTTP transport)
///
/// When using `Transport::Http`, the underlying rmcp service validates the
/// inbound `Host` header against an allowlist that defaults to loopback
/// addresses only (`localhost`, `127.0.0.1`, `::1`). Requests with any other
/// `Host` value are rejected with `421 Misdirected Request`.
///
/// If you bind to a non-loopback address (e.g. `0.0.0.0:3000`) and expose the
/// service through a reverse proxy, the proxy must forward `Host: localhost`
/// (or another loopback alias) to the mcpls process. Direct non-loopback
/// access is intentionally blocked to prevent DNS-rebinding attacks.
///
/// # Shutdown
///
/// [`Transport::Stdio`] is backed by `tokio::io::stdin()`, which internally
/// parks an uncancellable blocking-pool thread in a raw `read()` syscall
/// that only returns on more input or EOF. If your `main` uses
/// `#[tokio::main]` and simply returns after awaiting this function, the
/// macro-generated runtime-shutdown wrapper blocks waiting for that thread
/// -- hanging indefinitely on `SIGTERM`/`SIGINT` as long as the MCP
/// client's stdin write end is still open, since that never triggers EOF.
/// Call `std::process::exit` right after this function resolves instead of
/// returning normally from `main`, as in the example below (see mcpls's own
/// `mcpls-cli` binary; tracked as #308). This does not apply to
/// `Transport::Http`, which never touches `tokio::io::stdin()`.
///
/// # Examples
///
/// ```rust,ignore
/// use mcpls_core::{serve_with, Transport, ServerConfig};
///
/// #[tokio::main]
/// async fn main() {
///     let config = ServerConfig::load().expect("failed to load config");
///     let exit_code = match serve_with(config, Transport::Stdio).await {
///         Ok(()) => 0,
///         Err(_) => 1,
///     };
///     // See "Shutdown" above: process::exit avoids a runtime-shutdown hang.
///     std::process::exit(exit_code);
/// }
/// ```
#[allow(clippy::too_many_lines)]
pub async fn serve_with(config: ServerConfig, transport: Transport) -> Result<(), Error> {
    info!("Starting MCPLS server...");

    // Registered before any other startup work -- including
    // `spawn_lsp_servers_background` below, which spawns LSP child processes
    // concurrently on another worker thread -- so a `SIGTERM`/`SIGINT`
    // arriving during config validation, workspace-root heuristics, or LSP
    // spawning is caught rather than hitting the OS's default disposition
    // (immediate termination, skipping the `shutdown()` cleanup below
    // entirely; any LSP child mid-spawn still dies with the lifeline/job
    // binding, see #270 and #526). See
    // `ShutdownSignal`'s docs for why this must be a single instance carried
    // through by value rather than re-registered later.
    let shutdown_signal = ShutdownSignal::new();

    // `ServerConfig::load`/`load_from` already validate the TOML-loading
    // path; this covers the other one -- a caller building `ServerConfig`
    // programmatically (e.g. a library embedder) previously hit no
    // diagnosable error here, only silent clamping at accessor level (e.g.
    // `LspClient::request_timeout`). `serve` delegates to this function, so
    // one call site here covers both public entry points (`serve` and
    // `serve_with`); note this does mean a config loaded via the CLI's
    // `load_from` -> `serve` path is validated twice (harmless -- `validate`
    // is a pure check with no side effects beyond a `tracing::warn!` for a
    // non-fatal duplicate-name case, which will simply log twice).
    //
    // Considered wrapping this in a `Validated<ServerConfig>` marker type to
    // make "already validated" a compile-time guarantee instead of a runtime
    // check here; rejected as unnecessary ceremony for a pre-1.0 API (#282).
    config.validate()?;

    let workspace_roots = WorkspaceRoots::from_configured(&config.workspace.roots)?;
    let extension_map = config.build_effective_extension_map();
    let max_depth = Some(config.workspace.heuristics_max_depth);

    let startup_redactions = Arc::new(redaction::Redactions::for_servers(
        &config.lsp_servers,
        lsp::current_environment(),
    ));
    let applicable_configs: Vec<ServerInitConfig> = config
        .lsp_servers
        .iter()
        .filter_map(|lsp_config| {
            let should_spawn = workspace_roots
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
                workspace_roots: workspace_roots.canonical().to_vec(),
                initialization_options: lsp::tsserver_pin::pinned_initialization_options(
                    lsp_config,
                    &workspace_roots,
                    |key| std::env::var_os(key),
                ),
                position_encodings: config.workspace.position_encodings.clone(),
                redactions: Arc::clone(&startup_redactions),
            })
        })
        .collect();

    info!(
        "Attempting to spawn {} applicable LSP server(s)...",
        applicable_configs.len()
    );

    // Built over the applicable (post-heuristics) configs only: this is where
    // #174's workspace-scoped routing rules (duplicate ServerId, conflicting
    // `handles` claims) are enforced -- a startup error naming the
    // conflicting `[[lsp_servers]]` entries, not a silent drop.
    let router = ToolRouter::from_configs(applicable_configs.iter().map(|c| &c.server_config))?;

    // Built here (rather than alongside `subscription_registry` below) so
    // it can be handed to the translator, which uses it to invalidate a
    // respawned server's stale cached diagnostics -- see
    // `Translator::with_notification_cache`. Independent of `translator`
    // itself, which holds no outer lock: the pump only ever locks this
    // cache, so it never contends with a request handler running an
    // in-flight LSP round-trip.
    let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));

    let mut translator = Translator::new()
        .with_startup_redactions(Arc::clone(&startup_redactions))
        .with_resource_limits(config.workspace.resource_limits())
        .with_extensions(extension_map)
        .with_router(router)
        .with_notification_cache(Arc::clone(&notification_cache))
        .with_indexing_ready_timeout(config.workspace.indexing_ready_timeout_seconds);
    // moved, not cloned -- `config`'s last use is above
    let (project_config_status, mcp) = (config.project_config_status, config.mcp);
    let max_concurrent_server_starts = config.workspace.max_concurrent_server_starts;
    translator.set_workspace_roots(workspace_roots.clone());

    // Mark applicable servers as "expected" so a tool call that arrives while
    // its server is still initializing gets a clear "still initializing" error
    // (instead of "no server configured"), telling the caller to wait and retry.
    let expected_servers: HashSet<ServerId> = applicable_configs
        .iter()
        .map(|c| c.server_config.id())
        .collect();
    translator.set_expected_servers(expected_servers);

    // Shared state, built BEFORE LSP initialization so the MCP server can answer
    // `initialize` immediately. LSP servers (which can take minutes to initialize
    // on a large solution, e.g. a 130-project Unity .sln via OmniSharp) are spawned
    // in a background task and registered into this shared translator once ready.
    // Blocking the MCP handshake on LSP init makes slow servers exceed the client's
    // initialize-request timeout (Claude Code: ~60s) -> "Request timed out".
    // `workspace_roots` is fixed for the server's lifetime and cheap to clone,
    // so cache-only handlers (e.g. `get_cached_diagnostics`, `read_resource`)
    // validate a path from their own copy without locking `translator` below.

    let translator = Arc::new(translator);
    // Shared across every session (one per HTTP session, or the sole stdio
    // session): each joins it on its first subscribe -- see
    // `SubscriptionRegistry`.
    let subscription_registry = SubscriptionRegistry::new();

    // Cancellation for pump tasks: send `true` to request shutdown.
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    let lsp_init_handle = if applicable_configs.is_empty() {
        warn!("No applicable LSP servers configured — starting in protocol-only mode");
        None
    } else {
        info!(
            "Spawning {} LSP server(s) in the background...",
            applicable_configs.len()
        );
        Some(spawn_lsp_servers_background(
            applicable_configs,
            Arc::clone(&translator),
            Arc::clone(&notification_cache),
            subscription_registry.clone(),
            cancel_rx.clone(),
            workspace_roots.clone(),
            max_concurrent_server_starts,
        ))
    };

    info!("Starting MCP server with rmcp...");
    let mcp_server = mcp::McplsServer::new(
        Arc::clone(&translator),
        Arc::clone(&notification_cache),
        workspace_roots,
        subscription_registry,
        project_config_status,
        mcp,
    );
    info!("MCPLS server initialized successfully");

    let result = match transport {
        Transport::Stdio => {
            info!("Listening for MCP requests on stdio...");
            run_stdio(mcp_server, shutdown_signal).await
        }
        #[cfg(feature = "transport-http")]
        Transport::Http(cfg) => run_http(mcp_server, cfg, shutdown_signal).await,
    };

    shutdown(&cancel_tx, &translator, lsp_init_handle).await;

    info!("MCPLS server shutting down");
    result
}

/// Bounds how long [`shutdown`] waits for the background LSP init task
/// (see [`spawn_lsp_servers_background`]) to finish after cancellation is
/// signaled. Deliberately shorter than [`Translator`]'s own per-server
/// shutdown timeout: by the time `shutdown_servers` returns, every
/// registered server's notification channel has closed, so the init task's
/// diagnostics pumps should already be draining. This bound only matters
/// for the rarer case where the init task is still mid-`initialize` (never
/// registered anything for `shutdown_servers` to act on).
const LSP_INIT_TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

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
/// [`spawn_lsp_servers_background`] is still starting servers,
/// relying entirely on `kill_on_drop` to terminate them) are only actually
/// dropped once the runtime polls the task to completion. `mcpls-cli`'s
/// `main` calls `std::process::exit` right after `serve_with` returns (see
/// #308), which skips the executor's own task teardown that used to do this
/// polling implicitly — so this function awaits the aborted handle again,
/// bounded, to drive that drop here instead of leaving it to chance.
/// Otherwise a `SIGTERM` arriving mid-startup could orphan those LSP
/// child processes, the exact failure mode #270 was filed to prevent.
async fn await_lsp_init_handle(mut handle: JoinHandle<()>, timeout: Duration) {
    match tokio::time::timeout(timeout, &mut handle).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => error!("Background LSP initialization task failed: {err}"),
        Err(_) => {
            warn!("Timed out waiting for background LSP initialization task to stop");
            handle.abort();
            let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
        }
    }
}

/// Aborts the wrapped [`JoinHandle`] when dropped, including on an unwind out
/// of the enclosing scope — unlike a bare `.abort()` call placed at the end
/// of a function body, which is skipped if that scope is left early (a
/// panic, or a future `?` added above it).
///
/// `pub(crate)` (and its field along with it): also used by
/// [`crate::lsp::client`]'s message loop to abort its background reader task
/// (#451) -- see that module for the other caller.
pub(crate) struct AbortOnDrop<'a, T>(pub(crate) &'a JoinHandle<T>);

impl<T> Drop for AbortOnDrop<'_, T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Whether a shutdown signal caught during [`shutdown`]'s cleanup window
/// should force an immediate `std::process::exit`, given how many such
/// signals (including this one) have been received so far.
///
/// Extracted as a pure function, rather than inlined into the loop that
/// calls it, so the threshold is unit-testable without actually invoking
/// `std::process::exit` — which would tear down the test process itself
/// under `cargo nextest` before any assertion could run.
const fn should_escalate(repeat_signals: u32) -> bool {
    repeat_signals >= 1
}

/// Post-transport shutdown sequence, run once the transport future
/// (`run_stdio`/`run_http`) returns — whether that's because of a
/// `SIGTERM`/`SIGINT`, stdio EOF, or (for HTTP) its own graceful shutdown.
///
/// Signals background pump tasks to exit, then gracefully shuts down every
/// LSP server registered on `translator` (see
/// [`Translator::shutdown_servers`] for what "gracefully" bounds and falls
/// back to). Finally, if the background LSP init task (see
/// [`spawn_lsp_servers_background`]) is still running, awaits it via
/// [`await_lsp_init_handle`], giving its diagnostics pump tasks a chance to
/// finish draining before `serve_with` returns. Extracted from
/// [`serve_with`] so this sequence is exercised directly in tests without
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
/// `await_lsp_init_handle` (bounded by [`lsp::SHUTDOWN_TIMEOUT`] and
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
/// cleanup ([`should_escalate`]) is logged and forces an immediate
/// `std::process::exit(1)`, since the graceful default (waiting out
/// `shutdown_servers`'s bounded timeouts) already had its chance before the
/// operator intervened. This is deliberately not lenient — because a signal
/// in the re-registration gap above is silently dropped rather than
/// counted, requiring a second repeat before acting would let an unlucky
/// operator's second press go unnoticed too. `exit(1)` skips unwinding, so
/// it forfeits `Drop` (`kill_on_drop`) and the graceful LSP `exit`, but any
/// still-running LSP child is killed by the lifeline/job binding (see
/// [`Translator::shutdown_servers`]'s "Limitations" section) — an explicit
/// trade the operator is asking for, not a case this fix silently regresses.
async fn shutdown(
    cancel_tx: &tokio::sync::watch::Sender<bool>,
    translator: &Translator,
    lsp_init_handle: Option<JoinHandle<()>>,
) {
    translator.begin_shutdown();
    let _ = cancel_tx.send(true);

    let mut cleanup_signal = ShutdownSignal::new();
    let force_exit_on_signal = tokio::spawn(async move {
        let mut repeat_signals = 0u32;
        loop {
            cleanup_signal.recv().await;
            repeat_signals = repeat_signals.saturating_add(1);
            if should_escalate(repeat_signals) {
                error!("shutdown signal received during cleanup, forcing immediate exit");
                std::process::exit(1);
            }
        }
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
/// Returns the task's `JoinHandle` so [`shutdown`] can await it. A panic in the
/// task is contained by [`run_init_supervised`] rather than leaving every
/// affected tool on `ServerInitializing` forever (#528).
fn spawn_lsp_servers_background(
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
async fn publish_startup_failures(translator: &Translator, registry: &SubscriptionRegistry) {
    if translator.startup_failures().is_empty() {
        return;
    }
    registry
        .publish_matching(|uri| translator.diagnostics_route_for_uri(uri).is_failed())
        .await;
}

/// Best-effort text of a panic payload, for logging.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

/// Drives `body` (the LSP init sequence) to completion on the current task,
/// turning a panic in it into a settled translator state.
///
/// The body runs on the same task rather than an inner spawn so that aborting
/// the outer task (see [`await_lsp_init_handle`]) still drops every
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
    translator.install_wiring(Arc::new(PumpWiring {
        shared: pump_shared.clone(),
        cancel_rx: cancel_rx.clone(),
    }));
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
    let mut cache = notification_cache.lock().await;
    cache.mark_push_degraded(server_id);
    cache.reset_indexing_state(server_id);
}

/// Shared by any `#[cfg(test)]` module in this crate that needs to mutate
/// the process-wide working directory (`std::env::set_current_dir`). Such
/// tests must not run concurrently with each other or with any other test
/// that relies on cwd -- nextest runs each test in its own process, so this
/// only matters under a plain `cargo test`, but a single shared lock is what
/// makes that true across every module's tests in this crate, not just
/// within one module (#348).
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard, PoisonError};

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that serializes CWD-mutating tests behind [`CWD_LOCK`] and
    /// switches into `dir` for the guard's lifetime, restoring the original
    /// working directory on drop — including on an early return or panic.
    ///
    /// `pub`, not `pub(crate)`: this module is itself private (unexported),
    /// so `pub(crate)` on its items would be redundant -- see
    /// `clippy::redundant_pub_crate`. Still only reachable crate-internally
    /// via `crate::test_support::CwdGuard`, since the module isn't `pub`.
    pub struct CwdGuard {
        _lock: MutexGuard<'static, ()>,
        original_dir: PathBuf,
    }

    impl CwdGuard {
        pub fn enter(dir: &Path) -> Self {
            let lock = CWD_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            let original_dir = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            Self {
                _lock: lock,
                original_dir,
            }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let restored = std::env::set_current_dir(&self.original_dir);
            // A failure here during an already-unwinding panic must not
            // panic again (double panic aborts the process, losing the
            // original failure's message). On the normal path, though,
            // silently swallowing this would leave the process cwd wrong
            // for every subsequent test with no diagnostic — panic loudly
            // instead, since that's exactly the failure mode this guard
            // exists to prevent.
            if !std::thread::panicking() {
                #[allow(clippy::expect_used)]
                restored.expect("CwdGuard failed to restore original working directory");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::CwdGuard;

        #[test]
        fn test_cwd_guard_restores_cwd_on_panic() {
            let original_dir = std::env::current_dir().unwrap();
            let tmp_dir = tempfile::TempDir::new().unwrap();

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = CwdGuard::enter(tmp_dir.path());
                panic!("boom");
            }));

            assert!(result.is_err());
            assert_eq!(std::env::current_dir().unwrap(), original_dir);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::assert_matches;
    use std::path::PathBuf;

    use bridge::{DEFAULT_MAX_DOCUMENTS, DEFAULT_MAX_FILE_SIZE};

    use super::*;

    // Tests for graceful degradation behavior
    mod graceful_degradation_tests {
        use super::*;
        use crate::config::{IndexingReadyTimeoutSecs, PositionEncodings, TimeoutSecs};
        use crate::error::{ServerSpawnFailure, StartupFailure};

        #[test]
        fn test_all_servers_failed_to_init_error() {
            let failures = vec![
                ServerSpawnFailure {
                    server_id: ServerId::from("rust"),
                    language_id: LanguageId::from_static("rust"),
                    command: "rust-analyzer".to_string(),
                    reason: StartupFailure::InitTaskPanicked,
                },
                ServerSpawnFailure {
                    server_id: ServerId::from("python"),
                    language_id: LanguageId::from_static("python"),
                    command: "pyright".to_string(),
                    reason: StartupFailure::InitTaskPanicked,
                },
            ];

            let err = Error::AllServersFailedToInit { failures };

            assert!(err.to_string().contains("all LSP servers failed"));

            // Verify failures are preserved
            if let Error::AllServersFailedToInit { failures: f } = err {
                assert_eq!(f.len(), 2);
                assert_eq!(f[0].language_id, "rust");
                assert_eq!(f[1].language_id, "python");
            } else {
                panic!("Expected AllServersFailedToInit error");
            }
        }

        #[test]
        fn test_server_spawn_failure_display() {
            let failure = ServerSpawnFailure {
                server_id: ServerId::from("typescript"),
                language_id: LanguageId::from_static("typescript"),
                command: "tsserver".to_string(),
                reason: StartupFailure::InitTaskPanicked,
            };

            let display = failure.to_string();
            assert!(display.contains("typescript"));
            assert!(display.contains("tsserver"));
            assert!(display.contains("panicked"));
        }

        #[tokio::test]
        async fn test_serve_degrades_when_all_servers_fail_to_spawn() {
            use crate::config::{LspServerConfig, ServerStartConcurrency, WorkspaceConfig};

            // A configured server whose command cannot spawn used to make serve()
            // fail synchronously with AllServersFailedToInit.
            // LSP initialization now runs in a background task so the MCP
            // `initialize` handshake is never blocked, which means the spawn
            // failure is handled in the background instead: serve() starts the MCP
            // server in degraded mode (mirroring `test_serve_starts_with_empty_config`)
            // rather than failing fast. Any error it surfaces must therefore be a
            // transport/MCP error from the closed test connection, NOT a fail-fast
            // server-availability error.
            let config = ServerConfig {
                mcp: crate::config::McpConfig::default(),
                workspace: WorkspaceConfig {
                    roots: vec![PathBuf::from("/tmp/test-workspace")],
                    position_encodings: PositionEncodings::DEFAULT,
                    language_extensions: vec![],
                    heuristics_max_depth: 10,
                    max_documents: DEFAULT_MAX_DOCUMENTS,
                    max_file_size: DEFAULT_MAX_FILE_SIZE,
                    indexing_ready_timeout_seconds: IndexingReadyTimeoutSecs::DEFAULT,
                    max_concurrent_server_starts: ServerStartConcurrency::DEFAULT,
                },
                lsp_servers: vec![LspServerConfig {
                    language_id: LanguageId::from_static("rust"),
                    command: "nonexistent-command-that-will-fail-12345".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec!["**/*.rs".to_string()],
                    initialization_options: None,
                    settings: None,
                    timeout_seconds: TimeoutSecs::new(10).unwrap(),
                    request_timeout_seconds: TimeoutSecs::new(10).unwrap(),
                    heuristics: None,
                    name: None,
                    handles: None,
                    indexing: crate::bridge::IndexingPolicy::Auto,
                }],
                project_config_status: ProjectConfigStatus::NotIgnored,
            };

            // serve() proceeds to run the MCP server and blocks on the stdio
            // transport until EOF; bound it so the test can't hang if stdin stays
            // open (e.g. under multi-threaded `cargo test`, where several serve()
            // tests share the process stdin).
            let outcome =
                tokio::time::timeout(std::time::Duration::from_secs(2), serve(config)).await;

            match outcome {
                // Still serving after the deadline => it did not fail fast. Good.
                Err(_elapsed) => {}
                // Transport closed cleanly. Also fine.
                Ok(Ok(())) => {}
                // It returned an error: it must not be a fail-fast availability error.
                Ok(Err(err)) => assert!(
                    !matches!(err, Error::AllServersFailedToInit { .. }),
                    "serve() must not fail fast now that LSP init is backgrounded; got: {err:?}"
                ),
            }
        }

        #[tokio::test]
        async fn test_serve_starts_with_empty_config() {
            use crate::config::WorkspaceConfig;

            // Server starts in protocol-only mode when no LSP servers are configured.
            // serve() blocks until the MCP transport closes, so it will error with a
            // connection/transport error — not AllServersFailedToInit.
            let config = ServerConfig {
                mcp: crate::config::McpConfig::default(),
                workspace: WorkspaceConfig {
                    roots: vec![PathBuf::from("/tmp/test-workspace")],
                    position_encodings: PositionEncodings::DEFAULT,
                    language_extensions: vec![],
                    heuristics_max_depth: 10,
                    max_documents: DEFAULT_MAX_DOCUMENTS,
                    max_file_size: DEFAULT_MAX_FILE_SIZE,
                    indexing_ready_timeout_seconds: IndexingReadyTimeoutSecs::DEFAULT,
                    max_concurrent_server_starts: ServerStartConcurrency::DEFAULT,
                },
                lsp_servers: vec![],
                project_config_status: ProjectConfigStatus::NotIgnored,
            };

            let result = serve(config).await;

            // serve() may succeed or fail with a transport error, but must NOT
            // return AllServersFailedToInit when the config simply has no servers.
            if let Err(ref err) = result {
                assert!(
                    !matches!(err, Error::AllServersFailedToInit { .. }),
                    "serve() must not return AllServersFailedToInit for empty lsp_servers config"
                );
            }
        }

        /// #348 case 1 (tester-flagged coverage gap): proves `serve_with`
        /// itself skips `current_dir()` for an all-absolute
        /// `workspace.roots`, not just that `WorkspaceRoots::from_configured_with`
        /// tolerates an unread cwd when called directly (see
        /// `test_from_configured_ignores_cwd_when_all_absolute` in
        /// `bridge::workspace_roots`, which never exercises `serve_with`'s
        /// wiring). Mutates the process cwd (chdir into a directory,
        /// then remove it -- `current_dir()` reliably fails afterward on
        /// Unix), so it uses the crate-shared `test_support::CwdGuard` --
        /// the same lock/restore-on-drop `config::tests` uses -- rather than
        /// a one-off guard, since both modules' tests mutate cwd and compile
        /// into one binary. Unix-only since removing a directory that is
        /// still a live process's cwd is a Windows-specific error case, not
        /// the same reproducible `current_dir()` failure.
        #[tokio::test]
        #[cfg(unix)]
        async fn test_serve_with_all_absolute_roots_skips_current_dir() {
            use crate::config::WorkspaceConfig;
            use crate::test_support::CwdGuard;

            // Kept alive for the whole test so the configured workspace root
            // stays a valid, existing absolute directory distinct from the
            // cwd this test is about to remove.
            let workspace_root_dir = tempfile::TempDir::new().unwrap();
            let workspace_root = dunce::canonicalize(workspace_root_dir.path()).unwrap();

            let doomed_cwd = tempfile::TempDir::new().unwrap();
            let _guard = CwdGuard::enter(doomed_cwd.path());
            doomed_cwd.close().unwrap();

            let config = ServerConfig {
                mcp: crate::config::McpConfig::default(),
                workspace: WorkspaceConfig {
                    roots: vec![workspace_root],
                    position_encodings: PositionEncodings::DEFAULT,
                    language_extensions: vec![],
                    heuristics_max_depth: 10,
                    max_documents: DEFAULT_MAX_DOCUMENTS,
                    max_file_size: DEFAULT_MAX_FILE_SIZE,
                    indexing_ready_timeout_seconds: IndexingReadyTimeoutSecs::DEFAULT,
                    max_concurrent_server_starts: ServerStartConcurrency::DEFAULT,
                },
                lsp_servers: vec![],
                project_config_status: ProjectConfigStatus::NotIgnored,
            };

            // serve() with no LSP servers configured blocks on the stdio
            // transport, same as `test_serve_starts_with_empty_config`;
            // bound it so the test can't hang.
            let outcome =
                tokio::time::timeout(std::time::Duration::from_secs(2), serve(config)).await;

            match outcome {
                // Still serving after the deadline => it did not fail fast. Good.
                Err(_elapsed) => {}
                // Transport closed cleanly. Also fine.
                Ok(Ok(())) => {}
                // It returned an error: it must not be the `current_dir()`
                // failure this test set up (`ErrorKind::NotFound` from the
                // removed cwd). Narrowed to that specific `io::ErrorKind`
                // rather than any `Error::Io`, since the latter would also
                // match an unrelated IO error from the stdio transport
                // within the timeout window.
                Ok(Err(err)) => assert!(
                    !matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
                    "serve() must not need a working process cwd for an all-absolute \
                     workspace.roots; got: {err:?}"
                ),
            }
        }

        /// #282: a `ServerConfig` built programmatically (not via `load`/
        /// `load_from`, which already run `validate()`) previously skipped
        /// validation entirely, so `serve`/`serve_with` never rejected it —
        /// misconfiguration only surfaced later as silent accessor-level
        /// clamping. `serve` delegates straight to `serve_with`, so
        /// exercising it here also covers `serve_with`'s own `validate()`
        /// call. `validate()` runs before any LSP spawn or transport setup,
        /// so this returns immediately without needing a timeout guard.
        #[tokio::test]
        async fn test_serve_rejects_invalid_caller_supplied_config() {
            use crate::config::{LspServerConfig, ServerStartConcurrency, WorkspaceConfig};

            let config = ServerConfig {
                mcp: crate::config::McpConfig::default(),
                workspace: WorkspaceConfig {
                    roots: vec![PathBuf::from("/tmp/test-workspace")],
                    position_encodings: PositionEncodings::DEFAULT,
                    language_extensions: vec![],
                    heuristics_max_depth: 10,
                    max_documents: DEFAULT_MAX_DOCUMENTS,
                    max_file_size: DEFAULT_MAX_FILE_SIZE,
                    indexing_ready_timeout_seconds: IndexingReadyTimeoutSecs::DEFAULT,
                    max_concurrent_server_starts: ServerStartConcurrency::DEFAULT,
                },
                lsp_servers: vec![LspServerConfig {
                    language_id: LanguageId::from_static("rust"),
                    command: String::new(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec!["**/*.rs".to_string()],
                    initialization_options: None,
                    settings: None,
                    timeout_seconds: TimeoutSecs::new(10).unwrap(),
                    request_timeout_seconds: TimeoutSecs::new(10).unwrap(),
                    heuristics: None,
                    name: None,
                    handles: None,
                    indexing: crate::bridge::IndexingPolicy::Auto,
                }],
                project_config_status: ProjectConfigStatus::NotIgnored,
            };

            // `validate()` runs before any spawn/transport work and should
            // return immediately; bound it anyway so a regression that lets
            // an invalid config reach the stdio transport fails fast with a
            // clear timeout instead of hanging nextest for the default 120s
            // (mirroring the guard on `test_serve_degrades_when_all_servers_fail_to_spawn`).
            let outcome =
                tokio::time::timeout(std::time::Duration::from_secs(2), serve(config)).await;

            match outcome {
                Err(elapsed) => panic!(
                    "serve() must reject the invalid config immediately, not hang until \
                     timeout: {elapsed}"
                ),
                Ok(result) => assert_matches!(
                    result,
                    Err(Error::InvalidConfig(_)),
                    "serve() must reject a caller-supplied config with an empty `command` via \
                     Error::InvalidConfig, matching the load_from path; got: {result:?}"
                ),
            }
        }

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
                super::super::shutdown(&cancel_tx, &translator, None),
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
                super::super::shutdown(&cancel_tx, &translator, Some(handle)),
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

            super::super::await_lsp_init_handle(handle, std::time::Duration::from_millis(20)).await;

            // Give the just-aborted task's cancellation a moment to land.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(
                future_dropped.load(Ordering::SeqCst),
                "timed-out background init task's future must be dropped via abort(), \
                 not left running detached until its own sleep completes"
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

            super::super::await_lsp_init_handle(handle, std::time::Duration::from_secs(5)).await;

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

    // ------------------------------------------------------------------
    // diagnostics_pump unit tests
    // ------------------------------------------------------------------

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    mod settler_tests {
        use super::*;
        use crate::bridge::WorkspaceRoots;
        use crate::config::{LspServerConfig, ToolKind};
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
            let mut settler =
                settler_for(&translator, &cache, SubscriptionRegistry::new(), &[&config]);
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
            let mut settler =
                settler_for(&translator, &cache, SubscriptionRegistry::new(), &[&config]);
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

    #[cfg(unix)]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    mod startup_tests {
        use std::path::Path;

        use super::*;
        use crate::bridge::{RouteSupport, WorkspaceRoots};
        use crate::config::ToolKind;
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
            let router =
                ToolRouter::from_configs(configs.iter().map(|c| &c.server_config)).unwrap();
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
                    let mut config =
                        named_sh_init_config(dir.path(), language, language, "exit 1\n");
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
            let startup =
                start_limited(vec![first, second], ServerStartConcurrency::new(1).unwrap());

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

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    mod init_supervision_tests {
        use super::*;
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

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    mod pump_tests {
        use lsp_types::{PublishDiagnosticsParams, Uri};
        use tokio::sync::{mpsc, watch};

        use super::*;
        use crate::bridge::IndexingState;

        fn make_cache() -> Arc<Mutex<NotificationCache>> {
            Arc::new(Mutex::new(NotificationCache::new()))
        }

        fn make_subs() -> SubscriptionRegistry {
            SubscriptionRegistry::new()
        }

        /// A single real workspace root shared by the pump-mechanics tests
        /// below, cfg-gated because `Url::to_file_path` requires a drive
        /// letter on Windows -- mirrors
        /// `test_pump_drops_diagnostics_outside_workspace_roots`.
        #[cfg(windows)]
        fn test_workspace_roots() -> WorkspaceRoots {
            WorkspaceRoots::for_test(vec![PathBuf::from(r"C:\test")], vec![])
        }
        #[cfg(not(windows))]
        fn test_workspace_roots() -> WorkspaceRoots {
            WorkspaceRoots::for_test(vec![PathBuf::from("/test")], vec![])
        }

        /// A `file://` URI for `file` beneath [`test_workspace_roots`]'s root.
        #[cfg(windows)]
        fn test_uri(file: &str) -> Uri {
            Uri::from(format!("file:///C:/test/{file}").as_str())
        }
        #[cfg(not(windows))]
        fn test_uri(file: &str) -> Uri {
            Uri::from(format!("file:///test/{file}").as_str())
        }

        /// #566: a pinned server that reports another tsserver source is
        /// warned about; a report of the pinned source is not.
        #[tokio::test]
        async fn test_pump_warns_only_when_pinned_tsserver_is_ignored() {
            use tracing_subscriber::layer::SubscriberExt as _;

            use crate::test_lsp::{CapturedLogs, spawn_test_pump_with_tsserver_pin};

            let captured = CapturedLogs::default();
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::registry().with(captured.clone()),
            );
            let (lifecycle_tx, _cancel_tx) =
                spawn_test_pump_with_tsserver_pin(PathBuf::from("/pin/tsserver.js"));
            let report = |source: &str| LspNotification::Other {
                method: "$/typescriptVersion".into(),
                params: Some(serde_json::json!({"version": "5.0", "source": source})),
            };
            let ignored = || {
                captured
                    .messages()
                    .iter()
                    .filter(|m| m.contains("ignored the configured tsserver.path"))
                    .count()
            };

            lifecycle_tx.send(report("user-setting")).await.unwrap();
            lifecycle_tx.send(report("workspace")).await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while ignored() == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("mismatching source was never warned about");

            assert_eq!(ignored(), 1, "{:?}", captured.messages());
        }

        /// `PublishDiagnostics` is cached even when the peer is not yet connected.
        #[tokio::test]
        async fn test_pump_caches_before_peer_set() {
            let cache = make_cache();
            let subs = make_subs();
            let (tx, rx) = mpsc::channel(8);
            let (_lifecycle_tx, lifecycle_rx) = mpsc::channel(8);
            // Keep _cancel_tx alive: dropping it causes cancel_rx.changed() to return Err,
            // which makes the pump exit before processing any messages.
            let (_cancel_tx, cancel_rx) = watch::channel(false);

            let c = Arc::clone(&cache);
            tokio::spawn(diagnostics_pump(
                ServerId::from("rust"),
                rx,
                lifecycle_rx,
                cancel_rx,
                tokio::sync::watch::channel(crate::DiagnosticsRole::Authoritative).1,
                None,
                PumpShared {
                    notification_cache: c,
                    subs: subs.clone(),
                    workspace_roots: test_workspace_roots(),
                },
            ));

            let uri: Uri = test_uri("main.rs");
            tx.send(LspNotification::PublishDiagnostics(
                PublishDiagnosticsParams {
                    uri: uri.clone(),
                    diagnostics: vec![],
                    version: None,
                },
            ))
            .await
            .unwrap();
            drop(tx);

            // Poll until the pump processes the message or we time out.
            let cached = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    tokio::task::yield_now().await;
                    let found = {
                        let guard = cache.lock().await;
                        guard.diagnostics(&uri).is_some()
                    };
                    if found {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("pump did not cache diagnostics within 5 s");
            assert!(cached, "diagnostics should be cached before peer is set");
        }

        /// #234 (S1 hardening): diagnostics for URIs outside the configured
        /// workspace roots must be dropped rather than cached, closing the
        /// vector where a misbehaving server floods the FIFO-bounded cache
        /// with fabricated URIs to evict every legitimate entry.
        #[tokio::test]
        async fn test_pump_drops_diagnostics_outside_workspace_roots() {
            let cache = make_cache();
            let subs = make_subs();
            let (tx, rx) = mpsc::channel(8);
            let (_lifecycle_tx, lifecycle_rx) = mpsc::channel(8);
            let (_cancel_tx, cancel_rx) = watch::channel(false);

            // See `test_admits_uri_under_root_and_alias`
            // for why Windows needs a drive-letter path here.
            #[cfg(windows)]
            let (workspace_root, outside_uri_str, inside_uri_str) = (
                PathBuf::from(r"C:\workspace"),
                "file:///C:/etc/passwd",
                "file:///C:/workspace/src/main.rs",
            );
            #[cfg(not(windows))]
            let (workspace_root, outside_uri_str, inside_uri_str) = (
                PathBuf::from("/workspace"),
                "file:///etc/passwd",
                "file:///workspace/src/main.rs",
            );
            let workspace_roots = WorkspaceRoots::for_test(vec![workspace_root], vec![]);

            tokio::spawn(diagnostics_pump(
                ServerId::from("rust"),
                rx,
                lifecycle_rx,
                cancel_rx,
                tokio::sync::watch::channel(crate::DiagnosticsRole::Authoritative).1,
                None,
                PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs: subs.clone(),
                    workspace_roots,
                },
            ));

            let outside_uri: Uri = Uri::from(outside_uri_str);
            let inside_uri: Uri = Uri::from(inside_uri_str);

            tx.send(LspNotification::PublishDiagnostics(
                PublishDiagnosticsParams {
                    uri: outside_uri.clone(),
                    diagnostics: vec![],
                    version: None,
                },
            ))
            .await
            .unwrap();
            tx.send(LspNotification::PublishDiagnostics(
                PublishDiagnosticsParams {
                    uri: inside_uri.clone(),
                    diagnostics: vec![],
                    version: None,
                },
            ))
            .await
            .unwrap();
            drop(tx);

            // Poll until the (later-sent) in-workspace sentinel is cached --
            // proves the pump already processed the earlier out-of-workspace
            // message too, since the channel preserves send order.
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    {
                        let guard = cache.lock().await;
                        if guard.diagnostics(&inside_uri).is_some() {
                            return;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("pump did not cache in-workspace diagnostics within 5 s");

            let found_outside = cache.lock().await.diagnostics(&outside_uri).is_some();
            assert!(
                !found_outside,
                "diagnostics for a URI outside workspace roots must not be cached"
            );
        }

        /// Pump exits cleanly when the cancel watch sends `true`.
        #[tokio::test]
        async fn test_pump_exits_on_cancel() {
            let cache = make_cache();
            let subs = make_subs();
            let (_tx, rx) = mpsc::channel::<LspNotification>(8);
            let (_lifecycle_tx, lifecycle_rx) = mpsc::channel::<LspNotification>(8);
            let (cancel_tx, cancel_rx) = watch::channel(false);

            let handle = tokio::spawn(diagnostics_pump(
                ServerId::from("rust"),
                rx,
                lifecycle_rx,
                cancel_rx,
                tokio::sync::watch::channel(crate::DiagnosticsRole::Authoritative).1,
                None,
                PumpShared {
                    notification_cache: cache,
                    subs,
                    workspace_roots: test_workspace_roots(),
                },
            ));

            cancel_tx.send(true).unwrap();
            // Pump must finish within a short time after cancellation.
            tokio::time::timeout(std::time::Duration::from_millis(200), handle)
                .await
                .expect("pump did not exit within timeout")
                .unwrap();
        }

        /// Pump exits when the cancel sender is dropped (Err branch).
        #[tokio::test]
        async fn test_pump_exits_when_cancel_sender_dropped() {
            let cache = make_cache();
            let subs = make_subs();
            let (_tx, rx) = mpsc::channel::<LspNotification>(8);
            let (_lifecycle_tx, lifecycle_rx) = mpsc::channel::<LspNotification>(8);
            let (cancel_tx, cancel_rx) = watch::channel(false);

            let handle = tokio::spawn(diagnostics_pump(
                ServerId::from("rust"),
                rx,
                lifecycle_rx,
                cancel_rx,
                tokio::sync::watch::channel(crate::DiagnosticsRole::Authoritative).1,
                None,
                PumpShared {
                    notification_cache: cache,
                    subs,
                    workspace_roots: test_workspace_roots(),
                },
            ));

            drop(cancel_tx); // triggers Err in cancel_rx.changed()
            tokio::time::timeout(std::time::Duration::from_millis(200), handle)
                .await
                .expect("pump did not exit within timeout")
                .unwrap();
        }

        /// Regression test for #104: the pump must cache a notification promptly
        /// even while another task holds the translator lock for far longer than
        /// any acceptable pump latency. Before the `NotificationCache` split, the
        /// pump locked `Arc<Mutex<Translator>>` to cache diagnostics, so it would
        /// have stalled here until the holder released the lock.
        #[tokio::test]
        async fn test_pump_makes_progress_while_translator_lock_held() {
            let translator = Arc::new(Mutex::new(Translator::new()));
            let cache = make_cache();
            let subs = make_subs();
            let (tx, rx) = mpsc::channel(8);
            let (_lifecycle_tx, lifecycle_rx) = mpsc::channel(8);
            let (_cancel_tx, cancel_rx) = watch::channel(false);

            // Simulate a slow in-flight MCP request (e.g. `pull_diagnostics`)
            // holding the translator lock across an LSP round-trip.
            let lock_acquired = Arc::new(tokio::sync::Notify::new());
            let notify = Arc::clone(&lock_acquired);
            let holder = tokio::spawn(async move {
                let _guard = translator.lock().await;
                notify.notify_one();
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            });
            lock_acquired.notified().await;

            tokio::spawn(diagnostics_pump(
                ServerId::from("rust"),
                rx,
                lifecycle_rx,
                cancel_rx,
                tokio::sync::watch::channel(crate::DiagnosticsRole::Authoritative).1,
                None,
                PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs,
                    workspace_roots: test_workspace_roots(),
                },
            ));

            let uri: Uri = test_uri("locked.rs");
            tx.send(LspNotification::PublishDiagnostics(
                PublishDiagnosticsParams {
                    uri: uri.clone(),
                    diagnostics: vec![],
                    version: None,
                },
            ))
            .await
            .unwrap();
            drop(tx);

            // Well within the 2 s translator lock hold: a translator-locking
            // pump would still be blocked at this point.
            tokio::time::timeout(std::time::Duration::from_millis(500), async {
                loop {
                    {
                        let guard = cache.lock().await;
                        if guard.diagnostics(&uri).is_some() {
                            return;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("pump stalled behind translator lock");

            holder.await.unwrap();
        }

        /// The `Other` arm (custom/unrecognized notifications, e.g.
        /// rust-analyzer's `experimental/serverStatus`) must reach
        /// `NotificationCache::observe_indexing_signal` via the lifecycle
        /// lane -- this is the one place in production that notification
        /// actually gets from the LSP transport into the indexing-readiness
        /// gate; every other test for the gate pre-seeds the cache by hand
        /// and would not have caught a pump wiring regression.
        #[tokio::test]
        async fn test_pump_routes_other_notifications_to_indexing_signal() {
            let cache = make_cache();
            let subs = make_subs();
            let (_tx, rx) = mpsc::channel::<LspNotification>(8);
            let (lifecycle_tx, lifecycle_rx) = mpsc::channel(8);
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            let server_id = ServerId::from("rust");

            tokio::spawn(diagnostics_pump(
                server_id.clone(),
                rx,
                lifecycle_rx,
                cancel_rx,
                tokio::sync::watch::channel(crate::DiagnosticsRole::Authoritative).1,
                None,
                PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs,
                    workspace_roots: test_workspace_roots(),
                },
            ));

            lifecycle_tx
                .send(LspNotification::Other {
                    method: std::borrow::Cow::Borrowed("experimental/serverStatus"),
                    params: Some(serde_json::json!({"quiescent": false})),
                })
                .await
                .unwrap();
            drop(lifecycle_tx);

            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    {
                        let guard = cache.lock().await;
                        if guard.indexing_state(&server_id) == IndexingState::Loading {
                            return;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect(
                "pump did not route the Other{experimental/serverStatus} notification into \
                 NotificationCache::observe_indexing_signal within 5s",
            );
        }

        /// P3: a `$/progress` `report` frame must never reach the lifecycle
        /// lane at all (S3) -- classified and dropped by
        /// `LspClient::message_loop_inner` before enqueueing, not merely
        /// ignored once received. This test exercises the pump side: even
        /// if a `report` somehow arrived on the lifecycle lane, the pump
        /// itself only recognizes `begin`/`end` shapes via
        /// `NotificationCache::observe_progress`, so a `begin` sent
        /// afterward must still be the one that flips the state.
        #[tokio::test]
        async fn test_pump_routes_progress_begin_to_indexing_signal() {
            let cache = make_cache();
            let subs = make_subs();
            let (_tx, rx) = mpsc::channel::<LspNotification>(8);
            let (lifecycle_tx, lifecycle_rx) = mpsc::channel(8);
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            let server_id = ServerId::from("gopls");

            tokio::spawn(diagnostics_pump(
                server_id.clone(),
                rx,
                lifecycle_rx,
                cancel_rx,
                tokio::sync::watch::channel(crate::DiagnosticsRole::Authoritative).1,
                None,
                PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs,
                    workspace_roots: test_workspace_roots(),
                },
            ));

            lifecycle_tx
                .send(LspNotification::Progress(lsp_types::ProgressParams {
                    token: lsp_types::ProgressToken::String("indexing".to_string()),
                    value: serde_json::json!({"kind": "begin", "title": "Loading"}),
                }))
                .await
                .unwrap();
            drop(lifecycle_tx);

            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    {
                        let guard = cache.lock().await;
                        if guard.indexing_state(&server_id) == IndexingState::Loading {
                            return;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect(
                "pump did not route the Progress(begin) notification into \
                 NotificationCache::observe_progress within 5s",
            );
        }

        /// P3: saturating the diagnostics lane to capacity must not stall the
        /// lifecycle lane -- a `begin`/`end` frame arriving while the
        /// notification lane is backed up must still reach the readiness
        /// gate promptly.
        #[tokio::test]
        async fn test_lifecycle_lane_unaffected_by_saturated_notification_lane() {
            let cache = make_cache();
            let subs = make_subs();
            let (tx, rx) = mpsc::channel(2);
            let (lifecycle_tx, lifecycle_rx) = mpsc::channel(8);
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            let server_id = ServerId::from("gopls");

            // Fill the notification lane to capacity before the pump drains it, forcing a backlog.
            for _ in 0..2 {
                tx.send(LspNotification::PublishDiagnostics(
                    PublishDiagnosticsParams {
                        uri: test_uri("saturate.rs"),
                        diagnostics: vec![],
                        version: None,
                    },
                ))
                .await
                .unwrap();
            }

            tokio::spawn(diagnostics_pump(
                server_id.clone(),
                rx,
                lifecycle_rx,
                cancel_rx,
                tokio::sync::watch::channel(crate::DiagnosticsRole::Authoritative).1,
                None,
                PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs,
                    workspace_roots: test_workspace_roots(),
                },
            ));

            lifecycle_tx
                .send(LspNotification::Other {
                    method: std::borrow::Cow::Borrowed("experimental/serverStatus"),
                    params: Some(serde_json::json!({"quiescent": false})),
                })
                .await
                .unwrap();
            drop(tx);
            drop(lifecycle_tx);

            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    {
                        let guard = cache.lock().await;
                        if guard.indexing_state(&server_id) == IndexingState::Loading {
                            return;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("lifecycle lane must still be served while the notification lane is backed up");
        }

        use crate::test_lsp::{spawn_test_pump, spawn_test_pump_with_cache};

        fn test_mcp_uri(file: &str) -> DiagnosticsResourceUri {
            DiagnosticsResourceUri::for_published(&PublishedDiagnosticsUri::for_test(
                test_uri(file),
                test_uri(file),
            ))
            .unwrap()
        }

        fn publish(file: &str) -> LspNotification {
            LspNotification::PublishDiagnostics(PublishDiagnosticsParams {
                uri: test_uri(file),
                diagnostics: vec![],
                version: None,
            })
        }

        async fn recv_within(rx: &mut mpsc::Receiver<String>) -> String {
            tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("no resources/updated within 5 s")
                .expect("delivery channel closed")
        }

        /// #468 isolation: a session never receives updates for URIs it did not
        /// subscribe to. B's first message must be Y even though X was
        /// published first (ordering, not a timeout).
        #[tokio::test]
        async fn test_pump_delivers_only_to_subscribed_sessions() {
            use crate::mcp::{SessionHandle, Target};

            let subs = make_subs();
            let session_a = SessionHandle::new(subs.clone());
            let session_b = SessionHandle::new(subs.clone());
            let (tx_a, mut rx_a) = mpsc::channel(8);
            let (tx_b, mut rx_b) = mpsc::channel(8);
            let (x, y) = (test_mcp_uri("x.rs"), test_mcp_uri("y.rs"));
            for uri in [&x, &y] {
                session_a
                    .subscribe_for_test(uri, Target::Channel(tx_a.clone()))
                    .await
                    .unwrap();
            }
            session_b
                .subscribe_for_test(&y, Target::Channel(tx_b.clone()))
                .await
                .unwrap();

            let (tx, _cancel_tx) = spawn_test_pump(subs, test_workspace_roots());
            tx.send(publish("x.rs")).await.unwrap();
            assert_eq!(recv_within(&mut rx_a).await, x.as_str());
            // Gives a wrongly queued X time to reach B before Y exists.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_matches!(
                rx_b.try_recv(),
                Err(mpsc::error::TryRecvError::Empty),
                "B never subscribed to X"
            );

            tx.send(publish("y.rs")).await.unwrap();
            assert_eq!(recv_within(&mut rx_b).await, y.as_str());
            assert_eq!(recv_within(&mut rx_a).await, y.as_str());
        }

        /// A restart tells the subscribers of the diagnostics it cleared to
        /// re-read them, matching the cache key to the subscription URI even
        /// for a percent-encoded path, and leaves other subscribers alone.
        #[tokio::test]
        async fn test_publish_invalidated_notifies_only_subscribers_of_cleared_files() {
            use crate::mcp::{SessionHandle, Target};

            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let cleared_file = root.join("a b.rs");
            let other_file = root.join("other.rs");
            for file in [&cleared_file, &other_file] {
                std::fs::write(file, "fn main() {}").unwrap();
            }

            let cache = make_cache();
            let id = ServerId::from("rust");
            let roots = WorkspaceRoots::from_configured(&[root]).unwrap();
            let published =
                bridge::resolve_one(&bridge::path_to_uri(&cleared_file).unwrap(), &roots)
                    .await
                    .unwrap();
            let error = lsp_types::Diagnostic {
                message: "boom".to_owned().into(),
                ..Default::default()
            };
            let cleared = {
                let mut cache = cache.lock().await;
                cache.store_published_diagnostics(&id, &published, None, vec![error]);
                cache.clear_server_diagnostics(&id)
            };
            assert!(!cleared.is_empty());

            let subs = make_subs();
            let session = SessionHandle::new(subs.clone());
            let (tx_cleared, mut rx_cleared) = mpsc::channel(8);
            let (tx_other, mut rx_other) = mpsc::channel(8);
            let cleared_uri = bridge::resources::make_uri(&cleared_file).unwrap();
            let other_uri = bridge::resources::make_uri(&other_file).unwrap();
            session
                .subscribe_for_test(
                    &DiagnosticsResourceUri::for_test(&cleared_uri),
                    Target::Channel(tx_cleared.clone()),
                )
                .await
                .unwrap();
            session
                .subscribe_for_test(
                    &DiagnosticsResourceUri::for_test(&other_uri),
                    Target::Channel(tx_other.clone()),
                )
                .await
                .unwrap();
            let (_cancel, cancel_rx) = watch::channel(false);
            let wiring = PumpWiring {
                shared: PumpShared {
                    notification_cache: cache,
                    subs,
                    workspace_roots: roots,
                },
                cancel_rx,
            };

            bridge::NotificationWiring::publish_invalidated(&wiring, &cleared).await;

            assert_eq!(recv_within(&mut rx_cleared).await, cleared_uri);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_matches!(rx_other.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        }

        /// #532: diagnostics a server publishes through a symlinked spelling
        /// notify the subscriber of the canonical URI and read back under the
        /// canonical key; a link pointing outside the workspace is dropped.
        #[cfg(unix)]
        #[tokio::test]
        async fn test_pump_keys_symlinked_publish_by_canonical_path() {
            use crate::mcp::{SessionHandle, Target};

            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let file = root.join("main.rs");
            std::fs::write(&file, "fn main() {}").unwrap();
            let link = root.join("link.rs");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            let outside = tempfile::TempDir::new().unwrap();
            std::fs::write(outside.path().join("x.rs"), "").unwrap();
            let escape = root.join("escape.rs");
            std::os::unix::fs::symlink(outside.path().join("x.rs"), &escape).unwrap();

            let cache = make_cache();
            let subs = make_subs();
            let session = SessionHandle::new(subs.clone());
            let (tx_session, mut rx_session) = mpsc::channel(8);
            let canonical = bridge::resources::make_uri(&file).unwrap();
            session
                .subscribe_for_test(
                    &DiagnosticsResourceUri::for_test(&canonical),
                    Target::Channel(tx_session),
                )
                .await
                .unwrap();

            let (tx, _cancel_tx) = spawn_test_pump_with_cache(
                subs,
                WorkspaceRoots::from_configured(&[root]).unwrap(),
                Arc::clone(&cache),
            );
            let error = lsp_types::Diagnostic {
                message: "boom".to_owned().into(),
                ..Default::default()
            };
            for (path, diagnostics) in [(&escape, vec![error.clone()]), (&link, vec![error])] {
                tx.send(LspNotification::PublishDiagnostics(
                    PublishDiagnosticsParams {
                        uri: bridge::path_to_uri(path).unwrap(),
                        diagnostics,
                        version: None,
                    },
                ))
                .await
                .unwrap();
            }

            assert_eq!(recv_within(&mut rx_session).await, canonical);
            let canonical_lsp = bridge::path_to_uri(&file).unwrap();
            let (info, escaped) = {
                let guard = cache.lock().await;
                (
                    guard.diagnostic_sources(&canonical_lsp),
                    guard.has_diagnostics(&bridge::path_to_uri(&escape).unwrap()),
                )
            };
            assert_eq!(info.merge().unwrap().diagnostics.len(), 1);
            assert!(
                !escaped,
                "a symlink pointing outside the workspace must be dropped"
            );
        }

        /// A clear whose path hits a persistent transient filesystem error is
        /// still applied, and a later publication for the same URI wins.
        #[tokio::test]
        async fn test_pump_clear_survives_transient_error_and_stays_ordered() {
            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let failing: Arc<bridge::CanonicalizeFn> = Arc::new(|_: &std::path::Path| {
                Err(std::io::Error::from(std::io::ErrorKind::TimedOut))
            });
            let cache = make_cache();
            let (tx, rx) = mpsc::channel(8);
            let (_lifecycle_tx, lifecycle_rx) = mpsc::channel(8);
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            tokio::spawn(diagnostics_pump_with_resolver(
                ServerId::from("rust"),
                rx,
                lifecycle_rx,
                cancel_rx,
                tokio::sync::watch::channel(crate::DiagnosticsRole::Authoritative).1,
                None,
                PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs: make_subs(),
                    workspace_roots: WorkspaceRoots::from_configured(std::slice::from_ref(&root))
                        .unwrap(),
                },
                PublishedPathResolver::with_canonicalizer(failing),
            ));
            let uri = bridge::path_to_uri(&root.join("a.rs")).unwrap();
            tx.send(LspNotification::PublishDiagnostics(
                PublishDiagnosticsParams {
                    uri: uri.clone(),
                    diagnostics: vec![],
                    version: None,
                },
            ))
            .await
            .unwrap();

            tokio::time::timeout(Duration::from_secs(5), async {
                while !cache.lock().await.has_diagnostics(&uri) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the clear must be cached despite the transient error");
        }

        /// A lifecycle notification is applied while a diagnostics batch is
        /// still waiting on a slow filesystem.
        #[tokio::test]
        async fn test_pump_services_lifecycle_lane_while_a_batch_resolves() {
            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let (release, gate) = std::sync::mpsc::channel::<()>();
            let gate = std::sync::Mutex::new(gate);
            let slow: Arc<bridge::CanonicalizeFn> = Arc::new(move |p: &std::path::Path| {
                gate.lock().map(|rx| rx.recv()).ok();
                Ok(p.to_path_buf())
            });
            let cache = make_cache();
            let (tx, rx) = mpsc::channel(8);
            let (lifecycle_tx, lifecycle_rx) = mpsc::channel(8);
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            let server_id = ServerId::from("rust");
            tokio::spawn(diagnostics_pump_with_resolver(
                server_id.clone(),
                rx,
                lifecycle_rx,
                cancel_rx,
                tokio::sync::watch::channel(crate::DiagnosticsRole::Authoritative).1,
                None,
                PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs: make_subs(),
                    workspace_roots: WorkspaceRoots::from_configured(std::slice::from_ref(&root))
                        .unwrap(),
                },
                PublishedPathResolver::with_canonicalizer(slow),
            ));
            tx.send(LspNotification::PublishDiagnostics(
                PublishDiagnosticsParams {
                    uri: bridge::path_to_uri(&root.join("a.rs")).unwrap(),
                    diagnostics: vec![],
                    version: None,
                },
            ))
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;

            lifecycle_tx
                .send(LspNotification::Other {
                    method: std::borrow::Cow::Borrowed("experimental/serverStatus"),
                    params: Some(serde_json::json!({"quiescent": false})),
                })
                .await
                .unwrap();

            tokio::time::timeout(Duration::from_secs(5), async {
                while cache.lock().await.indexing_state(&server_id) != IndexingState::Loading {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("lifecycle lane must be serviced while the batch is still resolving");
            drop(release);
        }

        /// A demotion to `Secondary` while a batch is still resolving stops
        /// that batch from being cached.
        #[tokio::test]
        async fn test_pump_demotion_during_resolve_stops_caching() {
            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let (release, gate) = std::sync::mpsc::channel::<()>();
            let gate = std::sync::Mutex::new(gate);
            let slow: Arc<bridge::CanonicalizeFn> = Arc::new(move |p: &std::path::Path| {
                gate.lock().map(|rx| rx.recv()).ok();
                Ok(p.to_path_buf())
            });
            let cache = make_cache();
            let (tx, rx) = mpsc::channel(8);
            let (_lifecycle_tx, lifecycle_rx) = mpsc::channel(8);
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            let (role_tx, role_rx) = watch::channel(DiagnosticsRole::Authoritative);
            tokio::spawn(diagnostics_pump_with_resolver(
                ServerId::from("rust"),
                rx,
                lifecycle_rx,
                cancel_rx,
                role_rx,
                None,
                PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs: make_subs(),
                    workspace_roots: WorkspaceRoots::from_configured(std::slice::from_ref(&root))
                        .unwrap(),
                },
                PublishedPathResolver::with_canonicalizer(slow),
            ));
            let uri = bridge::path_to_uri(&root.join("a.rs")).unwrap();
            let error = lsp_types::Diagnostic {
                message: "boom".to_owned().into(),
                ..Default::default()
            };
            tx.send(LspNotification::PublishDiagnostics(
                PublishDiagnosticsParams {
                    uri: uri.clone(),
                    diagnostics: vec![error],
                    version: None,
                },
            ))
            .await
            .unwrap();
            tx.send(LspNotification::LogMessage(lsp_types::LogMessageParams {
                kind: lsp_types::MessageType::Log,
                message: "fence".to_owned(),
            }))
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;

            role_tx.send(DiagnosticsRole::Secondary).unwrap();
            drop(release);

            tokio::time::timeout(Duration::from_secs(5), async {
                while cache.lock().await.logs_count() == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the log after the publication must be applied");
            assert!(!cache.lock().await.has_diagnostics(&uri));
        }

        /// Cancellation must end the pump even while a canonicalization is
        /// stuck on a hung filesystem.
        #[tokio::test]
        async fn test_pump_cancel_completes_while_canonicalize_hangs() {
            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let (release, gate) = std::sync::mpsc::channel::<()>();
            let gate = std::sync::Mutex::new(gate);
            let hung: Arc<bridge::CanonicalizeFn> = Arc::new(move |p: &std::path::Path| {
                gate.lock().map(|rx| rx.recv()).ok();
                Ok(p.to_path_buf())
            });
            let (tx, rx) = mpsc::channel(8);
            let (_lifecycle_tx, lifecycle_rx) = mpsc::channel(8);
            let (cancel_tx, cancel_rx) = watch::channel(false);
            let pump = tokio::spawn(diagnostics_pump_with_resolver(
                ServerId::from("rust"),
                rx,
                lifecycle_rx,
                cancel_rx,
                tokio::sync::watch::channel(crate::DiagnosticsRole::Authoritative).1,
                None,
                PumpShared {
                    notification_cache: make_cache(),
                    subs: make_subs(),
                    workspace_roots: WorkspaceRoots::from_configured(std::slice::from_ref(&root))
                        .unwrap(),
                },
                PublishedPathResolver::with_canonicalizer(hung),
            ));
            tx.send(LspNotification::PublishDiagnostics(
                PublishDiagnosticsParams {
                    uri: bridge::path_to_uri(&root.join("a.rs")).unwrap(),
                    diagnostics: vec![],
                    version: None,
                },
            ))
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;

            cancel_tx.send(true).unwrap();

            tokio::time::timeout(Duration::from_secs(2), pump)
                .await
                .expect("pump must exit promptly on cancel")
                .unwrap();
            drop(release);
        }

        /// A cold burst resolved in parallel is still applied in arrival order:
        /// a clear published after an error for the same file wins, and no
        /// publication of the burst is lost.
        #[tokio::test]
        async fn test_pump_cold_burst_keeps_clear_after_error_and_loses_nothing() {
            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let cache = make_cache();
            let (tx, _cancel_tx) = spawn_test_pump_with_cache(
                make_subs(),
                WorkspaceRoots::from_configured(std::slice::from_ref(&root)).unwrap(),
                Arc::clone(&cache),
            );
            let error = lsp_types::Diagnostic {
                message: "boom".to_owned().into(),
                ..Default::default()
            };
            let publish = |name: String, diagnostics: Vec<lsp_types::Diagnostic>| {
                LspNotification::PublishDiagnostics(PublishDiagnosticsParams {
                    uri: bridge::path_to_uri(&root.join(name)).unwrap(),
                    diagnostics,
                    version: None,
                })
            };

            tx.send(publish("a.rs".to_owned(), vec![error.clone()]))
                .await
                .unwrap();
            for i in 0..150 {
                tx.send(publish(format!("f{i}.rs"), vec![error.clone()]))
                    .await
                    .unwrap();
            }
            tx.send(publish("a.rs".to_owned(), vec![])).await.unwrap();
            tx.send(publish("last.rs".to_owned(), vec![error.clone()]))
                .await
                .unwrap();

            let last = bridge::path_to_uri(&root.join("last.rs")).unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while !cache.lock().await.has_diagnostics(&last) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();

            let a = bridge::path_to_uri(&root.join("a.rs")).unwrap();
            let (missing, remaining) = {
                let guard = cache.lock().await;
                let missing: Vec<usize> = (0..150)
                    .filter(|i| {
                        let uri = bridge::path_to_uri(&root.join(format!("f{i}.rs"))).unwrap();
                        !guard.has_diagnostics(&uri)
                    })
                    .collect();
                let remaining = guard
                    .diagnostic_sources(&a)
                    .merge()
                    .map_or(0, |info| info.diagnostics.len());
                drop(guard);
                (missing, remaining)
            };
            assert!(missing.is_empty(), "publications missing: {missing:?}");
            assert_eq!(remaining, 0, "clear was reordered");
        }

        /// A server publishing under the configured spelling of a root that is a
        /// symlink (or the logical `$PWD`) is not dropped by the pre-filter: the
        /// diagnostics are cached under the canonical key.
        #[cfg(unix)]
        #[tokio::test]
        async fn test_pump_accepts_publish_under_a_configured_root_alias() {
            let workspace = tempfile::TempDir::new().unwrap();
            let base = dunce::canonicalize(workspace.path()).unwrap();
            let real = base.join("real");
            std::fs::create_dir(&real).unwrap();
            std::fs::write(real.join("main.rs"), "fn main() {}").unwrap();
            let link = base.join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let roots = WorkspaceRoots::from_configured(std::slice::from_ref(&link)).unwrap();
            assert_eq!(roots.canonical(), std::slice::from_ref(&real));

            let cache = make_cache();
            let subs = make_subs();
            let (tx, _cancel_tx) = spawn_test_pump_with_cache(subs, roots, Arc::clone(&cache));
            let error = lsp_types::Diagnostic {
                message: "boom".to_owned().into(),
                ..Default::default()
            };
            tx.send(LspNotification::PublishDiagnostics(
                PublishDiagnosticsParams {
                    uri: bridge::path_to_uri(&link.join("main.rs")).unwrap(),
                    diagnostics: vec![error],
                    version: None,
                },
            ))
            .await
            .unwrap();

            let canonical_uri = bridge::path_to_uri(&real.join("main.rs")).unwrap();
            let info = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let sources = cache.lock().await.diagnostic_sources(&canonical_uri);
                    if let Some(info) = sources.merge() {
                        return info;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await;
            let info = info.unwrap_or_else(|_| panic!("alias publish was dropped by the pump"));
            assert_eq!(info.diagnostics.len(), 1);
        }

        /// #532: every stored publish notifies the canonical subscriber, and
        /// an empty canonical publish after errors on an alias keeps the errors.
        #[cfg(unix)]
        #[tokio::test]
        async fn test_pump_notifies_canonical_key_on_each_alias_publish() {
            use crate::mcp::{SessionHandle, Target};

            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let file = root.join("main.rs");
            std::fs::write(&file, "fn main() {}").unwrap();
            let link = root.join("link.rs");
            std::os::unix::fs::symlink(&file, &link).unwrap();

            let cache = make_cache();
            let subs = make_subs();
            let session = SessionHandle::new(subs.clone());
            let (tx_session, mut rx_session) = mpsc::channel(8);
            let canonical = bridge::resources::make_uri(&file).unwrap();
            session
                .subscribe_for_test(
                    &DiagnosticsResourceUri::for_test(&canonical),
                    Target::Channel(tx_session),
                )
                .await
                .unwrap();
            let (tx, _cancel_tx) = spawn_test_pump_with_cache(
                subs,
                WorkspaceRoots::from_configured(&[root]).unwrap(),
                Arc::clone(&cache),
            );
            let error = lsp_types::Diagnostic {
                message: "boom".to_owned().into(),
                ..Default::default()
            };
            let publishes = [(&link, vec![error]), (&file, vec![]), (&link, vec![])];
            for (path, diagnostics) in publishes {
                tx.send(LspNotification::PublishDiagnostics(
                    PublishDiagnosticsParams {
                        uri: bridge::path_to_uri(path).unwrap(),
                        diagnostics,
                        version: None,
                    },
                ))
                .await
                .unwrap();
                assert_eq!(recv_within(&mut rx_session).await, canonical);
            }

            let canonical_lsp = bridge::path_to_uri(&file).unwrap();
            let info = cache
                .lock()
                .await
                .diagnostic_sources(&canonical_lsp)
                .merge()
                .unwrap();
            assert!(info.diagnostics.is_empty(), "the alias's clear must apply");
        }

        /// #468: a session whose peer stopped reading neither blocks the pump
        /// nor other sessions, and loses no URI once it resumes.
        #[tokio::test]
        async fn test_pump_stalled_session_does_not_block_others_or_lose_updates() {
            use crate::mcp::{SessionHandle, Target};

            const URI_COUNT: usize = 5;

            let subs = make_subs();
            let stalled = SessionHandle::new(subs.clone());
            let healthy = SessionHandle::new(subs.clone());
            let (tx_stalled, mut rx_stalled) = mpsc::channel(1);
            let (tx_healthy, mut rx_healthy) = mpsc::channel(URI_COUNT);
            let expected: HashSet<DiagnosticsResourceUri> = (0..URI_COUNT)
                .map(|i| test_mcp_uri(&format!("f{i}.rs")))
                .collect();
            for uri in &expected {
                stalled
                    .subscribe_for_test(uri, Target::Channel(tx_stalled.clone()))
                    .await
                    .unwrap();
                healthy
                    .subscribe_for_test(uri, Target::Channel(tx_healthy.clone()))
                    .await
                    .unwrap();
            }

            let (tx, _cancel_tx) = spawn_test_pump(subs, test_workspace_roots());
            for i in 0..URI_COUNT {
                tx.send(publish(&format!("f{i}.rs"))).await.unwrap();
            }

            let mut healthy_received = HashSet::new();
            for _ in 0..URI_COUNT {
                healthy_received.insert(DiagnosticsResourceUri::for_test(
                    &recv_within(&mut rx_healthy).await,
                ));
            }
            assert_eq!(healthy_received, expected);

            let mut stalled_received = HashSet::new();
            for _ in 0..URI_COUNT {
                stalled_received.insert(DiagnosticsResourceUri::for_test(
                    &recv_within(&mut rx_stalled).await,
                ));
            }
            assert_eq!(stalled_received, expected);
        }

        type DuplexReader = tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>;

        /// Reads newline-delimited JSON-RPC lines until one satisfies `matches`.
        async fn read_json_line_matching(
            reader: &mut DuplexReader,
            matches: impl Fn(&serde_json::Value) -> bool + Send + Sync,
        ) -> serde_json::Value {
            use tokio::io::AsyncBufReadExt as _;

            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let mut line = String::new();
                    let n = reader.read_line(&mut line).await.unwrap();
                    assert!(n > 0, "stream closed before the expected message");
                    let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                    if matches(&value) {
                        return value;
                    }
                }
            })
            .await
            .expect("expected JSON-RPC message not received within 5 s")
        }

        /// #468/#492 stdio path: a real `Peer` over an in-memory duplex stream,
        /// driven with raw JSON-RPC, receives `resources/updated` from the real
        /// `diagnostics_pump` after a guarded subscribe.
        #[tokio::test]
        async fn test_stdio_shaped_peer_receives_resource_updates_through_pump() {
            use rmcp::ServiceExt as _;
            use tokio::io::AsyncWriteExt as _;

            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let file = root.join("main.rs");
            std::fs::write(&file, "fn main() {}").unwrap();
            let canonical_uri = bridge::resources::make_uri(&file).unwrap();

            let subs = make_subs();
            let server = mcp::McplsServer::new(
                Arc::new(Translator::new()),
                make_cache(),
                WorkspaceRoots::from_configured(std::slice::from_ref(&root)).unwrap(),
                subs.clone(),
                ProjectConfigStatus::NotIgnored,
                config::McpConfig::default(),
            );
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let serving = tokio::spawn(async move { server.serve(server_io).await.unwrap() });
            let (read_half, mut write_half) = tokio::io::split(client_io);
            let mut reader = tokio::io::BufReader::new(read_half);

            let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
            write_half
                .write_all(format!("{initialize}\n").as_bytes())
                .await
                .unwrap();
            read_json_line_matching(&mut reader, |v| v["id"] == 1).await;

            let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
            let subscribe = serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "resources/subscribe",
                "params": {"uri": canonical_uri},
            });
            for line in [initialized.to_owned(), subscribe.to_string()] {
                write_half
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .unwrap();
            }
            // Keeps the service (and its peer) alive for the rest of the test.
            let _running = serving.await.unwrap();
            let response = read_json_line_matching(&mut reader, |v| v["id"] == 2).await;
            assert!(
                response.get("error").is_none(),
                "subscribe failed: {response}"
            );

            let (tx, _cancel_tx) =
                spawn_test_pump(subs, WorkspaceRoots::from_configured(&[root]).unwrap());
            tx.send(LspNotification::PublishDiagnostics(
                PublishDiagnosticsParams {
                    uri: bridge::path_to_uri(&file).unwrap(),
                    diagnostics: vec![],
                    version: None,
                },
            ))
            .await
            .unwrap();

            let update = read_json_line_matching(&mut reader, |v| {
                v["method"] == "notifications/resources/updated"
            })
            .await;
            assert_eq!(update["params"]["uri"], canonical_uri);
        }

        /// A real `resources/subscribe` client over an in-memory duplex stream,
        /// against a server whose only language server (`rust`) is still
        /// starting; [`Self::settle_failed`] then records its startup failure.
        struct SubscribeHarness {
            translator: Arc<Translator>,
            subs: SubscriptionRegistry,
            uri: String,
            reader: DuplexReader,
            write_half: tokio::io::WriteHalf<tokio::io::DuplexStream>,
            next_id: u32,
            _running: rmcp::service::RunningService<rmcp::RoleServer, mcp::McplsServer>,
            _workspace: tempfile::TempDir,
        }

        impl SubscribeHarness {
            async fn start() -> Self {
                use rmcp::ServiceExt as _;

                let workspace = tempfile::TempDir::new().unwrap();
                let root = dunce::canonicalize(workspace.path()).unwrap();
                let file = root.join("main.rs");
                std::fs::write(&file, "fn main() {}").unwrap();
                let uri = bridge::resources::make_uri(&file).unwrap();

                let id = ServerId::from("rust");
                let mut translator = Translator::new()
                    .with_extensions(crate::test_lsp::test_extensions())
                    .with_router(config::ToolRouter::catch_all([(
                        id.clone(),
                        LanguageId::from_static("rust"),
                    )]));
                translator.set_workspace_roots(
                    WorkspaceRoots::from_configured(std::slice::from_ref(&root)).unwrap(),
                );
                translator.set_expected_servers(HashSet::from([id]));
                let translator = Arc::new(translator);

                let subs = make_subs();
                let server = mcp::McplsServer::new(
                    Arc::clone(&translator),
                    make_cache(),
                    WorkspaceRoots::from_configured(&[root]).unwrap(),
                    subs.clone(),
                    ProjectConfigStatus::NotIgnored,
                    config::McpConfig::default(),
                );
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                let serving = tokio::spawn(async move { server.serve(server_io).await.unwrap() });
                let (read_half, mut write_half) = tokio::io::split(client_io);
                let mut reader = tokio::io::BufReader::new(read_half);

                let initialize = serde_json::json!({
                    "jsonrpc": "2.0", "id": 0, "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": {"name": "test", "version": "0"},
                    },
                });
                let initialized =
                    serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
                send_json_line(&mut write_half, &initialize).await;
                read_json_line_matching(&mut reader, |v| v["id"] == 0).await;
                send_json_line(&mut write_half, &initialized).await;
                let running = serving.await.unwrap();

                Self {
                    translator,
                    subs,
                    uri,
                    reader,
                    write_half,
                    next_id: 1,
                    _running: running,
                    _workspace: workspace,
                }
            }

            /// Sends `resources/subscribe` for the fixture file and returns the response.
            async fn subscribe(&mut self) -> serde_json::Value {
                let id = self.next_id;
                self.next_id += 1;
                let request = serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "method": "resources/subscribe",
                    "params": {"uri": self.uri},
                });
                send_json_line(&mut self.write_half, &request).await;
                read_json_line_matching(&mut self.reader, |v| v["id"] == id).await
            }

            /// Records the `rust` server's startup failure the way `init_lsp_servers` does.
            fn settle_failed(&self) {
                self.translator
                    .record_startup_failures(&[crate::error::ServerSpawnFailure {
                        server_id: ServerId::from("rust"),
                        language_id: LanguageId::from_static("rust"),
                        command: "rust-analyzer".to_string(),
                        reason: crate::error::StartupFailure::Spawn(Arc::new(
                            Error::ServerNotFound {
                                command: "rust-analyzer".to_string(),
                                source: std::io::Error::from(std::io::ErrorKind::NotFound),
                            },
                        )),
                    }]);
                self.translator.rebind_router(&HashSet::new());
                self.translator.clear_expected_servers();
            }

            /// Counts `resources/updated` notifications that arrive within `window`.
            async fn updates_within(&mut self, window: std::time::Duration) -> usize {
                use tokio::io::AsyncBufReadExt as _;

                let mut count = 0;
                let _ = tokio::time::timeout(window, async {
                    loop {
                        let mut line = String::new();
                        if self.reader.read_line(&mut line).await.unwrap() == 0 {
                            return;
                        }
                        let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                        if value["method"] == "notifications/resources/updated" {
                            assert_eq!(value["params"]["uri"], self.uri);
                            count += 1;
                        }
                    }
                })
                .await;
                count
            }
        }

        async fn send_json_line(
            write_half: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
            value: &serde_json::Value,
        ) {
            use tokio::io::AsyncWriteExt as _;

            write_half
                .write_all(format!("{value}\n").as_bytes())
                .await
                .unwrap();
        }

        const UPDATE_WINDOW: std::time::Duration = std::time::Duration::from_millis(300);

        /// FR-010: a subscribe to a failed route errors and rolls back the
        /// subscription it added, so a later publish reaches nobody.
        #[tokio::test]
        async fn test_subscribe_to_failed_route_errors_and_rolls_back() {
            let mut harness = SubscribeHarness::start().await;
            harness.settle_failed();

            let response = harness.subscribe().await;

            assert!(response.get("error").is_some(), "{response}");
            assert!(response.to_string().contains("rust-analyzer"), "{response}");
            harness.subs.publish_matching(|_| true).await;
            assert_eq!(harness.updates_within(UPDATE_WINDOW).await, 0);
        }

        /// FR-010: a re-subscribe to a failed route errors but keeps the
        /// subscription made while the server was still starting.
        #[tokio::test]
        async fn test_resubscribe_to_failed_route_keeps_earlier_subscription() {
            let mut harness = SubscribeHarness::start().await;
            let first = harness.subscribe().await;
            assert!(first.get("error").is_none(), "{first}");
            harness.settle_failed();

            let second = harness.subscribe().await;

            assert!(second.get("error").is_some(), "{second}");
            harness.subs.publish_matching(|_| true).await;
            assert_eq!(harness.updates_within(UPDATE_WINDOW).await, 1);
        }

        /// FR-010/FR-012: subscribing while the server starts succeeds, and
        /// the settle publish then notifies exactly once for the failed URI.
        #[tokio::test]
        async fn test_subscribe_during_startup_gets_one_update_when_startup_fails() {
            let mut harness = SubscribeHarness::start().await;
            let response = harness.subscribe().await;
            assert!(response.get("error").is_none(), "{response}");
            assert_eq!(
                harness.updates_within(UPDATE_WINDOW).await,
                0,
                "nothing may publish before settle"
            );

            harness.settle_failed();
            publish_startup_failures(&harness.translator, &harness.subs).await;

            assert_eq!(harness.updates_within(UPDATE_WINDOW).await, 1);
        }
    }
}
