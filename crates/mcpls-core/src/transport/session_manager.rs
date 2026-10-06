//! Bounded, idle-reaped HTTP session management over rmcp's `LocalSessionManager`.

use std::sync::Mutex as StdMutex;

use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::streamable_http_server::session::local::{
    LocalSessionManager, LocalSessionManagerError,
};
use rmcp::transport::streamable_http_server::session::{
    ServerSseMessage, SessionId, SessionManager,
};

use super::config::{ResponseStreamDeadline, SessionLimit, StreamLiveness, non_zero_duration};
use super::liveness::{
    ProbeId, SESSION_CLOSE_TIMEOUT, SessionLiveness, StreamProbe, is_common_channel_event_id,
};
use crate::util::{catch_panic, lock_std};

/// Log-safe correlation handle for a session: eight hex digits of a hash of
/// the id, so log lines can be matched without disclosing the bearer secret.
pub(super) struct SessionFingerprint<'a>(pub(super) &'a SessionId);

impl std::fmt::Display for SessionFingerprint<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::hash::{DefaultHasher, Hash as _, Hasher as _};

        let mut hasher = DefaultHasher::new();
        self.0.hash(&mut hasher);
        write!(f, "{:08x}", hasher.finish() >> 32)
    }
}

/// Wraps [`LocalSessionManager`], bounding concurrent HTTP sessions to a
/// fixed capacity.
///
/// A [`tokio::sync::Semaphore`] permit is acquired atomically inside
/// [`create_session`](SessionManager::create_session) — before delegating to
/// the inner manager — and held in a [`SessionSlot`], next to the session's
/// inbound-activity record, for the session's lifetime. The slot is removed,
/// releasing the permit, by [`close_session`](SessionManager::close_session)
/// or by the idle reaper ([`run_idle_reaper`]). Every response stream the
/// session hands out (POST, GET, resume) carries a [`StreamGuard`], so a
/// session with an open stream is never reaped. This makes the cap a
/// true hard bound: the check and the reservation happen as one step, so no
/// number of concurrent requests can observe spare capacity and all proceed
/// past it (a "check-then-create" race that a separate read of the session
/// count could not avoid).
///
/// Enforcement lives here, at the `SessionManager` layer, rather than in Axum
/// middleware sniffing request headers, because that is the only place
/// guaranteed to run exactly when — and only when — a session is actually
/// created. `rmcp`'s `StreamableHttpService::handle_post` classifies
/// every `initialize` request as legacy and always calls `create_session`,
/// whatever protocol version it names — the handshake only exists in
/// revisions before `2026-07-28`, so a version named in its params never
/// routes it to the stateless path. Only *non*-`initialize` requests that
/// carry SEP-2575 per-request `_meta` (`io.modelcontextprotocol/protocolVersion`
/// = `2026-07-28` plus the required `clientCapabilities` key), and
/// `server/discover` requests, take the stateless discover-lifecycle path
/// that never calls `create_session`. A header-based middleware heuristic
/// can't tell these apart without duplicating `rmcp`'s internal protocol
/// classification, so it either 429s traffic that never consumed a session
/// slot, or — in an all-stateless deployment — never fires at all.
///
/// `restore_session` and `event_store` deliberately use
/// [`SessionManager`]'s trait defaults (`NotSupported` / `None`) instead of
/// delegating to `inner`: `HttpConfig` exposes no session-store knob, so
/// these are unreachable today, but delegating them would let a restored
/// session skip the semaphore entirely — a cap bypass. Leave them as
/// defaults; overriding them to delegate is not a bug fix.
pub(super) struct CappedSessionManager {
    inner: std::sync::Arc<LocalSessionManager>,
    semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    slots: StdMutex<std::collections::HashMap<SessionId, SessionSlot>>,
    idle: IdleTimeout,
    stream_liveness: StreamLiveness,
    response_stream_deadline: ResponseStreamDeadline,
}

impl CappedSessionManager {
    pub(super) fn new(max_sessions: SessionLimit, idle: IdleTimeout) -> Self {
        let mut inner = LocalSessionManager::default();
        inner.session_config.keep_alive = None;
        Self {
            inner: std::sync::Arc::new(inner),
            semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(max_sessions.get())),
            slots: StdMutex::new(std::collections::HashMap::new()),
            idle,
            stream_liveness: StreamLiveness::Disabled,
            response_stream_deadline: ResponseStreamDeadline::DEFAULT,
        }
    }

    pub(super) const fn with_stream_liveness(mut self, stream_liveness: StreamLiveness) -> Self {
        self.stream_liveness = stream_liveness;
        self
    }

    pub(super) const fn with_response_stream_deadline(
        mut self,
        deadline: ResponseStreamDeadline,
    ) -> Self {
        self.response_stream_deadline = deadline;
        self
    }

    fn session_liveness(&self, id: &SessionId) -> Option<std::sync::Arc<SessionLiveness>> {
        lock_std(&self.slots)
            .get(id)
            .map(|slot| std::sync::Arc::clone(&slot.liveness))
    }

    /// Wraps a standalone-channel `stream` in a probing forwarding task when
    /// liveness probing is on; otherwise only guards it.
    ///
    /// Fails closed when probing is on but the session's slot is gone (the
    /// reaper removed it ahead of closing the inner session), rather than
    /// handing out an unprobed stream.
    fn standalone<S>(
        &self,
        id: &SessionId,
        guard: Option<StreamGuard>,
        stream: S,
    ) -> Result<
        impl futures::Stream<Item = ServerSseMessage> + Send + Sync + 'static + use<S>,
        CappedSessionManagerError,
    >
    where
        S: futures::Stream<Item = ServerSseMessage> + Send + Sync + 'static,
    {
        use futures::StreamExt as _;

        Ok(match self.stream_liveness {
            StreamLiveness::Probe { interval, deadline } => {
                let liveness = self
                    .session_liveness(id)
                    .ok_or_else(|| LocalSessionManagerError::SessionNotFound(id.clone()))?;
                StreamProbe {
                    liveness,
                    interval,
                    deadline,
                    manager: std::sync::Arc::clone(&self.inner),
                    session: id.clone(),
                }
                .forward(stream, guard)
                .left_stream()
            }
            StreamLiveness::Disabled => {
                drop(guard);
                stream.right_stream()
            }
        })
    }

    fn activity(&self, id: &SessionId) -> Option<std::sync::Arc<SessionActivity>> {
        lock_std(&self.slots)
            .get(id)
            .map(|slot| std::sync::Arc::clone(&slot.activity))
    }

    fn touch(&self, id: &SessionId) {
        if let Some(activity) = self.activity(id) {
            activity.touch();
        }
    }

    /// Counts a response stream about to open on `id` as activity; `None` for
    /// an unknown session.
    fn open_guard(&self, id: &SessionId) -> Option<StreamGuard> {
        self.activity(id).map(|activity| activity.open_stream())
    }

    /// Wraps `stream` so the session counts as active until it is dropped.
    ///
    /// The guard is taken before the inner call so a sweep cannot slip in
    /// between the call and the stream existing.
    fn guarded<S: futures::Stream>(
        guard: Option<StreamGuard>,
        stream: S,
    ) -> impl futures::Stream<Item = S::Item> {
        use futures::StreamExt as _;

        stream.map(move |message| {
            let _keep_open = &guard;
            message
        })
    }

    /// [`Self::guarded`], cut once the response stream deadline, counted from
    /// this call, passes.
    fn bounded<S: futures::Stream>(
        guard: Option<StreamGuard>,
        stream: S,
        deadline: ResponseStreamDeadline,
        session: SessionId,
    ) -> impl futures::Stream<Item = S::Item> + use<S> {
        use futures::StreamExt as _;

        let timer = tokio::time::sleep(deadline.get());
        Self::guarded(guard, stream).take_until(async move {
            timer.await;
            tracing::debug!(
                session = %SessionFingerprint(&session),
                "closing response stream at its deadline"
            );
        })
    }

    /// Removes every session idle at `now` (freeing its permit at once) and
    /// closes it in the inner manager on a detached task, so one wedged
    /// session worker cannot stall the sweep. Returns how many were reaped.
    ///
    /// Benign race: a request touching or opening a stream on a session between
    /// its idle check and its removal here is not seen, so that session is
    /// reaped anyway. It had been idle for the whole timeout, and the client
    /// gets a 404 on its next call and re-initializes.
    fn reap_idle(&self, now: tokio::time::Instant) -> usize {
        let idle_ids: Vec<SessionId> = {
            let mut slots = lock_std(&self.slots);
            let ids: Vec<_> = slots
                .iter()
                .filter(|(_, slot)| slot.activity.is_idle(now, self.idle))
                .map(|(id, _)| id.clone())
                .collect();
            for id in &ids {
                slots.remove(id);
            }
            ids
        };
        for id in idle_ids.iter().cloned() {
            tracing::debug!(session = %SessionFingerprint(&id), "closing idle HTTP session");
            let inner = std::sync::Arc::clone(&self.inner);
            let session = id.clone();
            spawn_bounded_close("closing idle HTTP session", id, async move {
                inner.close_session(&session).await
            });
        }
        idle_ids.len()
    }
}

/// Runs `close` on a detached task for at most [`SESSION_CLOSE_TIMEOUT`], so a
/// wedged session worker cannot park the task forever, and logs the outcome at
/// debug level under `label`.
pub(super) fn spawn_bounded_close<E: std::fmt::Display + Send + 'static>(
    label: &'static str,
    session: SessionId,
    close: impl std::future::Future<Output = Result<(), E>> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match tokio::time::timeout(SESSION_CLOSE_TIMEOUT, close).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(session = %SessionFingerprint(&session), "{label} failed: {e}");
            }
            Err(_elapsed) => {
                tracing::debug!(session = %SessionFingerprint(&session), "{label} timed out");
            }
        }
    })
}

non_zero_duration! {
    /// Non-zero duration after which a session without inbound client activity
    /// or open response stream is closed by the idle reaper.
    ///
    /// The sole session expiry owner: rmcp's own `keep_alive` is disabled
    /// because it measures any event on the session -- including outbound
    /// notifications and, for an answering GET listener, nothing at all -- so it
    /// both never fired for an abandoned but subscribed session (#521) and cut
    /// off a healthy one (#573). An open response stream holds the session only
    /// while it is proven alive (a POST stream, or a probed GET stream).
    pub IdleTimeout, std::time::Duration::from_mins(5)
}

impl IdleTimeout {
    /// A fifth of the timeout, so an idle session closes within 1.2x of it;
    /// never zero, which `tokio::time::interval` rejects.
    fn sweep_interval(self) -> std::time::Duration {
        self.get()
            .checked_div(5)
            .unwrap_or_default()
            .max(std::time::Duration::from_millis(1))
    }
}

#[derive(Debug)]
struct ActivityState {
    open_streams: usize,
    idle_since: tokio::time::Instant,
}

/// Inbound-activity record of one session: how many response streams are
/// open and since when none has been.
#[derive(Debug)]
pub(super) struct SessionActivity(StdMutex<ActivityState>);

impl SessionActivity {
    pub(super) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self(StdMutex::new(ActivityState {
            open_streams: 0,
            idle_since: tokio::time::Instant::now(),
        })))
    }

    #[cfg(test)]
    pub(super) fn open_stream_count(&self) -> usize {
        lock_std(&self.0).open_streams
    }

    fn touch(&self) {
        lock_std(&self.0).idle_since = tokio::time::Instant::now();
    }

    pub(super) fn open_stream(self: &std::sync::Arc<Self>) -> StreamGuard {
        let mut state = lock_std(&self.0);
        state.open_streams = state.open_streams.saturating_add(1);
        drop(state);
        StreamGuard(std::sync::Arc::clone(self))
    }

    fn is_idle(&self, now: tokio::time::Instant, timeout: IdleTimeout) -> bool {
        let state = lock_std(&self.0);
        state.open_streams == 0 && now.saturating_duration_since(state.idle_since) >= timeout.get()
    }
}

/// Keeps a session non-idle while one of its response streams is open;
/// dropping it restarts the idle clock.
#[derive(Debug)]
pub(super) struct StreamGuard(std::sync::Arc<SessionActivity>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let mut state = lock_std(&self.0.0);
        state.open_streams = state.open_streams.saturating_sub(1);
        state.idle_since = tokio::time::Instant::now();
    }
}

/// One live session's cap permit and activity record, kept together so the
/// two cannot diverge.
#[derive(Debug)]
struct SessionSlot {
    _permit: tokio::sync::OwnedSemaphorePermit,
    activity: std::sync::Arc<SessionActivity>,
    liveness: std::sync::Arc<SessionLiveness>,
}

/// Periodically closes the sessions of `manager` that are idle, until
/// `cancel` fires.
pub(super) async fn run_idle_reaper(
    manager: std::sync::Arc<CappedSessionManager>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut ticker = tokio::time::interval(manager.idle.sweep_interval());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticker.tick() => {
                manager.reap_idle(tokio::time::Instant::now());
            }
        }
    }
}

/// Delay before a panicked idle reaper is started again.
const REAPER_RESTART_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// Keeps [`run_idle_reaper`] alive until `cancel` fires: a panic is logged at
/// error level and the reaper restarted, because a dead reaper silently stops
/// session expiry until the session cap fills and every client gets 429.
pub(super) async fn supervise_idle_reaper(
    manager: std::sync::Arc<CappedSessionManager>,
    cancel: tokio_util::sync::CancellationToken,
) {
    supervise(
        || run_idle_reaper(std::sync::Arc::clone(&manager), cancel.clone()),
        &cancel,
        REAPER_RESTART_DELAY,
    )
    .await;
}

/// Runs `run` to completion, rerunning it after `restart_delay` each time it
/// panics, and stops once it returns or `cancel` fires during the delay.
async fn supervise<Fut: std::future::Future<Output = ()>>(
    mut run: impl FnMut() -> Fut,
    cancel: &tokio_util::sync::CancellationToken,
    restart_delay: std::time::Duration,
) {
    while let Err(panicked) = catch_panic(run()).await {
        tracing::error!(
            error = %panicked,
            "idle HTTP session reaper panicked; restarting it, sessions do not expire until it runs again"
        );
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(restart_delay) => {}
        }
    }
}

/// Marker embedded in [`CappedSessionManagerError::CapReached`]'s rendered
/// message.
///
/// `rmcp`'s `StreamableHttpService` always maps `create_session` failures to
/// a generic `500 Internal Server Error` (`internal_error_response` in
/// `server_side_http.rs` is a fixed, non-configurable mapping — `rmcp` gives
/// callers no other hook). [`enforce_session_cap`] looks for this marker in
/// the response body to translate a capacity rejection into
/// `429 Too Many Requests` without misclassifying other `create_session`
/// failures as capacity issues.
pub(super) const SESSION_CAP_MARKER: &str = "mcpls-http-session-cap-reached";

/// Error type for [`CappedSessionManager`].
#[derive(Debug, thiserror::Error)]
pub(super) enum CappedSessionManagerError {
    /// The concurrent-session cap was already reached.
    #[error("{SESSION_CAP_MARKER}: maximum concurrent HTTP sessions already active")]
    CapReached,
    /// The session is gone. Carries no id: rmcp logs this error and the id is
    /// a bearer secret (#555).
    #[error("session not found")]
    SessionGone,
    /// The wrapped [`LocalSessionManager`] failed.
    #[error(transparent)]
    Inner(LocalSessionManagerError),
}

impl From<LocalSessionManagerError> for CappedSessionManagerError {
    fn from(error: LocalSessionManagerError) -> Self {
        match error {
            LocalSessionManagerError::SessionNotFound(_) => Self::SessionGone,
            other => Self::Inner(other),
        }
    }
}

impl SessionManager for CappedSessionManager {
    type Error = CappedSessionManagerError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let permit = self
            .semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| CappedSessionManagerError::CapReached)?;
        let (id, transport) = self.inner.create_session().await?;
        lock_std(&self.slots).insert(
            id.clone(),
            SessionSlot {
                _permit: permit,
                activity: SessionActivity::new(),
                liveness: std::sync::Arc::default(),
            },
        );
        Ok((id, transport))
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, Self::Error> {
        self.touch(id);
        Ok(self.inner.initialize_session(id, message).await?)
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        Ok(self.inner.has_session(id).await?)
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        // Release the permit unconditionally, before propagating any error from
        // `inner.close_session`: on error the inner manager has already dropped
        // the session from its own table (see `LocalSessionManager::close_session`),
        // so skipping the removal here would leak the permit permanently and
        // monotonically shrink capacity.
        lock_std(&self.slots).remove(id);
        self.inner.close_session(id).await?;
        Ok(())
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl futures::Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error>
    {
        let guard = self.open_guard(id);
        let stream = self.inner.create_stream(id, message).await?;
        Ok(Self::bounded(
            guard,
            stream,
            self.response_stream_deadline,
            id.clone(),
        ))
    }

    async fn accept_message(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<(), Self::Error> {
        self.touch(id);
        if let Some(probe) = ProbeId::answered_by(&message) {
            if let Some(liveness) = self.session_liveness(id) {
                liveness.acknowledge(probe);
            }
            return Ok(());
        }
        Ok(self.inner.accept_message(id, message).await?)
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl futures::Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error>
    {
        let guard = self.open_guard(id);
        let stream = self.inner.create_standalone_stream(id).await?;
        self.standalone(id, guard, stream)
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl futures::Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error>
    {
        use futures::StreamExt as _;

        let guard = self.open_guard(id);
        let common = is_common_channel_event_id(&last_event_id);
        let stream = self.inner.resume(id, last_event_id).await?;
        Ok(if common {
            self.standalone(id, guard, stream)?.left_stream()
        } else {
            Self::bounded(guard, stream, self.response_stream_deadline, id.clone()).right_stream()
        })
    }
}

/// Axum middleware that rewrites `rmcp`'s generic `500 Internal Server Error`
/// into `429 Too Many Requests` when the failure was
/// [`CappedSessionManagerError::CapReached`] (detected via
/// [`SESSION_CAP_MARKER`] in the response body), adding a `Retry-After`
/// header.
///
/// This runs as response post-processing rather than a request pre-check
/// because only the real [`SessionManager::create_session`] call — deep
/// inside `rmcp` — knows whether a given request actually attempts to create
/// a session; see [`CappedSessionManager`]'s docs for why that can't be
/// determined from the request alone.
pub(super) async fn enforce_session_cap(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let response = next.run(request).await;
    if response.status() != axum::http::StatusCode::INTERNAL_SERVER_ERROR {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    // `create_session` failures always render as a small `Full<Bytes>` body
    // (`internal_error_response` in rmcp's `server_side_http.rs`); the large
    // streaming SSE/JSON success bodies never carry a 500 status, so this
    // never touches them. 64 KiB is far beyond any realistic error message.
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        // Buffering the original error body failed (e.g. it exceeded the 64
        // KiB cap, which should never happen per the comment above, or the
        // body stream errored). Preserve the 500 status but substitute a
        // minimal fallback body rather than dropping the error entirely.
        return axum::response::Response::from_parts(
            parts,
            axum::body::Body::from("Internal Server Error"),
        );
    };

    if bytes
        .windows(SESSION_CAP_MARKER.len())
        .any(|window| window == SESSION_CAP_MARKER.as_bytes())
    {
        parts.status = axum::http::StatusCode::TOO_MANY_REQUESTS;
        parts.headers.insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from_static("1"),
        );
        return axum::response::Response::from_parts(
            parts,
            axum::body::Body::from("Too Many Requests: maximum concurrent HTTP sessions reached"),
        );
    }

    axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::bridge::WorkspaceRoots;
    use crate::config::LanguageId;
    use crate::transport::config::{ProbeDeadline, ProbeInterval};
    use crate::transport::test_support::test_server;

    #[test]
    fn test_session_fingerprint_hides_id() {
        let id: super::SessionId = "61429d44-e35a-4615-bd7f-1ccb38acecae".into();
        let shown = super::SessionFingerprint(&id).to_string();

        assert_eq!(shown.len(), 8);
        assert!(shown.chars().all(|c| c.is_ascii_hexdigit()), "{shown}");
        assert!(!id.contains(&shown));
        assert_eq!(shown, super::SessionFingerprint(&id).to_string());
    }

    /// `CappedSessionManager::create_session` must enforce a hard bound:
    /// once `max_sessions` sessions exist, the next `create_session` call
    /// fails with the capacity marker, and closing a session frees the
    /// slot back up for a subsequent `create_session` to succeed.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test]
    async fn test_capped_session_manager_enforces_hard_bound() {
        use rmcp::transport::streamable_http_server::session::SessionManager as _;

        let manager =
            CappedSessionManager::new(crate::SessionLimit::new(1).unwrap(), IdleTimeout::DEFAULT);

        let (first_id, _transport) = manager.create_session().await.unwrap();

        let second_err = manager.create_session().await.map(|_| ()).unwrap_err();
        assert_matches!(
            second_err,
            CappedSessionManagerError::CapReached,
            "expected CapReached once at capacity, got: {second_err:?}"
        );

        manager.close_session(&first_id).await.unwrap();

        let (third_id, _transport) = manager.create_session().await.unwrap();
        assert_ne!(first_id, third_id);
    }

    /// Regression guard for S2: concurrent `create_session` calls must not
    /// overshoot `max_sessions`. Unlike the sequential test above (which
    /// would pass even against a racy check-then-create implementation),
    /// this spawns `N > max_sessions` calls at once and asserts exactly
    /// `max_sessions` succeed — the one test shape that actually
    /// distinguishes the atomic-semaphore design from a TOCTOU race.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test]
    async fn test_capped_session_manager_bounds_concurrent_create_session() {
        use rmcp::transport::streamable_http_server::session::SessionManager as _;

        const MAX_SESSIONS: usize = 5;
        const CONCURRENT_ATTEMPTS: usize = 25;

        let manager = std::sync::Arc::new(CappedSessionManager::new(
            crate::SessionLimit::new(MAX_SESSIONS).unwrap(),
            IdleTimeout::DEFAULT,
        ));

        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..CONCURRENT_ATTEMPTS {
            let manager = manager.clone();
            tasks.spawn(async move { manager.create_session().await.is_ok() });
        }

        let mut succeeded = 0usize;
        while let Some(result) = tasks.join_next().await {
            if result.unwrap() {
                succeeded += 1;
            }
        }

        assert_eq!(
            succeeded, MAX_SESSIONS,
            "exactly max_sessions concurrent create_session calls must succeed"
        );
    }

    #[test]
    fn test_idle_timeout_rejects_zero() {
        assert_eq!(IdleTimeout::new(std::time::Duration::ZERO), None);
        assert!(IdleTimeout::new(std::time::Duration::from_nanos(1)).is_some());
    }

    #[test]
    fn test_idle_timeout_sweep_interval_is_never_zero() {
        let tiny = IdleTimeout::new(std::time::Duration::from_nanos(1)).unwrap();
        assert!(!tiny.sweep_interval().is_zero());
        assert_eq!(
            IdleTimeout::new(std::time::Duration::from_secs(10))
                .unwrap()
                .sweep_interval(),
            std::time::Duration::from_secs(2)
        );
    }

    fn idle_secs(secs: u64) -> IdleTimeout {
        IdleTimeout::new(std::time::Duration::from_secs(secs)).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn test_session_with_open_stream_is_never_idle() {
        let activity = SessionActivity::new();
        let guard = activity.open_stream();
        tokio::time::advance(std::time::Duration::from_mins(1)).await;
        assert!(!activity.is_idle(tokio::time::Instant::now(), idle_secs(5)));

        drop(guard);
        assert!(
            !activity.is_idle(tokio::time::Instant::now(), idle_secs(5)),
            "closing the last stream restarts the idle clock"
        );
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        assert!(activity.is_idle(tokio::time::Instant::now(), idle_secs(5)));
    }

    #[tokio::test(start_paused = true)]
    async fn test_touch_restarts_idle_clock() {
        let activity = SessionActivity::new();
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        activity.touch();
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        assert!(!activity.is_idle(tokio::time::Instant::now(), idle_secs(5)));
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert!(activity.is_idle(tokio::time::Instant::now(), idle_secs(5)));
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_reap_idle_frees_permit_and_spares_touched_sessions() {
        let manager =
            CappedSessionManager::new(crate::SessionLimit::new(2).unwrap(), idle_secs(10));
        let (idle_id, _idle_transport) = manager.create_session().await.unwrap();
        let (busy_id, _busy_transport) = manager.create_session().await.unwrap();
        assert_matches!(
            manager.create_session().await.err(),
            Some(CappedSessionManagerError::CapReached)
        );

        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        manager.touch(&busy_id);
        tokio::time::advance(std::time::Duration::from_secs(6)).await;

        assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 1);
        assert_eq!(manager.semaphore.available_permits(), 1);
        assert!(manager.activity(&idle_id).is_none());
        assert!(manager.activity(&busy_id).is_some());
        assert!(manager.create_session().await.is_ok());
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_run_idle_reaper_closes_idle_session_and_stops_on_cancel() {
        let manager = std::sync::Arc::new(CappedSessionManager::new(
            crate::SessionLimit::new(1).unwrap(),
            idle_secs(10),
        ));
        let (id, _transport) = manager.create_session().await.unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let reaper = tokio::spawn(run_idle_reaper(
            std::sync::Arc::clone(&manager),
            cancel.clone(),
        ));

        tokio::time::sleep(std::time::Duration::from_secs(13)).await;
        assert!(manager.activity(&id).is_none());
        assert_eq!(manager.semaphore.available_permits(), 1);
        assert!(!manager.has_session(&id).await.unwrap());

        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), reaper)
            .await
            .unwrap()
            .unwrap();
    }

    /// Creates a session and serves an initialized MCP server on it, since
    /// the session worker answers stream requests only after the handshake.
    async fn initialized_session(
        manager: &CappedSessionManager,
    ) -> (
        rmcp::transport::streamable_http_server::session::SessionId,
        tokio::task::JoinHandle<()>,
    ) {
        initialized_session_serving(manager, test_server()).await
    }

    /// [`initialized_session`] serving `server`.
    async fn initialized_session_serving(
        manager: &CappedSessionManager,
        server: crate::mcp::McplsServer,
    ) -> (
        rmcp::transport::streamable_http_server::session::SessionId,
        tokio::task::JoinHandle<()>,
    ) {
        use rmcp::ServiceExt as _;

        let (id, transport) = manager.create_session().await.unwrap();
        let serving = tokio::spawn(async move {
            if let Ok(running) = server.serve(transport).await {
                running.waiting().await.ok();
            }
        });
        let initialize: ClientJsonRpcMessage = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"},
            },
        }))
        .unwrap();
        manager.initialize_session(&id, initialize).await.unwrap();
        let initialized: ClientJsonRpcMessage = serde_json::from_value(
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .unwrap();
        manager.accept_message(&id, initialized).await.unwrap();
        (id, serving)
    }

    /// Holds `stream` (a response stream of the manager's only session)
    /// unpolled past the idle timeout and checks it keeps the session from
    /// being reaped until it is dropped.
    async fn assert_open_stream_blocks_reaping<S>(manager: &CappedSessionManager, stream: S) {
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 0);

        drop(stream);
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 1);
    }

    /// #587: a response stream ends at its deadline, and only then does
    /// the session become reapable (after the idle timeout).
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_bounded_stream_ends_at_deadline_then_session_is_reaped() {
        use futures::StreamExt as _;

        let deadline =
            crate::ResponseStreamDeadline::new(std::time::Duration::from_secs(20)).unwrap();
        let manager =
            CappedSessionManager::new(crate::SessionLimit::new(1).unwrap(), idle_secs(10))
                .with_response_stream_deadline(deadline);
        let (id, _transport) = manager.create_session().await.unwrap();
        let mut stream = Box::pin(CappedSessionManager::bounded(
            manager.open_guard(&id),
            futures::stream::pending::<u8>(),
            deadline,
            id.clone(),
        ));
        let reader = tokio::spawn(async move { stream.next().await });

        tokio::time::advance(std::time::Duration::from_secs(19)).await;
        tokio::task::yield_now().await;
        assert!(!reader.is_finished());
        assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 0);

        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert_eq!(reader.await.unwrap(), None);
        assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 0);

        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 1);
    }

    /// A server whose only language server never answers, and a `tools/call`
    /// that therefore keeps its response stream open.
    fn hung_tool_call() -> (
        tempfile::TempDir,
        crate::test_lsp::FakeServer,
        crate::mcp::McplsServer,
        ClientJsonRpcMessage,
    ) {
        use std::sync::Arc;

        use tokio::sync::Mutex;

        use crate::bridge::{NotificationCache, Translator};
        use crate::config::{McpConfig, ServerId, ToolRouter};
        use crate::mcp::{McplsServer, SubscriptionRegistry};

        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let roots = WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap();
        let mut translator = Translator::new()
            .with_extensions(crate::test_lsp::test_extensions())
            .with_router(ToolRouter::catch_all([(
                ServerId::from("rust"),
                LanguageId::from_static("rust"),
            )]));
        translator.set_workspace_roots(roots.clone());
        let (client, fake_lsp) = crate::test_lsp::fake_lsp_client();
        translator.register_client("rust".to_string(), client);
        let server = McplsServer::new(
            Arc::new(translator),
            Arc::new(Mutex::new(NotificationCache::new())),
            roots,
            SubscriptionRegistry::new(),
            crate::ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );
        let call = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {
                "name": "get_hover",
                "arguments": {"file_path": file.to_string_lossy(), "line": 1, "character": 4}
            }
        }))
        .unwrap();
        (dir, fake_lsp, server, call)
    }

    /// A session manager whose response streams are cut after 20 s.
    fn manager_with_short_deadline() -> CappedSessionManager {
        let deadline =
            crate::ResponseStreamDeadline::new(std::time::Duration::from_secs(20)).unwrap();
        CappedSessionManager::new(crate::SessionLimit::new(1).unwrap(), idle_secs(3600))
            .with_response_stream_deadline(deadline)
    }

    /// #587 wiring: `create_stream` hands out a bounded stream. The tool
    /// call never completes, so only the deadline can end the stream.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_create_stream_is_cut_at_the_response_stream_deadline() {
        use futures::StreamExt as _;

        let manager = manager_with_short_deadline();
        let (_dir, _unread_lsp, server, call) = hung_tool_call();
        let (id, serving) = initialized_session_serving(&manager, server).await;

        let mut stream = Box::pin(manager.create_stream(&id, call).await.unwrap());
        assert!(
            stream.next().await.is_some(),
            "the open stream announces itself"
        );
        let early = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await;
        assert!(
            early.is_err(),
            "control: open before the deadline, got {early:?}"
        );

        let late = tokio::time::timeout(std::time::Duration::from_secs(30), stream.next()).await;
        assert_matches!(late, Ok(None), "stream outlived its deadline");
        serving.abort();
    }

    /// #587 wiring: a request-wise `resume` (not the common channel) is
    /// bounded too.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_request_wise_resume_is_cut_at_the_response_stream_deadline() {
        use futures::StreamExt as _;

        let manager = manager_with_short_deadline();
        let (_dir, _unread_lsp, server, call) = hung_tool_call();
        let (id, serving) = initialized_session_serving(&manager, server).await;
        let mut first = Box::pin(manager.create_stream(&id, call).await.unwrap());
        let primed = tokio::time::timeout(std::time::Duration::from_secs(1), first.next())
            .await
            .unwrap()
            .unwrap();
        let primed_id = primed.event_id.unwrap();
        let (_, request) = primed_id.split_once('/').unwrap();

        let mut resumed = Box::pin(manager.resume(&id, format!("0/{request}")).await.unwrap());

        let late = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while resumed.next().await.is_some() {}
        })
        .await;
        assert!(late.is_ok(), "resumed stream outlived its deadline");
        serving.abort();
    }

    /// A one-session manager with a 10 s idle timeout whose probes are far
    /// enough apart that they never fire within a test.
    fn quietly_probing_manager() -> CappedSessionManager {
        let hour = std::time::Duration::from_hours(1);
        CappedSessionManager::new(crate::SessionLimit::new(1).unwrap(), idle_secs(10))
            .with_stream_liveness(StreamLiveness::Probe {
                interval: ProbeInterval::new(hour).unwrap(),
                deadline: ProbeDeadline::new(hour).unwrap(),
            })
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_open_resume_stream_blocks_reaping() {
        let manager = quietly_probing_manager();
        let (id, serving) = initialized_session(&manager).await;
        let stream = manager.resume(&id, "0".to_owned()).await.unwrap();
        assert_open_stream_blocks_reaping(&manager, stream).await;
        serving.abort();
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_open_post_stream_blocks_reaping() {
        let manager =
            CappedSessionManager::new(crate::SessionLimit::new(1).unwrap(), idle_secs(10));
        let (id, serving) = initialized_session(&manager).await;
        let ping: ClientJsonRpcMessage = serde_json::from_value(
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}),
        )
        .unwrap();
        let stream = manager.create_stream(&id, ping).await.unwrap();
        assert_open_stream_blocks_reaping(&manager, stream).await;
        serving.abort();
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_open_standalone_stream_blocks_reaping() {
        let manager = quietly_probing_manager();
        let (id, serving) = initialized_session(&manager).await;
        let stream = manager.create_standalone_stream(&id).await.unwrap();
        assert_open_stream_blocks_reaping(&manager, stream).await;
        serving.abort();
    }

    fn probing_manager(
        interval: std::time::Duration,
        deadline: std::time::Duration,
    ) -> std::sync::Arc<CappedSessionManager> {
        std::sync::Arc::new(
            CappedSessionManager::new(crate::SessionLimit::new(4).unwrap(), idle_secs(3600))
                .with_stream_liveness(StreamLiveness::Probe {
                    interval: ProbeInterval::new(interval).unwrap(),
                    deadline: ProbeDeadline::new(deadline).unwrap(),
                }),
        )
    }

    fn probe_reply(
        message: &rmcp::transport::streamable_http_server::session::ServerSseMessage,
    ) -> Option<ClientJsonRpcMessage> {
        let json = serde_json::to_value(&**message.message.as_ref()?).ok()?;
        (json["method"] == "ping").then(|| {
            serde_json::from_value(
                serde_json::json!({"jsonrpc": "2.0", "id": json["id"], "result": {}}),
            )
            .unwrap()
        })
    }

    /// Drains `stream`, answering every probe in `answer_in` when `answering`;
    /// finishes when the stream ends.
    fn spawn_drain<S>(
        manager: &std::sync::Arc<CappedSessionManager>,
        answer_in: &rmcp::transport::streamable_http_server::session::SessionId,
        stream: S,
        answering: bool,
    ) -> tokio::task::JoinHandle<()>
    where
        S: futures::Stream<
                Item = rmcp::transport::streamable_http_server::session::ServerSseMessage,
            > + Send
            + 'static,
    {
        use futures::StreamExt as _;

        let manager = std::sync::Arc::clone(manager);
        let answer_in = answer_in.clone();
        tokio::spawn(async move {
            let mut stream = Box::pin(stream);
            while let Some(message) = stream.next().await {
                if let Some(reply) = probe_reply(&message).filter(|_| answering) {
                    manager.accept_message(&answer_in, reply).await.unwrap();
                }
            }
        })
    }

    const PROBE_STEP: std::time::Duration = std::time::Duration::from_millis(100);

    /// A probing manager with one initialized session; `serving` is the
    /// session worker's task.
    async fn probed_session() -> (
        std::sync::Arc<CappedSessionManager>,
        rmcp::transport::streamable_http_server::session::SessionId,
        tokio::task::JoinHandle<()>,
    ) {
        let manager = probing_manager(PROBE_STEP, PROBE_STEP * 2);
        let (id, serving) = initialized_session(&manager).await;
        (manager, id, serving)
    }

    /// #573: rmcp's own `keep_alive` must not end a session whose client
    /// answers probes on an open GET stream.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_answering_get_listener_outlives_rmcp_keep_alive() {
        let (manager, id, serving) = probed_session().await;
        let stream = manager.create_standalone_stream(&id).await.unwrap();
        let listener = spawn_drain(&manager, &id, stream, true);

        tokio::time::sleep(rmcp::transport::streamable_http_server::session::local::SessionConfig::DEFAULT_KEEP_ALIVE * 2).await;

        assert!(manager.has_session(&id).await.unwrap());
        assert!(!listener.is_finished());
        listener.abort();
        serving.abort();
    }

    /// With probing off, an open GET stream no longer holds a session: a
    /// quiet peer that may have vanished expires within the idle timeout.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_disabled_liveness_get_stream_does_not_block_reaping() {
        let manager =
            CappedSessionManager::new(crate::SessionLimit::new(1).unwrap(), idle_secs(10));
        let (id, serving) = initialized_session(&manager).await;
        let _stream = manager.create_standalone_stream(&id).await.unwrap();

        tokio::time::advance(std::time::Duration::from_secs(30)).await;

        assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 1);
        serving.abort();
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_primary_timeout_closes_standalone_stream_and_clears_shadows() {
        let (manager, id, serving) = probed_session().await;
        let primary = manager.create_standalone_stream(&id).await.unwrap();
        let shadow = manager.create_standalone_stream(&id).await.unwrap();
        let primary_task = spawn_drain(&manager, &id, primary, false);
        let shadow_task = spawn_drain(&manager, &id, shadow, true);

        tokio::time::timeout(std::time::Duration::from_secs(2), primary_task)
            .await
            .unwrap_or_else(|_| panic!("silent primary must end"))
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), shadow_task)
            .await
            .unwrap_or_else(|_| {
                panic!("closing the primary's stream must end the answering shadow too")
            })
            .unwrap();
        serving.abort();
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_shadow_timeout_leaves_primary_stream_open() {
        let (manager, id, serving) = probed_session().await;
        let primary = manager.create_standalone_stream(&id).await.unwrap();
        let shadow = manager.create_standalone_stream(&id).await.unwrap();
        let primary_task = spawn_drain(&manager, &id, primary, true);
        let shadow_task = spawn_drain(&manager, &id, shadow, false);

        tokio::time::timeout(std::time::Duration::from_secs(2), shadow_task)
            .await
            .unwrap_or_else(|_| panic!("silent shadow must end"))
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        assert!(
            !primary_task.is_finished(),
            "a shadow timeout must not close the primary stream"
        );
        primary_task.abort();
        serving.abort();
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_wedged_session_does_not_block_stream_end() {
        use futures::StreamExt as _;

        let (manager, id, serving) = probed_session().await;
        let mut stream = Box::pin(manager.create_standalone_stream(&id).await.unwrap());
        let wedge = manager.inner.sessions.write().await;

        let ended = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while stream.next().await.is_some() {}
        })
        .await;
        assert!(
            ended.is_ok(),
            "the stream must end although close is wedged"
        );

        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        drop(wedge);
        serving.abort();
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_ack_from_another_session_is_ignored_by_manager() {
        let manager = probing_manager(PROBE_STEP, PROBE_STEP * 2);
        let (probed, serving_a) = initialized_session(&manager).await;
        let (other, serving_b) = initialized_session(&manager).await;
        let stream = manager.create_standalone_stream(&probed).await.unwrap();
        let task = spawn_drain(&manager, &other, stream, true);

        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap_or_else(|_| panic!("a probe answered in another session must still time out"))
            .unwrap();
        serving_a.abort();
        serving_b.abort();
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_probe_replies_are_consumed_and_other_messages_forwarded() {
        let manager = probing_manager(PROBE_STEP, PROBE_STEP * 2);
        let (id, _transport) = manager.create_session().await.unwrap();
        manager.inner.sessions.write().await.remove(&id);
        let message = |json: serde_json::Value| -> ClientJsonRpcMessage {
            serde_json::from_value(json).unwrap()
        };

        let response =
            message(serde_json::json!({"jsonrpc": "2.0", "id": "mcpls-liveness-99", "result": {}}));
        assert!(manager.accept_message(&id, response).await.is_ok());

        let probe_error = message(serde_json::json!({
            "jsonrpc": "2.0", "id": "mcpls-liveness-99",
            "error": {"code": -32601, "message": "no ping"},
        }));
        assert!(manager.accept_message(&id, probe_error).await.is_ok());

        let anonymous_error = message(serde_json::json!({
            "jsonrpc": "2.0", "error": {"code": -32700, "message": "parse"},
        }));
        assert_matches!(
            manager.accept_message(&id, anonymous_error).await,
            Err(CappedSessionManagerError::SessionGone)
        );
        let foreign = message(serde_json::json!({"jsonrpc": "2.0", "id": 5, "result": {}}));
        assert_matches!(
            manager.accept_message(&id, foreign).await,
            Err(CappedSessionManagerError::SessionGone)
        );
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_disabled_liveness_never_pings_or_ends_stream() {
        use futures::StreamExt as _;

        let manager = std::sync::Arc::new(CappedSessionManager::new(
            crate::SessionLimit::new(1).unwrap(),
            idle_secs(3600),
        ));
        let (id, serving) = initialized_session(&manager).await;
        let mut stream = Box::pin(manager.create_standalone_stream(&id).await.unwrap());

        let next = tokio::time::timeout(std::time::Duration::from_mins(2), stream.next()).await;
        assert!(next.is_err(), "a disabled stream must stay silent and open");
        serving.abort();
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_request_wise_resume_is_not_tracked_as_standalone_stream() {
        let (manager, id, serving) = probed_session().await;
        let liveness = manager.session_liveness(&id).unwrap();

        manager.resume(&id, "3/2".to_owned()).await.err();
        assert!(!liveness.has_primary());

        let _common = manager.resume(&id, "0".to_owned()).await.unwrap();
        assert!(liveness.has_primary());
        serving.abort();
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the value lives to the end of the scope; the lint misreads its internal lock as an early-droppable temporary"
    )]
    #[tokio::test(start_paused = true)]
    async fn test_probing_fails_closed_when_session_slot_is_gone() {
        let (manager, id, serving) = probed_session().await;
        crate::util::lock_std(&manager.slots).remove(&id);

        let error = manager.create_standalone_stream(&id).await.err().unwrap();
        assert_matches!(error, CappedSessionManagerError::SessionGone);
        assert!(!error.to_string().contains(&*id), "{error}");
        serving.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn test_spawn_bounded_close_drops_a_wedged_close_at_the_timeout() {
        struct SetOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = SetOnDrop(std::sync::Arc::clone(&dropped));
        let id: SessionId = "wedged".into();
        let closer = spawn_bounded_close("closing wedged session", id, async move {
            let _guard = guard;
            std::future::pending::<Result<(), std::convert::Infallible>>().await
        });

        tokio::task::yield_now().await;
        tokio::time::advance(SESSION_CLOSE_TIMEOUT + std::time::Duration::from_secs(1)).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), closer)
            .await
            .unwrap()
            .unwrap();
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn test_supervise_restarts_a_panicking_reaper_until_it_runs_and_ends_on_cancel() {
        let runs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cancel = tokio_util::sync::CancellationToken::new();
        let supervisor = {
            let (runs, cancel) = (std::sync::Arc::clone(&runs), cancel.clone());
            tokio::spawn(async move {
                let waiting = cancel.clone();
                supervise(
                    || {
                        let run = runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let waiting = waiting.clone();
                        async move {
                            assert!(run >= 2, "reaper boom");
                            waiting.cancelled().await;
                        }
                    },
                    &cancel,
                    REAPER_RESTART_DELAY,
                )
                .await;
            })
        };

        tokio::time::sleep(REAPER_RESTART_DELAY * 3).await;
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 3);

        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), supervisor)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn test_supervise_stops_when_cancelled_during_the_restart_delay() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let supervisor = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                supervise(|| async { panic!("always") }, &cancel, REAPER_RESTART_DELAY).await;
            })
        };
        tokio::task::yield_now().await;
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_millis(1), supervisor)
            .await
            .unwrap()
            .unwrap();
    }
}
