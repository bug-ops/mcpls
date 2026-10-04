//! Canonicalization of the URIs LSP servers publish diagnostics for.
//!
//! The diagnostics pump must map every published URI to its canonical form
//! before caching it, which costs a filesystem round-trip per publish. On a
//! slow or networked filesystem that serialises a cold burst (a server
//! publishing for thousands of files after indexing), so the pump owns a
//! [`PublishedPathResolver`]: a TTL memo in front of a bounded-parallelism
//! blocking canonicalizer.
//!
//! # Guarantees
//!
//! - Results come back in input order, so a clear (an empty publish) can never
//!   be reordered behind or dropped in favour of an earlier publish.
//! - Only definitive results are memoized. A path that does not exist falls
//!   back to its nearest existing ancestor (and is cached); any other error
//!   (`ESTALE`, `EIO`, `EACCES`) is transient and is retried on the next
//!   publish.
//! - At most [`RESOLVE_CONCURRENCY`] canonicalizations run at once. They run
//!   on tokio's blocking pool and cannot be interrupted: when the caller drops
//!   the batch future (pump cancellation) a call stuck on a hung filesystem
//!   is abandoned and finishes whenever the filesystem lets it. Each pump
//!   generation can therefore abandon up to [`RESOLVE_CONCURRENCY`] blocking
//!   threads; a respawn loop against a permanently hung filesystem grows
//!   toward tokio's blocking-pool limit.
//! - A memoized entry can outlive a symlink retarget by up to [`MEMO_TTL`].
//!   The workspace containment check runs on every call, after the memo, so a
//!   stale entry can only misname a file the workspace already admitted.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use lsp_types::Uri;
use thiserror::Error;
use tracing::{debug, warn};

use super::resources::{CanonicalForm, PublishedDiagnosticsUri};
use super::{WorkspaceRoots, lexically_normalize, uri_to_path};

/// Maximum canonicalizations running concurrently for one resolver.
pub const RESOLVE_CONCURRENCY: usize = 8;

/// How long a resolved path stays memoized.
pub const MEMO_TTL: Duration = Duration::from_secs(60);

/// Maximum memoized paths; a full memo drops expired entries, then the oldest half.
pub const MEMO_CAPACITY: usize = 8192;

/// Delays before each retry of a transiently failing canonicalization.
pub const TRANSIENT_RETRY_BACKOFF: [Duration; 2] =
    [Duration::from_millis(50), Duration::from_millis(200)];

/// How long a path whose canonicalization kept failing transiently is left
/// alone, so a persistently broken ancestor costs one retry round, not one per
/// publish.
pub const TRANSIENT_HOLDOFF: Duration = Duration::from_secs(2);

/// A single canonicalization slower than this is logged (once per resolver).
pub const SLOW_CANONICALIZE: Duration = Duration::from_millis(100);

/// Canonicalizes one existing path; injectable so tests can simulate a slow
/// or failing filesystem.
pub type CanonicalizeFn = dyn Fn(&Path) -> io::Result<PathBuf> + Send + Sync;

/// Why a published path has no canonical form right now.
#[derive(Debug, Error)]
pub enum Unresolved {
    /// The filesystem failed in a way that may not repeat; never cached.
    #[error("transient filesystem error: {0}")]
    Transient(#[source] io::Error),
    /// Not even the root of the path could be canonicalized.
    #[error("no ancestor of the path exists")]
    NoExistingAncestor,
    /// A `..` component: resolving it lexically after a missing component
    /// could step around a symlink, so the path is refused outright.
    #[error("path contains a `..` component")]
    ParentComponent,
}

/// Canonicalizes the longest existing ancestor of `path` with `canonicalize`
/// and appends the remainder.
///
/// Falls back to the next ancestor only on [`io::ErrorKind::NotFound`] and
/// [`io::ErrorKind::NotADirectory`]; every other error is [`Unresolved::Transient`].
/// A path with a `..` component is [`Unresolved::ParentComponent`]: a legitimate
/// server publishes canonical paths, and the lexical join of the missing tail
/// would otherwise bypass symlinks that `..` should have followed.
fn canonicalize_published(
    path: &Path,
    canonicalize: &CanonicalizeFn,
) -> Result<PathBuf, Unresolved> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Unresolved::ParentComponent);
    }
    for ancestor in path.ancestors() {
        match canonicalize(ancestor) {
            Ok(canonical) => {
                return path
                    .strip_prefix(ancestor)
                    .map(|rest| lexically_normalize(&canonical.join(rest)))
                    .map_err(|_| Unresolved::NoExistingAncestor);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(e) => return Err(Unresolved::Transient(e)),
        }
    }
    Err(Unresolved::NoExistingAncestor)
}

/// What a published URI announces, which decides how a persistent transient
/// filesystem error is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationKind {
    /// Carries diagnostics; dropped when its path cannot be resolved.
    Diagnostics,
    /// Carries none: it only empties the entry, so it must never be lost.
    Clear,
}

/// One published URI handed to [`PublishedPathResolver::resolve_batch`].
#[derive(Debug, Clone, Copy)]
pub struct Publication<'a> {
    /// The URI exactly as the server published it.
    pub uri: &'a Uri,
    /// Whether it carries diagnostics or clears them.
    pub kind: PublicationKind,
}

/// Outcome of canonicalizing one cache-missed path.
#[derive(Debug)]
enum Resolution {
    Canonical(PathBuf),
    Transient,
}

/// Memo of resolved published paths with a TTL and a size bound.
#[derive(Debug, Default)]
struct ResolveMemo {
    entries: HashMap<PathBuf, (PathBuf, Instant)>,
    failed: HashMap<PathBuf, Instant>,
}

impl ResolveMemo {
    /// The canonical form last resolved for `source`, however old; used only
    /// to place a clear when the filesystem is failing.
    fn last_known(&self, source: &Path) -> Option<&Path> {
        self.entries
            .get(source)
            .map(|(canonical, _)| canonical.as_path())
    }

    fn recently_failed(&self, source: &Path, now: Instant) -> bool {
        self.failed
            .get(source)
            .is_some_and(|at| now.saturating_duration_since(*at) < TRANSIENT_HOLDOFF)
    }

    fn note_failure(&mut self, source: PathBuf, now: Instant) {
        if self.failed.len() >= MEMO_CAPACITY {
            self.failed
                .retain(|_, at| now.saturating_duration_since(*at) < TRANSIENT_HOLDOFF);
            if self.failed.len() >= MEMO_CAPACITY {
                self.failed.clear();
            }
        }
        self.failed.insert(source, now);
    }

    fn get(&self, source: &Path, now: Instant) -> Option<&Path> {
        self.entries
            .get(source)
            .filter(|(_, stored)| now.saturating_duration_since(*stored) < MEMO_TTL)
            .map(|(canonical, _)| canonical.as_path())
    }

    fn insert(&mut self, source: PathBuf, canonical: PathBuf, now: Instant) {
        if self.entries.len() >= MEMO_CAPACITY && !self.entries.contains_key(&source) {
            self.entries
                .retain(|_, (_, stored)| now.saturating_duration_since(*stored) < MEMO_TTL);
            if self.entries.len() >= MEMO_CAPACITY {
                let mut by_age: Vec<(Instant, PathBuf)> = self
                    .entries
                    .iter()
                    .map(|(source, (_, stored))| (*stored, source.clone()))
                    .collect();
                by_age.sort_unstable();
                for (_, source) in by_age.into_iter().take(MEMO_CAPACITY / 2) {
                    self.entries.remove(&source);
                }
            }
        }
        self.failed.remove(&source);
        self.entries.insert(source, (canonical, now));
    }
}

/// Pump-local resolver turning published URIs into [`PublishedDiagnosticsUri`]s.
///
/// See the [module docs](self) for the ordering, caching and cancellation
/// guarantees.
pub struct PublishedPathResolver {
    canonicalize: Arc<CanonicalizeFn>,
    memo: ResolveMemo,
    slow_logged: Arc<AtomicBool>,
}

impl std::fmt::Debug for PublishedPathResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishedPathResolver")
            .field("memo", &self.memo)
            .finish_non_exhaustive()
    }
}

impl PublishedPathResolver {
    /// A resolver backed by the real filesystem.
    pub fn new() -> Self {
        Self::build(Arc::new(|p: &Path| dunce::canonicalize(p)))
    }

    #[cfg(test)]
    pub fn with_canonicalizer(canonicalize: Arc<CanonicalizeFn>) -> Self {
        Self::build(canonicalize)
    }

    fn build(canonicalize: Arc<CanonicalizeFn>) -> Self {
        Self {
            canonicalize,
            memo: ResolveMemo::default(),
            slow_logged: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Resolves `publications` against `roots`, returning one entry
    /// per input in the same order.
    ///
    /// An entry is `None` for a non-`file:` URI, a path that cannot be
    /// resolved, or a canonical path outside every root (for example a symlink
    /// pointing out of the workspace). A transient filesystem error is retried
    /// after each of [`TRANSIENT_RETRY_BACKOFF`]; if it persists, a
    /// [`PublicationKind::Clear`] is placed under the path's last known
    /// canonical form, or failing that its own lexically normalized spelling
    /// marked [`CanonicalForm::Fallback`] (a clear only ever empties an entry,
    /// and dropping it would leave the client with stale errors), while any
    /// other publication is dropped until the next one. A path that failed
    /// transiently is left alone for [`TRANSIENT_HOLDOFF`]. Cache misses are
    /// canonicalized on the blocking pool with at most [`RESOLVE_CONCURRENCY`]
    /// in flight; dropping the returned future abandons the calls still
    /// running.
    pub async fn resolve_batch(
        &mut self,
        publications: &[Publication<'_>],
        roots: &WorkspaceRoots,
    ) -> Vec<Option<PublishedDiagnosticsUri>> {
        let sources: Vec<Option<PathBuf>> = publications
            .iter()
            .map(|publication| uri_to_path(publication.uri))
            .collect();
        let now = Instant::now();

        let mut seen: HashSet<&Path> = HashSet::new();
        let mut misses: Vec<PathBuf> = Vec::new();
        for source in sources.iter().flatten() {
            if self.memo.get(source, now).is_none()
                && !self.memo.recently_failed(source, now)
                && seen.insert(source)
            {
                misses.push(source.clone());
            }
        }

        let fresh = self.canonicalize_all(misses).await;

        let now = Instant::now();
        sources
            .into_iter()
            .zip(publications)
            .map(|(source, publication)| {
                let source = source?;
                let transient = matches!(fresh.get(&source), Some(Resolution::Transient))
                    || self.memo.recently_failed(&source, now);
                let (canonical, form) = if let Some(hit) = self.memo.get(&source, now) {
                    (hit.to_path_buf(), CanonicalForm::Resolved)
                } else if let Some(Resolution::Canonical(path)) = fresh.get(&source) {
                    (path.clone(), CanonicalForm::Resolved)
                } else if transient && publication.kind == PublicationKind::Clear {
                    self.memo.last_known(&source).map_or_else(
                        || (lexically_normalize(&source), CanonicalForm::Fallback),
                        |known| (known.to_path_buf(), CanonicalForm::Resolved),
                    )
                } else {
                    return None;
                };
                PublishedDiagnosticsUri::from_canonical_path(
                    publication.uri,
                    &source,
                    &canonical,
                    form,
                    roots,
                )
            })
            .collect()
    }

    /// Canonicalizes `misses` in order with bounded parallelism, memoizing the
    /// definitive results and returning every outcome worth acting on by
    /// source path.
    async fn canonicalize_all(&mut self, misses: Vec<PathBuf>) -> HashMap<PathBuf, Resolution> {
        let canonicalize = &self.canonicalize;
        let slow_logged = &self.slow_logged;
        let results: Vec<(PathBuf, Result<PathBuf, Unresolved>)> = futures::stream::iter(misses)
            .map(|source| {
                let canonicalize = Arc::clone(canonicalize);
                let slow_logged = Arc::clone(slow_logged);
                async move {
                    let mut backoff = TRANSIENT_RETRY_BACKOFF.iter();
                    let outcome = loop {
                        let outcome = canonicalize_blocking(
                            source.clone(),
                            Arc::clone(&canonicalize),
                            Arc::clone(&slow_logged),
                        )
                        .await;
                        match (&outcome, backoff.next()) {
                            (Err(Unresolved::Transient(_)), Some(delay)) => {
                                tokio::time::sleep(*delay).await;
                            }
                            _ => break outcome,
                        }
                    };
                    (source, outcome)
                }
            })
            .buffered(RESOLVE_CONCURRENCY)
            .collect()
            .await;

        let now = Instant::now();
        let mut fresh = HashMap::with_capacity(results.len());
        for (source, outcome) in results {
            match outcome {
                Ok(canonical) => {
                    self.memo.insert(source.clone(), canonical.clone(), now);
                    fresh.insert(source, Resolution::Canonical(canonical));
                }
                Err(e @ Unresolved::Transient(_)) => {
                    self.memo.note_failure(source.clone(), now);
                    debug!(
                        "transient error resolving published path {}: {e}",
                        source.display()
                    );
                    fresh.insert(source, Resolution::Transient);
                }
                Err(e) => debug!("not resolving published path {}: {e}", source.display()),
            }
        }
        fresh
    }
}

/// Runs [`canonicalize_published`] on the blocking pool, logging the first
/// call slower than [`SLOW_CANONICALIZE`].
async fn canonicalize_blocking(
    path: PathBuf,
    canonicalize: Arc<CanonicalizeFn>,
    slow_logged: Arc<AtomicBool>,
) -> Result<PathBuf, Unresolved> {
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let outcome = canonicalize_published(&path, canonicalize.as_ref());
        if started.elapsed() > SLOW_CANONICALIZE && !slow_logged.swap(true, Ordering::Relaxed) {
            warn!(
                "canonicalizing a published diagnostics path took {:?} (> {SLOW_CANONICALIZE:?}); \
                 the filesystem is slow",
                started.elapsed()
            );
        }
        outcome
    })
    .await
    .unwrap_or_else(|join| Err(Unresolved::Transient(io::Error::other(join.to_string()))))
}

/// A diagnostics-bearing [`Publication`] for `uri`.
#[cfg(test)]
pub const fn diagnostics(uri: &Uri) -> Publication<'_> {
    Publication {
        uri,
        kind: PublicationKind::Diagnostics,
    }
}

/// Resolves a single URI with a throwaway resolver.
#[cfg(test)]
pub async fn resolve_one(uri: &Uri, roots: &WorkspaceRoots) -> Option<PublishedDiagnosticsUri> {
    PublishedPathResolver::new()
        .resolve_batch(&[diagnostics(uri)], roots)
        .await
        .pop()
        .flatten()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::bridge::path_to_uri;

    fn root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        (dir, root)
    }

    fn roots_of(root: &Path) -> WorkspaceRoots {
        WorkspaceRoots::from_configured(&[root.to_path_buf()]).unwrap()
    }

    fn uri(path: &Path) -> Uri {
        path_to_uri(path).unwrap()
    }

    /// Identity canonicalizer that counts calls and fails per `fail`.
    fn counting(
        calls: &Arc<AtomicUsize>,
        fail: impl Fn(&Path, usize) -> Option<io::ErrorKind> + Send + Sync + 'static,
    ) -> Arc<CanonicalizeFn> {
        let calls = Arc::clone(calls);
        Arc::new(move |p: &Path| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            fail(p, n).map_or_else(|| Ok(p.to_path_buf()), |kind| Err(io::Error::from(kind)))
        })
    }

    #[test]
    fn test_canonicalize_published_falls_back_on_not_found_and_not_a_directory() {
        let canon = |p: &Path| -> io::Result<PathBuf> {
            match p.components().count() {
                0..=2 => Ok(p.to_path_buf()),
                3 => Err(io::Error::from(io::ErrorKind::NotADirectory)),
                _ => Err(io::Error::from(io::ErrorKind::NotFound)),
            }
        };
        let resolved = canonicalize_published(Path::new("/a/b/c/d.rs"), &canon).unwrap();
        assert_eq!(resolved, Path::new("/a/b/c/d.rs"));
    }

    #[test]
    fn test_canonicalize_published_rejects_parent_components() {
        let canon = |p: &Path| -> io::Result<PathBuf> { Ok(p.to_path_buf()) };
        assert!(matches!(
            canonicalize_published(Path::new("/ws/missing/../link/f.rs"), &canon),
            Err(Unresolved::ParentComponent)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_missing_component_then_parent_cannot_step_around_a_symlink() {
        let (_dir, root) = root();
        let outside = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        let raw = format!("{}/missing/../link/f.rs", root.display());
        let uri = Uri::from(format!("file://{raw}").as_str());

        assert!(resolve_one(&uri, &roots_of(&root)).await.is_none());
    }

    #[test]
    fn test_memo_eviction_keeps_the_newest_half() {
        let mut memo = ResolveMemo::default();
        let t0 = Instant::now();
        for i in 0..MEMO_CAPACITY {
            memo.insert(
                PathBuf::from(format!("/p{i}")),
                "/c".into(),
                t0 + Duration::from_millis(i as u64),
            );
        }
        memo.insert("/new".into(), "/c".into(), t0 + Duration::from_secs(1));
        assert!(memo.entries.len() <= MEMO_CAPACITY / 2 + 1);
        assert!(
            memo.get(Path::new("/new"), t0 + Duration::from_secs(1))
                .is_some()
        );
        assert!(
            memo.entries
                .contains_key(Path::new(&format!("/p{}", MEMO_CAPACITY - 1)))
        );
        assert!(!memo.entries.contains_key(Path::new("/p0")));
    }

    #[test]
    fn test_canonicalize_published_other_errors_are_transient() {
        let canon = |_: &Path| -> io::Result<PathBuf> {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        };
        assert!(matches!(
            canonicalize_published(Path::new("/a/b.rs"), &canon),
            Err(Unresolved::Transient(_))
        ));
    }

    #[test]
    fn test_memo_entry_expires_after_ttl() {
        let mut memo = ResolveMemo::default();
        let t0 = Instant::now();
        memo.insert("/a".into(), "/b".into(), t0);
        assert_eq!(
            memo.get(Path::new("/a"), t0 + MEMO_TTL / 2),
            Some(Path::new("/b"))
        );
        assert_eq!(memo.get(Path::new("/a"), t0 + MEMO_TTL), None);
    }

    #[test]
    fn test_memo_is_bounded() {
        let mut memo = ResolveMemo::default();
        let t0 = Instant::now();
        for i in 0..=MEMO_CAPACITY {
            memo.insert(PathBuf::from(format!("/p{i}")), "/c".into(), t0);
        }
        assert!(memo.entries.len() <= MEMO_CAPACITY);
        assert!(
            memo.get(Path::new(&format!("/p{MEMO_CAPACITY}")), t0)
                .is_some()
        );
    }

    #[tokio::test]
    async fn test_memo_hit_skips_canonicalization() {
        let (_dir, root) = root();
        let file = uri(&root.join("a.rs"));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut resolver = PublishedPathResolver::with_canonicalizer(counting(&calls, |_, _| None));

        let roots = roots_of(&root);
        assert!(resolver.resolve_batch(&[diagnostics(&file)], &roots).await[0].is_some());
        assert!(resolver.resolve_batch(&[diagnostics(&file)], &roots).await[0].is_some());

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_transient_failure_is_held_off_not_cached_as_success() {
        let (_dir, root) = root();
        let file = uri(&root.join("a.rs"));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut resolver = PublishedPathResolver::with_canonicalizer(counting(&calls, |_, n| {
            (n < 3).then_some(io::ErrorKind::TimedOut)
        }));
        let roots = roots_of(&root);

        assert!(resolver.resolve_batch(&[diagnostics(&file)], &roots).await[0].is_none());
        assert!(resolver.resolve_batch(&[diagnostics(&file)], &roots).await[0].is_none());

        assert_eq!(calls.load(Ordering::SeqCst), 3, "held off: no new calls");
    }

    #[test]
    fn test_transient_holdoff_expires() {
        let mut memo = ResolveMemo::default();
        let t0 = Instant::now();
        memo.note_failure("/a".into(), t0);
        assert!(memo.recently_failed(Path::new("/a"), t0 + TRANSIENT_HOLDOFF / 2));
        assert!(!memo.recently_failed(Path::new("/a"), t0 + TRANSIENT_HOLDOFF));
    }

    #[tokio::test]
    async fn test_clear_under_failing_filesystem_uses_last_known_canonical() {
        let (_dir, root) = root();
        let link = uri(&root.join("link.rs"));
        let real = uri(&root.join("real.rs"));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut resolver =
            PublishedPathResolver::with_canonicalizer(failing_then_ok(&calls, usize::MAX));
        let stale = Instant::now().checked_sub(MEMO_TTL * 2).unwrap();
        resolver
            .memo
            .insert(root.join("link.rs"), root.join("real.rs"), stale);
        let clear = Publication {
            uri: &link,
            kind: PublicationKind::Clear,
        };

        let out = resolver.resolve_batch(&[clear], &roots_of(&root)).await;

        let published = out[0].as_ref().unwrap();
        assert_eq!(published.canonical(), &real);
        assert_eq!(published.source(), &link);
        assert!(!published.is_canonical());
    }

    fn failing_then_ok(calls: &Arc<AtomicUsize>, failures: usize) -> Arc<CanonicalizeFn> {
        counting(calls, move |_, n| {
            (n < failures).then_some(io::ErrorKind::TimedOut)
        })
    }

    #[tokio::test]
    async fn test_transient_error_is_retried_within_the_batch() {
        let (_dir, root) = root();
        let file = uri(&root.join("a.rs"));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut resolver = PublishedPathResolver::with_canonicalizer(failing_then_ok(&calls, 2));

        let out = resolver
            .resolve_batch(&[diagnostics(&file)], &roots_of(&root))
            .await;

        assert!(out[0].is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_persistent_transient_error_drops_diagnostics_but_keeps_clear() {
        let (_dir, root) = root();
        let file = uri(&root.join("a.rs"));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut resolver =
            PublishedPathResolver::with_canonicalizer(failing_then_ok(&calls, usize::MAX));
        let clear = Publication {
            uri: &file,
            kind: PublicationKind::Clear,
        };

        let out = resolver
            .resolve_batch(&[diagnostics(&file), clear], &roots_of(&root))
            .await;

        assert!(out[0].is_none());
        assert_eq!(out[1].as_ref().unwrap().canonical(), &file);
        assert!(!out[1].as_ref().unwrap().is_canonical());
    }

    #[tokio::test]
    async fn test_clear_then_new_publication_stay_ordered_across_transient_error() {
        let (_dir, root) = root();
        let a = uri(&root.join("a.rs"));
        let b = uri(&root.join("b.rs"));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut resolver =
            PublishedPathResolver::with_canonicalizer(failing_then_ok(&calls, usize::MAX));
        let batch = [
            Publication {
                uri: &a,
                kind: PublicationKind::Clear,
            },
            diagnostics(&b),
            diagnostics(&a),
        ];

        let out = resolver.resolve_batch(&batch, &roots_of(&root)).await;

        assert_eq!(out.len(), 3);
        assert_eq!(out[0].as_ref().unwrap().source(), &a);
        assert!(out[1].is_none());
        assert!(out[2].is_none());
    }

    #[tokio::test]
    async fn test_duplicate_uris_in_one_batch_canonicalize_once() {
        let (_dir, root) = root();
        let file = uri(&root.join("a.rs"));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut resolver = PublishedPathResolver::with_canonicalizer(counting(&calls, |_, _| None));

        let out = resolver
            .resolve_batch(&[diagnostics(&file), diagnostics(&file)], &roots_of(&root))
            .await;

        assert!(out.iter().all(Option::is_some));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_cold_burst_of_5000_uris_resolves_all_in_order() {
        let (_dir, root) = root();
        let uris: Vec<Uri> = (0..5000)
            .map(|i| uri(&root.join(format!("dir{}/f{i}.rs", i % 50))))
            .collect();
        let refs: Vec<Publication<'_>> = uris.iter().map(diagnostics).collect();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut resolver = PublishedPathResolver::with_canonicalizer(counting(&calls, |_, _| None));

        let out = resolver.resolve_batch(&refs, &roots_of(&root)).await;

        assert_eq!(out.len(), 5000);
        for (input, resolved) in uris.iter().zip(&out) {
            assert_eq!(resolved.as_ref().unwrap().canonical(), input);
        }
    }

    #[tokio::test]
    async fn test_slow_canonicalizer_loses_nothing_and_keeps_order() {
        let (_dir, root) = root();
        let uris: Vec<Uri> = (0..40)
            .map(|i| uri(&root.join(format!("f{i}.rs"))))
            .collect();
        let refs: Vec<Publication<'_>> = uris.iter().map(diagnostics).collect();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (live, high) = (Arc::clone(&in_flight), Arc::clone(&peak));
        let canon: Arc<CanonicalizeFn> = Arc::new(move |p: &Path| {
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            high.fetch_max(now, Ordering::SeqCst);
            // Earlier files are slower, so completion order is the reverse of input order.
            let n: u64 = p
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.trim_start_matches('f').parse().ok())
                .unwrap_or(0);
            std::thread::sleep(Duration::from_millis(40u64.saturating_sub(n)));
            live.fetch_sub(1, Ordering::SeqCst);
            Ok(p.to_path_buf())
        });
        let mut resolver = PublishedPathResolver::with_canonicalizer(canon);

        let out = resolver.resolve_batch(&refs, &roots_of(&root)).await;

        assert_eq!(out.len(), 40);
        for (input, resolved) in uris.iter().zip(&out) {
            assert_eq!(resolved.as_ref().unwrap().source(), input);
        }
        assert!(peak.load(Ordering::SeqCst) <= RESOLVE_CONCURRENCY);
        assert!(peak.load(Ordering::SeqCst) > 1);
    }

    #[tokio::test]
    async fn test_dropping_the_batch_abandons_a_hung_canonicalizer() {
        let (_dir, root) = root();
        let file = uri(&root.join("a.rs"));
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let canon: Arc<CanonicalizeFn> = Arc::new(move |p: &Path| {
            let _ = gate.lock().map(|rx| rx.recv());
            Ok(p.to_path_buf())
        });
        let mut resolver = PublishedPathResolver::with_canonicalizer(canon);
        let roots = roots_of(&root);

        let timed_out = tokio::time::timeout(
            Duration::from_millis(50),
            resolver.resolve_batch(&[diagnostics(&file)], &roots),
        )
        .await;

        assert!(timed_out.is_err());
        drop(release);
    }
}
