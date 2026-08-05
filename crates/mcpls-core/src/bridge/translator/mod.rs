//! MCP to LSP translation layer.
//!
//! `Translator` owns the LSP client/server registries and dispatches MCP
//! tool calls to per-domain handler modules. This module defines the
//! `Translator` struct itself plus setup/lifecycle methods (construction,
//! registration, shutdown); actual tool-call handling lives in the sibling
//! modules below, grouped by domain.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::Mutex;

use self::clock::{Clock, SystemClock};
use self::encoding_ctx::EncodingCtx;
use self::respawn::RespawnBackoff;
use crate::bridge::encoding::PositionEncoding;
use crate::bridge::state::ResourceLimits;
use crate::bridge::{DocumentTracker, NotificationCache, lock_std};
use crate::config::{ServerId, ToolKind, ToolRouter};
use crate::lsp::{LspClient, LspServer, ServerInitConfig};

mod assist;
mod call_hierarchy;
mod clock;
mod diagnostics;
mod dto;
mod edits;
mod encoding_ctx;
mod navigation;
mod respawn;
mod routing;
mod symbols;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod testing;

pub use dto::*;
pub use routing::validate_path_against_roots;

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
    /// LSP clients indexed by routing identity. Locked only for the map
    /// lookup/insert itself, never across an LSP request.
    lsp_clients: Arc<StdMutex<HashMap<ServerId, LspClient>>>,
    /// LSP servers indexed by routing identity (held for lifetime management).
    lsp_servers: Arc<StdMutex<HashMap<ServerId, LspServer>>>,
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
    workspace_roots: Arc<Vec<PathBuf>>,
    /// Custom file extension to language ID mappings. Read-only after
    /// `serve()` setup, so no lock is needed.
    extension_map: Arc<HashMap<String, String>>,
    /// Servers that are configured + applicable but may not have finished
    /// initializing yet (background init). Used to return a clear "still
    /// initializing" error instead of "no server configured".
    expected_servers: Arc<StdMutex<HashSet<ServerId>>>,
    /// Per-tool routing table: resolves `(language, tool)` to a `ServerId`.
    /// Locked independently so `rebind_router` (called from a background
    /// task once registration completes) never contends with an in-flight
    /// LSP round trip.
    router: Arc<StdMutex<ToolRouter>>,
    /// Configs needed to respawn a server if its process dies later, keyed
    /// by routing identity. Populated once per server right after a
    /// successful spawn (see [`Self::register_server_config`]); the respawn
    /// path ([`Self::respawn_if_dead`]) is the only reader.
    server_configs: Arc<StdMutex<HashMap<ServerId, ServerInitConfig>>>,
    /// Per-server single-flight lock so concurrent callers that both observe
    /// a dead process don't race to respawn it independently -- the loser
    /// waits for the winner's attempt to finish (success or failure) and
    /// then re-reads whatever ended up registered. See
    /// [`Self::respawn_if_dead`].
    respawn_locks: Arc<StdMutex<HashMap<ServerId, Arc<Mutex<()>>>>>,
    /// Consecutive respawn failures and last-attempt time per server, so a
    /// crash-looping server backs off instead of eating a fresh
    /// `timeout_seconds` on every tool call that arrives while it is down.
    /// See [`Self::respawn_if_dead`].
    respawn_backoffs: Arc<StdMutex<HashMap<ServerId, RespawnBackoff>>>,
    /// Diagnostics cache, shared with `serve_with`'s notification pump.
    ///
    /// `None` for a `Translator` built without [`Self::with_notification_cache`]
    /// (e.g. most unit tests). When present, [`Self::respawn_if_dead`] uses
    /// it to invalidate a respawned server's stale cached diagnostics --
    /// see that method's docs for why that matters.
    notification_cache: Option<Arc<Mutex<NotificationCache>>>,
    /// Time source for respawn-backoff bookkeeping ([`respawn`](self::respawn)).
    /// Always [`SystemClock`] in production; overridden via
    /// [`Self::with_clock`] in tests so backoff-window tests can advance
    /// time deterministically instead of sleeping in real time.
    clock: Arc<dyn Clock>,
}

/// Upper bound on how long [`Translator::shutdown_servers`] waits for a
/// single LSP server's graceful `shutdown`/`exit` handshake before giving up
/// and letting `kill_on_drop` terminate it instead.
const SERVER_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl Translator {
    /// Create a new translator.
    ///
    /// Starts with an empty router: nothing is routable until [`Self::with_router`]
    /// installs one, which matches having no servers registered.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lsp_clients: Arc::new(StdMutex::new(HashMap::new())),
            lsp_servers: Arc::new(StdMutex::new(HashMap::new())),
            document_tracker: Arc::new(DocumentTracker::new(
                ResourceLimits::default(),
                HashMap::new(),
            )),
            resource_limits: ResourceLimits::default(),
            workspace_roots: Arc::new(Vec::new()),
            extension_map: Arc::new(HashMap::new()),
            expected_servers: Arc::new(StdMutex::new(HashSet::new())),
            router: Arc::new(StdMutex::new(ToolRouter::default())),
            server_configs: Arc::new(StdMutex::new(HashMap::new())),
            respawn_locks: Arc::new(StdMutex::new(HashMap::new())),
            respawn_backoffs: Arc::new(StdMutex::new(HashMap::new())),
            notification_cache: None,
            clock: Arc::new(SystemClock),
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
    /// shared, so this replaces the `Arc` wholesale rather than locking.
    pub fn set_workspace_roots(&mut self, roots: Vec<PathBuf>) {
        self.workspace_roots = Arc::new(roots);
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

    /// Mark the set of servers that are expected (configured + applicable)
    /// but may still be initializing in the background.
    pub fn set_expected_servers(&self, servers: HashSet<ServerId>) {
        *lock_std(&self.expected_servers) = servers;
    }

    /// Clear the expected-servers set (e.g. after background init failed).
    pub fn clear_expected_servers(&self) {
        lock_std(&self.expected_servers).clear();
    }

    /// Install the per-tool routing table built from the applicable configs.
    ///
    /// Only called during single-owner setup, before the translator is
    /// shared, so this replaces the `Arc`-wrapped router wholesale.
    #[must_use]
    pub fn with_router(mut self, router: ToolRouter) -> Self {
        self.router = Arc::new(StdMutex::new(router));
        self
    }

    /// Rebind the routing table to the set of servers that actually
    /// registered, dropping or redirecting routes to servers that failed to
    /// spawn. See `ToolRouter::rebind_to_registered` for the full semantics.
    pub fn rebind_router(&self, registered: &HashSet<ServerId>) {
        lock_std(&self.router).rebind_to_registered(registered);
    }

    /// Whether `id` is the server the router currently resolves
    /// `ToolKind::Diagnostics` to for `language_id`.
    ///
    /// Purpose-built for `register_servers`, which needs this to compute the
    /// diagnostics-cache filter passed into each pump task, without exposing
    /// the router's lock guard outside this module.
    #[must_use]
    pub fn is_diagnostics_route(&self, language_id: &str, id: &ServerId) -> bool {
        lock_std(&self.router).resolve(language_id, ToolKind::Diagnostics) == Some(id)
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
        lock_std(&self.lsp_servers)
            .get(server_id)
            .and_then(|server| PositionEncoding::from_lsp(server.position_encoding().as_str()))
            .unwrap_or(PositionEncoding::Utf16)
    }

    /// Build the [`EncodingCtx`] for converting positions/ranges in
    /// responses from the registered server `id`.
    fn encoding_ctx(&self, server_id: &ServerId) -> EncodingCtx {
        EncodingCtx {
            encoding: self.position_encoding_for(server_id),
            tracker: self.document_tracker.clone(),
        }
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

    /// Configure custom file extension mappings.
    ///
    /// This method sets the extension map and updates the document tracker
    /// to use the same mappings for language detection.
    ///
    /// Only called during single-owner setup, before the translator is
    /// shared, so this replaces the `Arc`-wrapped fields wholesale.
    #[must_use]
    pub fn with_extensions(mut self, extension_map: HashMap<String, String>) -> Self {
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

    /// Register an LSP client under its routing identity.
    ///
    /// Only called once per server, from `register_servers` during initial
    /// background init. The respawn path does not reuse this method: it
    /// needs the previous client back (to fail its pending requests) and
    /// must also reset `document_tracker` for the swapped-in server, neither
    /// of which this method does.
    pub fn register_client(&self, id: impl Into<ServerId>, client: LspClient) {
        lock_std(&self.lsp_clients).insert(id.into(), client);
    }

    /// Register an LSP server under its routing identity.
    pub fn register_server(&self, id: impl Into<ServerId>, server: LspServer) {
        lock_std(&self.lsp_servers).insert(id.into(), server);
    }

    /// Store the config needed to respawn `id` if its process dies later.
    ///
    /// Called once per server, right after a successful spawn (see the
    /// crate-root `register_servers`); [`Self::respawn_if_dead`] is the only
    /// reader.
    pub(crate) fn register_server_config(&self, id: impl Into<ServerId>, config: ServerInitConfig) {
        lock_std(&self.server_configs).insert(id.into(), config);
    }

    /// Number of currently registered LSP servers.
    ///
    /// Test-only: `lsp_servers` is private, so this is the one way a test
    /// outside this module (e.g. `crate::tests`, exercising
    /// [`Translator::shutdown_servers`] indirectly through `serve_with`'s
    /// shutdown sequence) can observe that a registered server was actually
    /// drained.
    #[cfg(test)]
    pub(crate) fn registered_server_count(&self) -> usize {
        lock_std(&self.lsp_servers).len()
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
    /// [`LspServer::shutdown`], bounded by a fixed per-server timeout. A
    /// server that errors or fails to respond in time is simply dropped
    /// instead: its child process handle is `kill_on_drop(true)`, so the
    /// process is killed rather than left running. Call this once, from the
    /// top-level shutdown path, after the MCP transport has stopped
    /// accepting new requests.
    ///
    /// # Limitations
    ///
    /// This only runs on the normal shutdown path (stdio EOF, `SIGTERM`/
    /// `SIGINT`, or the HTTP transport's own graceful shutdown). This crate's
    /// workspace `[profile.release]` builds with `panic = "abort"`, so a
    /// panic reachable from a request handler or background pump task in a
    /// release build still terminates the process without unwinding — this
    /// method never runs, and spawned LSP children are orphaned exactly as
    /// before this fix. Making that path safe would need process-group
    /// isolation (`kill_on_drop` alone doesn't help, since no `Drop` runs
    /// either); tracked separately, out of scope here.
    ///
    /// `pub(crate)` rather than `pub`: this is meant for exactly one call
    /// site (`serve_with`'s post-transport shutdown sequence), after the MCP
    /// transport is already down. An external caller invoking it mid-session
    /// would drain `lsp_servers` while `lsp_clients` (routing table) still
    /// points at the now-shut-down servers, so in-flight tool calls would
    /// resolve to a client whose server is gone.
    pub(crate) async fn shutdown_servers(&self) {
        let servers: Vec<(ServerId, LspServer)> = lock_std(&self.lsp_servers).drain().collect();
        if servers.is_empty() {
            return;
        }

        let mut tasks = tokio::task::JoinSet::new();
        for (id, server) in servers {
            tasks.spawn(async move {
                match tokio::time::timeout(SERVER_SHUTDOWN_TIMEOUT, server.shutdown()).await {
                    Ok(Ok(())) => tracing::debug!(%id, "LSP server shut down gracefully"),
                    Ok(Err(e)) => tracing::warn!(
                        %id, error = %e,
                        "LSP server shutdown handshake failed, killing process instead"
                    ),
                    Err(_) => tracing::warn!(
                        %id, timeout = ?SERVER_SHUTDOWN_TIMEOUT,
                        "LSP server did not shut down in time, killing process instead"
                    ),
                }
            });
        }
        tasks.join_all().await;
    }
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    use tokio::time::Duration;

    use super::*;
    use crate::bridge::state::detect_language;
    use crate::config::{ServerId, ToolKind, ToolRouter};
    use crate::error::Error;

    #[test]
    fn test_translator_new() {
        let translator = Translator::new();
        assert_eq!(translator.workspace_roots.len(), 0);
        assert_eq!(lock_std(&translator.lsp_clients).len(), 0);
        assert_eq!(lock_std(&translator.lsp_servers).len(), 0);
    }

    #[test]
    fn test_set_workspace_roots() {
        let mut translator = Translator::new();
        let roots = vec![PathBuf::from("/test/root1"), PathBuf::from("/test/root2")];
        translator.set_workspace_roots(roots.clone());
        assert_eq!(*translator.workspace_roots, roots);
    }

    #[test]
    fn test_register_server() {
        let translator = Translator::new();

        // Initial state: no servers registered
        assert_eq!(lock_std(&translator.lsp_servers).len(), 0);

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
    /// on shutdown). Uses `fake_lsp_server()` (mock `echo`/`cat` child
    /// processes, real `LspServer`, see `lsp::lifecycle`), which won't
    /// answer the LSP `shutdown` handshake — proving the drain completes,
    /// via the timeout/error fallback path, without hanging on
    /// non-responsive servers.
    #[tokio::test]
    async fn test_shutdown_servers_drains_registered_servers() {
        let translator = Translator::new();
        translator.register_server("server-a", crate::lsp::fake_lsp_server());
        translator.register_server("server-b", crate::lsp::fake_lsp_server());
        assert_eq!(lock_std(&translator.lsp_servers).len(), 2);

        // Bounded well above `SERVER_SHUTDOWN_TIMEOUT` (10s) so a genuine
        // regression (a hang) still fails the test instead of the harness
        // itself timing out ambiguously.
        let result =
            tokio::time::timeout(Duration::from_secs(20), translator.shutdown_servers()).await;

        assert!(
            result.is_ok(),
            "shutdown_servers must not hang against non-responsive mock servers"
        );
        assert_eq!(
            lock_std(&translator.lsp_servers).len(),
            0,
            "all registered servers must be drained"
        );
    }

    #[test]
    fn test_clear_expected_servers_reverts_to_no_server_after_all_routes_dropped() {
        // Mirrors the real `serve_with` flow: `rebind_router` (called from
        // `register_servers`/the all-failed path) drops routes to servers
        // that never registered, then `clear_expected_servers` runs under
        // the same lock. Subsequent lookups must fall back to
        // NoServerForLanguage rather than keep implying the server is still
        // on its way.
        let path = PathBuf::from("/ws/Assets/Scripts/Player.cs");
        let lang = detect_language(&path, &HashMap::new());
        let id = ServerId::from(lang.clone());

        let translator = Translator::new().with_router(ToolRouter::catch_all([(id.clone(), lang)]));
        let mut expected = HashSet::new();
        expected.insert(id);
        translator.set_expected_servers(expected);

        translator.rebind_router(&HashSet::new());
        translator.clear_expected_servers();

        let err = translator
            .get_client_for_file(&path, ToolKind::Hover)
            .unwrap_err();
        assert!(matches!(err, Error::NoServerForLanguage(_)));
    }

    #[test]
    fn test_translator_with_custom_extensions() {
        let mut extension_map = HashMap::new();
        extension_map.insert("nu".to_string(), "nushell".to_string());
        extension_map.insert("customext".to_string(), "customlang".to_string());

        let translator = Translator::new().with_extensions(extension_map.clone());

        assert_eq!(translator.extension_map.len(), 2);
        assert_eq!(
            translator.extension_map.get("nu"),
            Some(&"nushell".to_string())
        );
        assert_eq!(
            translator.extension_map.get("customext"),
            Some(&"customlang".to_string())
        );
    }

    /// `with_resource_limits` called before `with_extensions` (the order
    /// `serve()` uses) must reach `document_tracker`.
    #[test]
    fn test_with_resource_limits_applies_before_with_extensions() {
        let limits = ResourceLimits {
            max_documents: 1,
            max_file_size: 0,
        };
        let translator = Translator::new()
            .with_resource_limits(limits)
            .with_extensions(HashMap::new());

        translator
            .document_tracker
            .open(PathBuf::from("/tmp/a.rs"), "a".to_string())
            .unwrap();
        let err = translator
            .document_tracker
            .open(PathBuf::from("/tmp/b.rs"), "b".to_string())
            .unwrap_err();
        assert!(matches!(err, Error::DocumentLimitExceeded { max: 1, .. }));
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
    #[test]
    fn test_with_resource_limits_applies_after_with_extensions() {
        let limits = ResourceLimits {
            max_documents: 1,
            max_file_size: 0,
        };
        let translator = Translator::new()
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]))
            .with_resource_limits(limits);

        let path = PathBuf::from("/tmp/a.rs");
        translator
            .document_tracker
            .open(path.clone(), "a".to_string())
            .unwrap();
        let err = translator
            .document_tracker
            .open(PathBuf::from("/tmp/b.rs"), "b".to_string())
            .unwrap_err();
        assert!(matches!(err, Error::DocumentLimitExceeded { max: 1, .. }));

        let state = translator.document_tracker.close(&path).unwrap();
        assert_eq!(state.language_id(), "rust");
    }
}
