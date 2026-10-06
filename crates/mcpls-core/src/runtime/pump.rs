//! Diagnostics pump: drains one LSP server's notification lanes into the
//! shared cache and fans publications out to subscribed MCP sessions.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::sync::mpsc::error::TryRecvError;
use tracing::{debug, error};

use crate::bridge::resources::{DiagnosticsResourceUri, PublishedDiagnosticsUri};
use crate::bridge::{
    self, DiagnosticsRole, IndexingReset, NotificationCache, Publication, PublicationKind,
    PublishedPathResolver, WorkspaceRoots,
};
use crate::config::ServerId;
use crate::lsp::LspNotification;
use crate::lsp::tsserver_pin::warn_if_pin_ignored;
use crate::mcp::SubscriptionRegistry;
use crate::util::catch_panic;

/// `Arc`-backed state shared by every `diagnostics_pump` task spawned for one
/// `serve_with` run, factored out of `diagnostics_pump`'s parameter list to
/// keep it under clippy's argument-count lint. `Clone` is cheap (`Arc`
/// clones only).
#[derive(Clone)]
pub struct PumpShared {
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
pub async fn diagnostics_pump(
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
#[expect(
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

            publish_to_subscribers(subs, || DiagnosticsResourceUri::for_published(&published))
                .await;
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

/// Queues the resource `make_uri` builds on every session subscribed to it.
///
/// `make_uri` is only called when some session has a subscription at all, so
/// the common case of nobody subscribed costs no URI construction.
async fn publish_to_subscribers(
    subs: &SubscriptionRegistry,
    make_uri: impl FnOnce() -> Option<DiagnosticsResourceUri>,
) {
    if !subs.any_subscription().await {
        return;
    }

    let Some(mcp_uri) = make_uri() else {
        return;
    };
    for session in &subs.live_sessions() {
        session.publish_if_subscribed(&mcp_uri).await;
    }
}

/// Stops trusting a panicked pump's server: marks it push-degraded and resets
/// its indexing state, so callers poll instead of waiting on pushes that no
/// longer arrive.
pub async fn degrade_after_pump_panic(cache: &Mutex<NotificationCache>, id: &ServerId) {
    let mut cache = cache.lock().await;
    cache.mark_push_degraded(id);
    cache.reset_indexing_state(id, IndexingReset::Forget);
}

/// Re-starts diagnostics pumps for manually restarted servers over the same
/// shared state and shutdown watch the initial pumps use.
#[derive(Clone)]
pub struct PumpWiring {
    shared: PumpShared,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
}

impl std::fmt::Debug for PumpWiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PumpWiring").finish_non_exhaustive()
    }
}

impl PumpWiring {
    /// Wires restarted pumps to `shared` and to the shutdown watch `cancel_rx`.
    pub(crate) const fn new(
        shared: PumpShared,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Self {
        Self { shared, cancel_rx }
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
            if let Err(panicked) = catch_panic(pump).await {
                error!(
                    "Diagnostics pump for LSP server '{id}' panicked: {}",
                    panicked.message()
                );
                degrade_after_pump_panic(&cache, &id).await;
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

    fn has_subscriptions(&self) -> futures::future::BoxFuture<'_, bool> {
        Box::pin(self.shared.subs.any_subscription())
    }

    fn publish_changed<'a>(
        &'a self,
        file: &'a lsp_types::Uri,
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(publish_to_subscribers(&self.shared.subs, || {
            DiagnosticsResourceUri::for_canonical_file(file)
        }))
    }
}

#[cfg(test)]
mod pump_tests {
    use std::assert_matches;
    use std::time::Duration;

    use lsp_types::{PublishDiagnosticsParams, Uri};
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::bridge::{IndexingState, Translator};
    use crate::config::LanguageId;
    use crate::runtime::startup::publish_startup_failures;
    use crate::{Error, ProjectConfigStatus, config, mcp};

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
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
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
            tokio::sync::watch::channel(crate::bridge::DiagnosticsRole::Authoritative).1,
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
            tokio::sync::watch::channel(crate::bridge::DiagnosticsRole::Authoritative).1,
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
            tokio::sync::watch::channel(crate::bridge::DiagnosticsRole::Authoritative).1,
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
            tokio::sync::watch::channel(crate::bridge::DiagnosticsRole::Authoritative).1,
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
            tokio::sync::watch::channel(crate::bridge::DiagnosticsRole::Authoritative).1,
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
            tokio::sync::watch::channel(crate::bridge::DiagnosticsRole::Authoritative).1,
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
            tokio::sync::watch::channel(crate::bridge::DiagnosticsRole::Authoritative).1,
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
            tokio::sync::watch::channel(crate::bridge::DiagnosticsRole::Authoritative).1,
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
        let published = bridge::resolve_one(&bridge::path_to_uri(&cleared_file).unwrap(), &roots)
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

    /// #574 FR-007: a file that already has a pulled slot is notified exactly
    /// once per accepted push, as before the pulled slot existed.
    #[tokio::test]
    async fn test_push_beside_a_pulled_slot_notifies_exactly_once() {
        use crate::mcp::{SessionHandle, Target};

        let subs = make_subs();
        let session = SessionHandle::new(subs.clone());
        let (tx_session, mut rx_session) = mpsc::channel(8);
        let x = test_mcp_uri("x.rs");
        session
            .subscribe_for_test(&x, Target::Channel(tx_session.clone()))
            .await
            .unwrap();
        let cache = make_cache();
        cache.lock().await.store_pulled_for_test(
            &ServerId::from("rust"),
            &test_uri("x.rs"),
            vec![lsp_types::Diagnostic::default()],
        );

        let (tx, _cancel_tx) =
            spawn_test_pump_with_cache(subs, test_workspace_roots(), Arc::clone(&cache));
        tx.send(publish("x.rs")).await.unwrap();

        assert_eq!(recv_within(&mut rx_session).await, x.as_str());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_matches!(rx_session.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        assert!(
            cache
                .lock()
                .await
                .diagnostic_sources(&test_uri("x.rs"))
                .merge()
                .is_some_and(|info| info.diagnostics.len() == 1)
        );
    }

    /// A pull that changed a file notifies the subscribers of that file only.
    #[tokio::test]
    async fn test_publish_changed_notifies_only_subscribers_of_the_file() {
        use crate::mcp::{SessionHandle, Target};

        let subs = make_subs();
        let session = SessionHandle::new(subs.clone());
        let (tx_x, mut rx_x) = mpsc::channel(8);
        let (tx_y, mut rx_y) = mpsc::channel(8);
        let (x, y) = (test_mcp_uri("x.rs"), test_mcp_uri("y.rs"));
        session
            .subscribe_for_test(&x, Target::Channel(tx_x.clone()))
            .await
            .unwrap();
        session
            .subscribe_for_test(&y, Target::Channel(tx_y.clone()))
            .await
            .unwrap();
        let (_cancel, cancel_rx) = watch::channel(false);
        let wiring = PumpWiring {
            shared: PumpShared {
                notification_cache: make_cache(),
                subs,
                workspace_roots: test_workspace_roots(),
            },
            cancel_rx,
        };

        bridge::NotificationWiring::publish_changed(&wiring, &test_uri("x.rs")).await;

        assert_eq!(recv_within(&mut rx_x).await, x.as_str());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_matches!(rx_y.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    }

    /// With nobody subscribed a changed pull builds no URI and queues nothing.
    #[tokio::test]
    async fn test_publish_changed_without_subscribers_is_a_no_op() {
        let (_cancel, cancel_rx) = watch::channel(false);
        let wiring = PumpWiring {
            shared: PumpShared {
                notification_cache: make_cache(),
                subs: make_subs(),
                workspace_roots: test_workspace_roots(),
            },
            cancel_rx,
        };

        bridge::NotificationWiring::publish_changed(&wiring, &test_uri("x.rs")).await;
    }

    /// The wiring reports whether any session subscribed, so a caller can skip
    /// work only subscribers care about.
    #[tokio::test]
    async fn test_wiring_reports_whether_anything_is_subscribed() {
        use crate::mcp::{SessionHandle, Target};

        let subs = make_subs();
        let (_cancel, cancel_rx) = watch::channel(false);
        let wiring = PumpWiring {
            shared: PumpShared {
                notification_cache: make_cache(),
                subs: subs.clone(),
                workspace_roots: test_workspace_roots(),
            },
            cancel_rx,
        };
        assert!(!bridge::NotificationWiring::has_subscriptions(&wiring).await);

        let session = SessionHandle::new(subs);
        let (tx_session, _rx_session) = mpsc::channel(8);
        session
            .subscribe_for_test(&test_mcp_uri("x.rs"), Target::Channel(tx_session))
            .await
            .unwrap();

        assert!(bridge::NotificationWiring::has_subscriptions(&wiring).await);
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
        let failing: Arc<bridge::CanonicalizeFn> =
            Arc::new(|_: &std::path::Path| Err(std::io::Error::from(std::io::ErrorKind::TimedOut)));
        let cache = make_cache();
        let (tx, rx) = mpsc::channel(8);
        let (_lifecycle_tx, lifecycle_rx) = mpsc::channel(8);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        tokio::spawn(diagnostics_pump_with_resolver(
            ServerId::from("rust"),
            rx,
            lifecycle_rx,
            cancel_rx,
            tokio::sync::watch::channel(crate::bridge::DiagnosticsRole::Authoritative).1,
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
            tokio::sync::watch::channel(crate::bridge::DiagnosticsRole::Authoritative).1,
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
            tokio::sync::watch::channel(crate::bridge::DiagnosticsRole::Authoritative).1,
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
                    reason: crate::error::StartupFailure::Spawn(Arc::new(Error::ServerNotFound {
                        command: "rust-analyzer".to_string(),
                        source: std::io::Error::from(std::io::ErrorKind::NotFound),
                    })),
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
