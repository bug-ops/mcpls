//! Filesystem watcher that drives `workspace/didChangeWatchedFiles`.
//!
//! When an LSP server dynamically registers for
//! `workspace/didChangeWatchedFiles` via `client/registerCapability`, mcpls
//! starts a [`notify`] watcher rooted at the configured workspace roots,
//! matches each filesystem event against the server's registered glob
//! patterns, and forwards the matches as `workspace/didChangeWatchedFiles`
//! notifications.
//!
//! The watcher is per-server (each LSP can register its own glob set) and
//! independent of the document tracker: stat-on-access in
//! `bridge::DocumentTracker::ensure_open` already keeps mcpls's own view of
//! tracked files in sync, so this watcher's only job is to keep the LSP
//! server's *workspace index* (files mcpls has not opened) live.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use globset::{Glob, GlobSet, GlobSetBuilder};
use lsp_types::{
    DidChangeWatchedFilesParams, DidChangeWatchedFilesRegistrationOptions, FileChangeType,
    FileEvent, GlobPattern, RelativePattern, Uri, WatchKind,
};
use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::sync::mpsc::TrySendError;

use tokio::sync::{Mutex, mpsc};
use tokio::time::{Duration, Instant};
use tracing::{debug, trace, warn};

use crate::error::{Error, Result};
use crate::lsp::client::LspClient;

/// How long to coalesce filesystem events before flushing them as a single
/// `workspace/didChangeWatchedFiles` notification. Tools like `cargo build`
/// can fire thousands of events per second under `target/`; without
/// debouncing we would flood the LSP server.
const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(100);

/// Sleep time when the event loop has nothing to flush. Used to keep the
/// `tokio::select!` ready branch armed without polling the OS unnecessarily.
const IDLE_SLEEP: Duration = Duration::from_secs(3600);

/// Channel capacity for raw `notify` events. Sized generously because notify
/// itself does not back-pressure; if we lag, events are dropped on the floor.
const RAW_EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Path components that are almost always noise from a build perspective and
/// should never reach an LSP server. Filtered before glob matching to avoid
/// burning CPU on `target/` rewrites etc. Match by exact component name.
const NEVER_FORWARD_COMPONENTS: &[&str] = &[".git", "target", "node_modules", ".cache"];

/// One entry per `FileSystemWatcher` inside an LSP registration. Per-glob
/// `WatchKind` is preserved here so that a registration like
/// `[ {globs:[*.rs], kind:Change}, {globs:[Cargo.toml], kind:Create|Delete} ]`
/// matches each glob against only the kinds the server actually asked for.
#[derive(Debug)]
struct GlobBucket {
    globs: GlobSet,
    kinds: WatchKind,
}

/// A single watcher registration. May hold many [`GlobBucket`]s when the
/// `Registration` includes multiple `FileSystemWatcher`s with differing kinds.
#[derive(Debug)]
struct WatcherRegistration {
    buckets: Vec<GlobBucket>,
}

/// Manages dynamic `workspace/didChangeWatchedFiles` registrations and a
/// shared `notify` watcher.
///
/// Each registered ID maps to a compiled glob set; events are matched against
/// every active registration's globs and forwarded to the LSP server as a
/// single batched notification per debounce interval.
#[derive(Debug)]
pub struct FileWatcher {
    inner: Arc<Mutex<FileWatcherInner>>,
}

#[derive(Debug)]
struct FileWatcherInner {
    /// Workspace roots that the watcher is rooted at. Used to resolve relative
    /// patterns and to filter incoming events to "inside the workspace".
    workspace_roots: Vec<PathBuf>,
    /// Active registrations indexed by registration id.
    registrations: HashMap<String, WatcherRegistration>,
    /// The actual filesystem watcher. Held here so it lives as long as the
    /// `FileWatcher` itself.
    _watcher: RecommendedWatcher,
}

impl FileWatcher {
    /// Spawn a new watcher rooted at `workspace_roots` and forwarding matched
    /// events to `lsp_client` as `workspace/didChangeWatchedFiles`.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying `notify` watcher cannot be created,
    /// or if `workspace_roots` is non-empty but every root failed to register
    /// with `notify::watch`. Individual root failures are logged but do not
    /// fail the spawn as long as at least one root is being watched. Failure
    /// here should be non-fatal at the caller (the `bridge` already covers
    /// per-file freshness via stat-on-access); callers should log and continue.
    pub fn spawn(workspace_roots: Vec<PathBuf>, lsp_client: LspClient) -> Result<Self> {
        // Canonicalize roots so glob matching against canonical event paths
        // works even when the original path goes through symlinks (notably
        // /var → /private/var on macOS, where notify reports canonical paths
        // but the LSP server may have given us the unresolved root).
        let workspace_roots: Vec<PathBuf> = workspace_roots
            .into_iter()
            .map(|root| root.canonicalize().unwrap_or(root))
            .collect();

        let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel(RAW_EVENT_CHANNEL_CAPACITY);

        let mut watcher = notify::recommended_watcher(move |event| {
            // Notify invokes this callback from its own delivery thread(s);
            // never block here. Drop on full to avoid stalling filesystem
            // event delivery under heavy churn (e.g. `target/` rebuilds).
            match raw_tx.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    debug!("file watcher: dropping event, channel full");
                }
                Err(TrySendError::Disconnected(_)) => {
                    warn!("file watcher: dropping event, channel closed");
                }
            }
        })
        .map_err(|e| Error::Transport(format!("notify::recommended_watcher: {e}")))?;

        let mut watched_root_count = 0usize;
        for root in &workspace_roots {
            match watcher.watch(root, RecursiveMode::Recursive) {
                Ok(()) => watched_root_count += 1,
                Err(e) => {
                    warn!("file watcher: failed to watch {}: {e}", root.display());
                }
            }
        }
        if !workspace_roots.is_empty() && watched_root_count == 0 {
            return Err(Error::Transport(
                "file watcher: failed to watch any workspace root".to_string(),
            ));
        }

        let inner = Arc::new(Mutex::new(FileWatcherInner {
            workspace_roots,
            registrations: HashMap::new(),
            _watcher: watcher,
        }));

        // Bridge the blocking std channel to a tokio channel.
        let (event_tx, event_rx) =
            mpsc::channel::<notify::Result<notify::Event>>(RAW_EVENT_CHANNEL_CAPACITY);
        std::thread::spawn(move || {
            while let Ok(event) = raw_rx.recv() {
                if event_tx.blocking_send(event).is_err() {
                    break;
                }
            }
        });

        let inner_for_loop = Arc::clone(&inner);
        tokio::spawn(forward_events_loop(inner_for_loop, event_rx, lsp_client));

        Ok(Self { inner })
    }

    /// Install a `workspace/didChangeWatchedFiles` registration.
    ///
    /// Each [`Registration`] from `client/registerCapability` whose `method`
    /// is `workspace/didChangeWatchedFiles` should be passed here. Subsequent
    /// filesystem events are matched against the new globs from the next
    /// debounce flush onward.
    ///
    /// [`Registration`]: lsp_types::Registration
    ///
    /// # Errors
    ///
    /// Returns an error if `register_options` cannot be deserialized or if
    /// any glob pattern fails to compile.
    pub async fn register(&self, id: String, register_options: serde_json::Value) -> Result<()> {
        let opts: DidChangeWatchedFilesRegistrationOptions =
            serde_json::from_value(register_options).map_err(|e| {
                Error::LspProtocolError(format!(
                    "invalid didChangeWatchedFiles register options: {e}"
                ))
            })?;

        let workspace_roots = {
            let guard = self.inner.lock().await;
            guard.workspace_roots.clone()
        };

        let mut buckets: Vec<GlobBucket> = Vec::with_capacity(opts.watchers.len());
        for fs_watcher in &opts.watchers {
            let mut builder = GlobSetBuilder::new();
            let mut compiled = 0usize;
            for glob_str in resolve_pattern(&fs_watcher.glob_pattern, &workspace_roots) {
                match Glob::new(&glob_str) {
                    Ok(glob) => {
                        builder.add(glob);
                        compiled += 1;
                    }
                    Err(e) => {
                        warn!(
                            "file watcher: ignoring uncompilable glob '{glob_str}' for registration {id}: {e}"
                        );
                    }
                }
            }
            if compiled == 0 {
                continue;
            }
            let globs = builder
                .build()
                .map_err(|e| Error::LspProtocolError(format!("globset build failed: {e}")))?;
            let kinds = fs_watcher
                .kind
                .unwrap_or(WatchKind::Create | WatchKind::Change | WatchKind::Delete);
            buckets.push(GlobBucket { globs, kinds });
        }

        let bucket_count = buckets.len();
        {
            let mut guard = self.inner.lock().await;
            guard
                .registrations
                .insert(id.clone(), WatcherRegistration { buckets });
        }

        debug!("file watcher: registered {id} ({bucket_count} buckets)");
        Ok(())
    }

    /// Remove a previously installed registration.
    pub async fn unregister(&self, id: &str) {
        let mut guard = self.inner.lock().await;
        if guard.registrations.remove(id).is_some() {
            debug!("file watcher: unregistered {id}");
        }
    }

    /// Cheap clone of the watcher handle for use by request dispatchers.
    /// Both handles share the same underlying state.
    #[must_use]
    pub fn clone_handle(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// Resolve an LSP glob pattern into one or more `globset`-compatible pattern
/// strings. Relative patterns are anchored to their base URI by prepending
/// the absolute path. Bare string patterns are anchored to the entire workspace
/// with a `**/` prefix when they don't already start with one (or with `/`),
/// because `globset` matches the *full path* — a bare `*.rs` would never match
/// `/repo/src/lib.rs`, only filenames at the root.
fn resolve_pattern(pattern: &GlobPattern, workspace_roots: &[PathBuf]) -> Vec<String> {
    match pattern {
        GlobPattern::String(s) => vec![anchor_bare_pattern(s)],
        GlobPattern::Relative(rel) => relative_pattern_to_globs(rel, workspace_roots),
    }
}

/// Anchor a bare LSP glob with `**/` so it matches anywhere under the
/// workspace. Already-absolute patterns and patterns that already start with
/// `**/` are returned unchanged.
fn anchor_bare_pattern(pattern: &str) -> String {
    if pattern.starts_with('/') || pattern.starts_with("**/") {
        pattern.to_string()
    } else {
        format!("**/{pattern}")
    }
}

/// Expand a `RelativePattern` into one absolute glob per matching workspace
/// root.
fn relative_pattern_to_globs(rel: &RelativePattern, workspace_roots: &[PathBuf]) -> Vec<String> {
    let Some(base_path) = base_uri_to_path(&rel.base_uri) else {
        warn!("file watcher: dropping relative pattern with non-file base URI");
        return Vec::new();
    };

    // Canonicalize so the resulting glob matches event paths reported by
    // notify (which are canonical: e.g. `/var/folders/...` resolves to
    // `/private/var/folders/...` on macOS).
    let base_path = base_path.canonicalize().unwrap_or(base_path);

    // Some servers send a base URI matching exactly one of our workspace
    // roots; others send a child path. Either way, build the absolute glob.
    let pattern = format!("{}/{}", base_path.display(), rel.pattern);

    // If the base path is unrelated to any workspace root, still keep the
    // pattern: notify is rooted at workspace_roots, so events under unrelated
    // paths simply will not match.
    if !workspace_roots.is_empty()
        && !workspace_roots
            .iter()
            .any(|root| base_path.starts_with(root) || root.starts_with(&base_path))
    {
        trace!(
            "file watcher: relative pattern base {} is outside all workspace roots",
            base_path.display()
        );
    }

    vec![pattern]
}

/// Resolve an LSP `BaseUri` (workspace folder or absolute URI) to a filesystem
/// path. Returns `None` if the URI does not have a `file://` scheme.
fn base_uri_to_path(base: &lsp_types::OneOf<lsp_types::WorkspaceFolder, Uri>) -> Option<PathBuf> {
    let uri = match base {
        lsp_types::OneOf::Left(folder) => &folder.uri,
        lsp_types::OneOf::Right(uri) => uri,
    };
    uri_to_path(uri)
}

fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let rest = s.strip_prefix("file://")?;
    // Handle Windows "file:///C:/..." form.
    #[cfg(windows)]
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    Some(PathBuf::from(rest))
}

fn path_to_uri(path: &Path) -> Option<Uri> {
    let s = path.to_str()?;
    let uri_str = if cfg!(windows) {
        format!("file:///{}", s.replace('\\', "/"))
    } else {
        format!("file://{s}")
    };
    Uri::from_str(&uri_str).ok()
}

/// Tokio task: pull raw notify events, match, debounce, and forward.
async fn forward_events_loop(
    inner: Arc<Mutex<FileWatcherInner>>,
    mut event_rx: mpsc::Receiver<notify::Result<notify::Event>>,
    lsp_client: LspClient,
) {
    let mut pending: HashMap<PathBuf, FileChangeType> = HashMap::new();
    let mut deadline: Option<Instant> = None;

    loop {
        let timeout = deadline.map_or(IDLE_SLEEP, |d| d.saturating_duration_since(Instant::now()));

        tokio::select! {
            maybe_event = event_rx.recv() => {
                let Some(event) = maybe_event else { break };
                let event = match event {
                    Ok(ev) => ev,
                    Err(e) => {
                        warn!("file watcher: notify error: {e}");
                        continue;
                    }
                };
                merge_event(&mut pending, &event);
                if !pending.is_empty() && deadline.is_none() {
                    deadline = Some(Instant::now() + DEBOUNCE_INTERVAL);
                }
            }
            () = tokio::time::sleep(timeout), if deadline.is_some() => {
                deadline = None;
                if pending.is_empty() {
                    continue;
                }
                let drained: Vec<(PathBuf, FileChangeType)> = pending.drain().collect();
                flush_pending(&inner, &lsp_client, drained).await;
            }
        }
    }
    debug!("file watcher: event-forward loop exiting");
}

/// Fold a single notify event into `pending`. The same path may legitimately
/// appear with multiple types in one debounce window (e.g. a quick
/// create-then-modify); the LSP spec is ambiguous so we keep the latest type.
fn merge_event(pending: &mut HashMap<PathBuf, FileChangeType>, event: &notify::Event) {
    let Some(typ) = notify_kind_to_lsp(&event.kind) else {
        return;
    };
    for path in &event.paths {
        if NEVER_FORWARD_COMPONENTS.iter().any(|skip| {
            path.components()
                .any(|c| matches!(c, Component::Normal(s) if s == *skip))
        }) {
            continue;
        }
        pending.insert(path.clone(), typ);
    }
}

/// Translate a `notify::EventKind` into an LSP `FileChangeType`. Returns
/// `None` for events we deliberately do not forward (e.g. metadata-only
/// `Modify(Metadata)` changes that do not affect file content).
#[allow(clippy::trivially_copy_pass_by_ref)] // notify::EventKind is large; clippy mis-sizes it
#[allow(clippy::missing_const_for_fn)] // pattern matching on EventKind variants is not stable in const
fn notify_kind_to_lsp(kind: &EventKind) -> Option<FileChangeType> {
    match kind {
        EventKind::Create(CreateKind::File | CreateKind::Folder | CreateKind::Any) => {
            Some(FileChangeType::CREATED)
        }
        EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any) => {
            Some(FileChangeType::CHANGED)
        }
        EventKind::Remove(RemoveKind::File | RemoveKind::Folder | RemoveKind::Any) => {
            Some(FileChangeType::DELETED)
        }
        // Metadata-only changes, access events, and unknown kinds are ignored.
        _ => None,
    }
}

/// Match the drained event set against active registrations and send the
/// resulting `workspace/didChangeWatchedFiles` notification, if any matches.
async fn flush_pending(
    inner: &Arc<Mutex<FileWatcherInner>>,
    lsp_client: &LspClient,
    pending: Vec<(PathBuf, FileChangeType)>,
) {
    let changes = {
        let guard = inner.lock().await;
        if guard.registrations.is_empty() {
            return;
        }
        compute_changes(&guard.registrations, pending)
    };

    if changes.is_empty() {
        return;
    }

    let params = DidChangeWatchedFilesParams { changes };
    if let Err(e) = lsp_client
        .notify("workspace/didChangeWatchedFiles", params)
        .await
    {
        warn!("file watcher: failed to send didChangeWatchedFiles: {e}");
    }
}

/// Pure helper: match the pending events against the active registrations
/// and translate hits into LSP `FileEvent`s.
fn compute_changes(
    registrations: &HashMap<String, WatcherRegistration>,
    pending: Vec<(PathBuf, FileChangeType)>,
) -> Vec<FileEvent> {
    let mut changes: Vec<FileEvent> = Vec::new();
    for (path, typ) in pending {
        let matched = registrations.values().any(|r| {
            r.buckets
                .iter()
                .any(|b| bucket_accepts(b, typ) && b.globs.is_match(&path))
        });
        if !matched {
            continue;
        }
        let Some(uri) = path_to_uri(&path) else {
            continue;
        };
        changes.push(FileEvent { uri, typ });
    }
    changes
}

/// Whether the bucket accepts change kind `typ`. The kind bitmask in LSP
/// defaults to all three types when unset.
fn bucket_accepts(bucket: &GlobBucket, typ: FileChangeType) -> bool {
    let want = if bucket.kinds.is_empty() {
        WatchKind::Create | WatchKind::Change | WatchKind::Delete
    } else {
        bucket.kinds
    };
    match typ {
        FileChangeType::CREATED => want.contains(WatchKind::Create),
        FileChangeType::CHANGED => want.contains(WatchKind::Change),
        FileChangeType::DELETED => want.contains(WatchKind::Delete),
        // FileChangeType is a transparent newtype around i32; unknown values
        // are forwarded as no-op rejections.
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_notify_kind_to_lsp_known_variants() {
        assert_eq!(
            notify_kind_to_lsp(&EventKind::Create(CreateKind::File)),
            Some(FileChangeType::CREATED)
        );
        assert_eq!(
            notify_kind_to_lsp(&EventKind::Modify(ModifyKind::Data(
                notify::event::DataChange::Content
            ))),
            Some(FileChangeType::CHANGED)
        );
        assert_eq!(
            notify_kind_to_lsp(&EventKind::Remove(RemoveKind::File)),
            Some(FileChangeType::DELETED)
        );
    }

    #[test]
    fn test_notify_kind_to_lsp_ignores_metadata() {
        assert_eq!(
            notify_kind_to_lsp(&EventKind::Modify(ModifyKind::Metadata(
                notify::event::MetadataKind::Permissions
            ))),
            None
        );
        assert_eq!(
            notify_kind_to_lsp(&EventKind::Access(notify::event::AccessKind::Any)),
            None
        );
    }

    #[test]
    fn test_merge_event_skips_target_dir() {
        let mut pending: HashMap<PathBuf, FileChangeType> = HashMap::new();
        let evt = notify::Event {
            kind: EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            paths: vec![PathBuf::from("/repo/target/debug/foo.rs")],
            attrs: notify::event::EventAttributes::new(),
        };
        merge_event(&mut pending, &evt);
        assert!(pending.is_empty(), "events under target/ must be filtered");
    }

    #[test]
    fn test_merge_event_keeps_latest_type_per_path() {
        let mut pending: HashMap<PathBuf, FileChangeType> = HashMap::new();
        let path = PathBuf::from("/repo/src/lib.rs");
        let create = notify::Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![path.clone()],
            attrs: notify::event::EventAttributes::new(),
        };
        let modify = notify::Event {
            kind: EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            paths: vec![path.clone()],
            attrs: notify::event::EventAttributes::new(),
        };
        merge_event(&mut pending, &create);
        merge_event(&mut pending, &modify);
        assert_eq!(pending.get(&path), Some(&FileChangeType::CHANGED));
    }

    #[test]
    fn test_uri_round_trip() {
        let path = PathBuf::from("/tmp/example/file.rs");
        let uri = path_to_uri(&path).unwrap();
        assert_eq!(uri_to_path(&uri).unwrap(), path);
    }

    #[test]
    fn test_bucket_accepts_default_kind() {
        let bucket = GlobBucket {
            globs: GlobSetBuilder::new().build().unwrap(),
            kinds: WatchKind::empty(),
        };
        assert!(bucket_accepts(&bucket, FileChangeType::CREATED));
        assert!(bucket_accepts(&bucket, FileChangeType::CHANGED));
        assert!(bucket_accepts(&bucket, FileChangeType::DELETED));
    }

    #[test]
    fn test_bucket_accepts_explicit_kind() {
        let bucket = GlobBucket {
            globs: GlobSetBuilder::new().build().unwrap(),
            kinds: WatchKind::Change,
        };
        assert!(!bucket_accepts(&bucket, FileChangeType::CREATED));
        assert!(bucket_accepts(&bucket, FileChangeType::CHANGED));
        assert!(!bucket_accepts(&bucket, FileChangeType::DELETED));
    }

    #[test]
    fn test_anchor_bare_pattern_anchors_extension_globs() {
        assert_eq!(anchor_bare_pattern("*.rs"), "**/*.rs");
        assert_eq!(anchor_bare_pattern("Cargo.toml"), "**/Cargo.toml");
    }

    #[test]
    fn test_anchor_bare_pattern_preserves_already_anchored() {
        assert_eq!(anchor_bare_pattern("**/*.rs"), "**/*.rs");
        assert_eq!(anchor_bare_pattern("/repo/src/*.rs"), "/repo/src/*.rs");
    }

    #[test]
    fn test_compute_changes_respects_per_bucket_kind() {
        let mut create_only = GlobSetBuilder::new();
        create_only.add(Glob::new("**/Cargo.toml").unwrap());
        let mut change_only = GlobSetBuilder::new();
        change_only.add(Glob::new("**/*.rs").unwrap());

        let mut regs = HashMap::new();
        regs.insert(
            "1".to_string(),
            WatcherRegistration {
                buckets: vec![
                    GlobBucket {
                        globs: create_only.build().unwrap(),
                        kinds: WatchKind::Create,
                    },
                    GlobBucket {
                        globs: change_only.build().unwrap(),
                        kinds: WatchKind::Change,
                    },
                ],
            },
        );

        // .rs CHANGED → matches change_only bucket
        let changes = compute_changes(
            &regs,
            vec![(PathBuf::from("/repo/src/lib.rs"), FileChangeType::CHANGED)],
        );
        assert_eq!(changes.len(), 1);

        // .rs CREATED → globs match change_only but kinds reject; create_only
        // kinds accept but globs don't. No match overall.
        let changes = compute_changes(
            &regs,
            vec![(PathBuf::from("/repo/src/lib.rs"), FileChangeType::CREATED)],
        );
        assert!(
            changes.is_empty(),
            "create on a *.rs file must not match a Change-only bucket"
        );

        // Cargo.toml CREATED → matches create_only bucket
        let changes = compute_changes(
            &regs,
            vec![(PathBuf::from("/repo/Cargo.toml"), FileChangeType::CREATED)],
        );
        assert_eq!(changes.len(), 1);
    }
}
