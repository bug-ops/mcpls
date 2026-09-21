//! Per-response position/range encoding conversion between MCP's 1-based
//! UTF-16 columns and an LSP server's negotiated encoding.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use super::dto::{Position2D, Range};
use crate::bridge::encoding::{PositionEncoding, lsp_to_mcp_position, mcp_to_lsp_position};
use crate::bridge::state::{DEFAULT_MAX_FILE_SIZE, uri_to_path};
use crate::bridge::{DocumentTracker, lock_std};

/// Total bytes [`read_line_text`]'s disk-read fallback (via
/// [`DocumentTracker::read_line_checked`]) may scan across one
/// `EncodingCtx`'s whole lifetime (one MCP response), independent of how
/// many distinct `(path, line)` lookups that spans.
///
/// [`LineCacheState::entries`] alone caps repeats of the *same* line, but a
/// response naming enough distinct lines (e.g. `references` results spread
/// across a large file, or several call-hierarchy/inlay-hint/workspace-edit
/// locations) could still add up to an unbounded amount of scanning even
/// with that cache and [`super::navigation::MAX_NORMALIZED_LOCATIONS`]'s
/// count cap in place (see #474's follow-up). Every conversion that reaches
/// disk goes through [`read_line_text`], so charging this single budget
/// there caps every `EncodingCtx`-mediated handler uniformly -- `to_lsp`,
/// `to_mcp`, `normalize_range`, `denormalize_range` -- with no per-handler
/// cap needed.
///
/// Set to four times the default single-file read bound: enough slack for a
/// legitimate response touching a handful of large files, while still
/// bounding a hostile response to double-digit MiB of I/O rather than the
/// unbounded (or count-cap x `max_file_size`) amount possible without it.
/// This is a fixed constant, not derived from the tracker's *configured*
/// `ResourceLimits::max_file_size` -- deliberately: `read_line_checked`'s
/// `budget` parameter always caps an individual read to
/// `min(bounded_read_cap(configured_max_file_size), remaining_budget)`, so a
/// larger configured `max_file_size` (including `0`, meaning unlimited)
/// only widens what *one* read is theoretically allowed to scan before
/// finding its line, never what it can actually charge against this
/// response-wide budget -- the physical cap always wins.
const MAX_LINE_READ_BYTES_PER_RESPONSE: u64 = 4 * DEFAULT_MAX_FILE_SIZE;

/// [`EncodingCtx::line_cache`]'s guarded state: the per-`(path, line)`
/// memoization table plus the shared disk-read byte budget both are checked
/// and charged against (see [`MAX_LINE_READ_BYTES_PER_RESPONSE`]).
#[derive(Debug)]
pub(super) struct LineCacheState {
    /// Memoized line text keyed by `(path, 0-based line)`, `None` meaning
    /// "resolved to no such line". Populated for both the tracker-hit and
    /// disk-read paths (see [`read_line_text`]).
    pub(super) entries: HashMap<(PathBuf, u32), Option<String>>,
    /// Remaining disk-read byte allowance for this response; see
    /// [`MAX_LINE_READ_BYTES_PER_RESPONSE`].
    bytes_remaining: u64,
    /// Whether the once-per-response budget-exhausted warning has already
    /// been logged, so a response with many post-exhaustion lookups logs
    /// once rather than once per lookup.
    budget_exhausted_logged: bool,
}

impl LineCacheState {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            bytes_remaining: MAX_LINE_READ_BYTES_PER_RESPONSE,
            budget_exhausted_logged: false,
        }
    }
}

/// [`EncodingCtx::line_cache`]'s field type.
type LineCache = Arc<StdMutex<LineCacheState>>;

/// Builds a fresh, empty [`LineCache`] for a new [`EncodingCtx`] -- used by
/// every construction site so the budget/cache initialization can't drift
/// between them.
pub(super) fn new_line_cache() -> LineCache {
    Arc::new(StdMutex::new(LineCacheState::new()))
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
    /// navigation result -- see `crate::bridge::uri_in_workspace_roots`'s
    /// docs for why filtering is deliberately not done here.
    pub(super) workspace_roots: Arc<Vec<PathBuf>>,
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
/// disk-read budget ([`MAX_LINE_READ_BYTES_PER_RESPONSE`]) is exhausted.
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
/// shared [`MAX_LINE_READ_BYTES_PER_RESPONSE`] budget. Once exhausted, no
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
                    budget_bytes = MAX_LINE_READ_BYTES_PER_RESPONSE,
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
    /// Whether `uri` is *not provably* inside any configured workspace root.
    ///
    /// Advisory only, for a read-only navigation handler to annotate a
    /// result location (`Location::out_of_workspace`,
    /// `CallHierarchyItemResult::out_of_workspace`) instead of rejecting it
    /// -- never a safety/security gate. Delegates to
    /// [`crate::bridge::uri_in_workspace_roots`], which is a purely lexical
    /// `starts_with` check: unlike
    /// [`Translator::validate_path`](super::Translator::validate_path) (via
    /// `validate_path_against_roots`), it does **not** canonicalize `uri` or
    /// the configured roots first. A location that resolves to a workspace
    /// root through a symlink (e.g. macOS's `/var` -> `/private/var`, or a
    /// package manager's symlinked dependency store) can therefore come back
    /// `true` even though `validate_path` would accept the same path -- the
    /// two checks are not equivalent, and this one is never used to decide
    /// what mcpls will open or read.
    ///
    /// Also always `true` when no workspace roots are configured at all
    /// (empty `workspace_roots`, e.g. a library embedder that never called
    /// `Translator::set_workspace_roots`), consistent with
    /// `uri_in_workspace_roots`'s fail-closed convention (see its doc
    /// comment): without a configured root, nothing can be vouched for as
    /// inside the workspace, so every location is honestly reported as not
    /// provably contained.
    pub(super) fn is_out_of_workspace(&self, uri: &lsp_types::Uri) -> bool {
        !crate::bridge::uri_in_workspace_roots(uri, &self.workspace_roots)
    }

    /// Convert an MCP position for the document at `uri` into an LSP
    /// position in this context's negotiated encoding.
    pub(super) async fn to_lsp(
        &self,
        uri: &lsp_types::Uri,
        line: u32,
        character: u32,
    ) -> lsp_types::Position {
        let line_text = if self.encoding == PositionEncoding::Utf16 {
            None
        } else {
            let text = read_line_text(uri, line.saturating_sub(1), self).await;
            if text.is_none() {
                tracing::warn!(
                    uri = uri.as_ref(),
                    line,
                    encoding = self.encoding.to_lsp(),
                    "could not resolve line text for position conversion; passing MCP column \
                     through unconverted, which is wrong for a non-UTF-16 server"
                );
            }
            text
        };
        mcp_to_lsp_position(line, character, line_text.as_deref(), self.encoding)
    }

    /// Convert an LSP position (in this context's negotiated encoding) from
    /// the document at `uri` into an MCP position.
    pub(super) async fn to_mcp(
        &self,
        uri: &lsp_types::Uri,
        pos: lsp_types::Position,
    ) -> Position2D {
        let line_text = if self.encoding == PositionEncoding::Utf16 {
            None
        } else {
            let text = read_line_text(uri, pos.line, self).await;
            if text.is_none() {
                tracing::warn!(
                    uri = uri.as_ref(),
                    line = pos.line,
                    encoding = self.encoding.to_lsp(),
                    "could not resolve line text for position conversion; passing server \
                     column through unconverted, which is wrong for a non-UTF-16 server"
                );
            }
            text
        };
        let (line, character) = lsp_to_mcp_position(pos, line_text.as_deref(), self.encoding);
        Position2D { line, character }
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
        range: &Range,
    ) -> lsp_types::Range {
        lsp_types::Range {
            start: self
                .to_lsp(uri, range.start.line, range.start.character)
                .await,
            end: self.to_lsp(uri, range.end.line, range.end.character).await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::bridge::path_to_uri;
    use crate::bridge::state::ResourceLimits;
    use crate::bridge::translator::testing::*;

    #[test]
    fn test_is_out_of_workspace_false_when_uri_inside_configured_root() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let uri = path_to_uri(&path).unwrap();

        let ctx = test_ctx_with_roots(PositionEncoding::Utf16, vec![dir.path().to_path_buf()]);
        assert!(!ctx.is_out_of_workspace(&uri));
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
            vec![other_dir.path().to_path_buf()],
        );
        assert!(ctx.is_out_of_workspace(&uri));
    }

    /// Matches [`crate::bridge::uri_in_workspace_roots`]'s fail-closed
    /// convention -- see `is_out_of_workspace`'s doc for why an unconfigured
    /// workspace makes every location report as not provably contained.
    #[test]
    fn test_is_out_of_workspace_true_when_no_roots_configured() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let uri = path_to_uri(&path).unwrap();

        let ctx = test_ctx_with_roots(PositionEncoding::Utf16, Vec::new());
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
        let lsp_pos = ctx.to_lsp(&uri, 1, 3).await;
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

        let ctx = EncodingCtx {
            encoding: PositionEncoding::Utf8,
            tracker: Arc::new(DocumentTracker::new(
                ResourceLimits {
                    max_documents: 100,
                    max_file_size: 50,
                },
                HashMap::new(),
            )),
            workspace_roots: Arc::new(Vec::new()),
            line_cache: new_line_cache(),
        };
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

        let ctx = EncodingCtx {
            encoding: PositionEncoding::Utf8,
            tracker,
            workspace_roots: Arc::new(Vec::new()),
            line_cache: new_line_cache(),
        };
        let lsp_pos = ctx.to_lsp(&uri, 1, 3).await;
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
            MAX_LINE_READ_BYTES_PER_RESPONSE - content.len() as u64,
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
            MAX_LINE_READ_BYTES_PER_RESPONSE - crate::bridge::state::OPEN_FAILURE_CHARGE_BYTES,
            "a nonexistent path must charge only the small nominal amount, not the whole budget"
        );
    }
}
