//! Workspace-indexing readiness tracking.
//!
//! Extracted from `bridge::notifications` (already ~2900 lines) so the
//! settle/latch transition logic below -- the part most likely to grow a
//! subtle off-by-one -- has its own directly unit-testable surface. See
//! [`IndexingTracker`] for the entry point; [`NotificationCache`](super::NotificationCache)
//! owns one and delegates every indexing-related call to it.
//!
//! Two independent signal sources feed the same per-server state:
//! rust-analyzer's custom `experimental/serverStatus` notification (#421,
//! [`IndexingTracker::observe_server_status`]), and the generic LSP
//! `$/progress` `begin`/`end` sequence every spec-compliant server may send
//! once mcpls advertises `window.workDoneProgress` (this issue,
//! [`IndexingTracker::observe_progress`]). [`IndexingSignalSource`] keeps
//! them from fighting over the same entry -- see that type's docs.

use std::collections::HashMap;
use std::time::Duration;

use lsp_types::{ProgressParams, ProgressToken};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::config::ServerId;
use crate::lsp::types::ProgressKind;

/// Workspace-indexing readiness of a routed LSP server.
///
/// Tracked separately from the `initialize`/`initialized` handshake
/// completion (`ServerState::is_ready`). A server can finish the handshake
/// and still be mid-index for tens of seconds afterward, during which
/// whole-workspace queries (hover, definition, references, completions,
/// code actions) can silently return an empty/`null` result
/// indistinguishable from a genuine "nothing found".
///
/// `Unknown` and `Ready` are treated identically by
/// `Translator::wait_for_indexing_ready` (proceed without waiting): a server
/// that never emits a recognized readiness signal must never be penalized
/// with an artificial delay, and the only way to tell "no signal ever comes"
/// apart from "just hasn't reported yet" would require guessing at a
/// server's protocol support, so both stay unblocked. Only `Loading` -- a
/// positive signal that indexing is actively in progress -- triggers a
/// bounded wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexingState {
    /// No recognized workspace-readiness signal has been observed for this
    /// server yet.
    #[default]
    Unknown,
    /// A recognized signal reported that indexing is still in progress.
    Loading,
    /// A recognized signal reported that the initial workspace load is
    /// complete.
    Ready,
}

/// Escape hatch for `IndexingTracker`'s readiness gate, configured per
/// server via `LspServerConfig::indexing`.
///
/// `Disabled` pins `IndexingTracker::state` at [`IndexingState::Unknown`]
/// unconditionally -- the same fail-open path a server that has simply never
/// sent a recognized signal already takes -- rather than adding a new branch
/// to the gate itself. Exists for a server whose `$/progress`/`serverStatus`
/// shape doesn't fit this tracker's assumptions (Accepted cost 1/3 in the
/// design): per-server granularity is enough, since `Error::WorkspaceIndexing`
/// already names the offending `server_id`.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::IndexingPolicy;
///
/// assert_eq!(IndexingPolicy::default(), IndexingPolicy::Auto);
/// let json = serde_json::to_string(&IndexingPolicy::Disabled).unwrap();
/// assert_eq!(json, "\"disabled\"");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexingPolicy {
    /// Track readiness normally from whatever signals the server sends.
    #[default]
    Auto,
    /// Never gate on this server; `indexing_state` always reads `Unknown`.
    Disabled,
}

impl IndexingPolicy {
    /// Whether this is the default [`Self::Auto`] policy.
    ///
    /// Used as `LspServerConfig::indexing`'s `skip_serializing_if` (M4):
    /// without it, the generated default `mcpls.toml` would write `indexing
    /// = "auto"` into every one of its ~30 builtin server entries, unlike
    /// every other optional field in that struct, which is omitted at its
    /// default. Takes `&self`, not `self`, since `skip_serializing_if`
    /// requires `fn(&T) -> bool` (matching `Option::is_none`'s convention).
    // serde needs `&self` here despite Self being a trivially-Copy 1-byte enum.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    #[must_use]
    pub(crate) const fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// Which signal kind last drove a server's [`IndexingEntry`].
///
/// Both sources write the same entry, so once one has spoken it must not be
/// silently overwritten by stale reasoning from the other (S2/#421): a
/// `Ready(ServerStatus)` rust-analyzer entry must not be knocked back to
/// `Loading` by a later `$/progress` sequence it never asked for, and
/// conversely a `ServerStatus` signal -- authoritative, since it is
/// rust-analyzer's own purpose-built readiness notification -- always
/// overrides whatever a generic `$/progress` sequence guessed, in any state.
/// See [`IndexingTracker::observe_server_status`] and
/// [`IndexingTracker::observe_progress`] for the exact precedence rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexingSignalSource {
    /// rust-analyzer's `experimental/serverStatus`.
    ServerStatus,
    /// A generic `$/progress` `begin`/`end` sequence.
    Progress,
}

/// A tracked [`IndexingState`] plus the bookkeeping needed to derive it.
///
/// `last_updated` is the *only* timestamp field (N5): both signal sources
/// refresh it on every accepted update, so [`IndexingTracker::state`]'s
/// staleness check (`INDEXING_STALENESS_BOUND`) works identically for
/// either source without a second, source-specific clock to keep in sync.
/// For a `Progress`-sourced entry this also means a multi-phase load whose
/// individual phases each stay under the staleness bound remains gated for
/// its whole duration, even if the load as a whole runs past
/// `INDEXING_STALENESS_BOUND` -- only a single phase that itself never
/// reports a boundary for that long fails open.
///
/// `open`/`empty_since`/`latched` are meaningful only for a `Progress`
/// source; a `ServerStatus`-sourced entry leaves them at their initial
/// values and `state` is read directly instead -- see
/// [`IndexingTracker::state`].
#[derive(Debug, Clone)]
struct IndexingEntry {
    /// Directly authoritative for a `ServerStatus` source; ignored on read
    /// for a `Progress` source, which recomputes state from
    /// `open`/`empty_since`/`latched` instead.
    state: IndexingState,
    source: IndexingSignalSource,
    last_updated: Instant,
    /// Progress tokens with an outstanding `begin` and no matching `end`
    /// yet, each paired with when its `begin` was accepted.
    ///
    /// A `HashMap`, not a `HashSet` (S1 / security HIGH): a single lost
    /// `end` -- realistic, since `try_send` drops frames once the
    /// lifecycle lane is full -- used to leave a token in the set forever,
    /// pinning [`IndexingTracker::state`] at `Loading` for the process
    /// lifetime even while unrelated later frames on the same server kept
    /// refreshing `last_updated` and masking the entry-wide staleness
    /// self-heal below. `observe_progress` evicts tokens older than
    /// `INDEXING_STALENESS_BOUND` on every write, and `state` additionally
    /// treats any individually stale token as absent even between writes,
    /// so the self-heal no longer depends on `last_updated` alone. Also
    /// capped at [`PROGRESS_OPEN_CAP`] so worst-case memory is bounded
    /// independent of eviction timing.
    open: HashMap<ProgressToken, Instant>,
    /// When `open` most recently became empty, if it ever has. `None` means
    /// "never been empty" -- critically different from "became empty a long
    /// time ago" (N2): both `state` and `latch` reads must treat them
    /// differently, or a fresh entry's very first `begin` reads/latches as
    /// though progress had already settled.
    empty_since: Option<Instant>,
    /// Once set, `observe_progress` ignores this server's `$/progress`
    /// stream forever and `state` reads `Ready` unconditionally. Set when a
    /// `begin` arrives after `open` has sat empty for at least
    /// `PROGRESS_LATCH_IDLE` -- the generic signal for "the workspace-load
    /// phase is over and everything after this is a per-request progress
    /// sequence (formatting, a single completion, ...), not more indexing".
    latched: bool,
}

impl IndexingEntry {
    /// A freshly observed `Progress`-sourced entry, with no outstanding
    /// `begin` yet -- `observe_progress` fills in `open`/`empty_since` right
    /// after this returns.
    fn fresh_progress() -> Self {
        Self {
            state: IndexingState::Unknown,
            source: IndexingSignalSource::Progress,
            last_updated: Instant::now(),
            open: HashMap::new(),
            empty_since: None,
            latched: false,
        }
    }
}

/// Custom notification method rust-analyzer uses to report workspace-load
/// progress; requires opting in at `initialize` (see `LspServer::initialize`).
const SERVER_STATUS_METHOD: &str = "experimental/serverStatus";

/// Boolean field on a [`SERVER_STATUS_METHOD`] payload: `true` once
/// rust-analyzer's initial workspace load is complete, `false` while it is
/// still in progress.
const QUIESCENT_FIELD: &str = "quiescent";

/// Once a `Loading` entry has gone this long without a fresh signal, reads
/// stop trusting it -- see [`IndexingTracker::state`].
///
/// Deliberately larger than `navigation::INDEXING_READY_TIMEOUT` (30s) and
/// anchored to the signal's own age, not to any individual caller's wait:
/// a caller that times out must never affect another concurrent or later
/// caller's deadline, only the age of the last real signal does. See
/// `navigation.rs`'s const-asserts for the cross-checked ordering between
/// this, `INDEXING_READY_TIMEOUT`, and the two constants below.
pub const INDEXING_STALENESS_BOUND: Duration = Duration::from_secs(60);

/// Default maximum time, in seconds, `Translator::wait_for_indexing_ready`
/// waits for a routed LSP server to report indexing readiness.
///
/// Only takes effect once a readiness signal has shown indexing is actually
/// in progress. A raw `u64` (not a `Duration`), so
/// `config::default_indexing_ready_timeout_seconds` can share this single
/// source of truth without a config -> translator module dependency;
/// `navigation::INDEXING_READY_TIMEOUT` derives its `Duration` from this
/// same value. Overridable via `workspace.indexing_ready_timeout_seconds` in
/// `mcpls.toml` (#424).
pub const DEFAULT_INDEXING_READY_TIMEOUT_SECS: u64 = 30;

/// How long a `Progress`-sourced entry's `open` set must stay empty before a
/// *read* trusts it as settled and reports `Ready` -- the read-path
/// counterpart to [`PROGRESS_LATCH_IDLE`] below.
///
/// Covers the ordinary inter-phase gap in a multi-phase load (e.g. gopls
/// ending "Setting up workspace" and taking a few seconds to run `go list`
/// before beginning "Loading packages"): without this, a read landing in
/// that gap would report `Ready` mid-load. 3s, not 1s: shortening it only
/// narrows this real gap-covering window while buying nothing, since the
/// cost is a few seconds of latency paid once per server lifetime and only
/// by servers that emit progress at all.
pub const PROGRESS_SETTLE: Duration = Duration::from_secs(3);

/// How long a `Progress`-sourced entry's `open` set must stay empty before a
/// `begin` write latches it `Ready` forever, instead of re-gating.
///
/// Deliberately a *separate*, larger threshold than [`PROGRESS_SETTLE`]
/// (N1): collapsing them into one 3s threshold reintroduced the exact
/// regression this two-threshold split fixes. gopls's own multi-phase load
/// routinely leaves a several-second gap between ending one phase and
/// beginning the next -- with a single 3s threshold, that ordinary gap
/// would permanently latch the tracker `Ready` after the *first* phase,
/// leaving the actual package-loading phase (the part that matters) to run
/// completely ungated. 30s is long enough that only a genuine end of the
/// workspace-load phase -- followed by a real per-request sequence
/// (formatting, one completion) -- crosses it.
pub const PROGRESS_LATCH_IDLE: Duration = Duration::from_secs(30);

/// Hard cap on the number of distinct tokens tracked in one server's
/// `open` set (security HIGH).
///
/// 64 is far above any real server's concurrent-operation count. Bounds
/// worst-case memory independent of eviction timing -- a server flooding
/// many distinct never-ending tokens within a single
/// `INDEXING_STALENESS_BOUND` window would otherwise grow `open` without
/// limit before the time-based eviction in `observe_progress` ever gets a
/// chance to shrink it. Once reached, `observe_progress` stops inserting
/// and `state` fails open (`Unknown`) rather than trusting an accumulator
/// this large.
const PROGRESS_OPEN_CAP: usize = 64;

/// Maximum accepted length, in bytes, of a `ProgressToken::String` before
/// [`IndexingTracker::observe_progress`] rejects the frame outright
/// (security HIGH).
///
/// Without this, [`PROGRESS_OPEN_CAP`] bounds token *count* but not size --
/// a server-chosen token string is otherwise bounded only by the
/// transport's 10 MiB `MAX_CONTENT_LENGTH`, so the count cap alone could
/// still admit up to `PROGRESS_OPEN_CAP * 10 MiB` of retained token bytes.
/// A few hundred bytes is far more than any real progress token needs.
const PROGRESS_TOKEN_MAX_LEN: usize = 256;

/// Per-server workspace-indexing readiness state plus the escape-hatch
/// policy map, owned by [`NotificationCache`](super::NotificationCache) and
/// delegated to for every indexing-related call.
#[derive(Debug, Default)]
pub struct IndexingTracker {
    entries: HashMap<ServerId, IndexingEntry>,
    policies: HashMap<ServerId, IndexingPolicy>,
}

impl IndexingTracker {
    /// Create an empty tracker: every server starts at [`IndexingState::Unknown`]
    /// under [`IndexingPolicy::Auto`].
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Configure `server_id`'s [`IndexingPolicy`], set once at server
    /// registration from `LspServerConfig::indexing` before any signal for
    /// it can arrive.
    pub(crate) fn set_policy(&mut self, server_id: ServerId, policy: IndexingPolicy) {
        self.policies.insert(server_id, policy);
    }

    fn is_disabled(&self, server_id: &ServerId) -> bool {
        self.policies.get(server_id) == Some(&IndexingPolicy::Disabled)
    }

    /// Record a workspace-readiness signal from an unrecognized/custom LSP
    /// notification (`LspNotification::Other`).
    ///
    /// Currently recognizes rust-analyzer's `experimental/serverStatus`
    /// notification: a `quiescent` boolean of `false` marks the server
    /// [`IndexingState::Loading`], `true` marks it [`IndexingState::Ready`].
    /// Any other method, or a `serverStatus` payload missing/malformed the
    /// field, leaves the current entry entirely untouched -- including its
    /// `source` -- rather than erroring: a malformed frame must not mark
    /// this server `ServerStatus`-sourced and thereby permanently disable
    /// its `$/progress` path via [`Self::observe_progress`]'s stickiness
    /// check (N6).
    ///
    /// Once a `ServerStatus`-sourced entry reaches [`IndexingState::Ready`]
    /// it never regresses on its own (sticky); a `ServerStatus` signal
    /// always overwrites a `Progress`-sourced entry regardless of that
    /// entry's current state, since it is the more authoritative source.
    pub(crate) fn observe_server_status(
        &mut self,
        server_id: &ServerId,
        method: &str,
        params: Option<&serde_json::Value>,
    ) {
        if self.is_disabled(server_id) {
            return;
        }
        if method != SERVER_STATUS_METHOD {
            return;
        }
        let Some(quiescent) = params
            .and_then(|p| p.get(QUIESCENT_FIELD))
            .and_then(serde_json::Value::as_bool)
        else {
            return;
        };

        let sticky_ready = self.entries.get(server_id).is_some_and(|entry| {
            entry.source == IndexingSignalSource::ServerStatus
                && entry.state == IndexingState::Ready
        });
        if sticky_ready {
            return;
        }

        self.entries.insert(
            server_id.clone(),
            IndexingEntry {
                state: if quiescent {
                    IndexingState::Ready
                } else {
                    IndexingState::Loading
                },
                source: IndexingSignalSource::ServerStatus,
                last_updated: Instant::now(),
                open: HashMap::new(),
                empty_since: None,
                latched: false,
            },
        );
    }

    /// Record a `$/progress` notification toward `server_id`'s readiness.
    ///
    /// A no-op once a well-formed `experimental/serverStatus` signal has
    /// been seen for this server (P2b/S2): that source is authoritative and
    /// sticky, so a `$/progress` sequence arriving after it (e.g.
    /// rust-analyzer's own pre-`serverStatus` startup progress, or a later
    /// per-request sequence) must never resurrect or override it.
    ///
    /// Otherwise implements the settle/latch transition (see
    /// [`PROGRESS_SETTLE`]/[`PROGRESS_LATCH_IDLE`] docs for the two
    /// thresholds this balances):
    /// - an unparseable frame (`report`, or missing/unrecognized `kind`) is
    ///   ignored outright, as is a `ProgressToken::String` longer than
    ///   [`PROGRESS_TOKEN_MAX_LEN`] (security HIGH);
    /// - once `latched`, every later frame is ignored;
    /// - every accepted frame first evicts tokens whose `begin` is older
    ///   than `INDEXING_STALENESS_BOUND` from `open` (S1) -- otherwise a
    ///   single lost `end` leaves a token in `open` forever, and unrelated
    ///   later frames on the same server keep refreshing `last_updated`
    ///   and masking the entry-wide staleness self-heal that would
    ///   otherwise catch it;
    /// - `begin`: if `open` is empty and has been for at least
    ///   [`PROGRESS_LATCH_IDLE`], latches instead of reopening -- **the
    ///   `PROGRESS_LATCH_IDLE` check only fires when `empty_since` is
    ///   `Some`**; a fresh entry's very first `begin` has `empty_since ==
    ///   None` ("never been empty", not "empty forever ago") and must
    ///   always open normally, never latch (N2) -- otherwise the entire
    ///   feature silently no-ops on the very first workspace load. Anything
    ///   short of the latch threshold inserts the token (unless
    ///   [`PROGRESS_OPEN_CAP`] is already reached, security HIGH: the frame
    ///   is then dropped rather than grown further) and clears
    ///   `empty_since` normally, including the ordinary inter-phase gap a
    ///   multi-phase load leaves between `PROGRESS_SETTLE` and
    ///   `PROGRESS_LATCH_IDLE` (N1) -- that gap must re-gate, not latch;
    /// - `end`: sets `empty_since` **only if this token was actually open**
    ///   (`open.remove` returned `Some`) and `open` is now empty (N3) -- an
    ///   unmatched `end` (a dropped `begin`, or one arriving after latching)
    ///   must not fabricate a settle window on an already-empty entry.
    pub(crate) fn observe_progress(&mut self, server_id: &ServerId, params: &ProgressParams) {
        if self.is_disabled(server_id) {
            return;
        }
        let Some(kind) = ProgressKind::from_value(&params.value) else {
            return;
        };
        if self
            .entries
            .get(server_id)
            .is_some_and(|entry| entry.source == IndexingSignalSource::ServerStatus)
        {
            return;
        }
        if let ProgressToken::String(token) = &params.token
            && token.len() > PROGRESS_TOKEN_MAX_LEN
        {
            // Reject outright, before PROGRESS_OPEN_CAP could be multiplied by an oversized token (security HIGH).
            return;
        }

        let entry = self
            .entries
            .entry(server_id.clone())
            .or_insert_with(IndexingEntry::fresh_progress);
        if entry.latched {
            return;
        }

        let now = Instant::now();
        // Age out tokens whose `begin` never got a matching `end` (S1) -- see `IndexingEntry::open`'s doc.
        entry
            .open
            .retain(|_, began| began.elapsed() < INDEXING_STALENESS_BOUND);

        match kind {
            ProgressKind::Begin => {
                if entry.open.is_empty()
                    && entry
                        .empty_since
                        .is_some_and(|since| since.elapsed() >= PROGRESS_LATCH_IDLE)
                {
                    entry.latched = true;
                    entry.open.clear();
                    return;
                }
                if entry.open.len() < PROGRESS_OPEN_CAP {
                    entry.open.insert(params.token.clone(), now);
                }
                // Else at PROGRESS_OPEN_CAP already -- deliberately not inserted; `state` fails open instead.
                entry.empty_since = None;
            }
            ProgressKind::End => {
                if entry.open.remove(&params.token).is_some() && entry.open.is_empty() {
                    entry.empty_since = Some(now);
                }
            }
        }
        entry.last_updated = now;
    }

    /// Current tracked workspace-indexing readiness for `server_id`.
    ///
    /// Returns [`IndexingState::Unknown`] if no signal has ever been
    /// observed for `server_id`, or if it is configured
    /// [`IndexingPolicy::Disabled`].
    ///
    /// A `ServerStatus`-sourced `Loading` entry older than
    /// `INDEXING_STALENESS_BOUND` reads back as `Unknown` rather than
    /// `Loading`: the self-heal for a `quiescent: true` notification dropped
    /// by a full channel, or a server that stalled mid-index. A
    /// `Progress`-sourced entry applies the same staleness bound to
    /// `last_updated` regardless of `open`/`latched` as a coarse entry-wide
    /// gate, then: `latched` reads `Ready` unconditionally; `open.len() >=
    /// PROGRESS_OPEN_CAP` reads `Unknown` (security HIGH -- too many
    /// concurrently open tokens to track reliably, matching
    /// `observe_progress`'s refusal to grow `open` further); otherwise any
    /// token in `open` individually younger than `INDEXING_STALENESS_BOUND`
    /// reads `Loading` (S1: computed here, not just via write-time
    /// eviction, so a token that went stale purely with the passage of time
    /// between writes still stops pinning `Loading` on the very next read).
    /// If `open` is truly empty, reads `Loading` for [`PROGRESS_SETTLE`]
    /// after `empty_since`, `Ready` once past that -- but only when
    /// `empty_since` is `Some`, i.e. a real `begin`-then-`end` transition
    /// was actually observed to complete. `empty_since == None` reads
    /// `Unknown` instead of `Ready`: the only way to reach `open` empty
    /// with `empty_since` still `None` is an entry whose first-ever
    /// `$/progress` frame was an unmatched `end` (e.g. its `begin` was
    /// dropped) -- there is no evidence indexing ever finished, so this
    /// must fail open rather than falsely report readiness. If `open`
    /// is non-empty but every token in it is individually stale (a lost
    /// `end` that unrelated later frames kept `last_updated` fresh enough to
    /// survive the entry-wide gate above), also reads `Unknown` rather than
    /// running the settle/latch logic meant for a genuinely-observed `end`.
    pub(crate) fn state(&self, server_id: &ServerId) -> IndexingState {
        if self.is_disabled(server_id) {
            return IndexingState::Unknown;
        }
        let Some(entry) = self.entries.get(server_id) else {
            return IndexingState::Unknown;
        };
        match entry.source {
            IndexingSignalSource::ServerStatus => {
                if entry.state == IndexingState::Loading
                    && entry.last_updated.elapsed() >= INDEXING_STALENESS_BOUND
                {
                    IndexingState::Unknown
                } else {
                    entry.state
                }
            }
            IndexingSignalSource::Progress => {
                if entry.latched {
                    return IndexingState::Ready;
                }
                if entry.last_updated.elapsed() >= INDEXING_STALENESS_BOUND {
                    return IndexingState::Unknown;
                }
                if entry.open.len() >= PROGRESS_OPEN_CAP {
                    return IndexingState::Unknown;
                }
                let has_live_open = entry
                    .open
                    .values()
                    .any(|began| began.elapsed() < INDEXING_STALENESS_BOUND);
                if has_live_open {
                    return IndexingState::Loading;
                }
                if entry.open.is_empty() {
                    match entry.empty_since {
                        Some(since) if since.elapsed() < PROGRESS_SETTLE => IndexingState::Loading,
                        Some(_) => IndexingState::Ready,
                        // No transition has ever been observed to settle.
                        None => IndexingState::Unknown,
                    }
                } else {
                    IndexingState::Unknown
                }
            }
        }
    }

    /// Forget `server_id`'s tracked entry, reverting it to
    /// [`IndexingState::Unknown`]. Does not touch its [`IndexingPolicy`]
    /// (see [`Self::set_policy`]) -- a respawned process still honors
    /// whatever the static config said.
    pub(crate) fn reset(&mut self, server_id: &ServerId) {
        self.entries.remove(server_id);
    }
}

// TODO(critic): mock LSP harness for $/progress sequences -- see follow-up issue

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn test_server() -> ServerId {
        ServerId::from("test-server")
    }

    fn progress(kind: &str, token: i32) -> ProgressParams {
        ProgressParams {
            token: ProgressToken::Int(token),
            value: serde_json::json!({ "kind": kind }),
        }
    }

    #[test]
    fn test_state_defaults_unknown() {
        let tracker = IndexingTracker::new();
        assert_eq!(tracker.state(&test_server()), IndexingState::Unknown);
    }

    #[test]
    fn test_observe_server_status_quiescent_false_marks_loading() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_server_status(
            &server,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        assert_eq!(tracker.state(&server), IndexingState::Loading);
    }

    #[test]
    fn test_observe_server_status_quiescent_true_marks_ready() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_server_status(
            &server,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        assert_eq!(tracker.state(&server), IndexingState::Ready);
    }

    #[test]
    fn test_observe_server_status_ignores_unrecognized_method() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_server_status(
            &server,
            "window/logMessage",
            Some(&serde_json::json!({"quiescent": false})),
        );
        assert_eq!(tracker.state(&server), IndexingState::Unknown);
    }

    /// N6: a malformed `serverStatus` payload must not mark the entry's
    /// source, or it would permanently disable that server's `$/progress`
    /// path via `observe_progress`'s stickiness check even though no
    /// well-formed `ServerStatus` signal was ever actually seen.
    #[test]
    fn test_malformed_server_status_does_not_mark_source_or_disable_progress() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_server_status(&server, "experimental/serverStatus", None);
        assert_eq!(tracker.state(&server), IndexingState::Unknown);

        tracker.observe_progress(&server, &progress("begin", 1));
        assert_eq!(
            tracker.state(&server),
            IndexingState::Loading,
            "the progress path must still be live after a malformed serverStatus payload"
        );
    }

    #[test]
    fn test_observe_server_status_ready_is_sticky() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_server_status(
            &server,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        tracker.observe_server_status(
            &server,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        assert_eq!(tracker.state(&server), IndexingState::Ready);
    }

    #[test]
    fn test_observe_server_status_tracks_servers_independently() {
        let mut tracker = IndexingTracker::new();
        let rust = ServerId::from("rust");
        let python = ServerId::from("python");
        tracker.observe_server_status(
            &rust,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        assert_eq!(tracker.state(&rust), IndexingState::Loading);
        assert_eq!(tracker.state(&python), IndexingState::Unknown);
    }

    #[tokio::test(start_paused = true)]
    async fn test_state_treats_stale_server_status_loading_as_unknown() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_server_status(
            &server,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        assert_eq!(tracker.state(&server), IndexingState::Loading);

        tokio::time::advance(INDEXING_STALENESS_BOUND.saturating_sub(Duration::from_secs(1))).await;
        assert_eq!(tracker.state(&server), IndexingState::Loading);

        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(tracker.state(&server), IndexingState::Unknown);
    }

    #[tokio::test(start_paused = true)]
    async fn test_observe_server_status_refreshes_staleness_clock() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_server_status(
            &server,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );

        tokio::time::advance(INDEXING_STALENESS_BOUND.saturating_sub(Duration::from_secs(1))).await;
        tracker.observe_server_status(
            &server,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );

        tokio::time::advance(INDEXING_STALENESS_BOUND.saturating_sub(Duration::from_secs(1))).await;
        assert_eq!(tracker.state(&server), IndexingState::Loading);
    }

    /// N2: a fresh entry's very first `begin` must open (`Loading`), never
    /// latch straight to `Ready` -- the `empty_since == None` ("never been
    /// empty") case must not satisfy the latch-idle check.
    #[test]
    fn test_first_begin_on_fresh_entry_yields_loading_never_ready() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_progress(&server, &progress("begin", 1));
        assert_eq!(tracker.state(&server), IndexingState::Loading);
    }

    /// N3: an `end` for a token that was never open must not create (or
    /// refresh) `empty_since` on an already-empty entry.
    #[tokio::test(start_paused = true)]
    async fn test_unmatched_end_does_not_create_empty_since() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        // Open and close one legitimate operation first, establishing a real (old) empty_since.
        tracker.observe_progress(&server, &progress("begin", 1));
        tracker.observe_progress(&server, &progress("end", 1));

        tokio::time::advance(PROGRESS_SETTLE + Duration::from_secs(1)).await;
        assert_eq!(
            tracker.state(&server),
            IndexingState::Ready,
            "settled after the real end"
        );

        // An end for a token that was never open must not refresh empty_since back to "just now".
        tracker.observe_progress(&server, &progress("end", 99));
        assert_eq!(
            tracker.state(&server),
            IndexingState::Ready,
            "an unmatched end must not manufacture a fresh settle window"
        );
    }

    /// Fix 8 (code-review round 2): a fresh entry whose first-ever
    /// `$/progress` frame is an `end` (e.g. its matching `begin` was
    /// dropped by a full lifecycle lane) must read `Unknown`, not `Ready`
    /// -- `empty_since == None` means "no load-to-quiescent transition has
    /// ever been observed", not "already settled".
    #[tokio::test(start_paused = true)]
    async fn test_end_as_first_ever_frame_reads_unknown_not_ready() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_progress(&server, &progress("end", 1)); // no prior begin

        assert_eq!(
            tracker.state(&server),
            IndexingState::Unknown,
            "an end with no observed prior begin must not read Ready immediately"
        );

        tokio::time::advance(PROGRESS_SETTLE + Duration::from_secs(1)).await;
        assert_eq!(
            tracker.state(&server),
            IndexingState::Unknown,
            "must still read Unknown once the settle window would have expired -- there was \
             never a real empty_since to settle from"
        );
    }

    /// `begin` after the `open` set has sat empty for at least
    /// `PROGRESS_LATCH_IDLE` latches the entry `Ready` forever.
    #[tokio::test(start_paused = true)]
    async fn test_begin_after_latch_idle_gap_latches_permanently() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_progress(&server, &progress("begin", 1));
        tracker.observe_progress(&server, &progress("end", 1));

        tokio::time::advance(PROGRESS_LATCH_IDLE + Duration::from_secs(1)).await;
        tracker.observe_progress(&server, &progress("begin", 2));
        assert_eq!(
            tracker.state(&server),
            IndexingState::Ready,
            "a begin after PROGRESS_LATCH_IDLE of quiet must latch, not re-gate"
        );

        // Latched forever: even ending the "reopened" op, or a later begin, changes nothing.
        tracker.observe_progress(&server, &progress("end", 2));
        tracker.observe_progress(&server, &progress("begin", 3));
        assert_eq!(tracker.state(&server), IndexingState::Ready);
    }

    /// N1 (gopls regression guard): a `begin` after a gap strictly between
    /// `PROGRESS_SETTLE` and `PROGRESS_LATCH_IDLE` must re-gate as `Loading`,
    /// not latch -- this is the ordinary inter-phase gap shape gopls uses.
    #[tokio::test(start_paused = true)]
    async fn test_begin_after_mid_gap_regates_and_does_not_latch() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_progress(&server, &progress("begin", 1));
        tracker.observe_progress(&server, &progress("end", 1));

        tokio::time::advance(Duration::from_secs(10)).await; // between 3s and 30s
        tracker.observe_progress(&server, &progress("begin", 2));
        assert_eq!(
            tracker.state(&server),
            IndexingState::Loading,
            "a mid-gap begin must re-gate the next phase, not latch it away"
        );
    }

    /// S1 (core): a phase gap shorter than `PROGRESS_SETTLE` must not read
    /// `Ready` mid-gap.
    #[tokio::test(start_paused = true)]
    async fn test_phase_gap_shorter_than_settle_does_not_read_ready() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_progress(&server, &progress("begin", 1));
        tracker.observe_progress(&server, &progress("end", 1));

        tokio::time::advance(PROGRESS_SETTLE.checked_sub(Duration::from_secs(1)).unwrap()).await;
        assert_eq!(tracker.state(&server), IndexingState::Loading);
    }

    #[test]
    fn test_open_non_empty_reads_loading() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_progress(&server, &progress("begin", 1));
        tracker.observe_progress(&server, &progress("begin", 2));
        tracker.observe_progress(&server, &progress("end", 1));
        assert_eq!(
            tracker.state(&server),
            IndexingState::Loading,
            "token 2 is still open"
        );
    }

    /// Multi-server isolation for the `Progress` source, mirroring
    /// `test_observe_server_status_tracks_servers_independently` for
    /// `ServerStatus` -- the same `HashMap<ServerId, _>` isolation, proven
    /// for the other source too.
    #[test]
    fn test_observe_progress_tracks_servers_independently() {
        let mut tracker = IndexingTracker::new();
        let rust = ServerId::from("rust");
        let go = ServerId::from("go");

        tracker.observe_progress(&rust, &progress("begin", 1));

        assert_eq!(
            tracker.state(&rust),
            IndexingState::Loading,
            "rust's token is still open"
        );
        assert_eq!(
            tracker.state(&go),
            IndexingState::Unknown,
            "go has received no progress signal of its own"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_progress_last_updated_past_staleness_bound_reads_unknown() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_progress(&server, &progress("begin", 1));

        tokio::time::advance(INDEXING_STALENESS_BOUND + Duration::from_secs(1)).await;
        assert_eq!(
            tracker.state(&server),
            IndexingState::Unknown,
            "a single op with no phase boundary for over a minute must fail open"
        );
    }

    /// N5: a multi-phase load whose *individual* phases each refresh
    /// `last_updated` stays gated past the raw 60s bound, as long as no
    /// single phase itself runs that long uninterrupted.
    #[tokio::test(start_paused = true)]
    async fn test_multiphase_load_past_staleness_bound_with_boundaries_stays_loading() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_progress(&server, &progress("begin", 1));

        tokio::time::advance(Duration::from_secs(40)).await;
        tracker.observe_progress(&server, &progress("end", 1));
        tracker.observe_progress(&server, &progress("begin", 2));

        tokio::time::advance(Duration::from_secs(40)).await;
        // 80s total elapsed, but every gap between refreshes stayed under INDEXING_STALENESS_BOUND (60s).
        assert_eq!(tracker.state(&server), IndexingState::Loading);
    }

    /// S1 (security HIGH regression guard): a lost `end` must not pin
    /// `state` at `Loading` for the process lifetime, even while unrelated
    /// later frames on the *same* server keep refreshing `last_updated` --
    /// the exact mechanism that let this bug survive the pre-existing
    /// entry-wide-only staleness self-heal.
    #[tokio::test(start_paused = true)]
    async fn test_lost_end_self_heals_after_staleness_bound() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_progress(&server, &progress("begin", 1)); // never gets an `end`

        // Unrelated periodic activity on a different token keeps last_updated fresh.
        tokio::time::advance(Duration::from_secs(30)).await;
        tracker.observe_progress(&server, &progress("begin", 2));
        tracker.observe_progress(&server, &progress("end", 2));

        tokio::time::advance(Duration::from_secs(25)).await; // t=55s
        tracker.observe_progress(&server, &progress("begin", 3));
        tracker.observe_progress(&server, &progress("end", 3));

        assert_eq!(
            tracker.state(&server),
            IndexingState::Loading,
            "token 1 is still within its own staleness window at t=55s"
        );

        // Token 1's own begin (t=0) is now stale (t=61s), though last_updated (t=55s) is not.
        tokio::time::advance(Duration::from_secs(6)).await; // t=61s
        assert_eq!(
            tracker.state(&server),
            IndexingState::Unknown,
            "a token whose own begin exceeded INDEXING_STALENESS_BOUND must stop pinning \
             Loading even while unrelated later frames keep the entry-wide last_updated clock \
             fresh"
        );
    }

    /// Security HIGH: reaching `PROGRESS_OPEN_CAP` distinct open tokens
    /// must fail open (`Unknown`) instead of growing `open` further or
    /// staying `Loading` forever.
    #[test]
    fn test_open_token_cap_forces_unknown() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        for i in 0..(PROGRESS_OPEN_CAP - 1) {
            tracker.observe_progress(&server, &progress("begin", i32::try_from(i).unwrap()));
        }
        assert_eq!(
            tracker.state(&server),
            IndexingState::Loading,
            "PROGRESS_OPEN_CAP - 1 distinct open tokens must still read Loading"
        );

        tracker.observe_progress(
            &server,
            &progress("begin", i32::try_from(PROGRESS_OPEN_CAP - 1).unwrap()),
        );
        assert_eq!(
            tracker.state(&server),
            IndexingState::Unknown,
            "reaching PROGRESS_OPEN_CAP must fail open instead of staying Loading forever"
        );
    }

    /// Security HIGH: an oversized `ProgressToken::String` must be rejected
    /// outright, before it can multiply `PROGRESS_OPEN_CAP` by up to
    /// `MAX_CONTENT_LENGTH` (10 MiB) per token.
    #[test]
    fn test_oversized_string_token_is_rejected() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        let oversized = ProgressParams {
            token: ProgressToken::String("x".repeat(PROGRESS_TOKEN_MAX_LEN + 1)),
            value: serde_json::json!({ "kind": "begin" }),
        };
        tracker.observe_progress(&server, &oversized);
        assert_eq!(
            tracker.state(&server),
            IndexingState::Unknown,
            "an oversized token must be dropped outright, not tracked"
        );
    }

    #[test]
    fn test_malformed_progress_kind_ignored() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_progress(&server, &progress("report", 1));
        assert_eq!(tracker.state(&server), IndexingState::Unknown);

        let missing_kind = ProgressParams {
            token: ProgressToken::Int(1),
            value: serde_json::json!({}),
        };
        tracker.observe_progress(&server, &missing_kind);
        assert_eq!(tracker.state(&server), IndexingState::Unknown);
    }

    #[test]
    fn test_disabled_policy_pins_unknown() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.set_policy(server.clone(), IndexingPolicy::Disabled);

        tracker.observe_server_status(
            &server,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        tracker.observe_progress(&server, &progress("begin", 1));
        assert_eq!(tracker.state(&server), IndexingState::Unknown);
    }

    /// S2 (#421 regression guard): a settled `$/progress` sequence followed
    /// by `quiescent: false` must still report `Loading` -- the
    /// `ServerStatus` source must be able to override a `Progress` entry in
    /// any state, not just while that entry itself reads `Loading`.
    #[tokio::test(start_paused = true)]
    async fn test_progress_settle_then_quiescent_false_yields_loading() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_progress(&server, &progress("begin", 1));
        tracker.observe_progress(&server, &progress("end", 1));
        tokio::time::advance(PROGRESS_SETTLE + Duration::from_secs(1)).await;
        assert_eq!(tracker.state(&server), IndexingState::Ready);

        tracker.observe_server_status(
            &server,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        assert_eq!(tracker.state(&server), IndexingState::Loading);
    }

    /// P2b: once a `ServerStatus` signal has been seen, later `$/progress`
    /// frames are ignored entirely -- rust-analyzer's own pre-`serverStatus`
    /// `$/progress` chatter (or a later per-request sequence) must never
    /// resurrect or override the authoritative source.
    #[test]
    fn test_server_status_entry_ignores_later_progress_frames() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.observe_server_status(
            &server,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        tracker.observe_progress(&server, &progress("begin", 1));
        assert_eq!(tracker.state(&server), IndexingState::Ready);
    }

    #[test]
    fn test_reset_clears_entry_but_not_policy() {
        let mut tracker = IndexingTracker::new();
        let server = test_server();
        tracker.set_policy(server.clone(), IndexingPolicy::Disabled);
        tracker.observe_progress(&server, &progress("begin", 1));
        tracker.reset(&server);
        assert_eq!(
            tracker.state(&server),
            IndexingState::Unknown,
            "policy stays Disabled, so this reads Unknown regardless of the reset entry"
        );

        let mut auto_tracker = IndexingTracker::new();
        auto_tracker.observe_progress(&server, &progress("begin", 1));
        auto_tracker.reset(&server);
        assert_eq!(auto_tracker.state(&server), IndexingState::Unknown);
    }
}
