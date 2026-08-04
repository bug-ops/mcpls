//! Document state management.
//!
//! Tracks open documents and their versions for LSP synchronization.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};

use lsp_types::{
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, TextDocumentContentChangeEvent,
    TextDocumentItem, Uri, VersionedTextDocumentIdentifier,
};
use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio::time::Instant;
use url::Url;

use super::lock_std;
use crate::config::ServerId;
use crate::error::{Error, Result};
use crate::lsp::LspClient;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentState {
    /// Document URI.
    pub uri: Uri,
    /// Language identifier.
    pub language_id: String,
    /// Document version (monotonically increasing).
    pub version: i32,
    /// Document content.
    pub content: String,
    /// Filesystem snapshot as of the last time `content` was read from disk.
    ///
    /// `None` means the content's on-disk provenance is unknown (it came from
    /// an in-memory `open`/`update` call, not a verified disk read), so
    /// `ensure_open` must always re-verify by content compare rather than
    /// trusting a stat match.
    ///
    /// `DiskSync`'s hand-written `PartialEq` excludes `content_checked_at`
    /// (see that field's doc comment), and that exclusion propagates here:
    /// two `DocumentState`s can compare equal via this struct's derived
    /// `PartialEq`/`Eq` despite having been disk-verified at different
    /// instants. This is intentional -- `content_checked_at` is a debounce
    /// timer, not part of a document's logical state.
    pub disk: Option<DiskSync>,
    /// Last document version pushed to each server via `didOpen`/`didChange`.
    ///
    /// A single document can be synced to multiple servers (e.g. hover
    /// routed to one server, diagnostics to another for the same language),
    /// each needing its own `didOpen`/`didChange` history -- a server absent
    /// from this map has never seen the document and must receive
    /// `didOpen`, not `didChange`, on its next `ensure_open` call.
    pub synced: HashMap<ServerId, i32>,
}

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
            max_documents: 100,
            max_file_size: 10 * 1024 * 1024, // 10MB
        }
    }
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
    path_locks: StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>,
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
}

impl DocumentTracker {
    /// Create a new document tracker with custom limits and extension mappings.
    #[must_use]
    pub fn new(limits: ResourceLimits, extension_map: HashMap<String, String>) -> Self {
        Self {
            documents: StdMutex::new(HashMap::new()),
            path_locks: StdMutex::new(HashMap::new()),
            generations: StdMutex::new(HashMap::new()),
            limits,
            extension_map,
        }
    }

    /// Check if a document is currently open.
    #[must_use]
    pub fn is_open(&self, path: &Path) -> bool {
        lock_std(&self.documents).contains_key(path)
    }

    /// Get a clone of the state of an open document.
    #[must_use]
    pub fn get(&self, path: &Path) -> Option<DocumentState> {
        lock_std(&self.documents).get(path).cloned()
    }

    /// Get the number of open documents.
    #[must_use]
    pub fn len(&self) -> usize {
        lock_std(&self.documents).len()
    }

    /// Check if there are no open documents.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        lock_std(&self.documents).is_empty()
    }

    /// Open a document and track its state.
    ///
    /// Returns the document URI for use in LSP requests.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Document limit is exceeded
    /// - File size limit is exceeded
    pub fn open(&self, path: PathBuf, content: String) -> Result<Uri> {
        self.check_file_size(content.len() as u64)?;

        let uri = path_to_uri(&path);
        let language_id = detect_language(&path, &self.extension_map);

        let state = DocumentState {
            uri: uri.clone(),
            language_id,
            version: 1,
            content,
            disk: None,
            synced: HashMap::new(),
        };

        // Check document limit and insert under a single lock acquisition so
        // two concurrent `open` calls for different new paths can't both
        // pass the check and jointly exceed the limit by one. Dropped
        // explicitly right after the insert rather than at function return.
        let mut documents = lock_std(&self.documents);
        if self.limits.max_documents > 0 && documents.len() >= self.limits.max_documents {
            return Err(Error::DocumentLimitExceeded {
                current: documents.len(),
                max: self.limits.max_documents,
            });
        }
        documents.insert(path, state);
        drop(documents);
        Ok(uri)
    }

    /// Update a document's content and increment its version.
    ///
    /// Returns `None` if the document is not open. The updated content has no
    /// known disk provenance, so the next `ensure_open` call on this path
    /// will always re-verify by content compare rather than trusting a stat.
    pub fn update(&self, path: &Path, content: String) -> Option<i32> {
        let mut documents = lock_std(&self.documents);
        if let Some(state) = documents.get_mut(path) {
            state.version += 1;
            state.content = content;
            state.disk = None;
            Some(state.version)
        } else {
            None
        }
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
            st.disk = Some(snap);
        }
    }

    /// Close a document and remove it from tracking.
    ///
    /// Returns the document state if it was open.
    pub fn close(&self, path: &Path) -> Option<DocumentState> {
        lock_std(&self.documents).remove(path)
    }

    /// Close all documents.
    pub fn close_all(&self) -> Vec<DocumentState> {
        lock_std(&self.documents)
            .drain()
            .map(|(_, state)| state)
            .collect()
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
        *lock_std(&self.generations)
            .entry(server.clone())
            .or_insert(0) += 1;
        for state in lock_std(&self.documents).values_mut() {
            state.synced.remove(server);
        }
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
    /// **Sync phase**: compares `server`'s last-synced version (tracked in
    /// [`DocumentState::synced`]) against the version decided by the disk
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
    /// `st.version`/`st.content`/`st.disk`/`synced[server]` are all committed
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
        // held while `fast_path` is computed.
        let Some((uri, current_version, fast_path)) =
            lock_std(&self.documents).get(path).map(|st| {
                let stat_matches = st.disk.is_some_and(|d| d.mtime == mtime && d.size == size);
                let fast_path = match st.disk {
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
            .map(|st| fresh == st.content)
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
            fresh_content: Some(fresh),
            snap: Some(snap),
        })
    }

    /// Reads a not-yet-tracked file from disk and opens it in the tracker at
    /// version 1. No server has synced it yet, so the sync phase always
    /// sends `didOpen` regardless of which server calls next.
    async fn disk_phase_new(&self, path: &Path) -> Result<Decision> {
        let read_at = SystemTime::now();
        let (content, mtime, size) = self.read_to_string_checked(path).await?;

        let uri = self.open(path.to_path_buf(), content)?;
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

    /// Reads `path` through a single open file handle, checking its size
    /// against [`Self::check_file_size`] using that same handle's metadata
    /// rather than a separately-stat'd size. Reading and size-checking
    /// through one handle closes the TOCTOU window where an atomic replace
    /// (e.g. a concurrent `rename`) between an earlier `metadata()` call and
    /// a path-based read could let an oversized file bypass the pre-read
    /// size gate.
    ///
    /// Returns the content along with the handle's own mtime and size, so
    /// callers can build a [`DiskSync`] snapshot consistent with what was
    /// actually read.
    async fn read_to_string_checked(
        &self,
        path: &Path,
    ) -> Result<(String, Option<SystemTime>, u64)> {
        let mut file = fs::File::open(path).await.map_err(|e| Error::FileIo {
            path: path.to_path_buf(),
            source: e,
        })?;
        let meta = file.metadata().await.map_err(|e| Error::FileIo {
            path: path.to_path_buf(),
            source: e,
        })?;
        self.check_file_size(meta.len())?;
        let mut content = String::new();
        file.read_to_string(&mut content)
            .await
            .map_err(|e| Error::FileIo {
                path: path.to_path_buf(),
                source: e,
            })?;
        Ok((content, meta.modified().ok(), meta.len()))
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
            .map(|st| st.synced.get(server).copied())
        else {
            return Err(Error::DocumentNotFound(path.to_path_buf()));
        };
        let up_to_date = synced_version.is_some_and(|v| v >= target_version);
        let is_first_open = synced_version.is_none();

        if up_to_date {
            return Ok(uri);
        }

        let Some((language_id, text)) = lock_std(&self.documents).get(path).map(|st| {
            let text = fresh_content.clone().unwrap_or_else(|| st.content.clone());
            (st.language_id.clone(), text)
        }) else {
            return Err(Error::DocumentNotFound(path.to_path_buf()));
        };

        let notify_result = if is_first_open {
            lsp_client
                .notify(
                    "textDocument/didOpen",
                    DidOpenTextDocumentParams {
                        text_document: TextDocumentItem {
                            uri: uri.clone(),
                            language_id,
                            version: target_version,
                            text,
                        },
                    },
                )
                .await
        } else {
            lsp_client
                .notify(
                    "textDocument/didChange",
                    DidChangeTextDocumentParams {
                        text_document: VersionedTextDocumentIdentifier {
                            uri: uri.clone(),
                            version: target_version,
                        },
                        content_changes: vec![TextDocumentContentChangeEvent {
                            range: None,
                            range_length: None,
                            text,
                        }],
                    },
                )
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
                .is_some_and(|st| st.synced.is_empty());
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
            st.version = target_version;
            st.content = fresh;
            st.disk = snap;
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
            st.synced.insert(server.clone(), target_version);
        }
        drop(documents);

        Ok(uri)
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
    fresh_content: Option<String>,
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
/// # Panics
///
/// Panics if the path cannot be represented as a `file://` URI. This should
/// not occur for valid absolute paths.
#[must_use]
pub fn path_to_uri(path: &Path) -> Uri {
    #[allow(clippy::expect_used)]
    try_path_to_uri(path).expect("failed to create URI from path")
}

/// Convert a file path to a URI, returning `None` if the path cannot be
/// represented as a `file://` URI.
///
/// Prefer this over [`path_to_uri`] on paths that come from configuration,
/// where a bad value should surface as an error rather than a panic.
#[must_use]
pub fn try_path_to_uri(path: &Path) -> Option<Uri> {
    let uri_string = encode_rfc3986_path_chars(&file_url(path)?);
    uri_string.parse().ok()
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

fn encode_rfc3986_path_chars(url: &Url) -> String {
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
    let url = Url::parse(uri.as_str()).ok()?;
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

    #[test]
    fn test_document_tracker() {
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
        assert_eq!(state.version, 1);
        assert_eq!(state.language_id, "rust");

        let new_version = tracker.update(&path, "fn main() { println!() }".to_string());
        assert_eq!(new_version, Some(2));

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
        assert!(!state.synced.contains_key(&respawned));
        assert!(state.synced.contains_key(&untouched));
    }

    /// #249 S1 regression: a `sync_phase` call that captured `server`'s
    /// generation *before* a concurrent `forget_server` bumped it must not
    /// commit its `synced` write, even though its notification against the
    /// now-superseded connection reports success (`fake_lsp_client`'s `cat`
    /// backend always accepts writes, standing in for the window where a
    /// server's process has already died but its message loop has not yet
    /// observed that). Without this, a document synced against the old
    /// (crashed) process would be wrongly marked as already open on the
    /// respawned one, permanently desyncing it.
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
            !state.synced.contains_key(&server),
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
        assert_eq!(state.synced.get(&server), Some(&1));
    }

    #[test]
    fn test_document_limit() {
        let limits = ResourceLimits {
            max_documents: 2,
            max_file_size: 100,
        };
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(limits, map);

        // First two documents should succeed
        tracker
            .open(PathBuf::from("/test/file1.rs"), "fn test1() {}".to_string())
            .unwrap();
        tracker
            .open(PathBuf::from("/test/file2.rs"), "fn test2() {}".to_string())
            .unwrap();

        // Third should fail
        let result = tracker.open(PathBuf::from("/test/file3.rs"), "fn test3() {}".to_string());
        assert!(matches!(result, Err(Error::DocumentLimitExceeded { .. })));
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
            uri: "file:///test.rs".parse().unwrap(),
            language_id: "rust".to_string(),
            version: 5,
            content: "fn main() {}".to_string(),
            disk: None,
            synced: HashMap::new(),
        };

        #[allow(clippy::redundant_clone)]
        let cloned = state.clone();
        assert_eq!(cloned.uri, state.uri);
        assert_eq!(cloned.language_id, state.language_id);
        assert_eq!(cloned.version, 5);
        assert_eq!(cloned.content, state.content);
    }

    #[test]
    fn test_update_nonexistent_document() {
        let map = HashMap::new();
        let tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/nonexistent.rs");

        let version = tracker.update(&path, "new content".to_string());
        assert_eq!(
            version, None,
            "Updating non-existent document should return None"
        );
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
    fn test_close_all_documents() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(ResourceLimits::default(), map);

        tracker
            .open(PathBuf::from("/test/file1.rs"), "content1".to_string())
            .unwrap();
        tracker
            .open(PathBuf::from("/test/file2.rs"), "content2".to_string())
            .unwrap();
        tracker
            .open(PathBuf::from("/test/file3.rs"), "content3".to_string())
            .unwrap();

        assert_eq!(tracker.len(), 3);

        let closed = tracker.close_all();
        assert_eq!(closed.len(), 3);
        assert!(tracker.is_empty());
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
    fn test_document_version_increments() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/versioned.rs");

        tracker.open(path.clone(), "v1".to_string()).unwrap();
        assert_eq!(tracker.get(&path).unwrap().version, 1);

        tracker.update(&path, "v2".to_string());
        assert_eq!(tracker.get(&path).unwrap().version, 2);

        tracker.update(&path, "v3".to_string());
        assert_eq!(tracker.get(&path).unwrap().version, 3);

        tracker.update(&path, "v4".to_string());
        assert_eq!(tracker.get(&path).unwrap().version, 4);
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
            let uri = path_to_uri(path);
            assert!(
                uri.as_str()
                    .starts_with("file:///home/user/project/main.rs")
            );
        }
    }

    #[test]
    fn test_path_to_uri_with_special_chars() {
        let path = Path::new("/home/user/project-test/main.rs");
        let uri = path_to_uri(path);
        assert!(uri.as_str().starts_with("file://"));
        assert!(uri.as_str().contains("project-test"));
    }

    #[test]
    fn test_path_to_uri_percent_encodes_reserved_chars() {
        #[cfg(windows)]
        let path = Path::new(r"C:\home\user\routes\api\[...]^|.ts");
        #[cfg(not(windows))]
        let path = Path::new("/home/user/routes/api/[...]^|.ts");

        let uri = path_to_uri(path);

        #[cfg(windows)]
        let expected = "file:///C:/home/user/routes/api/%5B...%5D%5E%7C.ts";
        #[cfg(not(windows))]
        let expected = "file:///home/user/routes/api/%5B...%5D%5E%7C.ts";

        assert_eq!(uri.as_str(), expected);
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

    #[cfg(windows)]
    #[test]
    fn test_try_path_to_uri_encodes_synthetic_windows_root() {
        let uri = try_path_to_uri(Path::new("/home/user/#work %23")).unwrap();

        assert_eq!(uri.as_str(), "file:///home/user/%23work%20%2523");
    }

    #[test]
    fn test_path_to_uri_percent_encodes_reserved_chars_in_short_path() {
        // Regression: reserved chars near the URI start must still be encoded.
        #[cfg(windows)]
        let path = Path::new(r"C:\[a].ts");
        #[cfg(not(windows))]
        let path = Path::new("/[a].ts");

        let uri = path_to_uri(path);

        assert!(
            uri.as_str().ends_with("%5Ba%5D.ts"),
            "short path should percent-encode reserved chars, got {}",
            uri.as_str()
        );
        assert_eq!(uri_to_path(&uri).as_deref(), Some(path));
    }

    #[test]
    fn test_document_tracker_concurrent_operations() {
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

        tracker.update(&path1, "new content1".to_string());
        assert_eq!(tracker.get(&path1).unwrap().content, "new content1");
        assert_eq!(tracker.get(&path2).unwrap().content, "content2");

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
        assert_eq!(tracker.get(&path).unwrap().content, "");
    }

    #[test]
    fn test_unicode_content() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/unicode.rs");
        let content = "fn テスト() { println!(\"こんにちは\"); }";

        tracker.open(path.clone(), content.to_string()).unwrap();
        assert_eq!(tracker.get(&path).unwrap().content, content);
    }

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
            tracker
                .open(
                    PathBuf::from(format!("/test/file{i}.rs")),
                    "content".to_string(),
                )
                .unwrap();
        }

        assert_eq!(tracker.len(), 5);

        let result = tracker.open(PathBuf::from("/test/file6.rs"), "content".to_string());
        assert!(matches!(result, Err(Error::DocumentLimitExceeded { .. })));
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
        assert_eq!(state.language_id, "nushell");
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
        assert_eq!(state.language_id, "rust");
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
        let uri: Uri = "file:///home/user/main.rs".parse().unwrap();
        let path = uri_to_path(&uri).unwrap();
        assert_eq!(path, PathBuf::from("/home/user/main.rs"));
    }

    #[test]
    fn test_uri_to_path_non_file_scheme_returns_none() {
        let uri: Uri = "https://example.com/file.rs".parse().unwrap();
        assert!(uri_to_path(&uri).is_none());
    }

    #[test]
    fn test_uri_to_path_lsp_diagnostics_scheme_returns_none() {
        // Custom scheme must not be decoded by uri_to_path.
        let uri: Uri = "lsp-diagnostics:///home/user/main.rs".parse().unwrap();
        assert!(uri_to_path(&uri).is_none());
    }

    #[test]
    fn test_uri_to_path_with_authority_returns_none() {
        // Authority-bearing file URIs must be rejected (UNC path defence).
        // lsp_types::Uri may or may not accept this string; either way
        // uri_to_path should return None.
        let result = "file://server/share/path.rs"
            .parse::<Uri>()
            .ok()
            .and_then(|u| uri_to_path(&u));
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

    use std::process::Stdio;

    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    use tokio::process::{Child, ChildStdin, ChildStdout, Command};

    use crate::config::LspServerConfig;
    use crate::lsp::LspTransport;

    /// Holds both fake-transport child processes alive for a test.
    ///
    /// The read-half's stdin is deliberately never written to, so its `cat`
    /// process never sees EOF on input, never exits, and its stdout (which
    /// backs the transport's `receive()`) never closes -- `receive()` pends
    /// forever instead of observing EOF and tearing down the client's
    /// message loop. Using `echo` here instead would exit immediately and
    /// break every subsequent `notify()` call.
    ///
    /// `write_stdout` is the write-half's own stdout: since `cat` echoes
    /// whatever mcpls writes to its stdin, reading this back is how a test
    /// observes the actual framed JSON-RPC bytes sent to the "server".
    struct FakeServer {
        _write_half: Child,
        _read_half: Child,
        _read_half_stdin: ChildStdin,
        write_stdout: ChildStdout,
    }

    /// Builds an `LspClient` backed by two `cat` child processes so
    /// `notify()` succeeds without a real language server.
    fn fake_lsp_client() -> (LspClient, FakeServer) {
        let mut write_half = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let write_stdin = write_half.stdin.take().unwrap();
        let write_stdout = write_half.stdout.take().unwrap();

        let mut read_half = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let read_stdout = read_half.stdout.take().unwrap();
        let read_stdin = read_half.stdin.take().unwrap();

        let transport = LspTransport::new(write_stdin, read_stdout);
        let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);

        (
            client,
            FakeServer {
                _write_half: write_half,
                _read_half: read_half,
                _read_half_stdin: read_stdin,
                write_stdout,
            },
        )
    }

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

    /// Reads one `Content-Length`-framed JSON-RPC message off `reader`.
    ///
    /// `reader` must be reused across calls (not recreated per message):
    /// a fresh `BufReader` would silently drop any bytes of a later message
    /// it over-read into its internal buffer while parsing an earlier one.
    async fn read_framed_message(reader: &mut BufReader<&mut ChildStdout>) -> serde_json::Value {
        let mut content_length = None;
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some((key, value)) = line.trim_end().split_once(':')
                && key.trim().eq_ignore_ascii_case("content-length")
            {
                content_length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let mut buf = vec![0u8; content_length.unwrap()];
        reader.read_exact(&mut buf).await.unwrap();
        serde_json::from_slice(&buf).unwrap()
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
        assert_eq!(tracker.get(&path).unwrap().version, 1);

        let uri2 = tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        assert_eq!(uri1, uri2);
        assert_eq!(tracker.get(&path).unwrap().version, 1);
        assert_eq!(tracker.get(&path).unwrap().content, "fn main() {}");
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
        assert_eq!(state.version, 2);
        assert_eq!(state.content, "fn main() { println!(\"hi\"); }");
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
            state.version, 2,
            "must resync despite identical (mtime, size)"
        );
        assert_eq!(state.content, "BBBB");
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
        assert_eq!(state.version, 1, "documented limitation: fast path taken");
        assert_eq!(state.content, "AAAA");
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
        assert_eq!(state.version, 2);
        assert_eq!(state.content, "BBBBBBBB");
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
        assert_eq!(tracker.get(&path).unwrap().version, 1);

        tokio::time::advance(Duration::from_millis(300)).await;
        tracker
            .ensure_open(&path, &ServerId::from("rust"), &client)
            .await
            .unwrap();
        let state = tracker.get(&path).unwrap();
        assert_eq!(state.version, 2);
        assert_eq!(state.content, "BBBB");
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
        assert_eq!(tracker.get(&path).unwrap().version, 1);
        assert_eq!(tracker.get(&path).unwrap().content, "fn main() {}");
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
        assert_eq!(tracker.get(&path).unwrap().content, "small");
        assert_eq!(tracker.get(&path).unwrap().version, 1);
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
        assert_eq!(tracker.get(&path).unwrap().version, 2);
    }

    #[tokio::test]
    async fn test_update_clears_disk_provenance() {
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
        assert!(tracker.get(&path).unwrap().disk.is_some());

        tracker.update(&path, "fn main() { updated(); }".to_string());
        assert!(
            tracker.get(&path).unwrap().disk.is_none(),
            "update() must clear disk provenance so the next ensure_open re-verifies by content"
        );
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
            tracker.get(&path).unwrap().synced.get(&id),
            Some(&1),
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
        assert_eq!(tracker.get(&path).unwrap().content, "fn main() {}");
        assert_eq!(tracker.get(&path).unwrap().version, 1);
        assert_eq!(tracker.get(&path).unwrap().synced.get(&id_a), Some(&1));
        assert_eq!(tracker.get(&path).unwrap().synced.get(&id_b), Some(&1));

        // A's next call must independently detect the disk change (B's
        // failure did not consume it) and successfully advance both the
        // shared content/version and its own synced entry.
        tracker.ensure_open(&path, &id_a, &client_a).await.unwrap();
        assert_eq!(
            tracker.get(&path).unwrap().content,
            "fn main() { updated(); }"
        );
        assert_eq!(tracker.get(&path).unwrap().synced.get(&id_a), Some(&2));
        assert_eq!(tracker.get(&path).unwrap().synced.get(&id_b), Some(&1));
    }

    // ------------------------------------------------------------------
    // ensure_open concurrency (issue #227)
    // ------------------------------------------------------------------

    /// Regression for #227: `ensure_open` for one path must not block
    /// `ensure_open` for an unrelated path, even while the first call is
    /// stuck inside its own disk I/O.
    ///
    /// Simulated with a FIFO rather than a timing assumption: opening it for
    /// read blocks deterministically until a writer connects, so path A's
    /// `ensure_open` is guaranteed to still be in progress when path B's
    /// runs. Under the old design (a single lock spanning all of
    /// `ensure_open`, including disk I/O), path B would hang until path A's
    /// FIFO is unblocked below; the per-path lock added here must let it
    /// through immediately instead.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_ensure_open_different_paths_do_not_serialize() {
        let dir = TempDir::new().unwrap();
        let path_a = dir.path().join("a.rs");
        let path_b = dir.path().join("b.rs");

        std::fs::write(&path_b, "fn b() {}").unwrap();
        set_mtime(&path_b, settled_past());

        let status = std::process::Command::new("mkfifo")
            .arg(&path_a)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo must succeed to set up this test");

        let (client_a, _server_a) = fake_lsp_client();
        let (client_b, _server_b) = fake_lsp_client();
        let tracker = Arc::new(DocumentTracker::new(
            ResourceLimits::default(),
            HashMap::new(),
        ));

        // Spawned so it can genuinely block on the FIFO's open() while the
        // rest of this test proceeds concurrently on the same runtime.
        let tracker_for_a = Arc::clone(&tracker);
        let path_a_for_task = path_a.clone();
        let handle_a = tokio::spawn(async move {
            tracker_for_a
                .ensure_open(&path_a_for_task, &ServerId::from("server-a"), &client_a)
                .await
        });

        // Give the spawned task a chance to actually reach the FIFO's
        // blocking open() before racing it against path B below.
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

        // Unblock A: opening the FIFO for writing lets its open() proceed,
        // and closing the write end (at the end of this call) delivers EOF
        // to the read it's waiting to finish.
        let path_a_writer = path_a.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::write(path_a_writer, "fn a() {}").unwrap();
        })
        .await
        .unwrap();

        handle_a.await.unwrap().unwrap();
        assert_eq!(tracker.get(&path_a).unwrap().content, "fn a() {}");
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

        assert_eq!(tracker.get(&path).unwrap().synced.get(&id), Some(&1));
        assert_eq!(tracker.get(&path).unwrap().version, 1);
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
}
