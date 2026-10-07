//! Delivery of `textDocument/publishDiagnostics` from a server's message loop
//! to the bridge.
//!
//! A server may publish for many files in one burst, faster than the bridge
//! drains them. A bounded channel would drop the tail silently, so
//! diagnostics travel through a per-server mailbox instead: one pending
//! publish per file (a later publish supersedes an earlier one), bounded in
//! files and in bytes. A publish the mailbox cannot hold is not discarded
//! quietly: its file is recorded as lost, and the reader hands the lost files
//! over so the cache can say so ([`PublishDelivery::Lost`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use lsp_types::{PublishDiagnosticsParams, Uri};
use tokio::sync::{Notify, mpsc};
use tracing::{debug, warn};

use super::types::LspNotification;
use crate::bridge::{BoundedDiagnostics, DiagnosticsKey, MAX_DIAGNOSTIC_ENTRIES};
use crate::util::{WarnLimiter, lock_std};

/// Most files with an undelivered publish; as many as the cache holds, so the
/// mailbox is never the narrower bound.
pub const MAX_PENDING_PUBLISHES: usize = MAX_DIAGNOSTIC_ENTRIES;

/// Most bytes of undelivered diagnostics per server.
pub const MAILBOX_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Most lost files remembered per server before only the fact of a loss is.
const MAX_LOST_FILES: usize = MAX_DIAGNOSTIC_ENTRIES;

/// Period of the dropped-frame warning of one lane.
pub const DROP_WARN_PERIOD: Duration = WarnLimiter::DEFAULT_PERIOD;

/// What the mailbox holds at most.
#[derive(Debug, Clone, Copy)]
pub struct MailboxLimits {
    /// Most files with an undelivered publish.
    pub max_pending: usize,
    /// Most bytes of undelivered diagnostics.
    pub max_bytes: usize,
}

impl Default for MailboxLimits {
    fn default() -> Self {
        Self {
            max_pending: MAX_PENDING_PUBLISHES,
            max_bytes: MAILBOX_MAX_BYTES,
        }
    }
}

/// Files whose publish the mailbox could not deliver.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LostPublishes {
    /// The files, oldest loss first.
    pub uris: Vec<Uri>,
    /// More files were lost than are listed, so some losses are unnamed. Handed
    /// over on its own, after the pending publishes.
    pub overflowed: bool,
}

/// One thing the reader takes out of the mailbox.
#[derive(Debug)]
pub enum PublishDelivery {
    /// The latest pending publish of a file.
    Publish(PublishDiagnosticsParams),
    /// Publishes that never made it into the mailbox.
    Lost(LostPublishes),
}

#[derive(Debug)]
struct Pending {
    params: PublishDiagnosticsParams,
    bytes: usize,
}

#[derive(Debug, Default)]
struct State {
    pending: HashMap<DiagnosticsKey, Pending>,
    /// Arrival order of the keys of `pending`; a key whose publish was
    /// withdrawn or already taken is skipped.
    order: VecDeque<DiagnosticsKey>,
    bytes: usize,
    lost: Vec<Uri>,
    lost_keys: HashSet<DiagnosticsKey>,
    lost_overflowed: bool,
    dropped_since_warn: usize,
    warn: WarnLimiter,
    writer_gone: bool,
    reader_gone: bool,
}

#[derive(Debug)]
struct Shared {
    state: StdMutex<State>,
    wake: Notify,
    limits: MailboxLimits,
}

impl State {
    fn withdraw(&mut self, key: &DiagnosticsKey) {
        if let Some(pending) = self.pending.remove(key) {
            self.bytes = self.bytes.saturating_sub(pending.bytes);
        }
    }

    /// Drops the keys of `order` that no longer have a pending publish once
    /// they outnumber what the mailbox can hold twice over, so a stalled
    /// reader cannot make the queue grow with withdrawn files.
    fn compact_order(&mut self, max_pending: usize) {
        if self.order.len() > max_pending.saturating_mul(2).max(16) {
            let pending = &self.pending;
            self.order.retain(|key| pending.contains_key(key));
        }
    }

    /// The next delivery that is ready: lost files, then pending publishes in
    /// arrival order, then the unnamed losses.
    fn next_ready(&mut self) -> Option<PublishDelivery> {
        if let Some(lost) = self.take_lost_files() {
            return Some(PublishDelivery::Lost(lost));
        }
        while let Some(key) = self.order.pop_front() {
            if let Some(pending) = self.pending.remove(&key) {
                self.bytes = self.bytes.saturating_sub(pending.bytes);
                return Some(PublishDelivery::Publish(pending.params));
            }
        }
        self.take_lost_overflow().map(PublishDelivery::Lost)
    }

    fn mark_lost(&mut self, key: DiagnosticsKey, uri: Uri) {
        if self.lost_keys.contains(&key) {
            return;
        }
        if self.lost.len() >= MAX_LOST_FILES {
            self.lost_overflowed = true;
            return;
        }
        self.lost_keys.insert(key);
        self.lost.push(uri);
    }

    fn take_lost_files(&mut self) -> Option<LostPublishes> {
        if self.lost.is_empty() {
            return None;
        }
        self.lost_keys.clear();
        Some(LostPublishes {
            uris: std::mem::take(&mut self.lost),
            overflowed: false,
        })
    }

    /// The unnamed losses, handed over only once nothing is pending so the
    /// cache can mark them after the burst's own writes.
    fn take_lost_overflow(&mut self) -> Option<LostPublishes> {
        std::mem::take(&mut self.lost_overflowed)
            .then(LostPublishes::default)
            .map(|lost| LostPublishes {
                overflowed: true,
                ..lost
            })
    }

    fn count_drop(&mut self) {
        self.dropped_since_warn = self.dropped_since_warn.saturating_add(1);
        if self.warn.due(std::time::Instant::now(), DROP_WARN_PERIOD) {
            warn!(
                "publishDiagnostics mailbox is full: dropped {} publish(es) since the last \
                 warning; the files read as evicted until the server publishes again",
                std::mem::take(&mut self.dropped_since_warn)
            );
        }
    }
}

/// The message loop's side of the mailbox. Dropping it closes the mailbox, so
/// the reader ends once what is pending is taken.
#[derive(Debug)]
pub struct PublishWriter {
    shared: Arc<Shared>,
}

/// The bridge's side of the mailbox. Dropping it discards what is pending and
/// makes the writer stop buffering.
#[derive(Debug)]
pub struct PublishReader {
    shared: Arc<Shared>,
}

/// A mailbox bounded by `limits`: the message loop's writer and the bridge's
/// reader of one server's `publishDiagnostics`.
pub fn mailbox(limits: MailboxLimits) -> (PublishWriter, PublishReader) {
    let shared = Arc::new(Shared {
        state: StdMutex::new(State::default()),
        wake: Notify::new(),
        limits,
    });
    (
        PublishWriter {
            shared: Arc::clone(&shared),
        },
        PublishReader { shared },
    )
}

impl PublishWriter {
    /// Buffers `params`, replacing a pending publish of the same file.
    ///
    /// The diagnostics are bounded like the cache bounds an entry before they
    /// are counted. A publish that does not fit is dropped and its file
    /// recorded as lost; when a replacement does not fit, the older pending
    /// publish of the file is withdrawn too, so the file never reads as
    /// published with content older than what the server last said.
    pub fn publish(&self, params: PublishDiagnosticsParams) {
        let PublishDiagnosticsParams {
            uri,
            diagnostics,
            version,
        } = params;
        let diagnostics = BoundedDiagnostics::new(&uri, diagnostics);
        let bytes = diagnostics.buffered_bytes();
        let diagnostics = diagnostics.into_vec();
        let key = DiagnosticsKey::of(&uri);
        let limits = self.shared.limits;
        let mut state = lock_std(&self.shared.state);
        if state.reader_gone {
            debug!(
                "dropping a publish for {}: nobody reads the mailbox",
                uri.as_ref()
            );
            return;
        }
        let replaced_bytes = state.pending.get(&key).map(|pending| pending.bytes);
        let is_new = replaced_bytes.is_none();
        let fits = state
            .bytes
            .saturating_sub(replaced_bytes.unwrap_or(0))
            .saturating_add(bytes)
            <= limits.max_bytes
            && (!is_new || state.pending.len() < limits.max_pending);
        state.compact_order(limits.max_pending);
        state.withdraw(&key);
        if fits {
            state.bytes = state.bytes.saturating_add(bytes);
            if is_new {
                state.order.push_back(key.clone());
            }
            state.pending.insert(
                key,
                Pending {
                    params: PublishDiagnosticsParams {
                        uri,
                        diagnostics,
                        version,
                    },
                    bytes,
                },
            );
        } else {
            state.mark_lost(key, uri);
            state.count_drop();
        }
        drop(state);
        self.shared.wake.notify_one();
    }
}

impl Drop for PublishWriter {
    fn drop(&mut self) {
        lock_std(&self.shared.state).writer_gone = true;
        self.shared.wake.notify_one();
    }
}

impl PublishReader {
    /// A reader of a mailbox nothing will ever be written to.
    #[must_use]
    pub fn closed() -> Self {
        mailbox(MailboxLimits::default()).1
    }

    /// The next delivery if one is ready now, without waiting.
    pub fn try_recv(&mut self) -> Option<PublishDelivery> {
        lock_std(&self.shared.state).next_ready()
    }

    /// The next delivery: lost files first, then pending publishes in arrival
    /// order; `None` once the writer is gone and nothing is left.
    pub async fn recv(&mut self) -> Option<PublishDelivery> {
        loop {
            {
                let mut state = lock_std(&self.shared.state);
                if let Some(delivery) = state.next_ready() {
                    return Some(delivery);
                }
                if state.writer_gone {
                    return None;
                }
            }
            self.shared.wake.notified().await;
        }
    }
}

impl Drop for PublishReader {
    fn drop(&mut self) {
        let mut state = lock_std(&self.shared.state);
        state.reader_gone = true;
        state.order.clear();
        state.bytes = 0;
        let pending = std::mem::take(&mut state.pending);
        drop(state);
        drop(pending);
    }
}

/// The message loop's handle for everything a server notifies on the
/// notification lane: log and `showMessage` frames take a bounded channel and
/// yield under pressure, diagnostics take the mailbox and are never dropped
/// unrecorded.
#[derive(Debug)]
pub struct NotificationSink {
    messages: mpsc::Sender<LspNotification>,
    publishes: PublishWriter,
}

impl NotificationSink {
    /// A sink over `messages` with a mailbox of the default limits.
    pub fn new(messages: mpsc::Sender<LspNotification>) -> (Self, PublishReader) {
        Self::with_limits(messages, MailboxLimits::default())
    }

    /// As [`Self::new`] with explicit mailbox limits.
    pub fn with_limits(
        messages: mpsc::Sender<LspNotification>,
        limits: MailboxLimits,
    ) -> (Self, PublishReader) {
        let (publishes, reader) = mailbox(limits);
        (
            Self {
                messages,
                publishes,
            },
            reader,
        )
    }

    /// The channel log and `showMessage` frames go through.
    pub const fn messages(&self) -> &mpsc::Sender<LspNotification> {
        &self.messages
    }

    /// The mailbox diagnostics go through.
    pub const fn publishes(&self) -> &PublishWriter {
        &self.publishes
    }
}

/// Everything a server's notification lane delivers to the bridge: log and
/// `showMessage` frames from a bounded channel, and the diagnostics mailbox.
///
/// A plain receiver converts into an inbox with a closed mailbox, for
/// consumers that only look at the channel.
#[derive(Debug)]
pub struct NotificationInbox {
    /// The log/showMessage channel.
    pub messages: mpsc::Receiver<LspNotification>,
    /// The diagnostics mailbox.
    pub publishes: PublishReader,
}

impl NotificationInbox {
    /// An inbox over `messages` and `publishes`.
    #[must_use]
    pub const fn new(messages: mpsc::Receiver<LspNotification>, publishes: PublishReader) -> Self {
        Self {
            messages,
            publishes,
        }
    }
}

impl From<mpsc::Receiver<LspNotification>> for NotificationInbox {
    fn from(messages: mpsc::Receiver<LspNotification>) -> Self {
        Self::new(messages, PublishReader::closed())
    }
}

/// Counts the frames one lane dropped and warns about them at most once per
/// [`DROP_WARN_PERIOD`], naming the lane and the count since the last warning.
#[derive(Debug)]
pub struct DropLog {
    lane: &'static str,
    state: StdMutex<(WarnLimiter, usize)>,
}

impl DropLog {
    pub const fn new(lane: &'static str) -> Self {
        Self {
            lane,
            state: StdMutex::new((WarnLimiter::new(), 0)),
        }
    }

    /// The log of the lane named `lane` (`"notification"` or `"lifecycle"`);
    /// shared by every server, so a flood on one lane warns once a period.
    pub fn of_lane(lane: &str) -> &'static Self {
        static NOTIFICATION: DropLog = DropLog::new("notification");
        static LIFECYCLE: DropLog = DropLog::new("lifecycle");
        if lane == "lifecycle" {
            &LIFECYCLE
        } else {
            &NOTIFICATION
        }
    }

    /// Records one dropped frame of `method`; the frame itself is logged at
    /// DEBUG only.
    pub fn record(&self, method: &str) {
        debug!("dropping notification: lane={}, method={method}", self.lane);
        let mut state = lock_std(&self.state);
        state.1 = state.1.saturating_add(1);
        if state.0.due(std::time::Instant::now(), DROP_WARN_PERIOD) {
            let dropped = std::mem::take(&mut state.1);
            drop(state);
            warn!(
                "dropped {dropped} notification(s) on the {} lane since the last warning \
                 (channel full or closed)",
                self.lane
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use lsp_types::Diagnostic;

    use super::*;

    fn uri(n: usize) -> Uri {
        Uri::from(format!("file:///ws/f{n}.rs"))
    }

    fn publish_of(n: usize, message: &str) -> PublishDiagnosticsParams {
        PublishDiagnosticsParams {
            uri: uri(n),
            diagnostics: vec![Diagnostic {
                message: message.to_owned().into(),
                ..Diagnostic::default()
            }],
            version: None,
        }
    }

    fn bytes_of(n: usize, message: &str) -> usize {
        let params = publish_of(n, message);
        BoundedDiagnostics::new(&params.uri, params.diagnostics).buffered_bytes()
    }

    fn small(max_pending: usize, max_bytes: usize) -> MailboxLimits {
        MailboxLimits {
            max_pending,
            max_bytes,
        }
    }

    async fn next_publish(reader: &mut PublishReader) -> PublishDiagnosticsParams {
        match reader.recv().await {
            Some(PublishDelivery::Publish(params)) => params,
            other => panic!("expected a publish, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn burst_below_the_cap_is_delivered_whole_in_order() {
        let (writer, mut reader) = mailbox(small(10, usize::MAX));
        for n in 0..10 {
            writer.publish(publish_of(n, "e"));
        }
        drop(writer);

        for n in 0..10 {
            assert_eq!(next_publish(&mut reader).await.uri, uri(n));
        }
        assert!(reader.recv().await.is_none());
    }

    #[tokio::test]
    async fn later_publish_replaces_the_pending_one_of_the_same_file() {
        let (writer, mut reader) = mailbox(small(10, usize::MAX));
        writer.publish(publish_of(1, "old"));
        writer.publish(publish_of(1, "new"));
        drop(writer);

        let delivered = next_publish(&mut reader).await;
        assert_eq!(delivered.diagnostics.len(), 1);
        assert_eq!(
            delivered.diagnostics.first().map(|d| d.message.clone()),
            Some("new".to_owned().into())
        );
        assert!(reader.recv().await.is_none());
    }

    #[tokio::test]
    async fn publish_over_the_file_cap_is_reported_lost_not_dropped_quietly() {
        let (writer, mut reader) = mailbox(small(2, usize::MAX));
        for n in 0..4 {
            writer.publish(publish_of(n, "e"));
        }

        let Some(PublishDelivery::Lost(lost)) = reader.recv().await else {
            panic!("the loss must be handed over first");
        };
        assert_eq!(lost.uris, [uri(2), uri(3)]);
        assert!(!lost.overflowed);
        assert_eq!(next_publish(&mut reader).await.uri, uri(0));
        assert_eq!(next_publish(&mut reader).await.uri, uri(1));
    }

    /// A replacement that does not fit withdraws the older pending publish
    /// too, so the file reads lost rather than with older content.
    #[tokio::test]
    async fn oversized_replacement_withdraws_the_older_publish_and_marks_the_file_lost() {
        let one = bytes_of(1, "x");
        let (writer, mut reader) = mailbox(small(10, one + one / 2));
        writer.publish(publish_of(1, "x"));
        writer.publish(publish_of(1, &"y".repeat(one * 2)));
        drop(writer);

        let Some(PublishDelivery::Lost(lost)) = reader.recv().await else {
            panic!("expected the lost file");
        };
        assert_eq!(lost.uris, [uri(1)]);
        assert!(reader.recv().await.is_none());
    }

    /// A stalled reader cannot make the arrival queue grow with withdrawn files.
    #[tokio::test]
    async fn arrival_queue_stays_bounded_under_withdrawals() {
        let one = bytes_of(0, "x");
        let (writer, _reader) = mailbox(small(4, one * 2));
        for n in 0..1000 {
            writer.publish(publish_of(n, "x"));
            writer.publish(publish_of(n, &"y".repeat(one * 4)));
        }

        assert!(lock_std(&writer.shared.state).order.len() <= 16);
    }

    #[tokio::test]
    async fn replacement_after_order_compaction_is_still_delivered() {
        let one = bytes_of(0, "x");
        let (writer, mut reader) = mailbox(small(4, one * 2));
        for n in 0..16 {
            writer.publish(publish_of(n, "x"));
            writer.publish(publish_of(n, &"y".repeat(one * 4)));
        }
        writer.publish(publish_of(100, "x"));
        writer.publish(publish_of(100, "z"));
        drop(writer);

        let mut delivered = None;
        while let Some(delivery) = reader.recv().await {
            if let PublishDelivery::Publish(params) = delivery {
                delivered = Some(params);
            }
        }
        let delivered = delivered.expect("the replaced publish must reach the reader");
        assert_eq!(delivered.uri, uri(100));
        assert_eq!(
            delivered.diagnostics.first().map(|d| d.message.clone()),
            Some("z".to_owned().into())
        );
    }

    #[tokio::test]
    async fn byte_cap_bounds_what_is_buffered() {
        let one = bytes_of(0, "e");
        let (writer, mut reader) = mailbox(small(100, one * 3));
        for n in 0..10 {
            writer.publish(publish_of(n, "e"));
        }
        drop(writer);

        let mut delivered = 0;
        let mut lost = 0;
        while let Some(item) = reader.recv().await {
            match item {
                PublishDelivery::Publish(_) => delivered += 1,
                PublishDelivery::Lost(files) => lost += files.uris.len(),
            }
        }
        assert_eq!((delivered, lost), (3, 7));
    }

    #[tokio::test]
    async fn lost_list_is_capped_and_then_only_the_fact_is_kept() {
        let (writer, mut reader) = mailbox(small(0, usize::MAX));
        for n in 0..=MAX_LOST_FILES {
            writer.publish(publish_of(n, "e"));
        }
        drop(writer);

        let Some(PublishDelivery::Lost(lost)) = reader.recv().await else {
            panic!("expected losses");
        };
        assert_eq!(lost.uris.len(), MAX_LOST_FILES);
        assert!(!lost.overflowed);
        let Some(PublishDelivery::Lost(rest)) = reader.recv().await else {
            panic!("expected the unnamed losses");
        };
        assert!(rest.uris.is_empty() && rest.overflowed);
        assert!(reader.recv().await.is_none());
    }

    #[tokio::test]
    async fn dropping_the_writer_ends_the_reader_and_dropping_the_reader_stops_buffering() {
        let (writer, reader) = mailbox(small(10, usize::MAX));
        writer.publish(publish_of(1, "e"));
        drop(reader);
        writer.publish(publish_of(2, "e"));
        assert_eq!(lock_std(&writer.shared.state).bytes, 0);
        assert!(lock_std(&writer.shared.state).pending.is_empty());

        let (writer, mut reader) = mailbox(small(10, usize::MAX));
        drop(writer);
        assert!(reader.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_waiting_reader_wakes_for_a_new_publish() {
        let (writer, mut reader) = mailbox(small(10, usize::MAX));
        let waiting = tokio::spawn(async move { next_publish(&mut reader).await.uri });
        tokio::task::yield_now().await;

        writer.publish(publish_of(7, "e"));

        assert_eq!(waiting.await.unwrap(), uri(7));
    }
}
