//! Cancellation of in-flight LSP requests.
//!
//! The pending table is the single source of truth for "is this request still
//! outstanding": only a request whose slot is still waiting can be cancelled,
//! so `$/cancelRequest` goes out at most once per id however a cancel, a
//! timeout, a reply and a bulk failure race. [`InFlightRequest`] ties that to
//! the lifetime of the future awaiting the request, so dropping the future
//! cancels the request on the server.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex as StdMutex};

use lsp_types::Notification as _;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::oneshot;
use tracing::debug;

use super::LspClient;
use super::notification::OutboundNotification;
use crate::error::Result;
use crate::lsp::types::RequestId;
use crate::util::lock_std;

/// How many cancelled request ids are remembered to recognise their late
/// replies; the oldest is forgotten first.
///
/// A forgotten id only degrades its late reply to the "unknown request id"
/// warning, so a server that never answers cannot grow the table.
pub(super) const MAX_CANCELLED_TOMBSTONES: usize = 256;

/// The side that waits for the reply to one request.
pub(super) type Responder = oneshot::Sender<Result<Value>>;

/// Why mcpls stopped waiting for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CancelCause {
    /// The client-side timeout elapsed.
    Timeout,
    /// The future awaiting the reply was dropped.
    Abandoned,
}

#[derive(Debug)]
enum Slot {
    Waiting(Responder),
    Cancelled(CancelCause),
}

/// What became of the slot a reply was addressed to.
#[derive(Debug)]
pub(super) enum Resolution {
    /// A caller is waiting for this reply.
    Waiting(Responder),
    /// mcpls cancelled the request; the reply is dropped quietly.
    Cancelled(CancelCause),
    /// No such request.
    Unknown,
}

#[derive(Debug, Default)]
struct Table {
    slots: HashMap<RequestId, Slot>,
    /// Ids of the [`Slot::Cancelled`] slots, oldest first.
    tombstones: VecDeque<RequestId>,
}

/// Requests awaiting a reply, shared by a client, its clones and its message
/// loop.
///
/// Every operation is one short map operation under a `std` mutex, so a
/// [`Drop`] can use it.
#[derive(Debug, Clone, Default)]
pub(super) struct PendingRequests(Arc<StdMutex<Table>>);

impl PendingRequests {
    /// Starts waiting for the reply to `id`.
    pub(super) fn register(&self, id: RequestId, responder: Responder) {
        lock_std(&self.0).slots.insert(id, Slot::Waiting(responder));
    }

    /// Takes the slot of `id` out of the table because its reply arrived.
    pub(super) fn resolve(&self, id: &RequestId) -> Resolution {
        let mut table = lock_std(&self.0);
        let slot = table.slots.remove(id);
        if matches!(slot, Some(Slot::Cancelled(_))) {
            table.tombstones.retain(|tombstone| tombstone != id);
        }
        drop(table);
        match slot {
            Some(Slot::Waiting(responder)) => Resolution::Waiting(responder),
            Some(Slot::Cancelled(cause)) => Resolution::Cancelled(cause),
            None => Resolution::Unknown,
        }
    }

    /// Turns a waiting slot into a tombstone. Returns whether it was waiting,
    /// that is, whether the caller owns the one cancel of this id.
    fn cancel(&self, id: &RequestId, cause: CancelCause) -> bool {
        let mut table = lock_std(&self.0);
        let Some(slot) = table.slots.get_mut(id) else {
            return false;
        };
        if !matches!(slot, Slot::Waiting(_)) {
            return false;
        }
        *slot = Slot::Cancelled(cause);
        table.tombstones.push_back(id.clone());
        if table.tombstones.len() > MAX_CANCELLED_TOMBSTONES
            && let Some(oldest) = table.tombstones.pop_front()
        {
            table.slots.remove(&oldest);
        }
        true
    }

    /// Drops a waiting slot nobody will resolve.
    fn forget(&self, id: &RequestId) {
        let mut table = lock_std(&self.0);
        if matches!(table.slots.get(id), Some(Slot::Waiting(_))) {
            table.slots.remove(id);
        }
    }

    /// Empties the table and hands over everyone still waiting, so the caller
    /// can fail them. Tombstones are dropped: no reply will come.
    pub(super) fn drain_waiting(&self) -> Vec<Responder> {
        let mut table = lock_std(&self.0);
        table.tombstones.clear();
        table
            .slots
            .drain()
            .filter_map(|(_, slot)| match slot {
                Slot::Waiting(responder) => Some(responder),
                Slot::Cancelled(_) => None,
            })
            .collect()
    }

    /// Whether no caller is waiting for a reply.
    #[cfg(test)]
    pub(super) fn no_waiting(&self) -> bool {
        !lock_std(&self.0)
            .slots
            .values()
            .any(|slot| matches!(slot, Slot::Waiting(_)))
    }

    /// How many cancelled requests are remembered.
    #[cfg(test)]
    pub(super) fn tombstone_count(&self) -> usize {
        lock_std(&self.0).tombstones.len()
    }
}

/// Params of `$/cancelRequest`; keeps the full width of [`RequestId`], which
/// the narrower generated type would truncate.
#[derive(Debug, Serialize)]
struct CancelParams<'a> {
    id: &'a RequestId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardState {
    /// Registered, not yet enqueued: the server cannot know the id.
    Registered,
    /// Enqueued; dropping the guard cancels the request.
    Sent,
    /// Answered or deliberately abandoned; dropping does nothing.
    Settled,
}

/// The lifetime of one request attempt.
///
/// Dropping it while the request is outstanding cancels the request on the
/// server with `$/cancelRequest` and leaves a tombstone so the late reply is
/// dropped quietly. Dropping it after the reply, after a bulk failure or
/// before the request was enqueued sends nothing.
#[derive(Debug)]
#[must_use = "dropping the guard ends the request's registration"]
pub(super) struct InFlightRequest<'a> {
    client: &'a LspClient,
    id: RequestId,
    state: GuardState,
}

impl<'a> InFlightRequest<'a> {
    /// Registers `id` as waiting on `responder`.
    pub(super) fn register(client: &'a LspClient, id: RequestId, responder: Responder) -> Self {
        client.pending_requests.register(id.clone(), responder);
        Self {
            client,
            id,
            state: GuardState::Registered,
        }
    }

    /// Records that the request is on the command channel, so the server will
    /// see the id before any cancel for it.
    pub(super) const fn mark_sent(&mut self) {
        self.state = GuardState::Sent;
    }

    /// Records that the outcome was consumed; nothing is left to cancel.
    pub(super) const fn mark_settled(&mut self) {
        self.state = GuardState::Settled;
    }

    /// Ends the attempt because its timeout elapsed.
    pub(super) fn expire(mut self) {
        self.cancel(CancelCause::Timeout);
        self.state = GuardState::Settled;
    }

    fn cancel(&self, cause: CancelCause) {
        if self.state != GuardState::Sent || self.client.cancels_suppressed() {
            self.client.pending_requests.forget(&self.id);
            return;
        }
        if self.client.pending_requests.cancel(&self.id, cause) {
            self.client.send_cancel(&self.id, cause);
        }
    }
}

impl Drop for InFlightRequest<'_> {
    fn drop(&mut self) {
        if self.state != GuardState::Settled {
            self.cancel(CancelCause::Abandoned);
        }
    }
}

impl LspClient {
    /// Whether no `$/cancelRequest` may go out any more: the `shutdown`
    /// request was issued or the client is shutting down.
    fn cancels_suppressed(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// Stops all later cancels from being sent.
    pub(crate) fn suppress_cancels(&self) {
        self.closing.store(true, Ordering::Release);
    }

    /// Enqueues `$/cancelRequest` for `id` behind the request it cancels,
    /// without waiting: a full or closed queue is logged and ignored.
    fn send_cancel(&self, id: &RequestId, cause: CancelCause) {
        let notification = match OutboundNotification::new(
            lsp_types::CancelNotification::METHOD.as_str(),
            CancelParams { id },
        ) {
            Ok(notification) => notification,
            Err(error) => {
                debug!(?id, %error, "could not encode $/cancelRequest");
                return;
            }
        };
        match self.try_send_notification(notification) {
            Ok(()) => debug!(?id, ?cause, "sent $/cancelRequest"),
            Err(error) => debug!(?id, ?cause, %error, "dropped $/cancelRequest"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::time::Duration;

    use serde_json::json;
    use tokio::io::BufReader;
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;
    use tracing_subscriber::prelude::*;

    use super::super::ClientCommand;
    use super::*;
    use crate::config::LspServerConfig;
    use crate::error::Error;
    use crate::test_lsp::{
        CapturedLogs, fake_lsp_client, read_framed_message, write_error_response, write_response,
    };

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn id(n: i64) -> RequestId {
        RequestId::Number(n)
    }

    fn waiting(table: &PendingRequests, n: i64) -> oneshot::Receiver<Result<Value>> {
        let (tx, rx) = oneshot::channel();
        table.register(id(n), tx);
        rx
    }

    #[test]
    fn a_waiting_slot_is_cancelled_once() {
        let table = PendingRequests::default();
        let _rx = waiting(&table, 1);
        assert!(table.cancel(&id(1), CancelCause::Timeout));
        assert!(!table.cancel(&id(1), CancelCause::Abandoned));
        assert_eq!(table.tombstone_count(), 1);
        assert!(table.no_waiting());
    }

    #[test]
    fn nothing_is_cancelled_for_an_absent_or_resolved_id() {
        let table = PendingRequests::default();
        assert!(!table.cancel(&id(1), CancelCause::Abandoned));
        let _rx = waiting(&table, 2);
        assert_matches!(table.resolve(&id(2)), Resolution::Waiting(_));
        assert!(!table.cancel(&id(2), CancelCause::Abandoned));
        assert_eq!(table.tombstone_count(), 0);
    }

    #[test]
    fn a_reply_consumes_the_tombstone() {
        let table = PendingRequests::default();
        let _rx = waiting(&table, 1);
        assert!(table.cancel(&id(1), CancelCause::Timeout));
        assert_matches!(
            table.resolve(&id(1)),
            Resolution::Cancelled(CancelCause::Timeout)
        );
        assert_matches!(table.resolve(&id(1)), Resolution::Unknown);
        assert_eq!(table.tombstone_count(), 0);
    }

    #[test]
    fn tombstones_are_bounded_and_the_oldest_goes_first() {
        let table = PendingRequests::default();
        let total = i64::try_from(MAX_CANCELLED_TOMBSTONES).unwrap() + 3;
        let _receivers: Vec<_> = (0..total).map(|n| waiting(&table, n)).collect();
        for n in 0..total {
            assert!(table.cancel(&id(n), CancelCause::Abandoned));
        }
        assert_eq!(table.tombstone_count(), MAX_CANCELLED_TOMBSTONES);
        assert_matches!(table.resolve(&id(0)), Resolution::Unknown);
        assert_matches!(table.resolve(&id(total - 1)), Resolution::Cancelled(_));
    }

    #[test]
    fn forget_removes_only_a_waiting_slot() {
        let table = PendingRequests::default();
        let _first = waiting(&table, 1);
        let _second = waiting(&table, 2);
        assert!(table.cancel(&id(2), CancelCause::Abandoned));
        table.forget(&id(1));
        table.forget(&id(2));
        assert_matches!(table.resolve(&id(1)), Resolution::Unknown);
        assert_matches!(table.resolve(&id(2)), Resolution::Cancelled(_));
    }

    #[test]
    fn draining_hands_over_waiters_and_drops_tombstones() {
        let table = PendingRequests::default();
        let _first = waiting(&table, 1);
        let _second = waiting(&table, 2);
        assert!(table.cancel(&id(2), CancelCause::Timeout));
        assert_eq!(table.drain_waiting().len(), 1);
        assert_eq!(table.tombstone_count(), 0);
        assert_matches!(table.resolve(&id(2)), Resolution::Unknown);
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Model {
        Waiting,
        Cancelled,
        Absent,
    }

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Register(usize),
        Cancel(usize),
        Resolve(usize),
        Forget(usize),
        Drain,
    }

    fn op() -> impl proptest::strategy::Strategy<Value = Op> {
        use proptest::prelude::*;
        prop_oneof![
            (0..4usize).prop_map(Op::Register),
            (0..4usize).prop_map(Op::Cancel),
            (0..4usize).prop_map(Op::Resolve),
            (0..4usize).prop_map(Op::Forget),
            Just(Op::Drain),
        ]
    }

    fn id_of(slot: usize) -> RequestId {
        id(i64::try_from(slot).unwrap())
    }

    proptest::proptest! {
        /// FR-008: whatever the interleaving of replies, cancels, forgets and
        /// bulk failures, an id is cancelled only while it is still waiting,
        /// and then only once.
        #[test]
        fn an_id_is_cancelled_at_most_once_and_only_while_waiting(
            ops in proptest::collection::vec(op(), 0..64)
        ) {
            let table = PendingRequests::default();
            let mut model = [Model::Absent; 4];
            for op in ops {
                match op {
                    Op::Register(n) if model[n] == Model::Absent => {
                        drop(waiting(&table, i64::try_from(n).unwrap()));
                        model[n] = Model::Waiting;
                    }
                    Op::Register(_) => {}
                    Op::Cancel(n) => {
                        let cancelled = table.cancel(&id_of(n), CancelCause::Abandoned);
                        proptest::prop_assert_eq!(cancelled, model[n] == Model::Waiting);
                        if cancelled {
                            model[n] = Model::Cancelled;
                        }
                    }
                    Op::Resolve(n) => {
                        let resolved = table.resolve(&id_of(n));
                        let expected = model[n];
                        let observed = match resolved {
                            Resolution::Waiting(_) => Model::Waiting,
                            Resolution::Cancelled(_) => Model::Cancelled,
                            Resolution::Unknown => Model::Absent,
                        };
                        proptest::prop_assert_eq!(observed, expected);
                        model[n] = Model::Absent;
                    }
                    Op::Forget(n) => {
                        table.forget(&id_of(n));
                        if model[n] == Model::Waiting {
                            model[n] = Model::Absent;
                        }
                    }
                    Op::Drain => {
                        table.drain_waiting();
                        model = [Model::Absent; 4];
                    }
                }
                let tombstones = model.iter().filter(|m| **m == Model::Cancelled).count();
                proptest::prop_assert_eq!(table.tombstone_count(), tombstones);
            }
        }
    }

    fn spawn_request(client: &LspClient, method: &'static str) -> JoinHandle<Result<Value>> {
        let client = client.clone();
        tokio::spawn(async move { client.request(method, json!({}), TIMEOUT).await })
    }

    async fn drop_task(task: JoinHandle<Result<Value>>) {
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }

    fn capture_logs() -> (CapturedLogs, tracing::subscriber::DefaultGuard) {
        let logs = CapturedLogs::default();
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::registry()
                .with(tracing_subscriber::filter::LevelFilter::TRACE)
                .with(logs.clone()),
        );
        (logs, guard)
    }

    fn above_debug(logs: &CapturedLogs) -> Vec<String> {
        logs.entries()
            .into_iter()
            .filter(|(level, _)| *level <= tracing::Level::WARN)
            .map(|(_, message)| message)
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_an_outstanding_request_cancels_it_once() {
        let (logs, _guard) = capture_logs();
        let (client, mut server) = fake_lsp_client();
        let mut reader = BufReader::new(&mut server.write_stdout);

        let task = spawn_request(&client, "textDocument/references");
        let request = read_framed_message(&mut reader).await;
        drop_task(task).await;

        let cancel = read_framed_message(&mut reader).await;
        assert_eq!(cancel["method"], "$/cancelRequest");
        assert_eq!(cancel["params"]["id"], request["id"]);
        assert!(cancel.get("id").is_none(), "a notification carries no id");

        write_error_response(
            &mut server.read_half_stdin,
            &request["id"],
            -32800,
            "cancelled",
        )
        .await;
        let next = spawn_request(&client, "textDocument/hover");
        let next_request = read_framed_message(&mut reader).await;
        assert_eq!(next_request["method"], "textDocument/hover");
        write_response(&mut server.read_half_stdin, &next_request["id"], json!(1)).await;
        assert_eq!(next.await.unwrap().unwrap(), json!(1));

        assert!(client.pending_requests.no_waiting());
        assert_eq!(client.pending_requests.tombstone_count(), 0);
        assert!(above_debug(&logs).is_empty(), "{:?}", logs.entries());
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_cancels_the_request_and_its_late_reply_is_quiet() {
        let (logs, _guard) = capture_logs();
        let (client, mut server) = fake_lsp_client();
        let mut reader = BufReader::new(&mut server.write_stdout);

        let task = spawn_request(&client, "textDocument/references");
        let request = read_framed_message(&mut reader).await;
        tokio::time::advance(TIMEOUT + Duration::from_secs(1)).await;
        assert_matches!(task.await.unwrap(), Err(Error::Timeout(limit)) if limit == TIMEOUT);

        let cancel = read_framed_message(&mut reader).await;
        assert_eq!(cancel["method"], "$/cancelRequest");
        assert_eq!(cancel["params"]["id"], request["id"]);

        write_response(&mut server.read_half_stdin, &request["id"], json!("late")).await;
        let next = spawn_request(&client, "textDocument/hover");
        let next_request = read_framed_message(&mut reader).await;
        assert_eq!(next_request["method"], "textDocument/hover");
        write_response(&mut server.read_half_stdin, &next_request["id"], json!(2)).await;
        next.await.unwrap().unwrap();

        assert_eq!(client.pending_requests.tombstone_count(), 0);
        assert!(above_debug(&logs).is_empty(), "{:?}", logs.entries());
    }

    #[tokio::test(start_paused = true)]
    async fn a_completed_request_sends_no_cancel() {
        let (client, mut server) = fake_lsp_client();
        let mut reader = BufReader::new(&mut server.write_stdout);

        let first = spawn_request(&client, "textDocument/hover");
        let first_request = read_framed_message(&mut reader).await;
        write_response(&mut server.read_half_stdin, &first_request["id"], json!(1)).await;
        first.await.unwrap().unwrap();

        let second = spawn_request(&client, "textDocument/hover");
        let second_request = read_framed_message(&mut reader).await;
        assert_eq!(second_request["method"], "textDocument/hover");
        assert_ne!(second_request["id"], first_request["id"]);
        drop_task(second).await;
        let cancel = read_framed_message(&mut reader).await;
        assert_eq!(cancel["params"]["id"], second_request["id"]);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_during_retry_backoff_allocates_no_id_and_sends_no_cancel() {
        let (client, mut server) = fake_lsp_client();
        let mut reader = BufReader::new(&mut server.write_stdout);

        let task = spawn_request(&client, "textDocument/hover");
        let request = read_framed_message(&mut reader).await;
        write_error_response(&mut server.read_half_stdin, &request["id"], -32802, "busy").await;
        tokio::time::advance(Duration::from_millis(100)).await;
        drop_task(task).await;
        tokio::time::advance(Duration::from_secs(10)).await;

        let next = spawn_request(&client, "textDocument/hover");
        let next_request = read_framed_message(&mut reader).await;
        assert_eq!(next_request["method"], "textDocument/hover");
        assert_eq!(
            next_request["id"].as_i64().unwrap(),
            request["id"].as_i64().unwrap() + 1,
            "no id may be spent on the abandoned retry"
        );
        drop_task(next).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_bulk_failure_leaves_nothing_to_cancel() {
        let (client, mut server) = fake_lsp_client();
        let mut reader = BufReader::new(&mut server.write_stdout);

        let task = spawn_request(&client, "textDocument/hover");
        let _request = read_framed_message(&mut reader).await;
        client.fail_pending_requests();
        assert_matches!(task.await.unwrap(), Err(Error::ServerTerminated));

        let next = spawn_request(&client, "textDocument/hover");
        let next_request = read_framed_message(&mut reader).await;
        assert_eq!(next_request["method"], "textDocument/hover");
        drop_task(next).await;
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_cancelled_once_cancels_are_suppressed() {
        let (client, mut server) = fake_lsp_client();
        let mut reader = BufReader::new(&mut server.write_stdout);

        client.suppress_cancels();
        let stalled = spawn_request(&client, "textDocument/references");
        let request = read_framed_message(&mut reader).await;
        assert_eq!(request["method"], "textDocument/references");
        tokio::time::advance(TIMEOUT + Duration::from_secs(1)).await;
        assert_matches!(stalled.await.unwrap(), Err(Error::Timeout(_)));

        let hover = spawn_request(&client, "textDocument/hover");
        let hover_request = read_framed_message(&mut reader).await;
        assert_eq!(hover_request["method"], "textDocument/hover");
        drop_task(hover).await;
        client.notify("custom/probe", json!({})).await.unwrap();
        let probe = read_framed_message(&mut reader).await;
        assert_eq!(probe["method"], "custom/probe");
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_queue_does_not_fail_the_cancelled_call() {
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let client = LspClient {
            command_tx,
            ..LspClient::new(LspServerConfig::rust_analyzer())
        };
        let task = spawn_request(&client, "textDocument/hover");
        tokio::task::yield_now().await;
        assert_matches!(
            command_rx.recv().await,
            Some(ClientCommand::SendRequest { .. })
        );
        let blocked = spawn_request(&client, "textDocument/hover");
        tokio::task::yield_now().await;
        let filler = spawn_request(&client, "textDocument/hover");
        tokio::task::yield_now().await;
        drop_task(task).await;
        drop_task(blocked).await;
        drop_task(filler).await;
        assert!(client.pending_requests.no_waiting());
    }
}
