//! Per-session resource-subscription state and `resources/updated` delivery.
//!
//! Every `McplsServer` instance owns one [`SessionHandle`]. The handle's only
//! way to mutate subscription state is [`SessionHandle::require_stateful`],
//! which rejects requests rmcp served over its stateless per-request HTTP path
//! (#482) and otherwise returns a [`StatefulSession`] capability token (#492).
//!
//! Delivery is per session: the diagnostics pump asks every registered
//! [`SessionState`] to [`publish_if_subscribed`](SessionState::publish_if_subscribed),
//! which records the URI in that session's own coalescing pending set and
//! rings a capacity-1 doorbell. A per-session task drains the set and notifies
//! the session's peer, so a stalled client only ever delays itself.
//!
//! Sessions enter the [`SubscriptionRegistry`] lazily, when their first
//! guarded subscribe binds delivery, so stateless per-request instances can
//! never become a delivery target.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};

use rmcp::model::{ErrorCode, ResourceUpdatedNotificationParam};
use rmcp::service::{RequestContext, SubscriptionSink};
use rmcp::{ErrorData as McpError, Peer, RoleServer};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tracing::{debug, warn};

use crate::bridge::resources::{
    DiagnosticsResourceUri, MAX_LISTEN_STREAMS, MAX_SUBSCRIPTIONS, ResourceSubscriptions,
    SubscriptionError, parse_uri,
};
use crate::bridge::{WorkspaceRoots, lock_std};

/// Whether `meta` carries rmcp's discover-lifecycle keys -- the same test
/// `tower.rs::is_legacy_request` uses to route a request through its
/// stateless per-request HTTP path instead of a durable session (#482). An
/// attached `Mcp-Session-Id` header proves nothing here: rmcp never reads it
/// on that path, so it must not be trusted as a counter-signal.
#[cfg(feature = "transport-http")]
fn request_uses_discover_lifecycle_meta(meta: &rmcp::model::RequestMetaObject) -> bool {
    meta.missing_required_keys(&rmcp::model::ProtocolVersion::V_2026_07_28)
        .is_empty()
}

/// Whether this HTTP-served request must be rejected as effectively
/// stateless (#482): its `_meta` matches [`request_uses_discover_lifecycle_meta`],
/// or it never echoes an `Mcp-Session-Id` header. Gated on the `Parts`
/// extension being present so a non-HTTP transport (stdio) is never affected.
#[cfg(feature = "transport-http")]
fn is_stateless_http_request(
    extensions: &rmcp::model::Extensions,
    meta: &rmcp::model::RequestMetaObject,
) -> bool {
    extensions
        .get::<axum::http::request::Parts>()
        .is_some_and(|parts| {
            request_uses_discover_lifecycle_meta(meta)
                || !parts
                    .headers
                    .contains_key(rmcp::transport::common::http_header::HEADER_SESSION_ID)
        })
}

#[cfg(not(feature = "transport-http"))]
const fn is_stateless_http_request(
    _extensions: &rmcp::model::Extensions,
    _meta: &rmcp::model::RequestMetaObject,
) -> bool {
    false
}

/// Reject a subscription request rmcp served over the stateless per-request
/// HTTP path (#482): the state it would write is dropped the moment the
/// request completes, so this surfaces an explicit error instead of a
/// silent no-op.
fn reject_if_stateless_http(context: &RequestContext<RoleServer>) -> Result<(), McpError> {
    if is_stateless_http_request(&context.extensions, &context.meta) {
        return Err(McpError::new(
            ErrorCode(crate::error::STATELESS_SUBSCRIPTION_ERROR_CODE),
            "resource subscriptions require a stateful session; this request was served over \
             the stateless per-request HTTP path, which never persists a subscription past the \
             response that acknowledges it -- retry over a session established via the MCP \
             `initialize` handshake, and without per-request `_meta` protocol negotiation"
                .to_string(),
            None,
        ));
    }
    Ok(())
}

/// The delivery task's peer disappeared (transport closed).
#[derive(Debug)]
struct TargetClosed;

/// Where a session's delivery task sends `resources/updated` URIs.
#[derive(Debug)]
pub enum Target {
    /// The session's real MCP peer.
    Peer(Peer<RoleServer>),
    /// A `subscriptions/listen` stream: one notification per raw URI the
    /// client listed for the published resource.
    Sink {
        /// Filter-enforcing sink of the listen request.
        sink: SubscriptionSink,
        /// Maps each canonical URI to the raw URIs the client asked for.
        uris: Arc<ListenUris>,
    },
    /// In-memory sink so tests run the production coalescing loop.
    #[cfg(test)]
    Channel(mpsc::Sender<String>),
}

impl Target {
    async fn send(&self, uri: &DiagnosticsResourceUri) -> Result<(), TargetClosed> {
        match self {
            Self::Peer(peer) => peer
                .notify_resource_updated(ResourceUpdatedNotificationParam::new(uri.as_str()))
                .await
                .map_err(|e| {
                    debug!("peer closed while delivering resources/updated: {e}");
                    TargetClosed
                }),
            Self::Sink { sink, uris } => {
                for raw in uris.raw_for(uri) {
                    match sink.notify_resource_updated(raw).await {
                        Ok(()) => {}
                        Err(rmcp::service::SubscriptionSendError::NotificationNotAccepted(_)) => {
                            warn!("listen sink rejected {raw}: not in its accepted set");
                        }
                        Err(e) => {
                            debug!("listen stream closed while delivering resources/updated: {e}");
                            return Err(TargetClosed);
                        }
                    }
                }
                Ok(())
            }
            #[cfg(test)]
            Self::Channel(tx) => tx
                .send(uri.as_str().to_owned())
                .await
                .map_err(|_| TargetClosed),
        }
    }
}

/// Coalescing pending-URI set plus a capacity-1 doorbell for one session.
///
/// A full doorbell means the delivery task is already woken, so the latest
/// update per URI is never lost and the publisher never awaits the peer.
#[derive(Debug)]
struct Delivery {
    pending: Arc<StdMutex<HashSet<DiagnosticsResourceUri>>>,
    doorbell: mpsc::Sender<()>,
    cap_warned: AtomicBool,
}

/// Drains `pending` on every doorbell ring and forwards each URI to `target`.
///
/// Exits when the doorbell closes (the owning [`SessionState`] was dropped) or
/// the first send fails (the transport is gone).
async fn run_delivery(
    target: Target,
    pending: Arc<StdMutex<HashSet<DiagnosticsResourceUri>>>,
    mut doorbell: mpsc::Receiver<()>,
) {
    while doorbell.recv().await.is_some() {
        let batch = std::mem::take(&mut *lock_std(&pending));
        for uri in batch {
            if target.send(&uri).await.is_err() {
                return;
            }
        }
    }
}

/// Subscription set and delivery channel of one MCP session.
#[derive(Debug, Default)]
pub struct SessionState {
    subs: ResourceSubscriptions,
    delivery: OnceLock<Delivery>,
}

impl SessionState {
    /// Whether this session has no subscriptions.
    pub(crate) async fn is_empty(&self) -> bool {
        self.subs.is_empty().await
    }

    /// Queues `uri` for delivery to this session only if it is subscribed to it.
    pub(crate) async fn publish_if_subscribed(&self, uri: &DiagnosticsResourceUri) {
        if !self.subs.contains(uri).await {
            return;
        }
        let Some(delivery) = self.delivery.get() else {
            debug!("subscribed session has no bound delivery, dropping update for {uri}");
            return;
        };
        {
            let mut pending = lock_std(&delivery.pending);
            if !pending.contains(uri) {
                if pending.len() >= MAX_SUBSCRIPTIONS {
                    if delivery.cap_warned.swap(true, Ordering::Relaxed) {
                        debug!("pending resource updates at cap, dropping {uri}");
                    } else {
                        warn!(
                            "pending resource updates reached {MAX_SUBSCRIPTIONS}, dropping {uri} \
                             (further drops for this session are logged at debug)"
                        );
                    }
                    return;
                }
                pending.insert(uri.clone());
            }
        }
        match delivery.doorbell.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Closed(())) => {
                debug!("delivery task gone, dropping update for {uri}");
            }
        }
    }

    /// Queues every subscribed URI for which `pred` holds.
    ///
    /// `pred` runs on a snapshot with no subscription lock held, so it may
    /// take other locks.
    async fn publish_matching(&self, pred: &(impl Fn(&DiagnosticsResourceUri) -> bool + Sync)) {
        for uri in self.subs.snapshot().await {
            if pred(&uri) {
                self.publish_if_subscribed(&uri).await;
            }
        }
    }

    async fn subscribe(
        &self,
        canonical: DiagnosticsResourceUri,
        raw: String,
    ) -> Result<bool, SubscriptionError> {
        let newly_subscribed = self.subs.subscribe(canonical.clone()).await?;
        self.subs.record_alias(raw, &canonical).await;
        Ok(newly_subscribed)
    }

    async fn unsubscribe(&self, canonical: Option<&DiagnosticsResourceUri>, raw: &str) -> bool {
        let Some(canonical) = self.subs.unsubscribe(canonical, raw).await else {
            return false;
        };
        if let Some(delivery) = self.delivery.get() {
            lock_std(&delivery.pending).remove(&canonical);
        }
        true
    }
}

/// The resource URIs of one `subscriptions/listen` request, resolved against
/// the workspace roots.
///
/// Immutable once built: the canonical subscription set and the raw URIs
/// echoed back to the client are both derived from it, so they cannot
/// disagree.
#[derive(Debug)]
pub struct ListenUris(HashMap<DiagnosticsResourceUri, ListenEntry>);

#[derive(Debug)]
struct ListenEntry {
    raw: BTreeSet<String>,
    lsp_uri: lsp_types::Uri,
}

/// Upper bound on the summed byte length of the URIs one listen request may
/// name, keeping rmcp's quadratic filter intersections cheap however long
/// each URI is.
const MAX_LISTEN_REQUEST_BYTES: usize = 256 * 1024;

impl ListenUris {
    /// The subset of `requested` worth acknowledging: deduplicated, and
    /// syntactically valid `lsp-diagnostics:///` URIs. Touches no filesystem.
    ///
    /// Returns `None` when `requested` exceeds the size budget, so rmcp's
    /// quadratic intersection never sees an oversized list.
    pub(crate) fn syntactic_filter(requested: &[String]) -> Option<Vec<String>> {
        if Self::exceeds_budget(requested) {
            return None;
        }
        let mut seen = HashSet::new();
        Some(
            requested
                .iter()
                .filter(|raw| seen.insert(raw.as_str()) && parse_uri(raw).is_ok())
                .cloned()
                .collect(),
        )
    }

    /// Whether `requested` names more than [`MAX_SUBSCRIPTIONS`] URIs or more
    /// than [`MAX_LISTEN_REQUEST_BYTES`] bytes of them.
    pub(crate) fn exceeds_budget(requested: &[String]) -> bool {
        requested.len() > MAX_SUBSCRIPTIONS
            || requested.iter().map(String::len).sum::<usize>() > MAX_LISTEN_REQUEST_BYTES
    }

    /// Resolves the acknowledged `accepted` URIs, grouping raw spellings of
    /// one file under its canonical URI. URIs that no longer resolve inside
    /// `roots` (the acknowledgment is advisory) are dropped.
    ///
    /// Touches the filesystem; call from a blocking context.
    pub(crate) fn resolve(accepted: &[String], roots: &WorkspaceRoots) -> Self {
        let mut map: HashMap<DiagnosticsResourceUri, ListenEntry> = HashMap::new();
        let mut dropped = 0_usize;
        for raw in accepted {
            let resolved = DiagnosticsResourceUri::resolve(raw, roots).ok();
            let Some((resolved, lsp_uri)) = resolved.and_then(|resolved| {
                let lsp_uri = crate::bridge::try_path_to_uri(&resolved.path)?;
                Some((resolved, lsp_uri))
            }) else {
                dropped = dropped.saturating_add(1);
                continue;
            };
            map.entry(resolved.uri)
                .or_insert_with(|| ListenEntry {
                    raw: BTreeSet::new(),
                    lsp_uri,
                })
                .raw
                .insert(raw.clone());
        }
        if dropped > 0 {
            debug!("subscriptions/listen dropped {dropped} URIs that no longer resolve");
        }
        Self(map)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn raw_for(&self, uri: &DiagnosticsResourceUri) -> impl Iterator<Item = &str> {
        self.0
            .get(uri)
            .into_iter()
            .flat_map(|entry| entry.raw.iter().map(String::as_str))
    }

    /// Each canonical URI with the LSP URI of its file, for cache lookups.
    pub(crate) fn canonical(
        &self,
    ) -> impl Iterator<Item = (&DiagnosticsResourceUri, &lsp_types::Uri)> {
        self.0.iter().map(|(uri, entry)| (uri, &entry.lsp_uri))
    }
}

#[derive(Debug)]
struct RegistryInner {
    sessions: StdMutex<Vec<Weak<SessionState>>>,
    listen_permits: Arc<Semaphore>,
}

/// Tracks every session that has bound delivery, for the diagnostics pump.
///
/// Holds only [`Weak`] references, so a session becomes reclaimable the moment
/// its `McplsServer` is dropped; dead entries are pruned on the next
/// registration or snapshot (GC-on-next-use, not synchronous with close).
///
/// A session registers lazily, on its first guarded subscribe. Instances rmcp
/// builds per request on its stateless HTTP path (#482) never subscribe and so
/// never register, which keeps this bounded under any amount of stateless
/// traffic. The one exception is a `subscriptions/listen` request, which
/// registers request-scoped state for exactly the lifetime of the stream; the
/// number of such streams is capped at `MAX_LISTEN_STREAMS` (100), shared by
/// all transports and independent of `max_concurrent_sessions`.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
///
/// use mcpls_core::bridge::{NotificationCache, Translator, WorkspaceRoots};
/// use mcpls_core::config::McpConfig;
/// use mcpls_core::mcp::{McplsServer, SubscriptionRegistry};
/// use tokio::sync::Mutex;
///
/// // Clones share one registry; sessions built from the server join it
/// // on their first subscribe.
/// let registry = SubscriptionRegistry::new();
/// let server = McplsServer::new(
///     Arc::new(Translator::new()),
///     Arc::new(Mutex::new(NotificationCache::new())),
///     WorkspaceRoots::default(),
///     registry.clone(),
///     false,
///     McpConfig::default(),
/// );
/// let _session = server.for_new_session();
/// ```
#[derive(Debug, Clone)]
pub struct SubscriptionRegistry(Arc<RegistryInner>);

impl Default for SubscriptionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SubscriptionRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(RegistryInner {
            sessions: StdMutex::new(Vec::new()),
            listen_permits: Arc::new(Semaphore::new(MAX_LISTEN_STREAMS)),
        }))
    }

    fn register(&self, state: &Arc<SessionState>) {
        let mut guard = lock_std(&self.0.sessions);
        guard.retain(|weak| weak.strong_count() > 0);
        guard.push(Arc::downgrade(state));
    }

    /// Upgrade every still-live entry, pruning dead ones along the way.
    pub(crate) fn live_sessions(&self) -> Vec<Arc<SessionState>> {
        let mut guard = lock_std(&self.0.sessions);
        guard.retain(|weak| weak.strong_count() > 0);
        guard.iter().filter_map(Weak::upgrade).collect()
    }

    /// Queues, on every live session, each subscribed URI for which `pred`
    /// holds.
    ///
    /// Used once initialization settles, to tell clients subscribed to a
    /// file whose server failed to start that a re-read now returns the
    /// error. Delivery stays per session and never awaits a peer.
    pub(crate) async fn publish_matching(
        &self,
        pred: impl Fn(&DiagnosticsResourceUri) -> bool + Sync,
    ) {
        for session in self.live_sessions() {
            session.publish_matching(&pred).await;
        }
    }

    /// Reserves one of the [`MAX_LISTEN_STREAMS`] listen slots.
    ///
    /// # Errors
    ///
    /// Returns [`SubscriptionError::ListenLimitReached`] when all are taken.
    pub(crate) fn try_reserve_listen(&self) -> Result<ListenPermit, SubscriptionError> {
        let permit = Arc::clone(&self.0.listen_permits)
            .try_acquire_owned()
            .map_err(|_| SubscriptionError::ListenLimitReached)?;
        Ok(ListenPermit {
            registry: self.clone(),
            permit,
        })
    }

    /// Raw entry count, dead or alive, without pruning.
    ///
    /// Test-only and deliberately non-pruning: a pruning read would mask the
    /// growth regression this exists to catch.
    #[cfg(test)]
    pub(crate) fn raw_len(&self) -> usize {
        lock_std(&self.0.sessions).len()
    }
}

/// A reserved listen slot, held before any filesystem work so a flood of
/// listen requests cannot trigger unbounded canonicalization.
#[derive(Debug)]
pub struct ListenPermit {
    registry: SubscriptionRegistry,
    permit: OwnedSemaphorePermit,
}

impl ListenPermit {
    /// Registers request-scoped delivery for `uris` and ties the slot to the
    /// returned registration.
    ///
    /// `target` receives the same `uris` the subscription set is derived
    /// from, so the delivery map and the set cannot diverge.
    pub(crate) fn register(
        self,
        uris: Arc<ListenUris>,
        target: impl FnOnce(Arc<ListenUris>) -> Target,
    ) -> ListenRegistration {
        let state = Arc::new(SessionState {
            subs: ResourceSubscriptions::from_canonical(
                uris.canonical().map(|(uri, _)| uri.clone()),
            ),
            delivery: OnceLock::new(),
        });
        bind_delivery(&state, &self.registry, || target(uris));
        ListenRegistration {
            state,
            _permit: self.permit,
        }
    }
}

/// Live delivery of one `subscriptions/listen` stream; dropping it stops
/// delivery and frees the listen slot.
#[derive(Debug)]
pub struct ListenRegistration {
    state: Arc<SessionState>,
    _permit: OwnedSemaphorePermit,
}

impl ListenRegistration {
    /// Queues `uri` for delivery, e.g. to replay already cached diagnostics.
    pub(crate) async fn publish(&self, uri: &DiagnosticsResourceUri) {
        self.state.publish_if_subscribed(uri).await;
    }
}

/// Creates `state`'s delivery task and registers it, exactly once.
fn bind_delivery(
    state: &Arc<SessionState>,
    registry: &SubscriptionRegistry,
    target: impl FnOnce() -> Target,
) {
    state.delivery.get_or_init(|| {
        let (doorbell, rx) = mpsc::channel(1);
        let pending = Arc::new(StdMutex::new(HashSet::new()));
        tokio::spawn(run_delivery(target(), Arc::clone(&pending), rx));
        registry.register(state);
        Delivery {
            pending,
            doorbell,
            cap_warned: AtomicBool::new(false),
        }
    });
}

/// Owner of one session's [`SessionState`].
#[derive(Debug)]
pub struct SessionHandle {
    state: Arc<SessionState>,
    registry: SubscriptionRegistry,
}

impl SessionHandle {
    /// A handle with fresh, unregistered state.
    pub(crate) fn new(registry: SubscriptionRegistry) -> Self {
        Self {
            state: Arc::new(SessionState::default()),
            registry,
        }
    }

    /// A handle for a new session sharing this one's registry.
    pub(crate) fn sibling(&self) -> Self {
        Self::new(self.registry.clone())
    }

    pub(crate) fn registry(&self) -> SubscriptionRegistry {
        self.registry.clone()
    }

    #[cfg(test)]
    pub(crate) const fn state(&self) -> &Arc<SessionState> {
        &self.state
    }

    /// Rejects stateless HTTP requests and otherwise hands out the capability
    /// needed to mutate this session's subscriptions.
    ///
    /// # Errors
    ///
    /// Returns the stateless-subscription MCP error (#482) when rmcp served
    /// the request over its stateless per-request path.
    pub(crate) fn require_stateful(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<StatefulSession<'_>, McpError> {
        reject_if_stateless_http(context)?;
        Ok(StatefulSession {
            handle: self,
            peer: context.peer.clone(),
        })
    }

    fn ensure_delivery(&self, target: impl FnOnce() -> Target) {
        bind_delivery(&self.state, &self.registry, target);
    }

    /// Subscribes `canonical` through the production binding path with a
    /// custom delivery target.
    #[cfg(test)]
    pub(crate) async fn subscribe_for_test(
        &self,
        canonical: &DiagnosticsResourceUri,
        target: Target,
    ) -> Result<bool, SubscriptionError> {
        self.ensure_delivery(|| target);
        self.state.subs.subscribe(canonical.clone()).await
    }
}

/// Capability token proving a request came from a stateful session.
///
/// Only obtainable via [`SessionHandle::require_stateful`], and the only place
/// subscription mutators exist.
#[derive(Debug)]
pub struct StatefulSession<'a> {
    handle: &'a SessionHandle,
    peer: Peer<RoleServer>,
}

impl StatefulSession<'_> {
    /// Records a subscription to `canonical` (reachable from the client as
    /// `raw`), binding delivery and registering the session first.
    ///
    /// Returns whether the URI was newly subscribed.
    pub(crate) async fn subscribe(
        &self,
        canonical: DiagnosticsResourceUri,
        raw: String,
    ) -> Result<bool, SubscriptionError> {
        self.handle
            .ensure_delivery(|| Target::Peer(self.peer.clone()));
        self.handle.state.subscribe(canonical, raw).await
    }

    /// Removes the subscription `canonical` or the recorded alias `raw`
    /// resolves to, also dropping it from the pending set. Returns whether
    /// one was found.
    ///
    /// Two benign races can still deliver one stale `resources/updated` for
    /// the URI to this same session afterwards: a publish that passed the
    /// subscription check before this call, and a batch the delivery task
    /// already took. Neither crosses sessions.
    pub(crate) async fn unsubscribe(
        &self,
        canonical: Option<&DiagnosticsResourceUri>,
        raw: &str,
    ) -> bool {
        self.handle.state.unsubscribe(canonical, raw).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A request with no `http::request::Parts` extension at all (e.g. served
    /// over stdio) is never mistaken for a stateless HTTP request, even with
    /// an empty `_meta`.
    #[test]
    fn test_is_stateless_http_request_false_without_http_extension() {
        let extensions = rmcp::model::Extensions::new();
        let meta = rmcp::model::RequestMetaObject::new();
        assert!(!is_stateless_http_request(&extensions, &meta));
    }

    /// #482 regression: a stdio request (no `Parts` extension) whose `_meta`
    /// carries discover-lifecycle keys must still be allowed through -- see
    /// [`is_stateless_http_request`]'s docs for why.
    #[test]
    fn test_is_stateless_http_request_false_without_http_extension_even_with_discover_meta() {
        let extensions = rmcp::model::Extensions::new();
        let meta = rmcp::model::RequestMetaObject::with_client_context(
            rmcp::model::ProtocolVersion::V_2025_03_26,
            rmcp::model::Implementation::default(),
            rmcp::model::ClientCapabilities::default(),
        );
        assert!(!is_stateless_http_request(&extensions, &meta));
    }

    /// #482: an HTTP-served request that never echoes `Mcp-Session-Id` is
    /// detected as stateless -- the secondary, unioned signal (see
    /// [`request_uses_discover_lifecycle_meta`] for the primary,
    /// exhaustive one).
    #[cfg(feature = "transport-http")]
    #[test]
    fn test_is_stateless_http_request_true_without_session_header() {
        let (parts, ()) = axum::http::Request::builder()
            .body(())
            .unwrap()
            .into_parts();
        let mut extensions = rmcp::model::Extensions::new();
        extensions.insert(parts);
        let meta = rmcp::model::RequestMetaObject::new();
        assert!(is_stateless_http_request(&extensions, &meta));
    }

    /// A request that echoes an `Mcp-Session-Id` header and carries no
    /// discover-lifecycle `_meta` is not flagged -- the ordinary legacy
    /// session case.
    #[cfg(feature = "transport-http")]
    #[test]
    fn test_is_stateless_http_request_false_with_session_header_and_no_discover_meta() {
        let (mut parts, ()) = axum::http::Request::builder()
            .body(())
            .unwrap()
            .into_parts();
        parts.headers.insert(
            rmcp::transport::common::http_header::HEADER_SESSION_ID,
            axum::http::HeaderValue::from_static("test-session-id"),
        );
        let mut extensions = rmcp::model::Extensions::new();
        extensions.insert(parts);
        let meta = rmcp::model::RequestMetaObject::new();
        assert!(!is_stateless_http_request(&extensions, &meta));
    }

    /// A request that echoes an `Mcp-Session-Id` header is still flagged if
    /// its `_meta` carries discover-lifecycle keys -- proving the session
    /// header alone is *not* a sufficient counter-signal: rmcp never
    /// validates that header on the stateless branch #482 targets, so a
    /// request can carry one (fabricated, stale, or even genuinely live)
    /// while still being served statelessly.
    #[cfg(feature = "transport-http")]
    #[test]
    fn test_is_stateless_http_request_true_with_session_header_and_discover_meta() {
        let (mut parts, ()) = axum::http::Request::builder()
            .body(())
            .unwrap()
            .into_parts();
        parts.headers.insert(
            rmcp::transport::common::http_header::HEADER_SESSION_ID,
            axum::http::HeaderValue::from_static("test-session-id"),
        );
        let mut extensions = rmcp::model::Extensions::new();
        extensions.insert(parts);
        let meta = rmcp::model::RequestMetaObject::with_client_context(
            rmcp::model::ProtocolVersion::V_2025_03_26,
            rmcp::model::Implementation::default(),
            rmcp::model::ClientCapabilities::default(),
        );
        assert!(is_stateless_http_request(&extensions, &meta));
    }

    /// #482 primary signal: `_meta` carrying both discover-lifecycle keys
    /// (`protocolVersion` + `clientCapabilities`) is detected regardless of
    /// the declared protocol version's value -- mirroring rmcp's own
    /// `missing_required_keys`, which only checks presence.
    #[cfg(feature = "transport-http")]
    #[test]
    fn test_request_uses_discover_lifecycle_meta_true_with_both_keys_present() {
        let meta = rmcp::model::RequestMetaObject::with_client_context(
            rmcp::model::ProtocolVersion::V_2025_03_26,
            rmcp::model::Implementation::default(),
            rmcp::model::ClientCapabilities::default(),
        );
        assert!(request_uses_discover_lifecycle_meta(&meta));
    }

    /// Only one of the two required keys present is not enough -- matching
    /// rmcp's own `missing_required_keys`, which requires both.
    #[cfg(feature = "transport-http")]
    #[test]
    fn test_request_uses_discover_lifecycle_meta_false_with_only_one_key() {
        let mut meta = rmcp::model::RequestMetaObject::new();
        meta.set_protocol_version(rmcp::model::ProtocolVersion::V_2025_03_26);
        assert!(!request_uses_discover_lifecycle_meta(&meta));
    }

    /// Empty `_meta` (typical for a legacy session's ordinary request, which
    /// relies on the session's own handshake state instead of per-request
    /// metadata) is not flagged.
    #[cfg(feature = "transport-http")]
    #[test]
    fn test_request_uses_discover_lifecycle_meta_false_when_empty() {
        let meta = rmcp::model::RequestMetaObject::new();
        assert!(!request_uses_discover_lifecycle_meta(&meta));
    }

    #[test]
    fn test_registry_clones_share_entries() {
        let registry = SubscriptionRegistry::new();
        let clone = registry.clone();
        let state = Arc::new(SessionState::default());
        clone.register(&state);
        assert_eq!(registry.live_sessions().len(), 1);
    }

    #[test]
    fn test_registry_prunes_dead_entries_on_register() {
        let registry = SubscriptionRegistry::new();
        for _ in 0..10 {
            registry.register(&Arc::new(SessionState::default()));
        }
        assert_eq!(registry.raw_len(), 1);
    }

    #[test]
    fn test_unbound_handle_is_not_registered() {
        let handle = SessionHandle::new(SubscriptionRegistry::new());
        assert!(handle.registry().live_sessions().is_empty());
    }

    #[tokio::test]
    async fn test_unsubscribe_via_alias_purges_canonical_from_pending() {
        let handle = SessionHandle::new(SubscriptionRegistry::new());
        let (tx, _rx) = mpsc::channel(1);
        let canonical = DiagnosticsResourceUri::for_test("lsp-diagnostics:///private/a.rs");
        handle
            .subscribe_for_test(&canonical, Target::Channel(tx))
            .await
            .unwrap();
        let state = &handle.state;
        state
            .subs
            .record_alias("lsp-diagnostics:///a.rs".to_owned(), &canonical)
            .await;
        let delivery = state.delivery.get().unwrap();
        lock_std(&delivery.pending).insert(canonical);

        assert!(state.unsubscribe(None, "lsp-diagnostics:///a.rs").await);

        assert!(lock_std(&delivery.pending).is_empty());
        assert!(!state.unsubscribe(None, "lsp-diagnostics:///a.rs").await);
    }

    #[tokio::test]
    async fn test_subscribe_records_alias_for_later_unsubscribe() {
        let state = SessionState::default();
        let canonical = DiagnosticsResourceUri::for_test("lsp-diagnostics:///private/a.rs");
        let raw = "lsp-diagnostics:///a.rs".to_owned();
        assert!(
            state
                .subscribe(canonical.clone(), raw.clone())
                .await
                .unwrap()
        );
        assert!(!state.subscribe(canonical, raw.clone()).await.unwrap());
        assert!(state.unsubscribe(None, &raw).await);
        assert!(state.is_empty().await);
    }

    #[tokio::test]
    async fn test_pending_cap_warns_and_drops_new_uris() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let handle = SessionHandle::new(SubscriptionRegistry::new());
        let (tx, _rx) = mpsc::channel(1);
        let x = DiagnosticsResourceUri::for_test("lsp-diagnostics:///x.rs");
        handle
            .subscribe_for_test(&x, Target::Channel(tx))
            .await
            .unwrap();
        let delivery = handle.state.delivery.get().unwrap();
        lock_std(&delivery.pending).extend(
            (0..MAX_SUBSCRIPTIONS)
                .map(|i| DiagnosticsResourceUri::for_test(&format!("lsp-diagnostics:///p{i}.rs"))),
        );

        let captured = crate::test_lsp::CapturedLogs::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
        handle.state.publish_if_subscribed(&x).await;
        drop(guard);

        assert_eq!(lock_std(&delivery.pending).len(), MAX_SUBSCRIPTIONS);
        assert!(!lock_std(&delivery.pending).contains(&x));
        assert!(
            captured
                .messages()
                .iter()
                .any(|m| m.contains("dropping lsp-diagnostics:///x.rs")),
            "dropping at the pending cap must be logged"
        );
    }

    #[tokio::test]
    async fn test_delivery_task_exits_when_session_is_dropped() {
        let handle = SessionHandle::new(SubscriptionRegistry::new());
        let registry = handle.registry();
        let (tx, mut rx) = mpsc::channel(1);
        handle
            .subscribe_for_test(
                &DiagnosticsResourceUri::for_test("lsp-diagnostics:///a.rs"),
                Target::Channel(tx),
            )
            .await
            .unwrap();
        assert_eq!(registry.live_sessions().len(), 1);

        drop(handle);

        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("delivery task did not exit after the session was dropped"));
        assert!(closed.is_none());
        assert!(registry.live_sessions().is_empty());
    }

    /// `publish_matching` publishes only subscribed URIs the predicate selects,
    /// on every live session.
    #[tokio::test]
    async fn test_publish_matching_publishes_only_selected_subscribed_uris() {
        let registry = SubscriptionRegistry::new();
        let failed = DiagnosticsResourceUri::for_test("lsp-diagnostics:///failed.rs");
        let healthy = DiagnosticsResourceUri::for_test("lsp-diagnostics:///healthy.rs");
        let mut sessions = Vec::new();
        for _ in 0..2 {
            let handle = SessionHandle::new(registry.clone());
            let (tx, rx) = mpsc::channel(4);
            handle
                .subscribe_for_test(&failed, Target::Channel(tx))
                .await
                .unwrap();
            handle.state.subs.subscribe(healthy.clone()).await.unwrap();
            sessions.push((handle, rx));
        }

        registry.publish_matching(|uri| *uri == failed).await;

        for (_handle, rx) in &mut sessions {
            let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(got, failed.as_str());
            assert!(rx.try_recv().is_err(), "the healthy URI must not publish");
        }
    }

    /// The predicate runs with no subscription lock held: taking the write
    /// lock from inside it completes immediately instead of deadlocking.
    #[tokio::test]
    async fn test_publish_matching_predicate_runs_without_subscription_lock() {
        use futures::FutureExt as _;

        let handle = SessionHandle::new(SubscriptionRegistry::new());
        let registry = handle.registry();
        let uri = DiagnosticsResourceUri::for_test("lsp-diagnostics:///a.rs");
        let (tx, _rx) = mpsc::channel(4);
        handle
            .subscribe_for_test(&uri, Target::Channel(tx))
            .await
            .unwrap();
        let state = Arc::clone(handle.state());
        let other = DiagnosticsResourceUri::for_test("lsp-diagnostics:///b.rs");

        registry
            .publish_matching(|_| {
                assert!(
                    state.subs.subscribe(other.clone()).now_or_never().is_some(),
                    "the subscription lock must not be held while the predicate runs"
                );
                true
            })
            .await;
    }

    /// With the peer not reading, repeated publishes of one URI collapse into
    /// at most one buffered, one in-flight and one pending delivery.
    #[tokio::test]
    async fn test_same_uri_publishes_coalesce_while_peer_is_stalled() {
        const PUBLISHES: usize = 20;

        let handle = SessionHandle::new(SubscriptionRegistry::new());
        let uri = DiagnosticsResourceUri::for_test("lsp-diagnostics:///a.rs");
        let (tx, mut rx) = mpsc::channel(1);
        handle
            .subscribe_for_test(&uri, Target::Channel(tx))
            .await
            .unwrap();
        for _ in 0..PUBLISHES {
            handle.state.publish_if_subscribed(&uri).await;
            tokio::task::yield_now().await;
        }

        let mut delivered = 0;
        while let Ok(Some(received)) =
            tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await
        {
            assert_eq!(received, uri.as_str());
            delivered += 1;
        }
        assert!(
            (1..=3).contains(&delivered),
            "delivered {delivered} updates"
        );
    }

    use crate::test_lsp::{diagnostics_uri as u, workspace_with_main_rs as workspace_file};

    #[test]
    fn test_syntactic_filter_dedupes_and_drops_invalid_keeping_raw_verbatim() {
        let plain = crate::test_lsp::absolute_uri("a.rs");
        let encoded = plain.replace("a.rs", "%61.rs");
        let requested = vec![
            plain.clone(),
            encoded.clone(),
            plain.clone(),
            "file:///b.rs".to_owned(),
            "lsp-diagnostics://host/c.rs".to_owned(),
        ];
        assert_eq!(
            ListenUris::syntactic_filter(&requested),
            Some(vec![plain, encoded])
        );
    }

    #[test]
    fn test_syntactic_filter_rejects_oversized_list_without_inspecting_it() {
        let requested = vec!["lsp-diagnostics:///a.rs".to_owned(); MAX_SUBSCRIPTIONS + 1];
        assert_eq!(ListenUris::syntactic_filter(&requested), None);
        assert!(ListenUris::syntactic_filter(&requested[..MAX_SUBSCRIPTIONS]).is_some());
    }

    #[test]
    fn test_syntactic_filter_rejects_oversized_total_bytes() {
        let long = format!("lsp-diagnostics:///{}", "a".repeat(4096));
        let requested: Vec<String> = (0..100).map(|i| format!("{long}{i}")).collect();
        assert!(requested.len() <= MAX_SUBSCRIPTIONS);
        assert_eq!(ListenUris::syntactic_filter(&requested), None);
        assert!(ListenUris::syntactic_filter(&requested[..50]).is_some());
    }

    #[test]
    fn test_listen_uris_group_raw_aliases_under_one_canonical() {
        let (_dir, root, file) = workspace_file();
        let canonical = crate::bridge::resources::make_uri(&file).unwrap();
        let alias = canonical.replace("main.rs", "%6Dain.rs");
        let (_other_dir, _other_root, outside) = workspace_file();
        let outside = crate::bridge::resources::make_uri(&outside).unwrap();

        let uris = ListenUris::resolve(
            &[canonical.clone(), alias.clone(), outside],
            &WorkspaceRoots::resolve(vec![root]),
        );

        let entries: Vec<_> = uris.canonical().collect();
        assert_eq!(entries.len(), 1, "the outside URI must be dropped");
        let (uri, lsp_uri) = entries[0];
        assert_eq!(uri.as_str(), canonical);
        assert_eq!(lsp_uri, &crate::bridge::path_to_uri(&file).unwrap());
        let mut expected = [canonical.as_str(), alias.as_str()];
        expected.sort_unstable();
        assert_eq!(uris.raw_for(uri).collect::<Vec<_>>(), expected);
    }

    #[test]
    fn test_listen_uris_resolve_to_nothing_is_empty() {
        let (_dir, root, _file) = workspace_file();
        let uris = ListenUris::resolve(
            &[crate::test_lsp::absolute_uri("no/such/file.rs")],
            &WorkspaceRoots::resolve(vec![root]),
        );
        assert!(uris.is_empty());
    }

    #[test]
    fn test_listen_slots_are_capped_and_freed_on_drop() {
        let registry = SubscriptionRegistry::new();
        let permits: Vec<_> = (0..MAX_LISTEN_STREAMS)
            .map(|_| registry.try_reserve_listen().unwrap())
            .collect();
        assert_eq!(
            registry.try_reserve_listen().unwrap_err(),
            SubscriptionError::ListenLimitReached
        );
        drop(permits);
        assert!(registry.try_reserve_listen().is_ok());
    }

    #[tokio::test]
    async fn test_listen_registration_delivers_then_unregisters_and_frees_slot() {
        let (_dir, root, file) = workspace_file();
        let raw = crate::bridge::resources::make_uri(&file).unwrap();
        let uris = Arc::new(ListenUris::resolve(
            std::slice::from_ref(&raw),
            &WorkspaceRoots::resolve(vec![root]),
        ));
        let canonical = uris.canonical().next().unwrap().0.clone();

        let registry = SubscriptionRegistry::new();
        let (tx, mut rx) = mpsc::channel(4);
        let registration = registry
            .try_reserve_listen()
            .unwrap()
            .register(uris, |_| Target::Channel(tx));
        assert_eq!(registry.live_sessions().len(), 1);

        for session in registry.live_sessions() {
            session.publish_if_subscribed(&canonical).await;
        }
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap();
        assert_eq!(received.as_deref(), Some(raw.as_str()));

        registration
            .publish(&u("lsp-diagnostics:///unwatched.rs"))
            .await;
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        let held: Vec<_> = (1..MAX_LISTEN_STREAMS)
            .map(|_| registry.try_reserve_listen().unwrap())
            .collect();
        assert!(registry.try_reserve_listen().is_err());
        drop(registration);
        assert!(registry.live_sessions().is_empty());
        assert!(registry.try_reserve_listen().is_ok());
        drop(held);
    }
}
