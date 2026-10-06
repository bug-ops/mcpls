//! LSP notification storage and management.
//!
//! Stores diagnostics, log messages, and server messages received from LSP servers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};
use lsp_types::{Diagnostic as LspDiagnostic, Uri};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::bridge::indexing::{IndexingPolicy, IndexingReset, IndexingState, IndexingTracker};
use crate::bridge::resources::{DiagnosticsResourceUri, PublishedDiagnosticsUri};
use crate::config::ServerId;
use crate::util::truncate_string;

mod bounds;
mod pulled_index;

pub use bounds::BoundedDiagnostics;
use bounds::{MAX_ENTRY_TEXT_BYTES, cap_diagnostics_entry_size};
use pulled_index::{PulledIndex, point};
pub use pulled_index::{ReportedSeverity, reported_code};

/// Maximum number of log entries to store.
const MAX_LOG_ENTRIES: usize = 100;

/// Global budget for distinct-URI diagnostic entries, shared work-conservingly
/// across every registered diagnostics-route server rather than claimed by
/// one server alone.
///
/// Guards against unbounded growth when a spawned LSP server publishes
/// diagnostics for an unbounded number of distinct URIs over a long-running
/// session, matching the bounding already applied to `logs`/`messages`.
/// Eviction only triggers once this global total is reached; it then targets
/// whichever server most exceeds its fair share of
/// `MAX_DIAGNOSTIC_ENTRIES / diagnostics_route_count` (see
/// [`NotificationCache::set_diagnostics_route_count`]). If no server exceeds
/// its share, eviction falls back to the writer's own oldest entry instead
/// -- even if the writer is itself within its share -- since it is the one
/// whose new entry needs room; a narrower fallback further evicts from the
/// largest other in-share server only if the writer itself has no entries
/// yet (its very first write) and every existing server is already within
/// its own share, since otherwise there would be nothing to evict and the
/// aggregate cap could be exceeded (see the private `server_to_evict_from`
/// for both fallbacks). A quieter, non-writer server that is within its fair
/// share is otherwise never touched (#266). A single active server can
/// still use the full budget when other registered servers are idle (#276)
/// instead of being capped at a static equal split regardless of how much
/// of it they actually use. Which single entry within the chosen server (or
/// another over-share one) is actually removed is further refined by
/// emptiness -- see the private `entry_to_evict` (#284).
pub const MAX_DIAGNOSTIC_ENTRIES: usize = 1000;

/// A `file:` URI normalized for cache lookup; only built by [`Self::of`], so a
/// raw URI string can never be mistaken for a key.
///
/// On Windows, URI comparisons must be case-insensitive: the filesystem is
/// case-insensitive and different tools (e.g. rust-analyzer vs std) may
/// produce drive letters in different cases (`C:` vs `c:`).
/// Lowercasing the entire URI is safe for `file://` URIs because they have
/// no case-sensitive query or fragment components.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DiagnosticsKey(String);

impl DiagnosticsKey {
    fn of(uri: &Uri) -> Self {
        let text: &str = uri.as_ref();
        if cfg!(windows) {
            Self(text.to_ascii_lowercase())
        } else {
            Self(text.to_owned())
        }
    }
}

/// How a cached entry's URI relates to the canonical path of its file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Spelling {
    /// The server published under the canonical spelling of the path.
    Canonical,
    /// The server published under a symlink alias of the file at this key.
    Alias(DiagnosticsKey),
}

/// Where a cached diagnostics list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Provenance {
    /// Delivered by `textDocument/publishDiagnostics`.
    Pushed,
    /// Answered to a `textDocument/diagnostic` request.
    Pulled,
}

/// Identifies one cache slot: a published URI and the way its diagnostics arrived.
///
/// A file has at most one `Pulled` slot, always under its canonical URI, next
/// to its `Pushed` slots, so neither source erases the other.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct SlotKey {
    uri: DiagnosticsKey,
    provenance: Provenance,
}

impl SlotKey {
    const fn pushed(uri: DiagnosticsKey) -> Self {
        Self {
            uri,
            provenance: Provenance::Pushed,
        }
    }

    const fn pulled(uri: DiagnosticsKey) -> Self {
        Self {
            uri,
            provenance: Provenance::Pulled,
        }
    }
}

/// Orders the pulls of one server: a later request gets a greater ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PullTicket(u64);

/// Counts how often a server's diagnostics were cleared, so a pull answered by
/// a process that has since been replaced can be recognized.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ClearEpoch(u64);

/// A pull request's claim on the cache, issued by [`NotificationCache::begin_pull`]
/// before the request is sent and redeemed by
/// [`NotificationCache::store_pulled_diagnostics`].
#[derive(Debug, Clone, Copy)]
pub struct PullStamp {
    ticket: PullTicket,
    epoch: ClearEpoch,
    version: i32,
}

impl PullStamp {
    /// Document version the pull was requested at.
    pub(crate) const fn version(self) -> i32 {
        self.version
    }
}

/// Whether the tracker still holds the document version a pull was requested at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionCheck {
    /// The tracked version still equals the stamped one.
    Current,
    /// The document was resynced to another version while the pull was in flight.
    Moved,
}

/// Why a pull report was not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discard {
    /// A pull issued later already stored its report.
    OlderTicket,
    /// The server's diagnostics were cleared after the pull was issued.
    ServerCleared,
    /// The document moved to another version while the pull was in flight.
    VersionMoved,
    /// No entry could be evicted to make room.
    NoRoom,
}

/// What a stored pull report did to the file's pulled slot.
#[derive(Debug)]
pub enum SlotChange {
    /// The slot already held exactly this list.
    Identical,
    /// The slot was replaced; `before` is the file's snapshot taken just
    /// before, so the merged views can be compared.
    Replaced { before: DiagnosticSources },
}

/// Result of [`NotificationCache::store_pulled_diagnostics`].
#[derive(Debug)]
#[must_use]
pub enum PullWrite {
    /// The report is now the file's pulled slot.
    Stored {
        slot: SlotChange,
        /// Files whose slots were evicted to make room.
        evicted: Vec<DiagnosticsKey>,
    },
    /// The report was dropped and is handed back.
    Discarded {
        reason: Discard,
        /// Files whose slots were evicted before the pull was dropped.
        evicted: Vec<DiagnosticsKey>,
        items: BoundedDiagnostics,
    },
}

/// Result of [`NotificationCache::write_published_diagnostics`].
#[derive(Debug)]
#[must_use]
pub struct PushWrite {
    /// Files whose slots were evicted to make room for the published one.
    pub evicted: Vec<DiagnosticsKey>,
}

/// Whether the merged diagnostics of a file differ before and after a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOutcome {
    /// A read would now return something else.
    Changed,
    /// A read would return the same diagnostics.
    Unchanged,
}

impl ChangeOutcome {
    /// Compares two merged views as multisets of serialized diagnostics; an
    /// absent view differs from a present but empty one.
    pub(crate) fn of(before: Option<&DiagnosticInfo>, after: Option<&DiagnosticInfo>) -> Self {
        Self::compare(before, after, |d| serde_json::to_vec(d).ok())
    }

    /// [`Self::of`] over a caller-chosen serializer. A diagnostic that cannot
    /// be serialized makes its view incomparable, which reads as a change:
    /// the conservative side is to notify.
    fn compare(
        before: Option<&DiagnosticInfo>,
        after: Option<&DiagnosticInfo>,
        serialize: impl Fn(&LspDiagnostic) -> Option<Vec<u8>>,
    ) -> Self {
        let fingerprint = |info: &DiagnosticInfo| -> Option<Vec<Vec<u8>>> {
            let mut items = info
                .diagnostics
                .iter()
                .map(&serialize)
                .collect::<Option<Vec<_>>>()?;
            items.sort_unstable();
            Some(items)
        };
        match (before, after) {
            (None, None) => Self::Unchanged,
            (Some(before), Some(after)) => match (fingerprint(before), fingerprint(after)) {
                (Some(before), Some(after)) if before == after => Self::Unchanged,
                _ => Self::Changed,
            },
            _ => Self::Changed,
        }
    }
}

/// Which file's slots a capacity eviction must leave alone.
#[derive(Debug, Clone, Copy)]
enum Protect<'a> {
    Nothing,
    File(&'a DiagnosticsKey),
}

/// One cached diagnostics entry with all of its bookkeeping, so the indices
/// derived from it are only ever touched by `insert_entry`/`take_entry`.
#[derive(Debug)]
struct CachedEntry {
    info: DiagnosticInfo,
    /// Server that published the entry.
    owner: ServerId,
    /// Position in `owner`'s write order.
    seq: u64,
    spelling: Spelling,
}

impl CachedEntry {
    /// Key of the file this entry belongs to, given the entry's own `key`.
    const fn file<'a>(&'a self, key: &'a SlotKey) -> &'a DiagnosticsKey {
        match &self.spelling {
            Spelling::Canonical => &key.uri,
            Spelling::Alias(canonical) => canonical,
        }
    }
}

/// The cache key under which diagnostics for the file behind `uri` are
/// stored, for matching [`NotificationCache::clear_server_diagnostics`]
/// results against a subscription.
pub fn diagnostics_cache_key(uri: &DiagnosticsResourceUri) -> Option<DiagnosticsKey> {
    let path = crate::bridge::resources::parse_uri(uri.as_str()).ok()?;
    let lsp_uri = crate::bridge::try_path_to_uri(path.as_path())?;
    Some(DiagnosticsKey::of(&lsp_uri))
}

/// Maximum number of distinct published URIs (a canonical path and its
/// symlink aliases) whose diagnostics are unioned under one file; bounds how
/// far a misbehaving server can fan a single file out.
const MAX_SOURCES_PER_FILE: usize = 8;

/// Maximum number of server messages to store.
const MAX_SERVER_MESSAGES: usize = 50;

/// Most empty-entry removals remembered for replay on `subscriptions/listen`:
/// as many as the cache can hold entries, so a full cache cleared at once
/// (e.g. a server restart) loses no replayable clear.
const MAX_RECENT_EVICTIONS: usize = MAX_DIAGNOSTIC_ENTRIES;

/// How long an empty-entry removal stays replayable.
const EVICTION_REPLAY_WINDOW: std::time::Duration = std::time::Duration::from_mins(2);

/// File keys of empty entries removed within `EVICTION_REPLAY_WINDOW`.
///
/// `at` is the source of truth; `order` is an expiry queue over it in which an
/// item whose instant differs from `at`'s is stale (the key was re-recorded or
/// dropped) and is skipped. Re-recording a key adds an order item, so `order`
/// is rebuilt from `at` once it exceeds twice `at`'s size, keeping both
/// bounded under churn on few keys. Over `MAX_RECENT_EVICTIONS` the oldest
/// record is forgotten and logged once per window.
#[derive(Debug, Default)]
struct EvictionRecord {
    at: HashMap<DiagnosticsKey, std::time::Instant>,
    order: VecDeque<(DiagnosticsKey, std::time::Instant)>,
    overflow_warned_at: Option<std::time::Instant>,
}

impl EvictionRecord {
    fn record(&mut self, file: DiagnosticsKey, now: std::time::Instant) {
        self.prune_expired(now);
        self.at.insert(file.clone(), now);
        self.order.push_back((file, now));
        while self.at.len() > MAX_RECENT_EVICTIONS {
            self.forget_oldest(now);
        }
        if self.order.len() > self.at.len().saturating_mul(2) {
            self.compact();
        }
    }

    fn contains(&self, key: &DiagnosticsKey, now: std::time::Instant) -> bool {
        self.at
            .get(key)
            .is_some_and(|at| now.saturating_duration_since(*at) <= EVICTION_REPLAY_WINDOW)
    }

    fn is_live(&self, key: &DiagnosticsKey, at: std::time::Instant) -> bool {
        self.at.get(key) == Some(&at)
    }

    fn prune_expired(&mut self, now: std::time::Instant) {
        while let Some((key, at)) = self.order.front() {
            if now.saturating_duration_since(*at) <= EVICTION_REPLAY_WINDOW {
                break;
            }
            if self.is_live(key, *at) {
                self.at.remove(key);
            }
            self.order.pop_front();
        }
    }

    fn forget_oldest(&mut self, now: std::time::Instant) {
        while let Some((key, at)) = self.order.pop_front() {
            if self.is_live(&key, at) {
                self.at.remove(&key);
                self.warn_overflow(now);
                return;
            }
        }
    }

    fn warn_overflow(&mut self, now: std::time::Instant) {
        let due = self
            .overflow_warned_at
            .is_none_or(|at| now.saturating_duration_since(at) > EVICTION_REPLAY_WINDOW);
        if due {
            self.overflow_warned_at = Some(now);
            warn!(
                "more than {MAX_RECENT_EVICTIONS} diagnostics clears evicted within {}s, \
                 forgetting the oldest; a re-attaching listen may miss them",
                EVICTION_REPLAY_WINDOW.as_secs()
            );
        }
    }

    fn compact(&mut self) {
        let mut live: Vec<_> = self.at.iter().map(|(k, at)| (k.clone(), *at)).collect();
        live.sort_by_key(|(_, at)| *at);
        self.order = live.into();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.at.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.at.is_empty()
    }
}

/// Borrows a diagnostic's free-form `message` as plain text, regardless of
/// whether the server sent it as a plain string or (per LSP 3.18)
/// `MarkupContent`.
pub fn message_as_str(message: &lsp_types::Message) -> &str {
    match message {
        lsp_types::Message::String(s) => s,
        lsp_types::Message::MarkupContent(m) => &m.value,
    }
}

/// Information about diagnostics for a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticInfo {
    /// URI of the document.
    pub uri: Uri,
    /// Document version when diagnostics were received.
    pub version: Option<i32>,
    /// List of diagnostics.
    pub diagnostics: Vec<LspDiagnostic>,
}

/// One cached entry of a [`DiagnosticSources`] snapshot.
#[derive(Debug, Clone)]
struct SourceEntry {
    info: DiagnosticInfo,
    /// Whether the server published under the canonical spelling of the path.
    spelling: Spelling,
    provenance: Provenance,
}

/// Owned snapshot of the diagnostics cached for one file, possibly spread
/// over several published URIs (a canonical path plus symlink aliases) and
/// over a pushed and a pulled source.
///
/// Taken under the cache lock by [`NotificationCache::diagnostic_sources`];
/// the clone/dedupe/sort/cap work in [`Self::merge`] then runs after the
/// guard is dropped, so the diagnostics pump is never blocked behind it.
#[derive(Debug, Clone)]
pub struct DiagnosticSources {
    requested: Uri,
    entries: Vec<SourceEntry>,
}

impl DiagnosticSources {
    /// Replaces the snapshot's pulled entry with `diagnostics`, for a pull
    /// report that was answered but not stored.
    pub(crate) fn with_pulled(
        mut self,
        file: &Uri,
        version: Option<i32>,
        diagnostics: BoundedDiagnostics,
    ) -> Self {
        self.entries
            .retain(|entry| entry.provenance != Provenance::Pulled);
        self.entries.push(SourceEntry {
            info: DiagnosticInfo {
                uri: file.clone(),
                version,
                diagnostics: diagnostics.0,
            },
            spelling: Spelling::Canonical,
            provenance: Provenance::Pulled,
        });
        self
    }

    /// Collapses the sources into one entry for the requested file.
    ///
    /// - No source: `None`.
    /// - One pushed source: returned unchanged, with its own `uri` and
    ///   `version`, even when it is a symlink alias.
    /// - Otherwise: pulled diagnostics come first and are never collapsed
    ///   among themselves; a pushed diagnostic is dropped when it duplicates a
    ///   pulled one (same problem, see `PulledIndex::contains_same_problem`) or an earlier
    ///   pushed one (equal in every field). The result is ordered by range
    ///   (stable) and re-capped to the per-entry size bound. The entry's `uri`
    ///   is the requested one and its `version` is that of the pushed source
    ///   published under the canonical spelling, else the pulled one's, else
    ///   `None`.
    ///
    /// # Examples
    ///
    /// ```
    /// use lsp_types::Uri;
    /// use mcpls_core::bridge::NotificationCache;
    ///
    /// let cache = NotificationCache::new();
    /// let uri = Uri::from("file:///workspace/main.rs".to_owned());
    /// let sources = cache.diagnostic_sources(&uri);
    /// assert!(sources.merge().is_none());
    /// ```
    #[must_use]
    pub fn merge(self) -> Option<DiagnosticInfo> {
        let Self { requested, entries } = self;
        let has_pulled = entries
            .iter()
            .any(|entry| entry.provenance == Provenance::Pulled);
        if entries.len() <= 1 && !has_pulled {
            return entries.into_iter().next().map(|entry| entry.info);
        }
        let version = entries
            .iter()
            .find(|entry| {
                entry.provenance == Provenance::Pushed && entry.spelling == Spelling::Canonical
            })
            .and_then(|entry| entry.info.version)
            .or_else(|| {
                entries
                    .iter()
                    .find(|entry| entry.provenance == Provenance::Pulled)
                    .and_then(|entry| entry.info.version)
            });

        let (pulled, pushed): (Vec<SourceEntry>, Vec<SourceEntry>) = entries
            .into_iter()
            .partition(|entry| entry.provenance == Provenance::Pulled);
        let pulled: Vec<LspDiagnostic> = pulled
            .into_iter()
            .flat_map(|entry| entry.info.diagnostics)
            .collect();
        let index = PulledIndex::new(&pulled);
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        let mut kept_pushed = Vec::new();
        for diagnostic in pushed.into_iter().flat_map(|entry| entry.info.diagnostics) {
            if index.contains_same_problem(&diagnostic) {
                continue;
            }
            // A serialization error cannot happen for a `Diagnostic`; keeping
            // the item is the safe fallback.
            if serde_json::to_vec(&diagnostic).map_or(true, |bytes| seen.insert(bytes)) {
                kept_pushed.push(diagnostic);
            }
        }
        drop(index);
        let mut merged = pulled;
        merged.extend(kept_pushed);
        merged.sort_by_key(|d| (point(d.range.start), point(d.range.end)));
        cap_diagnostics_entry_size(&requested, &mut merged);

        Some(DiagnosticInfo {
            uri: requested,
            version,
            diagnostics: merged,
        })
    }
}

/// A log entry from the LSP server.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LogEntry {
    /// Log level.
    pub level: LogLevel,
    /// Log message.
    pub message: String,
    /// Timestamp when the log was received.
    pub timestamp: DateTime<Utc>,
}

/// Log severity level.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum LogLevel {
    /// Error log level.
    Error,
    /// Warning log level.
    Warning,
    /// Info log level.
    Info,
    /// Debug log level.
    Debug,
}

impl LogLevel {
    const fn rank(self) -> u8 {
        match self {
            Self::Error => 0,
            Self::Warning => 1,
            Self::Info => 2,
            Self::Debug => 3,
        }
    }

    /// Whether a log of this level is at least as severe as `min`.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::bridge::LogLevel;
    ///
    /// assert!(LogLevel::Error.meets(LogLevel::Warning));
    /// assert!(!LogLevel::Debug.meets(LogLevel::Info));
    /// assert!(LogLevel::Debug.meets(LogLevel::Debug));
    /// ```
    #[must_use]
    pub const fn meets(self, min: Self) -> bool {
        self.rank() <= min.rank()
    }
}

impl From<lsp_types::MessageType> for LogLevel {
    fn from(msg_type: lsp_types::MessageType) -> Self {
        match msg_type {
            lsp_types::MessageType::Error => Self::Error,
            lsp_types::MessageType::Warning => Self::Warning,
            lsp_types::MessageType::Info => Self::Info,
            // LOG and unknown message types default to Debug
            _ => Self::Debug,
        }
    }
}

/// A message from the LSP server.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ServerMessage {
    /// Message type.
    pub message_type: MessageType,
    /// Message content.
    pub message: String,
    /// Timestamp when the message was received.
    pub timestamp: DateTime<Utc>,
}

/// Server message type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MessageType {
    /// Error message.
    Error,
    /// Warning message.
    Warning,
    /// Info message.
    Info,
    /// Log message.
    Log,
}

impl From<lsp_types::MessageType> for MessageType {
    fn from(msg_type: lsp_types::MessageType) -> Self {
        match msg_type {
            lsp_types::MessageType::Error => Self::Error,
            lsp_types::MessageType::Warning => Self::Warning,
            lsp_types::MessageType::Info => Self::Info,
            // LOG and unknown message types default to Log
            _ => Self::Log,
        }
    }
}

/// Cache for LSP server notifications.
#[derive(Debug)]
pub struct NotificationCache {
    /// Diagnostics entries by published URI.
    entries: HashMap<SlotKey, CachedEntry>,
    /// Per-server entry keys ordered oldest-write-first, keyed by a
    /// monotonic sequence number rather than position: a re-publish removes
    /// its old entry by key in `O(log n)` instead of scanning for it, which a
    /// plain `VecDeque` would require. Not independently capped per server --
    /// only the aggregate across all servers is bounded, by
    /// `MAX_DIAGNOSTIC_ENTRIES` -- but each server's own map length is what
    /// eviction compares against its fair share (see
    /// [`NotificationCache::server_to_evict_from`]) to decide which server
    /// loses an entry once the aggregate is full, so one server's write
    /// volume can never evict another's entries while it still has room
    /// left in the global budget (#266, #276).
    order: HashMap<ServerId, BTreeMap<u64, SlotKey>>,
    /// Canonical file key -> keys of the entries (the canonical path and its
    /// symlink aliases) whose diagnostics are unioned on read.
    files: HashMap<DiagnosticsKey, BTreeSet<SlotKey>>,
    /// Next sequence number to assign in `order`. Shared across every
    /// server's order map and monotonically increasing for the cache's
    /// lifetime; never reused, so it never collides with an older entry
    /// still pending eviction.
    next_diagnostic_seq: u64,
    /// Next ticket handed to a pull by [`Self::begin_pull`].
    next_pull_ticket: u64,
    /// Ticket of the pull behind each file's pulled slot. Written with the
    /// slot by `store_pulled_diagnostics` and dropped with it by `take_entry`,
    /// so a ticket exists exactly while a `Pulled` slot does.
    pull_tickets: HashMap<DiagnosticsKey, PullTicket>,
    /// Per-server count of [`Self::clear_server_diagnostics`] calls; absent
    /// means epoch zero.
    clear_epochs: HashMap<ServerId, ClearEpoch>,
    /// Number of registered diagnostics-route servers currently sharing the
    /// `MAX_DIAGNOSTIC_ENTRIES` budget, explicitly configured via
    /// [`NotificationCache::set_diagnostics_route_count`].
    ///
    /// `None` until that setter is called -- `per_server_budget` then falls
    /// back to the number of servers whose `order` entry is
    /// non-empty (i.e. currently holds at least one entry) rather than
    /// treating an unset count as `1`, which used to hand a single early
    /// publisher the entire budget with no fair-share partitioning at all
    /// (#283). The explicit setter remains the preferred path when the
    /// caller knows it up front: it pre-accounts for servers that are
    /// registered but have not published anything yet, avoiding a window
    /// where an early publisher is temporarily over-allocated before a
    /// slower server's first write grows `order`.
    diagnostics_route_count: Option<usize>,

    /// Count of entries in `entries` whose diagnostics list is currently
    /// empty (`[]`), i.e. an LSP server reporting a previously-tracked file
    /// as now clean. A plain counter, not a duplicated key set, so `0` is an
    /// `O(1)` signal that lets `entry_to_evict` skip its empty-entry search
    /// entirely in the common steady state of a codebase full of real
    /// diagnostics (#284) -- which entry is empty is still answered by
    /// looking the key up in `entries` itself (see the private
    /// `is_empty_entry`), not by mirroring membership here. Maintained only
    /// by `insert_entry`/`take_entry`.
    empty_diagnostics_count: usize,
    /// Empty entries removed within `EVICTION_REPLAY_WINDOW`. Only empty
    /// entries are recorded: a clear lost to eviction is what a listen
    /// re-attaching after its lease gap must still replay, while replaying a
    /// removed non-empty entry would make clients re-read "not published" and
    /// drop valid errors, so a non-empty entry evicted by capacity is never
    /// replayed. Written only by `evict_entry`, never by publishers.
    recent_evictions: EvictionRecord,
    /// Recent log entries (FIFO queue with max size).
    logs: VecDeque<LogEntry>,
    /// Recent server messages (FIFO queue with max size).
    messages: VecDeque<ServerMessage>,
    /// Server ids whose `textDocument/publishDiagnostics` push notifications
    /// are known to be dark: `Translator::respawn_if_dead` replaced a crashed
    /// process for this id, and the replacement's notification receiver is
    /// drained and discarded rather than wired into a running
    /// `diagnostics_pump` (#249's documented trade-off -- the pump's
    /// remaining dependencies live in `serve_with`'s scope, not
    /// `Translator`'s). Once marked, an id is never unmarked here: only a
    /// full mcpls process restart actually restores push diagnostics for
    /// that server, so clearing this on a later respawn attempt would
    /// misreport the cache as fresh again.
    push_degraded: HashSet<ServerId>,
    /// Workspace-indexing readiness per server, driven by recognized
    /// out-of-band signals (rust-analyzer's `experimental/serverStatus`, and
    /// a generic `$/progress` `begin`/`end` sequence) rather than the
    /// `initialize`/`initialized` handshake. See
    /// `Self::observe_indexing_signal` and [`Self::observe_progress`].
    indexing: IndexingTracker,
}

impl Default for NotificationCache {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationCache {
    /// Create a new notification cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::with_capacity(32),
            order: HashMap::new(),
            files: HashMap::new(),
            next_diagnostic_seq: 0,
            next_pull_ticket: 0,
            pull_tickets: HashMap::new(),
            clear_epochs: HashMap::new(),
            diagnostics_route_count: None,
            empty_diagnostics_count: 0,
            recent_evictions: EvictionRecord::default(),
            logs: VecDeque::with_capacity(MAX_LOG_ENTRIES),
            messages: VecDeque::with_capacity(MAX_SERVER_MESSAGES),
            push_degraded: HashSet::new(),
            indexing: IndexingTracker::new(),
        }
    }

    /// Configure how many diagnostics-route servers share the global
    /// `MAX_DIAGNOSTIC_ENTRIES` budget.
    ///
    /// Each server's fair share becomes `MAX_DIAGNOSTIC_ENTRIES / count`
    /// (minimum 1). This does not cap any server's entries by itself -- the
    /// aggregate cache is only ever trimmed once it reaches
    /// `MAX_DIAGNOSTIC_ENTRIES` total -- it only decides, at that point,
    /// which server's oldest entry is the one that gets evicted. Call once
    /// after server registration completes and before diagnostics start
    /// flowing, to pre-account for servers that are registered but have not
    /// published anything yet. If never called, `per_server_budget` derives
    /// the count from the number of servers currently holding at least one
    /// entry instead (#283) -- a consumer that forgets to call this still
    /// gets fair-share partitioning once more than one server has written an
    /// entry, rather than silently handing the whole budget to a single
    /// early publisher.
    pub(crate) fn set_diagnostics_route_count(&mut self, count: usize) {
        self.diagnostics_route_count = Some(count.max(1));
    }

    #[cfg(test)]
    pub(crate) const fn configured_route_count(&self) -> Option<usize> {
        self.diagnostics_route_count
    }

    /// Current per-server fair share of `MAX_DIAGNOSTIC_ENTRIES`, divided
    /// evenly across the configured server count and floored at 1 so a
    /// large server count can never reduce a server's share to zero.
    ///
    /// Uses the explicit count from [`Self::set_diagnostics_route_count`]
    /// when set; otherwise falls back to the number of servers whose
    /// `order` entry is non-empty (#283) -- matching
    /// `server_to_evict_from`'s own `!order.is_empty()` filter, so a server
    /// that has been fully evicted or reassigned away from (an empty but
    /// still-present order map) is not double-counted in the denominator.
    /// Both are floored at 1 so a fresh cache with no entries and no
    /// explicit count yet still yields a usable budget instead of dividing
    /// by zero.
    ///
    /// This is a tie-breaker for eviction, not a hard per-server cap: a
    /// server may hold more than its fair share of entries at any time, as
    /// long as the aggregate across all servers stays within
    /// `MAX_DIAGNOSTIC_ENTRIES` (#276).
    fn per_server_budget(&self) -> usize {
        let count = self
            .diagnostics_route_count
            .unwrap_or_else(|| {
                self.order
                    .values()
                    .filter(|order| !order.is_empty())
                    .count()
            })
            .max(1);
        MAX_DIAGNOSTIC_ENTRIES
            .checked_div(count)
            .unwrap_or(1)
            .max(1)
    }

    /// Picks which server's oldest entry to evict once the aggregate cache
    /// is full: whichever registered server holds the most entries, if that
    /// exceeds its fair share ([`Self::per_server_budget`]) -- so a noisy
    /// server can only ever evict its own entries, never a quiet server's
    /// that is still within its share (#266). If every server (including
    /// `writer`) is within its share, falls back to `writer`'s own oldest
    /// entry, since it is the one currently growing. Falls back further, to
    /// whichever server holds the most entries regardless of share, only in
    /// the edge case where `writer` has no entries of its own yet (its very
    /// first write) while the aggregate is already full purely from other
    /// servers each individually within their share -- otherwise there
    /// would be nothing to evict from and the aggregate cap could be
    /// exceeded despite every server behaving fairly.
    ///
    /// Ties in entry count are broken by `ServerId`, not left to
    /// `HashMap`'s iteration order: `Iterator::max_by_key` returns the
    /// *last* equally-maximal element it sees, and a `HashMap`'s iteration
    /// order is randomized per process, so an `order.len()`-only key would
    /// make the eviction target for a genuine tie vary from run to run.
    /// Every candidate here is a distinct `order` key, so pairing
    /// the count with `id.as_str()` makes the sort key unique per server --
    /// no two entries can ever tie on the full key, which eliminates the
    /// non-determinism outright rather than just picking a fixed side of it.
    fn server_to_evict_from(&self, writer: &ServerId) -> Option<ServerId> {
        let largest = self
            .order
            .iter()
            .filter(|(_, order)| !order.is_empty())
            .max_by_key(|(id, order)| (order.len(), id.as_str()));

        let budget = self.per_server_budget();
        if let Some((id, order)) = largest
            && order.len() > budget
        {
            return Some(id.clone());
        }

        if self
            .order
            .get(writer)
            .is_some_and(|order| !order.is_empty())
        {
            return Some(writer.clone());
        }

        largest.map(|(id, _)| id.clone())
    }

    /// Whether the cached entry for `key` currently has an empty (`[]`)
    /// diagnostics list, i.e. an LSP server reporting a previously-tracked
    /// file as now clean. Derived directly from `entries` rather than
    /// from a separately maintained key set, so there is nothing else to
    /// keep in sync (#284).
    fn is_empty_entry(&self, key: &SlotKey) -> bool {
        self.entries
            .get(key)
            .is_some_and(|entry| entry.info.diagnostics.is_empty())
    }

    /// Whether `key` is a slot of the file `protect` names.
    fn is_protected(&self, key: &SlotKey, protect: Protect<'_>) -> bool {
        match protect {
            Protect::Nothing => false,
            Protect::File(file) => self
                .entries
                .get(key)
                .is_some_and(|entry| entry.file(key) == file),
        }
    }

    /// Oldest unprotected entry in `server`'s own order map whose diagnostics
    /// list is empty, if it has one.
    fn oldest_empty_entry_in(
        &self,
        server: &ServerId,
        protect: Protect<'_>,
    ) -> Option<(u64, SlotKey)> {
        let order = self.order.get(server)?;
        order
            .iter()
            .find(|(_, key)| self.is_empty_entry(key) && !self.is_protected(key, protect))
            .map(|(&seq, key)| (seq, key.clone()))
    }

    /// Oldest unprotected entry in `server`'s own order map.
    fn oldest_entry_in(&self, server: &ServerId, protect: Protect<'_>) -> Option<(u64, SlotKey)> {
        self.order
            .get(server)?
            .iter()
            .find(|(_, key)| !self.is_protected(key, protect))
            .map(|(&seq, key)| (seq, key.clone()))
    }

    /// Which single entry to remove next when the aggregate cache is full
    /// and a genuinely new URI needs room, returned as `(owner, seq, key)`
    /// so the caller can remove it from every index it appears in.
    ///
    /// [`Self::server_to_evict_from`] decides which server is fairness's
    /// primary target; this picks *which of that server's entries* to
    /// actually remove, preferring an empty (`[]`) one over its
    /// strictly-oldest entry wherever one can be found without disturbing a
    /// server that is within its own fair share (#284):
    ///
    /// 1. If the chosen victim itself holds an empty entry, evict its oldest
    ///    one -- a `[]` publish carries no diagnostic content to lose, so
    ///    this lets an older, still-meaningful entry from the same server
    ///    survive in its place.
    /// 2. Otherwise, if some *other* server that also exceeds
    ///    [`Self::per_server_budget`] holds an empty entry, evict that one
    ///    instead of destroying the chosen victim's real diagnostics (S1):
    ///    fairness only protects a server that is within its share, so an
    ///    over-share server's own clean entry is fair game regardless of
    ///    which over-share server `server_to_evict_from` happened to name.
    ///    Ties use the same `(count, id)` key as `server_to_evict_from`, for
    ///    the same determinism reason.
    /// 3. Otherwise -- no empty entry exists anywhere over-budget -- falls
    ///    back to the chosen victim's strictly-oldest entry, exactly as
    ///    before #284.
    ///
    /// Step 1/2's search is skipped entirely when `empty_diagnostics_count`
    /// is `0`, so the common steady state (a codebase full of real
    /// diagnostics, no clean-file churn) pays no extra cost over a plain
    /// oldest-first lookup (#284).
    ///
    /// Entries of the file `protect` names are never picked: a pull write
    /// must not evict the same file's pushed slot it is merged with. When the
    /// chosen server has nothing else, the largest other server's oldest
    /// unprotected entry is taken instead.
    fn entry_to_evict(
        &self,
        writer: &ServerId,
        protect: Protect<'_>,
    ) -> Option<(ServerId, u64, SlotKey)> {
        let evict_from = self.server_to_evict_from(writer)?;

        if self.empty_diagnostics_count > 0 {
            if let Some((seq, key)) = self.oldest_empty_entry_in(&evict_from, protect) {
                return Some((evict_from, seq, key));
            }

            let budget = self.per_server_budget();
            let cross_server_pick = self
                .order
                .iter()
                .filter(|(id, order)| order.len() > budget && *id != &evict_from)
                .filter_map(|(id, order)| {
                    self.oldest_empty_entry_in(id, protect)
                        .map(|(seq, key)| (id, order.len(), seq, key))
                })
                .max_by_key(|(id, len, ..)| (*len, id.as_str()));

            if let Some((id, _, seq, key)) = cross_server_pick {
                return Some((id.clone(), seq, key));
            }
        }

        if let Some((seq, key)) = self.oldest_entry_in(&evict_from, protect) {
            return Some((evict_from, seq, key));
        }
        self.order
            .iter()
            .filter(|(id, _)| *id != &evict_from)
            .filter_map(|(id, order)| {
                self.oldest_entry_in(id, protect)
                    .map(|(seq, key)| (id, order.len(), seq, key))
            })
            .max_by_key(|(id, len, ..)| (*len, id.as_str()))
            .map(|(id, _, seq, key)| (id.clone(), seq, key))
    }

    /// Store diagnostics for a document published by `server_id`, indexed
    /// under its canonical file so every spelling of one file is read back as
    /// the union of what each published.
    ///
    /// Each diagnostic's `message` is truncated to `MAX_ENTRY_TEXT_BYTES`,
    /// and the whole list is bounded to `MAX_DIAGNOSTICS_ENTRY_BYTES`
    /// serialized bytes, before storing (#311). When that bound requires
    /// dropping diagnostics, the *survivors* come back sorted by severity
    /// (`diagnostic_severity_rank`: `ERROR` first), not in the original
    /// publish/file-position order -- see [`Self::diagnostics`].
    ///
    /// If diagnostics already exist for the URI, they are replaced and the
    /// entry is repositioned to the back of its owner's eviction order, so
    /// a URI republished on every edit is tracked as most-recently-written
    /// and evicted last, not first -- and, since it is not a new distinct
    /// URI, never triggers eviction on its own.
    ///
    /// Eviction is work-conserving (#276): storing diagnostics for a
    /// genuinely new URI only evicts an existing entry once the *aggregate*
    /// across every server reaches `MAX_DIAGNOSTIC_ENTRIES`, and then only
    /// the least-recently-written entry of whichever server most exceeds its
    /// fair share, or -- per the fallbacks documented on
    /// `server_to_evict_from` -- the writer's own oldest entry when no
    /// server exceeds its share. A quieter, non-writer server that is within
    /// its fair share is never touched, outside the narrow edge case also
    /// documented there. This lets a single active server use the full
    /// aggregate budget while other registered servers are idle, instead of
    /// being capped at a static equal split regardless of how much of it
    /// they actually use. Which exact entry is removed is further refined by
    /// emptiness -- see the private `entry_to_evict` (#284).
    ///
    /// An empty publish replaces only its own source's entry. At most
    /// `MAX_SOURCES_PER_FILE` spellings are kept per file: a further alias is
    /// dropped before anything is stored, but the canonical spelling is always
    /// admitted, evicting the oldest alias, so a server fanning one file out
    /// cannot suppress the canonical publishes.
    ///
    /// The files whose entries were evicted to make room are returned so the
    /// caller can tell their subscribers (#649).
    pub(crate) fn write_published_diagnostics(
        &mut self,
        server_id: &ServerId,
        published: &PublishedDiagnosticsUri,
        version: Option<i32>,
        diagnostics: Vec<LspDiagnostic>,
    ) -> PushWrite {
        let source_key = SlotKey::pushed(DiagnosticsKey::of(published.source()));
        let canonical_key = DiagnosticsKey::of(published.canonical());

        let already_indexed = self
            .entries
            .get(&source_key)
            .is_some_and(|entry| entry.file(&source_key) == &canonical_key);
        let at_capacity = self.pushed_slot_count(&canonical_key) >= MAX_SOURCES_PER_FILE;
        if !already_indexed && at_capacity {
            let alias_to_evict = published
                .is_canonical()
                .then(|| self.oldest_alias_source(&canonical_key))
                .flatten();
            let Some(oldest_alias) = alias_to_evict else {
                debug!(
                    "dropping diagnostics for {}: {MAX_SOURCES_PER_FILE} published URIs already cached for {}",
                    published.source().as_ref(),
                    published.canonical().as_ref()
                );
                return PushWrite {
                    evicted: Vec::new(),
                };
            };
            self.evict_entry(&oldest_alias);
        }

        let info = DiagnosticInfo {
            uri: published.source().clone(),
            version,
            diagnostics: BoundedDiagnostics::new(published.source(), diagnostics).0,
        };

        self.drop_superseded_pull(&canonical_key, version);

        // A replacement leaves its previous owner's order map (the owner may
        // differ when the diagnostics route changed, e.g. on respawn) and
        // never needs room; only a genuinely new URI can trigger eviction.
        let mut evicted = Vec::new();
        let is_new_entry = self.take_entry(&source_key).is_none();
        if is_new_entry {
            while self.entries.len() >= MAX_DIAGNOSTIC_ENTRIES
                && let Some((_, _, evict_key)) = self.entry_to_evict(server_id, Protect::Nothing)
            {
                if let Some(entry) = self.entries.get(&evict_key) {
                    evicted.push(entry.file(&evict_key).clone());
                }
                self.evict_entry(&evict_key);
            }
        }

        let seq = self.next_seq();
        let spelling = if published.is_canonical() {
            Spelling::Canonical
        } else {
            Spelling::Alias(canonical_key)
        };
        self.insert_entry(
            source_key,
            CachedEntry {
                info,
                owner: server_id.clone(),
                seq,
                spelling,
            },
        );
        PushWrite { evicted }
    }

    /// [`Self::write_published_diagnostics`] for tests that do not look at
    /// what the write evicted.
    #[cfg(test)]
    pub(crate) fn store_published_diagnostics(
        &mut self,
        server_id: &ServerId,
        published: &PublishedDiagnosticsUri,
        version: Option<i32>,
        diagnostics: Vec<LspDiagnostic>,
    ) {
        drop(self.write_published_diagnostics(server_id, published, version, diagnostics));
    }

    const fn next_seq(&mut self) -> u64 {
        let seq = self.next_diagnostic_seq;
        self.next_diagnostic_seq = self.next_diagnostic_seq.saturating_add(1);
        seq
    }

    /// Number of pushed slots (canonical and aliases) indexed under `file`.
    fn pushed_slot_count(&self, file: &DiagnosticsKey) -> usize {
        self.files.get(file).map_or(0, |slots| {
            slots
                .iter()
                .filter(|slot| slot.provenance == Provenance::Pushed)
                .count()
        })
    }

    /// Removes `file`'s pulled slot when a push carries a document version
    /// newer than the one the pull answered, since the pulled content is then
    /// older than what the server last said.
    ///
    /// A push without a version, or with an equal one, keeps the pulled slot:
    /// servers that both push and pull report the same version twice, and a
    /// versionless flycheck push must not erase the pulled native diagnostics.
    // TODO(#670): a slot can outlive a versionless or lower-versioned push
    // (an LRU reopen restarts the tracker at version 1) until the next pull.
    fn drop_superseded_pull(&mut self, file: &DiagnosticsKey, pushed_version: Option<i32>) {
        let Some(pushed) = pushed_version else {
            return;
        };
        let slot = SlotKey::pulled(file.clone());
        let superseded = self
            .entries
            .get(&slot)
            .and_then(|entry| entry.info.version)
            .is_some_and(|pulled| pushed > pulled);
        if superseded {
            self.take_entry(&slot);
        }
    }

    /// Issues the claim a pull of `server`'s diagnostics for a document at
    /// `version` needs to redeem in [`Self::store_pulled_diagnostics`].
    ///
    /// Taken before the request is sent: the ticket orders concurrent pulls
    /// of one file and the epoch exposes a clear that happened meanwhile.
    pub(crate) fn begin_pull(&mut self, server: &ServerId, version: i32) -> PullStamp {
        let ticket = PullTicket(self.next_pull_ticket);
        self.next_pull_ticket = self.next_pull_ticket.saturating_add(1);
        PullStamp {
            ticket,
            epoch: self.clear_epochs.get(server).copied().unwrap_or_default(),
            version,
        }
    }

    /// Stores a `textDocument/diagnostic` report as `file`'s pulled slot,
    /// next to whatever the server pushed for it.
    ///
    /// The report is discarded unless `stamp`'s ticket is newer than the one
    /// stored, the server's diagnostics were not cleared since the stamp was
    /// issued, and `check` says the document still has the stamped version.
    /// Capacity evictions leave the written file's own slots alone and are
    /// returned so the caller can tell their subscribers.
    ///
    /// A slot that was evicted between two pulls no longer carries a ticket,
    /// so an older pull can then be stored; the next pull replaces it.
    pub(crate) fn store_pulled_diagnostics(
        &mut self,
        server_id: &ServerId,
        file: &Uri,
        stamp: PullStamp,
        check: VersionCheck,
        items: BoundedDiagnostics,
    ) -> PullWrite {
        let discard = |reason, items| PullWrite::Discarded {
            reason,
            evicted: Vec::new(),
            items,
        };
        if stamp.epoch
            != self
                .clear_epochs
                .get(server_id)
                .copied()
                .unwrap_or_default()
        {
            return discard(Discard::ServerCleared, items);
        }
        if check == VersionCheck::Moved {
            return discard(Discard::VersionMoved, items);
        }
        let file_key = DiagnosticsKey::of(file);
        let slot_key = SlotKey::pulled(file_key.clone());
        if self
            .pull_tickets
            .get(&file_key)
            .is_some_and(|stored| *stored >= stamp.ticket)
        {
            return discard(Discard::OlderTicket, items);
        }

        let identical = self
            .entries
            .get(&slot_key)
            .is_some_and(|entry| entry.info.diagnostics == items.0);
        let slot = if identical {
            SlotChange::Identical
        } else {
            SlotChange::Replaced {
                before: self.diagnostic_sources(file),
            }
        };

        let mut evicted = Vec::new();
        let is_new_slot = self.take_entry(&slot_key).is_none();
        if is_new_slot {
            while self.entries.len() >= MAX_DIAGNOSTIC_ENTRIES {
                let Some((_, _, victim)) = self.entry_to_evict(server_id, Protect::File(&file_key))
                else {
                    return PullWrite::Discarded {
                        reason: Discard::NoRoom,
                        evicted,
                        items,
                    };
                };
                if let Some(entry) = self.entries.get(&victim) {
                    evicted.push(entry.file(&victim).clone());
                }
                self.evict_entry(&victim);
            }
        }

        let seq = self.next_seq();
        self.pull_tickets.insert(file_key, stamp.ticket);
        self.insert_entry(
            slot_key,
            CachedEntry {
                info: DiagnosticInfo {
                    uri: file.clone(),
                    version: Some(stamp.version),
                    diagnostics: items.0,
                },
                owner: server_id.clone(),
                seq,
                spelling: Spelling::Canonical,
            },
        );
        PullWrite::Stored { slot, evicted }
    }

    /// Stores `diagnostics` as `file`'s pulled slot at document version 1, as
    /// an answered, current pull would.
    #[cfg(test)]
    pub(crate) fn store_pulled_for_test(
        &mut self,
        server_id: &ServerId,
        file: &Uri,
        diagnostics: Vec<LspDiagnostic>,
    ) {
        let stamp = self.begin_pull(server_id, 1);
        drop(self.store_pulled_diagnostics(
            server_id,
            file,
            stamp,
            VersionCheck::Current,
            BoundedDiagnostics::new(file, diagnostics),
        ));
    }

    /// Stores `uri`'s diagnostics as the canonical spelling of its own file.
    #[cfg(test)]
    pub(crate) fn store_diagnostics(
        &mut self,
        server_id: &ServerId,
        uri: &Uri,
        version: Option<i32>,
        diagnostics: Vec<LspDiagnostic>,
    ) {
        let published = PublishedDiagnosticsUri::for_test(uri.clone(), uri.clone());
        self.store_published_diagnostics(server_id, &published, version, diagnostics);
    }

    /// Adds `entry` under `key`, which must not be cached yet, to every
    /// index. Together with [`Self::take_entry`] the only code that touches
    /// `order`, `files` and `empty_diagnostics_count`.
    fn insert_entry(&mut self, key: SlotKey, entry: CachedEntry) {
        debug_assert!(!self.entries.contains_key(&key));
        self.order
            .entry(entry.owner.clone())
            .or_default()
            .insert(entry.seq, key.clone());
        self.files
            .entry(entry.file(&key).clone())
            .or_default()
            .insert(key.clone());
        if entry.info.diagnostics.is_empty() {
            self.empty_diagnostics_count = self.empty_diagnostics_count.saturating_add(1);
        }
        self.entries.insert(key, entry);
    }

    /// Removes the entry cached under `key` from every index and returns it.
    fn take_entry(&mut self, key: &SlotKey) -> Option<CachedEntry> {
        let entry = self.entries.remove(key)?;
        if key.provenance == Provenance::Pulled {
            self.pull_tickets.remove(&key.uri);
        }
        if let Some(order) = self.order.get_mut(&entry.owner) {
            order.remove(&entry.seq);
        }
        let file = entry.file(key);
        if let Some(members) = self.files.get_mut(file) {
            members.remove(key);
            if members.is_empty() {
                self.files.remove(file);
            }
        }
        if entry.info.diagnostics.is_empty() {
            self.empty_diagnostics_count = self.empty_diagnostics_count.saturating_sub(1);
        }
        Some(entry)
    }

    /// Removes the entry under `key` for good (eviction, alias replacement,
    /// server clear) and remembers an empty one in the replay ring, so a
    /// clear lost in a listen's lease gap is still replayed.
    fn evict_entry(&mut self, key: &SlotKey) {
        let Some(entry) = self.take_entry(key) else {
            return;
        };
        if entry.info.diagnostics.is_empty() {
            self.record_empty_eviction(entry.file(key).clone(), std::time::Instant::now());
        }
    }

    fn record_empty_eviction(&mut self, file: DiagnosticsKey, now: std::time::Instant) {
        self.recent_evictions.record(file, now);
    }

    /// Whether `uri` had an empty entry removed within the replay window and
    /// has not been cached again since.
    fn was_recently_evicted(&self, uri: &Uri, now: std::time::Instant) -> bool {
        let key = DiagnosticsKey::of(uri);
        !self.has_diagnostics(uri) && self.recent_evictions.contains(&key, now)
    }

    /// Whether a `subscriptions/listen` attaching now must be told about
    /// `uri`: it has cached diagnostics, or a clear for it was removed
    /// recently enough that a client re-attaching after a lease gap may have
    /// missed it.
    pub(crate) fn is_listen_replayable(&self, uri: &Uri) -> bool {
        self.has_diagnostics(uri) || self.was_recently_evicted(uri, std::time::Instant::now())
    }

    /// Panics unless `order`, `files` and `empty_diagnostics_count` describe
    /// exactly the cached entries.
    #[cfg(test)]
    fn assert_consistent(&self) {
        let mut ordered = 0;
        for (server, order) in &self.order {
            for (seq, key) in order {
                let entry = self
                    .entries
                    .get(key)
                    .unwrap_or_else(|| panic!("ordered key has no entry"));
                assert_eq!((&entry.owner, entry.seq), (server, *seq));
                ordered += 1;
            }
        }
        assert_eq!(ordered, self.entries.len(), "order and entries diverge");
        let pulled: BTreeSet<&DiagnosticsKey> = self
            .entries
            .keys()
            .filter(|key| key.provenance == Provenance::Pulled)
            .map(|key| &key.uri)
            .collect();
        assert_eq!(
            pulled,
            self.pull_tickets.keys().collect::<BTreeSet<_>>(),
            "pull tickets and pulled slots diverge"
        );

        let mut filed = 0;
        for (file, members) in &self.files {
            assert!(!members.is_empty(), "empty file set kept");
            for key in members {
                let entry = self
                    .entries
                    .get(key)
                    .unwrap_or_else(|| panic!("filed key has no entry"));
                assert_eq!(entry.file(key), file);
                filed += 1;
            }
        }
        assert_eq!(filed, self.entries.len(), "files and entries diverge");

        let empty = self
            .entries
            .values()
            .filter(|entry| entry.info.diagnostics.is_empty())
            .count();
        assert_eq!(empty, self.empty_diagnostics_count);
    }

    /// The least recently written non-canonical source indexed under
    /// `canonical_key`.
    fn oldest_alias_source(&self, canonical_key: &DiagnosticsKey) -> Option<SlotKey> {
        self.files
            .get(canonical_key)?
            .iter()
            .filter_map(|source| {
                let entry = self.entries.get(source)?;
                matches!(entry.spelling, Spelling::Alias(_)).then_some((entry.seq, source))
            })
            .min_by_key(|(seq, _)| *seq)
            .map(|(_, source)| source.clone())
    }

    /// Keys of every entry cached for the file `key` names: its indexed
    /// slots plus a pushed entry stored directly under `key` itself.
    fn source_keys(&self, key: &DiagnosticsKey) -> BTreeSet<SlotKey> {
        let mut keys: BTreeSet<SlotKey> =
            self.files.get(key).into_iter().flatten().cloned().collect();
        let direct = SlotKey::pushed(key.clone());
        if self.entries.contains_key(&direct) {
            keys.insert(direct);
        }
        keys
    }

    /// Store a log entry.
    ///
    /// Maintains a maximum of `MAX_LOG_ENTRIES` entries, removing oldest when full.
    /// `message` is truncated to `MAX_ENTRY_TEXT_BYTES` before storing.
    pub(crate) fn store_log(&mut self, level: LogLevel, message: String) {
        let entry = LogEntry {
            level,
            message: truncate_string(message, MAX_ENTRY_TEXT_BYTES),
            timestamp: Utc::now(),
        };

        if self.logs.len() >= MAX_LOG_ENTRIES {
            self.logs.pop_front();
        }
        self.logs.push_back(entry);
    }

    /// Store a server message.
    ///
    /// Maintains a maximum of `MAX_SERVER_MESSAGES` entries, removing oldest when full.
    /// `message` is truncated to `MAX_ENTRY_TEXT_BYTES` before storing.
    pub(crate) fn store_message(&mut self, message_type: MessageType, message: String) {
        let msg = ServerMessage {
            message_type,
            message: truncate_string(message, MAX_ENTRY_TEXT_BYTES),
            timestamp: Utc::now(),
        };

        if self.messages.len() >= MAX_SERVER_MESSAGES {
            self.messages.pop_front();
        }
        self.messages.push_back(msg);
    }

    /// Record a workspace-readiness signal from an unrecognized/custom LSP
    /// notification (`LspNotification::Other`), updating `server_id`'s
    /// tracked [`IndexingState`] if the notification is one this cache
    /// understands.
    ///
    /// Currently recognizes rust-analyzer's `experimental/serverStatus`
    /// notification: a `quiescent` boolean of `false` marks the
    /// server [`IndexingState::Loading`], `true` marks it
    /// [`IndexingState::Ready`]. Any other method, or a `serverStatus`
    /// payload missing/malformed the field, leaves the current state
    /// untouched rather than erroring -- an unrecognized signal is
    /// equivalent to no signal.
    ///
    /// Once a server reaches [`IndexingState::Ready`] it never regresses on
    /// its own: this only tracks the *initial* workspace load, not later
    /// re-indexing triggered by large-scale file changes. See
    /// [`Self::reset_indexing_state`] for the one case that does move a
    /// server back out of `Ready`/`Loading`.
    pub(crate) fn observe_indexing_signal(
        &mut self,
        server_id: &ServerId,
        method: &str,
        params: Option<&serde_json::Value>,
    ) {
        self.indexing
            .observe_server_status(server_id, method, params);
    }

    /// Record a `$/progress` notification toward `server_id`'s tracked
    /// [`IndexingState`] -- the generic LSP counterpart to
    /// `Self::observe_indexing_signal`'s rust-analyzer-specific
    /// `experimental/serverStatus`. See
    /// [`crate::bridge::indexing::IndexingTracker::observe_progress`] for
    /// the full begin/end/settle/latch transition rules.
    pub(crate) fn observe_progress(
        &mut self,
        server_id: &ServerId,
        params: &lsp_types::ProgressParams,
    ) {
        self.indexing.observe_progress(server_id, params);
    }

    /// Configure `server_id`'s [`IndexingPolicy`] -- see
    /// [`crate::bridge::indexing::IndexingTracker::set_policy`].
    pub(crate) fn set_indexing_policy(&mut self, server_id: ServerId, policy: IndexingPolicy) {
        self.indexing.set_policy(server_id, policy);
    }

    /// Current tracked workspace-indexing readiness for `server_id`.
    ///
    /// Returns [`IndexingState::Unknown`] for a server no readiness signal
    /// has ever been observed for -- see `Self::observe_indexing_signal`.
    ///
    /// A `Loading` entry older than `INDEXING_STALENESS_BOUND` is read
    /// back as `Unknown` rather than `Loading`: this is the self-heal for a
    /// `quiescent: true` notification dropped by a full channel, or a
    /// server that stalled mid-index, applied at *read* time based on the
    /// signal's own age so it can never be triggered by (and can never
    /// affect) any individual caller's own wait -- see
    /// `Translator::wait_for_indexing_ready`.
    #[must_use]
    pub fn indexing_state(&self, server_id: &ServerId) -> IndexingState {
        self.indexing.state(server_id)
    }

    /// Reset `server_id`'s tracked [`IndexingState`] as `reset` says;
    /// [`IndexingReset::Forget`] reverts it to [`IndexingState::Unknown`],
    /// [`IndexingReset::AwaitReplacement`] to [`IndexingState::Loading`] until
    /// the replacement's first signal when the server has reported one before.
    ///
    /// `Translator::respawn_if_dead` calls this after replacing a crashed
    /// server's process, since the new process starts indexing from
    /// scratch and has sent no signal of its own yet -- a stale
    /// `Ready`/`Loading` carried over from the crashed connection must not
    /// leak into requests routed to its replacement. This is the only
    /// production caller: a timed-out [`Self::indexing_state`] read does
    /// *not* call this, so one caller's wait can never affect another's.
    pub(crate) fn reset_indexing_state(&mut self, server_id: &ServerId, reset: IndexingReset) {
        self.indexing.reset(server_id, reset);
    }

    /// Get diagnostics for a document URI.
    ///
    /// If the stored list was ever truncated by `store_published_diagnostics`'s
    /// `MAX_DIAGNOSTICS_ENTRY_BYTES` cap (#311), the diagnostics here are in
    /// severity order (`ERROR` first), not the original publish/file-position
    /// order -- callers that assume file-position order should not rely on
    /// it after a cap-triggered truncation.
    ///
    /// Looks up the pushed entry stored under exactly `uri`. A file reachable
    /// through symlinks may have its diagnostics spread over several
    /// published URIs, and a pull may have stored its own list next to them;
    /// use [`Self::diagnostic_sources`] to read the union.
    #[inline]
    #[must_use]
    pub fn diagnostics(&self, uri: &Uri) -> Option<&DiagnosticInfo> {
        self.entries
            .get(&SlotKey::pushed(DiagnosticsKey::of(uri)))
            .map(|entry| &entry.info)
    }

    /// Snapshot of every entry cached for the file `uri` names, to be
    /// [merged](DiagnosticSources::merge) after the cache lock is released.
    ///
    /// Clones the entries (at most 8 pushed and 1 pulled, each bounded to
    /// 1 MiB) so the merge can run without the lock.
    ///
    /// # Examples
    ///
    /// ```
    /// use lsp_types::Uri;
    /// use mcpls_core::bridge::NotificationCache;
    ///
    /// let cache = NotificationCache::new();
    /// let uri = Uri::from("file:///workspace/main.rs".to_owned());
    /// let snapshot = cache.diagnostic_sources(&uri);
    /// // Merge once the cache lock has been released.
    /// assert!(snapshot.merge().is_none());
    /// ```
    #[must_use]
    pub fn diagnostic_sources(&self, uri: &Uri) -> DiagnosticSources {
        let key = DiagnosticsKey::of(uri);
        let entries = self
            .source_keys(&key)
            .into_iter()
            .filter_map(|source| {
                let entry = self.entries.get(&source)?;
                Some(SourceEntry {
                    info: entry.info.clone(),
                    spelling: entry.spelling.clone(),
                    provenance: source.provenance,
                })
            })
            .collect();
        DiagnosticSources {
            requested: uri.clone(),
            entries,
        }
    }

    /// Whether any diagnostics entry (possibly empty) is cached for the file
    /// `uri` names, under any of its published URIs.
    ///
    /// # Examples
    ///
    /// ```
    /// use lsp_types::Uri;
    /// use mcpls_core::bridge::NotificationCache;
    ///
    /// let cache = NotificationCache::new();
    /// let uri = Uri::from("file:///workspace/main.rs".to_owned());
    /// assert!(!cache.has_diagnostics(&uri));
    /// ```
    #[must_use]
    pub fn has_diagnostics(&self, uri: &Uri) -> bool {
        let key = DiagnosticsKey::of(uri);
        self.entries.contains_key(&SlotKey::pushed(key.clone())) || self.files.contains_key(&key)
    }

    /// Server that published the currently cached diagnostics for `uri`, if
    /// any. Used to look up that server's negotiated position encoding for a
    /// cache-only read that has no live LSP round trip of its own to resolve
    /// one from.
    #[inline]
    #[must_use]
    pub fn diagnostics_owner(&self, uri: &Uri) -> Option<&ServerId> {
        let key = DiagnosticsKey::of(uri);
        self.entries
            .get(&SlotKey::pushed(key.clone()))
            .or_else(|| {
                let slots = self.files.get(&key)?;
                slots
                    .iter()
                    .filter(|slot| slot.provenance == Provenance::Pushed)
                    .chain(slots.iter())
                    .find_map(|source| self.entries.get(source))
            })
            .map(|entry| &entry.owner)
    }

    /// All stored log entries.
    #[inline]
    #[must_use]
    pub const fn logs(&self) -> &VecDeque<LogEntry> {
        &self.logs
    }

    /// All stored server messages.
    #[inline]
    #[must_use]
    pub const fn messages(&self) -> &VecDeque<ServerMessage> {
        &self.messages
    }

    /// Clear all diagnostics owned by a single server.
    ///
    /// Used when a server crashes and respawns: its own stale entries must
    /// be invalidated without disturbing any other server's cache entries
    /// (#266). Returns the keys of the cleared files so a caller can tell
    /// subscribers which resources changed (see [`diagnostics_cache_key`]).
    ///
    /// Also invalidates every pull of this server issued before the call, so
    /// a report answered by the replaced process is not stored afterwards.
    pub(crate) fn clear_server_diagnostics(&mut self, server_id: &ServerId) -> Vec<DiagnosticsKey> {
        let epoch = self.clear_epochs.entry(server_id.clone()).or_default();
        epoch.0 = epoch.0.saturating_add(1);
        let Some(order) = self.order.remove(server_id) else {
            return Vec::new();
        };
        let mut cleared = Vec::with_capacity(order.len());
        for key in order.into_values() {
            if let Some(entry) = self.entries.get(&key) {
                cleared.push(entry.file(&key).clone());
            }
            self.evict_entry(&key);
        }
        cleared.sort_unstable();
        cleared.dedup();
        cleared
    }

    /// Marks `server_id`'s push-based diagnostics as no longer live -- see
    /// the `push_degraded` field doc for why this is permanent for the life
    /// of the cache.
    pub(crate) fn mark_push_degraded(&mut self, server_id: &ServerId) {
        self.push_degraded.insert(server_id.clone());
    }

    /// Marks `server_id`'s push-based diagnostics as live again, after its
    /// notification pump was re-wired by a manual restart.
    pub(crate) fn clear_push_degraded(&mut self, server_id: &ServerId) {
        self.push_degraded.remove(server_id);
    }

    /// Whether `server_id`'s push-based diagnostics are known to be
    /// degraded (see [`Self::mark_push_degraded`]) -- callers such as
    /// `get_cached_diagnostics` and `read_resource` use this to flag a
    /// cache-only result as potentially incomplete rather than presenting it as
    /// current.
    #[inline]
    #[must_use]
    pub(crate) fn is_push_degraded(&self, server_id: &ServerId) -> bool {
        self.push_degraded.contains(server_id)
    }

    /// Get the number of documents with stored diagnostics.
    #[inline]
    #[must_use]
    #[cfg(test)]
    pub(crate) fn diagnostics_count(&self) -> usize {
        self.entries.len()
    }

    /// Get the number of stored log entries.
    #[inline]
    #[must_use]
    #[cfg(test)]
    pub(crate) fn logs_count(&self) -> usize {
        self.logs.len()
    }

    /// Get the number of stored server messages.
    #[inline]
    #[must_use]
    #[cfg(test)]
    pub(crate) fn messages_count(&self) -> usize {
        self.messages.len()
    }
}

/// Applies one lifecycle-lane notification to `cache`.
///
/// Only `$/progress` and the generic `Other` variant (which carries e.g.
/// rust-analyzer's `experimental/serverStatus`) ever travel on that lane,
/// since the notification lane handles diagnostics/log/showMessage instead
/// (see `LspClient::message_loop_inner`'s routing).
///
/// Shared by `diagnostics_pump`'s lifecycle-lane arm (wiring a freshly
/// spawned server's own pump) and `Translator::respawn_if_dead`'s
/// lifecycle-lane forwarding task (wiring a respawned server's replacement
/// process), so the two can't silently drift apart on which notification
/// variants feed which `NotificationCache` method.
///
/// Public because [`LspServer::take_lifecycle_rx`](crate::lsp::LspServer::take_lifecycle_rx)
/// is: whoever drains that receiver needs this as the sink that feeds the
/// indexing state.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::{IndexingState, NotificationCache, apply_lifecycle_notification};
/// use mcpls_core::config::ServerId;
/// use mcpls_core::lsp::LspNotification;
///
/// let mut cache = NotificationCache::new();
/// let server_id = ServerId::from("rust");
/// apply_lifecycle_notification(
///     &mut cache,
///     &server_id,
///     LspNotification::Other {
///         method: "experimental/serverStatus".into(),
///         params: Some(serde_json::json!({"quiescent": true})),
///     },
/// );
/// assert_eq!(cache.indexing_state(&server_id), IndexingState::Ready);
/// ```
pub fn apply_lifecycle_notification(
    cache: &mut NotificationCache,
    server_id: &ServerId,
    notif: crate::lsp::LspNotification,
) {
    match notif {
        crate::lsp::LspNotification::Progress(params) => {
            cache.observe_progress(server_id, &params);
        }
        crate::lsp::LspNotification::Other { method, params } => {
            cache.observe_indexing_signal(server_id, &method, params.as_ref());
        }
        crate::lsp::LspNotification::PublishDiagnostics(_)
        | crate::lsp::LspNotification::LogMessage(_)
        | crate::lsp::LspNotification::ShowMessage(_) => {}
    }
}

/// Handles one lifecycle-lane notification of `server_id`: warns when a pinned
/// `tsserver` was ignored, then applies it to `cache` when there is one.
///
/// Every consumer of the lifecycle lane goes through here, so no consumer can
/// skip a step (#658).
pub async fn on_lifecycle(
    cache: Option<&tokio::sync::Mutex<NotificationCache>>,
    server_id: &ServerId,
    pinned_tsserver: Option<&std::path::Path>,
    notif: crate::lsp::LspNotification,
) {
    crate::lsp::tsserver_pin::warn_if_pin_ignored(pinned_tsserver, &notif, server_id.as_str());
    if let Some(cache) = cache {
        apply_lifecycle_notification(&mut *cache.lock().await, server_id, notif);
    }
}

#[cfg(test)]
mod pull_tests;

#[cfg(test)]
mod tests {
    use lsp_types::{Position, Range};

    use super::bounds::*;
    use super::*;
    use crate::bridge::indexing::IndexingReset;
    use crate::test_lsp::CapturedLogs;

    /// Every test in this module that doesn't exercise multi-server
    /// fairness routes through one implicit server, so `set_diagnostics_route_count`
    /// is left at its default of `1` (full `MAX_DIAGNOSTIC_ENTRIES` budget).
    fn test_server() -> ServerId {
        ServerId::from("test-server")
    }

    #[test]
    fn test_notification_cache_new() {
        let cache = NotificationCache::new();
        assert_eq!(cache.diagnostics_count(), 0);
        assert_eq!(cache.logs_count(), 0);
        assert_eq!(cache.messages_count(), 0);
    }

    #[test]
    fn test_store_and_diagnostics() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");

        let diagnostic = LspDiagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::Error),
            message: "test error".to_string().into(),
            code: None,
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);

        let stored = cache.diagnostics(&uri).unwrap();
        assert_eq!(stored.uri, uri);
        assert_eq!(stored.version, Some(1));
        assert_eq!(stored.diagnostics.len(), 1);
        assert_eq!(
            stored.diagnostics[0].message,
            lsp_types::Message::String("test error".to_string())
        );
    }

    /// #311: a single diagnostic's `message` must be bounded independently
    /// of `MAX_DIAGNOSTIC_ENTRIES`, which only caps the number of entries.
    #[test]
    fn test_store_diagnostics_truncates_oversized_message() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");
        let oversized = "a".repeat(MAX_ENTRY_TEXT_BYTES + 100);

        let diagnostic = LspDiagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::Error),
            message: oversized.clone().into(),
            code: None,
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);

        let stored = cache.diagnostics(&uri).unwrap();
        let stored = message_as_str(&stored.diagnostics[0].message);
        assert!(stored.len() < oversized.len());
        assert!(stored.ends_with("... (truncated)"));
    }

    /// Minimal diagnostic with an arbitrary `message`, for tests that only
    /// care about size/count bounds rather than range/severity details.
    fn minimal_diagnostic(message: String) -> LspDiagnostic {
        LspDiagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::Error),
            message: message.into(),
            code: None,
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        }
    }

    /// #311 C1: `MAX_ENTRY_TEXT_BYTES` alone bounds one `message` field, not
    /// the whole entry -- many diagnostics, each individually small, must
    /// still be capped in aggregate.
    #[test]
    fn test_store_diagnostics_caps_aggregate_size_for_many_small_diagnostics() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");

        // Each diagnostic is far under MAX_ENTRY_TEXT_BYTES individually,
        // but 5000 of them comfortably exceeds MAX_DIAGNOSTICS_ENTRY_BYTES
        // in aggregate.
        let diagnostics: Vec<LspDiagnostic> = (0..5000)
            .map(|i| {
                minimal_diagnostic(format!(
                    "diagnostic number {i}, padded: {}",
                    "x".repeat(200)
                ))
            })
            .collect();
        let original_count = diagnostics.len();

        cache.store_diagnostics(&test_server(), &uri, Some(1), diagnostics);

        let stored = &cache.diagnostics(&uri).unwrap().diagnostics;
        assert!(
            stored.len() < original_count,
            "aggregate cap must trim the list, kept {} of {original_count}",
            stored.len()
        );
        assert!(!stored.is_empty(), "must keep at least one diagnostic");
        let serialized_len = serde_json::to_vec(stored).unwrap().len();
        assert!(
            serialized_len <= MAX_DIAGNOSTICS_ENTRY_BYTES,
            "stored entry must fit the aggregate cap, got {serialized_len} bytes"
        );
    }

    /// #311 S6: a naive flat halve would keep only the first N/2
    /// diagnostics even when far more than that would actually fit --
    /// truncation must find the largest prefix that fits instead.
    #[test]
    fn test_store_diagnostics_truncation_keeps_largest_fitting_prefix() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");

        // Each diagnostic serializes to roughly 300 bytes; ~3800 of them
        // fit under the 1 MiB cap, well over half of the 5000 published --
        // a flat halve would incorrectly stop at 2500.
        let diagnostics: Vec<LspDiagnostic> = (0..5000)
            .map(|i| minimal_diagnostic(format!("diagnostic {i}: {}", "x".repeat(250))))
            .collect();

        cache.store_diagnostics(&test_server(), &uri, Some(1), diagnostics);

        let stored = &cache.diagnostics(&uri).unwrap().diagnostics;
        assert!(
            stored.len() > 2600,
            "largest-fitting-prefix search must keep far more than half, kept {}",
            stored.len()
        );
        let serialized_len = serde_json::to_vec(stored).unwrap().len();
        assert!(serialized_len <= MAX_DIAGNOSTICS_ENTRY_BYTES);
        // The search must find the *largest* fitting prefix, not just *a*
        // fitting one: one more diagnostic than what was kept must no
        // longer fit (otherwise it should have been kept too).
        let mut with_one_more = stored.clone();
        with_one_more.push(minimal_diagnostic(format!(
            "diagnostic overflow: {}",
            "x".repeat(250)
        )));
        assert!(
            serde_json::to_vec(&with_one_more).unwrap().len() > MAX_DIAGNOSTICS_ENTRY_BYTES,
            "kept count must be the largest that fits, not merely a fitting count"
        );
    }

    /// #311 S6: truncation must prefer keeping higher-severity diagnostics,
    /// not just whichever the server happened to publish first -- a late
    /// `ERROR` must survive over leading `HINT`-level noise.
    #[test]
    fn test_store_diagnostics_truncation_prefers_higher_severity() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");

        let mut diagnostics: Vec<LspDiagnostic> = (0..5000)
            .map(|i| {
                let mut d = minimal_diagnostic(format!("hint {i}: {}", "x".repeat(200)));
                d.severity = Some(lsp_types::DiagnosticSeverity::Hint);
                d
            })
            .collect();
        let mut trailing_error = minimal_diagnostic("the one real error".to_string());
        trailing_error.severity = Some(lsp_types::DiagnosticSeverity::Error);
        diagnostics.push(trailing_error);

        cache.store_diagnostics(&test_server(), &uri, Some(1), diagnostics);

        let stored = &cache.diagnostics(&uri).unwrap().diagnostics;
        assert!(
            stored
                .iter()
                .any(|d| message_as_str(&d.message) == "the one real error"),
            "the trailing ERROR diagnostic must survive truncation over leading HINT noise"
        );
    }

    /// #311 S7 / M7: truncating the diagnostics list must not be silent --
    /// a caller with no visibility into this cache would otherwise have no
    /// way to know a `get_cached_diagnostics` result is incomplete.
    #[test]
    fn test_store_diagnostics_warns_when_truncating_list() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");
        let diagnostics: Vec<LspDiagnostic> = (0..5000)
            .map(|i| minimal_diagnostic(format!("diagnostic {i}: {}", "x".repeat(250))))
            .collect();

        let captured = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        cache.store_diagnostics(&test_server(), &uri, Some(1), diagnostics);
        drop(guard);

        let messages = captured.messages();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("highest-severity") && m.contains("file:///test.rs")),
            "expected a truncation warning naming the URI, got: {messages:?}"
        );
    }

    /// #311 S7: dropping a diagnostic's `data` breaks the LSP contract that
    /// it round-trips to a later `textDocument/codeAction` request -- this
    /// must be logged, not silent.
    #[test]
    fn test_store_diagnostics_warns_when_dropping_data_blob() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");
        let mut diagnostic = minimal_diagnostic("small message".to_string());
        diagnostic.data = Some(serde_json::json!({
            "blob": "x".repeat(MAX_DIAGNOSTICS_ENTRY_BYTES + 1000),
        }));

        let captured = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);
        drop(guard);

        let messages = captured.messages();
        assert!(
            messages.iter().any(|m| m.contains("code-action")),
            "expected a warning noting the code-action quick-fix impact, got: {messages:?}"
        );
    }

    /// #311 C1: a single diagnostic dominated by an oversized `data` blob
    /// must be capped even though `message` alone is small -- the aggregate
    /// list-halving path can't shrink a one-element list, so the opaque
    /// fields on that single diagnostic must be dropped instead.
    #[test]
    fn test_store_diagnostics_drops_oversized_data_blob_on_single_diagnostic() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");

        let mut diagnostic = minimal_diagnostic("small message".to_string());
        diagnostic.data = Some(serde_json::json!({
            "blob": "x".repeat(MAX_DIAGNOSTICS_ENTRY_BYTES + 1000),
        }));

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);

        let stored = &cache.diagnostics(&uri).unwrap().diagnostics;
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].message,
            lsp_types::Message::String("small message".to_string())
        );
        assert!(
            stored[0].data.is_none(),
            "oversized data blob must be dropped"
        );
        let serialized_len = serde_json::to_vec(stored).unwrap().len();
        assert!(
            serialized_len <= MAX_DIAGNOSTICS_ENTRY_BYTES,
            "stored entry must fit the aggregate cap after dropping data, got {serialized_len} bytes"
        );
    }

    /// #311 C1 follow-up: an oversized `source` (not `data`) on a single
    /// diagnostic must also be brought back under the cap -- the
    /// opaque-field-drop mitigation alone does not touch `source`, which is
    /// a plain string and must be truncated instead.
    #[test]
    fn test_store_diagnostics_truncates_oversized_source_on_single_diagnostic() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");

        let mut diagnostic = minimal_diagnostic("small message".to_string());
        diagnostic.source = Some("x".repeat(MAX_DIAGNOSTICS_ENTRY_BYTES + 1000));

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);

        let stored = &cache.diagnostics(&uri).unwrap().diagnostics;
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].message,
            lsp_types::Message::String("small message".to_string())
        );
        let serialized_len = serde_json::to_vec(stored).unwrap().len();
        assert!(
            serialized_len <= MAX_DIAGNOSTICS_ENTRY_BYTES,
            "stored entry must fit the aggregate cap after truncating source, got {serialized_len} bytes"
        );
    }

    /// #311 C1 follow-up: `cap_diagnostics_entry_size`'s postcondition --
    /// the result always fits `MAX_DIAGNOSTICS_ENTRY_BYTES` -- must hold
    /// even when every uncapped field is maxed out simultaneously, not just
    /// one at a time. This is the terminal-enforcement guarantee itself,
    /// exercised end to end through `store_diagnostics` rather than by
    /// calling the private function directly.
    #[test]
    fn test_store_diagnostics_caps_single_diagnostic_with_every_field_maxed_out() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");

        // Each field individually exceeds MAX_ENTRY_TEXT_BYTES (so
        // source/code truncation is exercised) and the combination exceeds
        // MAX_DIAGNOSTICS_ENTRY_BYTES, without needing to allocate multiple
        // megabytes per field just to prove the same point.
        let mut diagnostic = minimal_diagnostic("x".repeat(MAX_ENTRY_TEXT_BYTES + 1000));
        diagnostic.source = Some("x".repeat(MAX_ENTRY_TEXT_BYTES + 1000));
        diagnostic.code = Some(lsp_types::Code::String(
            "x".repeat(MAX_ENTRY_TEXT_BYTES + 1000),
        ));
        diagnostic.data = Some(serde_json::json!({ "blob": "x".repeat(MAX_ENTRY_TEXT_BYTES) }));
        diagnostic.tags = Some(vec![lsp_types::DiagnosticTag::Unnecessary; 50]);
        diagnostic.related_information = Some(vec![
            lsp_types::DiagnosticRelatedInformation {
                location: lsp_types::Location {
                    uri: uri.clone(),
                    range: Range::default(),
                },
                message: "x".repeat(1000),
            };
            5
        ]);

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);

        let stored = &cache.diagnostics(&uri).unwrap().diagnostics;
        assert_eq!(stored.len(), 1);
        let serialized_len = serde_json::to_vec(stored).unwrap().len();
        assert!(
            serialized_len <= MAX_DIAGNOSTICS_ENTRY_BYTES,
            "postcondition must hold even with every field maxed out, got {serialized_len} bytes"
        );
    }

    /// #311 C1 follow-up: exercises `cap_diagnostics_entry_size`'s terminal
    /// fallback directly. `message` is the one field the field-specific
    /// mitigations never touch (they only cover
    /// `source`/`code`/`data`/`code_description`/`related_information`/
    /// `tags`), so an oversized, *untruncated* message -- as it would be if
    /// this private function were ever called without `store_diagnostics`'s
    /// own prior message truncation -- must still be brought under budget
    /// by the terminal step, not left to slip through.
    #[test]
    fn test_cap_diagnostics_entry_size_terminal_fallback_bounds_untruncated_message() {
        let uri: Uri = Uri::from("file:///test.rs");
        let mut diagnostics = vec![minimal_diagnostic(
            "x".repeat(MAX_DIAGNOSTICS_ENTRY_BYTES + 1000),
        )];

        cap_diagnostics_entry_size(&uri, &mut diagnostics);

        assert_eq!(diagnostics.len(), 1);
        assert!(
            message_as_str(&diagnostics[0].message).len()
                <= DIAGNOSTIC_TERMINAL_FALLBACK_MESSAGE_BYTES + 20,
            "terminal fallback must truncate the message itself, got {} bytes",
            message_as_str(&diagnostics[0].message).len()
        );
        let serialized_len = serde_json::to_vec(&diagnostics).unwrap().len();
        assert!(
            serialized_len <= MAX_DIAGNOSTICS_ENTRY_BYTES,
            "postcondition must hold via the terminal fallback, got {serialized_len} bytes"
        );
    }

    /// #311 S5: when no diagnostic carries `data`/`code_description`/
    /// `related_information`/`tags` and the cheap size estimate is already
    /// under budget, nothing should be modified -- the fast path must not
    /// alter content it didn't need to touch.
    #[test]
    fn test_store_diagnostics_cheap_path_leaves_small_diagnostics_untouched() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");

        let mut diagnostic = minimal_diagnostic("a small, ordinary diagnostic message".to_string());
        diagnostic.source = Some("rustc".to_string());

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);

        let stored = &cache.diagnostics(&uri).unwrap().diagnostics;
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].message,
            lsp_types::Message::String("a small, ordinary diagnostic message".to_string())
        );
        assert_eq!(stored[0].source.as_deref(), Some("rustc"));
    }

    /// #311 S5 follow-up: the critic's exact counterexample. A NUL-heavy
    /// message's *raw* byte length looks small enough for the cheap
    /// estimate to skip the real check, but its *serialized* (JSON-escaped)
    /// size is up to `JSON_ESCAPE_WORST_CASE_FACTOR`x larger -- each NUL
    /// byte costs 6 bytes as `\u0000` once JSON-encoded. Three diagnostics
    /// at exactly `MAX_ENTRY_TEXT_BYTES` of NULs each previously passed the
    /// old raw-length estimate (787,200 bytes, under the 1 MiB cap) while
    /// actually serializing to roughly 4.5 MiB -- letting an entry ~4.5x
    /// over budget skip `fits`/truncation/terminal-fallback entirely.
    #[test]
    fn test_store_diagnostics_cheap_path_escape_safe_for_control_character_heavy_message() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");

        let nul_heavy_message = "\0".repeat(MAX_ENTRY_TEXT_BYTES);
        let diagnostics: Vec<LspDiagnostic> = (0..3)
            .map(|_| minimal_diagnostic(nul_heavy_message.clone()))
            .collect();

        cache.store_diagnostics(&test_server(), &uri, Some(1), diagnostics);

        let stored = &cache.diagnostics(&uri).unwrap().diagnostics;
        let serialized_len = serde_json::to_vec(stored).unwrap().len();
        assert!(
            serialized_len <= MAX_DIAGNOSTICS_ENTRY_BYTES,
            "escape-heavy content must not let the cheap-estimate fast path skip the real cap, \
             got {serialized_len} bytes"
        );
    }

    #[test]
    fn test_store_diagnostics_replaces_existing() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        assert_eq!(cache.diagnostics_count(), 1);

        cache.store_diagnostics(&test_server(), &uri, Some(2), vec![]);
        assert_eq!(cache.diagnostics_count(), 1);

        let stored = cache.diagnostics(&uri).unwrap();
        assert_eq!(stored.version, Some(2));
    }

    #[test]
    fn test_store_and_get_logs() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error message".to_string());
        cache.store_log(LogLevel::Info, "info message".to_string());

        let logs = cache.logs();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].level, LogLevel::Error);
        assert_eq!(logs[0].message, "error message");
        assert_eq!(logs[1].level, LogLevel::Info);
        assert_eq!(logs[1].message, "info message");
    }

    #[test]
    fn test_logs_max_capacity() {
        let mut cache = NotificationCache::new();

        // Add more than MAX_LOG_ENTRIES
        for i in 0..MAX_LOG_ENTRIES + 10 {
            cache.store_log(LogLevel::Info, format!("message {i}"));
        }

        assert_eq!(cache.logs_count(), MAX_LOG_ENTRIES);

        // Oldest entries should be removed (FIFO)
        let logs = cache.logs();
        assert_eq!(logs.front().unwrap().message, "message 10");
        assert_eq!(
            logs.back().unwrap().message,
            format!("message {}", MAX_LOG_ENTRIES + 9)
        );
    }

    /// #311: `MAX_LOG_ENTRIES` bounds the number of log entries, but not the
    /// size of any one entry -- an oversized message must be truncated
    /// rather than stored verbatim.
    #[test]
    fn test_store_log_truncates_oversized_message() {
        let mut cache = NotificationCache::new();
        let oversized = "a".repeat(MAX_ENTRY_TEXT_BYTES + 100);

        cache.store_log(LogLevel::Info, oversized.clone());

        let stored = &cache.logs()[0].message;
        assert!(stored.len() < oversized.len());
        assert!(stored.ends_with("... (truncated)"));
    }

    #[test]
    fn test_store_log_does_not_truncate_message_at_or_below_limit() {
        let mut cache = NotificationCache::new();
        let message = "a".repeat(MAX_ENTRY_TEXT_BYTES);

        cache.store_log(LogLevel::Info, message.clone());

        assert_eq!(cache.logs()[0].message, message);
    }

    #[test]
    fn test_store_and_get_messages() {
        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Error, "error msg".to_string());
        cache.store_message(MessageType::Warning, "warning msg".to_string());

        let messages = cache.messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].message_type, MessageType::Error);
        assert_eq!(messages[0].message, "error msg");
        assert_eq!(messages[1].message_type, MessageType::Warning);
        assert_eq!(messages[1].message, "warning msg");
    }

    #[test]
    fn test_messages_max_capacity() {
        let mut cache = NotificationCache::new();

        // Add more than MAX_SERVER_MESSAGES
        for i in 0..MAX_SERVER_MESSAGES + 10 {
            cache.store_message(MessageType::Info, format!("message {i}"));
        }

        assert_eq!(cache.messages_count(), MAX_SERVER_MESSAGES);

        // Oldest entries should be removed (FIFO)
        let messages = cache.messages();
        assert_eq!(messages.front().unwrap().message, "message 10");
        assert_eq!(
            messages.back().unwrap().message,
            format!("message {}", MAX_SERVER_MESSAGES + 9)
        );
    }

    /// #311: same per-entry byte cap as `store_log`, applied to server messages.
    #[test]
    fn test_store_message_truncates_oversized_message() {
        let mut cache = NotificationCache::new();
        let oversized = "a".repeat(MAX_ENTRY_TEXT_BYTES + 100);

        cache.store_message(MessageType::Info, oversized.clone());

        let stored = &cache.messages()[0].message;
        assert!(stored.len() < oversized.len());
        assert!(stored.ends_with("... (truncated)"));
    }

    #[test]
    fn test_log_levels() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error".to_string());
        cache.store_log(LogLevel::Warning, "warning".to_string());
        cache.store_log(LogLevel::Info, "info".to_string());
        cache.store_log(LogLevel::Debug, "debug".to_string());

        let logs = cache.logs();
        assert_eq!(logs[0].level, LogLevel::Error);
        assert_eq!(logs[1].level, LogLevel::Warning);
        assert_eq!(logs[2].level, LogLevel::Info);
        assert_eq!(logs[3].level, LogLevel::Debug);
    }

    #[test]
    fn test_message_types() {
        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Error, "error".to_string());
        cache.store_message(MessageType::Warning, "warning".to_string());
        cache.store_message(MessageType::Info, "info".to_string());
        cache.store_message(MessageType::Log, "log".to_string());

        let messages = cache.messages();
        assert_eq!(messages[0].message_type, MessageType::Error);
        assert_eq!(messages[1].message_type, MessageType::Warning);
        assert_eq!(messages[2].message_type, MessageType::Info);
        assert_eq!(messages[3].message_type, MessageType::Log);
    }

    #[test]
    fn test_timestamp_ordering() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Info, "first".to_string());
        std::thread::sleep(std::time::Duration::from_millis(10));
        cache.store_log(LogLevel::Info, "second".to_string());

        let logs = cache.logs();
        assert!(logs[0].timestamp < logs[1].timestamp);
    }

    #[test]
    fn test_store_diagnostics_empty_list() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");

        let diagnostic = LspDiagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::Error),
            message: "test error".to_string().into(),
            code: None,
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);
        assert_eq!(cache.diagnostics(&uri).unwrap().diagnostics.len(), 1);

        cache.store_diagnostics(&test_server(), &uri, Some(2), vec![]);
        let stored = cache.diagnostics(&uri).unwrap();
        assert_eq!(stored.diagnostics.len(), 0);
        assert_eq!(stored.version, Some(2));
    }

    #[test]
    fn test_store_many_diagnostics_single_file() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");

        let diagnostics: Vec<LspDiagnostic> = (0..100)
            .map(|i| LspDiagnostic {
                range: Range {
                    start: Position {
                        line: i,
                        character: 0,
                    },
                    end: Position {
                        line: i,
                        character: 10,
                    },
                },
                message: format!("Error {i}").into(),
                severity: Some(lsp_types::DiagnosticSeverity::Error),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            })
            .collect();

        cache.store_diagnostics(&test_server(), &uri, Some(1), diagnostics);

        let stored = cache.diagnostics(&uri).unwrap();
        assert_eq!(stored.diagnostics.len(), 100);
    }

    #[test]
    fn test_logs_exact_capacity_boundary() {
        let mut cache = NotificationCache::new();

        for i in 0..MAX_LOG_ENTRIES {
            cache.store_log(LogLevel::Info, format!("message {i}"));
        }
        assert_eq!(cache.logs_count(), MAX_LOG_ENTRIES);

        cache.store_log(LogLevel::Info, "overflow".to_string());
        assert_eq!(cache.logs_count(), MAX_LOG_ENTRIES);
        assert_eq!(cache.logs().front().unwrap().message, "message 1");
    }

    #[test]
    fn test_messages_exact_capacity_boundary() {
        let mut cache = NotificationCache::new();

        for i in 0..MAX_SERVER_MESSAGES {
            cache.store_message(MessageType::Info, format!("message {i}"));
        }
        assert_eq!(cache.messages_count(), MAX_SERVER_MESSAGES);

        cache.store_message(MessageType::Info, "overflow".to_string());
        assert_eq!(cache.messages_count(), MAX_SERVER_MESSAGES);
        assert_eq!(cache.messages().front().unwrap().message, "message 1");
    }

    #[test]
    fn test_diagnostics_max_capacity() {
        let mut cache = NotificationCache::new();

        for i in 0..MAX_DIAGNOSTIC_ENTRIES + 10 {
            let uri: Uri = Uri::from(format!("file:///test{i}.rs"));
            cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        }

        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);

        // Oldest entries should be evicted (FIFO).
        let evicted: Uri = Uri::from("file:///test0.rs");
        assert!(cache.diagnostics(&evicted).is_none());
        let newest: Uri = Uri::from(format!("file:///test{}.rs", MAX_DIAGNOSTIC_ENTRIES + 9));
        assert!(cache.diagnostics(&newest).is_some());
    }

    #[test]
    fn test_diagnostics_replacing_existing_uri_does_not_trigger_eviction() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///stable.rs");

        for i in 0..MAX_DIAGNOSTIC_ENTRIES {
            cache.store_diagnostics(
                &test_server(),
                &uri,
                Some(i32::try_from(i).unwrap()),
                vec![],
            );
        }
        assert_eq!(cache.diagnostics_count(), 1);
        assert!(cache.diagnostics(&uri).is_some());
    }

    #[test]
    fn test_diagnostics_republish_refreshes_eviction_order() {
        // #234 S2 / #266 S3 regression: an actively-edited file, republished
        // on every keystroke, must not be evicted ahead of a file that was
        // merely opened once and never touched again.
        let mut cache = NotificationCache::new();
        let actively_edited: Uri = Uri::from("file:///keep.rs");
        cache.store_diagnostics(&test_server(), &actively_edited, Some(1), vec![]);

        // Fill the rest of the cache with untouched entries.
        for i in 0..MAX_DIAGNOSTIC_ENTRIES - 1 {
            let uri: Uri = Uri::from(format!("file:///untouched{i}.rs"));
            cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        }
        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);

        // Republish the actively-edited file -- this must move it to the
        // back of the eviction order, not leave it at its original (oldest)
        // position.
        cache.store_diagnostics(&test_server(), &actively_edited, Some(2), vec![]);

        // One more new URI arrives, exceeding the cap by one: the oldest
        // *untouched* entry must be evicted, not the republished one.
        let overflow: Uri = Uri::from("file:///overflow.rs");
        cache.store_diagnostics(&test_server(), &overflow, Some(1), vec![]);

        assert!(
            cache.diagnostics(&actively_edited).is_some(),
            "republished entry must survive eviction after being refreshed"
        );
        let oldest_untouched: Uri = Uri::from("file:///untouched0.rs");
        assert!(
            cache.diagnostics(&oldest_untouched).is_none(),
            "the oldest never-republished entry must be evicted instead"
        );
        assert!(cache.diagnostics(&overflow).is_some());
    }

    #[test]
    fn test_store_diagnostics_no_version() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///test.rs");

        cache.store_diagnostics(&test_server(), &uri, None, vec![]);
        let stored = cache.diagnostics(&uri).unwrap();
        assert_eq!(stored.version, None);
    }

    /// #266/#276: once the *aggregate* cache is full, a noisy server that has
    /// grown far past its fair share must have its own oldest entries
    /// evicted, never a quiet server's, even though both share one
    /// `NotificationCache` and the noisy server was allowed to keep growing
    /// past its static equal share while the aggregate still had room.
    #[test]
    fn test_noisy_server_does_not_evict_quiet_server_entries() {
        let mut cache = NotificationCache::new();
        cache.set_diagnostics_route_count(2);
        let noisy = ServerId::from("noisy");
        let quiet = ServerId::from("quiet");

        let quiet_uri: Uri = Uri::from("file:///quiet/only_file.rs");
        cache.store_diagnostics(&quiet, &quiet_uri, Some(1), vec![]);

        // Drive the noisy server well past the aggregate cap -- it must be
        // allowed to consume nearly all of it since the quiet server leaves
        // the rest unused (#276), and once the aggregate is full it must
        // only evict its own oldest entries.
        for i in 0..MAX_DIAGNOSTIC_ENTRIES + 50 {
            let uri: Uri = Uri::from(format!("file:///noisy/file{i}.rs"));
            cache.store_diagnostics(&noisy, &uri, Some(1), vec![]);
        }

        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);
        assert!(
            cache.diagnostics(&quiet_uri).is_some(),
            "quiet server's only entry must survive the noisy server's overflow"
        );

        let noisy_first: Uri = Uri::from("file:///noisy/file0.rs");
        assert!(
            cache.diagnostics(&noisy_first).is_none(),
            "noisy server's own oldest entries must be evicted once the aggregate cache is full"
        );
    }

    /// #276: a dominant server must be able to exceed its static equal share
    /// of the budget while other registered diagnostics-route servers are
    /// idle -- eviction is work-conserving and only triggers once the
    /// *aggregate* cache reaches `MAX_DIAGNOSTIC_ENTRIES`, not once a single
    /// server passes `MAX_DIAGNOSTIC_ENTRIES / diagnostics_route_count`.
    #[test]
    fn test_dominant_server_exceeds_equal_share_while_others_idle() {
        let mut cache = NotificationCache::new();
        cache.set_diagnostics_route_count(4);
        let dominant = ServerId::from("dominant");

        let equal_share = MAX_DIAGNOSTIC_ENTRIES / 4;
        let more_than_share = equal_share + 100;
        for i in 0..more_than_share {
            let uri: Uri = Uri::from(format!("file:///file{i}.rs"));
            cache.store_diagnostics(&dominant, &uri, Some(1), vec![]);
        }
        assert_eq!(
            cache.diagnostics_count(),
            more_than_share,
            "a dominant server must be able to exceed its static equal share while the aggregate has room"
        );

        // The other three registered servers never write anything, so the
        // dominant server can keep growing all the way to the full budget.
        for i in more_than_share..MAX_DIAGNOSTIC_ENTRIES {
            let uri: Uri = Uri::from(format!("file:///file{i}.rs"));
            cache.store_diagnostics(&dominant, &uri, Some(1), vec![]);
        }
        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);
    }

    /// M1: eviction-target ties (multiple servers holding the same entry
    /// count) must resolve deterministically, not depend on `HashMap`'s
    /// per-process randomized iteration order. This pins the exact winner
    /// rather than only checking repeat-call stability -- stability across
    /// calls would hold trivially even without the fix, since a single
    /// `HashMap` instance's iteration order does not change between calls
    /// within one process; the real risk is a *different* winner on a
    /// *different* process run, which this test can't observe directly, but
    /// the pinned assertion below only passes because the tie-break key
    /// (`(order.len(), id.as_str())`) is unique per server -- no two
    /// distinct `ServerId`s can ever share it, so `max_by_key` never
    /// actually has a tie left to resolve by iteration order.
    #[test]
    fn test_eviction_target_tie_break_is_deterministic() {
        let mut cache = NotificationCache::new();
        cache.set_diagnostics_route_count(1000); // fair share floors at 1

        let a = ServerId::from("a");
        let b = ServerId::from("b");
        for i in 0..2 {
            let uri: Uri = Uri::from(format!("file:///a/file{i}.rs"));
            cache.store_diagnostics(&a, &uri, Some(1), vec![]);
        }
        for i in 0..2 {
            let uri: Uri = Uri::from(format!("file:///b/file{i}.rs"));
            cache.store_diagnostics(&b, &uri, Some(1), vec![]);
        }

        // `a` and `b` are tied at 2 entries each, both over the floor-1
        // share -- `"b"` sorts after `"a"` lexicographically, so it is the
        // one always picked.
        let writer = ServerId::from("writer");
        assert_eq!(cache.server_to_evict_from(&writer), Some(b));
    }

    /// M2: `server_to_evict_from`'s "largest in-share server" fallback is
    /// reachable and correct through the public `store_diagnostics` API,
    /// not just in isolation -- a brand-new server's first write must still
    /// evict something when the aggregate cache is already full purely from
    /// other servers that are each individually within their fair share.
    /// Without this fallback there would be nothing to evict from (the
    /// writer has no entries yet, and no one else exceeds their share) and
    /// the aggregate could grow past `MAX_DIAGNOSTIC_ENTRIES`.
    #[test]
    fn test_new_writer_still_evicts_when_every_existing_server_is_in_share() {
        let mut cache = NotificationCache::new();
        cache.set_diagnostics_route_count(2); // fair share = 500 each

        let a = ServerId::from("a");
        let b = ServerId::from("b");
        for i in 0..500 {
            let uri: Uri = Uri::from(format!("file:///a/file{i}.rs"));
            cache.store_diagnostics(&a, &uri, Some(1), vec![]);
        }
        for i in 0..500 {
            let uri: Uri = Uri::from(format!("file:///b/file{i}.rs"));
            cache.store_diagnostics(&b, &uri, Some(1), vec![]);
        }
        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);

        // `c` has never written before -- its very first write hits a full,
        // entirely-in-share aggregate.
        let c = ServerId::from("c");
        let new_uri: Uri = Uri::from("file:///c/first.rs");
        cache.store_diagnostics(&c, &new_uri, Some(1), vec![]);

        assert_eq!(
            cache.diagnostics_count(),
            MAX_DIAGNOSTIC_ENTRIES,
            "the aggregate cap must still be enforced even when every existing server is within share"
        );
        assert!(cache.diagnostics(&new_uri).is_some());

        // `a` and `b` are tied at 500 entries each; the deterministic
        // tie-break in `server_to_evict_from` picks `b`, so `b`'s oldest
        // entry is the one evicted, not `a`'s.
        let b_oldest: Uri = Uri::from("file:///b/file0.rs");
        assert!(
            cache.diagnostics(&b_oldest).is_none(),
            "the largest in-share server (tie-broken to b) must lose its oldest entry"
        );
        assert!(
            cache
                .diagnostics(&Uri::from("file:///a/file0.rs".to_owned()))
                .is_some(),
            "the other in-share server must be untouched"
        );
    }

    /// Re-publishing diagnostics for a URI under its existing owner must not
    /// count as a new entry against that server's budget.
    #[test]
    fn test_repeated_writes_same_owner_do_not_grow_order_map() {
        let mut cache = NotificationCache::new();
        let server = ServerId::from("server");
        let uri: Uri = Uri::from("file:///test.rs");

        let max_version = i32::try_from(MAX_DIAGNOSTIC_ENTRIES).unwrap() + 10;
        for version in 0..max_version {
            cache.store_diagnostics(&server, &uri, Some(version), vec![]);
        }

        assert_eq!(cache.diagnostics_count(), 1);
        let stored = cache.diagnostics(&uri).unwrap();
        assert_eq!(stored.version, Some(max_version - 1));
    }

    /// If a URI's diagnostics route changes to a different server (e.g.
    /// after a respawn rebind), the entry must move to the new owner's
    /// order map rather than staying attributed to the old one.
    #[test]
    fn test_store_diagnostics_reassigns_ownership() {
        let mut cache = NotificationCache::new();
        let old_owner = ServerId::from("old");
        let new_owner = ServerId::from("new");
        let uri: Uri = Uri::from("file:///test.rs");

        cache.store_diagnostics(&old_owner, &uri, Some(1), vec![]);
        cache.store_diagnostics(&new_owner, &uri, Some(2), vec![]);

        assert_eq!(cache.diagnostics_count(), 1);
        let stored = cache.diagnostics(&uri).unwrap();
        assert_eq!(stored.version, Some(2));

        // The old owner's order map must no longer reference this URI:
        // filling the old owner's budget with fresh entries must not evict
        // this URI a second time (it's not there to evict) nor corrupt state.
        for i in 0..MAX_DIAGNOSTIC_ENTRIES + 5 {
            let other: Uri = Uri::from(format!("file:///old/file{i}.rs"));
            cache.store_diagnostics(&old_owner, &other, Some(1), vec![]);
        }
        assert!(cache.diagnostics(&uri).is_some());
    }

    /// #290: `diagnostics_owner` is what a cache-only read (e.g.
    /// `get_cached_diagnostics`) uses to resolve the publishing server's
    /// negotiated position encoding, so both branches -- an owner on record
    /// and none -- must behave correctly.
    #[test]
    fn test_diagnostics_owner_returns_publisher_after_store() {
        let mut cache = NotificationCache::new();
        let server = ServerId::from("rust");
        let uri: Uri = Uri::from("file:///main.rs");

        cache.store_diagnostics(&server, &uri, Some(1), vec![]);

        assert_eq!(cache.diagnostics_owner(&uri), Some(&server));
    }

    #[test]
    fn test_diagnostics_owner_none_for_untracked_uri() {
        let cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///never-seen.rs");

        assert_eq!(cache.diagnostics_owner(&uri), None);
    }

    /// Reassigning ownership (see `test_store_diagnostics_reassigns_ownership`
    /// above) must also update `diagnostics_owner`, not just the cached
    /// content -- otherwise a stale owner's encoding would be used to
    /// convert a different server's diagnostics.
    #[test]
    fn test_diagnostics_owner_reflects_reassigned_ownership() {
        let mut cache = NotificationCache::new();
        let old_owner = ServerId::from("old");
        let new_owner = ServerId::from("new");
        let uri: Uri = Uri::from("file:///test.rs");

        cache.store_diagnostics(&old_owner, &uri, Some(1), vec![]);
        assert_eq!(cache.diagnostics_owner(&uri), Some(&old_owner));

        cache.store_diagnostics(&new_owner, &uri, Some(2), vec![]);
        assert_eq!(cache.diagnostics_owner(&uri), Some(&new_owner));
    }

    /// #266 S2: clearing one server's diagnostics must not disturb another
    /// server's cached entries.
    #[test]
    fn test_clear_server_diagnostics_scopes_to_one_server() {
        let mut cache = NotificationCache::new();
        let crashed = ServerId::from("crashed");
        let healthy = ServerId::from("healthy");

        let crashed_uri: Uri = Uri::from("file:///crashed/main.py");
        let healthy_uri: Uri = Uri::from("file:///healthy/main.rs");
        cache.store_diagnostics(&crashed, &crashed_uri, Some(1), vec![]);
        cache.store_diagnostics(&healthy, &healthy_uri, Some(1), vec![]);

        cache.clear_server_diagnostics(&crashed);

        assert!(cache.diagnostics(&crashed_uri).is_none());
        assert!(cache.diagnostics(&healthy_uri).is_some());
        assert_eq!(cache.diagnostics_count(), 1);

        // Idempotent / no-op for a server with no (or no longer any) entries.
        cache.clear_server_diagnostics(&crashed);
        assert_eq!(cache.diagnostics_count(), 1);
    }

    /// #359: `mark_push_degraded` must be scoped per server and, once set,
    /// stay set -- there is no "unmark" operation, since only a full mcpls
    /// process restart actually restores push diagnostics for a respawned
    /// server.
    #[test]
    fn test_push_degraded_is_scoped_per_server_and_permanent() {
        let mut cache = NotificationCache::new();
        let degraded = ServerId::from("degraded");
        let healthy = ServerId::from("healthy");

        assert!(!cache.is_push_degraded(&degraded));
        assert!(!cache.is_push_degraded(&healthy));

        cache.mark_push_degraded(&degraded);

        assert!(cache.is_push_degraded(&degraded));
        assert!(!cache.is_push_degraded(&healthy));

        // Marking again is idempotent.
        cache.mark_push_degraded(&degraded);
        assert!(cache.is_push_degraded(&degraded));
    }

    /// #276: `set_diagnostics_route_count` shrinking a server's fair share
    /// must not retroactively evict any of its already-cached entries --
    /// eviction is work-conserving and only fires once the *aggregate* cache
    /// is full. Once full, though, the shrunk share is what makes that
    /// server the eviction target for a *different* server's write, rather
    /// than the write that actually needed room being rejected or evicting
    /// its own (nonexistent) entries.
    #[test]
    fn test_shrinking_budget_affects_eviction_target_not_existing_entries() {
        let mut cache = NotificationCache::new();
        let server = ServerId::from("server");

        for i in 0..MAX_DIAGNOSTIC_ENTRIES {
            let uri: Uri = Uri::from(format!("file:///file{i}.rs"));
            cache.store_diagnostics(&server, &uri, Some(1), vec![]);
        }
        assert_eq!(
            cache.diagnostics_count(),
            MAX_DIAGNOSTIC_ENTRIES,
            "filling to the aggregate cap must not evict anything early"
        );

        // A drastic shrink relative to the entries `server` already holds --
        // must not evict anything by itself.
        cache.set_diagnostics_route_count(4);
        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);

        // A different server's first write, once the aggregate is full,
        // evicts from `server` (now far over its shrunk share) instead.
        let other = ServerId::from("other");
        let new_uri: Uri = Uri::from("file:///other/new.rs");
        cache.store_diagnostics(&other, &new_uri, Some(1), vec![]);

        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);
        assert!(cache.diagnostics(&new_uri).is_some());
        let server_oldest: Uri = Uri::from("file:///file0.rs");
        assert!(
            cache.diagnostics(&server_oldest).is_none(),
            "the pre-existing server's oldest entry, now far over its shrunk share, must be evicted"
        );
    }

    /// #283: an external `NotificationCache` consumer that never calls
    /// `set_diagnostics_route_count` must still get fair-share partitioning
    /// once more than one server has written an entry -- the pre-#266
    /// regression this guards against is an unset count silently giving one
    /// server the entire aggregate budget, letting it starve a quiet server.
    #[test]
    fn test_fair_share_applies_by_default_without_explicit_route_count() {
        let mut cache = NotificationCache::new();
        let noisy = ServerId::from("noisy");
        let quiet = ServerId::from("quiet");

        let quiet_uri: Uri = Uri::from("file:///quiet/only_file.rs");
        cache.store_diagnostics(&quiet, &quiet_uri, Some(1), vec![]);

        // `set_diagnostics_route_count` is deliberately never called here.
        for i in 0..MAX_DIAGNOSTIC_ENTRIES + 50 {
            let uri: Uri = Uri::from(format!("file:///noisy/file{i}.rs"));
            cache.store_diagnostics(&noisy, &uri, Some(1), vec![]);
        }

        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);
        assert!(
            cache.diagnostics(&quiet_uri).is_some(),
            "quiet server's only entry must survive even without ever calling \
             set_diagnostics_route_count"
        );
        let noisy_first: Uri = Uri::from("file:///noisy/file0.rs");
        assert!(
            cache.diagnostics(&noisy_first).is_none(),
            "the noisy server, now auto-derived as one of two servers sharing the budget, \
             must still lose its own oldest entries once over its fair share"
        );
    }

    /// #283: with only one server ever writing, the auto-derived fair-share
    /// count (from `order.len()`) must stay `1`, letting that
    /// server use the whole aggregate budget -- the same as the old default
    /// of `1` when the setter went uncalled, not a regression for the
    /// common single-server case.
    #[test]
    fn test_single_server_gets_full_budget_without_explicit_route_count() {
        let mut cache = NotificationCache::new();

        for i in 0..MAX_DIAGNOSTIC_ENTRIES {
            let uri: Uri = Uri::from(format!("file:///file{i}.rs"));
            cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        }

        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);
    }

    /// #284: when the cache is full and a server's own entry must be
    /// evicted, an entry with an empty (`[]`) diagnostics list -- a file
    /// reported as now clean -- must be evicted ahead of an older entry that
    /// still carries real diagnostics, even though the empty entry is not
    /// that server's strictly-oldest entry.
    #[test]
    fn test_empty_diagnostics_entries_evicted_before_non_empty_ones() {
        let mut cache = NotificationCache::new();
        let server = test_server();

        let important: Uri = Uri::from("file:///important.rs");
        cache.store_diagnostics(
            &server,
            &important,
            Some(1),
            vec![minimal_diagnostic("real error".to_string())],
        );

        // Fill the rest of the budget with empty ("file is clean") entries,
        // all published after `important` and so all newer in eviction
        // order.
        for i in 0..MAX_DIAGNOSTIC_ENTRIES - 1 {
            let uri: Uri = Uri::from(format!("file:///clean{i}.rs"));
            cache.store_diagnostics(&server, &uri, Some(1), vec![]);
        }
        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);

        // One more new URI exceeds the cap: `important` is the strictly
        // oldest entry, but it must survive in favor of the oldest *empty*
        // entry instead.
        let overflow: Uri = Uri::from("file:///overflow.rs");
        cache.store_diagnostics(&server, &overflow, Some(1), vec![]);

        assert!(
            cache.diagnostics(&important).is_some(),
            "a non-empty entry must survive eviction over empty entries, even though it is older"
        );
        let oldest_clean: Uri = Uri::from("file:///clean0.rs");
        assert!(
            cache.diagnostics(&oldest_clean).is_none(),
            "the oldest empty entry must be evicted instead of the older non-empty one"
        );
        assert!(cache.diagnostics(&overflow).is_some());
    }

    /// #284: storing an empty diagnostics list must still create a fully
    /// tracked, cacheable entry -- eviction priority changes which entry is
    /// removed once the cache is full, but does not change what gets stored
    /// in the first place. See the `tracked` field semantics in
    /// `mcp::server::ResourceDiagnosticsResponse` for why callers rely on
    /// this: a `[]` publish for a previously-tracked URI must read back as
    /// "tracked, zero diagnostics", not as untracked.
    #[test]
    fn test_empty_diagnostics_entry_is_still_tracked_until_evicted() {
        let mut cache = NotificationCache::new();
        let uri: Uri = Uri::from("file:///clean.rs");

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);

        let stored = cache.diagnostics(&uri);
        assert!(
            stored.is_some(),
            "an empty-diagnostics entry must still be tracked"
        );
        assert_eq!(stored.unwrap().diagnostics.len(), 0);
    }

    /// #284 S1: an over-share server's own empty ("clean") entry must be
    /// evicted before a *different* over-share server's real diagnostics are
    /// destroyed, even when the fairness-selected victim (the largest
    /// over-share server) itself holds no empty entry of its own.
    #[test]
    fn test_over_share_servers_empty_entry_evicted_before_a_different_servers_real_diagnostic() {
        let mut cache = NotificationCache::new();
        cache.set_diagnostics_route_count(3); // fair share = 333

        let a = ServerId::from("a"); // over share, all real diagnostics
        let b = ServerId::from("b"); // over share, all empty/clean
        let c = ServerId::from("c"); // within share

        for i in 0..500 {
            let uri: Uri = Uri::from(format!("file:///a/file{i}.rs"));
            cache.store_diagnostics(
                &a,
                &uri,
                Some(1),
                vec![minimal_diagnostic(format!("error {i}"))],
            );
        }
        for i in 0..400 {
            let uri: Uri = Uri::from(format!("file:///b/file{i}.rs"));
            cache.store_diagnostics(&b, &uri, Some(1), vec![]);
        }
        for i in 0..100 {
            let uri: Uri = Uri::from(format!("file:///c/file{i}.rs"));
            cache.store_diagnostics(&c, &uri, Some(1), vec![]);
        }
        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);

        // One more write from `a` (the fairness-selected victim, being the
        // largest over-share server) must not destroy any of `a`'s real
        // diagnostics: `b`, also over its share, has an empty entry to give
        // up instead.
        let overflow: Uri = Uri::from("file:///a/overflow.rs");
        cache.store_diagnostics(
            &a,
            &overflow,
            Some(1),
            vec![minimal_diagnostic("overflow error".to_string())],
        );

        for i in 0..500 {
            let uri: Uri = Uri::from(format!("file:///a/file{i}.rs"));
            assert!(
                cache.diagnostics(&uri).is_some(),
                "server a's real diagnostics must all survive; b has an empty entry to lose \
                 instead"
            );
        }
        let b_oldest: Uri = Uri::from("file:///b/file0.rs");
        assert!(
            cache.diagnostics(&b_oldest).is_none(),
            "b's oldest empty entry must be evicted instead of a's real diagnostics"
        );
        assert!(cache.diagnostics(&overflow).is_some());
    }

    /// #284: a URI that transitions non-empty -> empty -> non-empty must not
    /// be treated as still-empty for eviction priority after the second
    /// transition -- emptiness tracking must reflect the *current* state,
    /// not the URI's history.
    #[test]
    fn test_dirty_then_clean_then_dirty_again_updates_emptiness_tracking() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        let uri: Uri = Uri::from("file:///flapping.rs");

        cache.store_diagnostics(
            &server,
            &uri,
            Some(1),
            vec![minimal_diagnostic("first error".to_string())],
        );
        cache.store_diagnostics(&server, &uri, Some(2), vec![]); // now clean
        cache.store_diagnostics(
            &server,
            &uri,
            Some(3),
            vec![minimal_diagnostic("second error".to_string())],
        ); // dirty again

        // Fill the rest of the budget with genuinely empty entries -- if
        // `uri` were still (wrongly) tracked as empty, one of these would be
        // evicted in its place instead of `uri` being left alone.
        for i in 0..MAX_DIAGNOSTIC_ENTRIES - 1 {
            let other: Uri = Uri::from(format!("file:///clean{i}.rs"));
            cache.store_diagnostics(&server, &other, Some(1), vec![]);
        }
        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);

        let overflow: Uri = Uri::from("file:///overflow.rs");
        cache.store_diagnostics(&server, &overflow, Some(1), vec![]);

        let stored = cache.diagnostics(&uri);
        assert!(
            stored.is_some_and(|info| info.diagnostics.len() == 1),
            "the re-dirtied entry must survive and keep its real diagnostic, not be mistaken \
             for an empty entry"
        );
    }

    /// `set_diagnostics_route_count(0)` must clamp to `1`, not panic via
    /// division by zero in `per_server_budget`.
    #[test]
    fn test_set_diagnostics_route_count_zero_clamps_to_one() {
        let mut cache = NotificationCache::new();
        cache.set_diagnostics_route_count(0);

        for i in 0..MAX_DIAGNOSTIC_ENTRIES + 5 {
            let uri: Uri = Uri::from(format!("file:///file{i}.rs"));
            cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        }

        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);
    }

    // Transition logic is unit-tested in `bridge::indexing`; these tests only cover delegation wiring.

    #[test]
    fn test_indexing_state_defaults_unknown() {
        let cache = NotificationCache::new();
        assert_eq!(cache.indexing_state(&test_server()), IndexingState::Unknown);
    }

    #[test]
    fn test_observe_indexing_signal_delegates_to_tracker() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        cache.observe_indexing_signal(
            &server,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        assert_eq!(cache.indexing_state(&server), IndexingState::Loading);
    }

    #[test]
    fn test_reset_indexing_state_delegates_to_tracker() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        cache.observe_indexing_signal(
            &server,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        cache.reset_indexing_state(&server, IndexingReset::Forget);
        assert_eq!(cache.indexing_state(&server), IndexingState::Unknown);
    }

    fn file_uri(path: &str) -> Uri {
        Uri::from(format!("file:///ws/{path}"))
    }

    fn published(source: &str) -> PublishedDiagnosticsUri {
        PublishedDiagnosticsUri::for_test(file_uri(source), file_uri("main.rs"))
    }

    fn diagnostic_at(line: u32, message: &str) -> LspDiagnostic {
        let mut diagnostic = minimal_diagnostic(message.to_owned());
        diagnostic.range.start.line = line;
        diagnostic.range.end.line = line;
        diagnostic
    }

    fn merged(cache: &NotificationCache) -> Option<DiagnosticInfo> {
        cache.diagnostic_sources(&file_uri("main.rs")).merge()
    }

    fn messages(info: &DiagnosticInfo) -> Vec<String> {
        info.diagnostics
            .iter()
            .map(|d| message_as_str(&d.message).to_owned())
            .collect()
    }

    /// The rust-analyzer probe sequence: errors arrive on the alias path and
    /// an empty list on the canonical one.
    #[test]
    fn test_published_alias_errors_survive_empty_canonical_publish() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        let errors = vec![diagnostic_at(1, "e1"), diagnostic_at(2, "e2")];

        cache.store_published_diagnostics(&server, &published("alias.rs"), None, errors);
        cache.store_published_diagnostics(&server, &published("main.rs"), Some(3), vec![]);
        assert_eq!(messages(&merged(&cache).unwrap()), ["e1", "e2"]);

        cache.store_published_diagnostics(&server, &published("main.rs"), Some(4), vec![]);
        assert_eq!(messages(&merged(&cache).unwrap()), ["e1", "e2"]);

        cache.store_published_diagnostics(&server, &published("alias.rs"), None, vec![]);
        assert!(merged(&cache).unwrap().diagnostics.is_empty());
    }

    #[test]
    fn test_published_empty_publish_clears_only_its_own_source() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        cache.store_published_diagnostics(
            &server,
            &published("a1.rs"),
            None,
            vec![diagnostic_at(1, "e1")],
        );
        cache.store_published_diagnostics(
            &server,
            &published("a2.rs"),
            None,
            vec![diagnostic_at(2, "e2")],
        );
        assert_eq!(messages(&merged(&cache).unwrap()), ["e1", "e2"]);

        cache.store_published_diagnostics(&server, &published("a1.rs"), None, vec![]);

        assert_eq!(messages(&merged(&cache).unwrap()), ["e2"]);
    }

    /// Dedup must not depend on duplicates being adjacent after the sort.
    #[test]
    fn test_merge_removes_duplicates_sharing_a_start_with_another_diagnostic() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        let x = diagnostic_at(1, "x");
        let y = diagnostic_at(1, "y");
        cache.store_published_diagnostics(&server, &published("a1.rs"), None, vec![x.clone(), y]);
        cache.store_published_diagnostics(&server, &published("a2.rs"), None, vec![x]);

        assert_eq!(messages(&merged(&cache).unwrap()), ["x", "y"]);
    }

    #[test]
    fn test_merge_orders_by_range_start() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        cache.store_published_diagnostics(
            &server,
            &published("a1.rs"),
            None,
            vec![diagnostic_at(5, "late")],
        );
        cache.store_published_diagnostics(
            &server,
            &published("a2.rs"),
            None,
            vec![diagnostic_at(1, "early")],
        );

        assert_eq!(messages(&merged(&cache).unwrap()), ["early", "late"]);
    }

    #[test]
    fn test_merged_version_is_the_canonical_sources() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        cache.store_published_diagnostics(
            &server,
            &published("main.rs"),
            Some(7),
            vec![diagnostic_at(1, "a")],
        );
        cache.store_published_diagnostics(
            &server,
            &published("alias.rs"),
            None,
            vec![diagnostic_at(2, "b")],
        );
        assert_eq!(merged(&cache).unwrap().version, Some(7));

        cache.store_published_diagnostics(
            &server,
            &published("main.rs"),
            Some(8),
            vec![diagnostic_at(1, "a")],
        );
        assert_eq!(merged(&cache).unwrap().version, Some(8));
    }

    #[test]
    fn test_merge_is_bounded_by_the_entry_size_cap() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        let big = "x".repeat(200 * 1024);
        for source in [
            "a1.rs", "a2.rs", "a3.rs", "a4.rs", "a5.rs", "a6.rs", "a7.rs", "a8.rs",
        ] {
            let diagnostics = (0..4)
                .map(|n| diagnostic_at(n, &format!("{source}-{n}-{big}")))
                .collect();
            cache.store_published_diagnostics(&server, &published(source), None, diagnostics);
        }

        let info = merged(&cache).unwrap();

        let bytes = serde_json::to_vec(&info.diagnostics).unwrap().len();
        assert!(bytes <= MAX_DIAGNOSTICS_ENTRY_BYTES, "{bytes} bytes");
    }

    #[test]
    fn test_single_source_is_returned_unchanged() {
        let mut cache = NotificationCache::new();
        cache.store_published_diagnostics(
            &test_server(),
            &published("alias.rs"),
            Some(2),
            vec![diagnostic_at(1, "e")],
        );

        let info = merged(&cache).unwrap();

        assert_eq!(info.uri, file_uri("alias.rs"));
        assert_eq!(info.version, Some(2));
    }

    #[test]
    fn test_ninth_source_is_dropped_without_an_orphan_entry() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        for n in 0..MAX_SOURCES_PER_FILE {
            cache.store_published_diagnostics(
                &server,
                &published(&format!("a{n}.rs")),
                None,
                vec![diagnostic_at(1, "e")],
            );
        }

        cache.store_published_diagnostics(
            &server,
            &published("extra.rs"),
            None,
            vec![diagnostic_at(9, "extra")],
        );

        assert_eq!(cache.diagnostics_count(), MAX_SOURCES_PER_FILE);
        assert!(cache.diagnostics(&file_uri("extra.rs")).is_none());
        assert!(!messages(&merged(&cache).unwrap()).contains(&"extra".to_owned()));
    }

    #[test]
    fn test_republishing_an_indexed_source_at_the_cap_is_accepted() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        for n in 0..MAX_SOURCES_PER_FILE {
            cache.store_published_diagnostics(
                &server,
                &published(&format!("a{n}.rs")),
                None,
                vec![],
            );
        }

        cache.store_published_diagnostics(
            &server,
            &published("a0.rs"),
            None,
            vec![diagnostic_at(1, "again")],
        );

        assert_eq!(messages(&merged(&cache).unwrap()), ["again"]);
    }

    #[test]
    fn test_clear_server_diagnostics_prunes_the_source_index() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        cache.store_published_diagnostics(
            &server,
            &published("a1.rs"),
            None,
            vec![diagnostic_at(1, "e")],
        );

        cache.clear_server_diagnostics(&server);

        cache.assert_consistent();
        assert!(cache.files.is_empty());
        assert!(!cache.has_diagnostics(&file_uri("main.rs")));
        assert!(merged(&cache).is_none());
    }

    #[test]
    fn test_eviction_prunes_the_source_index() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        cache.store_published_diagnostics(&server, &published("a1.rs"), None, vec![]);
        for n in 0..MAX_DIAGNOSTIC_ENTRIES {
            let uri = Uri::from(format!("file:///other/{n}.rs"));
            cache.store_diagnostics(&server, &uri, None, vec![]);
        }

        assert!(cache.diagnostics(&file_uri("a1.rs")).is_none());
        cache.assert_consistent();
        assert!(
            !cache
                .files
                .contains_key(&DiagnosticsKey::of(&file_uri("main.rs")))
        );
    }

    #[test]
    fn test_diagnostics_owner_resolves_through_the_source_index() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        cache.store_published_diagnostics(&server, &published("alias.rs"), None, vec![]);

        assert_eq!(cache.diagnostics_owner(&file_uri("main.rs")), Some(&server));
    }

    /// S5: a server fanning one file out over many spellings must not be able
    /// to suppress the canonical publishes.
    #[test]
    fn test_canonical_source_is_admitted_when_aliases_fill_the_cap() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        for n in 0..MAX_SOURCES_PER_FILE {
            cache.store_published_diagnostics(
                &server,
                &published(&format!("a{n}.rs")),
                None,
                vec![diagnostic_at(
                    u32::try_from(n).unwrap(),
                    &format!("alias{n}"),
                )],
            );
        }

        cache.store_published_diagnostics(
            &server,
            &published("main.rs"),
            Some(5),
            vec![diagnostic_at(99, "canonical")],
        );

        let info = merged(&cache).unwrap();
        let shown = messages(&info);
        assert!(shown.contains(&"canonical".to_owned()), "{shown:?}");
        assert!(
            !shown.contains(&"alias0".to_owned()),
            "oldest alias is evicted"
        );
        assert_eq!(shown.len(), MAX_SOURCES_PER_FILE);
        assert_eq!(info.version, Some(5));
        assert_eq!(cache.diagnostics_count(), MAX_SOURCES_PER_FILE);
    }

    #[test]
    fn test_alias_beyond_the_cap_is_still_dropped_when_canonical_is_present() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        cache.store_published_diagnostics(&server, &published("main.rs"), None, vec![]);
        for n in 0..MAX_SOURCES_PER_FILE - 1 {
            cache.store_published_diagnostics(
                &server,
                &published(&format!("a{n}.rs")),
                None,
                vec![],
            );
        }

        cache.store_published_diagnostics(
            &server,
            &published("extra.rs"),
            None,
            vec![diagnostic_at(1, "x")],
        );

        assert!(cache.diagnostics(&file_uri("extra.rs")).is_none());
        assert!(cache.diagnostics(&file_uri("main.rs")).is_some());
    }

    /// The merge must not compare every diagnostic against every other one
    /// sharing its start: 8 sources of 3000 same-start diagnostics each.
    #[test]
    fn test_merge_handles_many_same_start_diagnostics_across_sources() {
        const PER_SOURCE: u32 = 3000;
        let mut cache = NotificationCache::new();
        let server = test_server();
        for source in 0..MAX_SOURCES_PER_FILE {
            let diagnostics = (0..PER_SOURCE)
                .map(|n| diagnostic_at(0, &format!("diagnostic-{n}")))
                .collect();
            cache.store_published_diagnostics(
                &server,
                &published(&format!("a{source}.rs")),
                None,
                diagnostics,
            );
        }

        let info = merged(&cache).unwrap();

        assert_eq!(info.diagnostics.len(), PER_SOURCE as usize);
    }

    /// Diagnostics equal in range and message but differing elsewhere are
    /// distinct and both survive the merge.
    #[test]
    fn test_merge_keeps_diagnostics_that_differ_outside_range_and_message() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        let plain = diagnostic_at(1, "same");
        let mut coded = diagnostic_at(1, "same");
        coded.code = Some(lsp_types::Code::String("E1".to_owned()));
        cache.store_published_diagnostics(&server, &published("a1.rs"), None, vec![plain]);
        cache.store_published_diagnostics(&server, &published("a2.rs"), None, vec![coded]);

        assert_eq!(merged(&cache).unwrap().diagnostics.len(), 2);
    }

    #[test]
    fn test_clear_server_diagnostics_leaves_no_dangling_file_index() {
        let mut cache = NotificationCache::new();
        let (a, b) = (ServerId::from("a"), ServerId::from("b"));
        cache.store_published_diagnostics(&a, &published("a1.rs"), None, vec![]);
        cache.store_published_diagnostics(
            &b,
            &published("main.rs"),
            None,
            vec![diagnostic_at(1, "e")],
        );

        cache.clear_server_diagnostics(&a);

        cache.assert_consistent();
        assert_eq!(cache.files.len(), 1);
        assert!(cache.diagnostics(&file_uri("main.rs")).is_some());
        assert!(cache.diagnostics(&file_uri("a1.rs")).is_none());
    }

    #[derive(Debug, Clone)]
    enum CacheOp {
        Publish {
            server: u8,
            file: u8,
            alias: Option<u8>,
            empty: bool,
        },
        Clear {
            server: u8,
        },
    }

    fn cache_op() -> impl proptest::strategy::Strategy<Value = CacheOp> {
        use proptest::prelude::*;

        prop_oneof![
            4 => (0..3u8, 0..3u8, proptest::option::of(0..3u8), any::<bool>()).prop_map(
                |(server, file, alias, empty)| CacheOp::Publish { server, file, alias, empty }
            ),
            1 => (0..3u8).prop_map(|server| CacheOp::Clear { server }),
        ]
    }

    proptest::proptest! {
        #[test]
        fn cache_indices_stay_consistent_under_random_operations(
            ops in proptest::collection::vec(cache_op(), 0..60)
        ) {
            let mut cache = NotificationCache::new();
            for op in ops {
                match op {
                    CacheOp::Publish { server, file, alias, empty } => {
                        let canonical = file_uri(&format!("f{file}.rs"));
                        let source = alias.map_or_else(
                            || canonical.clone(),
                            |n| file_uri(&format!("alias{n}.rs")),
                        );
                        let diagnostics = if empty { vec![] } else { vec![diagnostic_at(1, "e")] };
                        cache.store_published_diagnostics(
                            &ServerId::from(format!("s{server}")),
                            &PublishedDiagnosticsUri::for_test(source, canonical),
                            None,
                            diagnostics,
                        );
                    }
                    CacheOp::Clear { server } => {
                        cache.clear_server_diagnostics(&ServerId::from(format!("s{server}")));
                    }
                }
                cache.assert_consistent();
            }
        }
    }

    fn first_uri() -> Uri {
        Uri::from("file:///first.rs")
    }

    fn error_diagnostic() -> LspDiagnostic {
        LspDiagnostic {
            severity: Some(lsp_types::DiagnosticSeverity::Error),
            message: "broken".to_owned().into(),
            ..LspDiagnostic::default()
        }
    }

    /// Fills the cache with `MAX_DIAGNOSTIC_ENTRIES` entries: `file:///first.rs`
    /// (empty or not, per `first_empty`) then non-empty fillers.
    fn full_cache(first_empty: bool) -> NotificationCache {
        let mut cache = NotificationCache::new();
        let first: Uri = Uri::from("file:///first.rs");
        let diagnostics = if first_empty {
            vec![]
        } else {
            vec![error_diagnostic()]
        };
        cache.store_diagnostics(&test_server(), &first, None, diagnostics);
        for i in 1..MAX_DIAGNOSTIC_ENTRIES {
            let uri: Uri = Uri::from(format!("file:///filler{i}.rs"));
            cache.store_diagnostics(&test_server(), &uri, None, vec![error_diagnostic()]);
        }
        cache
    }

    fn evict_one(cache: &mut NotificationCache) {
        let overflow: Uri = Uri::from("file:///overflow.rs");
        cache.store_diagnostics(&test_server(), &overflow, None, vec![error_diagnostic()]);
    }

    #[test]
    fn test_evicted_empty_entry_is_listen_replayable() {
        let mut cache = full_cache(true);
        assert!(cache.is_listen_replayable(&first_uri()));
        evict_one(&mut cache);
        assert!(!cache.has_diagnostics(&first_uri()));
        assert!(cache.is_listen_replayable(&first_uri()));
        assert!(!cache.is_listen_replayable(&Uri::from("file:///unrelated.rs")));
    }

    fn write_error(cache: &mut NotificationCache, file: &str) -> PushWrite {
        let uri = Uri::from(file);
        cache.write_published_diagnostics(
            &test_server(),
            &PublishedDiagnosticsUri::for_test(uri.clone(), uri),
            None,
            vec![error_diagnostic()],
        )
    }

    /// #649: a push that needs room names the file whose entry it evicted.
    #[test]
    fn test_a_push_reports_the_file_it_evicts() {
        let mut cache = full_cache(false);
        let write = write_error(&mut cache, "file:///overflow.rs");
        assert_eq!(write.evicted, vec![DiagnosticsKey::of(&first_uri())]);
        assert!(!cache.has_diagnostics(&first_uri()));
    }

    /// #649: replacing a cached entry, or writing below capacity, evicts nothing.
    #[test]
    fn test_a_push_that_needs_no_room_reports_no_eviction() {
        let mut cache = full_cache(false);
        assert!(
            write_error(&mut cache, "file:///first.rs")
                .evicted
                .is_empty()
        );
        let mut roomy = NotificationCache::new();
        assert!(write_error(&mut roomy, "file:///a.rs").evicted.is_empty());
    }

    #[test]
    fn test_evicted_non_empty_entry_is_not_replayable() {
        let mut cache = full_cache(false);
        evict_one(&mut cache);
        assert!(!cache.has_diagnostics(&first_uri()));
        assert!(!cache.is_listen_replayable(&first_uri()));
    }

    #[test]
    fn test_recached_entry_leaves_the_replay_ring_view() {
        let mut cache = full_cache(true);
        evict_one(&mut cache);
        let first: Uri = Uri::from("file:///first.rs");
        cache.store_diagnostics(&test_server(), &first, None, vec![error_diagnostic()]);
        assert!(cache.has_diagnostics(&first_uri()));
        assert!(!cache.was_recently_evicted(&first_uri(), std::time::Instant::now()));
    }

    #[test]
    fn test_eviction_replay_expires_after_the_window() {
        let mut cache = NotificationCache::new();
        let now = std::time::Instant::now();
        let gone = Uri::from("file:///gone.rs");
        cache.record_empty_eviction(DiagnosticsKey::of(&gone), now);
        assert!(cache.was_recently_evicted(&gone, now + EVICTION_REPLAY_WINDOW));
        assert!(!cache.was_recently_evicted(
            &gone,
            now + EVICTION_REPLAY_WINDOW + std::time::Duration::from_secs(1)
        ));
    }

    #[test]
    fn test_eviction_record_is_bounded_and_keeps_the_newest() {
        let mut cache = NotificationCache::new();
        let now = std::time::Instant::now();
        for i in 0..MAX_RECENT_EVICTIONS + 44 {
            let gone = Uri::from(format!("file:///gone{i}.rs"));
            cache.record_empty_eviction(
                DiagnosticsKey::of(&gone),
                now + std::time::Duration::from_millis(u64::try_from(i).unwrap()),
            );
        }
        assert_eq!(cache.recent_evictions.len(), MAX_RECENT_EVICTIONS);
        let at = now + std::time::Duration::from_secs(1);
        assert!(!cache.was_recently_evicted(&Uri::from("file:///gone0.rs"), at));
        assert!(!cache.was_recently_evicted(&Uri::from("file:///gone43.rs"), at));
        assert!(cache.was_recently_evicted(&Uri::from("file:///gone44.rs"), at));
        let newest = MAX_RECENT_EVICTIONS + 43;
        assert!(cache.was_recently_evicted(&Uri::from(format!("file:///gone{newest}.rs")), at));
    }

    #[test]
    fn test_clearing_a_full_cache_keeps_every_empty_clear_replayable() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        let uris: Vec<Uri> = (0..MAX_DIAGNOSTIC_ENTRIES)
            .map(|i| Uri::from(format!("file:///clean{i}.rs")))
            .collect();
        for uri in &uris {
            cache.store_diagnostics(&server, uri, None, vec![]);
        }

        cache.clear_server_diagnostics(&server);

        assert!(uris.iter().all(|uri| cache.is_listen_replayable(uri)));
    }

    #[test]
    fn test_eviction_order_stays_within_twice_the_record_under_churn() {
        let mut cache = NotificationCache::new();
        let now = std::time::Instant::now();
        let keys: Vec<DiagnosticsKey> = (0..3)
            .map(|i| DiagnosticsKey::of(&Uri::from(format!("file:///hot{i}.rs"))))
            .collect();
        for tick in 0..10_000u64 {
            let key = keys[usize::try_from(tick % 3).unwrap()].clone();
            cache.record_empty_eviction(key, now + std::time::Duration::from_micros(tick));
            let record = &cache.recent_evictions;
            assert!(record.at.len() <= MAX_RECENT_EVICTIONS);
            assert!(record.order.len() <= 2 * record.at.len());
        }
        assert_eq!(cache.recent_evictions.len(), 3);
    }

    #[test]
    fn test_eviction_record_stays_bounded_under_churn_past_the_cap() {
        const KEYS: usize = MAX_RECENT_EVICTIONS + 300;
        let mut cache = NotificationCache::new();
        let now = std::time::Instant::now();
        let keys: Vec<DiagnosticsKey> = (0..KEYS)
            .map(|i| DiagnosticsKey::of(&Uri::from(format!("file:///churn{i}.rs"))))
            .collect();
        for tick in 0..KEYS * 8 {
            let key = keys[tick % KEYS].clone();
            let at = now + std::time::Duration::from_micros(u64::try_from(tick).unwrap());
            cache.record_empty_eviction(key, at);
            let record = &cache.recent_evictions;
            assert!(record.at.len() <= MAX_RECENT_EVICTIONS);
            assert!(record.order.len() <= 2 * record.at.len() + 1);
        }
        assert_eq!(cache.recent_evictions.len(), MAX_RECENT_EVICTIONS);
    }

    #[test]
    fn test_compaction_keeps_the_oldest_first_expiry_order() {
        let mut record = EvictionRecord::default();
        let now = std::time::Instant::now();
        let a = DiagnosticsKey::of(&Uri::from("file:///a.rs"));
        let b = DiagnosticsKey::of(&Uri::from("file:///b.rs"));
        record.record(a.clone(), now);
        record.record(b.clone(), now + std::time::Duration::from_secs(1));
        record.record(a.clone(), now + std::time::Duration::from_secs(2));
        record.record(b.clone(), now + std::time::Duration::from_secs(3));
        record.compact();
        let order: Vec<_> = record.order.iter().map(|(key, _)| key.clone()).collect();
        assert_eq!(order, vec![a, b]);
    }

    #[test]
    fn test_eviction_record_overflow_warns_once_per_window() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let mut record = EvictionRecord::default();
        let now = std::time::Instant::now();
        let captured = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        for i in 0..MAX_RECENT_EVICTIONS + 10 {
            let key = DiagnosticsKey::of(&Uri::from(format!("file:///gone{i}.rs")));
            record.record(
                key,
                now + std::time::Duration::from_millis(u64::try_from(i).unwrap()),
            );
        }
        drop(guard);

        let warnings = captured
            .messages()
            .iter()
            .filter(|m| m.contains("diagnostics clears evicted"))
            .count();
        assert_eq!(warnings, 1);
    }

    #[test]
    fn test_eviction_record_deduplicates_and_prunes_expired_entries() {
        let mut cache = NotificationCache::new();
        let now = std::time::Instant::now();
        let a = DiagnosticsKey::of(&Uri::from("file:///a.rs"));
        cache.record_empty_eviction(a.clone(), now);
        cache.record_empty_eviction(a, now);
        assert_eq!(cache.recent_evictions.len(), 1);
        let later = now + EVICTION_REPLAY_WINDOW + std::time::Duration::from_secs(1);
        cache.record_empty_eviction(DiagnosticsKey::of(&Uri::from("file:///b.rs")), later);
        assert_eq!(cache.recent_evictions.len(), 1);
    }

    #[test]
    fn test_alias_eviction_of_an_empty_entry_is_recorded() {
        let mut cache = NotificationCache::new();
        let server = test_server();
        for n in 0..MAX_SOURCES_PER_FILE {
            cache.store_published_diagnostics(
                &server,
                &published(&format!("a{n}.rs")),
                None,
                vec![],
            );
        }
        assert!(cache.recent_evictions.is_empty());

        cache.store_published_diagnostics(
            &server,
            &PublishedDiagnosticsUri::for_test(file_uri("main.rs"), file_uri("main.rs")),
            None,
            vec![diagnostic_at(1, "e")],
        );

        assert!(cache.diagnostics(&file_uri("a0.rs")).is_none());
        assert_eq!(cache.recent_evictions.len(), 1);
        assert!(cache.is_listen_replayable(&file_uri("main.rs")));
    }

    #[test]
    fn test_clearing_a_server_records_its_empty_entries() {
        let mut cache = NotificationCache::new();
        let empty = file_uri("empty.rs");
        let broken = file_uri("broken.rs");
        cache.store_diagnostics(&test_server(), &empty, None, vec![]);
        cache.store_diagnostics(&test_server(), &broken, None, vec![error_diagnostic()]);

        let cleared = cache.clear_server_diagnostics(&test_server());

        assert_eq!(cleared.len(), 2);
        assert!(cache.is_listen_replayable(&empty));
        assert!(!cache.is_listen_replayable(&broken));
    }
}
