//! Delivery of `textDocument/publishDiagnostics` from a server's message loop
//! to the bridge.
//!
//! A server may publish for many files in one burst, faster than the bridge
//! drains them. A bounded channel would drop the tail silently, so
//! diagnostics travel through a per-server mailbox instead: one pending
//! publish per file (a later publish supersedes an earlier one), bounded in
//! files and in bytes. A publish the mailbox cannot hold is not discarded
//! quietly: its file is recorded as lost, and the reader hands the lost files
//! over so the cache can say so ([`PublishDelivery::Lost`],
//! [`PublishDelivery::LostUnnamed`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use lsp_types::{LogMessageParams, PublishDiagnosticsParams, ShowMessageParams, Uri};
use tokio::sync::{Notify, mpsc};
use tracing::{debug, warn};

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

/// Counts dropped frames and says when one is worth a warning: at most once
/// per [`DROP_WARN_PERIOD`], so a flood cannot flood the log.
#[derive(Debug, Default)]
pub struct DropCounter {
    warn: WarnLimiter,
    suppressed: usize,
}

impl DropCounter {
    /// A counter whose first drop warns at once.
    pub const fn new() -> Self {
        Self {
            warn: WarnLimiter::new(),
            suppressed: 0,
        }
    }

    /// Records one drop at `now`. `Some(n)` when a warning is due, with `n` the
    /// drops silenced since the previous warning; the caller logs it once it
    /// holds no lock.
    pub fn record(&mut self, now: Instant) -> Option<usize> {
        if self.warn.due(now, DROP_WARN_PERIOD) {
            Some(std::mem::take(&mut self.suppressed))
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            None
        }
    }
}

/// A `publishDiagnostics` whose diagnostics are already bounded the way the
/// cache stores them, so no later stage bounds them again.
#[derive(Debug)]
pub struct BoundedPublish {
    uri: Uri,
    version: Option<i32>,
    diagnostics: BoundedDiagnostics,
}

impl BoundedPublish {
    fn new(params: PublishDiagnosticsParams) -> (Self, usize) {
        let PublishDiagnosticsParams {
            uri,
            diagnostics,
            version,
        } = params;
        let diagnostics = BoundedDiagnostics::new(&uri, diagnostics);
        let bytes = diagnostics.buffered_bytes();
        (
            Self {
                uri,
                version,
                diagnostics,
            },
            bytes,
        )
    }

    /// The file the server published for, exactly as it spelled it.
    #[must_use]
    pub const fn uri(&self) -> &Uri {
        &self.uri
    }

    /// The document version the diagnostics are for, if the server said.
    #[must_use]
    pub const fn version(&self) -> Option<i32> {
        self.version
    }

    /// The bounded diagnostics.
    #[must_use]
    pub const fn diagnostics(&self) -> &BoundedDiagnostics {
        &self.diagnostics
    }

    /// The URI, version and bounded diagnostics.
    #[must_use]
    pub fn into_parts(self) -> (Uri, Option<i32>, BoundedDiagnostics) {
        (self.uri, self.version, self.diagnostics)
    }
}

/// A non-empty list of files whose publish the mailbox could not deliver,
/// oldest loss first.
#[derive(Debug, PartialEq, Eq)]
pub struct LostFiles(Vec<Uri>);

impl LostFiles {
    fn new(uris: Vec<Uri>) -> Option<Self> {
        (!uris.is_empty()).then_some(Self(uris))
    }

    /// The lost files.
    #[must_use]
    pub fn uris(&self) -> &[Uri] {
        &self.0
    }
}

/// One thing the reader takes out of the mailbox.
#[derive(Debug)]
pub enum PublishDelivery {
    /// The latest pending publish of a file.
    Publish(BoundedPublish),
    /// Named files whose publish never made it into the mailbox.
    Lost(LostFiles),
    /// More files were lost than the mailbox could name. Handed over on its
    /// own, after the pending publishes.
    LostUnnamed,
}

/// A frame of the log and `showMessage` lane.
#[derive(Debug)]
pub enum ServerMessage {
    /// A `window/logMessage`.
    Log(LogMessageParams),
    /// A `window/showMessage`.
    Show(ShowMessageParams),
}

#[derive(Debug)]
struct Pending {
    publish: BoundedPublish,
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
    drops: DropCounter,
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
                return Some(PublishDelivery::Publish(pending.publish));
            }
        }
        std::mem::take(&mut self.lost_overflowed).then_some(PublishDelivery::LostUnnamed)
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

    fn take_lost_files(&mut self) -> Option<LostFiles> {
        let lost = LostFiles::new(std::mem::take(&mut self.lost))?;
        self.lost_keys.clear();
        Some(lost)
    }

    /// Counts one dropped publish; `Some(n)` when a warning is due.
    fn count_drop(&mut self) -> Option<usize> {
        self.drops.record(Instant::now())
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
        let (publish, bytes) = BoundedPublish::new(params);
        let key = DiagnosticsKey::of(publish.uri());
        let limits = self.shared.limits;
        let mut state = lock_std(&self.shared.state);
        if state.reader_gone {
            debug!(
                "dropping a publish for {}: nobody reads the mailbox",
                publish.uri().as_ref()
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
        let warn_due = if fits {
            state.bytes = state.bytes.saturating_add(bytes);
            if is_new {
                state.order.push_back(key.clone());
            }
            state.pending.insert(key, Pending { publish, bytes });
            None
        } else {
            let (uri, ..) = publish.into_parts();
            state.mark_lost(key, uri);
            state.count_drop()
        };
        drop(state);
        if let Some(suppressed) = warn_due {
            warn!(
                "publishDiagnostics mailbox is full: dropped a publish ({suppressed} others \
                 since the last warning); the files read as evicted until the server publishes \
                 again"
            );
        }
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
    messages: mpsc::Sender<ServerMessage>,
    publishes: PublishWriter,
}

impl NotificationSink {
    /// A sink over `messages` with a mailbox of the default limits.
    pub fn new(messages: mpsc::Sender<ServerMessage>) -> (Self, PublishReader) {
        Self::with_limits(messages, MailboxLimits::default())
    }

    /// As [`Self::new`] with explicit mailbox limits.
    pub fn with_limits(
        messages: mpsc::Sender<ServerMessage>,
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
    pub const fn messages(&self) -> &mpsc::Sender<ServerMessage> {
        &self.messages
    }

    /// The mailbox diagnostics go through.
    pub const fn publishes(&self) -> &PublishWriter {
        &self.publishes
    }
}

/// Everything a server's notification lane delivers to the bridge: log and
/// `showMessage` frames from a bounded channel, and the diagnostics mailbox.
#[derive(Debug)]
pub struct NotificationInbox {
    /// The log/showMessage channel.
    pub messages: mpsc::Receiver<ServerMessage>,
    /// The diagnostics mailbox.
    pub publishes: PublishReader,
}

impl NotificationInbox {
    /// An inbox over `messages` and `publishes`.
    #[must_use]
    pub const fn new(messages: mpsc::Receiver<ServerMessage>, publishes: PublishReader) -> Self {
        Self {
            messages,
            publishes,
        }
    }
}

#[cfg(test)]
impl From<mpsc::Receiver<ServerMessage>> for NotificationInbox {
    fn from(messages: mpsc::Receiver<ServerMessage>) -> Self {
        Self::new(messages, PublishReader::closed())
    }
}

/// A notification lane that can drop a frame under pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Log and `showMessage` frames.
    Notification,
    /// `$/progress` `begin`/`end` and unrecognized notifications.
    Lifecycle,
}

impl Lane {
    const fn name(self) -> &'static str {
        match self {
            Self::Notification => "notification",
            Self::Lifecycle => "lifecycle",
        }
    }
}

/// Counts the frames one lane dropped and warns about them at most once per
/// [`DROP_WARN_PERIOD`], naming the lane and the count since the last warning.
#[derive(Debug)]
pub struct DropLog {
    lane: Lane,
    drops: StdMutex<DropCounter>,
}

impl DropLog {
    const fn new(lane: Lane) -> Self {
        Self {
            lane,
            drops: StdMutex::new(DropCounter::new()),
        }
    }

    /// The log of `lane`; shared by every server, so a flood on one lane warns
    /// once a period.
    pub fn of(lane: Lane) -> &'static Self {
        static NOTIFICATION: DropLog = DropLog::new(Lane::Notification);
        static LIFECYCLE: DropLog = DropLog::new(Lane::Lifecycle);
        match lane {
            Lane::Notification => &NOTIFICATION,
            Lane::Lifecycle => &LIFECYCLE,
        }
    }

    /// Records one dropped frame of `method`; the frame itself is logged at
    /// DEBUG only.
    pub fn record(&self, method: &str) {
        let lane = self.lane.name();
        debug!("dropping notification: lane={lane}, method={method}");
        let warn_due = lock_std(&self.drops).record(Instant::now());
        if let Some(suppressed) = warn_due {
            warn!(
                "dropped a notification on the {lane} lane ({suppressed} others since the last \
                 warning; channel full or closed)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

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

    async fn next_publish(reader: &mut PublishReader) -> BoundedPublish {
        match reader.recv().await {
            Some(PublishDelivery::Publish(publish)) => publish,
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
            assert_eq!(next_publish(&mut reader).await.uri(), &uri(n));
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
        assert_eq!(delivered.diagnostics().as_slice().len(), 1);
        assert_eq!(
            delivered
                .diagnostics()
                .as_slice()
                .first()
                .map(|d| d.message.clone()),
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
        assert_eq!(lost.uris(), [uri(2), uri(3)]);
        assert_eq!(next_publish(&mut reader).await.uri(), &uri(0));
        assert_eq!(next_publish(&mut reader).await.uri(), &uri(1));
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
        assert_eq!(lost.uris(), [uri(1)]);
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
        assert_eq!(delivered.uri(), &uri(100));
        assert_eq!(
            delivered
                .diagnostics()
                .as_slice()
                .first()
                .map(|d| d.message.clone()),
            Some("z".to_owned().into())
        );
    }

    #[tokio::test]
    async fn delivered_publish_carries_bounded_diagnostics() {
        let (writer, mut reader) = mailbox(small(10, usize::MAX));
        let oversized = 512 * 1024;
        writer.publish(publish_of(1, &"m".repeat(oversized)));
        drop(writer);

        let delivered = next_publish(&mut reader).await;

        let message = delivered
            .diagnostics()
            .as_slice()
            .first()
            .map(|d| d.message.clone());
        assert_matches!(
            message,
            Some(lsp_types::Message::String(text)) if text.len() < oversized
        );
    }

    #[test]
    fn drop_counter_warns_once_per_period_and_counts_the_silenced() {
        let mut counter = DropCounter::new();
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);

        assert_eq!(counter.record(at(0)), Some(0));
        assert_eq!(counter.record(at(1)), None);
        assert_eq!(counter.record(at(2)), None);
        assert_eq!(counter.record(at(61)), Some(2));
    }

    #[test]
    fn each_lane_has_its_own_drop_log() {
        for lane in [Lane::Notification, Lane::Lifecycle] {
            assert_eq!(DropLog::of(lane).lane, lane);
        }
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
                PublishDelivery::Lost(files) => lost += files.uris().len(),
                PublishDelivery::LostUnnamed => panic!("no unnamed losses expected"),
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
        assert_eq!(lost.uris().len(), MAX_LOST_FILES);
        assert_matches!(reader.recv().await, Some(PublishDelivery::LostUnnamed));
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
