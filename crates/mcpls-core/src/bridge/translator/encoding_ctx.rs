//! Per-response position/range encoding conversion between MCP's 1-based
//! UTF-16 columns and an LSP server's negotiated encoding.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use super::dto::{Position, Position2D, PositionDegradation, PositionRange, Range};
use crate::bridge::encoding::{
    ColumnFidelity, PositionEncoding, lsp_to_mcp_position, mcp_to_lsp_position,
};
use crate::bridge::state::{ResourceLimits, uri_to_path};
use crate::bridge::{DocumentTracker, WorkspaceRoots};
use crate::config::SizeLimit;
use crate::util::lock_std;

/// Multiple of `ResourceLimits::max_file_size` that [`read_line_text`]'s
/// disk-read fallback (via [`DocumentTracker::read_line_checked`]) may scan
/// across one `EncodingCtx`'s whole lifetime (one MCP response), independent
/// of how many distinct `(path, line)` lookups that spans.
///
/// [`LineCacheState::entries`] alone caps repeats of the *same* line, but a
/// response naming enough distinct lines could still add up to an unbounded
/// amount of scanning even with that cache and
/// [`super::navigation::MAX_NORMALIZED_LOCATIONS`]'s count cap in place (see
/// #474). Every conversion that reaches disk goes through [`read_line_text`],
/// so charging one budget there caps every `EncodingCtx`-mediated handler
/// uniformly.
///
/// The budget covers disk I/O only. A tracked document's lookups never touch
/// it: they are bounded by `DocumentText`'s line checkpoints (#488).
const LINE_READ_BUDGET_FILE_MULTIPLE: u64 = 4;

/// Absolute ceiling of the per-response disk-read budget (256 MiB),
/// whatever `max_file_size` is configured to.
const LINE_READ_BUDGET_CEILING: u64 = 256 * 1024 * 1024;

/// Per-response disk-read budget derived from the tracker's configured
/// limits (#489): [`LINE_READ_BUDGET_FILE_MULTIPLE`] times
/// `max_file_size`, so a deployment that raises the single-file limit gets a
/// proportionally larger budget, up to [`LINE_READ_BUDGET_CEILING`]. Above
/// `ceiling / multiple` (64 MiB) a single maximal file read can exceed the
/// budget; such a read is then reported as unconverted, not unbounded.
///
/// An unlimited `max_file_size` disables the per-file limit, not the I/O
/// budget: it falls back to the multiple of [`SizeLimit::DEFAULT`]. The result
/// never exceeds [`LINE_READ_BUDGET_CEILING`].
const fn line_read_budget(limits: ResourceLimits) -> u64 {
    let basis = match limits.max_file_size.get() {
        Some(max) => max.get(),
        None => match SizeLimit::DEFAULT.get() {
            Some(default) => default.get(),
            None => 0,
        },
    };
    let budget = basis.saturating_mul(LINE_READ_BUDGET_FILE_MULTIPLE);
    if budget > LINE_READ_BUDGET_CEILING {
        LINE_READ_BUDGET_CEILING
    } else {
        budget
    }
}

/// [`EncodingCtx::line_cache`]'s guarded state: the per-`(path, line)`
/// memoization table plus the shared disk-read byte budget both are checked
/// and charged against (see [`line_read_budget`]).
#[derive(Debug)]
pub(super) struct LineCacheState {
    /// Memoized line text keyed by `(path, 0-based line)`, `None` meaning
    /// "resolved to no such line". Populated for both the tracker-hit and
    /// disk-read paths (see [`read_line_text`]).
    pub(super) entries: HashMap<(PathBuf, u32), Option<String>>,
    /// Remaining disk-read byte allowance for this response; see
    /// [`line_read_budget`].
    bytes_remaining: u64,
    /// Whether the once-per-response budget-exhausted warning has already
    /// been logged, so a response with many post-exhaustion lookups logs
    /// once rather than once per lookup. Purely a log-dedup flag -- do not
    /// reuse this for [`Self::positions_degraded`] (#497 S1/S2): it is only
    /// set in [`disk_read_line_budgeted`]'s already-exhausted branch, never
    /// for a read that merely couldn't complete *within* the remaining
    /// budget (which still returns `None` and drains the budget, but
    /// through the other branch), and it says nothing about any of the
    /// other ways a position conversion can fall back to an unconverted
    /// column (an unresolvable/non-`file:` URI, a path over
    /// `max_file_size`, invalid UTF-8, or a line past EOF).
    budget_exhausted_logged: bool,
    /// Worst degradation of any position conversion in this response -- set
    /// directly where a column could not be converted exactly
    /// ([`EncodingCtx::to_lsp`]/[`EncodingCtx::to_mcp`]), covering every
    /// cause uniformly (disk-read budget exhaustion, an unresolvable or
    /// non-`file:` URI, a path over `max_file_size`, invalid UTF-8, a line
    /// past EOF, or a column inside a multi-unit character) rather than
    /// only the one `budget_exhausted_logged` covers. See
    /// [`EncodingCtx::positions_degraded`].
    positions_degraded: Option<PositionDegradation>,
}

impl LineCacheState {
    fn new(budget: u64) -> Self {
        Self {
            entries: HashMap::new(),
            bytes_remaining: budget,
            budget_exhausted_logged: false,
            positions_degraded: None,
        }
    }
}

/// [`EncodingCtx::line_cache`]'s field type.
type LineCache = Arc<StdMutex<LineCacheState>>;

/// Builds a fresh, empty [`LineCache`] holding `budget` bytes of disk-read
/// allowance.
fn new_line_cache(budget: u64) -> LineCache {
    Arc::new(StdMutex::new(LineCacheState::new(budget)))
}

/// Per-response encoding context: the negotiated [`PositionEncoding`] of the
/// LSP server that produced a response, used to convert every
/// position/range in that response between MCP's 1-based UTF-16 columns and
/// the server's own 0-based columns.
///
/// A single MCP tool call is always answered by exactly one LSP server, so
/// one context covers every location in its response -- even when
/// individual locations point into other files (e.g. `references` results
/// spanning multiple documents): each conversion resolves the *referenced*
/// file's line text independently rather than assuming it matches the
/// originally queried document.
#[derive(Debug, Clone)]
pub(super) struct EncodingCtx {
    pub(super) encoding: PositionEncoding,
    /// Source of a tracked document's in-memory content -- the text mcpls
    /// actually sent the server via `didOpen`/`didChange` -- consulted
    /// before falling back to disk. See [`read_line_text`].
    pub(super) tracker: Arc<DocumentTracker>,
    /// Snapshot of the configured workspace roots, used only by
    /// [`Self::is_out_of_workspace`] to annotate (never filter) a read-only
    /// navigation result -- see [`WorkspaceRoots::admits_uri`] for why
    /// filtering is deliberately not done here.
    pub(super) workspace_roots: WorkspaceRoots,
    /// Memoizes [`read_line_text`]'s result (both the tracker hit and the
    /// disk-read fallback) per `(path, line)` for the lifetime of this
    /// context, and tracks the shared disk-read byte budget -- one
    /// `EncodingCtx` is built per MCP response (see
    /// [`Translator::encoding_ctx`](super::Translator::encoding_ctx)), so
    /// this bounds a response that reconverts the same file/line many times
    /// (e.g. `references` results clustered in one file) to a single lookup
    /// per distinct line, and caps the response's total disk-read I/O
    /// regardless of how many distinct lines it touches (see #474).
    pub(super) line_cache: LineCache,
}

/// Text of the 0-based `line`'th line of the file at `uri`, or `None` if it
/// cannot be resolved to a path, read, has no such line, or the response's
/// disk-read budget ([`line_read_budget`]) is exhausted.
///
/// Only ever consulted when the negotiated encoding is not UTF-16 (see
/// [`EncodingCtx::to_lsp`]/[`EncodingCtx::to_mcp`]). Every outcome --
/// tracker hit, disk hit, or "no such line" -- is memoized in
/// `ctx.line_cache` per `(path, line)`, so a response reconverting the same
/// line more than once pays for `ctx.tracker.line_text`'s lock/scan or
/// [`DocumentTracker::read_line_checked`]'s disk read only the first time.
///
/// On a cache miss, checks `ctx.tracker` first (in-memory) -- correct even
/// when cached, since it is exactly the text the server was told about and
/// can't diverge from the server's own view within one response's lifetime
/// (see #290 S1: that concern is about disk-vs-tracker divergence across
/// requests, not within one). Only a document the tracker has never seen
/// falls through to [`DocumentTracker::read_line_checked`], which applies
/// the same `ResourceLimits::max_file_size` and regular-file gate as any
/// tracked document's disk read (see #427) while reading only up to the
/// requested line rather than the whole file (see #474) -- gated by the
/// per-response byte budget so that no single response can rack up
/// unbounded disk I/O by naming enough distinct lines.
async fn read_line_text(uri: &lsp_types::Uri, line: u32, ctx: &EncodingCtx) -> Option<String> {
    let path = uri_to_path(uri)?;
    let key = (path.clone(), line);

    if let Some(cached) = lock_std(&ctx.line_cache).entries.get(&key) {
        return cached.clone();
    }

    let text = if let Some(text) = ctx.tracker.line_text(&path, line) {
        Some(text)
    } else {
        disk_read_line_budgeted(&path, line, ctx).await
    };

    lock_std(&ctx.line_cache).entries.insert(key, text.clone());
    text
}

/// [`read_line_text`]'s disk-read fallback, charging the bytes
/// [`DocumentTracker::read_line_checked`] scans against `ctx.line_cache`'s
/// shared [`line_read_budget`] budget. Once exhausted, no
/// further disk reads are attempted for the rest of this response -- every
/// subsequent budget-gated lookup returns `None` immediately, logging a
/// single `warn!` the first time that happens.
///
/// The remaining budget is passed *into* the read itself
/// (`read_line_checked`'s `budget` parameter), which physically bounds how
/// many bytes that call can scan -- so unlike charging only on success,
/// this can't be bypassed by a read that ends in a content-shaped failure
/// (invalid UTF-8 at the target line, a truncation-by-cap, or a path that
/// doesn't resolve via `open_checked` at all -- e.g. an LSP server naming a
/// stdlib path not present locally): `LineRead` reports `bytes_read` on
/// every one of those outcomes too (a small nominal charge, not a literal
/// `0`, for the `open_checked`-failure case -- see
/// `state::OPEN_FAILURE_CHARGE_BYTES`), and this function always charges
/// exactly that. Only a genuine mid-read I/O error (rare, not
/// attacker-controlled by response content) has no byte count available;
/// that one case fails safe by charging this call's whole budget slice
/// rather than leaving it unaccounted (see #474's S1 budget-bypass fix).
async fn disk_read_line_budgeted(path: &Path, line: u32, ctx: &EncodingCtx) -> Option<String> {
    let budget = {
        let mut state = lock_std(&ctx.line_cache);
        if state.bytes_remaining != 0 {
            state.bytes_remaining
        } else {
            let already_logged = state.budget_exhausted_logged;
            state.budget_exhausted_logged = true;
            drop(state);
            if !already_logged {
                tracing::warn!(
                    path = %path.display(),
                    budget_bytes = line_read_budget(ctx.tracker.limits()),
                    "per-response disk-read budget exhausted; further position conversions \
                     requiring a disk read in this response will pass columns through \
                     unconverted"
                );
            }
            return None;
        }
    };

    if let Ok(read) = ctx.tracker.read_line_checked(path, line, budget).await {
        let mut state = lock_std(&ctx.line_cache);
        state.bytes_remaining = state.bytes_remaining.saturating_sub(read.bytes_read);
        drop(state);
        read.text
    } else {
        let mut state = lock_std(&ctx.line_cache);
        state.bytes_remaining = state.bytes_remaining.saturating_sub(budget);
        drop(state);
        None
    }
}

impl EncodingCtx {
    /// Builds a context for one MCP response, deriving the disk-read budget
    /// from `tracker`'s configured limits (see [`line_read_budget`]).
    pub(super) fn new(
        encoding: PositionEncoding,
        tracker: Arc<DocumentTracker>,
        workspace_roots: WorkspaceRoots,
    ) -> Self {
        let line_cache = new_line_cache(line_read_budget(tracker.limits()));
        Self {
            encoding,
            tracker,
            workspace_roots,
            line_cache,
        }
    }

    /// Whether `uri` is *not provably* inside any configured workspace root.
    ///
    /// Advisory only, for a read-only navigation handler to annotate a
    /// result location (`Location::out_of_workspace`,
    /// `HierarchyItem::out_of_workspace`) instead of rejecting it
    /// -- never a safety/security gate. Delegates to
    /// [`WorkspaceRoots::admits_uri`], a purely lexical check over the
    /// canonical roots and their verified aliases: unlike
    /// [`Translator::validate_path`](super::Translator::validate_path) (via
    /// `WorkspaceRoots::validate`), it does **not** canonicalize `uri`. A
    /// location that reaches a workspace root through a symlink other than a
    /// recorded alias (e.g. a package manager's symlinked dependency store)
    /// can therefore come back `true` even though `validate_path` would
    /// accept the same path -- the two checks are not equivalent, and this
    /// one is never used to decide what mcpls will open or read.
    ///
    /// Also always `true` when no workspace roots are configured at all
    /// (empty `workspace_roots`, e.g. a library embedder that never called
    /// `Translator::set_workspace_roots`): without a configured root,
    /// nothing can be vouched for as inside the workspace, so every location
    /// is honestly reported as not provably contained.
    pub(super) fn is_out_of_workspace(&self, uri: &lsp_types::Uri) -> bool {
        !self.workspace_roots.admits_uri(uri)
    }

    /// Worst degradation among the position conversions made through this
    /// context so far, or `None` if every column converted exactly --
    /// surfaced to the caller via a `positions_degraded` field on the
    /// affected result DTOs (#497).
    ///
    /// A failed [`Self::to_lsp`] is [`PositionDegradation::Request`] (the
    /// queried position itself may be wrong), a failed [`Self::to_mcp`] is
    /// [`PositionDegradation::Response`]; `Request` subsumes `Response`.
    /// Sticky for the context's lifetime -- once set, it never improves, even
    /// if a later lookup for a *different* `(path, line)` succeeds.
    pub(super) fn positions_degraded(&self) -> Option<PositionDegradation> {
        lock_std(&self.line_cache).positions_degraded
    }

    /// Records `degradation`, returning whether it raised the recorded level
    /// (so callers log once per level rather than per position).
    fn record_degradation(&self, degradation: PositionDegradation) -> bool {
        let mut state = lock_std(&self.line_cache);
        let raised = state.positions_degraded < Some(degradation);
        state.positions_degraded = state.positions_degraded.max(Some(degradation));
        raised
    }

    /// Convert an MCP position for the document at `uri` into an LSP
    /// position in this context's negotiated encoding.
    ///
    /// Column 0 (MCP `character <= 1`) never needs line text, so it skips the
    /// lookup entirely.
    pub(super) async fn to_lsp(
        &self,
        uri: &lsp_types::Uri,
        position: Position,
    ) -> lsp_types::Position {
        let line_text =
            if self.encoding == PositionEncoding::Utf16 || position.character().get() <= 1 {
                None
            } else {
                read_line_text(uri, position.lsp_line(), self).await
            };
        let converted = mcp_to_lsp_position(position, line_text.as_deref(), self.encoding);
        if converted.fidelity == ColumnFidelity::PassedThrough
            && self.record_degradation(PositionDegradation::Request)
        {
            tracing::warn!(
                uri = uri.as_ref(),
                line = position.line().get(),
                encoding = self.encoding.to_lsp(),
                "could not convert MCP column exactly; passing it through unconverted, which \
                 is wrong for a non-UTF-16 server (logged once per response)"
            );
        }
        converted.value
    }

    /// Convert an LSP position (in this context's negotiated encoding) from
    /// the document at `uri` into an MCP position.
    ///
    /// Column 0 and the `u32::MAX` end-of-line sentinel never need line text,
    /// so they skip the lookup entirely.
    pub(super) async fn to_mcp(
        &self,
        uri: &lsp_types::Uri,
        pos: lsp_types::Position,
    ) -> Position2D {
        let needs_text = self.encoding != PositionEncoding::Utf16
            && pos.character != 0
            && pos.character != u32::MAX;
        let line_text = if needs_text {
            read_line_text(uri, pos.line, self).await
        } else {
            None
        };
        let converted = lsp_to_mcp_position(pos, line_text.as_deref(), self.encoding);
        if converted.fidelity == ColumnFidelity::PassedThrough
            && self.record_degradation(PositionDegradation::Response)
        {
            tracing::warn!(
                uri = uri.as_ref(),
                line = pos.line,
                encoding = self.encoding.to_lsp(),
                "could not convert server column exactly; passing it through unconverted, \
                 which is wrong for a non-UTF-16 server (logged once per response)"
            );
        }
        converted.value
    }

    /// Convert an LSP range (in this context's negotiated encoding) from the
    /// document at `uri` into an MCP range.
    pub(super) async fn normalize_range(
        &self,
        uri: &lsp_types::Uri,
        range: lsp_types::Range,
    ) -> Range {
        Range {
            start: self.to_mcp(uri, range.start).await,
            end: self.to_mcp(uri, range.end).await,
        }
    }

    /// Convert an MCP range for the document at `uri` back into an LSP range
    /// in this context's negotiated encoding -- the inverse of
    /// [`Self::normalize_range`].
    pub(super) async fn denormalize_range(
        &self,
        uri: &lsp_types::Uri,
        range: PositionRange,
    ) -> lsp_types::Range {
        lsp_types::Range {
            start: self.to_lsp(uri, range.start()).await,
            end: self.to_lsp(uri, range.end()).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::bridge::path_to_uri;
    use crate::bridge::state::ResourceLimits;
    use crate::bridge::translator::testing::*;
    use crate::config::{DocumentLimit, SizeLimit};

    #[test]
    fn test_is_out_of_workspace_false_when_uri_inside_configured_root() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let uri = path_to_uri(&path).unwrap();

        let ctx = test_ctx_with_roots(
            PositionEncoding::Utf16,
            WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap(),
        );
        assert!(!ctx.is_out_of_workspace(&uri));
    }

    /// #558: a location under an alias spelling of a root is inside it.
    #[cfg(unix)]
    #[test]
    fn test_is_out_of_workspace_false_under_alias_spelling() {
        let dir = TempDir::new().unwrap();
        let base = dunce::canonicalize(dir.path()).unwrap();
        let real = base.join("real");
        fs::create_dir(&real).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let roots = WorkspaceRoots::from_configured(std::slice::from_ref(&link)).unwrap();
        let ctx = test_ctx_with_roots(PositionEncoding::Utf16, roots);

        assert!(!ctx.is_out_of_workspace(&path_to_uri(&link.join("main.rs")).unwrap()));
        assert!(!ctx.is_out_of_workspace(&path_to_uri(&real.join("main.rs")).unwrap()));
        assert!(ctx.is_out_of_workspace(&path_to_uri(&base.join("other/main.rs")).unwrap()));
    }

    /// #605: a server location under the `/tmp` spelling of a root configured
    /// as `/private/tmp/..` is inside it.
    #[cfg(target_os = "macos")]
    #[test]
    fn test_is_out_of_workspace_false_under_system_symlink_alias() {
        let dir = TempDir::new_in("/tmp").unwrap();
        let canonical = dunce::canonicalize(dir.path()).unwrap();
        assert!(canonical.starts_with("/private/tmp"));
        let roots = WorkspaceRoots::from_configured(&[canonical]).unwrap();
        let ctx = test_ctx_with_roots(PositionEncoding::Utf16, roots);

        let alias_spelling = dir.path().join("a.rs");
        assert!(alias_spelling.starts_with("/tmp"));
        assert!(!ctx.is_out_of_workspace(&path_to_uri(&alias_spelling).unwrap()));
    }

    #[test]
    fn test_is_out_of_workspace_true_when_uri_outside_configured_roots() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let uri = path_to_uri(&path).unwrap();

        let other_dir = TempDir::new().unwrap();
        let ctx = test_ctx_with_roots(
            PositionEncoding::Utf16,
            WorkspaceRoots::from_configured(&[other_dir.path().to_path_buf()]).unwrap(),
        );
        assert!(ctx.is_out_of_workspace(&uri));
    }

    /// Matches [`WorkspaceRoots::admits_uri`]'s fail-closed
    /// convention -- see `is_out_of_workspace`'s doc for why an unconfigured
    /// workspace makes every location report as not provably contained.
    #[test]
    fn test_is_out_of_workspace_true_when_no_roots_configured() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let uri = path_to_uri(&path).unwrap();

        let ctx = test_ctx_with_roots(PositionEncoding::Utf16, WorkspaceRoots::default());
        assert!(ctx.is_out_of_workspace(&uri));
    }

    #[tokio::test]
    async fn test_normalize_range() {
        let lsp_range = lsp_types::Range {
            start: lsp_types::Position {
                line: 0,
                character: 0,
            },
            end: lsp_types::Position {
                line: 2,
                character: 5,
            },
        };

        let mcp_range = test_ctx().normalize_range(&test_uri(), lsp_range).await;
        assert_eq!(mcp_range.start.line, 1);
        assert_eq!(mcp_range.start.character, 1);
        assert_eq!(mcp_range.end.line, 3);
        assert_eq!(mcp_range.end.character, 6);
    }

    /// End-to-end proof that a non-UTF-16 `EncodingCtx` is actually wired to
    /// `read_line_text`/disk, not just correct in isolation at the
    /// `encoding.rs` function level: a real temp file with a multibyte line
    /// ("héllo"), converted through `EncodingCtx::to_lsp` for a document the
    /// tracker has never seen (forcing the disk-read fallback).
    #[tokio::test]
    async fn test_encoding_ctx_utf8_reads_disk_line_text_for_untracked_document() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("multibyte.rs");
        fs::write(&path, "héllo").unwrap();
        let uri = path_to_uri(&path).unwrap();

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        let lsp_pos = ctx.to_lsp(&uri, Position::at(1, 3)).await;
        // "hé" is 3 bytes in UTF-8 (h=1, é=2); MCP column 3 (UTF-16, after
        // "hé") must re-derive to that byte offset via the disk-read line
        // text, matching the `encoding.rs`-level math for the same input.
        assert_eq!(lsp_pos.character, 3);
    }

    /// Regression for #427: `read_line_text`'s disk-read fallback for a
    /// document `tracker` has never seen must respect
    /// `ResourceLimits::max_file_size`, not read the file unbounded. Without
    /// the fix, a server-supplied path outside any tracked document (e.g. one
    /// reached only through position-encoding conversion) could be read in
    /// full regardless of size.
    #[tokio::test]
    async fn test_read_line_text_enforces_max_file_size_for_untracked_document() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.rs");
        fs::write(&path, "a".repeat(200)).unwrap();
        let uri = path_to_uri(&path).unwrap();

        let ctx = EncodingCtx::new(
            PositionEncoding::Utf8,
            Arc::new(DocumentTracker::new(
                ResourceLimits {
                    max_documents: DocumentLimit::new(100),
                    max_file_size: SizeLimit::from_static(50),
                },
                HashMap::new(),
            )),
            WorkspaceRoots::default(),
        );
        assert!(
            read_line_text(&uri, 0, &ctx).await.is_none(),
            "must refuse to return content from a file over max_file_size"
        );
    }

    /// C3/S1: when a document is tracked, `EncodingCtx` must prefer its
    /// in-memory content over disk -- both cheaper (no I/O) and more correct
    /// when they've diverged. Here disk holds stale ASCII ("hello", no
    /// accent) while the tracker holds the live multibyte content
    /// ("héllo"); if conversion used disk instead, MCP column 3 would
    /// re-derive to LSP byte offset 2 (ASCII, no multibyte char) instead of
    /// 3 (multibyte-correct) -- so this distinguishes the two sources rather
    /// than merely tolerating either.
    #[tokio::test]
    async fn test_encoding_ctx_utf8_prefers_tracked_content_over_stale_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tracked.rs");
        fs::write(&path, "hello").unwrap(); // stale: no accent

        let tracker = Arc::new(DocumentTracker::new(
            ResourceLimits::default(),
            HashMap::new(),
        ));
        let uri = tracker.open(path.clone(), "héllo".to_string()).unwrap(); // live: accent

        let ctx = EncodingCtx::new(PositionEncoding::Utf8, tracker, WorkspaceRoots::default());
        let lsp_pos = ctx.to_lsp(&uri, Position::at(1, 3)).await;
        assert_eq!(
            lsp_pos.character, 3,
            "must convert against the tracker's live content (\"héllo\" -> byte 3), not disk's \
             stale content (\"hello\" -> byte 2)"
        );
    }

    /// A single `EncodingCtx` answering one MCP tool call may still need to
    /// convert positions in several different files (e.g. `references`
    /// results spanning multiple documents) -- each conversion must resolve
    /// *that* location's own file, never reuse or leak another file's line
    /// text. Two untracked files with different content at the same
    /// byte offset make a wrong-file conversion produce a visibly different
    /// (wrong) answer: byte offset 3 is UTF-16 column 3 in "héllo" but
    /// column 4 in the all-ASCII "hello".
    #[tokio::test]
    async fn test_normalize_range_multi_file_converts_each_location_against_its_own_uri() {
        let dir = TempDir::new().unwrap();
        let path_a = dir.path().join("a.rs");
        fs::write(&path_a, "héllo").unwrap();
        let uri_a = path_to_uri(&path_a).unwrap();

        let path_b = dir.path().join("b.rs");
        fs::write(&path_b, "hello").unwrap();
        let uri_b = path_to_uri(&path_b).unwrap();

        let lsp_range = lsp_types::Range {
            start: lsp_types::Position {
                line: 0,
                character: 0,
            },
            end: lsp_types::Position {
                line: 0,
                character: 3,
            },
        };

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        let range_a = ctx.normalize_range(&uri_a, lsp_range).await;
        let range_b = ctx.normalize_range(&uri_b, lsp_range).await;

        assert_eq!(
            range_a.end.character, 3,
            "must convert against a.rs's own content"
        );
        assert_eq!(
            range_b.end.character, 4,
            "must convert against b.rs's own content"
        );
    }

    /// Regression for #474: a single `EncodingCtx` must memoize
    /// [`read_line_text`]'s disk-read fallback per `(path, line)`, so a
    /// response that reconverts the same untracked file's line more than
    /// once (e.g. several `references` locations on one line) reads disk
    /// only the first time. Proven by mutating the file between two lookups
    /// through the same `ctx`: if the second lookup re-read disk, it would
    /// observe the new content instead of the cached one.
    #[tokio::test]
    async fn test_read_line_text_caches_disk_read_per_path_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cached.rs");
        fs::write(&path, "hello").unwrap();
        let uri = path_to_uri(&path).unwrap();

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        assert_eq!(
            read_line_text(&uri, 0, &ctx).await.as_deref(),
            Some("hello")
        );

        fs::write(&path, "héllo").unwrap();
        assert_eq!(
            read_line_text(&uri, 0, &ctx).await.as_deref(),
            Some("hello"),
            "must reuse the first lookup's cached result instead of re-reading disk"
        );
    }

    /// Regression for S1: the per-`(path, line)` cache alone doesn't bound a
    /// response naming enough *distinct* lines/files -- `read_line_text`
    /// must also stop performing disk reads once the shared per-response
    /// byte budget is spent, refusing further lookups rather than letting
    /// each new distinct key add unbounded I/O.
    #[tokio::test]
    async fn test_read_line_text_stops_disk_reads_once_budget_exhausted() {
        let dir = TempDir::new().unwrap();
        let path_a = dir.path().join("a.rs");
        fs::write(&path_a, "hello\n").unwrap();
        let uri_a = path_to_uri(&path_a).unwrap();

        let path_b = dir.path().join("b.rs");
        fs::write(&path_b, "world\n").unwrap();
        let uri_b = path_to_uri(&path_b).unwrap();

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        // Exactly enough budget for the first read ("hello\n" is 6 bytes) to
        // complete, but nothing left after.
        lock_std(&ctx.line_cache).bytes_remaining = 6;

        assert_eq!(
            read_line_text(&uri_a, 0, &ctx).await.as_deref(),
            Some("hello")
        );
        assert_eq!(lock_std(&ctx.line_cache).bytes_remaining, 0);

        // A second, distinct (path, line) lookup must now be refused.
        assert_eq!(read_line_text(&uri_b, 0, &ctx).await, None);
    }

    /// Regression for the S1 budget-bypass fix: a read whose remaining
    /// budget is smaller than the line it's scanning for must stop at
    /// exactly the budget (never returning the truncated text as if it
    /// were complete), and must still charge exactly what it scanned --
    /// proven by draining the budget to zero rather than leaving any
    /// unaccounted.
    #[tokio::test]
    async fn test_read_line_text_bounds_read_by_remaining_budget() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("long_line.rs");
        fs::write(&path, "a".repeat(1000)).unwrap();
        let uri = path_to_uri(&path).unwrap();

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        lock_std(&ctx.line_cache).bytes_remaining = 10;

        assert_eq!(
            read_line_text(&uri, 0, &ctx).await,
            None,
            "a line far longer than the remaining budget must not be returned"
        );
        assert_eq!(
            lock_std(&ctx.line_cache).bytes_remaining,
            0,
            "the physically-capped read must charge (at most one byte over) the budget it was \
             given, not overshoot to max_file_size"
        );
    }

    /// Regression for #497 S1: `positions_degraded()` must become `true` on
    /// the very first lookup that falls back to an unconverted column, even
    /// when that lookup's *own* remaining budget was nonzero going in
    /// (merely insufficient for the line it needed) -- not only once the
    /// budget has already been fully drained to zero by some earlier
    /// lookup. Before this fix, `positions_degraded()` read
    /// `budget_exhausted_logged`, which `disk_read_line_budgeted` only sets
    /// in its *already-exhausted* branch, so this exact case (a single
    /// `to_lsp` call against a too-long line) silently reported
    /// `positions_degraded: false`.
    #[tokio::test]
    async fn test_positions_degraded_true_on_first_lookup_with_insufficient_remaining_budget() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("long_line.rs");
        fs::write(&path, "a".repeat(1000)).unwrap();
        let uri = path_to_uri(&path).unwrap();

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        lock_std(&ctx.line_cache).bytes_remaining = 10;

        assert_eq!(ctx.positions_degraded(), None, "no lookup has happened yet");

        ctx.to_lsp(&uri, Position::at(1, 3)).await;

        assert_eq!(
            ctx.positions_degraded(),
            Some(PositionDegradation::Request),
            "a line too long for the remaining budget must mark positions_degraded, even on \
             the very first such lookup"
        );
    }

    /// Regression for #497 S2: `positions_degraded()` must reflect every
    /// cause of a `to_lsp`/`to_mcp` unconverted-column fallback, not only
    /// disk-read budget exhaustion -- an unresolvable path (e.g.
    /// rust-analyzer naming a stdlib location without `rust-src` installed)
    /// is the more common real-world case, and shares none of
    /// `disk_read_line_budgeted`'s budget bookkeeping.
    #[tokio::test]
    async fn test_positions_degraded_true_for_nonexistent_path() {
        let dir = TempDir::new().unwrap();
        let uri = path_to_uri(&dir.path().join("does_not_exist.rs")).unwrap();

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        assert_eq!(ctx.positions_degraded(), None);

        ctx.to_mcp(
            &uri,
            lsp_types::Position {
                line: 0,
                character: 1,
            },
        )
        .await;

        assert_eq!(
            ctx.positions_degraded(),
            Some(PositionDegradation::Response)
        );
    }

    fn lsp_position(line: u32, character: u32) -> lsp_types::Position {
        lsp_types::Position { line, character }
    }

    /// Column 0 is identical in every encoding, so an unresolvable line must
    /// not be reported as degraded for it -- and must not cost any I/O.
    #[tokio::test]
    async fn test_column_zero_on_unresolvable_line_is_not_degraded_and_skips_io() {
        let dir = TempDir::new().unwrap();
        let uri = path_to_uri(&dir.path().join("does_not_exist.rs")).unwrap();

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        let before = lock_std(&ctx.line_cache).bytes_remaining;

        assert_eq!(ctx.to_lsp(&uri, pos(3, 1)).await.character, 0);
        assert_eq!(ctx.to_mcp(&uri, lsp_position(2, 0)).await.character, 1);

        assert_eq!(ctx.positions_degraded(), None);
        assert_eq!(lock_std(&ctx.line_cache).bytes_remaining, before);
        assert!(lock_std(&ctx.line_cache).entries.is_empty());
    }

    #[tokio::test]
    async fn test_to_lsp_nonzero_column_on_unresolvable_line_is_request() {
        let dir = TempDir::new().unwrap();
        let uri = path_to_uri(&dir.path().join("does_not_exist.rs")).unwrap();

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        let _ = ctx.to_lsp(&uri, pos(1, 2)).await;
        assert_eq!(ctx.positions_degraded(), Some(PositionDegradation::Request));
    }

    #[tokio::test]
    async fn test_request_degradation_subsumes_response_in_either_order() {
        let dir = TempDir::new().unwrap();
        let uri = path_to_uri(&dir.path().join("does_not_exist.rs")).unwrap();

        let response_first = test_ctx_with(PositionEncoding::Utf8);
        let _ = response_first.to_mcp(&uri, lsp_position(0, 3)).await;
        assert_eq!(
            response_first.positions_degraded(),
            Some(PositionDegradation::Response)
        );
        let _ = response_first.to_lsp(&uri, pos(1, 3)).await;
        assert_eq!(
            response_first.positions_degraded(),
            Some(PositionDegradation::Request)
        );

        let request_first = test_ctx_with(PositionEncoding::Utf8);
        let _ = request_first.to_lsp(&uri, pos(1, 3)).await;
        let _ = request_first.to_mcp(&uri, lsp_position(0, 3)).await;
        assert_eq!(
            request_first.positions_degraded(),
            Some(PositionDegradation::Request)
        );
    }

    fn tracked_ctx(content: &str) -> (EncodingCtx, lsp_types::Uri, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tracked.rs");
        let tracker = Arc::new(DocumentTracker::new(
            ResourceLimits::default(),
            HashMap::new(),
        ));
        let uri = tracker.open(path, content.to_string()).unwrap();
        let ctx = EncodingCtx::new(PositionEncoding::Utf8, tracker, WorkspaceRoots::default());
        (ctx, uri, dir)
    }

    /// A column past the end of a tracked line clamps exactly: `"éééé"` is 8
    /// bytes, so MCP column 6 (UTF-16 offset 5, past the 4 units) is byte 8.
    #[tokio::test]
    async fn test_to_lsp_past_end_of_tracked_line_clamps_without_degradation() {
        let (ctx, uri, _dir) = tracked_ctx("éééé");
        assert_eq!(ctx.to_lsp(&uri, pos(1, 6)).await.character, 8);
        assert_eq!(ctx.positions_degraded(), None);
    }

    /// The empty line after a final newline exists in the tracker, so a
    /// position on it converts exactly.
    #[tokio::test]
    async fn test_to_lsp_on_trailing_empty_line_is_exact() {
        let (ctx, uri, _dir) = tracked_ctx("fn main() {}\n");
        assert_eq!(ctx.to_lsp(&uri, pos(2, 1)).await.character, 0);
        assert_eq!(ctx.to_lsp(&uri, pos(2, 4)).await.character, 0);
        assert_eq!(ctx.positions_degraded(), None);
    }

    #[tokio::test]
    async fn test_to_lsp_mid_surrogate_column_is_request() {
        let (ctx, uri, _dir) = tracked_ctx("𝄞x");
        let _ = ctx.to_lsp(&uri, pos(1, 2)).await;
        assert_eq!(ctx.positions_degraded(), Some(PositionDegradation::Request));
    }

    #[tokio::test]
    async fn test_to_mcp_end_of_line_sentinel_is_not_degraded_and_skips_io() {
        let dir = TempDir::new().unwrap();
        let uri = path_to_uri(&dir.path().join("does_not_exist.rs")).unwrap();

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        assert_eq!(
            ctx.to_mcp(&uri, lsp_position(0, u32::MAX)).await.character,
            u32::MAX
        );
        assert_eq!(ctx.positions_degraded(), None);
        assert!(lock_std(&ctx.line_cache).entries.is_empty());
    }

    /// Regression for the S1 budget-bypass fix (security re-audit): an
    /// invalid-UTF-8 line -- the realistic attack shape (a `.rlib`, image,
    /// or pack file under `max_file_size`) -- must still charge the shared
    /// per-response budget for the bytes actually scanned, not leave it
    /// unaccounted because the line failed to decode. Before this fix, this
    /// exact case charged zero, letting a hostile response repeat it over
    /// enough distinct `(path, line)` keys to restore unbounded scanning.
    #[tokio::test]
    async fn test_read_line_text_charges_budget_even_when_line_is_invalid_utf8() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("invalid_utf8.rs");
        let mut content = vec![0xFFu8, 0xFE, 0xFD];
        content.push(b'\n');
        fs::write(&path, &content).unwrap();
        let uri = path_to_uri(&path).unwrap();

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        assert_eq!(read_line_text(&uri, 0, &ctx).await, None);
        assert_eq!(
            lock_std(&ctx.line_cache).bytes_remaining,
            line_read_budget(ResourceLimits::default()) - content.len() as u64,
            "the budget must be charged for the bytes scanned even though the line was not \
             valid UTF-8"
        );
    }

    /// Regression for the open-failure-charge fix: an LSP server routinely
    /// names a path that doesn't exist locally (e.g. rust-analyzer's
    /// `file:///rustc/<hash>/library/...` stdlib locations without
    /// `rust-src` installed) -- a completely normal, non-attacker scenario.
    /// This must charge only the small nominal `OPEN_FAILURE_CHARGE_BYTES`
    /// amount, not the previous round's regression of zeroing the *entire*
    /// remaining per-response budget on the very first such location.
    #[tokio::test]
    async fn test_read_line_text_charges_nominal_amount_for_nonexistent_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rustc_stdlib_without_rust_src.rs");
        let uri = path_to_uri(&path).unwrap();

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        assert_eq!(read_line_text(&uri, 0, &ctx).await, None);
        assert_eq!(
            lock_std(&ctx.line_cache).bytes_remaining,
            line_read_budget(ResourceLimits::default())
                - crate::bridge::state::OPEN_FAILURE_CHARGE_BYTES,
            "a nonexistent path must charge only the small nominal amount, not the whole budget"
        );
    }

    #[test]
    fn test_line_read_budget_scales_with_max_file_size() {
        let budget = |max_file_size| {
            line_read_budget(ResourceLimits {
                max_documents: DocumentLimit::new(1),
                max_file_size: SizeLimit::from_static(max_file_size),
            })
        };
        assert_eq!(budget(1000), 4000);
        assert_eq!(budget(1000 * 1024), 4 * 1000 * 1024);
    }

    #[test]
    fn test_line_read_budget_is_capped_at_ceiling() {
        let budget = |max_file_size| {
            line_read_budget(ResourceLimits {
                max_documents: DocumentLimit::new(1),
                max_file_size: SizeLimit::from_static(max_file_size),
            })
        };
        assert_eq!(
            budget(crate::config::MAX_FILE_SIZE_LIMIT),
            LINE_READ_BUDGET_CEILING
        );
        assert_eq!(
            budget(LINE_READ_BUDGET_CEILING / 4),
            LINE_READ_BUDGET_CEILING
        );
    }

    #[test]
    fn test_line_read_budget_zero_limit_falls_back_to_default() {
        let budget = line_read_budget(ResourceLimits {
            max_documents: DocumentLimit::new(1),
            max_file_size: SizeLimit::UNLIMITED,
        });
        assert_eq!(budget, 4 * 10 * 1024 * 1024);
    }

    #[test]
    fn test_encoding_ctx_new_derives_budget_from_tracker_limits() {
        let tracker = Arc::new(DocumentTracker::new(
            ResourceLimits {
                max_documents: DocumentLimit::new(1),
                max_file_size: SizeLimit::from_static(4096),
            },
            HashMap::new(),
        ));
        let ctx = EncodingCtx::new(PositionEncoding::Utf8, tracker, WorkspaceRoots::default());
        assert_eq!(lock_std(&ctx.line_cache).bytes_remaining, 4 * 4096);
    }
}
