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
//! async fn main() -> Result<(), mcpls_core::Error> {
//!     let config = ServerConfig::load()?;
//!     // Stdio (default):
//!     serve(config).await
//!     // HTTP (requires `transport-http` feature):
//!     // let http = mcpls_core::HttpConfig::new("127.0.0.1:3000".parse().unwrap(), "/mcp");
//!     // serve_with(config, Transport::Http(http)).await
//! }
//! ```

pub mod bridge;
pub mod config;
pub mod error;
pub mod lsp;
pub mod mcp;
pub mod transport;

use std::collections::{HashMap, HashSet};
use std::path::{Component, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bridge::resources::make_uri;
use bridge::{NotificationCache, ResourceSubscriptions, Translator};
pub use config::{ProjectConfigTrust, ServerConfig};
use config::{ServerId, ToolRouter};
pub use error::Error;
use lsp::{LspNotification, LspServer, ServerInitConfig};
use lsp_types::Uri;
use rmcp::model::ResourceUpdatedNotificationParam;
use tokio::sync::{Mutex, OnceCell};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, error, info, warn};
#[cfg(feature = "transport-http")]
pub use transport::HttpConfig;
pub use transport::Transport;
#[cfg(feature = "transport-http")]
use transport::run_http;
use transport::run_stdio;

/// Whether `uri` falls within one of `workspace_roots`.
///
/// Used to reject diagnostics for out-of-workspace URIs before caching them:
/// a misbehaving or compromised LSP server could otherwise publish
/// diagnostics for an unbounded number of fabricated (often non-existent)
/// URIs, defeating `MAX_DIAGNOSTIC_ENTRIES`'s FIFO cap by flushing every
/// legitimate entry out of the cache before it (see #234). Deliberately does
/// not canonicalize -- this runs per incoming notification, and LSP servers
/// report already-resolved canonical paths, so a prefix check is enough to
/// reject URIs a legitimate server would never publish for, without a
/// filesystem syscall on every diagnostic.
///
/// # Preconditions
///
/// `workspace_roots` must itself already be canonical, or every diagnostic
/// silently fails to match and gets dropped (a raw `[[lsp_servers]]`-derived
/// or relative root will never `starts_with`-match a canonical LSP path).
/// `serve_with` guarantees this by passing `workspace_roots_snapshot`, which
/// is built via [`canonicalize_workspace_roots`] -- see that function's docs.
///
/// An empty `workspace_roots` (no workspace configured) allows any URI,
/// matching `validate_path_against_roots`'s "no roots = no restriction"
/// behavior.
fn diagnostic_path_in_workspace(uri: &Uri, workspace_roots: &[PathBuf]) -> bool {
    if workspace_roots.is_empty() {
        return true;
    }
    let Some(path) = bridge::uri_to_path(uri) else {
        return false;
    };
    // `Path::starts_with` compares components lexically and does not resolve
    // `.`/`..`, so `/workspace/../etc/passwd` would otherwise pass the
    // `/workspace` prefix check despite pointing outside it. A legitimate LSP
    // server never publishes such a path (canonical paths never contain
    // `.`/`..` components), so rejecting them outright costs nothing and
    // closes the bypass for a server that deliberately crafts one.
    if path
        .components()
        .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return false;
    }
    workspace_roots.iter().any(|root| path.starts_with(root))
}

/// `Arc`-backed state shared by every `diagnostics_pump` task spawned for one
/// `serve_with` run, factored out of `diagnostics_pump`'s parameter list to
/// keep it under clippy's argument-count lint. `Clone` is cheap (`Arc`
/// clones only).
#[derive(Clone)]
pub(crate) struct PumpShared {
    pub(crate) notification_cache: Arc<Mutex<NotificationCache>>,
    pub(crate) subs: Arc<ResourceSubscriptions>,
    pub(crate) peer_cell: Arc<OnceCell<rmcp::Peer<rmcp::RoleServer>>>,
    /// Used to reject diagnostics for out-of-workspace URIs; see
    /// `diagnostic_path_in_workspace`.
    pub(crate) workspace_roots: Arc<[PathBuf]>,
}

/// Background task that drains LSP notifications, writes them to the cache,
/// and forwards `resources/updated` to the MCP peer when subscribed.
///
/// The task operates in two phases without explicit state:
/// - **Phase A** (before peer is set): caches every notification, skips peer notify.
/// - **Phase B** (after peer is set): additionally fires `notify_resource_updated`
///   for subscribed `PublishDiagnostics` URIs.
///
/// The task exits when:
/// - The LSP notification channel closes (`rx.recv()` returns `None`).
/// - The cancellation watch fires (or the sender is dropped).
/// - `notify_resource_updated` returns an error (peer disconnect / transport closed).
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
    mut rx: tokio::sync::mpsc::Receiver<LspNotification>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    caches_diagnostics: bool,
    shared: PumpShared,
) {
    let PumpShared {
        notification_cache,
        subs,
        peer_cell,
        workspace_roots,
    } = shared;
    loop {
        tokio::select! {
            // Exit when cancellation is requested or the sender is dropped.
            result = cancel_rx.changed() => {
                // Err means the sender was dropped; treat as cancellation.
                if result.is_err() || *cancel_rx.borrow() {
                    break;
                }
            }
            msg = rx.recv() => {
                let Some(notif) = msg else { break };
                match notif {
                    LspNotification::PublishDiagnostics(p) => {
                        // Only the server the router resolves `Diagnostics` to for
                        // this notification's language caches (and notifies
                        // subscribers of) it -- see #174 §8. A server that was
                        // never the diagnostics route, or lost it without a live
                        // catch-all to rebind to, is not the authoritative source
                        // for this language's diagnostics; skip publishing so it
                        // doesn't overwrite (or spuriously notify about) another
                        // server's cache entry.
                        if !caches_diagnostics {
                            continue;
                        }
                        if !diagnostic_path_in_workspace(&p.uri, &workspace_roots) {
                            debug!(
                                "dropping diagnostics for out-of-workspace URI: {}",
                                p.uri.as_str()
                            );
                            continue;
                        }
                        {
                            let mut cache = notification_cache.lock().await;
                            cache.store_diagnostics(&server_id, &p.uri, p.version, p.diagnostics);
                        }

                        // Fast path: skip URI construction when nothing is subscribed.
                        if subs.is_empty().await {
                            continue;
                        }

                        // Notify only when peer is ready and URI is subscribed.
                        let Some(peer) = peer_cell.get() else { continue };
                        let Some(path) = bridge::uri_to_path(&p.uri) else { continue };
                        let Ok(mcp_uri) = make_uri(&path) else { continue };

                        if !subs.contains(&mcp_uri).await {
                            continue;
                        }

                        if peer
                            .notify_resource_updated(ResourceUpdatedNotificationParam::new(
                                mcp_uri,
                            ))
                            .await
                            .is_err()
                        {
                            // Peer disconnected; stop the pump.
                            break;
                        }
                    }
                    LspNotification::LogMessage(m) => {
                        let mut cache = notification_cache.lock().await;
                        cache.store_log(m.typ.into(), m.message);
                    }
                    LspNotification::ShowMessage(m) => {
                        let mut cache = notification_cache.lock().await;
                        cache.store_message(m.typ.into(), m.message);
                    }
                    LspNotification::Progress { .. } | LspNotification::Other { .. } => {}
                }
            }
        }
    }
}

/// Result of [`register_servers`]: everything the caller needs to start the
/// per-server diagnostics pump tasks.
pub(crate) struct RegisteredServers {
    /// Notification receivers extracted from each server before registration.
    pub(crate) receivers: HashMap<ServerId, tokio::sync::mpsc::Receiver<lsp::LspNotification>>,
    /// Whether each server is the one the (rebound) router resolves
    /// `ToolKind::Diagnostics` to for its language -- see #174 §8. Computed
    /// here, right after the rebind, so it always reflects the post-rebind
    /// router rather than a stale pre-rebind view.
    pub(crate) diagnostics_flags: HashMap<ServerId, bool>,
}

/// Register initialized LSP servers with the translator, rebind the router to
/// the set that actually registered, and extract notification receivers.
///
/// Takes ownership of the `ServerInitResult`, extracts `notification_rx` from
/// each server before registration. Registration itself is a sequence of
/// short, independently-locked map inserts (see `Translator`'s field docs),
/// so no external synchronization is required here; the rebind that follows
/// relies only on all of *this* function's inserts having completed, which
/// the sequential code below guarantees.
///
/// `configs` supplies the `ServerInitConfig` each surviving server was
/// spawned from, keyed by routing identity, so the translator can respawn it
/// later if its process dies (see `Translator::respawn_if_dead`).
pub(crate) fn register_servers(
    mut result: lsp::ServerInitResult,
    translator: &bridge::Translator,
    configs: &HashMap<ServerId, ServerInitConfig>,
) -> RegisteredServers {
    let mut receivers = HashMap::new();
    for (id, server) in &mut result.servers {
        receivers.insert(id.clone(), server.take_notification_rx());
    }

    let registered: HashSet<ServerId> = result.servers.keys().cloned().collect();

    let mut language_by_id = HashMap::new();
    for (id, server) in result.servers {
        let client = server.client().clone();
        language_by_id.insert(id.clone(), client.language_id().to_string());
        translator.register_client(id.clone(), client);
        if let Some(config) = configs.get(&id) {
            translator.register_server_config(id.clone(), config.clone());
        } else {
            // Would silently turn auto-respawn into a no-op for this server
            // (surfacing as `Error::ServerUnavailable` instead of actually
            // recovering) -- the keys are derived identically on both sides
            // (`LspServerConfig::id()`), so this should never happen; warn
            // rather than fail, since the server is otherwise usable.
            warn!(
                "No respawn config registered for LSP server '{id}'; auto-respawn on crash will be unavailable for it"
            );
        }
        translator.register_server(id, server);
    }

    translator.rebind_router(&registered);

    let diagnostics_flags = language_by_id
        .into_iter()
        .map(|(id, language)| {
            let is_diagnostics_server = translator.is_diagnostics_route(&language, &id);
            (id, is_diagnostics_server)
        })
        .collect();

    RegisteredServers {
        receivers,
        diagnostics_flags,
    }
}

/// Resolve workspace roots from config or current directory.
///
/// If no workspace roots are provided in the configuration, this function
/// will use the current working directory, canonicalized for security.
///
/// # Returns
///
/// A vector of workspace root paths. If config roots are provided, they are
/// returned as-is. Otherwise, returns the canonicalized current directory,
/// falling back to relative "." if canonicalization fails.
fn resolve_workspace_roots(config_roots: &[PathBuf]) -> Vec<PathBuf> {
    if config_roots.is_empty() {
        match std::env::current_dir() {
            Ok(cwd) => {
                // current_dir() always returns an absolute path
                match cwd.canonicalize() {
                    Ok(canonical) => {
                        info!(
                            "Using current directory as workspace root: {}",
                            canonical.display()
                        );
                        vec![canonical]
                    }
                    Err(e) => {
                        // Canonicalization can fail if directory was deleted or permissions changed
                        // but cwd itself is still absolute
                        warn!(
                            "Failed to canonicalize current directory: {e}, using non-canonical path"
                        );
                        vec![cwd]
                    }
                }
            }
            Err(e) => {
                // This is extremely rare - only happens if cwd was deleted or unlinked
                // In this case, we have no choice but to use a relative path
                warn!("Failed to get current directory: {e}, using fallback");
                vec![PathBuf::from(".")]
            }
        }
    } else {
        config_roots.to_vec()
    }
}

/// Canonicalize each workspace root, falling back to the original path for
/// any root that fails to canonicalize (e.g. deleted after startup).
///
/// `resolve_workspace_roots` returns config-provided roots unmodified
/// (relative paths, symlinks kept as-is); this normalizes them so a plain
/// prefix comparison against an already-resolved LSP path (see
/// `diagnostic_path_in_workspace`) works without a filesystem syscall on
/// that hot per-notification path.
///
/// Uses [`dunce::canonicalize`] rather than [`Path::canonicalize`]: on
/// Windows, the latter returns the `\\?\`-prefixed verbatim form (e.g.
/// `\\?\C:\...`), which a URI-derived path from `Url::to_file_path` (never
/// verbatim-prefixed) can never `starts_with`-match, silently dropping every
/// diagnostic. `dunce::canonicalize` resolves symlinks identically but
/// returns the ordinary `C:\...` form when the result doesn't require the
/// verbatim syntax (i.e. essentially always, for realistic workspace paths).
fn canonicalize_workspace_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .map(|root| dunce::canonicalize(root).unwrap_or_else(|_| root.clone()))
        .collect()
}

/// Start the MCPLS server with the given configuration over stdio.
///
/// This is the backward-compatible entry point. It is equivalent to calling
/// `serve_with(config, Transport::Stdio)`.
///
/// # Errors
///
/// Returns an error if:
/// - All LSP servers fail to initialize
/// - MCP server setup fails
/// - Configuration is invalid
///
/// # Graceful Degradation
///
/// - **All servers succeed**: Service runs normally
/// - **Partial success**: Logs warnings for failures, continues with available servers
/// - **All servers fail**: Returns `Error::AllServersFailedToInit` with details
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
/// - All LSP servers fail to initialize
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
/// # Examples
///
/// ```rust,ignore
/// use mcpls_core::{serve_with, Transport, ServerConfig};
///
/// #[tokio::main]
/// async fn main() -> Result<(), mcpls_core::Error> {
///     let config = ServerConfig::load()?;
///     serve_with(config, Transport::Stdio).await
/// }
/// ```
pub async fn serve_with(config: ServerConfig, transport: Transport) -> Result<(), Error> {
    info!("Starting MCPLS server...");

    let project_config_ignored = config.project_config_ignored;
    let workspace_roots = resolve_workspace_roots(&config.workspace.roots);
    let extension_map = config.build_effective_extension_map();
    let max_depth = Some(config.workspace.heuristics_max_depth);

    let applicable_configs: Vec<ServerInitConfig> = config
        .lsp_servers
        .iter()
        .filter_map(|lsp_config| {
            let should_spawn = workspace_roots
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
                workspace_roots: workspace_roots.clone(),
                initialization_options: lsp_config.initialization_options.clone(),
                notification_tx: None,
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

    // Built here (rather than alongside `subscriptions`/`peer_cell` below) so
    // it can be handed to the translator, which uses it to invalidate a
    // respawned server's stale cached diagnostics -- see
    // `Translator::with_notification_cache`. Independent of `translator`
    // itself, which holds no outer lock: the pump only ever locks this
    // cache, so it never contends with a request handler running an
    // in-flight LSP round-trip.
    let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));

    let mut translator = Translator::new()
        .with_extensions(extension_map)
        .with_router(router)
        .with_notification_cache(Arc::clone(&notification_cache));
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
    // Fixed for the server's lifetime: shared as a lock-free snapshot so
    // cache-only handlers (e.g. `get_cached_diagnostics`, `read_resource`) can
    // validate a path without locking `translator` below.
    //
    // Canonicalized once here rather than left as-is: `resolve_workspace_roots`
    // returns config-provided roots unmodified (relative paths, symlinks kept),
    // but `diagnostic_path_in_workspace` (fed by this snapshot) does a plain
    // prefix check with no filesystem I/O on its hot per-notification path --
    // that only matches correctly if both sides are already in the same
    // (canonical) form, and LSP servers report already-resolved canonical
    // paths. Comparing an un-canonicalized root against a canonical path would
    // silently drop every diagnostic for a workspace configured with a
    // relative or symlinked root. Falls back to the original root if
    // canonicalization fails (e.g. deleted between startup and this point);
    // `validate_path_against_roots`'s own per-call canonicalize is unaffected
    // either way, since canonicalizing an already-canonical path is a no-op.
    let workspace_roots_snapshot: Arc<[PathBuf]> =
        Arc::from(canonicalize_workspace_roots(&workspace_roots));

    let translator = Arc::new(translator);
    let subscriptions = Arc::new(ResourceSubscriptions::new());
    // Peer cell is populated after the MCP transport is established (Phase B).
    let peer_cell = Arc::new(OnceCell::new());

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
            Arc::clone(&subscriptions),
            Arc::clone(&peer_cell),
            cancel_rx.clone(),
            Arc::clone(&workspace_roots_snapshot),
        ))
    };

    info!("Starting MCP server with rmcp...");
    let mcp_server = mcp::McplsServer::new(
        Arc::clone(&translator),
        Arc::clone(&notification_cache),
        Arc::clone(&workspace_roots_snapshot),
        Arc::clone(&subscriptions),
        project_config_ignored,
    );
    info!("MCPLS server initialized successfully");

    let result = match transport {
        Transport::Stdio => {
            info!("Listening for MCP requests on stdio...");
            run_stdio(mcp_server, &peer_cell).await
        }
        #[cfg(feature = "transport-http")]
        Transport::Http(cfg) => run_http(mcp_server, cfg).await,
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

/// Post-transport shutdown sequence, run once the transport future
/// (`run_stdio`/`run_http`) returns — whether that's because of a
/// `SIGTERM`/`SIGINT`, stdio EOF, or (for HTTP) its own graceful shutdown.
///
/// Signals background pump tasks to exit, then gracefully shuts down every
/// LSP server registered on `translator` (see
/// [`Translator::shutdown_servers`] for what "gracefully" bounds and falls
/// back to). Finally, if the background LSP init task (see
/// [`spawn_lsp_servers_background`]) is still running, awaits its
/// `JoinHandle` with a bounded timeout: this surfaces a panic in that task
/// (previously dropped silently, see #196) and gives its diagnostics pump
/// tasks a chance to finish draining before `serve_with` returns.
/// Extracted from [`serve_with`] so this sequence is exercised directly in
/// tests without needing a full stdio/HTTP transport round trip.
async fn shutdown(
    cancel_tx: &tokio::sync::watch::Sender<bool>,
    translator: &Translator,
    lsp_init_handle: Option<JoinHandle<()>>,
) {
    let _ = cancel_tx.send(true);

    info!("Shutting down LSP servers...");
    translator.shutdown_servers().await;

    if let Some(handle) = lsp_init_handle {
        match tokio::time::timeout(LSP_INIT_TASK_SHUTDOWN_TIMEOUT, handle).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => error!("Background LSP initialization task failed: {err}"),
            Err(_) => {
                warn!("Timed out waiting for background LSP initialization task to stop");
            }
        }
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
/// retry. If every server fails, the "expected servers" set is cleared so those
/// calls fall back to a plain "no server configured" error instead.
///
/// Returns the task's `JoinHandle` so [`shutdown`] can await it: previously
/// this handle was dropped, silently swallowing panics from
/// `LspServer::spawn_batch`, `register_servers`, or a diagnostics pump task
/// (see #196).
fn spawn_lsp_servers_background(
    applicable_configs: Vec<ServerInitConfig>,
    translator: Arc<Translator>,
    notification_cache: Arc<Mutex<NotificationCache>>,
    subscriptions: Arc<ResourceSubscriptions>,
    peer_cell: Arc<OnceCell<rmcp::Peer<rmcp::RoleServer>>>,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    workspace_roots: Arc<[PathBuf]>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let configs_by_id: HashMap<ServerId, ServerInitConfig> = applicable_configs
            .iter()
            .map(|c| (c.server_config.id(), c.clone()))
            .collect();
        let result = LspServer::spawn_batch(&applicable_configs).await;

        if result.all_failed() {
            error!(
                "All {} configured LSP server(s) failed to initialize",
                result.failure_count()
            );
            for failure in &result.failures {
                error!("Server initialization failed: {}", failure);
            }
            // No server will register: rebind against an empty registered
            // set so every route drops (one rule, no special case -- see
            // `ToolRouter::rebind_to_registered`), then stop reporting
            // "still initializing". This path returns before
            // `register_servers` ever runs, so it needs its own rebind call;
            // skipping it would leave every route pointed at a dead server.
            translator.rebind_router(&HashSet::new());
            translator.clear_expected_servers();
            return;
        }

        if result.partial_success() {
            warn!(
                "Partial server initialization: {} succeeded, {} failed",
                result.server_count(),
                result.failure_count()
            );
            for failure in &result.failures {
                error!("Server initialization failed: {}", failure);
            }
        }

        let server_count = result.server_count();
        let registered = register_servers(result, &translator, &configs_by_id);
        // Background initialization has completed; stop reporting "still
        // initializing" (especially for servers that failed to spawn on
        // partial success, which would otherwise return ServerInitializing
        // forever instead of NoServerForLanguage/Tool).
        translator.clear_expected_servers();
        info!("Proceeding with {} LSP server(s)", server_count);

        // Give each diagnostics-route server a fair share of the shared
        // diagnostics cache budget now that the full set is known -- see
        // `NotificationCache::set_diagnostics_route_count` (#266).
        let diagnostics_route_count = registered
            .diagnostics_flags
            .values()
            .filter(|&&is_route| is_route)
            .count();
        notification_cache
            .lock()
            .await
            .set_diagnostics_route_count(diagnostics_route_count);

        // Start diagnostics pump tasks now that servers are registered.
        let pump_shared = PumpShared {
            notification_cache,
            subs: subscriptions,
            peer_cell,
            workspace_roots,
        };
        let mut pumps: JoinSet<()> = JoinSet::new();
        for (id, rx) in registered.receivers {
            let caches_diagnostics = registered
                .diagnostics_flags
                .get(&id)
                .copied()
                .unwrap_or(false);
            pumps.spawn(diagnostics_pump(
                id,
                rx,
                cancel_rx.clone(),
                caches_diagnostics,
                pump_shared.clone(),
            ));
        }
        while pumps.join_next().await.is_some() {}
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_diagnostic_path_in_workspace_empty_roots_allows_any_uri() {
        let uri: Uri = "file:///anywhere/at/all.rs".parse().unwrap();
        assert!(diagnostic_path_in_workspace(&uri, &[]));
    }

    #[test]
    fn test_diagnostic_path_in_workspace_accepts_uri_under_root() {
        // `Url::to_file_path` on Windows requires the URL's first path
        // segment to be a drive letter; a Unix-style path with no drive
        // letter fails to convert at all (`uri_to_path` returns `None`),
        // trivially satisfying this assertion for the wrong reason. Use a
        // drive-letter path so the test actually exercises the prefix check
        // on every platform.
        #[cfg(windows)]
        let (root, uri_str) = (
            PathBuf::from(r"C:\workspace\project"),
            "file:///C:/workspace/project/src/main.rs",
        );
        #[cfg(not(windows))]
        let (root, uri_str) = (
            PathBuf::from("/workspace/project"),
            "file:///workspace/project/src/main.rs",
        );
        let uri: Uri = uri_str.parse().unwrap();
        assert!(diagnostic_path_in_workspace(&uri, &[root]));
    }

    #[test]
    fn test_diagnostic_path_in_workspace_rejects_uri_outside_roots() {
        #[cfg(windows)]
        let (root, uri_str) = (
            PathBuf::from(r"C:\workspace\project"),
            "file:///C:/etc/passwd",
        );
        #[cfg(not(windows))]
        let (root, uri_str) = (PathBuf::from("/workspace/project"), "file:///etc/passwd");
        let uri: Uri = uri_str.parse().unwrap();
        assert!(!diagnostic_path_in_workspace(&uri, &[root]));
    }

    #[test]
    fn test_diagnostic_path_in_workspace_rejects_non_file_uri() {
        let root = PathBuf::from("/workspace/project");
        let uri: Uri = "untitled:Untitled-1".parse().unwrap();
        assert!(!diagnostic_path_in_workspace(&uri, &[root]));
    }

    /// `Path::starts_with` is a lexical, component-wise comparison that does
    /// not resolve `.`/`..` — without an explicit check, a URI like
    /// `file:///workspace/project/../../etc/passwd` would lexically "start
    /// with" `/workspace/project` despite pointing outside it.
    #[test]
    fn test_diagnostic_path_in_workspace_rejects_parent_dir_traversal() {
        #[cfg(windows)]
        let (root, uri_str) = (
            PathBuf::from(r"C:\workspace\project"),
            "file:///C:/workspace/project/../../etc/passwd",
        );
        #[cfg(not(windows))]
        let (root, uri_str) = (
            PathBuf::from("/workspace/project"),
            "file:///workspace/project/../../etc/passwd",
        );
        let uri: Uri = uri_str.parse().unwrap();
        assert!(!diagnostic_path_in_workspace(&uri, &[root]));
    }

    #[test]
    fn test_canonicalize_workspace_roots_falls_back_on_nonexistent_path() {
        let missing = PathBuf::from("/definitely/does/not/exist/anywhere");
        let result = canonicalize_workspace_roots(std::slice::from_ref(&missing));
        assert_eq!(result, vec![missing]);
    }

    /// #234 round-3 regression: a symlinked workspace root must canonicalize
    /// to its real path, matching what LSP servers report in diagnostics --
    /// otherwise `diagnostic_path_in_workspace`'s uncanonicalized prefix check
    /// would silently drop every diagnostic for that workspace.
    #[test]
    #[cfg(unix)]
    fn test_canonicalize_workspace_roots_resolves_symlink() {
        use std::os::unix::fs::symlink;

        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let base = temp_dir.path().canonicalize().unwrap();
        let real_dir = base.join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let link_dir = base.join("link");
        symlink(&real_dir, &link_dir).unwrap();

        let result = canonicalize_workspace_roots(&[link_dir]);
        assert_eq!(result, vec![real_dir]);
    }

    #[test]
    fn test_resolve_workspace_roots_empty_config() {
        let roots = resolve_workspace_roots(&[]);
        assert_eq!(roots.len(), 1);
        assert!(
            roots[0].is_absolute(),
            "Workspace root should be absolute path"
        );
    }

    #[test]
    fn test_resolve_workspace_roots_with_config() {
        let config_roots = vec![PathBuf::from("/test/root")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots, config_roots);
    }

    #[test]
    fn test_resolve_workspace_roots_multiple_paths() {
        let config_roots = vec![PathBuf::from("/test/root1"), PathBuf::from("/test/root2")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots, config_roots);
        assert_eq!(roots.len(), 2);
    }

    #[test]
    fn test_resolve_workspace_roots_preserves_order() {
        let config_roots = vec![
            PathBuf::from("/workspace/alpha"),
            PathBuf::from("/workspace/beta"),
            PathBuf::from("/workspace/gamma"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots[0], PathBuf::from("/workspace/alpha"));
        assert_eq!(roots[1], PathBuf::from("/workspace/beta"));
        assert_eq!(roots[2], PathBuf::from("/workspace/gamma"));
    }

    #[test]
    fn test_resolve_workspace_roots_single_path() {
        let config_roots = vec![PathBuf::from("/single/workspace")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from("/single/workspace"));
    }

    #[test]
    fn test_resolve_workspace_roots_empty_returns_cwd() {
        let roots = resolve_workspace_roots(&[]);
        assert!(
            !roots.is_empty(),
            "Should return at least one workspace root"
        );
    }

    #[test]
    fn test_resolve_workspace_roots_relative_paths() {
        let config_roots = vec![
            PathBuf::from("relative/path1"),
            PathBuf::from("relative/path2"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("relative/path1"));
        assert_eq!(roots[1], PathBuf::from("relative/path2"));
    }

    #[test]
    fn test_resolve_workspace_roots_mixed_paths() {
        let config_roots = vec![
            PathBuf::from("/absolute/path"),
            PathBuf::from("relative/path"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/absolute/path"));
        assert_eq!(roots[1], PathBuf::from("relative/path"));
    }

    #[test]
    fn test_resolve_workspace_roots_with_dot_path() {
        let config_roots = vec![PathBuf::from(".")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots, config_roots);
    }

    #[test]
    fn test_resolve_workspace_roots_with_parent_path() {
        let config_roots = vec![PathBuf::from("..")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from(".."));
    }

    #[test]
    fn test_resolve_workspace_roots_unicode_paths() {
        let config_roots = vec![
            PathBuf::from("/workspace/テスト"),
            PathBuf::from("/workspace/тест"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/workspace/テスト"));
        assert_eq!(roots[1], PathBuf::from("/workspace/тест"));
    }

    #[test]
    fn test_resolve_workspace_roots_spaces_in_paths() {
        let config_roots = vec![
            PathBuf::from("/workspace/path with spaces"),
            PathBuf::from("/another path/workspace"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/workspace/path with spaces"));
    }

    // Tests for graceful degradation behavior
    mod graceful_degradation_tests {
        use super::*;
        use crate::error::ServerSpawnFailure;
        use crate::lsp::ServerInitResult;

        #[test]
        fn test_all_servers_failed_error_handling() {
            let mut result = ServerInitResult::new();
            result.add_failure(ServerSpawnFailure {
                server_id: ServerId::from("rust"),
                language_id: "rust".to_string(),
                command: "rust-analyzer".to_string(),
                message: "not found".to_string(),
            });
            result.add_failure(ServerSpawnFailure {
                server_id: ServerId::from("python"),
                language_id: "python".to_string(),
                command: "pyright".to_string(),
                message: "not found".to_string(),
            });

            assert!(result.all_failed());
            assert_eq!(result.failure_count(), 2);
            assert_eq!(result.server_count(), 0);
        }

        #[test]
        fn test_partial_success_detection() {
            use std::collections::HashMap;

            let mut result = ServerInitResult::new();
            // Simulate one success and one failure
            result.servers = HashMap::new(); // Would have a real server in production
            result.add_failure(ServerSpawnFailure {
                server_id: ServerId::from("python"),
                language_id: "python".to_string(),
                command: "pyright".to_string(),
                message: "not found".to_string(),
            });

            // Without actual servers, we can verify the failure was recorded
            assert_eq!(result.failure_count(), 1);
            assert_eq!(result.server_count(), 0);
        }

        #[test]
        fn test_all_servers_succeeded_detection() {
            use std::collections::HashMap;

            let mut result = ServerInitResult::new();
            result.servers = HashMap::new(); // Would have real servers in production

            assert_eq!(result.failure_count(), 0);
            assert!(!result.all_failed());
            assert!(!result.partial_success());
        }

        #[test]
        fn test_all_servers_failed_to_init_error() {
            let failures = vec![
                ServerSpawnFailure {
                    server_id: ServerId::from("rust"),
                    language_id: "rust".to_string(),
                    command: "rust-analyzer".to_string(),
                    message: "command not found".to_string(),
                },
                ServerSpawnFailure {
                    server_id: ServerId::from("python"),
                    language_id: "python".to_string(),
                    command: "pyright".to_string(),
                    message: "permission denied".to_string(),
                },
            ];

            let err = Error::AllServersFailedToInit { count: 2, failures };

            assert!(err.to_string().contains("all LSP servers failed"));
            assert!(err.to_string().contains("2 configured"));

            // Verify failures are preserved
            if let Error::AllServersFailedToInit { count, failures: f } = err {
                assert_eq!(count, 2);
                assert_eq!(f.len(), 2);
                assert_eq!(f[0].language_id, "rust");
                assert_eq!(f[1].language_id, "python");
            } else {
                panic!("Expected AllServersFailedToInit error");
            }
        }

        #[test]
        fn test_graceful_degradation_with_empty_config() {
            let result = ServerInitResult::new();

            // Empty config means no servers configured
            assert!(!result.all_failed());
            assert!(!result.partial_success());
            assert!(!result.has_servers());
            assert_eq!(result.server_count(), 0);
            assert_eq!(result.failure_count(), 0);
        }

        #[test]
        fn test_server_spawn_failure_display() {
            let failure = ServerSpawnFailure {
                server_id: ServerId::from("typescript"),
                language_id: "typescript".to_string(),
                command: "tsserver".to_string(),
                message: "executable not found in PATH".to_string(),
            };

            let display = failure.to_string();
            assert!(display.contains("typescript"));
            assert!(display.contains("tsserver"));
            assert!(display.contains("executable not found"));
        }

        #[test]
        fn test_result_helpers_consistency() {
            let mut result = ServerInitResult::new();

            // Initially empty
            assert!(!result.has_servers());
            assert!(!result.all_failed());
            assert!(!result.partial_success());

            // Add a failure
            result.add_failure(ServerSpawnFailure {
                server_id: ServerId::from("go"),
                language_id: "go".to_string(),
                command: "gopls".to_string(),
                message: "error".to_string(),
            });

            assert!(result.all_failed());
            assert!(!result.has_servers());
            assert!(!result.partial_success());
        }

        #[tokio::test]
        async fn test_serve_degrades_when_all_servers_fail_to_spawn() {
            use crate::config::{LspServerConfig, WorkspaceConfig};

            // A configured server whose command cannot spawn used to make serve()
            // fail synchronously with NoServersAvailable / AllServersFailedToInit.
            // LSP initialization now runs in a background task so the MCP
            // `initialize` handshake is never blocked, which means the spawn
            // failure is handled in the background instead: serve() starts the MCP
            // server in degraded mode (mirroring `test_serve_starts_with_empty_config`)
            // rather than failing fast. Any error it surfaces must therefore be a
            // transport/MCP error from the closed test connection, NOT a fail-fast
            // server-availability error.
            let config = ServerConfig {
                workspace: WorkspaceConfig {
                    roots: vec![PathBuf::from("/tmp/test-workspace")],
                    position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                    language_extensions: vec![],
                    heuristics_max_depth: 10,
                },
                lsp_servers: vec![LspServerConfig {
                    language_id: "rust".to_string(),
                    command: "nonexistent-command-that-will-fail-12345".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec!["**/*.rs".to_string()],
                    initialization_options: None,
                    timeout_seconds: 10,
                    request_timeout_seconds: 10,
                    heuristics: None,
                    name: None,
                    handles: None,
                }],
                project_config_ignored: false,
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
                    !matches!(err, Error::NoServersAvailable(_))
                        && !matches!(err, Error::AllServersFailedToInit { .. }),
                    "serve() must not fail fast now that LSP init is backgrounded; got: {err:?}"
                ),
            }
        }

        #[tokio::test]
        async fn test_serve_starts_with_empty_config() {
            use crate::config::WorkspaceConfig;

            // Server starts in protocol-only mode when no LSP servers are configured.
            // serve() blocks until the MCP transport closes, so it will error with a
            // connection/transport error — not NoServersAvailable.
            let config = ServerConfig {
                workspace: WorkspaceConfig {
                    roots: vec![PathBuf::from("/tmp/test-workspace")],
                    position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                    language_extensions: vec![],
                    heuristics_max_depth: 10,
                },
                lsp_servers: vec![],
                project_config_ignored: false,
            };

            let result = serve(config).await;

            // serve() may succeed or fail with a transport error, but must NOT
            // return NoServersAvailable when the config simply has no servers.
            if let Err(ref err) = result {
                assert!(
                    !matches!(err, Error::NoServersAvailable(_)),
                    "serve() must not return NoServersAvailable for empty lsp_servers config"
                );
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

        /// #196: a panicking background init task must not hang or crash
        /// `shutdown` — the panic is caught by the `JoinHandle` and logged.
        #[tokio::test]
        async fn test_shutdown_logs_panicking_background_init_task() {
            let translator = Translator::new();
            let (cancel_tx, _cancel_rx) = tokio::sync::watch::channel(false);

            let handle = tokio::spawn(async {
                panic!("simulated background LSP init panic");
            });

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                super::super::shutdown(&cancel_tx, &translator, Some(handle)),
            )
            .await;

            assert!(
                result.is_ok(),
                "shutdown must not hang or propagate a panic from the background init task"
            );
        }
    }

    // ------------------------------------------------------------------
    // diagnostics_pump unit tests
    // ------------------------------------------------------------------

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    mod pump_tests {
        use lsp_types::{PublishDiagnosticsParams, Uri};
        use tokio::sync::{mpsc, watch};

        use super::*;

        fn make_cache() -> Arc<Mutex<NotificationCache>> {
            Arc::new(Mutex::new(NotificationCache::new()))
        }

        fn make_subs() -> Arc<ResourceSubscriptions> {
            Arc::new(ResourceSubscriptions::new())
        }

        type PeerCell = Arc<OnceCell<rmcp::Peer<rmcp::RoleServer>>>;

        fn make_peer_cell() -> PeerCell {
            Arc::new(OnceCell::new())
        }

        /// Empty workspace roots: `diagnostic_path_in_workspace` allows any
        /// URI in this mode, matching `validate_path_against_roots`, so these
        /// pump-mechanics tests don't need to construct real workspace paths.
        fn no_workspace_roots() -> Arc<[PathBuf]> {
            Arc::from([])
        }

        /// `PublishDiagnostics` is cached even when the peer is not yet connected.
        #[tokio::test]
        async fn test_pump_caches_before_peer_set() {
            let cache = make_cache();
            let subs = make_subs();
            let peer_cell = make_peer_cell();
            let (tx, rx) = mpsc::channel(8);
            // Keep _cancel_tx alive: dropping it causes cancel_rx.changed() to return Err,
            // which makes the pump exit before processing any messages.
            let (_cancel_tx, cancel_rx) = watch::channel(false);

            let c = Arc::clone(&cache);
            tokio::spawn(diagnostics_pump(
                ServerId::from("rust"),
                rx,
                cancel_rx,
                true,
                PumpShared {
                    notification_cache: c,
                    subs: Arc::clone(&subs),
                    peer_cell: Arc::clone(&peer_cell),
                    workspace_roots: no_workspace_roots(),
                },
            ));

            let uri: Uri = "file:///test/main.rs".parse().unwrap();
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
                        guard.get_diagnostics(uri.as_str()).is_some()
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
            let peer_cell = make_peer_cell();
            let (tx, rx) = mpsc::channel(8);
            let (_cancel_tx, cancel_rx) = watch::channel(false);

            // See `test_diagnostic_path_in_workspace_accepts_uri_under_root`
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
            let workspace_roots: Arc<[PathBuf]> = Arc::from([workspace_root]);

            tokio::spawn(diagnostics_pump(
                ServerId::from("rust"),
                rx,
                cancel_rx,
                true,
                PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs: Arc::clone(&subs),
                    peer_cell: Arc::clone(&peer_cell),
                    workspace_roots,
                },
            ));

            let outside_uri: Uri = outside_uri_str.parse().unwrap();
            let inside_uri: Uri = inside_uri_str.parse().unwrap();

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
                        if guard.get_diagnostics(inside_uri.as_str()).is_some() {
                            return;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("pump did not cache in-workspace diagnostics within 5 s");

            let found_outside = cache
                .lock()
                .await
                .get_diagnostics(outside_uri.as_str())
                .is_some();
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
            let peer_cell = make_peer_cell();
            let (_tx, rx) = mpsc::channel::<LspNotification>(8);
            let (cancel_tx, cancel_rx) = watch::channel(false);

            let handle = tokio::spawn(diagnostics_pump(
                ServerId::from("rust"),
                rx,
                cancel_rx,
                true,
                PumpShared {
                    notification_cache: cache,
                    subs,
                    peer_cell,
                    workspace_roots: no_workspace_roots(),
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
            let peer_cell = make_peer_cell();
            let (_tx, rx) = mpsc::channel::<LspNotification>(8);
            let (cancel_tx, cancel_rx) = watch::channel(false);

            let handle = tokio::spawn(diagnostics_pump(
                ServerId::from("rust"),
                rx,
                cancel_rx,
                true,
                PumpShared {
                    notification_cache: cache,
                    subs,
                    peer_cell,
                    workspace_roots: no_workspace_roots(),
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
            let peer_cell = make_peer_cell();
            let (tx, rx) = mpsc::channel(8);
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
                cancel_rx,
                true,
                PumpShared {
                    notification_cache: Arc::clone(&cache),
                    subs,
                    peer_cell,
                    workspace_roots: no_workspace_roots(),
                },
            ));

            let uri: Uri = "file:///test/locked.rs".parse().unwrap();
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
                        if guard.get_diagnostics(uri.as_str()).is_some() {
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
    }
}
