//! Document state management.
//!
//! Tracks open documents and their versions for LSP synchronization.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};

use lsp_types::{
    DidChangeTextDocumentNotification, DidChangeTextDocumentParams,
    DidCloseTextDocumentNotification, DidCloseTextDocumentParams, DidOpenTextDocumentNotification,
    DidOpenTextDocumentParams, TextDocumentContentChangeEvent, TextDocumentItem, Uri,
    VersionedTextDocumentIdentifier,
};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio::time::Instant;
use url::Url;

use super::lock_std;
use crate::config::ServerId;
use crate::error::{Error, Result};
use crate::lsp::LspClient;
use crate::util::{BoundedReadOutcome, bounded_read_cap, check_bounded_utf8};

/// Debounce window for re-reading a file's content when its mtime is not yet
/// [`mtime_settled`]. The stat itself is never debounced -- only this
/// (comparatively expensive) content re-read is rate-limited, so a burst of
/// calls against a genuinely changed file still resyncs on the first stat
/// that observes the new `(mtime, size)`.
///
/// This only bounds the *stable-but-unsettled* case: the same `(mtime,
/// size)` observed repeatedly while that mtime is still within
/// [`MTIME_GRANULARITY`] of "now". A file whose `(mtime, size)` changes on
/// every stat is never debounced at all -- each such call already disagrees
/// with the cached snapshot, so it always takes the immediate re-read path
/// regardless of how recently the last one happened.
const DISK_CHECK_DEBOUNCE: Duration = Duration::from_millis(250);

/// Filesystem mtime granularity margin: covers FAT/exFAT (2s) and is a safe
/// superset of HFS+/ext3/APFS (1s or finer). An mtime observed more recently
/// than this cannot be trusted to distinguish "unchanged" from "rewritten
/// within the same tick", so such entries are re-verified by content compare
/// instead of by stat alone -- this is what closes the racy-rewrite gap.
const MTIME_GRANULARITY: Duration = Duration::from_secs(2);

/// Content up to this size is indexed inline; larger content goes to the
/// blocking pool.
const INLINE_INDEX_MAX_BYTES: usize = 1024 * 1024;

/// Lines between two consecutive [`DocumentText`] checkpoints.
const LINE_CHECKPOINT_STRIDE: usize = 64;

/// Whether `byte` ends a line under the LSP 3.17 line model.
const fn is_line_terminator(byte: u8) -> bool {
    matches!(byte, b'\n' | b'\r')
}

/// Byte offset where the line starting at `start` ends (its terminator, or
/// `content.len()` for the last line), and where the next line starts, or
/// `None` for the last line. `\r\n` is one terminator.
fn line_bounds(content: &str, start: usize) -> (usize, Option<usize>) {
    let bytes = content.as_bytes();
    let Some(offset) = bytes
        .get(start..)
        .and_then(|rest| rest.iter().position(|&b| is_line_terminator(b)))
    else {
        return (content.len(), None);
    };
    let end = start.saturating_add(offset);
    let next = end.saturating_add(1);
    let crlf = bytes.get(end) == Some(&b'\r') && bytes.get(next) == Some(&b'\n');
    (end, Some(next.saturating_add(usize::from(crlf))))
}

/// A document's text together with sparse line checkpoints, so a line lookup
/// scans at most one checkpoint stride rather than the whole document (#488).
///
/// Lines follow the LSP 3.17 line model, the same one
/// [`DocumentTracker::read_line_checked`] applies to disk reads: a line ends
/// at `\n`, `\r\n` or a lone `\r`, the terminator is not part of the line,
/// and the empty line after a final terminator (or line 0 of empty content)
/// exists. Servers that split only on `\n`/`\r\n` (reportedly rust-analyzer,
/// gopls and clangd) disagree with this on files containing a lone `\r`;
/// positions after such a `\r` are then converted against the wrong line
/// text.
///
/// Content and checkpoints are built together and never mutated, so the index
/// cannot go stale. Memory overhead is one `usize` per stride lines, at most
/// `len / 8` bytes at the default stride even for content made only of empty
/// lines.
#[derive(Clone)]
pub(super) struct DocumentText {
    content: String,
    stride: usize,
    /// `checkpoints[k]` is the byte offset where line `k * stride` starts.
    checkpoints: Box<[usize]>,
}

impl std::fmt::Debug for DocumentText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentText")
            .field("len", &self.content.len())
            .field("checkpoints", &self.checkpoints.len())
            .finish_non_exhaustive()
    }
}

impl DocumentText {
    /// Indexes `content` at the default stride.
    pub(super) fn new(content: String) -> Self {
        Self::with_stride(content, LINE_CHECKPOINT_STRIDE)
    }

    /// Indexes `content`, with a checkpoint every `stride` lines (`stride`
    /// is clamped to at least 1).
    fn with_stride(content: String, stride: usize) -> Self {
        let stride = stride.max(1);
        let mut checkpoints = vec![0];
        let mut start = 0;
        let mut line = 0usize;
        while let (_, Some(next)) = line_bounds(&content, start) {
            start = next;
            line = line.saturating_add(1);
            if line.is_multiple_of(stride) {
                checkpoints.push(start);
            }
        }
        Self {
            content,
            stride,
            checkpoints: checkpoints.into_boxed_slice(),
        }
    }

    /// As [`Self::new`], but indexes content above [`INLINE_INDEX_MAX_BYTES`]
    /// on the blocking pool so a large file does not stall the runtime.
    async fn build(content: String) -> Result<Self> {
        if content.len() <= INLINE_INDEX_MAX_BYTES {
            return Ok(Self::new(content));
        }
        tokio::task::spawn_blocking(move || Self::new(content))
            .await
            .map_err(|e| Error::Io(std::io::Error::other(e)))
    }

    /// The full text.
    pub(super) fn as_str(&self) -> &str {
        &self.content
    }

    /// The 0-based `n`'th line without its terminator, or `None` if there is
    /// no such line. Scans at most `stride - 1` terminators from the nearest
    /// checkpoint plus the target line itself.
    pub(super) fn line(&self, n: u32) -> Option<&str> {
        let n = n as usize;
        let mut start = *self.checkpoints.get(n.checked_div(self.stride)?)?;
        for _ in 0..n.checked_rem(self.stride)? {
            start = line_bounds(&self.content, start).1?;
        }
        let (end, _) = line_bounds(&self.content, start);
        Some(&self.content[start..end])
    }
}

impl PartialEq for DocumentText {
    fn eq(&self, other: &Self) -> bool {
        self.content == other.content
    }
}

impl Eq for DocumentText {}

/// Returns whether `mtime` is old enough, relative to `read_at`, that a write
/// landing after `read_at` could not have preserved it.
///
/// `read_at` must be captured *before* the filesystem is stat'd (not after any
/// subsequent read), otherwise a write racing the read itself could produce a
/// new mtime that still appears "settled" against a later timestamp.
fn mtime_settled(mtime: Option<SystemTime>, read_at: SystemTime) -> bool {
    mtime.is_some_and(|m| {
        m.checked_add(MTIME_GRANULARITY)
            .is_some_and(|t| t <= read_at)
    })
}

/// Rejects `file` unless its Win32 file type is `FILE_TYPE_DISK`, the
/// Windows equivalent of the Unix `fstat`-based regular-file check in
/// [`DocumentTracker::open_checked`]. `std::fs::Metadata::is_file()` alone
/// is not a reliable rejection for every special path on Windows (e.g.
/// reserved device names like `CON`, `COM1`, `NUL`); those can still block
/// indefinitely on read, so this bounds the read -- not the open itself,
/// which Win32 has no non-blocking equivalent for (see #442).
#[cfg(windows)]
async fn check_disk_file_type(file: &fs::File, path: &Path) -> Result<()> {
    // `winapi_util` only accepts `std::fs::File`; the duplicate handle reports the same type.
    let std_file = file
        .try_clone()
        .await
        .map_err(|e| Error::FileIo {
            path: path.to_path_buf(),
            source: e,
        })?
        .into_std()
        .await;

    match winapi_util::file::typ(&std_file) {
        Ok(file_type) if file_type.is_disk() => Ok(()),
        _ => Err(Error::NotARegularFile(path.to_path_buf())),
    }
}

/// A snapshot of a document's on-disk filesystem state, captured the last
/// time its content was actually read and compared.
///
/// [`DocumentTracker::ensure_open`] stats the file on every call; when the
/// stat matches this snapshot and [`Self::mtime_settled`] holds, the cached
/// content is trusted without touching the file's bytes again. This is what
/// keeps the common "file unchanged" path cheap while still detecting
/// external edits (git checkout/stash, formatters, the MCP host's own
/// edits) made outside mcpls.
#[derive(Debug, Clone, Copy)]
pub struct DiskSync {
    /// Last observed modification time, or `None` if the filesystem or
    /// platform does not report one (in which case the entry is never
    /// treated as settled, forcing a content re-read outside the debounce
    /// window).
    pub mtime: Option<SystemTime>,
    /// Last observed file size in bytes.
    pub size: u64,
    /// Whether `mtime` was already old enough, relative to when it was
    /// observed, that a same-tick rewrite could not have preserved it.
    pub mtime_settled: bool,
    /// When the file's content was last actually re-read and compared.
    ///
    /// Used only to debounce the content re-read on a racy (not-yet-settled)
    /// entry; deliberately excluded from equality so two otherwise-identical
    /// snapshots don't compare unequal merely because they were checked at
    /// different instants.
    pub content_checked_at: Instant,
}

impl PartialEq for DiskSync {
    fn eq(&self, other: &Self) -> bool {
        self.mtime == other.mtime
            && self.size == other.size
            && self.mtime_settled == other.mtime_settled
    }
}

impl Eq for DiskSync {}

/// State of a single document.
///
/// All fields are private. `DocumentTracker::open` (via `Self::new`)
/// establishes the initial state: `version` starts at 1, `disk` provenance
/// starts `None`, and no server is recorded as synced. From there, every
/// mutation goes through a dedicated method (`commit_reload`, `set_disk`,
/// `mark_synced`, `forget_server`) rather than a
/// partial field write, so within a single tracked lifetime `version` (see
/// [`Self::version`]) only increases. This does not cover re-opening: calling
/// `DocumentTracker::open` again for an already-tracked path unconditionally
/// replaces the entry, resetting `version` to 1 and clearing `synced` -- see
/// that method's docs.
///
/// The `disk` provenance invariant: `None` means the content's on-disk
/// provenance is unknown (it came from an in-memory `open` call, not a
/// verified disk read), so `ensure_open` must always re-verify by content
/// compare rather than trusting a stat match. `DiskSync`'s hand-written
/// `PartialEq` excludes `content_checked_at` (see that field's doc comment),
/// and that exclusion propagates here: two `DocumentState`s can compare
/// equal via this struct's own hand-written `PartialEq`/`Eq` (below) despite
/// having been disk-verified at different instants. This is intentional --
/// `content_checked_at` is a debounce timer, not part of a document's
/// logical state. `last_accessed` (also excluded, for the same reason) is
/// likewise not logical state, just an LRU-eviction timestamp (#495).
#[derive(Debug, Clone)]
pub(super) struct DocumentState {
    uri: Uri,
    language_id: String,
    version: i32,
    text: DocumentText,
    disk: Option<DiskSync>,
    synced: HashMap<ServerId, i32>,
    /// When this document was last accessed via `ensure_open`
    /// (`Self::touch`), used to pick the least-recently-used entry when
    /// `DocumentTracker::open` must evict to stay under
    /// `ResourceLimits::max_documents` (#495).
    last_accessed: Instant,
}

impl PartialEq for DocumentState {
    fn eq(&self, other: &Self) -> bool {
        // Destructured (rather than plain field access) so a future new
        // field fails to compile here until it's deliberately included or
        // excluded -- unlike a derived impl, hand-written equality gets no
        // such reminder for free.
        let Self {
            uri,
            language_id,
            version,
            text,
            disk,
            synced,
            last_accessed: _,
        } = self;
        *uri == other.uri
            && *language_id == other.language_id
            && *version == other.version
            && *text == other.text
            && *disk == other.disk
            && *synced == other.synced
    }
}

impl Eq for DocumentState {}

impl DocumentState {
    /// Creates a new document state at version 1, with unknown disk
    /// provenance and no server yet recorded as synced.
    fn new(uri: Uri, language_id: String, text: DocumentText) -> Self {
        Self {
            uri,
            language_id,
            version: 1,
            text,
            disk: None,
            synced: HashMap::new(),
            last_accessed: Instant::now(),
        }
    }

    /// Marks this document as just accessed, for LRU eviction ordering under
    /// `ResourceLimits::max_documents` (#495).
    fn touch(&mut self) {
        self.last_accessed = Instant::now();
    }

    /// Document URI.
    #[must_use]
    #[cfg(test)]
    pub(crate) const fn uri(&self) -> &Uri {
        &self.uri
    }

    /// Language identifier.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn language_id(&self) -> &str {
        &self.language_id
    }

    /// Document version. Monotonically increasing: every mutation that
    /// changes `content` (`commit_reload`) also bumps
    /// this, and never decreases it.
    #[must_use]
    #[cfg(test)]
    pub(crate) const fn version(&self) -> i32 {
        self.version
    }

    /// Document content.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn content(&self) -> &str {
        self.text.as_str()
    }

    /// Filesystem snapshot as of the last time `content` was read from disk.
    /// See the struct-level docs for the meaning of `None`.
    const fn disk(&self) -> Option<DiskSync> {
        self.disk
    }

    /// Last document version pushed to `server` via `didOpen`/`didChange`,
    /// or `None` if `server` has never seen this document.
    ///
    /// A single document can be synced to multiple servers (e.g. hover
    /// routed to one server, diagnostics to another for the same language),
    /// each needing its own `didOpen`/`didChange` history -- a server absent
    /// from this map has never seen the document and must receive
    /// `didOpen`, not `didChange`, on its next `ensure_open` call.
    #[must_use]
    pub(crate) fn synced_version(&self, server: &ServerId) -> Option<i32> {
        self.synced.get(server).copied()
    }

    /// Whether no server has ever synced this document.
    fn has_never_synced(&self) -> bool {
        self.synced.is_empty()
    }

    /// Commits a disk-verified reload: sets `version`, `content`, and `disk`
    /// together. `version` must be no less than the current version,
    /// preserving the monotonicity invariant. (Not strictly greater: the
    /// caller computes `version` via `saturating_add`, which can legitimately
    /// clamp to the current value at `i32::MAX`.)
    fn commit_reload(&mut self, version: i32, text: DocumentText, snap: Option<DiskSync>) {
        debug_assert!(
            version >= self.version,
            "document version must be monotonically increasing"
        );
        self.version = version;
        self.text = text;
        self.disk = snap;
    }

    /// Sets the disk snapshot without changing `content` or `version`.
    const fn set_disk(&mut self, snap: DiskSync) {
        self.disk = Some(snap);
    }

    /// Records that `server` has synced up to `version`.
    fn mark_synced(&mut self, server: ServerId, version: i32) {
        self.synced.insert(server, version);
    }

    /// Forgets `server`'s sync history for this document.
    fn forget_server(&mut self, server: &ServerId) {
        self.synced.remove(server);
    }
}

/// Default value for [`ResourceLimits::max_documents`], also used as the
/// TOML default for `workspace.max_documents` (`config::default_max_documents`).
pub const DEFAULT_MAX_DOCUMENTS: usize = 100;

/// Default value for [`ResourceLimits::max_file_size`] (10MB), also used as
/// the TOML default for `workspace.max_file_size` (`config::default_max_file_size`).
pub const DEFAULT_MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Resource limits for document tracking.
#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    /// Maximum number of open documents (0 = unlimited).
    pub max_documents: usize,
    /// Maximum file size in bytes (0 = unlimited).
    pub max_file_size: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_documents: DEFAULT_MAX_DOCUMENTS,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

/// Nominal charge for a [`DocumentTracker::read_line_checked`] call whose
/// [`DocumentTracker::open_checked`] failed (path doesn't exist, isn't a
/// regular file, or already exceeds `max_file_size`) -- zero bytes were
/// actually scanned, but charging a literal `0` would let a response naming
/// many nonexistent paths (a routine, non-attacker-controlled LSP server
/// behavior -- e.g. rust-analyzer's stdlib locations without `rust-src`
/// installed) repeat that cheap-but-nonzero syscall for free against a
/// per-response I/O budget (see #474's budget-bypass follow-up). Small
/// enough to have no material effect on a legitimate response's budget
/// (~10,000 failed opens before exhausting [`DEFAULT_MAX_FILE_SIZE`]'s
/// worth of budget on their own), while still bounding the failed-open
/// amplification to the same order of magnitude as other count caps in this
/// crate.
pub const OPEN_FAILURE_CHARGE_BYTES: u64 = 4096;

/// Outcome of [`DocumentTracker::read_line_checked`]: the requested line
/// (`None` if the file has fewer lines, doesn't exist, or otherwise
/// resolved to no usable text), plus the bytes to charge a caller
/// tracking its own I/O budget across many calls (see `EncodingCtx`'s
/// per-response disk-read budget, #474) -- not always a literal count of
/// bytes scanned (see [`OPEN_FAILURE_CHARGE_BYTES`]), but always safe to
/// charge as such. Charge this rather than assuming cost is proportional
/// to `text`'s own length -- most of the cost is the lines skipped before
/// it.
#[derive(Debug, Clone)]
pub struct LineRead {
    /// The requested line's text, or `None` if the file has no such line.
    pub(crate) text: Option<String>,
    /// Bytes to charge against a caller's I/O budget for this call; see
    /// this type's own doc for when this isn't a literal scanned-byte count.
    pub(crate) bytes_read: u64,
}

/// A `textDocument/didClose` still owed after `DocumentTracker::open`'s LRU
/// eviction removed a document (#495): the servers that had it open and the
/// URI to close.
///
/// `DocumentTracker` has no access to any server's [`LspClient`] -- that
/// registry lives one layer up, in `Translator` -- so it cannot send the
/// close itself. Debts are kept per `(path, server)`: a server that later
/// re-opens the path settles its own debt first (close, then open, see
/// `DocumentTracker::sync_phase`), while the remaining servers' debts stay
/// pending for [`DocumentTracker::try_claim_pending_close`].
#[derive(Debug)]
struct PendingClose {
    uri: Uri,
    servers: HashSet<ServerId>,
}

/// Exclusive claim on one path's owed `didClose` notifications, returned by
/// [`DocumentTracker::try_claim_pending_close`].
///
/// Holds the path's lock for as long as it lives, so any later `didOpen` for
/// the path through [`DocumentTracker::ensure_open`] is ordered after the
/// closes. Drop it as soon as the notifications are sent.
#[derive(Debug)]
#[must_use = "dropping the claim releases the path lock; send the closes first"]
pub struct PendingCloseClaim<'a> {
    /// Filesystem path of the evicted document.
    pub(crate) path: PathBuf,
    /// URI of the evicted document, as sent to any server that had it open.
    pub(crate) uri: Uri,
    /// Servers that still need a `textDocument/didClose` for `uri`.
    pub(crate) servers: Vec<ServerId>,
    _path_guard: PathLockGuard<'a>,
}

/// Tracks document state across the workspace.
///
/// Every method takes `&self`: the document map and the per-path locks used
/// by [`Self::ensure_open`] are both interior-mutable, so a single tracker
/// can be shared behind a plain `Arc<DocumentTracker>` with no outer lock.
/// See [`Self::ensure_open`] for the concurrency contract this maintains.
#[derive(Debug)]
pub struct DocumentTracker {
    /// Open documents by file path. Locked only for the short, synchronous
    /// section that touches it — never held across an `await`.
    documents: StdMutex<HashMap<PathBuf, DocumentState>>,
    /// Per-path locks serializing [`Self::ensure_open`] calls for the same
    /// path, so calls for different paths never wait on each other. See
    /// `lock_path` for how entries are created and evicted.
    ///
    /// Also excluded from `Self::open`'s LRU eviction (#495): a path is
    /// present here for the whole duration of any `ensure_open` call against
    /// it (`lock_path`'s guard is held across it). That window ends before
    /// the caller's own LSP round-trip, which `in_flight` covers.
    path_locks: StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>,
    /// Refcount of [`InFlightGuard`]s per path: documents a handler is still
    /// using across its LSP round-trip, which [`Self::open`]'s LRU eviction
    /// must not remove (#503). Shared with each guard so it can release
    /// without borrowing the tracker.
    in_flight: InFlightMap,
    /// Per-server sync generation, bumped by [`Self::forget_server`].
    ///
    /// `ensure_open` captures a server's generation before doing any I/O and
    /// only commits its `synced` update if the generation is unchanged when
    /// it finishes -- see [`Self::forget_server`]'s docs for the race this
    /// closes. Absent from the map is equivalent to generation `0`.
    generations: StdMutex<HashMap<ServerId, u64>>,
    /// Resource limits for tracking.
    limits: ResourceLimits,
    /// Custom file extension to language ID mappings.
    extension_map: HashMap<String, String>,
    /// `didClose` notifications owed after `Self::open`'s LRU eviction (#495),
    /// per path and server. See [`PendingClose`].
    ///
    /// Lock order: `documents` before `pending_closes`, never the reverse.
    /// Under a path's lock, the servers pending for that path and the
    /// servers synced to it never overlap.
    pending_closes: StdMutex<HashMap<PathBuf, PendingClose>>,
}

impl DocumentTracker {
    /// Create a new document tracker with custom limits and extension mappings.
    #[must_use]
    pub fn new(limits: ResourceLimits, extension_map: HashMap<String, String>) -> Self {
        Self {
            documents: StdMutex::new(HashMap::new()),
            path_locks: StdMutex::new(HashMap::new()),
            in_flight: Arc::default(),
            generations: StdMutex::new(HashMap::new()),
            limits,
            extension_map,
            pending_closes: StdMutex::new(HashMap::new()),
        }
    }

    /// The configured resource limits.
    #[must_use]
    pub(crate) const fn limits(&self) -> ResourceLimits {
        self.limits
    }

    /// Marks `path` as in use by a handler until the returned guard drops, so
    /// [`Self::open`]'s LRU eviction skips it (#503).
    ///
    /// Take the guard *before* [`Self::ensure_open`] and keep it alive across
    /// the LSP round-trip that follows; guards for the same path stack, and
    /// the path is evictable again only once all of them have dropped.
    pub(crate) fn mark_in_flight(&self, path: &Path) -> InFlightGuard {
        let mut in_flight = lock_std(&self.in_flight);
        let count = in_flight.entry(path.to_path_buf()).or_insert(0);
        *count = count.saturating_add(1);
        drop(in_flight);
        InFlightGuard {
            in_flight: Arc::clone(&self.in_flight),
            path: path.to_path_buf(),
        }
    }

    /// Number of live [`InFlightGuard`]s for `path`.
    #[cfg(test)]
    pub(crate) fn in_flight_count(&self, path: &Path) -> usize {
        lock_std(&self.in_flight).get(path).copied().unwrap_or(0)
    }

    /// Check if a document is currently open.
    #[must_use]
    pub fn is_open(&self, path: &Path) -> bool {
        lock_std(&self.documents).contains_key(path)
    }

    /// Get a clone of the state of an open document.
    #[must_use]
    #[cfg(test)]
    pub(super) fn get(&self, path: &Path) -> Option<DocumentState> {
        lock_std(&self.documents).get(path).cloned()
    }

    /// Text of the 0-based `line`'th line of `path`'s currently tracked
    /// content, or `None` if the document is not open or has no such line.
    ///
    /// Reads the in-memory content mcpls already sent the server via
    /// `didOpen`/`didChange` -- cheaper than a disk read (no I/O, no
    /// re-scanning the whole file) and more correct when disk and server
    /// state have diverged (e.g. an edit not yet flushed to disk).
    #[must_use]
    pub fn line_text(&self, path: &Path, line: u32) -> Option<String> {
        let documents = lock_std(&self.documents);
        documents.get(path)?.text.line(line).map(str::to_string)
    }

    /// Get the number of open documents.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        lock_std(&self.documents).len()
    }

    /// Check if there are no open documents.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        lock_std(&self.documents).is_empty()
    }

    /// Open a document and track its state.
    ///
    /// Returns the document URI for use in LSP requests.
    ///
    /// When `max_documents` would otherwise be exceeded, evicts the
    /// least-recently-used tracked document that both has no
    /// `ensure_open` call currently in flight against it and is
    /// disk-verified (see `evict_lru`) to make room, rather than failing
    /// outright (#495) -- the servers that had the evicted document open are
    /// recorded as owed a `didClose` (see [`PendingClose`]). Only falls back
    /// to [`Error::DocumentLimitExceeded`] when no tracked document meets
    /// both conditions, so none is safe to evict.
    ///
    /// The owed closes are bounded by `max_documents` times the number of
    /// servers: evicting the same path again merges into its existing entry.
    /// `Translator` flushes them after every `ensure_open` that could have
    /// triggered eviction.
    ///
    /// The caller must hold `lock_path` for `path` (as `ensure_open` does):
    /// without it, a concurrent `ensure_open` for the same path could
    /// interleave with the insert below and lose its disk snapshot.
    ///
    /// A document a handler is still using across its LSP round-trip (see
    /// `mark_in_flight`) is never evicted, so a concurrent `open` cannot take
    /// it away mid-request even at a very small `max_documents` (#503).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Document limit is exceeded and no document is evictable
    /// - File size limit is exceeded
    #[cfg(test)]
    pub(crate) fn open(&self, path: PathBuf, content: String) -> Result<Uri> {
        self.open_text(path, DocumentText::new(content))
    }

    /// As [`Self::open`], for content already indexed -- lets async callers
    /// build the [`DocumentText`] off the runtime thread.
    fn open_text(&self, path: PathBuf, text: DocumentText) -> Result<Uri> {
        self.check_file_size(text.as_str().len() as u64)?;

        let uri = path_to_uri(&path)?;
        let language_id = detect_language(&path, &self.extension_map);

        let state = DocumentState::new(uri.clone(), language_id, text);

        // Check document limit and insert under a single lock acquisition so
        // two concurrent `open` calls for different new paths can't both
        // pass the check and jointly exceed the limit by one. Dropped
        // explicitly right after the insert rather than at function return.
        //
        // Skipped entirely when `path` is already tracked: re-opening an
        // existing path (`insert` below overwrites its entry in place, not
        // growing the map) never needs room made for it -- checking the
        // limit anyway would needlessly evict some unrelated victim (or, if
        // `path` itself were picked as the LRU candidate, evict and then
        // immediately re-insert it, queuing a spurious `didClose`).
        let mut documents = lock_std(&self.documents);
        if self.limits.max_documents > 0
            && documents.len() >= self.limits.max_documents
            && !documents.contains_key(&path)
        {
            let Some((evicted_path, evicted_state)) =
                Self::evict_lru(&mut documents, &self.path_locks, &self.in_flight)
            else {
                return Err(Error::DocumentLimitExceeded {
                    current: documents.len(),
                    max: self.limits.max_documents,
                });
            };
            self.record_pending_close(evicted_path, evicted_state);
        }
        documents.insert(path, state);
        drop(documents);
        Ok(uri)
    }

    /// Records that every server `evicted` had synced is owed a `didClose`,
    /// merging with any debts already pending for `path`.
    fn record_pending_close(&self, path: PathBuf, evicted: DocumentState) {
        if evicted.synced.is_empty() {
            return;
        }
        lock_std(&self.pending_closes)
            .entry(path)
            .or_insert_with(|| PendingClose {
                uri: evicted.uri,
                servers: HashSet::new(),
            })
            .servers
            .extend(evicted.synced.into_keys());
    }

    /// Removes and returns the least-recently-used entry in `documents` that
    /// is unlocked (not in `path_locks`, #495), not held by an
    /// [`InFlightGuard`] (#503), and disk-verified -- see below for why that
    /// last condition is required.
    ///
    /// A candidate whose `disk()` is `None` is skipped: its in-memory
    /// `content` has not been read-back-verified against disk (e.g. it was
    /// just `open`ed and `ensure_open` has not yet recorded a snapshot), so
    /// evicting it could discard content mcpls has no other record of --
    /// unlike a disk-verified candidate, whose evicted content is
    /// reproducible by re-reading the file.
    ///
    /// Returns `None` if every tracked document is currently locked, in
    /// flight, or not disk-verified, in which case the caller must not evict
    /// anything.
    fn evict_lru(
        documents: &mut HashMap<PathBuf, DocumentState>,
        path_locks: &StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>,
        in_flight: &InFlightMap,
    ) -> Option<(PathBuf, DocumentState)> {
        let mut busy = lock_std(path_locks)
            .keys()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        busy.extend(lock_std(in_flight).keys().cloned());
        let lru_path = documents
            .iter()
            .filter(|(path, state)| !busy.contains(path.as_path()) && state.disk().is_some())
            .min_by_key(|(_, state)| state.last_accessed)
            .map(|(path, _)| path.clone())?;
        documents.remove(&lru_path).map(|state| (lru_path, state))
    }

    /// Returns an error if `size` exceeds the configured file size limit.
    const fn check_file_size(&self, size: u64) -> Result<()> {
        if self.limits.max_file_size > 0 && size > self.limits.max_file_size {
            return Err(Error::FileSizeLimitExceeded {
                size,
                max: self.limits.max_file_size,
            });
        }
        Ok(())
    }

    /// Sets the disk snapshot for an already-tracked document.
    ///
    /// A no-op if the path is no longer tracked; every call site runs under
    /// the per-path lock for the whole `ensure_open` call, so this should
    /// not happen in practice, but it avoids an `unwrap`/`expect` on the
    /// lookup.
    fn set_disk(&self, path: &Path, snap: DiskSync) {
        if let Some(st) = lock_std(&self.documents).get_mut(path) {
            st.set_disk(snap);
        }
    }

    /// Close a document and remove it from tracking.
    ///
    /// Returns the document state if it was open.
    #[cfg(test)]
    pub(super) fn close(&self, path: &Path) -> Option<DocumentState> {
        lock_std(&self.documents).remove(path)
    }

    /// Snapshot of the filesystem paths of all currently open documents.
    pub fn open_paths(&self) -> Vec<PathBuf> {
        lock_std(&self.documents).keys().cloned().collect()
    }

    /// Forget `server`'s last-synced version for every currently open
    /// document, so the next `ensure_open` call sends `didOpen` again
    /// instead of `didChange`.
    ///
    /// Called after `server` is respawned: the fresh process has no memory
    /// of any document the old one had open, so this tracker's per-server
    /// sync history for it must be forgotten too, or `ensure_open` would
    /// wrongly send `didChange` for a document the new process never saw.
    ///
    /// Also bumps `server`'s sync generation. Clearing `synced` alone is not
    /// enough: a call already in flight against the old (dead) connection
    /// when this runs can still have its `didOpen`/`didChange` notify
    /// "succeed" (`LspClient::notify` only enqueues onto a channel -- a dead
    /// process is not observed by the send itself), and would otherwise
    /// re-insert a stale entry after this method has already cleared it.
    /// `ensure_open` captures the generation before starting and discards
    /// its `synced` write if the generation moved in the meantime, closing
    /// that race regardless of exactly when the notify "succeeds".
    pub fn forget_server(&self, server: &ServerId) {
        let mut generations = lock_std(&self.generations);
        let generation = generations.entry(server.clone()).or_insert(0);
        *generation = generation.saturating_add(1);
        drop(generations);
        for state in lock_std(&self.documents).values_mut() {
            state.forget_server(server);
        }
        // The respawned process never had these documents open, so owes no close.
        lock_std(&self.pending_closes).retain(|_, pending| {
            pending.servers.remove(server);
            !pending.servers.is_empty()
        });
    }

    /// Current sync generation for `server` (see [`Self::forget_server`]).
    fn generation(&self, server: &ServerId) -> u64 {
        lock_std(&self.generations)
            .get(server)
            .copied()
            .unwrap_or(0)
    }

    /// Acquire the per-path lock used by [`Self::ensure_open`], creating its
    /// entry on first use.
    ///
    /// The map of per-path locks (`path_locks`) is itself locked only for
    /// the map lookup/insert/remove — never across an `await` — so acquiring
    /// one path's lock never blocks a concurrent acquisition for a different
    /// path. Awaiting the returned path's own lock is what actually
    /// serializes calls for the same path.
    ///
    /// The returned guard evicts its `path_locks` entry when dropped, but
    /// only if no other caller is concurrently waiting on it (see
    /// [`PathLockGuard`]'s `Drop` impl) — otherwise the map would grow by
    /// one entry per distinct path ever opened, for the lifetime of the
    /// process.
    async fn lock_path(&self, path: &Path) -> PathLockGuard<'_> {
        let arc = {
            let mut locks = lock_std(&self.path_locks);
            locks
                .entry(path.to_path_buf())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let guard = Arc::clone(&arc).lock_owned().await;
        PathLockGuard {
            path_locks: &self.path_locks,
            path: path.to_path_buf(),
            arc,
            guard: Some(guard),
        }
    }

    /// As [`Self::lock_path`], but returns `None` instead of waiting when
    /// another caller holds `path`'s lock.
    fn try_lock_path(&self, path: &Path) -> Option<PathLockGuard<'_>> {
        let arc = {
            let mut locks = lock_std(&self.path_locks);
            locks
                .entry(path.to_path_buf())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        if let Ok(guard) = Arc::clone(&arc).try_lock_owned() {
            return Some(PathLockGuard {
                path_locks: &self.path_locks,
                path: path.to_path_buf(),
                arc,
                guard: Some(guard),
            });
        }
        // Mirrors `PathLockGuard`'s eviction rule: our `arc` and the map's
        // entry are the only references left when nobody holds or awaits it.
        let mut locks = lock_std(&self.path_locks);
        if Arc::strong_count(&arc) <= 2 {
            locks.remove(path);
        }
        None
    }

    /// Paths with a `didClose` still owed after LRU eviction, as a snapshot
    /// for [`Self::try_claim_pending_close`].
    pub(crate) fn pending_close_paths(&self) -> Vec<PathBuf> {
        lock_std(&self.pending_closes).keys().cloned().collect()
    }

    /// Claims `path`'s owed `didClose` notifications, or `None` if there are
    /// none for it or another caller currently holds the path's lock.
    ///
    /// Never waits on a path lock: a busy path is left pending for a later
    /// call, and its own `ensure_open` settles the debt of the server it syncs
    /// anyway. The returned claim holds the path lock, so a concurrent
    /// `ensure_open` for the path waits until the closes have been sent --
    /// claim, notify and drop one path at a time so a wedged server cannot
    /// stall unrelated paths.
    ///
    /// Servers that have meanwhile synced the path are filtered out: a close
    /// for them would undo a live open.
    pub(crate) fn try_claim_pending_close(&self, path: &Path) -> Option<PendingCloseClaim<'_>> {
        let path_guard = self.try_lock_path(path)?;
        let synced: HashSet<ServerId> = lock_std(&self.documents)
            .get(path)
            .map(|st| st.synced.keys().cloned().collect())
            .unwrap_or_default();
        let pending = lock_std(&self.pending_closes).remove(path)?;
        let servers: Vec<ServerId> = pending.servers.difference(&synced).cloned().collect();
        if servers.is_empty() {
            return None;
        }
        Some(PendingCloseClaim {
            path: path.to_path_buf(),
            uri: pending.uri,
            servers,
            _path_guard: path_guard,
        })
    }

    /// Removes `server`'s owed close for `path`, returning whether there was
    /// one. Must be called under `path`'s lock.
    fn take_pending_close(&self, path: &Path, server: &ServerId) -> bool {
        let mut pending_closes = lock_std(&self.pending_closes);
        let Some(pending) = pending_closes.get_mut(path) else {
            return false;
        };
        let owed = pending.servers.remove(server);
        if pending.servers.is_empty() {
            pending_closes.remove(path);
        }
        owed
    }

    /// Re-records `server`'s owed close for `path` after its `didClose`
    /// failed to send. Must be called under `path`'s lock.
    ///
    /// Skipped when `server` was forgotten since `generation` was observed: a
    /// respawned process never had the path open, so it owes no close. The
    /// check runs under the `pending_closes` lock, which `forget_server`'s
    /// purge also takes after bumping the generation, so one of the two
    /// always wins.
    fn restore_pending_close(&self, path: &Path, uri: &Uri, server: &ServerId, generation: u64) {
        let mut pending_closes = lock_std(&self.pending_closes);
        if self.generation(server) != generation {
            return;
        }
        pending_closes
            .entry(path.to_path_buf())
            .or_insert_with(|| PendingClose {
                uri: uri.clone(),
                servers: HashSet::new(),
            })
            .servers
            .insert(server.clone());
    }

    /// Ensure a document is open *for `server`*, opening it lazily if
    /// necessary, and resynchronize it with disk and with `server` if either
    /// has fallen behind.
    ///
    /// A single path can be synced to several servers independently (e.g.
    /// hover routed to one server, diagnostics to another, for the same
    /// language) -- this call syncs only the one server it is for. Internally
    /// it runs in two phases:
    ///
    /// **Disk phase**: stats the file on every call (a cheap syscall, never
    /// debounced) to detect external changes -- `git checkout`/`stash`,
    /// formatters, or edits made by the MCP host itself outside mcpls -- and
    /// re-reads its content when the stat indicates a possible change (see
    /// `DiskSync` for the settled/debounce rules). This phase never skips
    /// the *per-server* sync check below, even when it takes a fast path
    /// that skips the disk read: a second server that has never seen this
    /// document must still receive `didOpen` even if the file has not
    /// changed since a first server was opened on it.
    ///
    /// **Sync phase**: compares `server`'s last-synced version (tracked via
    /// `DocumentState::synced_version`) against the version decided by the disk
    /// phase, and sends exactly one of `didOpen` (server has never seen this
    /// document), `didChange` (server is behind), or nothing (server is
    /// already caught up). A `didChange` is always a single full-replacement
    /// notification (a `TextDocumentContentChangeEvent` with `range: None`,
    /// which per the LSP spec means "this is the entire new document
    /// content"); mcpls does not consult the server's negotiated
    /// `TextDocumentSyncKind` (`LspClient` has no access to
    /// `ServerCapabilities` at this layer) -- full-replacement is accepted in
    /// practice by rust-analyzer, pyright, tsserver, gopls and clangd, but is
    /// the first place to look if a future maintainer sees sync errors from
    /// a new server. The document is never closed and reopened on a change,
    /// so `get_cached_diagnostics` keeps serving the last-known diagnostics
    /// until the server re-publishes -- there is no transient empty window.
    ///
    /// `st.version`/`st.text`/`st.disk`/`synced[server]` are all committed
    /// only after the notification succeeds. A server that is never asked
    /// again never catches up to a later edit -- which is correct, since a
    /// server that is never asked never needs the content.
    ///
    /// Two cases fall outside the disk-change-detection mechanism entirely:
    /// - A tool that restores a file with an mtime and size identical to the
    ///   last ones observed (e.g. `tar x`, `rsync -a`, `cp -p`) is
    ///   indistinguishable from "unchanged", however long ago that snapshot
    ///   was taken -- not just within the racy detection window. Once a
    ///   snapshot is `mtime_settled`, restoring its exact `(mtime, size)`
    ///   retakes the fast path forever. Closing this would require hashing
    ///   content on every access.
    /// - `workspace_symbol_search` is served from the LSP server's own
    ///   index and is unaffected by this per-document mechanism for files
    ///   mcpls has never opened.
    ///
    /// # Concurrency
    ///
    /// Calls for the *same* `path` are serialized against each other (via
    /// `lock_path`), so no two such calls can observe or mutate that
    /// path's state concurrently -- this is what prevents duplicate
    /// `didOpen`/`didChange` notifications for the same document. Calls for
    /// *different* paths run fully concurrently: neither the per-path lock
    /// nor the short, synchronous locks used to touch the shared document
    /// map are ever held across this call's disk I/O or LSP notify.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file cannot be stat'd or read from disk
    /// - The `didOpen`/`didChange` notification fails to send
    /// - Resource limits are exceeded
    pub async fn ensure_open(
        &self,
        path: &Path,
        server: &ServerId,
        lsp_client: &LspClient,
    ) -> Result<Uri> {
        let _path_guard = self.lock_path(path).await;
        let generation = self.generation(server);
        let decision = self.disk_phase(path).await?;
        self.sync_phase(path, server, lsp_client, decision, generation)
            .await
    }

    /// Disk-verification phase of `ensure_open`: decides the version `path`
    /// should be at, reading from disk only when necessary. Never sends any
    /// LSP notification and never returns early in a way that would skip the
    /// per-server sync phase -- see `ensure_open`'s docs.
    async fn disk_phase(&self, path: &Path) -> Result<Decision> {
        if !lock_std(&self.documents).contains_key(path) {
            return self.disk_phase_new(path).await;
        }

        let read_at = SystemTime::now();
        let meta = fs::metadata(path).await.map_err(|e| Error::FileIo {
            path: path.to_path_buf(),
            source: e,
        })?;
        let mtime = meta.modified().ok();
        let size = meta.len();

        // `.map(...)` extracts an owned tuple from the lookup in a single
        // statement, so the lock releases immediately rather than staying
        // held while `fast_path` is computed. `get_mut` (rather than `get`)
        // so this same lookup can also `touch` the entry for LRU eviction
        // ordering (#495) -- every `ensure_open` call for an already-tracked
        // document reaches here, whether or not it ends up taking the fast
        // path below.
        let Some((uri, current_version, fast_path)) =
            lock_std(&self.documents).get_mut(path).map(|st| {
                st.touch();
                let stat_matches = st
                    .disk()
                    .is_some_and(|d| d.mtime == mtime && d.size == size);
                let fast_path = match st.disk() {
                    Some(d) if stat_matches && d.mtime_settled => true,
                    Some(d)
                        if stat_matches && d.content_checked_at.elapsed() < DISK_CHECK_DEBOUNCE =>
                    {
                        true
                    }
                    _ => false,
                };
                (st.uri.clone(), st.version, fast_path)
            })
        else {
            return Err(Error::DocumentNotFound(path.to_path_buf()));
        };
        if fast_path {
            return Ok(Decision::unchanged(uri, current_version));
        }

        let (fresh, ..) = self.read_to_string_checked(path).await?;
        let snap = DiskSync {
            mtime,
            size,
            mtime_settled: mtime_settled(mtime, read_at),
            content_checked_at: Instant::now(),
        };

        let Some(unchanged) = lock_std(&self.documents)
            .get(path)
            .map(|st| fresh == st.text.as_str())
        else {
            return Err(Error::DocumentNotFound(path.to_path_buf()));
        };

        if unchanged {
            self.set_disk(path, snap);
            return Ok(Decision::unchanged(uri, current_version));
        }

        Ok(Decision {
            uri,
            target_version: current_version.saturating_add(1),
            fresh_content: Some(DocumentText::build(fresh).await?),
            snap: Some(snap),
        })
    }

    /// Reads a not-yet-tracked file from disk and opens it in the tracker at
    /// version 1. No server has synced it yet, so the sync phase always
    /// sends `didOpen` regardless of which server calls next.
    async fn disk_phase_new(&self, path: &Path) -> Result<Decision> {
        let read_at = SystemTime::now();
        let (content, mtime, size) = self.read_to_string_checked(path).await?;

        let uri = self.open_text(path.to_path_buf(), DocumentText::build(content).await?)?;
        self.set_disk(
            path,
            DiskSync {
                mtime,
                size,
                mtime_settled: mtime_settled(mtime, read_at),
                content_checked_at: Instant::now(),
            },
        );

        Ok(Decision::unchanged(uri, 1))
    }

    /// Opens `path` for reading and verifies, via that same open handle's
    /// metadata, that it is a regular file within [`Self::check_file_size`]'s
    /// limit -- never a separately-stat'd path, which would let an atomic
    /// replace (e.g. a concurrent `rename`) between the check and the open
    /// swap in something else entirely.
    ///
    /// On Unix the open itself uses `O_NONBLOCK`, which has no effect on
    /// regular files but makes opening a FIFO (or other peer-waiting special
    /// file) return immediately instead of blocking indefinitely for a
    /// writer -- the file-type check below then rejects it. Without this,
    /// a FIFO substituted for an expected regular file could hang the
    /// calling task (and pin a blocking-pool thread) forever (see #418).
    ///
    /// **Known gap on Windows**: `CreateFileW` (what `fs::File::open` and
    /// `OpenOptions::open` call into) has no `O_NONBLOCK` equivalent, so the
    /// open itself can still block indefinitely on a hostile path (e.g. an
    /// oplock held by another process, or a dead network redirector) --
    /// Win32 offers nothing to bound that. What Windows does get is a
    /// content-read guarantee: the open handle is checked via `GetFileType`
    /// (see [`check_disk_file_type`]) immediately after open and before
    /// `metadata()` or any content read, rejecting anything that is not
    /// `FILE_TYPE_DISK` (e.g. reserved device names like `CON`, `COM1`,
    /// `NUL`, which `FileType::is_file()` alone does not reliably reject) --
    /// see #442. Platforms that are neither Unix nor Windows get neither
    /// protection: a plain blocking open with no file-type check beyond
    /// `is_file()`.
    async fn open_checked(&self, path: &Path) -> Result<(fs::File, std::fs::Metadata)> {
        #[cfg(unix)]
        let opened = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .await;
        #[cfg(not(unix))]
        let opened = fs::File::open(path).await;

        let file = opened.map_err(|e| Error::FileIo {
            path: path.to_path_buf(),
            source: e,
        })?;
        // Must precede metadata() below: GetFileInformationByHandle may fail for non-disk handles.
        #[cfg(windows)]
        check_disk_file_type(&file, path).await?;
        let meta = file.metadata().await.map_err(|e| Error::FileIo {
            path: path.to_path_buf(),
            source: e,
        })?;
        if !meta.is_file() {
            return Err(Error::NotARegularFile(path.to_path_buf()));
        }
        self.check_file_size(meta.len())?;
        Ok((file, meta))
    }

    /// Reads `file`'s content as UTF-8, bounded to one byte past
    /// [`Self::check_file_size`]'s limit regardless of the already-checked
    /// stat result -- defense in depth against the file growing between the
    /// stat (in [`Self::open_checked`]) and this read completing (see #418).
    /// A read that reaches the bound is reported as oversized even though
    /// the earlier stat passed, since the file grew past what was verified.
    ///
    /// `size_hint` is the size [`Self::open_checked`] already observed via
    /// `stat`, used only to preallocate the read buffer and avoid
    /// reallocation growth on the common (non-racing) path -- it is never
    /// trusted for the size check itself, which is always re-derived from
    /// the bytes actually read.
    async fn read_string_bounded(
        &self,
        path: &Path,
        mut file: fs::File,
        size_hint: u64,
    ) -> Result<String> {
        let max = self.limits.max_file_size;
        let cap = bounded_read_cap(max);
        let mut buf = Vec::with_capacity(usize::try_from(size_hint.min(cap)).unwrap_or(0));
        let io_err = |e: std::io::Error| Error::FileIo {
            path: path.to_path_buf(),
            source: e,
        };

        (&mut file)
            .take(cap)
            .read_to_end(&mut buf)
            .await
            .map_err(io_err)?;
        match check_bounded_utf8(buf, max) {
            BoundedReadOutcome::Ok(s) => Ok(s),
            BoundedReadOutcome::TooLarge { size } => {
                Err(Error::FileSizeLimitExceeded { size, max })
            }
            BoundedReadOutcome::InvalidUtf8(e) => Err(io_err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e,
            ))),
        }
    }

    /// Reads `path` through a single open file handle, checking its size and
    /// type via [`Self::open_checked`] and bounding the read via
    /// [`Self::read_string_bounded`].
    ///
    /// Returns the content along with the handle's own mtime and size, so
    /// callers can build a [`DiskSync`] snapshot consistent with what was
    /// actually read.
    async fn read_to_string_checked(
        &self,
        path: &Path,
    ) -> Result<(String, Option<SystemTime>, u64)> {
        let (file, meta) = self.open_checked(path).await?;
        let mtime = meta.modified().ok();
        let size = meta.len();
        let content = self.read_string_bounded(path, file, size).await?;
        Ok((content, mtime, size))
    }

    /// Reads only the 0-based `line`'th line of `path` from disk, applying
    /// the same regular-file and [`Self::check_file_size`] checks as a
    /// tracked document's disk read (see [`Self::read_to_string_checked`]),
    /// but stopping as soon as `line` is found rather than buffering the
    /// whole file just to discard everything past one line (see #474).
    ///
    /// For a document not tracked by this tracker at all -- e.g. one
    /// resolved only for encoding-conversion purposes, never opened for LSP
    /// sync -- there is otherwise no size or file-type gate on the path at
    /// all (see #427). Callers that only need best-effort text (falling back
    /// to `None` on any error) should treat every error here that way rather
    /// than surfacing it.
    ///
    /// [`LineRead::text`] is `None` if `path` doesn't resolve to an
    /// existing, readable regular file at all (see [`Self::open_checked`]),
    /// if `path` has fewer than `line + 1` lines, if the line's bytes are
    /// not valid UTF-8, or if `budget` (or `max_file_size`) was exhausted
    /// before a complete line could be read -- [`LineRead::bytes_read`] is
    /// populated in every one of these cases (see below), never silently
    /// dropped via an `Err` with no byte count. Lines follow the same rule
    /// as [`Self::line_text`] (see [`DocumentText`]): they end at `\n`,
    /// `\r\n` or a lone `\r`, and the empty line right after a final
    /// terminator (or line 0 of an empty file) is `Some("")`.
    ///
    /// `budget` bounds this call's own read on top of
    /// [`crate::util::bounded_read_cap`] of `max_file_size`: the actual cap
    /// used is `min(bounded_read_cap(max_file_size), budget + 1)`, enforced
    /// by wrapping the file handle itself in [`AsyncReadExt::take`] rather
    /// than checked after the fact -- so this call physically cannot scan
    /// more than one byte past `budget`, regardless of how large
    /// `max_file_size` is configured (including `max_file_size = 0`,
    /// meaning unlimited). The `+ 1` is the same disambiguation slack
    /// `bounded_read_cap` already applies to `max_file_size`: without it, a
    /// read whose remaining budget exactly equals its target line's byte
    /// length (no trailing newline) is indistinguishable from one
    /// genuinely truncated by the cap. A caller enforcing its own I/O
    /// budget across many calls (see `EncodingCtx`'s per-response
    /// disk-read budget, #474) passes its remaining allowance here and
    /// charges exactly [`LineRead::bytes_read`] afterward -- always
    /// available, on every outcome, so the budget can never be bypassed by
    /// triggering a failure mid-scan, and never overshoots by more than
    /// this one byte of slack.
    ///
    /// [`Self::open_checked`] failing (path doesn't exist, isn't a regular
    /// file, or already exceeds `max_file_size` at stat time) is reported
    /// the same way, charging [`OPEN_FAILURE_CHARGE_BYTES`] rather than a
    /// literal `0` -- zero bytes were actually scanned, but an LSP server
    /// routinely names paths that don't exist locally (e.g. rust-analyzer's
    /// `file:///rustc/<hash>/library/...` without `rust-src` installed),
    /// and a literal `0` would let a response naming many such paths repeat
    /// this cheap-but-nonzero syscall for free against the per-response
    /// budget (see #474's budget-bypass follow-up). A real mid-read I/O
    /// error (rare, not attacker-controlled by response content) is the one
    /// case that still returns a genuine `Err` with no byte count.
    ///
    /// Also closes #427/#418's TOCTOU margin without a dedicated error: if
    /// `path` grows past `max_file_size` (or past `budget`) between
    /// [`Self::open_checked`]'s stat and this read completing, the capped
    /// take-adapter simply runs out mid-line, which this method detects
    /// (`buf` doesn't end in the expected `\n`) and reports as `None` rather
    /// than returning a truncated line as if it were complete.
    pub(crate) async fn read_line_checked(
        &self,
        path: &Path,
        line: u32,
        budget: u64,
    ) -> Result<LineRead> {
        let Ok((file, _meta)) = self.open_checked(path).await else {
            return Ok(LineRead {
                text: None,
                bytes_read: OPEN_FAILURE_CHARGE_BYTES,
            });
        };
        let max = self.limits.max_file_size;
        // `+1` slack on `budget`, same trick `bounded_read_cap` already
        // applies to `max_file_size`: without it, a read whose remaining
        // budget exactly equals its target line's byte length (no trailing
        // newline) is indistinguishable from one truncated by the cap, and
        // was misreported as truncated (see #474's correctness-gate fix).
        let cap = bounded_read_cap(max).min(budget.saturating_add(1));
        let mut reader = tokio::io::BufReader::new(file.take(cap));
        let io_err = |e: std::io::Error| Error::FileIo {
            path: path.to_path_buf(),
            source: e,
        };

        read_nth_line(&mut reader, line, cap).await.map_err(io_err)
    }

    /// Per-server sync phase of `ensure_open`: sends `didOpen`, `didChange`,
    /// or nothing to `server` depending on its last-synced version, and
    /// commits the outcome only after the notification succeeds.
    ///
    /// `generation` is `server`'s sync generation as observed by the caller
    /// before this call started (see [`Self::forget_server`]): the
    /// `synced` write at the end is skipped if it no longer matches,
    /// meaning `server` was respawned while this call was in flight and its
    /// notify -- however it turned out -- was not actually delivered to the
    /// connection now on file for `server`.
    async fn sync_phase(
        &self,
        path: &Path,
        server: &ServerId,
        lsp_client: &LspClient,
        decision: Decision,
        generation: u64,
    ) -> Result<Uri> {
        let Decision {
            uri,
            target_version,
            fresh_content,
            snap,
        } = decision;

        // Cheap check first: the common case (an already-synced document,
        // which is most tool calls against a file already open elsewhere)
        // must not pay for cloning the full document content only to
        // discard it on the `up_to_date` return below. `.map(...)` extracts
        // an owned value from the lookup so the lock is released at the end
        // of this statement rather than held across the checks that follow.
        let Some(synced_version) = lock_std(&self.documents)
            .get(path)
            .map(|st| st.synced_version(server))
        else {
            return Err(Error::DocumentNotFound(path.to_path_buf()));
        };
        let up_to_date = synced_version.is_some_and(|v| v >= target_version);
        let is_first_open = synced_version.is_none();

        if up_to_date {
            return Ok(uri);
        }

        let Some((language_id, text)) = lock_std(&self.documents).get(path).map(|st| {
            let text = fresh_content
                .as_ref()
                .unwrap_or(&st.text)
                .as_str()
                .to_owned();
            (st.language_id.clone(), text)
        }) else {
            return Err(Error::DocumentNotFound(path.to_path_buf()));
        };

        let notify_result = if is_first_open {
            let closed = if self.take_pending_close(path, server) {
                lsp_client
                    .notify_typed::<DidCloseTextDocumentNotification>(DidCloseTextDocumentParams {
                        text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                    })
                    .await
                    .inspect_err(|_| self.restore_pending_close(path, &uri, server, generation))
            } else {
                Ok(())
            };
            match closed {
                Ok(()) => {
                    lsp_client
                        .notify_typed::<DidOpenTextDocumentNotification>(
                            DidOpenTextDocumentParams {
                                text_document: TextDocumentItem {
                                    uri: uri.clone(),
                                    language_id: language_id.into(),
                                    version: target_version,
                                    text,
                                },
                            },
                        )
                        .await
                }
                Err(err) => Err(err),
            }
        } else {
            lsp_client
                .notify_typed::<DidChangeTextDocumentNotification>(DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier {
                        version: target_version,
                        text_document_identifier: lsp_types::TextDocumentIdentifier {
                            uri: uri.clone(),
                        },
                    },
                    content_changes: vec![
                        TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
                            lsp_types::TextDocumentContentChangeWholeDocument { text },
                        ),
                    ],
                })
                .await
        };

        if let Err(err) = notify_result {
            // The server never learned about this document. If no server at
            // all has synced this path yet, leaving it tracked would
            // permanently desync every future server from the tracker, so
            // undo the insert and let the next call retry from scratch. If
            // another server already synced successfully, the path stays
            // tracked for that server's sake; this server's `synced` entry
            // simply stays absent/stale, so its own next call retries.
            // Two short lock scopes rather than one held across the
            // conditional `remove`: safe because `ensure_open`'s per-path
            // lock already serializes every caller for this path, so
            // nothing else can observe or mutate its `synced` map between
            // them.
            let first_ever_sync = lock_std(&self.documents)
                .get(path)
                .is_some_and(DocumentState::has_never_synced);
            if is_first_open && first_ever_sync {
                lock_std(&self.documents).remove(path);
            }
            return Err(err);
        }

        // Dropped explicitly right after the commit, rather than staying
        // alive (unused) until the function returns.
        let mut documents = lock_std(&self.documents);
        let Some(st) = documents.get_mut(path) else {
            return Err(Error::DocumentNotFound(path.to_path_buf()));
        };
        if let Some(fresh) = fresh_content {
            st.commit_reload(target_version, fresh, snap);
        }
        // Read while `documents` is still held, not before: `forget_server`
        // bumps the generation strictly before it acquires `documents`
        // itself (see its docs), so checking under this same lock is
        // airtight against the TOCTOU a separate, earlier read would leave
        // open -- either this sees the new generation and skips (in which
        // case `forget_server` has already cleared `synced`, or is blocked
        // waiting for *this* guard to release before it does), or it sees
        // the old one, in which case `forget_server` cannot have started
        // clearing yet and will correctly clear the entry this commits.
        if self.generation(server) == generation {
            st.mark_synced(server.clone(), target_version);
        }
        drop(documents);

        Ok(uri)
    }
}

/// Reads the 0-based `line`'th line from `reader` under the [`DocumentText`]
/// line model, scanning the buffered chunks directly so skipped lines are never
/// copied.
///
/// `cap` is the byte limit the caller imposed on `reader` (a `take` adapter):
/// a read that consumed `cap` bytes without finishing is reported as `None`
/// rather than returning a line the cap may have cut. A `\r` ending a skipped
/// line at a chunk boundary is remembered, so a `\n` opening the next chunk
/// still counts as part of the same `\r\n` terminator.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "every index derives from `position()` over the same chunk and stays within `chunk.len()`"
)]
async fn read_nth_line<R>(reader: &mut R, line: u32, cap: u64) -> std::io::Result<LineRead>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut buf = Vec::new();
    let mut bytes_read: u64 = 0;
    let mut current_line = 0u32;
    let mut after_cr = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            let reached_line = current_line == line && bytes_read < cap;
            return Ok(LineRead {
                text: reached_line.then(|| String::from_utf8(buf).ok()).flatten(),
                bytes_read,
            });
        }

        let mut i = usize::from(after_cr && chunk[0] == b'\n');
        after_cr = false;
        let mut finished = false;
        while i < chunk.len() {
            let rest = &chunk[i..];
            let Some(offset) = rest.iter().position(|&b| is_line_terminator(b)) else {
                if current_line == line {
                    buf.extend_from_slice(rest);
                }
                i = chunk.len();
                break;
            };
            let terminator = i + offset;
            if current_line == line {
                buf.extend_from_slice(&chunk[i..terminator]);
                i = terminator + 1;
                finished = true;
                break;
            }
            current_line += 1;
            i = terminator + 1;
            if chunk[terminator] == b'\r' {
                match chunk.get(i) {
                    Some(b'\n') => i += 1,
                    Some(_) => {}
                    None => after_cr = true,
                }
            }
        }

        bytes_read += i as u64;
        reader.consume(i);
        if finished {
            return Ok(LineRead {
                text: String::from_utf8(buf).ok(),
                bytes_read,
            });
        }
    }
}

/// Per-path count of live [`InFlightGuard`]s.
type InFlightMap = Arc<StdMutex<HashMap<PathBuf, usize>>>;

/// RAII marker that keeps a document out of [`DocumentTracker::open`]'s LRU
/// eviction for as long as it is alive (#503).
///
/// Obtained from [`DocumentTracker::mark_in_flight`]. Releases on drop, so
/// every exit path -- including errors and cancelled futures -- unprotects
/// the path again.
#[derive(Debug)]
#[must_use = "dropping the guard immediately makes the document evictable again"]
pub struct InFlightGuard {
    in_flight: InFlightMap,
    path: PathBuf,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut in_flight = lock_std(&self.in_flight);
        if let Some(count) = in_flight.get_mut(&self.path) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                in_flight.remove(&self.path);
            }
        }
    }
}

/// RAII guard for the per-path lock acquired by
/// [`DocumentTracker::lock_path`].
///
/// Holds an `OwnedMutexGuard` on the path's `Arc<AsyncMutex<()>>>` for as
/// long as the guard is alive, serializing `ensure_open` calls for that
/// path. On drop, evicts the `path_locks` map entry if (and only if) no
/// other caller holds a clone of the same `Arc` -- see the `Drop` impl for
/// why that check is race-free.
#[derive(Debug)]
struct PathLockGuard<'a> {
    path_locks: &'a StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>,
    path: PathBuf,
    arc: Arc<AsyncMutex<()>>,
    guard: Option<OwnedMutexGuard<()>>,
}

impl Drop for PathLockGuard<'_> {
    fn drop(&mut self) {
        // Unlock first so a task waiting on `arc.lock_owned()` can proceed
        // as soon as possible, rather than also waiting on `path_locks`.
        self.guard.take();

        let mut locks = lock_std(self.path_locks);
        // Checked only after `self.guard` -- and the extra internal `Arc`
        // clone it held -- was already dropped above, so what's left here is:
        // this task's own `self.arc`, the map's entry, and one more
        // reference for every *other* task that has already looked up this
        // same entry in `lock_path` (each holds its own clone continuously
        // from before that lookup until its own `Drop` runs this same check)
        // but hasn't finished dropping yet. A `strong_count` of 2 means no
        // such task exists, so it's safe to evict; any later caller just
        // creates a fresh entry. Leaving it forever would instead grow this
        // map by one entry per distinct path ever opened, for the process's
        // lifetime.
        if Arc::strong_count(&self.arc) <= 2 {
            locks.remove(&self.path);
        }
    }
}

/// Outcome of `DocumentTracker::disk_phase`: the version `ensure_open`'s
/// caller should end up synced to, and -- only when this call detected an
/// as-yet-uncommitted content change -- the content and disk snapshot to
/// commit alongside it.
struct Decision {
    uri: Uri,
    target_version: i32,
    fresh_content: Option<DocumentText>,
    snap: Option<DiskSync>,
}

impl Decision {
    /// A decision where nothing changed on disk this call: `target_version`
    /// is already what's committed in `DocumentState`.
    const fn unchanged(uri: Uri, target_version: i32) -> Self {
        Self {
            uri,
            target_version,
            fresh_content: None,
            snap: None,
        }
    }
}

/// Convert a file path to a URI.
///
/// Prefer `try_path_to_uri` on paths that come from configuration or
/// otherwise untrusted input; this wrapper exists for the common case of an
/// already-canonicalized path, where the conversion is not expected to fail
/// but must still surface as an error rather than a panic on unforeseen
/// inputs.
///
/// # Errors
///
/// Returns [`Error::InvalidUri`] if the path cannot be represented as a
/// `file://` URI.
pub fn path_to_uri(path: &Path) -> Result<Uri> {
    try_path_to_uri(path)
        .ok_or_else(|| Error::InvalidUri(format!("cannot convert path to URI: {}", path.display())))
}

/// Convert a file path to a URI, returning `None` if the path cannot be
/// represented as a `file://` URI.
///
/// Prefer this over [`path_to_uri`] on paths that come from configuration,
/// where a bad value should surface as an error rather than a panic.
#[must_use]
pub fn try_path_to_uri(path: &Path) -> Option<Uri> {
    let uri_string = encode_rfc3986_path_chars(&file_url(path)?);
    Some(Uri::from(uri_string))
}

#[cfg(not(windows))]
fn file_url(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

#[cfg(windows)]
fn file_url(path: &Path) -> Option<Url> {
    match Url::from_file_path(path) {
        Ok(file_url) => Some(file_url),
        Err(()) if path.has_root() => windows_rooted_path_to_file_url(path),
        Err(()) => None,
    }
}

#[cfg(windows)]
fn windows_rooted_path_to_file_url(path: &Path) -> Option<Url> {
    let path_str = path.to_string_lossy();
    let stripped = path_str.strip_prefix(r"\\?\").unwrap_or(&path_str);
    let mut file_url = Url::parse("file:///").ok()?;
    file_url.path_segments_mut().ok()?.clear().extend(
        stripped
            .split(['\\', '/'])
            .filter(|segment| !segment.is_empty()),
    );
    Some(file_url)
}

/// Percent-encodes the RFC 3986 §2.2 "other reserved" characters that the
/// `url` crate's default WHATWG path percent-encode set leaves untouched:
/// `[`, `]`, `^`, `|`. The remaining three characters in that set -- `{`,
/// `}`, and backtick -- are already encoded by `url` on serialization, so
/// they need no handling here; see
/// `test_path_to_uri_percent_encodes_all_rfc3986_other_reserved_chars`.
///
/// Shared with [`crate::bridge::resources::make_uri`] so `lsp-diagnostics://`
/// resource URIs get the same encoding as `file://` document URIs.
pub(super) fn encode_rfc3986_path_chars(url: &Url) -> String {
    let prefix = url[..url::Position::BeforePath].to_owned();
    let encoded = url[url::Position::BeforePath..]
        .replace('[', "%5B")
        .replace(']', "%5D")
        .replace('^', "%5E")
        .replace('|', "%7C");
    format!("{prefix}{encoded}")
}

/// Convert an LSP `file://` URI to an absolute filesystem path.
///
/// Returns `None` if the URI is not a valid `file://` URI, uses a non-file
/// scheme, or contains percent-encoding that cannot map to a valid path.
#[must_use]
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let url = Url::parse(uri.as_ref()).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    // Reject authority-bearing file URIs (e.g. `file://server/share`) to
    // avoid UNC path confusion on Windows.
    if !url.host_str().unwrap_or("").is_empty() {
        return None;
    }
    url.to_file_path().ok()
}

/// Detect the language ID from a file path.
///
/// Consults the extension map to determine the language ID for a file.
/// If the extension is not found in the map, returns "plaintext".
#[must_use]
pub fn detect_language(path: &Path, extension_map: &HashMap<String, String>) -> String {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    extension_map
        .get(extension)
        .cloned()
        .unwrap_or_else(|| "plaintext".to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_language() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());
        map.insert("py".to_string(), "python".to_string());
        map.insert("ts".to_string(), "typescript".to_string());

        assert_eq!(detect_language(Path::new("main.rs"), &map), "rust");
        assert_eq!(detect_language(Path::new("script.py"), &map), "python");
        assert_eq!(detect_language(Path::new("app.ts"), &map), "typescript");
        assert_eq!(detect_language(Path::new("unknown.xyz"), &map), "plaintext");
    }

    #[tokio::test]
    async fn test_document_tracker() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/file.rs");

        assert!(!tracker.is_open(&path));

        tracker
            .open(path.clone(), "fn main() {}".to_string())
            .unwrap();
        assert!(tracker.is_open(&path));
        assert_eq!(tracker.len(), 1);

        let state = tracker.get(&path).unwrap();
        assert_eq!(state.version(), 1);
        assert_eq!(state.language_id(), "rust");

        tracker.close(&path);
        assert!(!tracker.is_open(&path));
        assert!(tracker.is_empty());
    }

    /// #249: after a respawn, `forget_server` must clear only the respawned
    /// server's sync history so the next `ensure_open` call for it sends
    /// `didOpen` again -- while leaving other servers synced to the same
    /// document untouched (a path can be synced to more than one server,
    /// e.g. hover routed to one, diagnostics to another).
    #[test]
    fn test_forget_server_clears_only_that_servers_synced_version() {
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let path = PathBuf::from("/test/file.rs");
        tracker
            .open(path.clone(), "fn main() {}".to_string())
            .unwrap();

        let respawned = ServerId::from("rust-respawned");
        let untouched = ServerId::from("rust-diagnostics");
        lock_std(&tracker.documents)
            .get_mut(&path)
            .unwrap()
            .synced
            .insert(respawned.clone(), 1);
        lock_std(&tracker.documents)
            .get_mut(&path)
            .unwrap()
            .synced
            .insert(untouched.clone(), 1);

        tracker.forget_server(&respawned);

        let state = tracker.get(&path).unwrap();
        assert!(state.synced_version(&respawned).is_none());
        assert!(state.synced_version(&untouched).is_some());
    }

    /// #249 S1 regression: a `sync_phase` call that captured `server`'s
    /// generation *before* a concurrent `forget_server` bumped it must not
    /// commit its `synced` write, even though its notification against the
    /// now-superseded connection reports success (`fake_lsp_client`'s
    /// `DuplexStream` peer, held alive by the test's `FakeServer`, always
    /// accepts writes, standing in for the window where a server's process
    /// has already died but its message loop has not yet observed that).
    /// Without this, a document synced against the old (crashed) process
    /// would be wrongly marked as already open on the respawned one,
    /// permanently desyncing it.
    #[tokio::test]
    async fn test_sync_phase_skips_commit_when_generation_is_stale() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("race.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        set_mtime(&path, settled_past());

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let server = ServerId::from("rust");
        let generation_before_respawn = 0; // fresh tracker: generation starts at 0

        // A respawn happens "concurrently" with the in-flight call that
        // captured the generation above before this ran.
        tracker.forget_server(&server);

        let (stale_client, _guard) = fake_lsp_client();
        let decision = tracker.disk_phase(&path).await.unwrap();
        tracker
            .sync_phase(
                &path,
                &server,
                &stale_client,
                decision,
                generation_before_respawn,
            )
            .await
            .unwrap();

        let state = tracker.get(&path).unwrap();
        assert!(
            state.synced_version(&server).is_none(),
            "a sync_phase call that captured a stale generation must not \
             commit `synced`, even though its notify against the \
             superseded connection succeeded"
        );
    }

    /// Companion to the regression above: the ordinary, non-racing path
    /// (`ensure_open` capturing and committing against the *current*
    /// generation) must still work -- the generation check must not
    /// suppress a legitimate commit.
    #[tokio::test]
    async fn test_ensure_open_commits_when_generation_is_current() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("no_race.rs");
        std::fs::write(&path, "fn main() {}").unwrap();

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let server = ServerId::from("rust");
        let (client, _guard) = fake_lsp_client();

        tracker.ensure_open(&path, &server, &client).await.unwrap();

        let state = tracker.get(&path).unwrap();
        assert_eq!(state.synced_version(&server), Some(1));
    }

    /// Marks `path`'s tracked document as disk-verified, for a test that
    /// opens a document directly via `open` (bypassing `ensure_open`'s
    /// `disk_phase`, which is what normally sets this) but still needs it
    /// eligible for `evict_lru`'s LRU eviction -- disk-verified is a
    /// precondition for eviction, not just unlocked (#495 S4).
    fn mark_disk_verified(tracker: &DocumentTracker, path: &Path) {
        tracker.set_disk(
            path,
            DiskSync {
                mtime: None,
                size: 0,
                mtime_settled: false,
                content_checked_at: Instant::now(),
            },
        );
    }

    /// #495: at capacity with every existing document unlocked and
    /// disk-verified, `open` must evict the least-recently-used one to make
    /// room rather than fail -- its servers are recorded as owed a
    /// `didClose`.
    #[test]
    fn test_document_limit_evicts_lru_instead_of_failing() {
        let limits = ResourceLimits {
            max_documents: 2,
            max_file_size: 100,
        };
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(limits, map);

        tracker
            .open(PathBuf::from("/test/file1.rs"), "fn test1() {}".to_string())
            .unwrap();
        mark_disk_verified(&tracker, Path::new("/test/file1.rs"));
        tracker
            .open(PathBuf::from("/test/file2.rs"), "fn test2() {}".to_string())
            .unwrap();
        mark_disk_verified(&tracker, Path::new("/test/file2.rs"));

        tracker
            .open(PathBuf::from("/test/file3.rs"), "fn test3() {}".to_string())
            .unwrap();

        assert_eq!(tracker.len(), 2);
        assert!(!tracker.is_open(Path::new("/test/file1.rs")));
        assert!(tracker.is_open(Path::new("/test/file2.rs")));
        assert!(tracker.is_open(Path::new("/test/file3.rs")));

        assert!(
            tracker.pending_close_paths().is_empty(),
            "opened directly via `open`, never synced to any server"
        );
    }

    /// #495: `open` must fall back to `DocumentLimitExceeded` when every
    /// tracked document currently has an operation in flight against it
    /// (simulated here by inserting its `path_locks` entry directly, which
    /// is exactly what `evict_lru` checks for) -- evicting a locked document
    /// would pull it out from under that in-flight operation.
    #[test]
    fn test_document_limit_falls_back_to_error_when_only_candidate_is_locked() {
        let limits = ResourceLimits {
            max_documents: 1,
            max_file_size: 100,
        };
        let tracker = DocumentTracker::new(limits, HashMap::new());

        let locked_path = PathBuf::from("/test/locked.rs");
        tracker
            .open(locked_path.clone(), "fn locked() {}".to_string())
            .unwrap();
        lock_std(&tracker.path_locks).insert(locked_path.clone(), Arc::new(AsyncMutex::new(())));

        let result = tracker.open(PathBuf::from("/test/other.rs"), "fn other() {}".to_string());
        assert!(matches!(result, Err(Error::DocumentLimitExceeded { .. })));
        assert!(
            tracker.is_open(&locked_path),
            "the locked document must not be evicted"
        );
        assert!(tracker.pending_close_paths().is_empty());
    }

    /// Servers currently owed a `didClose` for `path`.
    fn pending_servers(tracker: &DocumentTracker, path: &Path) -> HashSet<ServerId> {
        lock_std(&tracker.pending_closes)
            .get(path)
            .map(|pending| pending.servers.clone())
            .unwrap_or_default()
    }

    /// Opens `name` through `ensure_open` so it is disk-verified (the only
    /// kind `evict_lru` will consider).
    async fn ensure_open_disk_verified(
        tracker: &DocumentTracker,
        dir: &TempDir,
        name: &str,
    ) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, name).unwrap();
        set_mtime(&path, settled_past());
        let (client, _server) = fake_lsp_client();
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        path
    }

    /// #503: a document held by an `InFlightGuard` is never evicted -- `open`
    /// falls back to `DocumentLimitExceeded` -- and becomes evictable the
    /// moment the guard drops.
    #[tokio::test]
    async fn test_open_does_not_evict_in_flight_document_until_guard_drops() {
        let dir = TempDir::new().unwrap();
        let limits = ResourceLimits {
            max_documents: 1,
            max_file_size: 0,
        };
        let tracker = DocumentTracker::new(limits, HashMap::new());
        let path_a = ensure_open_disk_verified(&tracker, &dir, "a.rs").await;

        let guard = tracker.mark_in_flight(&path_a);
        let path_b = dir.path().join("b.rs");
        std::fs::write(&path_b, "BBBB").unwrap();

        let result = tracker.open(path_b.clone(), "BBBB".to_string());
        assert!(matches!(result, Err(Error::DocumentLimitExceeded { .. })));
        assert!(tracker.is_open(&path_a));
        assert!(tracker.pending_close_paths().is_empty());

        drop(guard);
        tracker.open(path_b.clone(), "BBBB".to_string()).unwrap();
        assert!(!tracker.is_open(&path_a));
        assert!(tracker.is_open(&path_b));
        assert_eq!(tracker.pending_close_paths(), vec![path_a.clone()]);
        assert_eq!(
            pending_servers(&tracker, &path_a),
            HashSet::from([ServerId::from("rust")])
        );
    }

    /// #503: guards for one path stack -- dropping one leaves the path
    /// protected until the last is gone, and the bookkeeping entry is removed.
    #[tokio::test]
    async fn test_in_flight_guards_are_refcounted_per_path() {
        let dir = TempDir::new().unwrap();
        let limits = ResourceLimits {
            max_documents: 1,
            max_file_size: 0,
        };
        let tracker = DocumentTracker::new(limits, HashMap::new());
        let path_a = ensure_open_disk_verified(&tracker, &dir, "a.rs").await;

        let first = tracker.mark_in_flight(&path_a);
        let second = tracker.mark_in_flight(&path_a);
        drop(first);

        let path_b = dir.path().join("b.rs");
        let result = tracker.open(path_b.clone(), "BBBB".to_string());
        assert!(
            matches!(result, Err(Error::DocumentLimitExceeded { .. })),
            "one guard still outstanding"
        );

        drop(second);
        assert!(lock_std(&tracker.in_flight).is_empty());
        tracker.open(path_b, "BBBB".to_string()).unwrap();
        assert!(!tracker.is_open(&path_a));
    }

    /// #503: a guard held inside a future that is cancelled mid-await is
    /// released, so a timed-out handler cannot pin its document forever.
    #[tokio::test]
    async fn test_in_flight_guard_released_when_holding_future_is_cancelled() {
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let path = PathBuf::from("/test/cancelled.rs");

        let held = async {
            let _guard = tracker.mark_in_flight(&path);
            std::future::pending::<()>().await;
        };
        let timed_out = tokio::time::timeout(Duration::from_millis(10), held).await;

        assert!(timed_out.is_err());
        assert_eq!(tracker.in_flight_count(&path), 0);
    }

    /// #495 S4: a document with no disk-verified snapshot (`disk()` is
    /// `None`) must never be evicted, even though it is unlocked.
    #[test]
    fn test_evict_lru_skips_document_without_disk_snapshot() {
        let limits = ResourceLimits {
            max_documents: 1,
            max_file_size: 0,
        };
        let tracker = DocumentTracker::new(limits, HashMap::new());
        let first = PathBuf::from("/test/first.rs");
        tracker
            .open(first.clone(), "fn first() {}".to_string())
            .unwrap();

        let result = tracker.open(
            PathBuf::from("/test/second.rs"),
            "fn second() {}".to_string(),
        );
        assert!(matches!(result, Err(Error::DocumentLimitExceeded { .. })));
        assert!(
            tracker.is_open(&first),
            "the not-disk-verified document must not be evicted"
        );
        assert!(tracker.pending_close_paths().is_empty());
    }

    /// #495: `ensure_open` must bump a document's LRU recency (via
    /// `disk_phase`'s `touch`), so a document that was merely opened first
    /// but has since been re-accessed is not the one evicted -- eviction
    /// order must reflect actual usage, not just insertion order.
    #[tokio::test]
    async fn test_ensure_open_touch_changes_lru_eviction_order() {
        let dir = TempDir::new().unwrap();
        let path_a = dir.path().join("a.rs");
        let path_b = dir.path().join("b.rs");
        std::fs::write(&path_a, "AAAA").unwrap();
        std::fs::write(&path_b, "BBBB").unwrap();
        set_mtime(&path_a, settled_past());
        set_mtime(&path_b, settled_past());

        let limits = ResourceLimits {
            max_documents: 2,
            max_file_size: 0,
        };
        let (client, _server) = fake_lsp_client();
        let tracker = DocumentTracker::new(limits, HashMap::new());
        let server_id = ServerId::from("rust");

        tracker
            .ensure_open(&path_a, &server_id, &client)
            .await
            .unwrap();
        tracker
            .ensure_open(&path_b, &server_id, &client)
            .await
            .unwrap();

        // Re-access `a` so it becomes the more-recently-used of the two,
        // leaving `b` as the LRU entry despite having been opened second.
        tracker
            .ensure_open(&path_a, &server_id, &client)
            .await
            .unwrap();

        let path_c = dir.path().join("c.rs");
        std::fs::write(&path_c, "CCCC").unwrap();
        set_mtime(&path_c, settled_past());
        tracker
            .ensure_open(&path_c, &server_id, &client)
            .await
            .unwrap();

        assert!(
            tracker.is_open(&path_a),
            "recently re-accessed, must survive"
        );
        assert!(
            !tracker.is_open(&path_b),
            "least-recently-used, must be evicted"
        );
        assert!(tracker.is_open(&path_c));

        assert_eq!(tracker.pending_close_paths(), vec![path_b.clone()]);
        assert_eq!(
            pending_servers(&tracker, &path_b),
            HashSet::from([server_id])
        );
    }

    #[test]
    fn test_file_size_limit() {
        let limits = ResourceLimits {
            max_documents: 10,
            max_file_size: 10,
        };
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(limits, map);

        // Small file should succeed
        tracker
            .open(PathBuf::from("/test/small.rs"), "fn f(){}".to_string())
            .unwrap();

        // Large file should fail
        let large_content = "x".repeat(100);
        let result = tracker.open(PathBuf::from("/test/large.rs"), large_content);
        assert!(matches!(result, Err(Error::FileSizeLimitExceeded { .. })));
    }

    #[test]
    fn test_resource_limits_default() {
        let limits = ResourceLimits::default();
        assert_eq!(limits.max_documents, 100);
        assert_eq!(limits.max_file_size, 10 * 1024 * 1024);
    }

    #[test]
    fn test_resource_limits_custom() {
        let limits = ResourceLimits {
            max_documents: 50,
            max_file_size: 5 * 1024 * 1024,
        };
        assert_eq!(limits.max_documents, 50);
        assert_eq!(limits.max_file_size, 5 * 1024 * 1024);
    }

    #[test]
    fn test_resource_limits_zero_unlimited() {
        let limits = ResourceLimits {
            max_documents: 0,
            max_file_size: 0,
        };
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(limits, map);

        // Should allow many documents when limit is 0
        for i in 0..200 {
            tracker
                .open(
                    PathBuf::from(format!("/test/file{i}.rs")),
                    "content".to_string(),
                )
                .unwrap();
        }
        assert_eq!(tracker.len(), 200);

        // Should allow large files when limit is 0
        let huge_content = "x".repeat(100_000_000);
        tracker
            .open(PathBuf::from("/test/huge.rs"), huge_content)
            .unwrap();
    }

    #[test]
    fn test_document_state_clone() {
        let state = DocumentState {
            uri: Uri::from("file:///test.rs"),
            language_id: "rust".to_string(),
            version: 5,
            text: DocumentText::new("fn main() {}".to_string()),
            disk: None,
            synced: HashMap::new(),
            last_accessed: Instant::now(),
        };

        #[allow(clippy::redundant_clone)]
        let cloned = state.clone();
        assert_eq!(cloned.uri(), state.uri());
        assert_eq!(cloned.language_id(), state.language_id());
        assert_eq!(cloned.version(), 5);
        assert_eq!(cloned.content(), state.content());
    }

    #[test]
    fn test_close_nonexistent_document() {
        let map = HashMap::new();
        let tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/nonexistent.rs");

        let state = tracker.close(&path);
        assert_eq!(
            state, None,
            "Closing non-existent document should return None"
        );
    }

    #[test]
    fn test_get_nonexistent_document() {
        let map = HashMap::new();
        let tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/nonexistent.rs");

        let state = tracker.get(&path);
        assert!(
            state.is_none(),
            "Getting non-existent document should return None"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_detect_language_all_extensions() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());
        map.insert("py".to_string(), "python".to_string());
        map.insert("pyw".to_string(), "python".to_string());
        map.insert("pyi".to_string(), "python".to_string());
        map.insert("js".to_string(), "javascript".to_string());
        map.insert("mjs".to_string(), "javascript".to_string());
        map.insert("cjs".to_string(), "javascript".to_string());
        map.insert("ts".to_string(), "typescript".to_string());
        map.insert("mts".to_string(), "typescript".to_string());
        map.insert("cts".to_string(), "typescript".to_string());
        map.insert("tsx".to_string(), "typescriptreact".to_string());
        map.insert("jsx".to_string(), "javascriptreact".to_string());
        map.insert("go".to_string(), "go".to_string());
        map.insert("c".to_string(), "c".to_string());
        map.insert("h".to_string(), "c".to_string());
        map.insert("cpp".to_string(), "cpp".to_string());
        map.insert("cc".to_string(), "cpp".to_string());
        map.insert("cxx".to_string(), "cpp".to_string());
        map.insert("hpp".to_string(), "cpp".to_string());
        map.insert("hh".to_string(), "cpp".to_string());
        map.insert("hxx".to_string(), "cpp".to_string());
        map.insert("java".to_string(), "java".to_string());
        map.insert("rb".to_string(), "ruby".to_string());
        map.insert("php".to_string(), "php".to_string());
        map.insert("swift".to_string(), "swift".to_string());
        map.insert("kt".to_string(), "kotlin".to_string());
        map.insert("kts".to_string(), "kotlin".to_string());
        map.insert("scala".to_string(), "scala".to_string());
        map.insert("sc".to_string(), "scala".to_string());
        map.insert("zig".to_string(), "zig".to_string());
        map.insert("lua".to_string(), "lua".to_string());
        map.insert("sh".to_string(), "shellscript".to_string());
        map.insert("bash".to_string(), "shellscript".to_string());
        map.insert("zsh".to_string(), "shellscript".to_string());
        map.insert("json".to_string(), "json".to_string());
        map.insert("toml".to_string(), "toml".to_string());
        map.insert("yaml".to_string(), "yaml".to_string());
        map.insert("yml".to_string(), "yaml".to_string());
        map.insert("xml".to_string(), "xml".to_string());
        map.insert("html".to_string(), "html".to_string());
        map.insert("htm".to_string(), "html".to_string());
        map.insert("css".to_string(), "css".to_string());
        map.insert("scss".to_string(), "scss".to_string());
        map.insert("less".to_string(), "less".to_string());
        map.insert("md".to_string(), "markdown".to_string());
        map.insert("markdown".to_string(), "markdown".to_string());

        assert_eq!(detect_language(Path::new("main.rs"), &map), "rust");
        assert_eq!(detect_language(Path::new("script.py"), &map), "python");
        assert_eq!(detect_language(Path::new("script.pyw"), &map), "python");
        assert_eq!(detect_language(Path::new("script.pyi"), &map), "python");
        assert_eq!(detect_language(Path::new("app.js"), &map), "javascript");
        assert_eq!(detect_language(Path::new("app.mjs"), &map), "javascript");
        assert_eq!(detect_language(Path::new("app.cjs"), &map), "javascript");
        assert_eq!(detect_language(Path::new("app.ts"), &map), "typescript");
        assert_eq!(detect_language(Path::new("app.mts"), &map), "typescript");
        assert_eq!(detect_language(Path::new("app.cts"), &map), "typescript");
        assert_eq!(
            detect_language(Path::new("component.tsx"), &map),
            "typescriptreact"
        );
        assert_eq!(
            detect_language(Path::new("component.jsx"), &map),
            "javascriptreact"
        );
        assert_eq!(detect_language(Path::new("main.go"), &map), "go");
        assert_eq!(detect_language(Path::new("main.c"), &map), "c");
        assert_eq!(detect_language(Path::new("header.h"), &map), "c");
        assert_eq!(detect_language(Path::new("main.cpp"), &map), "cpp");
        assert_eq!(detect_language(Path::new("main.cc"), &map), "cpp");
        assert_eq!(detect_language(Path::new("main.cxx"), &map), "cpp");
        assert_eq!(detect_language(Path::new("header.hpp"), &map), "cpp");
        assert_eq!(detect_language(Path::new("header.hh"), &map), "cpp");
        assert_eq!(detect_language(Path::new("header.hxx"), &map), "cpp");
        assert_eq!(detect_language(Path::new("Main.java"), &map), "java");
        assert_eq!(detect_language(Path::new("script.rb"), &map), "ruby");
        assert_eq!(detect_language(Path::new("index.php"), &map), "php");
        assert_eq!(detect_language(Path::new("App.swift"), &map), "swift");
        assert_eq!(detect_language(Path::new("Main.kt"), &map), "kotlin");
        assert_eq!(detect_language(Path::new("script.kts"), &map), "kotlin");
        assert_eq!(detect_language(Path::new("Main.scala"), &map), "scala");
        assert_eq!(detect_language(Path::new("script.sc"), &map), "scala");
        assert_eq!(detect_language(Path::new("main.zig"), &map), "zig");
        assert_eq!(detect_language(Path::new("script.lua"), &map), "lua");
        assert_eq!(detect_language(Path::new("script.sh"), &map), "shellscript");
        assert_eq!(
            detect_language(Path::new("script.bash"), &map),
            "shellscript"
        );
        assert_eq!(
            detect_language(Path::new("script.zsh"), &map),
            "shellscript"
        );
        assert_eq!(detect_language(Path::new("data.json"), &map), "json");
        assert_eq!(detect_language(Path::new("config.toml"), &map), "toml");
        assert_eq!(detect_language(Path::new("config.yaml"), &map), "yaml");
        assert_eq!(detect_language(Path::new("config.yml"), &map), "yaml");
        assert_eq!(detect_language(Path::new("data.xml"), &map), "xml");
        assert_eq!(detect_language(Path::new("index.html"), &map), "html");
        assert_eq!(detect_language(Path::new("index.htm"), &map), "html");
        assert_eq!(detect_language(Path::new("styles.css"), &map), "css");
        assert_eq!(detect_language(Path::new("styles.scss"), &map), "scss");
        assert_eq!(detect_language(Path::new("styles.less"), &map), "less");
        assert_eq!(detect_language(Path::new("README.md"), &map), "markdown");
        assert_eq!(
            detect_language(Path::new("README.markdown"), &map),
            "markdown"
        );
        assert_eq!(detect_language(Path::new("unknown.xyz"), &map), "plaintext");
        assert_eq!(
            detect_language(Path::new("no_extension"), &map),
            "plaintext"
        );
    }

    #[test]
    fn test_path_to_uri_unix() {
        #[cfg(not(windows))]
        {
            let path = Path::new("/home/user/project/main.rs");
            let uri = path_to_uri(path).unwrap();
            assert!(
                uri.as_ref()
                    .starts_with("file:///home/user/project/main.rs")
            );
        }
    }

    #[test]
    fn test_path_to_uri_with_special_chars() {
        let path = Path::new("/home/user/project-test/main.rs");
        let uri = path_to_uri(path).unwrap();
        assert!(uri.as_ref().starts_with("file://"));
        assert!(uri.as_ref().contains("project-test"));
    }

    #[test]
    fn test_path_to_uri_percent_encodes_reserved_chars() {
        #[cfg(windows)]
        let path = Path::new(r"C:\home\user\routes\api\[...]^|.ts");
        #[cfg(not(windows))]
        let path = Path::new("/home/user/routes/api/[...]^|.ts");

        let uri = path_to_uri(path).unwrap();

        #[cfg(windows)]
        let expected = "file:///C:/home/user/routes/api/%5B...%5D%5E%7C.ts";
        #[cfg(not(windows))]
        let expected = "file:///home/user/routes/api/%5B...%5D%5E%7C.ts";

        assert_eq!(uri.as_ref(), expected);
        assert_eq!(
            uri_to_path(&uri).as_deref(),
            Some(path),
            "encoded file URI should round-trip to the original path"
        );
    }

    #[test]
    fn test_try_path_to_uri_returns_none_for_relative_path() {
        assert_eq!(try_path_to_uri(Path::new("relative/file.ts")), None);
    }

    /// #234 regression: `path_to_uri` must surface a conversion failure as
    /// `Err`, not panic -- the whole point of the fix was making this path
    /// testable instead of aborting the process.
    #[test]
    fn test_path_to_uri_returns_err_for_relative_path() {
        let err = path_to_uri(Path::new("relative/file.ts")).unwrap_err();
        assert!(matches!(err, Error::InvalidUri(_)));
    }

    #[cfg(windows)]
    #[test]
    fn test_try_path_to_uri_encodes_synthetic_windows_root() {
        let uri = try_path_to_uri(Path::new("/home/user/#work %23")).unwrap();

        assert_eq!(uri.as_ref(), "file:///home/user/%23work%20%2523");
    }

    /// A rooted-but-not-absolute Windows path (`\foo`, no drive/UNC prefix)
    /// satisfies `Path::has_root()` but not `Path::is_absolute()`.
    /// `file_url`'s `#[cfg(windows)]` variant deliberately falls back to
    /// `windows_rooted_path_to_file_url` on this exact case -- pinned here so
    /// a future change to `try_path_to_uri` (e.g. swapping the fallible
    /// `.parse()` this migration replaced for an `is_absolute()` guard)
    /// cannot silently narrow this without failing a test.
    #[cfg(windows)]
    #[test]
    fn test_try_path_to_uri_accepts_rooted_but_not_absolute_windows_path() {
        let path = Path::new(r"\foo");
        assert!(path.has_root());
        assert!(!path.is_absolute());

        let uri = try_path_to_uri(path).unwrap();

        assert_eq!(uri.as_ref(), "file:///foo");
    }

    #[test]
    fn test_path_to_uri_percent_encodes_reserved_chars_in_short_path() {
        // Regression: reserved chars near the URI start must still be encoded.
        #[cfg(windows)]
        let path = Path::new(r"C:\[a].ts");
        #[cfg(not(windows))]
        let path = Path::new("/[a].ts");

        let uri = path_to_uri(path).unwrap();

        assert!(
            uri.as_ref().ends_with("%5Ba%5D.ts"),
            "short path should percent-encode reserved chars, got {}",
            uri.as_ref()
        );
        assert_eq!(uri_to_path(&uri).as_deref(), Some(path));
    }

    #[test]
    fn test_path_to_uri_percent_encodes_all_rfc3986_other_reserved_chars() {
        // RFC 3986 §2.2 "other reserved" characters. The `url` crate already
        // percent-encodes `{`, `}`, and backtick when serializing; `[`, `]`,
        // `^`, `|` are handled explicitly by `encode_rfc3986_path_chars`.
        #[cfg(windows)]
        let path = Path::new(r"C:\home\user\test[]^|{}`.ts");
        #[cfg(not(windows))]
        let path = Path::new("/home/user/test[]^|{}`.ts");

        let uri = try_path_to_uri(path).unwrap();
        let uri_str = uri.as_ref();

        for (raw, encoded) in [
            ('[', "%5B"),
            (']', "%5D"),
            ('^', "%5E"),
            ('|', "%7C"),
            ('{', "%7B"),
            ('}', "%7D"),
            ('`', "%60"),
        ] {
            assert!(
                uri_str.contains(encoded),
                "expected {raw:?} to be percent-encoded as {encoded} in {uri_str}"
            );
        }
        assert!(
            !uri_str.contains(['[', ']', '^', '|', '{', '}', '`']),
            "no raw reserved characters should remain in {uri_str}"
        );
    }

    #[tokio::test]
    async fn test_document_tracker_concurrent_operations() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path1 = PathBuf::from("/test/file1.rs");
        let path2 = PathBuf::from("/test/file2.rs");

        tracker.open(path1.clone(), "content1".to_string()).unwrap();
        tracker.open(path2.clone(), "content2".to_string()).unwrap();

        assert_eq!(tracker.len(), 2);
        assert!(tracker.is_open(&path1));
        assert!(tracker.is_open(&path2));

        assert_eq!(tracker.get(&path1).unwrap().content(), "content1");
        assert_eq!(tracker.get(&path2).unwrap().content(), "content2");

        tracker.close(&path1);
        assert_eq!(tracker.len(), 1);
        assert!(!tracker.is_open(&path1));
        assert!(tracker.is_open(&path2));
    }

    #[test]
    fn test_empty_content() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/empty.rs");

        tracker.open(path.clone(), String::new()).unwrap();
        assert!(tracker.is_open(&path));
        assert_eq!(tracker.get(&path).unwrap().content(), "");
    }

    #[test]
    fn test_unicode_content() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/unicode.rs");
        let content = "fn テスト() { println!(\"こんにちは\"); }";

        tracker.open(path.clone(), content.to_string()).unwrap();
        assert_eq!(tracker.get(&path).unwrap().content(), content);
    }

    /// #495: at exactly `max_documents`, `open` must evict the LRU entry
    /// (here `file0`, the first opened) rather than fail, since none of the
    /// existing documents are locked and all are disk-verified.
    #[test]
    fn test_document_limit_exact_boundary() {
        let limits = ResourceLimits {
            max_documents: 5,
            max_file_size: 1000,
        };
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(limits, map);

        for i in 0..5 {
            let path = PathBuf::from(format!("/test/file{i}.rs"));
            tracker.open(path.clone(), "content".to_string()).unwrap();
            mark_disk_verified(&tracker, &path);
        }

        assert_eq!(tracker.len(), 5);

        tracker
            .open(PathBuf::from("/test/file6.rs"), "content".to_string())
            .unwrap();

        assert_eq!(tracker.len(), 5);
        assert!(!tracker.is_open(Path::new("/test/file0.rs")));
        assert!(tracker.is_open(Path::new("/test/file6.rs")));
    }

    #[test]
    fn test_file_size_exact_boundary() {
        let limits = ResourceLimits {
            max_documents: 10,
            max_file_size: 100,
        };
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(limits, map);

        let exact_size_content = "x".repeat(100);
        tracker
            .open(PathBuf::from("/test/exact.rs"), exact_size_content)
            .unwrap();

        let over_size_content = "x".repeat(101);
        let result = tracker.open(PathBuf::from("/test/over.rs"), over_size_content);
        assert!(matches!(result, Err(Error::FileSizeLimitExceeded { .. })));
    }

    #[test]
    fn test_detect_language_with_custom_extension() {
        let mut map = HashMap::new();
        map.insert("nu".to_string(), "nushell".to_string());

        assert_eq!(detect_language(Path::new("script.nu"), &map), "nushell");

        let empty_map = HashMap::new();
        assert_eq!(
            detect_language(Path::new("script.nu"), &empty_map),
            "plaintext"
        );
    }

    #[test]
    fn test_detect_language_custom_overrides_default() {
        let mut custom_map = HashMap::new();
        custom_map.insert("rs".to_string(), "custom-rust".to_string());

        assert_eq!(
            detect_language(Path::new("main.rs"), &custom_map),
            "custom-rust"
        );

        let mut default_map = HashMap::new();
        default_map.insert("rs".to_string(), "rust".to_string());

        assert_eq!(detect_language(Path::new("main.rs"), &default_map), "rust");
    }

    #[test]
    fn test_detect_language_fallback_to_plaintext() {
        let mut map = HashMap::new();
        map.insert("nu".to_string(), "nushell".to_string());

        // .rs not in custom map, should return plaintext
        assert_eq!(detect_language(Path::new("main.rs"), &map), "plaintext");
    }

    #[test]
    fn test_detect_language_empty_map() {
        let map = HashMap::new();
        assert_eq!(detect_language(Path::new("main.rs"), &map), "plaintext");
    }

    #[test]
    fn test_document_tracker_with_extensions() {
        let mut map = HashMap::new();
        map.insert("nu".to_string(), "nushell".to_string());

        let tracker = DocumentTracker::new(ResourceLimits::default(), map);

        let path = PathBuf::from("/test/script.nu");
        tracker
            .open(path.clone(), "# nushell script".to_string())
            .unwrap();

        let state = tracker.get(&path).unwrap();
        assert_eq!(state.language_id(), "nushell");
    }

    #[test]
    fn test_document_tracker_uses_provided_map() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/main.rs");
        tracker
            .open(path.clone(), "fn main() {}".to_string())
            .unwrap();

        let state = tracker.get(&path).unwrap();
        assert_eq!(state.language_id(), "rust");
    }

    #[test]
    fn test_multiple_extensions_same_language() {
        let mut map = HashMap::new();
        map.insert("cpp".to_string(), "c++".to_string());
        map.insert("cc".to_string(), "c++".to_string());
        map.insert("cxx".to_string(), "c++".to_string());

        assert_eq!(detect_language(Path::new("main.cpp"), &map), "c++");
        assert_eq!(detect_language(Path::new("main.cc"), &map), "c++");
        assert_eq!(detect_language(Path::new("main.cxx"), &map), "c++");
    }

    #[test]
    fn test_case_sensitive_extensions() {
        let mut map = HashMap::new();
        map.insert("NU".to_string(), "nushell".to_string());

        // Lowercase .nu should not match uppercase "NU" in map
        assert_eq!(detect_language(Path::new("script.nu"), &map), "plaintext");
    }

    // ------------------------------------------------------------------
    // uri_to_path
    // ------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn test_uri_to_path_file_scheme() {
        let uri: Uri = Uri::from("file:///home/user/main.rs");
        let path = uri_to_path(&uri).unwrap();
        assert_eq!(path, PathBuf::from("/home/user/main.rs"));
    }

    #[test]
    fn test_uri_to_path_non_file_scheme_returns_none() {
        let uri: Uri = Uri::from("https://example.com/file.rs");
        assert!(uri_to_path(&uri).is_none());
    }

    #[test]
    fn test_uri_to_path_lsp_diagnostics_scheme_returns_none() {
        // Custom scheme must not be decoded by uri_to_path.
        let uri: Uri = Uri::from("lsp-diagnostics:///home/user/main.rs");
        assert!(uri_to_path(&uri).is_none());
    }

    #[test]
    fn test_uri_to_path_with_authority_returns_none() {
        // Authority-bearing file URIs must be rejected (UNC path defence).
        // lsp_types::Uri may or may not accept this string; either way
        // uri_to_path should return None.
        let result = uri_to_path(&Uri::from("file://server/share/path.rs"));
        assert!(result.is_none());
    }

    // ------------------------------------------------------------------
    // open_paths
    // ------------------------------------------------------------------

    #[test]
    fn test_open_paths_empty_tracker() {
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        assert_eq!(tracker.open_paths().len(), 0);
    }

    #[test]
    fn test_open_paths_populated_tracker() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());
        let tracker = DocumentTracker::new(ResourceLimits::default(), map);
        tracker.open(PathBuf::from("/a.rs"), String::new()).unwrap();
        tracker.open(PathBuf::from("/b.rs"), String::new()).unwrap();
        let mut paths = tracker.open_paths();
        paths.sort();
        assert_eq!(paths, [PathBuf::from("/a.rs"), PathBuf::from("/b.rs")]);
    }

    #[test]
    fn test_open_paths_after_close() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());
        let tracker = DocumentTracker::new(ResourceLimits::default(), map);
        tracker.open(PathBuf::from("/a.rs"), String::new()).unwrap();
        tracker.close(Path::new("/a.rs"));
        assert_eq!(tracker.open_paths().len(), 0);
    }

    // ------------------------------------------------------------------
    // ensure_open resync (issue #102)
    // ------------------------------------------------------------------

    use tempfile::TempDir;
    use tokio::io::BufReader;

    use crate::test_lsp::{fake_lsp_client, read_framed_message};

    /// Backdates or forwards a file's mtime for deterministic disk-sync tests.
    ///
    /// Opened with `write(true)` rather than [`std::fs::File::open`]: on
    /// Windows, `set_modified` needs a handle with write access, and a
    /// read-only handle fails with `PermissionDenied` (Unix's
    /// `utimensat`-based implementation has no such requirement, which is
    /// why a read-only handle works there).
    fn set_mtime(path: &Path, time: SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    fn settled_past() -> SystemTime {
        SystemTime::now() - Duration::from_secs(10)
    }

    #[test]
    fn test_mtime_settled_boundary() {
        let read_at = SystemTime::now();
        assert!(!mtime_settled(None, read_at), "no mtime is never settled");
        assert!(
            mtime_settled(Some(read_at - Duration::from_secs(3)), read_at),
            "3s older than read_at is past the 2s granularity margin"
        );
        assert!(
            !mtime_settled(Some(read_at - Duration::from_secs(1)), read_at),
            "1s older than read_at is within the 2s granularity margin"
        );
        assert!(
            !mtime_settled(Some(read_at + Duration::from_secs(10)), read_at),
            "an mtime after read_at is never settled"
        );
    }

    #[tokio::test]
    async fn test_ensure_open_unchanged_file_is_fast_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        set_mtime(&path, settled_past());

        let (client, _server) = fake_lsp_client();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());

        let uri1 = tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        assert_eq!(tracker.get(&path).unwrap().version(), 1);

        let uri2 = tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        assert_eq!(uri1, uri2);
        assert_eq!(tracker.get(&path).unwrap().version(), 1);
        assert_eq!(tracker.get(&path).unwrap().content(), "fn main() {}");
    }

    #[tokio::test]
    async fn test_ensure_open_resyncs_on_size_change() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        set_mtime(&path, settled_past());

        let (client, _server) = fake_lsp_client();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();

        std::fs::write(&path, "fn main() { println!(\"hi\"); }").unwrap();
        set_mtime(&path, settled_past());

        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        let state = tracker.get(&path).unwrap();
        assert_eq!(state.version(), 2);
        assert_eq!(state.content(), "fn main() { println!(\"hi\"); }");
    }

    #[tokio::test(start_paused = true)]
    async fn test_ensure_open_regression_102_103_racy_same_size_rewrite() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "AAAA").unwrap();
        // Leave the mtime at "now" (racy) rather than backdating it.

        let (client, _server) = fake_lsp_client();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        let original_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

        // Same-length rewrite with the mtime forced back to the recorded
        // value -- exactly the same-tick rewrite issue #102/#103 missed.
        std::fs::write(&path, "BBBB").unwrap();
        set_mtime(&path, original_mtime);

        tokio::time::advance(DISK_CHECK_DEBOUNCE + Duration::from_millis(1)).await;

        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        let state = tracker.get(&path).unwrap();
        assert_eq!(
            state.version(),
            2,
            "must resync despite identical (mtime, size)"
        );
        assert_eq!(state.content(), "BBBB");
    }

    #[tokio::test(start_paused = true)]
    async fn test_ensure_open_regression_102_103_settled_mtime_is_the_documented_limit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "AAAA").unwrap();
        set_mtime(&path, settled_past());

        let (client, _server) = fake_lsp_client();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        let original_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

        // Same-length rewrite restoring an already-settled mtime: this is
        // the documented residual limitation (e.g. `tar x`, `rsync -a`),
        // not a bug -- it is out of reach without hashing on every access.
        std::fs::write(&path, "BBBB").unwrap();
        set_mtime(&path, original_mtime);

        tokio::time::advance(DISK_CHECK_DEBOUNCE + Duration::from_millis(1)).await;

        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        let state = tracker.get(&path).unwrap();
        assert_eq!(state.version(), 1, "documented limitation: fast path taken");
        assert_eq!(state.content(), "AAAA");
    }

    #[tokio::test(start_paused = true)]
    async fn test_ensure_open_stat_is_never_debounced() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "AAAA").unwrap();
        set_mtime(&path, settled_past());

        let (client, _server) = fake_lsp_client();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();

        // Different-size rewrite with no time advance at all: must resync
        // immediately, proving the debounce never gates the stat itself.
        std::fs::write(&path, "BBBBBBBB").unwrap();
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();

        let state = tracker.get(&path).unwrap();
        assert_eq!(state.version(), 2);
        assert_eq!(state.content(), "BBBBBBBB");
    }

    #[tokio::test(start_paused = true)]
    async fn test_ensure_open_debounce_gates_reread_only() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "AAAA").unwrap();
        // Racy: leave the mtime at "now".

        let (client, _server) = fake_lsp_client();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        let original_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

        std::fs::write(&path, "BBBB").unwrap(); // same size
        set_mtime(&path, original_mtime); // stat matches, entry stays racy

        // Inside the debounce window: the re-read is gated, cache wins.
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        assert_eq!(tracker.get(&path).unwrap().version(), 1);

        tokio::time::advance(Duration::from_millis(300)).await;
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        let state = tracker.get(&path).unwrap();
        assert_eq!(state.version(), 2);
        assert_eq!(state.content(), "BBBB");
    }

    #[tokio::test]
    async fn test_ensure_open_deleted_file_errors_state_untouched() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        set_mtime(&path, settled_past());

        let (client, _server) = fake_lsp_client();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();

        std::fs::remove_file(&path).unwrap();

        let result = tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await;
        assert!(matches!(result, Err(Error::FileIo { .. })));
        assert!(tracker.is_open(&path));
        assert_eq!(tracker.get(&path).unwrap().version(), 1);
        assert_eq!(tracker.get(&path).unwrap().content(), "fn main() {}");
    }

    #[tokio::test]
    async fn test_ensure_open_grows_past_limit_errors_state_intact() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "small").unwrap();
        set_mtime(&path, settled_past());

        let limits = ResourceLimits {
            max_documents: 10,
            max_file_size: 10,
        };
        let (client, _server) = fake_lsp_client();
        let tracker = DocumentTracker::new(limits, HashMap::new());
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();

        std::fs::write(&path, "x".repeat(100)).unwrap();

        let result = tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await;
        assert!(matches!(result, Err(Error::FileSizeLimitExceeded { .. })));
        assert_eq!(tracker.get(&path).unwrap().content(), "small");
        assert_eq!(tracker.get(&path).unwrap().version(), 1);
    }

    #[tokio::test]
    async fn test_ensure_open_resync_at_document_capacity() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "AAAA").unwrap();
        set_mtime(&path, settled_past());

        let limits = ResourceLimits {
            max_documents: 1,
            max_file_size: 0,
        };
        let (client, _server) = fake_lsp_client();
        let tracker = DocumentTracker::new(limits, HashMap::new());
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        assert_eq!(tracker.len(), 1);

        std::fs::write(&path, "BBBBBBBB").unwrap();
        let result = tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await;
        assert!(
            result.is_ok(),
            "resync must not re-run the doc-count check on an already-tracked path"
        );
        assert_eq!(tracker.len(), 1);
        assert_eq!(tracker.get(&path).unwrap().version(), 2);
    }

    #[tokio::test]
    async fn test_first_open_self_heals_when_did_open_notify_fails() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}").unwrap();

        let (client, _server) = fake_lsp_client();
        // A clone shares the same command channel. Shutting down the
        // original (which owns the receiver task) blocks until the
        // background message loop has fully exited and dropped that
        // channel's receiver -- so the clone's next `notify()` fails
        // deterministically, with no race against process teardown.
        let notify_will_fail = client.clone();
        client.shutdown().await.unwrap();

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let result = tracker
            .ensure_open(&path, &ServerId::from("rust"), &notify_will_fail)
            .await;

        assert!(result.is_err(), "notify failure must propagate as an error");
        assert!(
            !tracker.is_open(&path),
            "a failed didOpen must not leave the document tracked, or the server \
             and tracker would stay permanently desynced"
        );
    }

    #[tokio::test]
    async fn test_resync_sends_didchange_with_full_replacement_over_the_wire() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        set_mtime(&path, settled_past());

        let (client, mut server) = fake_lsp_client();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");

        std::fs::write(&path, "fn main() { println!(\"hi\"); }").unwrap();
        set_mtime(&path, settled_past());
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();

        let changed = read_framed_message(&mut wire).await;
        assert_eq!(changed["method"], "textDocument/didChange");
        let params = &changed["params"];
        assert_eq!(params["textDocument"]["version"], 2);
        let change = &params["contentChanges"][0];
        assert!(
            change.get("range").is_none(),
            "range must be omitted, not null, for a full-replacement change"
        );
        assert!(
            change.get("rangeLength").is_none(),
            "rangeLength must be omitted, not null, for a full-replacement change"
        );
        assert_eq!(change["text"], "fn main() { println!(\"hi\"); }");
    }

    /// Regression for #174 §7.1: a second server must receive `didOpen` even
    /// when the file has not changed since a first server was opened on it --
    /// the disk-phase fast path only skips the disk read, never the
    /// per-server sync decision. Exercises the settled-mtime fast path.
    #[tokio::test]
    async fn test_ensure_open_second_server_gets_didopen_no_disk_change() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        set_mtime(&path, settled_past());

        let (client_a, mut server_a) = fake_lsp_client();
        let (client_b, mut server_b) = fake_lsp_client();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());

        let id_a = ServerId::from("server-a");
        let id_b = ServerId::from("server-b");

        tracker.ensure_open(&path, &id_a, &client_a).await.unwrap();
        let mut wire_a = BufReader::new(&mut server_a.write_stdout);
        let opened_a = read_framed_message(&mut wire_a).await;
        assert_eq!(opened_a["method"], "textDocument/didOpen");

        // No disk change between calls: server B's ensure_open must still
        // take the disk-phase fast path (settled mtime) but still send B its
        // own didOpen.
        tracker.ensure_open(&path, &id_b, &client_b).await.unwrap();
        let mut wire_b = BufReader::new(&mut server_b.write_stdout);
        let opened_b = read_framed_message(&mut wire_b).await;
        assert_eq!(opened_b["method"], "textDocument/didOpen");
        assert_eq!(opened_b["params"]["textDocument"]["version"], 1);
        assert_eq!(opened_b["params"]["textDocument"]["text"], "fn main() {}");
    }

    /// Same as above but through the unchanged-content re-read path (racy,
    /// unsettled mtime past the debounce window, forcing a real content
    /// compare) rather than the settled-mtime fast path.
    #[tokio::test(start_paused = true)]
    async fn test_ensure_open_second_server_gets_didopen_unchanged_content_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        // Leave mtime racy (unsettled) rather than backdating it.

        let (client_a, _server_a) = fake_lsp_client();
        let (client_b, mut server_b) = fake_lsp_client();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());

        tracker
            .ensure_open(&path, &ServerId::from("server-a"), &client_a)
            .await
            .unwrap();

        // Past the debounce window: server B's call must genuinely re-read
        // and compare content rather than taking either fast-path leg.
        tokio::time::advance(DISK_CHECK_DEBOUNCE + Duration::from_millis(1)).await;

        tracker
            .ensure_open(&path, &ServerId::from("server-b"), &client_b)
            .await
            .unwrap();
        let mut wire_b = BufReader::new(&mut server_b.write_stdout);
        let opened_b = read_framed_message(&mut wire_b).await;
        assert_eq!(opened_b["method"], "textDocument/didOpen");
    }

    /// Regression for #174 §6.2/§12: `prepare_call_hierarchy` and
    /// `incoming_calls`/`outgoing_calls` must resolve to the same server, since
    /// only `prepare` calls `ensure_open` -- pinned here at the tracker level
    /// by asserting a second `ensure_open` for the same server is a no-op
    /// once synced, so a caller that reuses the same `ServerId` for both
    /// calls never double-opens.
    #[tokio::test]
    async fn test_ensure_open_same_server_twice_sends_nothing_second_time() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        set_mtime(&path, settled_past());

        let (client, mut server) = fake_lsp_client();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let id = ServerId::from("rust");

        tracker.ensure_open(&path, &id, &client).await.unwrap();
        tracker.ensure_open(&path, &id, &client).await.unwrap();

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        assert_eq!(
            tracker.get(&path).unwrap().synced_version(&id),
            Some(1),
            "second call for the same server must not re-open or re-change"
        );
    }

    /// Regression for #174 §7.2/S6: a failing `didChange` for one server must
    /// leave that server's `synced` entry untouched (self-heals on retry)
    /// without disturbing another server that already synced successfully.
    #[tokio::test]
    async fn test_sync_phase_failed_didchange_does_not_disturb_other_server() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        set_mtime(&path, settled_past());

        let (client_a, _server_a) = fake_lsp_client();
        let (client_b, _server_b) = fake_lsp_client();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let id_a = ServerId::from("server-a");
        let id_b = ServerId::from("server-b");

        tracker.ensure_open(&path, &id_a, &client_a).await.unwrap();
        tracker.ensure_open(&path, &id_b, &client_b).await.unwrap();

        // Shut down B's client so its next notify fails, then change the file
        // so both servers have version 2 to catch up to.
        let client_b_will_fail = client_b.clone();
        client_b.shutdown().await.unwrap();

        std::fs::write(&path, "fn main() { updated(); }").unwrap();
        set_mtime(&path, settled_past());

        let result = tracker.ensure_open(&path, &id_b, &client_b_will_fail).await;
        assert!(result.is_err(), "B's didChange must fail and propagate");

        // No commit happens before a successful notify: content, version and
        // both servers' `synced` entries all stay exactly as they were
        // before this call, so the next attempt retries from the same
        // starting point rather than drifting the tracker out of sync with
        // what was actually acknowledged over the wire.
        assert!(tracker.is_open(&path));
        assert_eq!(tracker.get(&path).unwrap().content(), "fn main() {}");
        assert_eq!(tracker.get(&path).unwrap().version(), 1);
        assert_eq!(tracker.get(&path).unwrap().synced_version(&id_a), Some(1));
        assert_eq!(tracker.get(&path).unwrap().synced_version(&id_b), Some(1));

        // A's next call must independently detect the disk change (B's
        // failure did not consume it) and successfully advance both the
        // shared content/version and its own synced entry.
        tracker.ensure_open(&path, &id_a, &client_a).await.unwrap();
        assert_eq!(
            tracker.get(&path).unwrap().content(),
            "fn main() { updated(); }"
        );
        assert_eq!(tracker.get(&path).unwrap().synced_version(&id_a), Some(2));
        assert_eq!(tracker.get(&path).unwrap().synced_version(&id_b), Some(1));
    }

    // ------------------------------------------------------------------
    // ensure_open concurrency (issue #227)
    // ------------------------------------------------------------------

    /// Regression for #227: `ensure_open` for one path must not block
    /// `ensure_open` for an unrelated path, even while the first call is
    /// stuck inside its own disk I/O.
    ///
    /// Path A's own `ensure_open` call is genuinely parked on path A's
    /// per-path lock: `path_a_guard` (held via `lock_path`, the exact
    /// primitive `ensure_open` acquires before its disk I/O) is taken first,
    /// then a *real*, spawned `ensure_open(path_a)` call is raced against
    /// it, so the serialization point under test is inside `ensure_open`
    /// itself, not merely the standalone `lock_path` guard. Previously this
    /// used a FIFO, whose `open()` for read blocked deterministically until
    /// a writer connected; that is no longer usable for this purpose now
    /// that `open_checked` opens with `O_NONBLOCK` and rejects non-regular
    /// files immediately (see #418) -- a FIFO can no longer be coaxed into
    /// blocking `ensure_open`'s `open()` call at all. Under the old design
    /// (a single lock spanning all of `ensure_open`, including disk I/O),
    /// path B would hang until path A's lock is released below; the
    /// per-path lock added here must let it through immediately instead.
    #[tokio::test]
    async fn test_ensure_open_different_paths_do_not_serialize() {
        let dir = TempDir::new().unwrap();
        let path_a = dir.path().join("a.rs");
        let path_b = dir.path().join("b.rs");

        std::fs::write(&path_a, "fn a() {}").unwrap();
        std::fs::write(&path_b, "fn b() {}").unwrap();
        set_mtime(&path_a, settled_past());
        set_mtime(&path_b, settled_past());

        let (client_a, _server_a) = fake_lsp_client();
        let (client_b, _server_b) = fake_lsp_client();
        let tracker = Arc::new(DocumentTracker::new(
            ResourceLimits::default(),
            HashMap::new(),
        ));

        let path_a_guard = tracker.lock_path(&path_a).await;

        // Spawned so a real `ensure_open(path_a)` call is genuinely parked
        // on path A's lock (held by `path_a_guard` above) while path B's
        // call below runs.
        let tracker_for_a = Arc::clone(&tracker);
        let path_a_for_task = path_a.clone();
        let handle_a = tokio::spawn(async move {
            tracker_for_a
                .ensure_open(&path_a_for_task, &ServerId::from("server-a"), &client_a)
                .await
        });

        // Give the spawned task a chance to actually reach and block on
        // path A's lock before racing path B's call against it below.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // A `timeout` error here means path B is blocked by path A's stuck
        // ensure_open -- the exact regression #227 fixes.
        tokio::time::timeout(
            Duration::from_secs(5),
            tracker.ensure_open(&path_b, &ServerId::from("server-b"), &client_b),
        )
        .await
        .unwrap()
        .unwrap();

        drop(path_a_guard);

        handle_a.await.unwrap().unwrap();
        assert_eq!(tracker.get(&path_a).unwrap().content(), "fn a() {}");
    }

    /// Regression for #227: N concurrent `ensure_open` calls for the same
    /// path and the same server must still collapse into exactly one
    /// `didOpen` -- the per-path lock introduced to let different paths run
    /// concurrently must not weaken the existing same-path serialization
    /// that prevents duplicate opens.
    #[tokio::test]
    async fn test_ensure_open_concurrent_same_path_single_didopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        set_mtime(&path, settled_past());

        let (client, mut server) = fake_lsp_client();
        let tracker = Arc::new(DocumentTracker::new(
            ResourceLimits::default(),
            HashMap::new(),
        ));
        let id = ServerId::from("rust");

        let mut handles = Vec::new();
        for _ in 0..8 {
            let tracker = Arc::clone(&tracker);
            let client = client.clone();
            let path = path.clone();
            let id = id.clone();
            handles.push(tokio::spawn(async move {
                tracker.ensure_open(&path, &id, &client).await
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");

        // No further notification should have been queued -- proves the 8
        // concurrent callers collapsed into exactly one `didOpen`.
        let extra =
            tokio::time::timeout(Duration::from_millis(200), read_framed_message(&mut wire)).await;
        assert!(
            extra.is_err(),
            "expected no additional notification after the single didOpen"
        );

        assert_eq!(tracker.get(&path).unwrap().synced_version(&id), Some(1));
        assert_eq!(tracker.get(&path).unwrap().version(), 1);
    }

    /// Regression for #227: `lock_path`'s guard must evict its `path_locks`
    /// entry once no caller is left waiting on it, or the map grows by one
    /// entry per distinct path ever opened for the lifetime of the process.
    /// Exercises three concurrent distinct paths (not just the two used in
    /// `test_ensure_open_different_paths_do_not_serialize`) to rule out an
    /// eviction bug that only manifests with more than two live entries.
    #[tokio::test]
    async fn test_ensure_open_path_locks_evicted_after_completion() {
        let dir = TempDir::new().unwrap();
        let paths: Vec<_> = ["a.rs", "b.rs", "c.rs"]
            .iter()
            .map(|name| dir.path().join(name))
            .collect();
        for path in &paths {
            std::fs::write(path, "fn f() {}").unwrap();
            set_mtime(path, settled_past());
        }

        let tracker = Arc::new(DocumentTracker::new(
            ResourceLimits::default(),
            HashMap::new(),
        ));
        let id = ServerId::from("rust");

        let mut handles = Vec::new();
        let mut servers = Vec::new();
        for path in paths.clone() {
            let tracker = Arc::clone(&tracker);
            let (client, server) = fake_lsp_client();
            servers.push(server);
            let id = id.clone();
            handles.push(tokio::spawn(async move {
                tracker.ensure_open(&path, &id, &client).await
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        drop(servers);

        assert!(
            lock_std(&tracker.path_locks).is_empty(),
            "path_locks must be fully evicted once every ensure_open call \
             for every path has completed, otherwise the map grows \
             unbounded for the lifetime of the process"
        );
    }

    /// Regression for #418: `read_to_string_checked` must reject a FIFO
    /// rather than trust its (always-zero) reported size and either hang
    /// reading it or return an unbounded stream of bytes.
    ///
    /// Unlike `test_ensure_open_different_paths_do_not_serialize`'s use of
    /// the same `mkfifo` idiom, this test needs no background writer and no
    /// timeout race to prove non-blocking behavior: `open_checked`'s
    /// `O_NONBLOCK` open is the fix under test, so a correct implementation
    /// returns an error immediately, with no peer ever connecting. The
    /// outer `timeout` is only a safety net so a regression here fails fast
    /// instead of hanging the test suite.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_to_string_checked_rejects_fifo() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo must succeed to set up this test");

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        // A timeout here means the fix failed and open() is still blocking
        // indefinitely on the FIFO -- the exact regression #418 fixes.
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            tracker.read_to_string_checked(&path),
        )
        .await
        .unwrap();

        assert!(matches!(result, Err(Error::NotARegularFile(_))));
    }

    /// Direct regression for #442: `check_disk_file_type` itself, isolated
    /// from `open_checked`'s surrounding `is_file()` check. Unlike
    /// `test_read_to_string_checked_rejects_nul_device` below, this fails if
    /// `check_disk_file_type` were ever bypassed or deleted -- both checks
    /// currently produce the identical `Error::NotARegularFile` variant, so
    /// an end-to-end test alone can't tell them apart.
    #[cfg(windows)]
    #[tokio::test]
    async fn test_check_disk_file_type_accepts_regular_rejects_nul() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("regular.txt");
        std::fs::write(&path, "hello").unwrap();

        let regular = fs::File::open(&path).await.unwrap();
        assert!(check_disk_file_type(&regular, &path).await.is_ok());

        let nul_path = PathBuf::from("NUL");
        let nul = fs::File::open(&nul_path).await.unwrap();
        assert!(matches!(
            check_disk_file_type(&nul, &nul_path).await,
            Err(Error::NotARegularFile(_))
        ));
    }

    /// Regression for #442: `read_to_string_checked` must reject the `NUL`
    /// device on Windows via `GetFileType`, not `FileType::is_file()` --
    /// which does not reliably classify reserved device names as
    /// non-regular. This is the Windows counterpart of
    /// `test_read_to_string_checked_rejects_fifo`; `NUL` opens immediately
    /// (unlike a FIFO with no writer), so the outer `timeout` here is only a
    /// safety net, not proof of non-blocking behavior on its own -- the
    /// `GetFileType` check itself is what's under test.
    #[cfg(windows)]
    #[tokio::test]
    async fn test_read_to_string_checked_rejects_nul_device() {
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let path = PathBuf::from("NUL");
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            tracker.read_to_string_checked(&path),
        )
        .await
        .unwrap();

        assert!(matches!(result, Err(Error::NotARegularFile(_))));
    }

    /// Boundary regression for #427/#418's shared size gate: a file of
    /// exactly `max_file_size` bytes must succeed through
    /// `read_to_string_checked` (the disk-read path `ensure_open` uses),
    /// and one byte more must fail as `FileSizeLimitExceeded` -- not just
    /// "some file well over the limit is rejected".
    #[tokio::test]
    async fn test_read_to_string_checked_size_boundary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("boundary.rs");
        let tracker = DocumentTracker::new(
            ResourceLimits {
                max_documents: 100,
                max_file_size: 10,
            },
            HashMap::new(),
        );

        std::fs::write(&path, "a".repeat(10)).unwrap();
        let (content, ..) = tracker.read_to_string_checked(&path).await.unwrap();
        assert_eq!(content.len(), 10);

        std::fs::write(&path, "a".repeat(11)).unwrap();
        let result = tracker.read_to_string_checked(&path).await;
        assert!(matches!(
            result,
            Err(Error::FileSizeLimitExceeded { size: 11, max: 10 })
        ));
    }

    /// Regression for #474: `read_line_checked` must stop reading (and
    /// UTF-8-decoding) once it has the requested line, not buffer/validate
    /// the rest of the file. The file's second line is invalid UTF-8, which
    /// would fail a whole-file read (as the pre-#474 `read_checked` +
    /// `.lines().nth(...)` path did); reading line 0 must still succeed.
    #[tokio::test]
    async fn test_read_line_checked_does_not_read_past_target_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("partial.rs");
        let mut content = b"hello\n".to_vec();
        content.extend_from_slice(&[0xFF, 0xFE]);
        content.push(b'\n');
        std::fs::write(&path, &content).unwrap();

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let line = tracker.read_line_checked(&path, 0, u64::MAX).await.unwrap();
        assert_eq!(line.text.as_deref(), Some("hello"));
    }

    /// Regression for M3: an off-by-one in `current_line` (e.g. returning
    /// line `N + 1` for `N`) would ship green if every test used line 0.
    /// Exercises a non-zero target line on a multi-line fixture.
    #[tokio::test]
    async fn test_read_line_checked_returns_requested_non_zero_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("multi.rs");
        std::fs::write(&path, "first\nsecond\nthird\nfourth\n").unwrap();

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        assert_eq!(
            tracker
                .read_line_checked(&path, 2, u64::MAX)
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("third")
        );
    }

    /// `read_line_checked` must report `Ok(None)`, not an error, when `line`
    /// is past the file's last line -- distinguishing "file has fewer lines
    /// than requested" from an actual read failure.
    #[tokio::test]
    async fn test_read_line_checked_returns_none_past_last_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("short.rs");
        std::fs::write(&path, "only one line").unwrap();

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        assert_eq!(
            tracker
                .read_line_checked(&path, 5, u64::MAX)
                .await
                .unwrap()
                .text,
            None
        );
    }

    /// A requested line with no trailing `\n` at all (the file's only line,
    /// never terminated) must still be returned -- distinct from
    /// `test_read_line_checked_returns_none_past_last_line`, which requests a
    /// line number past this same kind of file instead of the line itself.
    #[tokio::test]
    async fn test_read_line_checked_reads_last_line_without_trailing_newline() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("no_newline.rs");
        std::fs::write(&path, "only one line").unwrap();

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        assert_eq!(
            tracker
                .read_line_checked(&path, 0, u64::MAX)
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("only one line")
        );
    }

    #[tokio::test]
    async fn test_read_line_checked_empty_file_line_zero_is_empty_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.rs");
        std::fs::write(&path, "").unwrap();

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        assert_eq!(
            tracker
                .read_line_checked(&path, 0, u64::MAX)
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("")
        );
        assert_eq!(
            tracker
                .read_line_checked(&path, 1, u64::MAX)
                .await
                .unwrap()
                .text,
            None
        );
    }

    #[tokio::test]
    async fn test_read_line_checked_trailing_empty_line_after_final_newline() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("trailing.rs");
        std::fs::write(&path, "a\n").unwrap();

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let read = |line| tracker.read_line_checked(&path, line, u64::MAX);
        assert_eq!(read(1).await.unwrap().text.as_deref(), Some(""));
        assert_eq!(read(2).await.unwrap().text, None);
    }

    #[tokio::test]
    async fn test_read_line_checked_cap_at_line_boundary_is_not_empty_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("capped.rs");
        std::fs::write(&path, "abc\nrest\n").unwrap();

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        // budget 3 -> cap 4: the read stops right after "abc\n", mid-file.
        assert_eq!(
            tracker.read_line_checked(&path, 1, 3).await.unwrap().text,
            None
        );
    }

    /// Pins that CRLF-terminated lines come out identical to LF-only ones.
    #[tokio::test]
    async fn test_read_line_checked_strips_crlf_line_ending() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("crlf.rs");
        std::fs::write(&path, "first\r\nsecond\r\n").unwrap();

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        assert_eq!(
            tracker
                .read_line_checked(&path, 0, u64::MAX)
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("first")
        );
        assert_eq!(
            tracker
                .read_line_checked(&path, 1, u64::MAX)
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("second")
        );
    }

    /// The tracker and disk readers must agree on every line, including the
    /// CR-stripping and trailing-empty-line edge cases -- `to_lsp` falls back
    /// to disk whenever the tracker misses, so a divergence would
    /// reintroduce a false degradation signal.
    #[tokio::test]
    async fn test_line_text_and_read_line_checked_agree_on_line_rule() {
        let dir = TempDir::new().unwrap();
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());

        let contents = [
            "",
            "abc",
            "abc\n",
            "abc\r\n",
            "abc\r\r\n",
            "abc\r",
            "a\n\nb",
            "a\r\nb\r\n",
            "a\rb",
            "a\r\r\nb",
            "\n\r",
            "\r\n\r\n",
        ];
        for (i, content) in contents.iter().enumerate() {
            let path = dir.path().join(format!("parity_{i}.rs"));
            std::fs::write(&path, content).unwrap();
            tracker.open(path.clone(), (*content).to_string()).unwrap();
            for line in 0..6 {
                let disk = tracker
                    .read_line_checked(&path, line, u64::MAX)
                    .await
                    .unwrap()
                    .text;
                assert_eq!(
                    tracker.line_text(&path, line),
                    disk,
                    "content {content:?}, line {line}"
                );
            }
        }
    }

    #[test]
    fn test_line_text_trailing_empty_line_and_lone_cr_terminator() {
        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let path = PathBuf::from("/test/lines.rs");
        tracker
            .open(path.clone(), "ab\r\ncd\r\r\n".to_string())
            .unwrap();

        assert_eq!(tracker.line_text(&path, 0).as_deref(), Some("ab"));
        assert_eq!(tracker.line_text(&path, 1).as_deref(), Some("cd"));
        assert_eq!(tracker.line_text(&path, 2).as_deref(), Some(""));
        assert_eq!(tracker.line_text(&path, 3).as_deref(), Some(""));
        assert_eq!(tracker.line_text(&path, 4), None);

        let empty = PathBuf::from("/test/empty.rs");
        tracker.open(empty.clone(), String::new()).unwrap();
        assert_eq!(tracker.line_text(&empty, 0).as_deref(), Some(""));
        assert_eq!(tracker.line_text(&empty, 1), None);
    }

    /// Regression for the `bounded_read_cap` off-by-one: a file whose size
    /// is exactly `max_file_size` must not be misreported as oversized when
    /// a request (for a line past the file's content) forces a full read to
    /// EOF. The cap is `max_file_size + 1` precisely so this exact-boundary
    /// case is distinguishable from a genuinely oversized file.
    #[tokio::test]
    async fn test_read_line_checked_exact_max_file_size_reads_to_eof_without_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exact.rs");
        let content = "a".repeat(20);
        std::fs::write(&path, &content).unwrap();

        let limits = ResourceLimits {
            max_documents: 100,
            max_file_size: 20,
        };
        let tracker = DocumentTracker::new(limits, HashMap::new());

        assert_eq!(
            tracker
                .read_line_checked(&path, 0, u64::MAX)
                .await
                .unwrap()
                .text
                .as_deref(),
            Some(content.as_str())
        );
        assert_eq!(
            tracker
                .read_line_checked(&path, 1, u64::MAX)
                .await
                .unwrap()
                .text,
            None,
            "a line past an exact-max_file_size file's only line must read to EOF cleanly, not \
             be misreported as truncated"
        );
    }

    /// Regression for the S1 budget-bypass fix: `budget` must physically
    /// bound the read (via the take-adapter), not just gate whether a read
    /// is attempted -- a read that starts with budget left must still stop
    /// at exactly that many bytes, never at the full `max_file_size`.
    /// Distinguishes this from `bounded_read_cap(max_file_size)` alone by
    /// using a `budget` far smaller than `max_file_size`.
    #[tokio::test]
    async fn test_read_line_checked_bounds_read_by_budget_not_just_max_file_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("budget.rs");
        std::fs::write(&path, "a".repeat(1000)).unwrap();

        let limits = ResourceLimits {
            max_documents: 100,
            max_file_size: 1000,
        };
        let tracker = DocumentTracker::new(limits, HashMap::new());

        let read = tracker.read_line_checked(&path, 0, 10).await.unwrap();
        assert_eq!(
            read.text, None,
            "a single line far longer than the budget must not be returned as if complete"
        );
        assert_eq!(
            read.bytes_read, 11,
            "the read must stop at exactly the budget's +1 slack (see the correctness-gate fix \
             below), not at max_file_size"
        );
    }

    /// Regression for a correctness-gate finding: `cap`'s `budget` component
    /// needs the same `+1` disambiguation slack `bounded_read_cap` already
    /// applies to `max_file_size` -- without it, a read whose remaining
    /// budget exactly equals its target line's byte length (no trailing
    /// newline) is indistinguishable from one genuinely truncated by the
    /// cap, and was misreported as truncated (`text: None`) even though the
    /// read fully succeeded.
    #[tokio::test]
    async fn test_read_line_checked_exact_budget_match_on_unterminated_line_not_truncated() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exact_budget.rs");
        let content = "twelve chars";
        std::fs::write(&path, content).unwrap();

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let read = tracker
            .read_line_checked(&path, 0, content.len() as u64)
            .await
            .unwrap();
        assert_eq!(
            read.text.as_deref(),
            Some(content),
            "budget exactly matching the line's byte length must not be misreported as truncated"
        );
        assert_eq!(read.bytes_read, content.len() as u64);
    }

    /// Regression for the S1 budget-bypass fix: an invalid-UTF-8 line (the
    /// realistic attack shape -- a `.rlib`/image/pack file under
    /// `max_file_size`) must still report an accurate `bytes_read` on
    /// `LineRead::text == None`, not lose it down an `Err` path with no byte
    /// count -- that loss is exactly what let a hostile response scan
    /// unlimited bytes while charging the per-response budget zero.
    #[tokio::test]
    async fn test_read_line_checked_reports_bytes_read_for_invalid_utf8_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("invalid_utf8.rs");
        let mut content = vec![0xFFu8, 0xFE, 0xFD];
        content.push(b'\n');
        std::fs::write(&path, &content).unwrap();

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        let read = tracker.read_line_checked(&path, 0, u64::MAX).await.unwrap();
        assert_eq!(read.text, None);
        assert_eq!(
            read.bytes_read,
            content.len() as u64,
            "bytes scanned must be reported even though the line wasn't valid UTF-8"
        );
    }

    /// Regression for the open-failure-charge fix: a path that doesn't
    /// exist (the realistic, non-attacker case -- e.g. an LSP server naming
    /// a stdlib location not present locally) must resolve to `Ok(None)`,
    /// not `Err`, and must charge the small nominal
    /// `OPEN_FAILURE_CHARGE_BYTES` amount rather than `0` (which would let
    /// a response repeat this for free) or the full budget (the previous
    /// round's regression, which zeroed the whole per-response budget on
    /// the very first such location).
    #[tokio::test]
    async fn test_read_line_checked_charges_nominal_amount_for_nonexistent_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does_not_exist.rs");

        let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
        // A nonexistent path must resolve to Ok(None), not Err.
        let read = tracker.read_line_checked(&path, 0, u64::MAX).await.unwrap();
        assert_eq!(read.text, None);
        assert_eq!(read.bytes_read, OPEN_FAILURE_CHARGE_BYTES);
    }

    /// Independent line splitter for the oracle tests: a single left-to-right
    /// pass over the bytes, unlike the checkpointed lookup under test.
    fn naive_lines(content: &str) -> Vec<&str> {
        let bytes = content.as_bytes();
        let mut lines = Vec::new();
        let (mut start, mut i) = (0, 0);
        while i < bytes.len() {
            match bytes[i] {
                b'\n' => {
                    lines.push(&content[start..i]);
                    i += 1;
                    start = i;
                }
                b'\r' => {
                    lines.push(&content[start..i]);
                    i += 1;
                    if bytes.get(i) == Some(&b'\n') {
                        i += 1;
                    }
                    start = i;
                }
                _ => i += 1,
            }
        }
        lines.push(&content[start..]);
        lines
    }

    /// Every string up to `max_len` characters over `{a, e-acute, CR, LF}`.
    fn all_strings(max_len: usize) -> Vec<String> {
        let alphabet = ['a', 'é', '\r', '\n'];
        let mut all = vec![String::new()];
        let mut frontier = vec![String::new()];
        for _ in 0..max_len {
            frontier = frontier
                .iter()
                .flat_map(|s| {
                    alphabet.iter().map(move |c| {
                        let mut next = s.clone();
                        next.push(*c);
                        next
                    })
                })
                .collect();
            all.extend(frontier.iter().cloned());
        }
        all
    }

    /// #513/#488: `DocumentText::line` agrees with an independent splitter on
    /// every short string, at strides small enough that checkpoints land on
    /// every CR/LF pair and on the EOF boundary.
    #[test]
    fn test_document_text_line_matches_naive_splitter_exhaustively() {
        for content in all_strings(8) {
            let expected = naive_lines(&content);
            for stride in 1..=3 {
                let text = DocumentText::with_stride(content.clone(), stride);
                for (n, want) in expected.iter().enumerate() {
                    assert_eq!(
                        text.line(u32::try_from(n).unwrap()),
                        Some(*want),
                        "{content:?} stride {stride} line {n}"
                    );
                }
                assert_eq!(
                    text.line(u32::try_from(expected.len()).unwrap()),
                    None,
                    "{content:?} stride {stride} past the last line"
                );
            }
        }
    }

    /// #513: the disk reader agrees with the same oracle, including with a
    /// one-byte buffer that splits every CRLF across two fills.
    #[tokio::test]
    async fn test_read_nth_line_matches_naive_splitter_exhaustively() {
        for content in all_strings(7) {
            let expected = naive_lines(&content);
            for capacity in [1, 2, 8 * 1024] {
                for n in 0..=expected.len() {
                    let mut reader =
                        tokio::io::BufReader::with_capacity(capacity, content.as_bytes());
                    let read = read_nth_line(&mut reader, u32::try_from(n).unwrap(), u64::MAX)
                        .await
                        .unwrap();
                    assert_eq!(
                        read.text.as_deref(),
                        expected.get(n).copied(),
                        "{content:?} capacity {capacity} line {n}"
                    );
                }
            }
        }
    }

    /// #488 M3(a): exactly one stride of lines plus a final terminator leaves
    /// an empty line starting at EOF, which must still be reachable.
    #[test]
    fn test_document_text_checkpoint_at_eof_after_full_stride() {
        for terminator in ["\n", "\r\n", "\r"] {
            let content = format!("x{terminator}").repeat(LINE_CHECKPOINT_STRIDE);
            let text = DocumentText::new(content);
            let stride = u32::try_from(LINE_CHECKPOINT_STRIDE).unwrap();
            assert_eq!(text.line(stride - 1), Some("x"));
            assert_eq!(text.line(stride), Some(""), "terminator {terminator:?}");
            assert_eq!(text.line(stride + 1), None);
        }
    }

    /// #488: a document spanning several checkpoint strides resolves lines on
    /// both sides of each checkpoint, with CRLF straddling none of them wrong.
    #[test]
    fn test_document_text_lines_across_default_stride_boundaries() {
        let content: String = (0..200)
            .map(|i| format!("line{i}\r\n"))
            .collect::<Vec<_>>()
            .concat();
        let text = DocumentText::new(content);
        for i in [0u32, 1, 63, 64, 65, 127, 128, 199] {
            assert_eq!(text.line(i), Some(format!("line{i}").as_str()), "line {i}");
        }
        assert_eq!(text.line(200), Some(""));
        assert_eq!(text.line(201), None);
    }

    /// Content above the inline threshold is indexed on the blocking pool and
    /// yields the same lines.
    #[tokio::test]
    async fn test_document_text_build_above_inline_threshold_matches_inline() {
        let content = "ab\r\n".repeat(INLINE_INDEX_MAX_BYTES / 4 + 10);
        assert!(content.len() > INLINE_INDEX_MAX_BYTES);
        let built = DocumentText::build(content.clone()).await.unwrap();
        let inline = DocumentText::new(content);
        assert_eq!(built.checkpoints, inline.checkpoints);
        assert_eq!(built.line(1000), Some("ab"));
    }

    #[test]
    fn test_document_text_checkpoint_memory_is_sparse() {
        let text = DocumentText::new("\n".repeat(6400));
        assert_eq!(text.checkpoints.len(), 6400 / LINE_CHECKPOINT_STRIDE + 1);
    }

    /// #513: a chunk boundary between `\r` and `\n` must not count the pair as
    /// two terminators.
    #[tokio::test]
    async fn test_read_nth_line_crlf_split_across_fills_is_one_terminator() {
        let mut reader = tokio::io::BufReader::with_capacity(1, b"a\r\nb".as_slice());
        let read = read_nth_line(&mut reader, 1, u64::MAX).await.unwrap();
        assert_eq!(read.text.as_deref(), Some("b"));
    }

    /// #513: the cap running out mid-line is reported as `None`, not a
    /// truncated line.
    #[tokio::test]
    async fn test_read_nth_line_cap_exhausted_mid_line_is_none() {
        let mut reader =
            tokio::io::BufReader::new(tokio::io::AsyncReadExt::take(b"abcdef\n".as_slice(), 3));
        let read = read_nth_line(&mut reader, 0, 3).await.unwrap();
        assert_eq!(read.text, None);
        assert_eq!(read.bytes_read, 3);
    }

    /// Opens `name` for `servers` in turn through `ensure_open`, returning the
    /// path and one `(client, fake server)` pair per server.
    async fn open_for_servers(
        tracker: &DocumentTracker,
        dir: &TempDir,
        name: &str,
        servers: &[&str],
    ) -> (
        PathBuf,
        Vec<(ServerId, LspClient, crate::test_lsp::FakeServer)>,
    ) {
        let path = dir.path().join(name);
        std::fs::write(&path, name).unwrap();
        set_mtime(&path, settled_past());
        let mut out = Vec::new();
        for server in servers {
            let (client, fake) = fake_lsp_client();
            let id = ServerId::from(*server);
            tracker.ensure_open(&path, &id, &client).await.unwrap();
            out.push((id, client, fake));
        }
        (path, out)
    }

    /// Evicts the tracker's only document by opening `other`.
    async fn evict_by_opening(tracker: &DocumentTracker, dir: &TempDir, other: &str) {
        let path = dir.path().join(other);
        std::fs::write(&path, other).unwrap();
        set_mtime(&path, settled_past());
        let (client, _fake) = fake_lsp_client();
        tracker
            .ensure_open(&path, &ServerId::from("other"), &client)
            .await
            .unwrap();
    }

    fn one_document_tracker() -> DocumentTracker {
        DocumentTracker::new(
            ResourceLimits {
                max_documents: 1,
                max_file_size: 0,
            },
            HashMap::new(),
        )
    }

    /// #515: a path evicted while synced to A and D, then re-opened through A
    /// alone, sends A a close before its open, and still owes D its close.
    #[tokio::test]
    async fn test_reopen_through_one_server_settles_only_that_servers_close() {
        let dir = TempDir::new().unwrap();
        let tracker = one_document_tracker();
        let (path, mut servers) = open_for_servers(&tracker, &dir, "p.rs", &["a", "d"]).await;
        evict_by_opening(&tracker, &dir, "q.rs").await;
        assert_eq!(
            pending_servers(&tracker, &path),
            HashSet::from([ServerId::from("a"), ServerId::from("d")])
        );

        let (id_a, client_a, mut fake_a) = servers.remove(0);
        tracker.ensure_open(&path, &id_a, &client_a).await.unwrap();

        let mut wire = BufReader::new(&mut fake_a.write_stdout);
        assert_eq!(
            read_framed_message(&mut wire).await["method"],
            "textDocument/didOpen"
        );
        assert_eq!(
            read_framed_message(&mut wire).await["method"],
            "textDocument/didClose"
        );
        assert_eq!(
            read_framed_message(&mut wire).await["method"],
            "textDocument/didOpen"
        );

        assert_eq!(
            pending_servers(&tracker, &path),
            HashSet::from([ServerId::from("d")])
        );
        let claim = tracker.try_claim_pending_close(&path).unwrap();
        assert_eq!(claim.servers, vec![ServerId::from("d")]);
    }

    /// #515: evict then flush hands out every owed close exactly once.
    #[tokio::test]
    async fn test_claim_returns_every_owed_server_once() {
        let dir = TempDir::new().unwrap();
        let tracker = one_document_tracker();
        let (path, _servers) = open_for_servers(&tracker, &dir, "p.rs", &["a", "d"]).await;
        evict_by_opening(&tracker, &dir, "q.rs").await;

        assert_eq!(tracker.pending_close_paths(), vec![path.clone()]);
        let claim = tracker.try_claim_pending_close(&path).unwrap();
        assert_eq!(claim.path, path);
        let mut claimed = claim.servers.clone();
        claimed.sort_by_key(ToString::to_string);
        assert_eq!(claimed, vec![ServerId::from("a"), ServerId::from("d")]);
        drop(claim);

        assert!(tracker.pending_close_paths().is_empty());
        assert!(tracker.try_claim_pending_close(&path).is_none());
    }

    /// #515 S2: a busy path is deferred, not awaited, and its debt survives.
    #[tokio::test]
    async fn test_claim_defers_busy_path_without_waiting() {
        let dir = TempDir::new().unwrap();
        let tracker = one_document_tracker();
        let (path, _servers) = open_for_servers(&tracker, &dir, "p.rs", &["a"]).await;
        evict_by_opening(&tracker, &dir, "q.rs").await;

        let busy = tracker.lock_path(&path).await;
        assert!(tracker.try_claim_pending_close(&path).is_none());
        assert_eq!(
            pending_servers(&tracker, &path),
            HashSet::from([ServerId::from("a")]),
            "a deferred claim must leave the debt pending"
        );
        drop(busy);
        assert!(tracker.try_claim_pending_close(&path).is_some());
        assert!(
            lock_std(&tracker.path_locks).is_empty(),
            "failed and successful claims must not leak path-lock entries"
        );
    }

    /// #515: while a claim is alive, `ensure_open` for the path waits for it,
    /// so a later `didOpen` is ordered after the claimed closes.
    #[tokio::test]
    async fn test_claim_holds_path_lock_until_dropped() {
        let dir = TempDir::new().unwrap();
        let tracker = Arc::new(one_document_tracker());
        let (path, _servers) = open_for_servers(&tracker, &dir, "p.rs", &["a"]).await;
        evict_by_opening(&tracker, &dir, "q.rs").await;

        let claim = tracker.try_claim_pending_close(&path).unwrap();
        let (client, _fake) = fake_lsp_client();
        let reopen = {
            let tracker = Arc::clone(&tracker);
            let path = path.clone();
            tokio::spawn(async move {
                tracker
                    .ensure_open(&path, &ServerId::from("b"), &client)
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!reopen.is_finished(), "ensure_open must wait for the claim");
        drop(claim);
        reopen.await.unwrap().unwrap();
    }

    /// #515: evict, re-open and evict again merges into one entry per server.
    #[tokio::test]
    async fn test_repeated_eviction_merges_pending_closes() {
        let dir = TempDir::new().unwrap();
        let tracker = one_document_tracker();
        let (path, mut servers) = open_for_servers(&tracker, &dir, "p.rs", &["a", "d"]).await;
        evict_by_opening(&tracker, &dir, "q.rs").await;

        let (id_d, client_d, _fake_d) = servers.remove(1);
        tracker.ensure_open(&path, &id_d, &client_d).await.unwrap();
        evict_by_opening(&tracker, &dir, "r.rs").await;

        assert_eq!(
            pending_servers(&tracker, &path),
            HashSet::from([ServerId::from("a"), ServerId::from("d")])
        );
        assert_eq!(tracker.pending_close_paths().len(), 2);
    }

    /// #515: a close that fails after `forget_server` ran (respawn) must not
    /// re-add the forgotten server's debt.
    #[tokio::test]
    async fn test_restore_pending_close_skips_forgotten_server() {
        let dir = TempDir::new().unwrap();
        let tracker = one_document_tracker();
        let (path, _servers) = open_for_servers(&tracker, &dir, "p.rs", &["a"]).await;
        evict_by_opening(&tracker, &dir, "q.rs").await;
        let server = ServerId::from("a");
        let uri = path_to_uri(&path).unwrap();

        let generation = tracker.generation(&server);
        assert!(tracker.take_pending_close(&path, &server));
        tracker.forget_server(&server);
        tracker.restore_pending_close(&path, &uri, &server, generation);
        assert!(pending_servers(&tracker, &path).is_empty());

        tracker.restore_pending_close(&path, &uri, &server, tracker.generation(&server));
        assert_eq!(pending_servers(&tracker, &path), HashSet::from([server]));
    }

    /// #515: a respawned server never had the documents open, so it owes no
    /// close; other servers' debts stay.
    #[tokio::test]
    async fn test_forget_server_purges_its_pending_closes() {
        let dir = TempDir::new().unwrap();
        let tracker = one_document_tracker();
        let (path, _servers) = open_for_servers(&tracker, &dir, "p.rs", &["a", "d"]).await;
        evict_by_opening(&tracker, &dir, "q.rs").await;

        tracker.forget_server(&ServerId::from("a"));
        assert_eq!(
            pending_servers(&tracker, &path),
            HashSet::from([ServerId::from("d")])
        );
        tracker.forget_server(&ServerId::from("d"));
        assert!(!lock_std(&tracker.pending_closes).contains_key(&path));
    }

    /// #515: a failed `didClose` keeps the server's debt and surfaces the error.
    #[tokio::test]
    async fn test_failed_close_before_reopen_restores_the_debt() {
        let dir = TempDir::new().unwrap();
        let tracker = one_document_tracker();
        let (path, mut servers) = open_for_servers(&tracker, &dir, "p.rs", &["a"]).await;
        evict_by_opening(&tracker, &dir, "q.rs").await;

        let (id_a, client_a, _fake_a) = servers.remove(0);
        let will_fail = client_a.clone();
        client_a.shutdown().await.unwrap();
        assert!(tracker.ensure_open(&path, &id_a, &will_fail).await.is_err());
        assert_eq!(
            pending_servers(&tracker, &path),
            HashSet::from([ServerId::from("a")])
        );
    }
}
