//! LSP notification storage and management.
//!
//! Stores diagnostics, log messages, and server messages received from LSP servers.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};
use lsp_types::{Diagnostic as LspDiagnostic, Uri};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::ServerId;
use crate::util::{truncate_str, truncate_string};

/// Maximum number of log entries to store.
const MAX_LOG_ENTRIES: usize = 100;

/// Maximum size, in bytes, of a single cached log message, server message,
/// or a single diagnostic's free-form `message` text.
///
/// `MAX_LOG_ENTRIES`/`MAX_SERVER_MESSAGES`/`MAX_DIAGNOSTIC_ENTRIES` bound
/// the *number* of cached entries, but not the size of any one entry -- a
/// spawned LSP server could publish a single pathologically large message
/// and still fit under those caps while consuming unbounded memory (#311).
/// This is independent of the transport-level `MAX_CONTENT_LENGTH` cap in
/// `lsp::transport`, which bounds a whole JSON-RPC frame, not one field
/// within it. 256 KiB comfortably fits any realistic diagnostic or log
/// message while still capping the worst case.
///
/// This alone does not bound a whole diagnostics *entry* (a
/// `Vec<LspDiagnostic>`), only one diagnostic's `message` field -- see
/// `MAX_DIAGNOSTICS_ENTRY_BYTES` for the entry-level cap.
const MAX_ENTRY_TEXT_BYTES: usize = 256 * 1024;

/// Maximum serialized size, in bytes, of a single document's *whole*
/// diagnostics list (`Vec<LspDiagnostic>`), enforced by
/// [`cap_diagnostics_entry_size`].
///
/// `MAX_ENTRY_TEXT_BYTES` alone does not bound this: it only truncates one
/// diagnostic's `message` field, but the list's *length* is uncapped, and
/// `LspDiagnostic` carries several more free-form or arbitrary-JSON fields
/// besides `message` (`source`, `code`, `code_description`,
/// `related_information`, `data`). A hostile server can stay under
/// `MAX_ENTRY_TEXT_BYTES` on every individual message while still
/// publishing e.g. 100k diagnostics for one URI, or a single diagnostic
/// with a multi-MiB `data` blob -- both still fit under the transport-level
/// `lsp::transport::MAX_CONTENT_LENGTH` (10 MiB) per notification, and
/// `MAX_DIAGNOSTIC_ENTRIES` bounds only the *number* of distinct cached
/// URIs, not their individual size, so up to 1000 such entries could
/// otherwise accumulate to gigabytes. 1 MiB is far larger than any
/// realistic diagnostics list for one file, and combined with
/// `MAX_DIAGNOSTIC_ENTRIES` bounds the cache's total diagnostics footprint
/// to roughly 1 GiB in the worst case.
const MAX_DIAGNOSTICS_ENTRY_BYTES: usize = 1024 * 1024;

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
/// of it they actually use.
const MAX_DIAGNOSTIC_ENTRIES: usize = 1000;

/// Normalize a URI string to a stable cache key.
///
/// On Windows, URI comparisons must be case-insensitive: the filesystem is
/// case-insensitive and different tools (e.g. rust-analyzer vs std) may
/// produce drive letters in different cases (`C:` vs `c:`).
/// Lowercasing the entire URI is safe for `file://` URIs because they have
/// no case-sensitive query or fragment components.
fn uri_cache_key(uri: &str) -> std::borrow::Cow<'_, str> {
    if cfg!(windows) {
        std::borrow::Cow::Owned(uri.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(uri)
    }
}

/// Maximum number of server messages to store.
const MAX_SERVER_MESSAGES: usize = 50;

/// Conservative fixed-field/JSON-structure overhead assumed per diagnostic
/// (`range`, `severity`, and object/field-name punctuation) by
/// [`cap_diagnostics_entry_size`]'s cheap size estimate. Deliberately
/// generous relative to the true overhead (`range` alone serializes to
/// roughly 70 bytes) so the estimate can only ever *overcount*, never
/// undercount, actual serialized size.
const DIAGNOSTIC_ESTIMATE_OVERHEAD_BYTES: usize = 256;

/// Worst-case JSON string-escaping expansion factor, applied to each raw
/// string field's byte length in [`cap_diagnostics_entry_size`]'s cheap
/// size estimate.
///
/// A raw byte's serialized JSON form is at most 6 bytes: `"` and `\` and
/// the five control characters with a short escape (`\b \f \n \r \t`) cost
/// 2 bytes, but every other control character (`U+0000`..=`U+001F`, e.g.
/// NUL) has no short escape and is emitted as `\u00XX` -- 6 bytes for 1 raw
/// byte. The original estimate summed raw string lengths directly and
/// could *undercount* an escape-heavy string (e.g. all-NUL) by up to this
/// factor, letting an oversized entry skip the real `fits` check
/// entirely -- multiplying by it keeps the estimate a true upper bound on
/// serialized size rather than merely a typical-case guess.
const JSON_ESCAPE_WORST_CASE_FACTOR: usize = 6;

/// Last-resort message length used by [`cap_diagnostics_entry_size`]'s
/// terminal-enforcement fallback -- small enough that a single diagnostic
/// (fixed-size `range`/`severity` plus this one short string, every other
/// field cleared) can never approach [`MAX_DIAGNOSTICS_ENTRY_BYTES`]
/// regardless of JSON encoding overhead.
const DIAGNOSTIC_TERMINAL_FALLBACK_MESSAGE_BYTES: usize = 1024;

/// Ordinal rank used to sort diagnostics by severity before
/// [`cap_diagnostics_entry_size`] truncates an oversized list -- lower rank
/// sorts first, so it is kept preferentially (#311 S6).
///
/// `DiagnosticSeverity`'s inner value is private, so its natural numeric
/// ordering (`ERROR` < `WARNING` < `INFORMATION` < `HINT`) can't be read
/// directly; `Option<DiagnosticSeverity>`'s *derived* `Ord` would also rank
/// `None` before every `Some` value, the opposite of what's wanted here
/// (no reported severity is treated as least important, same as `HINT`).
/// This maps explicitly instead of relying on either.
const fn diagnostic_severity_rank(diagnostic: &LspDiagnostic) -> u8 {
    match diagnostic.severity {
        Some(lsp_types::DiagnosticSeverity::ERROR) => 0,
        Some(lsp_types::DiagnosticSeverity::WARNING) => 1,
        Some(lsp_types::DiagnosticSeverity::INFORMATION) => 2,
        // An unrecognized (future) severity value is treated the same as
        // no severity at all: least important, not most.
        Some(_) | None => 3,
    }
}

/// Largest `k` such that `fits(&diagnostics[..k])`, found via binary search
/// rather than a linear scan or a flat halve (#311 S6).
///
/// Correct because a JSON array's serialized length is monotonically
/// non-decreasing in its element count -- appending a diagnostic can only
/// add bytes, never remove them -- so `fits(&diagnostics[..k])` is `true`
/// for a contiguous run of small `k` and `false` for every larger `k`,
/// exactly the shape a boundary binary search requires. `fits(&[])` is
/// always `true`, so the search is well-defined even if no diagnostic at
/// all fits individually.
fn largest_fitting_prefix(
    diagnostics: &[LspDiagnostic],
    fits: impl Fn(&[LspDiagnostic]) -> bool,
) -> usize {
    let (mut lo, mut hi) = (0usize, diagnostics.len());
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(&diagnostics[..mid]) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// Bounds `diagnostics`' serialized size to at most
/// `MAX_DIAGNOSTICS_ENTRY_BYTES` (#311 C1 fix).
///
/// Measures the list's *actual* serialized size via `serde_json::to_vec`
/// rather than bounding each field individually -- that covers every
/// field on `LspDiagnostic` (`source`, `code`, `code_description`,
/// `related_information`, `data`, `tags`) at once, not just `message`.
///
/// # Guarantee
///
/// The postcondition -- the returned list's serialized size is at most
/// `MAX_DIAGNOSTICS_ENTRY_BYTES` -- is enforced directly by a final,
/// unconditional check at the end of this function, not merely assumed to
/// follow from the field-specific mitigations below it. Those mitigations
/// are best-effort (preserve as much real content as fits) and only cover
/// the fields known today; the terminal step is what actually guarantees
/// the bound holds even if a mitigation is incomplete or `LspDiagnostic`
/// gains a new unbounded field in a future `lsp-types` upgrade.
///
/// # Cost (#311 S5)
///
/// `publishDiagnostics` is a hot path (rust-analyzer republishes
/// whole-workspace diagnostics on every save), so this avoids a full
/// `serde_json` serialization pass whenever every diagnostic's size is
/// cheaply accountable from `message`/`source`/`code` alone (i.e. none
/// carry `data`, `code_description`, `related_information`, or `tags`,
/// each of which needs real serialization to size safely) and a
/// conservative *upper bound* on their sum already fits. The estimate is
/// not their raw byte length: JSON string escaping can expand a byte up to
/// [`JSON_ESCAPE_WORST_CASE_FACTOR`]-fold (a NUL-heavy string previously
/// let this fast path undercount actual serialized size by that much and
/// skip the real `fits` check below entirely), so raw lengths are
/// multiplied by that factor before comparing against the cap.
///
/// # Visibility (#311 S7)
///
/// Every mitigation that drops or truncates real content -- discarding
/// diagnostics entirely, or clearing a survivor's `data` (which the LSP
/// spec says is preserved through to a later `textDocument/codeAction`
/// request, so losing it can silently break that diagnostic's quick fix)
/// -- logs a `tracing::warn!` so the degradation is visible rather than a
/// silent, hard-to-diagnose gap in what a caller sees.
fn cap_diagnostics_entry_size(uri: &Uri, diagnostics: &mut Vec<LspDiagnostic>) {
    let fits = |ds: &[LspDiagnostic]| {
        // A serialization error is conservatively treated as "does not
        // fit" (triggers the mitigations below) rather than as success.
        // `LspDiagnostic`'s fields can't actually produce one in practice
        // (no floats, no non-string map keys anywhere in `Diagnostic` or
        // `serde_json::Value`'s own object representation), but failing
        // safe costs nothing here.
        serde_json::to_vec(ds).is_ok_and(|bytes| bytes.len() <= MAX_DIAGNOSTICS_ENTRY_BYTES)
    };

    let cheaply_estimable = diagnostics.iter().all(|d| {
        d.data.is_none()
            && d.code_description.is_none()
            && d.related_information.is_none()
            && d.tags.is_none()
    });
    if cheaply_estimable {
        let estimated: usize = diagnostics
            .iter()
            .map(|d| {
                let raw_string_bytes = d.message.len()
                    + d.source.as_deref().map_or(0, str::len)
                    + match &d.code {
                        Some(lsp_types::NumberOrString::String(s)) => s.len(),
                        _ => 0,
                    };
                raw_string_bytes * JSON_ESCAPE_WORST_CASE_FACTOR
                    + DIAGNOSTIC_ESTIMATE_OVERHEAD_BYTES
            })
            .sum();
        if estimated <= MAX_DIAGNOSTICS_ENTRY_BYTES {
            return;
        }
    }

    if fits(diagnostics) {
        return;
    }

    let original_count = diagnostics.len();

    // Prefer dropping lower-severity diagnostics first (a stable sort, so
    // same-severity diagnostics keep their original -- typically
    // file-position -- relative order), then keep the largest prefix that
    // actually fits rather than a flat halve, which both overshoots (a
    // list one byte over the cap would otherwise lose half its
    // diagnostics) and was severity-blind (would keep hundreds of leading
    // HINT-level noise over a later ERROR). At least one diagnostic is
    // always kept here so the mitigations below have a survivor to act on.
    diagnostics.sort_by_key(diagnostic_severity_rank);
    let keep = largest_fitting_prefix(diagnostics, fits).max(1);
    diagnostics.truncate(keep);
    if diagnostics.len() < original_count {
        warn!(
            "diagnostics for {} exceeded the {MAX_DIAGNOSTICS_ENTRY_BYTES}-byte cache cap; kept \
             the {} highest-severity of {original_count} diagnostics",
            uri.as_str(),
            diagnostics.len(),
        );
    }

    // Drop opaque/structured fields first -- cheap, and often enough on
    // its own (e.g. the single-huge-`data`-blob shape).
    if diagnostics.len() == 1 && !fits(diagnostics) {
        let diagnostic = &mut diagnostics[0];
        let had_data = diagnostic.data.is_some();
        diagnostic.data = None;
        diagnostic.code_description = None;
        diagnostic.related_information = None;
        diagnostic.tags = None;
        warn!(
            "diagnostic for {} exceeded the cache cap; dropped its data/code_description/\
             related_information/tags fields{}",
            uri.as_str(),
            if had_data {
                " (a later code-action request for this diagnostic may not resolve its quick fix)"
            } else {
                ""
            },
        );
    }

    // Still oversized: `source`/`code` (plain strings, unlike the opaque
    // fields above) are truncated rather than dropped, to preserve some
    // content.
    if diagnostics.len() == 1 && !fits(diagnostics) {
        let diagnostic = &mut diagnostics[0];
        if let Some(source) = &diagnostic.source {
            diagnostic.source = Some(truncate_str(source, MAX_ENTRY_TEXT_BYTES));
        }
        if let Some(lsp_types::NumberOrString::String(code)) = &diagnostic.code {
            diagnostic.code = Some(lsp_types::NumberOrString::String(truncate_str(
                code,
                MAX_ENTRY_TEXT_BYTES,
            )));
        }
    }

    // Terminal enforcement: guarantee the postcondition directly rather
    // than trusting the mitigations above to have covered every case --
    // see this function's doc.
    if !fits(diagnostics) {
        diagnostics.truncate(1);
        if let Some(diagnostic) = diagnostics.first_mut() {
            diagnostic.message = truncate_str(
                &diagnostic.message,
                DIAGNOSTIC_TERMINAL_FALLBACK_MESSAGE_BYTES,
            );
            diagnostic.source = None;
            diagnostic.code = None;
            diagnostic.code_description = None;
            diagnostic.related_information = None;
            diagnostic.tags = None;
            diagnostic.data = None;
        }
        warn!(
            "diagnostic for {} still exceeded the cache cap after every other mitigation; \
             truncated its message to {DIAGNOSTIC_TERMINAL_FALLBACK_MESSAGE_BYTES} bytes and \
             cleared all other fields",
            uri.as_str(),
        );
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

/// A log entry from the LSP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Log level.
    pub level: LogLevel,
    /// Log message.
    pub message: String,
    /// Timestamp when the log was received.
    pub timestamp: DateTime<Utc>,
}

/// Log severity level.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
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

impl From<lsp_types::MessageType> for LogLevel {
    fn from(msg_type: lsp_types::MessageType) -> Self {
        match msg_type {
            lsp_types::MessageType::ERROR => Self::Error,
            lsp_types::MessageType::WARNING => Self::Warning,
            lsp_types::MessageType::INFO => Self::Info,
            // LOG and unknown message types default to Debug
            _ => Self::Debug,
        }
    }
}

/// A message from the LSP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMessage {
    /// Message type.
    pub message_type: MessageType,
    /// Message content.
    pub message: String,
    /// Timestamp when the message was received.
    pub timestamp: DateTime<Utc>,
}

/// Server message type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
            lsp_types::MessageType::ERROR => Self::Error,
            lsp_types::MessageType::WARNING => Self::Warning,
            lsp_types::MessageType::INFO => Self::Info,
            // LOG and unknown message types default to Log
            _ => Self::Log,
        }
    }
}

/// Cache for LSP server notifications.
#[derive(Debug)]
pub struct NotificationCache {
    /// Diagnostics indexed by document URI.
    diagnostics: HashMap<String, DiagnosticInfo>,
    /// Server that currently owns each cached URI, so an entry's order map
    /// can be found without scanning every server's.
    diagnostics_owners: HashMap<String, ServerId>,
    /// Per-server `diagnostics` keys ordered oldest-write-first, keyed by a
    /// monotonic sequence number rather than position: a re-publish removes
    /// its old entry by key in `O(log n)` (via `diagnostic_seq`) instead of
    /// scanning for it, which a plain `VecDeque` would require. Not
    /// independently capped per server -- only the aggregate across all
    /// servers is bounded, by `MAX_DIAGNOSTIC_ENTRIES` -- but each server's
    /// own map length is what eviction compares against its fair share (see
    /// [`NotificationCache::server_to_evict_from`]) to decide which server
    /// loses an entry once the aggregate is full, so one server's write
    /// volume can never evict another's entries while it still has room
    /// left in the global budget (#266, #276). Kept in sync with
    /// `diagnostics` by every method that adds or removes an entry.
    diagnostic_order: HashMap<ServerId, BTreeMap<u64, String>>,
    /// Maps each cached URI to its current sequence number in its owner's
    /// `diagnostic_order` map, so a re-publish or clear can find and remove
    /// its old order entry without scanning.
    diagnostic_seq: HashMap<String, u64>,
    /// Next sequence number to assign in `diagnostic_order`. Shared across
    /// every server's order map and monotonically increasing for the
    /// cache's lifetime; never reused, so it never collides with an older
    /// entry still pending eviction.
    next_diagnostic_seq: u64,
    /// Number of registered diagnostics-route servers currently sharing the
    /// `MAX_DIAGNOSTIC_ENTRIES` budget; see
    /// [`NotificationCache::set_diagnostics_route_count`].
    diagnostics_route_count: usize,
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
            diagnostics: HashMap::with_capacity(32),
            diagnostics_owners: HashMap::with_capacity(32),
            diagnostic_order: HashMap::new(),
            diagnostic_seq: HashMap::with_capacity(32),
            next_diagnostic_seq: 0,
            diagnostics_route_count: 1,
            logs: VecDeque::with_capacity(MAX_LOG_ENTRIES),
            messages: VecDeque::with_capacity(MAX_SERVER_MESSAGES),
            push_degraded: HashSet::new(),
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
    /// flowing. Defaults to `1` if never called (a single implicit server
    /// owns the whole budget).
    pub fn set_diagnostics_route_count(&mut self, count: usize) {
        self.diagnostics_route_count = count.max(1);
    }

    /// Current per-server fair share of `MAX_DIAGNOSTIC_ENTRIES`, divided
    /// evenly across `diagnostics_route_count` servers and floored at 1 so a
    /// large server count can never reduce a server's share to zero.
    ///
    /// This is a tie-breaker for eviction, not a hard per-server cap: a
    /// server may hold more than its fair share of entries at any time, as
    /// long as the aggregate across all servers stays within
    /// `MAX_DIAGNOSTIC_ENTRIES` (#276).
    fn per_server_budget(&self) -> usize {
        (MAX_DIAGNOSTIC_ENTRIES / self.diagnostics_route_count.max(1)).max(1)
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
    /// Every candidate here is a distinct `diagnostic_order` key, so pairing
    /// the count with `id.as_str()` makes the sort key unique per server --
    /// no two entries can ever tie on the full key, which eliminates the
    /// non-determinism outright rather than just picking a fixed side of it.
    fn server_to_evict_from(&self, writer: &ServerId) -> Option<ServerId> {
        let largest = self
            .diagnostic_order
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
            .diagnostic_order
            .get(writer)
            .is_some_and(|order| !order.is_empty())
        {
            return Some(writer.clone());
        }

        largest.map(|(id, _)| id.clone())
    }

    /// Store diagnostics for a document published by `server_id`.
    ///
    /// Each diagnostic's `message` is truncated to `MAX_ENTRY_TEXT_BYTES`,
    /// and the whole list is bounded to `MAX_DIAGNOSTICS_ENTRY_BYTES`
    /// serialized bytes, before storing (#311). When that bound requires
    /// dropping diagnostics, the *survivors* come back sorted by severity
    /// (`diagnostic_severity_rank`: `ERROR` first), not in the original
    /// publish/file-position order -- see [`Self::get_diagnostics`].
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
    /// they actually use.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::bridge::NotificationCache;
    /// use mcpls_core::config::ServerId;
    /// use lsp_types::Uri;
    ///
    /// let mut cache = NotificationCache::new();
    /// let server: ServerId = "rust-analyzer".into();
    /// let uri: Uri = "file:///main.rs".parse().unwrap();
    /// cache.store_diagnostics(&server, &uri, Some(1), vec![]);
    /// assert!(cache.get_diagnostics(uri.as_str()).is_some());
    /// ```
    pub fn store_diagnostics(
        &mut self,
        server_id: &ServerId,
        uri: &Uri,
        version: Option<i32>,
        mut diagnostics: Vec<LspDiagnostic>,
    ) {
        // Bound each diagnostic's free-form message text (#311); see
        // `MAX_ENTRY_TEXT_BYTES`. `mem::take` + `truncate_string` avoids an
        // extra clone on the common (already-under-limit) path, since
        // `message` is already an owned `String` here.
        for diagnostic in &mut diagnostics {
            diagnostic.message = truncate_string(
                std::mem::take(&mut diagnostic.message),
                MAX_ENTRY_TEXT_BYTES,
            );
        }
        // Bound the whole list's serialized size (#311 C1); see
        // `MAX_DIAGNOSTICS_ENTRY_BYTES`.
        cap_diagnostics_entry_size(uri, &mut diagnostics);

        let key = uri_cache_key(uri.as_str()).into_owned();
        let info = DiagnosticInfo {
            uri: uri.clone(),
            version,
            diagnostics,
        };

        // Remove the URI's existing order entry, if any -- from its
        // previous owner's order map, whether that's this same server (a
        // republish, repositioned to the back below) or a different one
        // (the diagnostics route changed, e.g. on respawn). Also tells us
        // whether this store adds a new entry to the aggregate (and so may
        // need to evict to stay within budget) or merely replaces one.
        let mut is_new_entry = true;
        if let Some(old_seq) = self.diagnostic_seq.remove(&key) {
            is_new_entry = false;
            if let Some(previous_owner) = self.diagnostics_owners.get(&key)
                && let Some(order) = self.diagnostic_order.get_mut(previous_owner)
            {
                order.remove(&old_seq);
            }
        }

        if is_new_entry {
            while self.diagnostics.len() >= MAX_DIAGNOSTIC_ENTRIES
                && let Some(evict_from) = self.server_to_evict_from(server_id)
                && let Some(order) = self.diagnostic_order.get_mut(&evict_from)
                && let Some((&oldest_seq, oldest_key)) = order.iter().next()
            {
                let oldest_key = oldest_key.clone();
                order.remove(&oldest_seq);
                self.diagnostic_seq.remove(&oldest_key);
                self.diagnostics_owners.remove(&oldest_key);
                self.diagnostics.remove(&oldest_key);
            }
        }

        self.diagnostics_owners
            .insert(key.clone(), server_id.clone());
        let seq = self.next_diagnostic_seq;
        self.next_diagnostic_seq += 1;
        self.diagnostic_order
            .entry(server_id.clone())
            .or_default()
            .insert(seq, key.clone());
        self.diagnostic_seq.insert(key.clone(), seq);
        self.diagnostics.insert(key, info);
    }

    /// Store a log entry.
    ///
    /// Maintains a maximum of `MAX_LOG_ENTRIES` entries, removing oldest when full.
    /// `message` is truncated to `MAX_ENTRY_TEXT_BYTES` before storing.
    pub fn store_log(&mut self, level: LogLevel, message: String) {
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
    pub fn store_message(&mut self, message_type: MessageType, message: String) {
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

    /// Get diagnostics for a document URI.
    ///
    /// If the stored list was ever truncated by `store_diagnostics`'s
    /// `MAX_DIAGNOSTICS_ENTRY_BYTES` cap (#311), the diagnostics here are in
    /// severity order (`ERROR` first), not the original publish/file-position
    /// order -- callers that assume file-position order should not rely on
    /// it after a cap-triggered truncation.
    #[inline]
    #[must_use]
    pub fn get_diagnostics(&self, uri: &str) -> Option<&DiagnosticInfo> {
        self.diagnostics.get(uri_cache_key(uri).as_ref())
    }

    /// Server that published the currently cached diagnostics for `uri`, if
    /// any. Used to look up that server's negotiated position encoding for a
    /// cache-only read that has no live LSP round trip of its own to resolve
    /// one from.
    #[inline]
    #[must_use]
    pub fn diagnostics_owner(&self, uri: &str) -> Option<&ServerId> {
        self.diagnostics_owners.get(uri_cache_key(uri).as_ref())
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

    /// Clear diagnostics for a specific document URI.
    ///
    /// Returns the cleared diagnostics if they existed.
    pub fn clear_diagnostics(&mut self, uri: &str) -> Option<DiagnosticInfo> {
        let key = uri_cache_key(uri).into_owned();
        if let Some(owner) = self.diagnostics_owners.remove(&key)
            && let Some(seq) = self.diagnostic_seq.remove(&key)
            && let Some(order) = self.diagnostic_order.get_mut(&owner)
        {
            order.remove(&seq);
        }
        self.diagnostics.remove(&key)
    }

    /// Clear all diagnostics owned by a single server.
    ///
    /// Used when a server crashes and respawns: its own stale entries must
    /// be invalidated without disturbing any other server's cache entries
    /// (#266), unlike [`Self::clear_all_diagnostics`].
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::bridge::NotificationCache;
    /// use mcpls_core::config::ServerId;
    /// use lsp_types::Uri;
    ///
    /// let mut cache = NotificationCache::new();
    /// let crashed: ServerId = "pyright".into();
    /// let healthy: ServerId = "rust-analyzer".into();
    /// let crashed_uri: Uri = "file:///main.py".parse().unwrap();
    /// let healthy_uri: Uri = "file:///main.rs".parse().unwrap();
    /// cache.store_diagnostics(&crashed, &crashed_uri, Some(1), vec![]);
    /// cache.store_diagnostics(&healthy, &healthy_uri, Some(1), vec![]);
    ///
    /// cache.clear_server_diagnostics(&crashed);
    ///
    /// assert!(cache.get_diagnostics(crashed_uri.as_str()).is_none());
    /// assert!(cache.get_diagnostics(healthy_uri.as_str()).is_some());
    /// ```
    pub fn clear_server_diagnostics(&mut self, server_id: &ServerId) {
        let Some(order) = self.diagnostic_order.remove(server_id) else {
            return;
        };
        for (_, key) in order {
            self.diagnostics.remove(&key);
            self.diagnostics_owners.remove(&key);
            self.diagnostic_seq.remove(&key);
        }
    }

    /// Marks `server_id`'s push-based diagnostics as no longer live -- see
    /// the `push_degraded` field doc for why this is permanent for the life
    /// of the cache.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::bridge::NotificationCache;
    /// use mcpls_core::config::ServerId;
    ///
    /// let mut cache = NotificationCache::new();
    /// let id: ServerId = "rust-analyzer".into();
    /// assert!(!cache.is_push_degraded(&id));
    /// cache.mark_push_degraded(&id);
    /// assert!(cache.is_push_degraded(&id));
    /// ```
    pub fn mark_push_degraded(&mut self, server_id: &ServerId) {
        self.push_degraded.insert(server_id.clone());
    }

    /// Whether `server_id`'s push-based diagnostics are known to be
    /// degraded (see [`Self::mark_push_degraded`]) -- callers such as
    /// `get_cached_diagnostics` and `read_resource` use this to flag a
    /// cache-only result as potentially stale rather than presenting it as
    /// current.
    #[inline]
    #[must_use]
    pub fn is_push_degraded(&self, server_id: &ServerId) -> bool {
        self.push_degraded.contains(server_id)
    }

    /// Clear all diagnostics, for every server.
    pub fn clear_all_diagnostics(&mut self) {
        self.diagnostics.clear();
        self.diagnostics_owners.clear();
        self.diagnostic_order.clear();
        self.diagnostic_seq.clear();
    }

    /// Clear all logs.
    pub fn clear_logs(&mut self) {
        self.logs.clear();
    }

    /// Clear all messages.
    pub fn clear_messages(&mut self) {
        self.messages.clear();
    }

    /// Get the number of documents with stored diagnostics.
    #[inline]
    #[must_use]
    pub fn diagnostics_count(&self) -> usize {
        self.diagnostics.len()
    }

    /// Get the number of stored log entries.
    #[inline]
    #[must_use]
    pub fn logs_count(&self) -> usize {
        self.logs.len()
    }

    /// Get the number of stored server messages.
    #[inline]
    #[must_use]
    pub fn messages_count(&self) -> usize {
        self.messages.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use lsp_types::{Position, Range};

    use super::*;

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
    fn test_store_and_get_diagnostics() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

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
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: None,
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);

        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.uri, uri);
        assert_eq!(stored.version, Some(1));
        assert_eq!(stored.diagnostics.len(), 1);
        assert_eq!(stored.diagnostics[0].message, "test error");
    }

    /// #311: a single diagnostic's `message` must be bounded independently
    /// of `MAX_DIAGNOSTIC_ENTRIES`, which only caps the number of entries.
    #[test]
    fn test_store_diagnostics_truncates_oversized_message() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();
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
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: oversized.clone(),
            code: None,
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);

        let stored = &cache.get_diagnostics(uri.as_str()).unwrap().diagnostics[0].message;
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
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message,
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
        let uri: Uri = "file:///test.rs".parse().unwrap();

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

        let stored = &cache.get_diagnostics(uri.as_str()).unwrap().diagnostics;
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
        let uri: Uri = "file:///test.rs".parse().unwrap();

        // Each diagnostic serializes to roughly 300 bytes; ~3800 of them
        // fit under the 1 MiB cap, well over half of the 5000 published --
        // a flat halve would incorrectly stop at 2500.
        let diagnostics: Vec<LspDiagnostic> = (0..5000)
            .map(|i| minimal_diagnostic(format!("diagnostic {i}: {}", "x".repeat(250))))
            .collect();

        cache.store_diagnostics(&test_server(), &uri, Some(1), diagnostics);

        let stored = &cache.get_diagnostics(uri.as_str()).unwrap().diagnostics;
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
        let uri: Uri = "file:///test.rs".parse().unwrap();

        let mut diagnostics: Vec<LspDiagnostic> = (0..5000)
            .map(|i| {
                let mut d = minimal_diagnostic(format!("hint {i}: {}", "x".repeat(200)));
                d.severity = Some(lsp_types::DiagnosticSeverity::HINT);
                d
            })
            .collect();
        let mut trailing_error = minimal_diagnostic("the one real error".to_string());
        trailing_error.severity = Some(lsp_types::DiagnosticSeverity::ERROR);
        diagnostics.push(trailing_error);

        cache.store_diagnostics(&test_server(), &uri, Some(1), diagnostics);

        let stored = &cache.get_diagnostics(uri.as_str()).unwrap().diagnostics;
        assert!(
            stored.iter().any(|d| d.message == "the one real error"),
            "the trailing ERROR diagnostic must survive truncation over leading HINT noise"
        );
    }

    /// Captures `tracing` events emitted while a closure runs, mirroring
    /// `transport::tests::http_tests::CapturedMessages` -- there is no
    /// shared `tracing_test`-style helper in this codebase to reuse.
    #[derive(Clone, Default)]
    struct CapturedMessages(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedMessages {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct MessageVisitor(String);
            impl tracing::field::Visit for MessageVisitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" {
                        self.0 = format!("{value:?}");
                    }
                }
            }
            let mut visitor = MessageVisitor(String::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
    }

    /// #311 S7 / M7: truncating the diagnostics list must not be silent --
    /// a caller with no visibility into this cache would otherwise have no
    /// way to know a `get_cached_diagnostics` result is incomplete.
    #[test]
    fn test_store_diagnostics_warns_when_truncating_list() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();
        let diagnostics: Vec<LspDiagnostic> = (0..5000)
            .map(|i| minimal_diagnostic(format!("diagnostic {i}: {}", "x".repeat(250))))
            .collect();

        let captured = CapturedMessages::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        cache.store_diagnostics(&test_server(), &uri, Some(1), diagnostics);
        drop(guard);

        let messages = captured.0.lock().unwrap().clone();
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
        let uri: Uri = "file:///test.rs".parse().unwrap();
        let mut diagnostic = minimal_diagnostic("small message".to_string());
        diagnostic.data = Some(serde_json::json!({
            "blob": "x".repeat(MAX_DIAGNOSTICS_ENTRY_BYTES + 1000),
        }));

        let captured = CapturedMessages::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);
        drop(guard);

        let messages = captured.0.lock().unwrap().clone();
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
        let uri: Uri = "file:///test.rs".parse().unwrap();

        let mut diagnostic = minimal_diagnostic("small message".to_string());
        diagnostic.data = Some(serde_json::json!({
            "blob": "x".repeat(MAX_DIAGNOSTICS_ENTRY_BYTES + 1000),
        }));

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);

        let stored = &cache.get_diagnostics(uri.as_str()).unwrap().diagnostics;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].message, "small message");
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
        let uri: Uri = "file:///test.rs".parse().unwrap();

        let mut diagnostic = minimal_diagnostic("small message".to_string());
        diagnostic.source = Some("x".repeat(MAX_DIAGNOSTICS_ENTRY_BYTES + 1000));

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);

        let stored = &cache.get_diagnostics(uri.as_str()).unwrap().diagnostics;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].message, "small message");
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
        let uri: Uri = "file:///test.rs".parse().unwrap();

        // Each field individually exceeds MAX_ENTRY_TEXT_BYTES (so
        // source/code truncation is exercised) and the combination exceeds
        // MAX_DIAGNOSTICS_ENTRY_BYTES, without needing to allocate multiple
        // megabytes per field just to prove the same point.
        let mut diagnostic = minimal_diagnostic("x".repeat(MAX_ENTRY_TEXT_BYTES + 1000));
        diagnostic.source = Some("x".repeat(MAX_ENTRY_TEXT_BYTES + 1000));
        diagnostic.code = Some(lsp_types::NumberOrString::String(
            "x".repeat(MAX_ENTRY_TEXT_BYTES + 1000),
        ));
        diagnostic.data = Some(serde_json::json!({ "blob": "x".repeat(MAX_ENTRY_TEXT_BYTES) }));
        diagnostic.tags = Some(vec![lsp_types::DiagnosticTag::UNNECESSARY; 50]);
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

        let stored = &cache.get_diagnostics(uri.as_str()).unwrap().diagnostics;
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
        let uri: Uri = "file:///test.rs".parse().unwrap();
        let mut diagnostics = vec![minimal_diagnostic(
            "x".repeat(MAX_DIAGNOSTICS_ENTRY_BYTES + 1000),
        )];

        cap_diagnostics_entry_size(&uri, &mut diagnostics);

        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0].message.len() <= DIAGNOSTIC_TERMINAL_FALLBACK_MESSAGE_BYTES + 20,
            "terminal fallback must truncate the message itself, got {} bytes",
            diagnostics[0].message.len()
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
        let uri: Uri = "file:///test.rs".parse().unwrap();

        let mut diagnostic = minimal_diagnostic("a small, ordinary diagnostic message".to_string());
        diagnostic.source = Some("rustc".to_string());

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);

        let stored = &cache.get_diagnostics(uri.as_str()).unwrap().diagnostics;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].message, "a small, ordinary diagnostic message");
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
        let uri: Uri = "file:///test.rs".parse().unwrap();

        let nul_heavy_message = "\0".repeat(MAX_ENTRY_TEXT_BYTES);
        let diagnostics: Vec<LspDiagnostic> = (0..3)
            .map(|_| minimal_diagnostic(nul_heavy_message.clone()))
            .collect();

        cache.store_diagnostics(&test_server(), &uri, Some(1), diagnostics);

        let stored = &cache.get_diagnostics(uri.as_str()).unwrap().diagnostics;
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
        let uri: Uri = "file:///test.rs".parse().unwrap();

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        assert_eq!(cache.diagnostics_count(), 1);

        cache.store_diagnostics(&test_server(), &uri, Some(2), vec![]);
        assert_eq!(cache.diagnostics_count(), 1);

        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.version, Some(2));
    }

    #[test]
    fn test_clear_diagnostics() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        assert_eq!(cache.diagnostics_count(), 1);

        let cleared = cache.clear_diagnostics(uri.as_str());
        assert!(cleared.is_some());
        assert_eq!(cache.diagnostics_count(), 0);
    }

    #[test]
    fn test_clear_all_diagnostics() {
        let mut cache = NotificationCache::new();
        let uri1: Uri = "file:///test1.rs".parse().unwrap();
        let uri2: Uri = "file:///test2.rs".parse().unwrap();

        cache.store_diagnostics(&test_server(), &uri1, Some(1), vec![]);
        cache.store_diagnostics(&test_server(), &uri2, Some(1), vec![]);
        assert_eq!(cache.diagnostics_count(), 2);

        cache.clear_all_diagnostics();
        assert_eq!(cache.diagnostics_count(), 0);
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
    fn test_clear_logs() {
        let mut cache = NotificationCache::new();
        cache.store_log(LogLevel::Info, "test".to_string());
        assert_eq!(cache.logs_count(), 1);

        cache.clear_logs();
        assert_eq!(cache.logs_count(), 0);
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

    #[test]
    fn test_clear_messages() {
        let mut cache = NotificationCache::new();
        cache.store_message(MessageType::Info, "test".to_string());
        assert_eq!(cache.messages_count(), 1);

        cache.clear_messages();
        assert_eq!(cache.messages_count(), 0);
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
        let uri: Uri = "file:///test.rs".parse().unwrap();

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
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: None,
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);
        assert_eq!(
            cache
                .get_diagnostics(uri.as_str())
                .unwrap()
                .diagnostics
                .len(),
            1
        );

        cache.store_diagnostics(&test_server(), &uri, Some(2), vec![]);
        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.diagnostics.len(), 0);
        assert_eq!(stored.version, Some(2));
    }

    #[test]
    fn test_store_many_diagnostics_single_file() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

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
                message: format!("Error {i}"),
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            })
            .collect();

        cache.store_diagnostics(&test_server(), &uri, Some(1), diagnostics);

        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
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
            let uri: Uri = format!("file:///test{i}.rs").parse().unwrap();
            cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        }

        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);

        // Oldest entries should be evicted (FIFO).
        let evicted: Uri = "file:///test0.rs".parse().unwrap();
        assert!(cache.get_diagnostics(evicted.as_str()).is_none());
        let newest: Uri = format!("file:///test{}.rs", MAX_DIAGNOSTIC_ENTRIES + 9)
            .parse()
            .unwrap();
        assert!(cache.get_diagnostics(newest.as_str()).is_some());
    }

    #[test]
    fn test_diagnostics_replacing_existing_uri_does_not_trigger_eviction() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///stable.rs".parse().unwrap();

        for i in 0..MAX_DIAGNOSTIC_ENTRIES {
            cache.store_diagnostics(
                &test_server(),
                &uri,
                Some(i32::try_from(i).unwrap()),
                vec![],
            );
        }
        assert_eq!(cache.diagnostics_count(), 1);
        assert!(cache.get_diagnostics(uri.as_str()).is_some());
    }

    #[test]
    fn test_diagnostics_republish_refreshes_eviction_order() {
        // #234 S2 / #266 S3 regression: an actively-edited file, republished
        // on every keystroke, must not be evicted ahead of a file that was
        // merely opened once and never touched again.
        let mut cache = NotificationCache::new();
        let actively_edited: Uri = "file:///keep.rs".parse().unwrap();
        cache.store_diagnostics(&test_server(), &actively_edited, Some(1), vec![]);

        // Fill the rest of the cache with untouched entries.
        for i in 0..MAX_DIAGNOSTIC_ENTRIES - 1 {
            let uri: Uri = format!("file:///untouched{i}.rs").parse().unwrap();
            cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        }
        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);

        // Republish the actively-edited file -- this must move it to the
        // back of the eviction order, not leave it at its original (oldest)
        // position.
        cache.store_diagnostics(&test_server(), &actively_edited, Some(2), vec![]);

        // One more new URI arrives, exceeding the cap by one: the oldest
        // *untouched* entry must be evicted, not the republished one.
        let overflow: Uri = "file:///overflow.rs".parse().unwrap();
        cache.store_diagnostics(&test_server(), &overflow, Some(1), vec![]);

        assert!(
            cache.get_diagnostics(actively_edited.as_str()).is_some(),
            "republished entry must survive eviction after being refreshed"
        );
        let oldest_untouched: Uri = "file:///untouched0.rs".parse().unwrap();
        assert!(
            cache.get_diagnostics(oldest_untouched.as_str()).is_none(),
            "the oldest never-republished entry must be evicted instead"
        );
        assert!(cache.get_diagnostics(overflow.as_str()).is_some());
    }

    #[test]
    fn test_clear_diagnostics_then_refill_does_not_evict_early() {
        let mut cache = NotificationCache::new();
        let first: Uri = "file:///first.rs".parse().unwrap();
        cache.store_diagnostics(&test_server(), &first, Some(1), vec![]);
        cache.clear_diagnostics(first.as_str());
        assert_eq!(cache.diagnostics_count(), 0);

        for i in 0..MAX_DIAGNOSTIC_ENTRIES {
            let uri: Uri = format!("file:///test{i}.rs").parse().unwrap();
            cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        }
        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);
        // Every entry from this batch must still be present -- the earlier
        // clear must not have left a stale `diagnostic_order` entry that
        // causes a premature eviction here.
        let first_of_batch: Uri = "file:///test0.rs".parse().unwrap();
        assert!(cache.get_diagnostics(first_of_batch.as_str()).is_some());
    }

    #[test]
    fn test_clear_diagnostics_nonexistent() {
        let mut cache = NotificationCache::new();
        let result = cache.clear_diagnostics("file:///nonexistent.rs");
        assert!(result.is_none());
    }

    #[test]
    fn test_store_diagnostics_no_version() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        cache.store_diagnostics(&test_server(), &uri, None, vec![]);
        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
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

        let quiet_uri: Uri = "file:///quiet/only_file.rs".parse().unwrap();
        cache.store_diagnostics(&quiet, &quiet_uri, Some(1), vec![]);

        // Drive the noisy server well past the aggregate cap -- it must be
        // allowed to consume nearly all of it since the quiet server leaves
        // the rest unused (#276), and once the aggregate is full it must
        // only evict its own oldest entries.
        for i in 0..MAX_DIAGNOSTIC_ENTRIES + 50 {
            let uri: Uri = format!("file:///noisy/file{i}.rs").parse().unwrap();
            cache.store_diagnostics(&noisy, &uri, Some(1), vec![]);
        }

        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);
        assert!(
            cache.get_diagnostics(quiet_uri.as_str()).is_some(),
            "quiet server's only entry must survive the noisy server's overflow"
        );

        let noisy_first: Uri = "file:///noisy/file0.rs".parse().unwrap();
        assert!(
            cache.get_diagnostics(noisy_first.as_str()).is_none(),
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
            let uri: Uri = format!("file:///file{i}.rs").parse().unwrap();
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
            let uri: Uri = format!("file:///file{i}.rs").parse().unwrap();
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
            let uri: Uri = format!("file:///a/file{i}.rs").parse().unwrap();
            cache.store_diagnostics(&a, &uri, Some(1), vec![]);
        }
        for i in 0..2 {
            let uri: Uri = format!("file:///b/file{i}.rs").parse().unwrap();
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
            let uri: Uri = format!("file:///a/file{i}.rs").parse().unwrap();
            cache.store_diagnostics(&a, &uri, Some(1), vec![]);
        }
        for i in 0..500 {
            let uri: Uri = format!("file:///b/file{i}.rs").parse().unwrap();
            cache.store_diagnostics(&b, &uri, Some(1), vec![]);
        }
        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);

        // `c` has never written before -- its very first write hits a full,
        // entirely-in-share aggregate.
        let c = ServerId::from("c");
        let new_uri: Uri = "file:///c/first.rs".parse().unwrap();
        cache.store_diagnostics(&c, &new_uri, Some(1), vec![]);

        assert_eq!(
            cache.diagnostics_count(),
            MAX_DIAGNOSTIC_ENTRIES,
            "the aggregate cap must still be enforced even when every existing server is within share"
        );
        assert!(cache.get_diagnostics(new_uri.as_str()).is_some());

        // `a` and `b` are tied at 500 entries each; the deterministic
        // tie-break in `server_to_evict_from` picks `b`, so `b`'s oldest
        // entry is the one evicted, not `a`'s.
        let b_oldest: Uri = "file:///b/file0.rs".parse().unwrap();
        assert!(
            cache.get_diagnostics(b_oldest.as_str()).is_none(),
            "the largest in-share server (tie-broken to b) must lose its oldest entry"
        );
        assert!(
            cache.get_diagnostics("file:///a/file0.rs").is_some(),
            "the other in-share server must be untouched"
        );
    }

    /// Re-publishing diagnostics for a URI under its existing owner must not
    /// count as a new entry against that server's budget.
    #[test]
    fn test_repeated_writes_same_owner_do_not_grow_order_map() {
        let mut cache = NotificationCache::new();
        let server = ServerId::from("server");
        let uri: Uri = "file:///test.rs".parse().unwrap();

        let max_version = i32::try_from(MAX_DIAGNOSTIC_ENTRIES).unwrap() + 10;
        for version in 0..max_version {
            cache.store_diagnostics(&server, &uri, Some(version), vec![]);
        }

        assert_eq!(cache.diagnostics_count(), 1);
        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
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
        let uri: Uri = "file:///test.rs".parse().unwrap();

        cache.store_diagnostics(&old_owner, &uri, Some(1), vec![]);
        cache.store_diagnostics(&new_owner, &uri, Some(2), vec![]);

        assert_eq!(cache.diagnostics_count(), 1);
        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.version, Some(2));

        // The old owner's order map must no longer reference this URI:
        // filling the old owner's budget with fresh entries must not evict
        // this URI a second time (it's not there to evict) nor corrupt state.
        for i in 0..MAX_DIAGNOSTIC_ENTRIES + 5 {
            let other: Uri = format!("file:///old/file{i}.rs").parse().unwrap();
            cache.store_diagnostics(&old_owner, &other, Some(1), vec![]);
        }
        assert!(cache.get_diagnostics(uri.as_str()).is_some());
    }

    /// #290: `diagnostics_owner` is what a cache-only read (e.g.
    /// `get_cached_diagnostics`) uses to resolve the publishing server's
    /// negotiated position encoding, so both branches -- an owner on record
    /// and none -- must behave correctly.
    #[test]
    fn test_diagnostics_owner_returns_publisher_after_store() {
        let mut cache = NotificationCache::new();
        let server = ServerId::from("rust");
        let uri: Uri = "file:///main.rs".parse().unwrap();

        cache.store_diagnostics(&server, &uri, Some(1), vec![]);

        assert_eq!(cache.diagnostics_owner(uri.as_str()), Some(&server));
    }

    #[test]
    fn test_diagnostics_owner_none_for_untracked_uri() {
        let cache = NotificationCache::new();
        let uri: Uri = "file:///never-seen.rs".parse().unwrap();

        assert_eq!(cache.diagnostics_owner(uri.as_str()), None);
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
        let uri: Uri = "file:///test.rs".parse().unwrap();

        cache.store_diagnostics(&old_owner, &uri, Some(1), vec![]);
        assert_eq!(cache.diagnostics_owner(uri.as_str()), Some(&old_owner));

        cache.store_diagnostics(&new_owner, &uri, Some(2), vec![]);
        assert_eq!(cache.diagnostics_owner(uri.as_str()), Some(&new_owner));
    }

    /// #266 S2: clearing one server's diagnostics must not disturb another
    /// server's cached entries, unlike `clear_all_diagnostics`.
    #[test]
    fn test_clear_server_diagnostics_scopes_to_one_server() {
        let mut cache = NotificationCache::new();
        let crashed = ServerId::from("crashed");
        let healthy = ServerId::from("healthy");

        let crashed_uri: Uri = "file:///crashed/main.py".parse().unwrap();
        let healthy_uri: Uri = "file:///healthy/main.rs".parse().unwrap();
        cache.store_diagnostics(&crashed, &crashed_uri, Some(1), vec![]);
        cache.store_diagnostics(&healthy, &healthy_uri, Some(1), vec![]);

        cache.clear_server_diagnostics(&crashed);

        assert!(cache.get_diagnostics(crashed_uri.as_str()).is_none());
        assert!(cache.get_diagnostics(healthy_uri.as_str()).is_some());
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
            let uri: Uri = format!("file:///file{i}.rs").parse().unwrap();
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
        let new_uri: Uri = "file:///other/new.rs".parse().unwrap();
        cache.store_diagnostics(&other, &new_uri, Some(1), vec![]);

        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);
        assert!(cache.get_diagnostics(new_uri.as_str()).is_some());
        let server_oldest: Uri = "file:///file0.rs".parse().unwrap();
        assert!(
            cache.get_diagnostics(server_oldest.as_str()).is_none(),
            "the pre-existing server's oldest entry, now far over its shrunk share, must be evicted"
        );
    }
}
