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

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use lsp_types::{LogMessageParams, ShowMessageParams};
use tokio::sync::{Notify, mpsc};
use tracing::{debug, warn};

use crate::bridge::{BoundedDiagnostics, DiagnosticsKey, MAX_DIAGNOSTIC_ENTRIES};
use crate::lsp::types::{BoundedUri, DecodedPublish, LostPublish};
use crate::util::{DropCounter, MAX_LOG_STRING_BYTES, escape_bounded, lock_std};

/// Most files with an undelivered publish; as many as the cache holds, so the
/// mailbox is never the narrower bound.
pub const MAX_PENDING_PUBLISHES: usize = MAX_DIAGNOSTIC_ENTRIES;

/// Most bytes of undelivered diagnostics per server.
pub const MAILBOX_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Most lost files remembered per server before only the fact of a loss is.
///
/// Each remembered file holds a [`BoundedUri`] of up to
/// [`BoundedUri::MAX_BYTES`] and its dedupe key of the same length, so the
/// list retains at most 1000 x 2 x 16 KiB = 32 MiB per server, outside the
/// `MAILBOX_MAX_BYTES` budget of undelivered diagnostics.
const MAX_LOST_FILES: usize = MAX_DIAGNOSTIC_ENTRIES;

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

/// A `publishDiagnostics` whose diagnostics are already bounded the way the
/// cache stores them, so no later stage bounds them again.
#[derive(Debug)]
pub struct BoundedPublish {
    uri: BoundedUri,
    version: Option<i32>,
    diagnostics: BoundedDiagnostics,
}

impl BoundedPublish {
    fn new(publish: DecodedPublish) -> (Self, usize) {
        let (uri, version, diagnostics) = publish.into_parts();
        let diagnostics = BoundedDiagnostics::new(uri.uri(), diagnostics);
        let bytes = diagnostics
            .buffered_bytes()
            .saturating_add(AsRef::<str>::as_ref(uri.uri()).len());
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
    pub const fn uri(&self) -> &BoundedUri {
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
    pub fn into_parts(self) -> (BoundedUri, Option<i32>, BoundedDiagnostics) {
        (self.uri, self.version, self.diagnostics)
    }
}

/// A non-empty list of files whose publish the mailbox could not deliver,
/// oldest loss first.
#[derive(Debug, PartialEq, Eq)]
pub struct LostFiles(Vec<BoundedUri>);

impl LostFiles {
    fn new(uris: Vec<BoundedUri>) -> Option<Self> {
        (!uris.is_empty()).then_some(Self(uris))
    }

    /// The lost files.
    #[must_use]
    pub fn uris(&self) -> &[BoundedUri] {
        &self.0
    }
}

/// One thing the reader takes out of the mailbox.
#[derive(Debug)]
#[non_exhaustive]
pub enum PublishDelivery {
    /// The latest pending publish of a file.
    Publish(BoundedPublish),
    /// Named files whose publish never made it into the mailbox.
    Lost(LostFiles),
    /// A publish was lost whose file is unknown, or named files were lost
    /// past what the mailbox can list: every file the server pushed is
    /// suspect, so each pushed entry is evicted and the overflow mark is set.
    /// Handed over after the named losses and before the pending publishes,
    /// which are all newer than the loss.
    LostUnidentified,
    /// More files were lost than the mailbox could name. Only the overflow
    /// mark is set; entries the cache holds are kept. Handed over on its own,
    /// after the pending publishes.
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
    /// Position in `State::order`; a replacement keeps it, a new publish of
    /// a file whose earlier one was withdrawn gets a fresh one.
    seq: u64,
}

/// Losses the mailbox cannot name a file for; the later variant subsumes the
/// earlier ones, since it evicts everything the earlier ones would.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum UnnamedLoss {
    #[default]
    None,
    Overflowed,
    Unidentified,
}

/// What a full list of lost files escalates to.
#[derive(Debug, Clone, Copy)]
enum OnFull {
    MarkOverflowed,
    LoseAll,
}

#[derive(Debug, Default)]
struct State {
    pending: HashMap<DiagnosticsKey, Pending>,
    /// Arrival order of the pending publishes as `(key, seq)`; an entry whose
    /// `seq` is not the pending publish's (withdrawn, taken or republished
    /// later) is skipped.
    order: VecDeque<(DiagnosticsKey, u64)>,
    next_seq: u64,
    bytes: usize,
    lost: Vec<BoundedUri>,
    lost_keys: HashSet<DiagnosticsKey>,
    unnamed_loss: UnnamedLoss,
    drops: DropCounter,
    lost_frames: DropCounter,
    writer_gone: bool,
    reader_gone: bool,
}

#[derive(Debug)]
struct Shared {
    state: StdMutex<State>,
    wake: Notify,
    limits: MailboxLimits,
}

/// Whether `seq` is the sequence number of the pending publish of `key`.
fn is_current(pending: &HashMap<DiagnosticsKey, Pending>, key: &DiagnosticsKey, seq: u64) -> bool {
    pending.get(key).is_some_and(|pending| pending.seq == seq)
}

impl State {
    /// Appends `key` to the arrival order under a fresh sequence number.
    fn enqueue(&mut self, key: DiagnosticsKey) -> u64 {
        let seq = self.next_seq;
        self.next_seq = seq.saturating_add(1);
        self.order.push_back((key, seq));
        seq
    }

    fn withdraw(&mut self, key: &DiagnosticsKey) {
        if let Some(pending) = self.pending.remove(key) {
            self.bytes = self.bytes.saturating_sub(pending.bytes);
        }
    }

    /// Drops the entries of `order` that no longer match a pending publish
    /// once they outnumber what the mailbox can hold twice over, so a stalled
    /// reader cannot make the queue grow with withdrawn files.
    fn compact_order(&mut self, max_pending: usize) {
        if self.order.len() > max_pending.saturating_mul(2).max(16) {
            let pending = &self.pending;
            self.order
                .retain(|(key, seq)| is_current(pending, key, *seq));
        }
    }

    /// The next delivery that is ready: lost files, an unidentified loss, then
    /// pending publishes in arrival order, then the unnamed losses.
    fn next_ready(&mut self) -> Option<PublishDelivery> {
        if let Some(lost) = self.take_lost_files() {
            return Some(PublishDelivery::Lost(lost));
        }
        if self.unnamed_loss == UnnamedLoss::Unidentified {
            self.unnamed_loss = UnnamedLoss::None;
            return Some(PublishDelivery::LostUnidentified);
        }
        while let Some((key, seq)) = self.order.pop_front() {
            let Entry::Occupied(entry) = self.pending.entry(key) else {
                continue;
            };
            if entry.get().seq != seq {
                continue;
            }
            let pending = entry.remove();
            self.bytes = self.bytes.saturating_sub(pending.bytes);
            return Some(PublishDelivery::Publish(pending.publish));
        }
        (std::mem::take(&mut self.unnamed_loss) == UnnamedLoss::Overflowed)
            .then_some(PublishDelivery::LostUnnamed)
    }

    /// Remembers `uri` as a lost file unless it already is; a full list
    /// escalates to `on_full`.
    fn remember_lost(&mut self, key: DiagnosticsKey, uri: BoundedUri, on_full: OnFull) {
        if self.lost_keys.contains(&key) {
            return;
        }
        if self.lost.len() >= MAX_LOST_FILES {
            match on_full {
                OnFull::LoseAll => self.lose_all(),
                OnFull::MarkOverflowed => {
                    self.unnamed_loss = self.unnamed_loss.max(UnnamedLoss::Overflowed);
                }
            }
            return;
        }
        self.lost_keys.insert(key);
        self.lost.push(uri);
    }

    /// Records a loss that no file can be named for: every pending publish
    /// predates it and is withdrawn.
    fn lose_all(&mut self) {
        self.pending.clear();
        self.order.clear();
        self.bytes = 0;
        self.unnamed_loss = UnnamedLoss::Unidentified;
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
    pub fn publish(&self, publish: DecodedPublish) {
        let (publish, bytes) = BoundedPublish::new(publish);
        let key = DiagnosticsKey::of(publish.uri().uri());
        let limits = self.shared.limits;
        let mut state = lock_std(&self.shared.state);
        if state.reader_gone {
            debug!(
                uri = publish.uri().uri().as_ref(),
                "dropping a publish: nobody reads the mailbox"
            );
            return;
        }
        let replaced = state
            .pending
            .get(&key)
            .map(|pending| (pending.bytes, pending.seq));
        let is_new = replaced.is_none();
        let fits = state
            .bytes
            .saturating_sub(replaced.map_or_default(|(bytes, _)| bytes))
            .saturating_add(bytes)
            <= limits.max_bytes
            && (!is_new || state.pending.len() < limits.max_pending);
        state.compact_order(limits.max_pending);
        state.withdraw(&key);
        let warn_due = if fits {
            state.bytes = state.bytes.saturating_add(bytes);
            let seq = match replaced {
                Some((_, seq)) => seq,
                None => state.enqueue(key.clone()),
            };
            state.pending.insert(
                key,
                Pending {
                    publish,
                    bytes,
                    seq,
                },
            );
            None
        } else {
            let (uri, ..) = publish.into_parts();
            state.remember_lost(key, uri, OnFull::MarkOverflowed);
            state.count_drop()
        };
        drop(state);
        if let Some(suppressed) = warn_due {
            warn!(
                suppressed,
                "publishDiagnostics mailbox is full: dropped a publish; the files read as \
                 evicted until the server publishes again"
            );
        }
        self.shared.wake.notify_one();
    }

    /// Records a `publishDiagnostics` that was dropped before it could be
    /// buffered: its frame could not be decoded, or it named a URI past
    /// [`BoundedUri::MAX_BYTES`].
    ///
    /// A named file is marked lost like one the mailbox could not hold, and its
    /// pending publish is withdrawn, since the lost one was newer. An
    /// unidentified loss withdraws every pending publish and has the reader hand over
    /// [`PublishDelivery::LostUnidentified`]. Every source of a lost publish
    /// passes here, so this is the one place that warns about it: at most
    /// once per period of [`WarnLimiter::DEFAULT_PERIOD`][period], without the
    /// URI or any frame content.
    ///
    /// [period]: crate::util::WarnLimiter::DEFAULT_PERIOD
    pub fn lose(&self, lost: LostPublish) {
        let mut state = lock_std(&self.shared.state);
        if state.reader_gone {
            return;
        }
        match lost {
            LostPublish::File(uri) => {
                let key = DiagnosticsKey::of(uri.uri());
                state.withdraw(&key);
                state.remember_lost(key, uri, OnFull::LoseAll);
            }
            LostPublish::Unidentified => state.lose_all(),
        }
        let warn_due = state.lost_frames.record(Instant::now());
        drop(state);
        if let Some(suppressed) = warn_due {
            warn!(
                suppressed,
                "dropped an unreadable or over-long-uri publishDiagnostics; its files read as \
                 evicted until the server publishes them again"
            );
        }
        self.shared.wake.notify_one();
    }
}

#[cfg(test)]
impl PublishWriter {
    /// Whether a loss is recorded that the reader has not taken yet.
    pub(crate) fn has_undelivered_loss(&self) -> bool {
        let state = lock_std(&self.shared.state);
        !state.lost.is_empty() || state.unnamed_loss != UnnamedLoss::None
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
/// [`WarnLimiter::DEFAULT_PERIOD`][period], naming the lane and the count
/// since the last warning.
///
/// [period]: crate::util::WarnLimiter::DEFAULT_PERIOD
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
        let method = escape_bounded(method, MAX_LOG_STRING_BYTES);
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

    use lsp_types::{Diagnostic, PublishDiagnosticsParams, Uri};

    use super::*;
    use crate::test_support::uri_of_len;

    fn uri(n: usize) -> Uri {
        Uri::from(format!("file:///ws/f{n}.rs"))
    }

    fn bounded(n: usize) -> BoundedUri {
        BoundedUri::try_from(uri(n)).unwrap()
    }

    fn publish_at(uri: Uri, message: &str) -> DecodedPublish {
        DecodedPublish::try_from(PublishDiagnosticsParams {
            uri,
            diagnostics: vec![Diagnostic {
                message: message.to_owned().into(),
                ..Diagnostic::default()
            }],
            version: None,
        })
        .unwrap()
    }

    fn publish_of(n: usize, message: &str) -> DecodedPublish {
        publish_at(uri(n), message)
    }

    fn bytes_of(n: usize, message: &str) -> usize {
        BoundedPublish::new(publish_of(n, message)).1
    }

    /// #738: the method of a dropped frame is escaped and bounded in the log.
    #[test]
    fn drop_log_escapes_and_bounds_the_method() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let captured = crate::test_lsp::CapturedLogs::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));

        DropLog::of(Lane::Notification).record(&format!("evil\nFORGED{}", "x".repeat(10_000)));
        drop(guard);

        let line = captured
            .messages()
            .into_iter()
            .find(|message| message.contains("dropping notification"))
            .expect("the drop is logged at debug");
        assert!(!line.contains('\n'), "{line:.80}");
        assert!(line.contains("evil\\nFORGED"), "{line:.80}");
        assert!(line.len() < 2 * MAX_LOG_STRING_BYTES, "{}", line.len());
    }

    /// #738: the URI text counts toward the byte budget.
    #[test]
    fn publish_bytes_include_the_uri() {
        let (uri, _, raw) = publish_of(1, "x").into_parts();
        let diagnostics = BoundedDiagnostics::new(uri.uri(), raw);

        assert_eq!(
            bytes_of(1, "x"),
            diagnostics.buffered_bytes() + "file:///ws/f1.rs".len()
        );
    }

    /// #742: a republished file is delivered at its new arrival position, not
    /// at the place a withdrawn publish held.
    #[tokio::test]
    async fn republished_file_follows_arrival_order_after_a_withdrawal() {
        let one = bytes_of(1, "x");
        let (writer, mut reader) = mailbox(small(10, one * 2 + one / 2));
        writer.publish(publish_of(1, "x"));
        writer.publish(publish_of(1, &"y".repeat(one * 4)));
        writer.publish(publish_of(2, "x"));
        writer.publish(publish_of(1, "x"));
        drop(writer);

        let Some(PublishDelivery::Lost(lost)) = reader.recv().await else {
            panic!("expected the lost file first");
        };
        assert_eq!(lost.uris(), [bounded(1)]);
        assert_eq!(next_publish(&mut reader).await.uri().uri(), &uri(2));
        assert_eq!(next_publish(&mut reader).await.uri().uri(), &uri(1));
        assert!(reader.recv().await.is_none());
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
            assert_eq!(next_publish(&mut reader).await.uri().uri(), &uri(n));
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
        assert_eq!(lost.uris(), [bounded(2), bounded(3)]);
        assert_eq!(next_publish(&mut reader).await.uri().uri(), &uri(0));
        assert_eq!(next_publish(&mut reader).await.uri().uri(), &uri(1));
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
        assert_eq!(lost.uris(), [bounded(1)]);
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
        assert_eq!(delivered.uri().uri(), &uri(100));
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
                PublishDelivery::LostUnidentified | PublishDelivery::LostUnnamed => {
                    panic!("no unnamed losses expected")
                }
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
    async fn named_loss_withdraws_the_pending_publish_of_its_file_only() {
        let (writer, mut reader) = mailbox(MailboxLimits::default());
        writer.publish(publish_of(1, "old"));
        writer.publish(publish_of(2, "kept"));

        writer.lose(LostPublish::File(bounded(1)));
        drop(writer);

        let Some(PublishDelivery::Lost(lost)) = reader.recv().await else {
            panic!("expected the lost file first");
        };
        assert_eq!(lost.uris(), [bounded(1)]);
        assert_eq!(next_publish(&mut reader).await.uri().uri(), &uri(2));
        assert!(reader.recv().await.is_none());
    }

    #[tokio::test]
    async fn unidentified_loss_withdraws_every_pending_publish_and_comes_before_later_ones() {
        let (writer, mut reader) = mailbox(MailboxLimits::default());
        writer.publish(publish_of(1, "old"));
        writer.publish(publish_of(2, "old"));

        writer.lose(LostPublish::Unidentified);
        writer.publish(publish_of(3, "new"));
        drop(writer);

        assert_matches!(reader.recv().await, Some(PublishDelivery::LostUnidentified));
        assert_eq!(next_publish(&mut reader).await.uri().uri(), &uri(3));
        assert!(reader.recv().await.is_none());
    }

    #[tokio::test]
    async fn named_loss_with_a_full_lost_list_escalates_to_an_unidentified_loss() {
        let (writer, mut reader) = mailbox(small(0, usize::MAX));
        for n in 0..MAX_LOST_FILES {
            writer.publish(publish_of(n, "e"));
        }

        writer.lose(LostPublish::File(bounded(MAX_LOST_FILES)));
        drop(writer);

        assert_matches!(reader.recv().await, Some(PublishDelivery::Lost(_)));
        assert_matches!(reader.recv().await, Some(PublishDelivery::LostUnidentified));
        assert!(reader.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_publish_with_a_uri_at_the_bound_is_buffered() {
        let (writer, mut reader) = mailbox(MailboxLimits::default());

        writer.publish(publish_at(uri_of_len(BoundedUri::MAX_BYTES), "e"));
        drop(writer);

        assert_eq!(
            next_publish(&mut reader).await.uri().uri().as_ref().len(),
            BoundedUri::MAX_BYTES
        );
    }

    #[tokio::test]
    async fn every_source_of_a_lost_publish_warns_once_per_period_without_the_uri() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let logs = crate::test_lsp::CapturedLogs::default();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
        let (writer, _reader) = mailbox(MailboxLimits::default());

        writer.lose(LostPublish::File(bounded(1)));
        writer.lose(LostPublish::Unidentified);
        writer.lose(LostPublish::File(bounded(2)));

        let warnings: Vec<String> = logs
            .entries()
            .into_iter()
            .filter(|(level, _)| *level == tracing::Level::WARN)
            .map(|(_, message)| message)
            .collect();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(!warnings[0].contains("f1.rs"), "{warnings:?}");
    }

    #[tokio::test]
    async fn a_loss_after_the_reader_is_gone_is_ignored() {
        let (writer, reader) = mailbox(MailboxLimits::default());
        drop(reader);

        writer.lose(LostPublish::Unidentified);
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

        assert_eq!(waiting.await.unwrap(), bounded(7));
    }
}
