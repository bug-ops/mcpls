//! Liveness probing of standalone GET (SSE) streams.
//!
//! A peer that vanished without closing its connection (sleeping laptop,
//! dropped NAT mapping, proxy that kept the upstream open) leaves its GET
//! stream open until the OS gives up retransmitting. Each probed stream
//! therefore runs a forwarding task that sends an MCP `ping` request every
//! interval and ends the stream when the client does not answer it in time.
//!
//! The task owns the inner `rmcp` stream and the [`StreamGuard`], and feeds a
//! bounded channel that the HTTP layer drains. It never awaits a plain send
//! on that channel, so a stalled consumer cannot keep it from reaching its
//! deadline. A probe waits for a free slot ahead of queued messages, so a
//! client that recovers within the deadline still receives it. Ending the
//! task drops the inner receiver, which releases an `rmcp` worker parked on a
//! full common channel.

use std::collections::HashMap;
use std::str::FromStr as _;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures::{Stream, StreamExt as _};
use rmcp::model::{
    ClientJsonRpcMessage, JsonRpcMessage, PingRequest, RequestId, ServerJsonRpcMessage,
    ServerRequest,
};
use rmcp::transport::streamable_http_server::session::local::{EventId, LocalSessionManager};
use rmcp::transport::streamable_http_server::session::{ServerSseMessage, SessionId};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::{ProbeDeadline, ProbeInterval, StreamGuard};
use crate::bridge::lock_std;

const PROBE_ID_PREFIX: &str = "mcpls-liveness-";
const OUTBOUND_CAPACITY: usize = 16;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const FAR_FUTURE: Duration = Duration::from_secs(946_080_000);

/// `delay` from now, saturating at [`FAR_FUTURE`] so an absurd configured
/// duration cannot overflow `Instant`.
fn after(delay: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(delay)
        .or_else(|| now.checked_add(FAR_FUTURE))
        .unwrap_or(now)
}

/// Identifier of one liveness probe, unique within a session.
///
/// Travels as the JSON-RPC id `mcpls-liveness-<n>`; `rmcp`'s own server
/// request ids are numeric, so the two cannot collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ProbeId(u64);

impl ProbeId {
    pub(super) fn request_id(self) -> RequestId {
        RequestId::String(format!("{PROBE_ID_PREFIX}{}", self.0).into())
    }

    pub(super) fn from_request_id(id: &RequestId) -> Option<Self> {
        match id {
            RequestId::String(id) => id.strip_prefix(PROBE_ID_PREFIX)?.parse().ok().map(Self),
            RequestId::Number(_) => None,
        }
    }

    /// The probe a client message replies to, if it is a response or an
    /// error that carries a probe id.
    pub(super) fn answered_by(message: &ClientJsonRpcMessage) -> Option<Self> {
        let id = match message {
            JsonRpcMessage::Response(response) => &response.id,
            JsonRpcMessage::Error(error) => error.id.as_ref()?,
            JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => return None,
        };
        Self::from_request_id(id)
    }
}

/// Identity of one probed standalone stream within its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamToken(u64);

#[derive(Debug, Default)]
struct LivenessState {
    primary: Option<StreamToken>,
    next_token: u64,
    next_probe: u64,
    pending: HashMap<ProbeId, oneshot::Sender<()>>,
}

/// Probe bookkeeping of one session: which probed stream mirrors `rmcp`'s
/// primary common channel, and the probes awaiting an answer.
///
/// Pending probes live per session so one session can never answer another's
/// probe.
#[derive(Debug, Default)]
pub(super) struct SessionLiveness(StdMutex<LivenessState>);

impl SessionLiveness {
    /// Registers a new standalone stream. It becomes the primary when no
    /// tracked primary task is alive, mirroring `rmcp`'s own decision to
    /// replace a dead common channel rather than open a shadow stream.
    fn claim_standalone(&self) -> StreamToken {
        let mut state = lock_std(&self.0);
        let token = StreamToken(state.next_token);
        state.next_token = state.next_token.wrapping_add(1);
        if state.primary.is_none() {
            state.primary = Some(token);
        }
        token
    }

    /// Unregisters `token`; whether it was the primary.
    fn release(&self, token: StreamToken) -> bool {
        let mut state = lock_std(&self.0);
        let was_primary = state.primary == Some(token);
        if was_primary {
            state.primary = None;
        }
        was_primary
    }

    #[cfg(test)]
    pub(super) fn has_primary(&self) -> bool {
        lock_std(&self.0).primary.is_some()
    }

    fn register_probe(&self) -> (ProbeId, oneshot::Receiver<()>) {
        let (answer, answered) = oneshot::channel();
        let id = {
            let mut state = lock_std(&self.0);
            let id = ProbeId(state.next_probe);
            state.next_probe = state.next_probe.wrapping_add(1);
            state.pending.insert(id, answer);
            id
        };
        (id, answered)
    }

    fn forget_probe(&self, id: ProbeId) {
        lock_std(&self.0).pending.remove(&id);
    }

    /// Marks `id` answered; a late or unknown id is a no-op.
    pub(super) fn acknowledge(&self, id: ProbeId) {
        let answer = lock_std(&self.0).pending.remove(&id);
        if let Some(answer) = answer {
            // The receiver is gone only when the stream already ended.
            answer.send(()).ok();
        }
    }
}

/// Whether a `Last-Event-ID` names the common (standalone) channel.
///
/// `rmcp` keeps the request-wise channel id private, so this relies on the
/// `<index>/<request>` versus `<index>` rendering of [`EventId`]; a unit test
/// pins that format.
pub(super) fn is_common_channel_event_id(last_event_id: &str) -> bool {
    EventId::from_str(last_event_id).is_ok_and(|id| !id.to_string().contains('/'))
}

fn ping_message(id: ProbeId) -> ServerSseMessage {
    let mut message = ServerSseMessage::default();
    message.message = Some(Arc::new(ServerJsonRpcMessage::request(
        ServerRequest::PingRequest(PingRequest::default()),
        id.request_id(),
    )));
    message
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    ClientGone,
    InnerEnded,
    Unresponsive,
}

/// An unanswered probe: its id and the answer to wait for.
struct Outstanding {
    id: ProbeId,
    answered: oneshot::Receiver<()>,
}

async fn expiry_or_never(expires_at: Option<Instant>) {
    match expires_at {
        Some(expires_at) => tokio::time::sleep_until(expires_at).await,
        None => std::future::pending().await,
    }
}

async fn answer_or_never(outstanding: Option<&mut Outstanding>) {
    match outstanding {
        // A dropped sender also ends the wait; only the owning task forgets
        // probes.
        Some(outstanding) => (&mut outstanding.answered).await.unwrap_or_default(),
        None => std::future::pending().await,
    }
}

/// Everything a forwarding task needs besides the stream itself.
pub(super) struct StreamProbe {
    pub(super) liveness: Arc<SessionLiveness>,
    pub(super) interval: ProbeInterval,
    pub(super) deadline: ProbeDeadline,
    pub(super) manager: Arc<LocalSessionManager>,
    pub(super) session: SessionId,
}

impl StreamProbe {
    /// Moves `inner` and `guard` into a forwarding task and returns the
    /// stream the HTTP layer polls.
    ///
    /// The probe role (primary or shadow) is decided here, after the inner
    /// call that created `inner` returned, so a sequential reconnect cannot
    /// leave a promoted primary tracked as a shadow. Two concurrent GETs on
    /// one session can still invert, and a chained inversion can leave a live
    /// shadow unkicked; both are rare and end when the client reconnects.
    pub(super) fn forward<S>(
        self,
        inner: S,
        guard: Option<StreamGuard>,
    ) -> impl Stream<Item = ServerSseMessage> + Send + Sync + 'static
    where
        S: Stream<Item = ServerSseMessage> + Send + 'static,
    {
        let token = self.liveness.claim_standalone();
        let (tx, mut rx) = mpsc::channel(OUTBOUND_CAPACITY);
        tokio::spawn(self.run(token, inner, tx, guard));
        futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
    }

    async fn run<S>(
        self,
        token: StreamToken,
        inner: S,
        tx: mpsc::Sender<ServerSseMessage>,
        guard: Option<StreamGuard>,
    ) where
        S: Stream<Item = ServerSseMessage> + Send + 'static,
    {
        let mut inner = Box::pin(inner);
        let mut held: Option<ServerSseMessage> = None;
        let mut ping: Option<ServerSseMessage> = None;
        let mut inner_done = false;
        let mut outstanding: Option<Outstanding> = None;
        let mut expires_at: Option<Instant> = None;
        let mut next_probe_at = after(self.interval.get());

        let exit = loop {
            if inner_done && held.is_none() && ping.is_none() {
                break Exit::InnerEnded;
            }
            tokio::select! {
                () = tx.closed() => break Exit::ClientGone,
                () = expiry_or_never(expires_at) => break Exit::Unresponsive,
                () = answer_or_never(outstanding.as_mut()) => {
                    outstanding = None;
                    expires_at = None;
                    next_probe_at = after(self.interval.get());
                }
                () = tokio::time::sleep_until(next_probe_at), if outstanding.is_none() => {
                    let (id, answered) = self.liveness.register_probe();
                    outstanding = Some(Outstanding { id, answered });
                    expires_at = Some(after(self.deadline.get()));
                    ping = Some(ping_message(id));
                }
                item = inner.next(), if held.is_none() && !inner_done => {
                    match item {
                        Some(message) => held = Some(message),
                        None => inner_done = true,
                    }
                }
                permit = tx.reserve(), if ping.is_some() || held.is_some() => {
                    match permit {
                        Ok(permit) => {
                            if let Some(message) = ping.take().or_else(|| held.take()) {
                                permit.send(message);
                            }
                        }
                        Err(_) => break Exit::ClientGone,
                    }
                }
            }
        };

        if let Some(outstanding) = &outstanding {
            self.liveness.forget_probe(outstanding.id);
        }
        // Clear the primary before dropping the inner receiver: `rmcp`
        // decides primary versus shadow for the next GET from that drop.
        let was_primary = self.liveness.release(token);
        drop(inner);
        drop(guard);
        tracing::debug!(session = %self.session, ?exit, was_primary, "standalone stream ended");
        if exit == Exit::Unresponsive && was_primary {
            self.close_standalone_stream();
        }
    }

    /// Clears the stale shadow streams `rmcp` still holds so the client's
    /// reconnected GET is promoted; detached and bounded so a wedged session
    /// worker cannot stall it.
    fn close_standalone_stream(self) {
        tokio::spawn(async move {
            let closed = tokio::time::timeout(CLOSE_TIMEOUT, async {
                let handle = self
                    .manager
                    .sessions
                    .read()
                    .await
                    .get(&self.session)
                    .cloned();
                match handle {
                    Some(handle) => handle.close_standalone_sse_stream(None).await.err(),
                    None => None,
                }
            })
            .await;
            match closed {
                Ok(None) => {}
                Ok(Some(e)) => {
                    tracing::debug!(session = %self.session, "closing standalone stream failed: {e}");
                }
                Err(_) => {
                    tracing::debug!(session = %self.session, "closing standalone stream timed out");
                }
            }
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::super::SessionActivity;
    use super::*;

    const STEP: Duration = Duration::from_millis(100);

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn probe(interval: Duration, deadline: Duration) -> (StreamProbe, Arc<SessionLiveness>) {
        let liveness = Arc::new(SessionLiveness::default());
        let probe = StreamProbe {
            liveness: Arc::clone(&liveness),
            interval: ProbeInterval::new(interval).unwrap(),
            deadline: ProbeDeadline::new(deadline).unwrap(),
            manager: Arc::new(LocalSessionManager::default()),
            session: Arc::from("test-session"),
        };
        (probe, liveness)
    }

    fn open_streams(activity: &SessionActivity) -> usize {
        lock_std(&activity.0).open_streams
    }

    fn ping_id(message: &ServerSseMessage) -> ProbeId {
        let message = message.message.as_ref().unwrap();
        let json = serde_json::to_value(&**message).unwrap();
        assert_eq!(json["method"], "ping");
        let JsonRpcMessage::Request(request) = &**message else {
            panic!("probe must be a request, got {json}");
        };
        ProbeId::from_request_id(&request.id).unwrap()
    }

    fn client_message(json: serde_json::Value) -> ClientJsonRpcMessage {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn test_probe_id_round_trips_through_request_id() {
        let id = ProbeId(42);
        assert_eq!(ProbeId::from_request_id(&id.request_id()), Some(id));
        assert_eq!(
            id.request_id(),
            RequestId::String("mcpls-liveness-42".into())
        );
    }

    #[test]
    fn test_probe_id_ignores_foreign_request_ids() {
        assert_eq!(ProbeId::from_request_id(&RequestId::Number(7)), None);
        assert_eq!(
            ProbeId::from_request_id(&RequestId::String("7".into())),
            None
        );
        assert_eq!(
            ProbeId::from_request_id(&RequestId::String("mcpls-liveness-x".into())),
            None
        );
    }

    #[test]
    fn test_answered_by_classifies_client_messages() {
        let response = client_message(
            serde_json::json!({"jsonrpc": "2.0", "id": "mcpls-liveness-3", "result": {}}),
        );
        assert_eq!(ProbeId::answered_by(&response), Some(ProbeId(3)));

        let error = client_message(serde_json::json!({
            "jsonrpc": "2.0", "id": "mcpls-liveness-4",
            "error": {"code": -32601, "message": "no ping"},
        }));
        assert_eq!(ProbeId::answered_by(&error), Some(ProbeId(4)));

        let anonymous_error = client_message(serde_json::json!({
            "jsonrpc": "2.0", "error": {"code": -32700, "message": "parse"},
        }));
        assert_eq!(ProbeId::answered_by(&anonymous_error), None);

        let foreign = client_message(serde_json::json!({"jsonrpc": "2.0", "id": 5, "result": {}}));
        assert_eq!(ProbeId::answered_by(&foreign), None);

        let request = client_message(
            serde_json::json!({"jsonrpc": "2.0", "id": "mcpls-liveness-3", "method": "ping"}),
        );
        assert_eq!(ProbeId::answered_by(&request), None);
    }

    #[test]
    fn test_event_id_format_distinguishes_common_from_request_wise() {
        for (raw, common) in [("0", true), ("17", true), ("3/2", false)] {
            let parsed = EventId::from_str(raw).unwrap();
            assert_eq!(parsed.to_string(), raw, "rmcp EventId rendering changed");
            assert_eq!(is_common_channel_event_id(raw), common, "{raw}");
        }
        assert!(!is_common_channel_event_id("not-an-event-id"));
    }

    #[test]
    fn test_primary_is_first_live_stream_and_released_once() {
        let liveness = SessionLiveness::default();
        let first = liveness.claim_standalone();
        let second = liveness.claim_standalone();
        assert!(!liveness.release(second), "second stream is a shadow");
        assert!(liveness.release(first));
        assert!(!liveness.release(first), "primary is cleared only once");

        let third = liveness.claim_standalone();
        assert!(liveness.release(third), "a new stream is promoted");
    }

    #[test]
    fn test_acknowledging_unknown_probe_leaves_pending_probes_untouched() {
        let liveness = SessionLiveness::default();
        let (id, mut answered) = liveness.register_probe();

        liveness.acknowledge(ProbeId(id.0 + 1));
        assert!(answered.try_recv().is_err());
        assert_eq!(lock_std(&liveness.0).pending.len(), 1);

        liveness.acknowledge(id);
        assert!(answered.try_recv().is_ok());
        assert!(lock_std(&liveness.0).pending.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn test_ping_follows_interval_ack_rearms_and_silence_ends_stream() {
        let (probe, liveness) = probe(STEP, STEP * 2);
        let mut outer = Box::pin(probe.forward(futures::stream::pending(), None));
        let started = Instant::now();

        let first = outer.next().await.unwrap();
        assert!(started.elapsed() >= STEP && started.elapsed() < STEP * 2);
        liveness.acknowledge(ping_id(&first));

        let second = outer.next().await.unwrap();
        assert!(started.elapsed() >= STEP * 2 && started.elapsed() < STEP * 3);
        assert_ne!(ping_id(&first), ping_id(&second));

        assert!(
            outer.next().await.is_none(),
            "unanswered probe ends the stream"
        );
        assert!(started.elapsed() >= STEP * 4);
        assert!(lock_std(&liveness.0).pending.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn test_unpolled_outer_stream_still_ends_within_interval_plus_deadline() {
        let activity = SessionActivity::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let inner = futures::stream::unfold(DropFlag(Arc::clone(&dropped)), |flag| async move {
            Some((ServerSseMessage::default(), flag))
        });
        let (probe, _liveness) = probe(STEP, STEP * 2);
        let outer = probe.forward(inner, Some(activity.open_stream()));
        assert_eq!(open_streams(&activity), 1);

        tokio::time::sleep(STEP * 3 + STEP / 2).await;

        assert!(
            dropped.load(Ordering::SeqCst),
            "the inner stream must be dropped so rmcp's worker is released"
        );
        assert_eq!(open_streams(&activity), 0);
        drop(outer);
    }

    #[tokio::test(start_paused = true)]
    async fn test_dropping_outer_stream_ends_task_and_releases_guard() {
        let activity = SessionActivity::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let inner = futures::stream::unfold(DropFlag(Arc::clone(&dropped)), |flag| async move {
            std::future::pending::<()>().await;
            Some((ServerSseMessage::default(), flag))
        });
        let (probe, liveness) = probe(Duration::from_secs(60), Duration::from_secs(30));
        let outer = probe.forward(inner, Some(activity.open_stream()));

        drop(outer);
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(open_streams(&activity), 0);
        assert!(lock_std(&liveness.0).primary.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn test_inner_messages_are_forwarded_in_order_and_end_closes_outer() {
        let (probe, _liveness) = probe(Duration::from_secs(60), Duration::from_secs(30));
        let messages = (0..3).map(|n| {
            let mut message = ServerSseMessage::default();
            message.event_id = Some(n.to_string());
            message
        });
        let outer = probe.forward(futures::stream::iter(messages), None);
        let ids: Vec<_> = outer
            .map(|message| message.event_id.unwrap())
            .collect()
            .await;
        assert_eq!(ids, ["0", "1", "2"]);
    }

    #[tokio::test(start_paused = true)]
    async fn test_ack_from_another_session_does_not_keep_stream_alive() {
        let (probe, _liveness) = probe(STEP, STEP * 2);
        let other = SessionLiveness::default();
        let mut outer = Box::pin(probe.forward(futures::stream::pending(), None));

        let ping = outer.next().await.unwrap();
        other.acknowledge(ping_id(&ping));

        assert!(outer.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn test_probe_dropped_by_full_channel_is_still_delivered_when_client_recovers() {
        let inner =
            futures::stream::unfold((), |()| async { Some((ServerSseMessage::default(), ())) });
        let (probe, liveness) = probe(STEP, STEP * 2);
        let mut outer = Box::pin(probe.forward(inner, None));

        tokio::time::sleep(STEP + STEP / 2).await;

        let mut drained = 0;
        let ping = loop {
            let message = outer.next().await.unwrap();
            if message.message.is_some() {
                break message;
            }
            drained += 1;
            assert!(drained <= OUTBOUND_CAPACITY * 2, "probe never arrived");
        };
        liveness.acknowledge(ping_id(&ping));
        assert!(
            outer.next().await.is_some(),
            "an answered stream stays open"
        );
    }
}
