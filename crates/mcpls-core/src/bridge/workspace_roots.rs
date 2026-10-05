//! Workspace root set shared by every path-validation site.
//!
//! A [`WorkspaceRoots`] holds the canonical roots plus the lexical aliases a
//! client may legitimately name them by (configured form, logical `$PWD`).
//! Aliases are precomputed once so [`WorkspaceRoots::validate`] can reject an
//! out-of-workspace path without touching the filesystem.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf, Prefix, PrefixComponent};
use std::sync::Arc;

use lsp_types::Uri;
use thiserror::Error as ThisError;
use tracing::{debug, info, warn};

use super::{ClientPath, uri_to_path};
use crate::error::{BackgroundTask, Error};

/// How path components are compared during containment checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaseRule {
    Exact,
    FoldAscii,
}

impl CaseRule {
    /// Rule for lexical pre-checks: platforms with case-insensitive default
    /// filesystems may only be more permissive than the canonical check.
    const LEXICAL: Self = if cfg!(any(windows, target_os = "macos")) {
        Self::FoldAscii
    } else {
        Self::Exact
    };

    fn components_eq(self, a: Component<'_>, b: Component<'_>) -> bool {
        match (a, b) {
            (Component::Prefix(a), Component::Prefix(b)) => prefix_eq(a, b),
            _ => match self {
                Self::Exact => a == b,
                Self::FoldAscii => a
                    .as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy()),
            },
        }
    }
}

/// Drive letters compare equal across `C:` and `\\?\C:`, and network shares
/// across `\\server\share` and `\\?\UNC\server\share` (dunce keeps the
/// verbatim form for paths beyond `MAX_PATH`).
fn prefix_eq(a: PrefixComponent<'_>, b: PrefixComponent<'_>) -> bool {
    let same_name = |x: &std::ffi::OsStr, y: &std::ffi::OsStr| {
        x.to_string_lossy()
            .eq_ignore_ascii_case(&y.to_string_lossy())
    };
    match (a.kind(), b.kind()) {
        (Prefix::Disk(x) | Prefix::VerbatimDisk(x), Prefix::Disk(y) | Prefix::VerbatimDisk(y)) => {
            x.eq_ignore_ascii_case(&y)
        }
        (
            Prefix::UNC(server_a, share_a) | Prefix::VerbatimUNC(server_a, share_a),
            Prefix::UNC(server_b, share_b) | Prefix::VerbatimUNC(server_b, share_b),
        ) => same_name(server_a, server_b) && same_name(share_a, share_b),
        _ => a == b,
    }
}

fn is_within(path: &Path, root: &Path, rule: CaseRule) -> bool {
    let mut components = path.components();
    root.components()
        .all(|r| components.next().is_some_and(|c| rule.components_eq(c, r)))
}

/// Resolves `.` and `..` without touching the filesystem; `..` never rises
/// above the root.
pub fn lexically_normalize(path: &Path) -> PathBuf {
    let mut parts: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match parts.last() {
                Some(Component::Normal(_)) => {
                    parts.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => parts.push(component),
            },
            other => parts.push(other),
        }
    }
    parts.iter().collect()
}

/// Canonicalizes one existing path; injectable so tests can simulate a slow
/// or failing filesystem.
pub type CanonicalizeFn = dyn Fn(&Path) -> io::Result<PathBuf> + Send + Sync;

/// Why a path has no canonical form right now.
#[derive(Debug, ThisError)]
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

fn canonicalize_on_disk(path: &Path) -> io::Result<PathBuf> {
    dunce::canonicalize(path)
}

/// Canonicalizes the longest existing ancestor of `path` with `canonicalize`
/// and appends the remainder, so a not-yet-created (or since-deleted) file
/// still gets a canonical form.
///
/// The single canonicalization policy of the bridge. Falls back to the next
/// ancestor only on [`io::ErrorKind::NotFound`] and
/// [`io::ErrorKind::NotADirectory`]; every other error is
/// [`Unresolved::Transient`]. A path with a `..` component is
/// [`Unresolved::ParentComponent`]: a legitimate server publishes canonical
/// paths, and the lexical join of the missing tail would otherwise bypass
/// symlinks that `..` should have followed.
pub fn canonicalize_existing_prefix(
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

/// Classifies an I/O failure on a client path: a path that cannot name a file
/// is malformed, anything else is an I/O error.
fn path_io_error(path: &Path, source: io::Error) -> Error {
    match source.kind() {
        io::ErrorKind::NotADirectory
        | io::ErrorKind::InvalidFilename
        | io::ErrorKind::InvalidInput => Error::MalformedPath {
            path: path.to_path_buf(),
            source,
        },
        _ => Error::FileIo {
            path: path.to_path_buf(),
            source,
        },
    }
}

/// The process working directory in its physical form plus the logical
/// (`$PWD`) spelling when that names the same directory.
///
/// Injected into [`WorkspaceRoots::from_configured_with`] so tests can drive
/// both spellings without changing the real environment.
#[derive(Debug, Clone)]
pub struct ProcessCwd {
    physical: PathBuf,
    logical: Option<PathBuf>,
}

impl ProcessCwd {
    /// Reads the current directory and `$PWD` from the process environment.
    pub(crate) fn current() -> Result<Self, Error> {
        let physical = std::env::current_dir().map_err(Error::Io)?;
        Ok(Self::new(physical, std::env::var_os("PWD")))
    }

    /// Pairs `physical` with `pwd`, which is accepted only when it is
    /// absolute and names the same directory; a forged or stale value is
    /// ignored.
    pub(crate) fn new(physical: PathBuf, pwd: Option<OsString>) -> Self {
        let logical = pwd.map(PathBuf::from).filter(|pwd| {
            pwd.is_absolute()
                && matches!(
                    (dunce::canonicalize(pwd), dunce::canonicalize(&physical)),
                    (Ok(a), Ok(b)) if a == b
                )
        });
        Self { physical, logical }
    }
}

/// Canonicalizes `probe` (resolved through the process working directory when
/// relative) and returns the canonical path, reporting a failure against the
/// root entry as `written` and the `base_dir` it was resolved relative to.
///
/// # Errors
///
/// Returns [`Error::InvalidConfig`] when `probe` cannot be canonicalized.
pub fn probe_root(written: &Path, base_dir: &Path, probe: &Path) -> Result<PathBuf, Error> {
    dunce::canonicalize(probe).map_err(|source| {
        Error::InvalidConfig(format!(
            "workspace root '{}' resolved relative to '{}' as '{}' could not be canonicalized: {source}",
            written.display(),
            base_dir.display(),
            probe.display()
        ))
    })
}

/// Join a relative `root` onto `base_dir`, correctly handling a root that
/// [`Path::is_relative`] classifies `true` yet still carries a leading
/// [`Component::Prefix`] and/or [`Component::RootDir`] -- on Windows,
/// `is_absolute()` requires *both* a prefix and a root, so two distinct
/// shapes are `is_relative() == true` despite being (partially) rooted:
/// - no prefix, has root (e.g. `\workspace`) -- rooted on whichever drive is
///   current.
/// - has prefix, no root (e.g. `C:workspace`) -- drive-relative, resolved
///   against that drive's own current directory.
///
/// Plain `base_dir.join(root)` would hit [`PathBuf::push`]'s documented
/// special cases for both shapes, each discarding some or all of `base_dir`.
/// Skipping any leading `Prefix`/`RootDir` components before joining
/// sidesteps both: only the ordinary relative tail is ever appended to
/// `base_dir`. Detected via `Component` iteration (not `#[cfg(windows)]`),
/// so the logic is exercised by a unit test on any host -- see `#348`.
pub fn join_relative_root(base_dir: &Path, root: &Path) -> PathBuf {
    let mut joined = base_dir.to_path_buf();
    joined.extend(
        root.components()
            .skip_while(|c| matches!(c, Component::Prefix(_) | Component::RootDir)),
    );
    joined
}

/// The `file:` path of `uri` when it is absolute and free of `.`/`..`
/// components (a prefix match would otherwise let `/ws/../etc` pass for `/ws`).
fn plain_absolute_path(uri: &Uri) -> Option<PathBuf> {
    let path = uri_to_path(uri)?;
    let plain = path.is_absolute()
        && !path
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir));
    plain.then_some(path)
}

/// The simplified, lexically normalized form an alias is stored (and
/// verified) in; `None` for a relative or empty alias.
fn stored_alias_form(alias: &Path) -> Option<PathBuf> {
    if !alias.is_absolute() {
        return None;
    }
    let form = lexically_normalize(dunce::simplified(alias));
    (!form.is_empty()).then_some(form)
}

/// One configured root: its canonical form plus the pre-canonical spellings
/// a client may name it by.
struct ResolvedRoot {
    canonical: PathBuf,
    aliases: Vec<PathBuf>,
}

impl ResolvedRoot {
    fn from_absolute(root: &Path) -> Self {
        let canonical = dunce::canonicalize(root).unwrap_or_else(|source| {
            warn!(
                "Failed to canonicalize absolute workspace root {}: {source}, using non-canonical path",
                root.display()
            );
            canonicalize_existing_prefix(root, &canonicalize_on_disk)
                .unwrap_or_else(|_| root.to_path_buf())
        });
        Self {
            canonical,
            aliases: vec![root.to_path_buf()],
        }
    }

    fn from_relative(root: &Path, cwd: &ProcessCwd) -> Result<Self, Error> {
        let joined = join_relative_root(&cwd.physical, root);
        let canonical = probe_root(root, &cwd.physical, &joined)?;
        let mut aliases = vec![joined];
        aliases.extend(
            cwd.logical
                .as_deref()
                .map(|logical| join_relative_root(logical, root)),
        );
        Ok(Self { canonical, aliases })
    }

    fn from_cwd(cwd: &ProcessCwd) -> Self {
        let canonical = dunce::canonicalize(&cwd.physical).unwrap_or_else(|e| {
            warn!(
                "Failed to canonicalize workspace base directory {}: {e}, using non-canonical absolute path",
                cwd.physical.display()
            );
            cwd.physical.clone()
        });
        info!(
            "Using workspace base directory as root: {}",
            canonical.display()
        );
        let mut aliases = vec![cwd.physical.clone()];
        aliases.extend(cwd.logical.clone());
        Self { canonical, aliases }
    }

    /// Aliases whose stored form still resolves to this root's canonical
    /// path. An alias that climbs out through a symlink (`link/..`) would
    /// otherwise admit an unrelated tree and let `FileIo` errors probe it.
    fn verified_aliases(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.aliases.iter().filter_map(|alias| {
            let form = stored_alias_form(alias)?;
            if form == self.canonical
                || canonicalize_existing_prefix(&form, &canonicalize_on_disk)
                    .is_ok_and(|real| real == self.canonical)
            {
                Some(form)
            } else {
                debug!(
                    "dropping workspace root alias {} that does not resolve to {}",
                    form.display(),
                    self.canonical.display()
                );
                None
            }
        })
    }
}

/// A symlink found directly under the filesystem root, with its target
/// already resolved lexically against that directory.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct RootLink {
    link: PathBuf,
    target: PathBuf,
}

/// Lists the symlinks directly under `links_dir` without following them, so
/// automount or dead network links are never touched. Any I/O error yields
/// an empty list.
#[cfg(unix)]
fn read_root_links(links_dir: &Path) -> Vec<RootLink> {
    let entries = match std::fs::read_dir(links_dir) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::debug!(dir = %links_dir.display(), %err, "cannot list root-level links");
            return Vec::new();
        }
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_symlink()))
        .filter_map(|entry| {
            let link = entry.path();
            let target = std::fs::read_link(&link).ok()?;
            Some(RootLink {
                target: lexically_normalize(&links_dir.join(target)),
                link,
            })
        })
        .collect()
}

/// Candidate aliases of `roots` that differ only by a root-level symlink whose
/// target is a component prefix of the root. Unverified: callers must check
/// `canonicalize(alias) == root`.
#[cfg(unix)]
fn system_symlink_aliases(roots: &[PathBuf], links: &[RootLink]) -> Vec<(PathBuf, PathBuf)> {
    let mut candidates = Vec::new();
    for root in roots {
        for RootLink { link, target } in links {
            if let Ok(rest) = root.strip_prefix(target) {
                candidates.push((link.join(rest), root.clone()));
            }
        }
    }
    candidates
}

/// Aliases of `roots` through symlinks directly under `links_dir`, each
/// verified to canonicalize back to its own root.
#[cfg(unix)]
fn verified_system_aliases(links_dir: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    system_symlink_aliases(roots, &read_root_links(links_dir))
        .into_iter()
        .filter(|(alias, root)| dunce::canonicalize(alias).is_ok_and(|real| &real == root))
        .map(|(alias, _)| alias)
        .collect()
}

/// A client path that passed workspace validation: lexically admitted and
/// canonicalized under a canonical root.
///
/// The field is private and the only constructors are
/// [`WorkspaceRoots::validate`] and [`WorkspaceRoots::validate_blocking`], so
/// a function taking a `WorkspacePath` knows the path was checked once and
/// need not canonicalize it again.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::{ClientPath, WorkspaceRoots};
///
/// let dir = std::env::temp_dir();
/// let roots = WorkspaceRoots::from_configured(std::slice::from_ref(&dir))?;
/// let path = roots.validate_blocking(&ClientPath::try_from(dir.clone())?)?;
/// assert!(path.as_path().is_absolute());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspacePath(PathBuf);

impl WorkspacePath {
    /// The canonical path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// The canonical path, by value.
    #[must_use]
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for WorkspacePath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// The canonical workspace roots plus the lexical aliases they may be named by.
///
/// A path is admitted by [`WorkspaceRoots::validate`] only if it passes the
/// lexical pre-check against the canonical roots and their aliases (configured
/// form, logical `$PWD`) *and* its physical, canonical form lies under a
/// canonical root; the aliases only widen the pre-check.
///
/// Cheap to clone (both lists are shared). Fields are private so every
/// instance upholds the invariants: aliases are absolute, simplified,
/// lexically normalized and distinct from the canonical roots.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceRoots {
    canonical: Arc<[PathBuf]>,
    aliases: Arc<[PathBuf]>,
}

impl WorkspaceRoots {
    /// Builds a root set from already-canonical roots and extra lexical aliases.
    ///
    /// Performs no filesystem access and trusts the caller that `canonical`
    /// is canonical. Aliases that are relative, empty after normalization,
    /// duplicated, or equal to a canonical root are dropped.
    fn from_parts(canonical: Vec<PathBuf>, aliases: Vec<PathBuf>) -> Self {
        let mut kept: Vec<PathBuf> = Vec::new();
        for alias in aliases {
            let Some(alias) = stored_alias_form(&alias) else {
                continue;
            };
            if canonical.contains(&alias) || kept.contains(&alias) {
                continue;
            }
            kept.push(alias);
        }
        Self {
            canonical: canonical.into(),
            aliases: kept.into(),
        }
    }

    /// Builds a root set for tests that exercise purely lexical behavior;
    /// the caller vouches for `canonical` and nothing touches the filesystem.
    #[cfg(test)]
    pub(crate) fn for_test(canonical: Vec<PathBuf>, aliases: Vec<PathBuf>) -> Self {
        Self::from_parts(canonical, aliases)
    }

    /// Builds the root set for the `workspace.roots` of a configuration.
    ///
    /// Relative roots are resolved against the process working directory and
    /// must exist; an empty list means the working directory itself. Absolute
    /// roots that cannot be canonicalized (for example a directory created
    /// after startup) fall back to their longest existing prefix. The
    /// configured spelling, the working-directory spelling and the logical
    /// `$PWD` spelling are kept as aliases, but only when they verifiably
    /// resolve to the same root, so a client naming a root the way it was
    /// configured (for example a `/var` temp dir that canonicalizes to
    /// `/private/var`) is still admitted.
    ///
    /// The working directory is read only when a root needs it, so a
    /// fully-absolute list does not fail startup over an unreadable cwd.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] for a relative root that cannot be
    /// canonicalized and [`Error::Io`] when the working directory is needed
    /// but unreadable.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::bridge::WorkspaceRoots;
    ///
    /// let roots = WorkspaceRoots::from_configured(&[std::env::temp_dir()])?;
    /// assert!(!roots.is_empty());
    /// # Ok::<(), mcpls_core::Error>(())
    /// ```
    pub fn from_configured(roots: &[PathBuf]) -> Result<Self, Error> {
        Self::from_configured_with(roots, ProcessCwd::current)
    }

    pub(crate) fn from_configured_with(
        roots: &[PathBuf],
        cwd: impl FnOnce() -> Result<ProcessCwd, Error>,
    ) -> Result<Self, Error> {
        let needs_cwd = roots.is_empty() || roots.iter().any(|root| root.is_relative());
        let cwd = needs_cwd.then(cwd).transpose()?;

        let resolved = match &cwd {
            Some(cwd) if roots.is_empty() => vec![ResolvedRoot::from_cwd(cwd)],
            _ => roots
                .iter()
                .map(|root| match &cwd {
                    Some(cwd) if root.is_relative() => ResolvedRoot::from_relative(root, cwd),
                    _ => Ok(ResolvedRoot::from_absolute(root)),
                })
                .collect::<Result<Vec<_>, Error>>()?,
        };

        let aliases = resolved
            .iter()
            .flat_map(ResolvedRoot::verified_aliases)
            .collect();
        let canonical = resolved.into_iter().map(|root| root.canonical).collect();
        Ok(Self::from_parts(canonical, aliases).with_system_aliases())
    }

    /// Adds aliases that differ from a canonical root only through a
    /// root-level system symlink (`/tmp`, `/var` on macOS), discovered once
    /// from `/`. Each alias is verified to canonicalize to its own root, so
    /// the admitted file set never widens. No-op on non-Unix platforms.
    #[cfg(unix)]
    #[must_use]
    fn with_system_aliases(self) -> Self {
        self.with_system_aliases_in(Path::new("/"))
    }

    /// Non-Unix platforms have no root-level system symlinks to admit.
    #[cfg(not(unix))]
    #[must_use]
    const fn with_system_aliases(self) -> Self {
        self
    }

    #[cfg(unix)]
    fn with_system_aliases_in(self, links_dir: &Path) -> Self {
        let extra = verified_system_aliases(links_dir, &self.canonical);
        let aliases = self.aliases.iter().cloned().chain(extra).collect();
        Self::from_parts(self.canonical.to_vec(), aliases)
    }

    /// Validates a client path against the roots, canonicalizing it on the
    /// blocking pool so a slow filesystem cannot stall a runtime worker.
    ///
    /// See [`Self::validate_blocking`] for the checks and errors.
    ///
    /// # Errors
    ///
    /// As [`Self::validate_blocking`], plus [`Error::TaskFailed`] when the
    /// blocking task panics.
    pub async fn validate(&self, path: &ClientPath) -> Result<WorkspacePath, Error> {
        self.lexical_gate(path)?;
        let roots = self.clone();
        let path = path.clone();
        tokio::task::spawn_blocking(move || roots.canonical_gate(&path))
            .await
            .map_err(|source| Error::TaskFailed {
                task: BackgroundTask::PathValidation,
                source,
            })?
    }

    /// Validates a client path against the roots on the calling thread.
    ///
    /// The lexical check (absolute, `.`/`..` resolved, against the canonical
    /// roots and their aliases) is only a pre-filter: it rejects an
    /// out-of-workspace path without touching the filesystem and can only
    /// reject, never accept. The decision that counts uses the physical
    /// path: the original path is canonicalized (so `..` after a symlink is
    /// resolved against the real directory) and that canonical form must lie
    /// under a canonical root. The returned path is the canonical one.
    ///
    /// Touches the filesystem; async callers use [`Self::validate`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoWorkspaceRoots`] if no root is configured -- fails
    /// closed rather than allowing unrestricted access -- and
    /// [`Error::PathOutsideWorkspace`] if the path is outside every root.
    /// When the lexical check admitted the path but it cannot be
    /// canonicalized, [`Error::FileIo`] is returned (for example it does not
    /// exist), or [`Error::MalformedPath`] when the path itself is malformed
    /// (it runs through a regular file, or has an invalid or over-long name).
    pub fn validate_blocking(&self, path: &ClientPath) -> Result<WorkspacePath, Error> {
        self.lexical_gate(path)?;
        self.canonical_gate(path)
    }

    fn lexical_gate(&self, path: &ClientPath) -> Result<(), Error> {
        let path = path.as_path();
        if self.is_empty() {
            return Err(Error::NoWorkspaceRoots(path.to_path_buf()));
        }
        let absolute = std::path::absolute(path).map_err(|source| path_io_error(path, source))?;
        let normalized = lexically_normalize(dunce::simplified(&absolute));
        if self.admits_lexically(&normalized) {
            Ok(())
        } else {
            Err(Error::PathOutsideWorkspace(path.to_path_buf()))
        }
    }

    fn canonical_gate(&self, path: &ClientPath) -> Result<WorkspacePath, Error> {
        let path = path.as_path();
        let canonical = dunce::canonicalize(path).map_err(|source| path_io_error(path, source))?;
        if self.contains_canonical(&canonical) {
            Ok(WorkspacePath(canonical))
        } else {
            Err(Error::PathOutsideWorkspace(path.to_path_buf()))
        }
    }

    /// The canonical roots, in configuration order.
    #[must_use]
    pub fn canonical(&self) -> &[PathBuf] {
        &self.canonical
    }

    /// Whether no root is configured (path validation then fails closed).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.canonical.is_empty()
    }

    /// Whether `uri` names an absolute, `.`/`..`-free `file:` path lying under
    /// a canonical root or one of its aliases, compared with the platform's
    /// lenient case rule.
    ///
    /// A lexical **pre-filter**, never a trust boundary: it neither touches
    /// the filesystem nor resolves symlinks, and on case-insensitive
    /// platforms it folds case. It suits callers that either follow it with a
    /// canonical check (the diagnostics pump, via
    /// `PublishedPathResolver::resolve_batch`) or only annotate (the advisory
    /// `out_of_workspace` flag). Anything that writes through a
    /// server-supplied URI must use [`Self::admits_edit_uri`] instead.
    /// Read-only navigation results are deliberately not filtered (standard
    /// library and dependency locations are legitimately outside); opening
    /// such a path still goes through the inbound [`Self::validate`] gate. Empty roots admit nothing.
    pub(crate) fn admits_uri(&self, uri: &Uri) -> bool {
        plain_absolute_path(uri).is_some_and(|path| self.admits_lexically(&path))
    }

    /// Whether `uri` may be written through on behalf of a server.
    ///
    /// Stricter than [`Self::admits_uri`]: the path must lie under a canonical
    /// root or alias by exact component comparison (no case folding), *and*
    /// its canonical form (longest existing prefix resolved, so a symlink
    /// pointing out of the workspace or a retargeted alias is seen through)
    /// must lie under a canonical root. Aliases are only a spelling aid here.
    /// Canonicalization runs on the blocking pool.
    pub(crate) async fn admits_edit_uri(&self, uri: &Uri) -> bool {
        let Some(path) = plain_absolute_path(uri) else {
            return false;
        };
        if !self.admits_with(&path, CaseRule::Exact) {
            return false;
        }
        match tokio::task::spawn_blocking(move || {
            canonicalize_existing_prefix(&path, &canonicalize_on_disk)
        })
        .await
        {
            Ok(canonical) => canonical.is_ok_and(|canonical| self.contains_canonical(&canonical)),
            Err(error) => {
                tracing::warn!(%error, uri = uri.as_ref(), "edit URI canonicalization task failed; dropping the edit");
                false
            }
        }
    }

    /// Whether the absolute, lexically normalized `path` lies under a
    /// canonical root or one of its aliases (lenient case rule). Never touches
    /// the filesystem.
    pub(crate) fn admits_lexically(&self, normalized: &Path) -> bool {
        self.admits_with(normalized, CaseRule::LEXICAL)
    }

    fn admits_with(&self, path: &Path, rule: CaseRule) -> bool {
        self.canonical
            .iter()
            .chain(self.aliases.iter())
            .any(|root| is_within(path, root, rule))
    }

    /// Whether the canonical `path` lies under a canonical root.
    pub(crate) fn contains_canonical(&self, path: &Path) -> bool {
        self.canonical
            .iter()
            .any(|root| is_within(path, root, CaseRule::Exact))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::assert_matches;

    use super::*;

    /// An absolute path for the host platform (`/p` on Unix, `C:\p` on Windows).
    fn abs(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{}", path.replace('/', "\\")))
        } else {
            PathBuf::from(path)
        }
    }

    fn file_uri(path: &str) -> Uri {
        let prefix = if cfg!(windows) {
            "file:///C:"
        } else {
            "file://"
        };
        Uri::from(format!("{prefix}{path}").as_str())
    }

    fn resolve_in(roots: &[PathBuf], base: &Path) -> Result<WorkspaceRoots, Error> {
        WorkspaceRoots::from_configured_with(roots, || {
            Ok(ProcessCwd::new(base.to_path_buf(), None))
        })
    }

    #[cfg(unix)]
    fn resolve_in_with_pwd(
        roots: &[PathBuf],
        base: &Path,
        pwd: &Path,
    ) -> Result<WorkspaceRoots, Error> {
        WorkspaceRoots::from_configured_with(roots, || {
            Ok(ProcessCwd::new(
                base.to_path_buf(),
                Some(pwd.as_os_str().to_owned()),
            ))
        })
    }

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(dir.path()).unwrap();
        (dir, base)
    }

    #[test]
    fn test_lexically_normalize_resolves_dots() {
        assert_eq!(
            lexically_normalize(Path::new("/ws/./a/../b")),
            PathBuf::from("/ws/b")
        );
    }

    #[test]
    fn test_lexically_normalize_never_rises_above_root() {
        assert_eq!(
            lexically_normalize(Path::new("/ws/../../etc")),
            PathBuf::from("/etc")
        );
    }

    #[test]
    fn test_from_parts_drops_relative_duplicate_and_canonical_aliases() {
        let roots = WorkspaceRoots::for_test(
            vec![abs("/real")],
            vec![
                PathBuf::from("relative"),
                abs("/real"),
                abs("/alias"),
                abs("/alias/./"),
            ],
        );
        assert_eq!(roots.aliases.len(), 1);
    }

    #[test]
    fn test_admits_lexically_alias_and_canonical() {
        let roots = WorkspaceRoots::for_test(vec![abs("/real/ws")], vec![abs("/alias/ws")]);
        assert!(roots.admits_lexically(&abs("/real/ws/a.rs")));
        assert!(roots.admits_lexically(&abs("/alias/ws/a.rs")));
        assert!(!roots.admits_lexically(&abs("/other/a.rs")));
        assert!(!roots.admits_lexically(&abs("/real/wsx/a.rs")));
    }

    #[test]
    fn test_empty_roots_admit_nothing() {
        let roots = WorkspaceRoots::default();
        assert!(roots.is_empty());
        assert!(!roots.admits_lexically(Path::new("/a")));
        assert!(!roots.admits_uri(&file_uri("/anywhere/at/all.rs")));
    }

    #[test]
    fn test_admits_uri_under_root_and_alias() {
        let roots = WorkspaceRoots::for_test(vec![abs("/real/ws")], vec![abs("/alias/ws")]);
        assert!(roots.admits_uri(&file_uri("/real/ws/src/main.rs")));
        assert!(roots.admits_uri(&file_uri("/alias/ws/src/main.rs")));
    }

    #[test]
    fn test_admits_uri_rejects_outside_sibling_and_non_file() {
        let roots = WorkspaceRoots::for_test(vec![abs("/ws")], vec![]);
        assert!(!roots.admits_uri(&file_uri("/etc/passwd")));
        assert!(!roots.admits_uri(&file_uri("/ws2/a.rs")));
        assert!(!roots.admits_uri(&Uri::from("untitled:Untitled-1")));
    }

    /// `Path::starts_with` does not resolve `.`/`..`: without the explicit
    /// check `file:///ws/../etc/passwd` would pass for `/ws`.
    #[test]
    fn test_admits_uri_rejects_dot_components() {
        let roots = WorkspaceRoots::for_test(vec![abs("/ws")], vec![]);
        assert!(!roots.admits_uri(&file_uri("/ws/../../etc/passwd")));
    }

    #[test]
    fn test_canonicalize_existing_prefix_handles_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dunce::canonicalize(dir.path()).unwrap();
        let result =
            canonicalize_existing_prefix(&dir.path().join("gone/x.rs"), &canonicalize_on_disk)
                .unwrap();
        assert_eq!(result, canonical.join("gone/x.rs"));
    }

    #[test]
    fn test_contains_canonical_is_case_exact() {
        let roots = WorkspaceRoots::for_test(vec![PathBuf::from("/Real")], vec![]);
        assert!(roots.contains_canonical(Path::new("/Real/a")));
        assert!(!roots.contains_canonical(Path::new("/real/a")));
    }

    #[cfg(windows)]
    #[test]
    fn test_verbatim_unc_equals_unc() {
        let roots = WorkspaceRoots::for_test(vec![PathBuf::from(r"\\server\share\ws")], vec![]);
        assert!(roots.contains_canonical(Path::new(r"\\?\UNC\server\share\ws\a.rs")));
        assert!(!roots.contains_canonical(Path::new(r"\\?\UNC\server\other\ws\a.rs")));
    }

    #[cfg(windows)]
    #[test]
    fn test_verbatim_disk_equals_disk() {
        let roots = WorkspaceRoots::for_test(vec![PathBuf::from(r"C:\ws")], vec![]);
        assert!(roots.contains_canonical(Path::new(r"\\?\C:\ws\a.rs")));
    }

    #[cfg(windows)]
    #[test]
    fn test_admits_uri_accepts_verbatim_prefix_root() {
        let roots = WorkspaceRoots::for_test(vec![PathBuf::from(r"\\?\C:\ws")], vec![]);
        assert!(roots.admits_uri(&Uri::from("file:///C:/ws/a.rs")));
    }

    #[test]
    fn test_from_configured_absolute_missing_root_keeps_alias() {
        let (_dir, base) = canonical_tempdir();
        let missing = base.join("missing");
        let roots = resolve_in(std::slice::from_ref(&missing), &base).unwrap();
        assert_eq!(roots.canonical(), std::slice::from_ref(&missing));
        assert!(roots.admits_lexically(&missing.join("a.rs")));
    }

    #[test]
    fn test_from_configured_rejects_nonexistent_relative_path() {
        let (_dir, base) = canonical_tempdir();

        let err = resolve_in(&[PathBuf::from("missing")], &base).unwrap_err();

        let Error::InvalidConfig(message) = err else {
            panic!("expected InvalidConfig, got {err:?}");
        };
        assert!(message.contains("workspace root 'missing'"));
        assert!(message.contains(&base.display().to_string()));
    }

    #[test]
    fn test_from_configured_surfaces_unreadable_cwd_only_when_needed() {
        let (_dir, base) = canonical_tempdir();
        let broken = || Err(Error::Io(std::io::Error::other("cwd gone")));

        assert_matches!(
            WorkspaceRoots::from_configured_with(&[], broken),
            Err(Error::Io(_))
        );
        assert_matches!(
            WorkspaceRoots::from_configured_with(&[PathBuf::from("rel")], broken),
            Err(Error::Io(_))
        );
        assert!(WorkspaceRoots::from_configured_with(&[base], broken).is_ok());
    }

    /// #234 round-3 regression: a symlinked workspace root must canonicalize
    /// to its real path, matching what LSP servers report in diagnostics,
    /// while its configured spelling stays admitted as an alias.
    #[cfg(unix)]
    #[test]
    fn test_from_configured_symlinked_root_keeps_alias() {
        let (_dir, base) = canonical_tempdir();
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let roots = resolve_in(std::slice::from_ref(&link), &base).unwrap();

        assert_eq!(roots.canonical(), std::slice::from_ref(&real));
        assert!(roots.admits_lexically(&link.join("a.rs")));
        assert!(roots.admits_lexically(&real.join("a.rs")));
        assert!(!roots.admits_lexically(&base.join("other/a.rs")));
    }

    #[test]
    fn test_from_configured_empty_returns_cwd() {
        let (_dir, base) = canonical_tempdir();
        let roots = resolve_in(&[], &base).unwrap();
        assert_eq!(roots.canonical(), [base]);
    }

    #[test]
    fn test_from_configured_preserves_order_and_resolves_relative() {
        let (_dir, base) = canonical_tempdir();
        let absolute = base.join("absolute");
        std::fs::create_dir(&absolute).unwrap();
        std::fs::create_dir_all(base.join("relative/path")).unwrap();

        let roots = resolve_in(&[absolute.clone(), PathBuf::from("relative/path")], &base).unwrap();

        assert_eq!(roots.canonical(), [absolute, base.join("relative/path")]);
    }

    #[test]
    fn test_from_configured_dot_and_parent() {
        let (_dir, parent) = canonical_tempdir();
        let nested = parent.join("nested");
        std::fs::create_dir(&nested).unwrap();

        let dot = resolve_in(&[PathBuf::from(".")], &nested).unwrap();
        let up = resolve_in(&[PathBuf::from("..")], &nested).unwrap();

        assert_eq!(dot.canonical(), [nested]);
        assert_eq!(up.canonical(), [parent]);
    }

    #[test]
    fn test_from_configured_unicode_and_spaces() {
        let (_dir, base) = canonical_tempdir();
        let config_roots = [
            PathBuf::from("workspace/テスト"),
            PathBuf::from("workspace/тест"),
            PathBuf::from("another path/workspace"),
        ];
        for root in &config_roots {
            std::fs::create_dir_all(base.join(root)).unwrap();
        }

        let roots = resolve_in(&config_roots, &base).unwrap();

        let expected: Vec<PathBuf> = config_roots.iter().map(|r| base.join(r)).collect();
        assert_eq!(roots.canonical(), expected);
    }

    /// #348 case 1: a fully-absolute list never reads the working directory.
    #[test]
    fn test_from_configured_ignores_cwd_when_all_absolute() {
        let (_dir, base) = canonical_tempdir();
        let root = base.join("root");
        std::fs::create_dir(&root).unwrap();

        let roots = WorkspaceRoots::from_configured_with(std::slice::from_ref(&root), || {
            panic!("cwd must not be read")
        })
        .unwrap();

        assert_eq!(roots.canonical(), [root]);
    }

    /// #348 case 3 (S2): platform-independent test of the `Component`
    /// stripping; the rooted-without-prefix shape is only `is_relative()` on
    /// Windows, but the helper can be driven directly on any host.
    #[test]
    fn test_join_relative_root_strips_leading_root_and_prefix_components() {
        let base = Path::new("/base/dir");

        assert_eq!(
            join_relative_root(base, Path::new("/workspace")),
            PathBuf::from("/base/dir/workspace")
        );
        assert_eq!(
            join_relative_root(base, Path::new("/workspace/sub")),
            PathBuf::from("/base/dir/workspace/sub")
        );
        assert_eq!(
            join_relative_root(base, Path::new("workspace")),
            PathBuf::from("/base/dir/workspace")
        );
        assert_eq!(
            join_relative_root(base, Path::new("..")),
            PathBuf::from("/base/dir/..")
        );
    }

    /// #348 case 3: `\workspace` is relative on Windows despite being rooted.
    #[cfg(windows)]
    #[test]
    fn test_from_configured_windows_root_without_prefix() {
        let (_dir, base) = canonical_tempdir();
        let nested = base.join("workspace");
        std::fs::create_dir(&nested).unwrap();

        let root = PathBuf::from(r"\workspace");
        assert!(root.is_relative());

        let roots = resolve_in(std::slice::from_ref(&root), &base).unwrap();
        assert_eq!(roots.canonical(), [nested]);
    }

    /// #348 M2: a drive-relative root (`C:workspace`) is joined under the
    /// base like every other relative root.
    #[cfg(windows)]
    #[test]
    fn test_from_configured_windows_drive_relative_root() {
        let (_dir, base) = canonical_tempdir();
        let nested = base.join("workspace");
        std::fs::create_dir(&nested).unwrap();

        let Some(drive_prefix) = base.components().find_map(|c| match c {
            Component::Prefix(p) => Some(p.as_os_str().to_owned()),
            _ => None,
        }) else {
            panic!("temp dir path should have a Windows drive prefix");
        };
        let mut root = drive_prefix;
        root.push("workspace");
        let root = PathBuf::from(root);
        assert!(root.is_relative());

        let roots = resolve_in(std::slice::from_ref(&root), &base).unwrap();
        assert_eq!(roots.canonical(), [nested]);
    }

    #[cfg(unix)]
    #[test]
    fn test_process_cwd_accepts_pwd_only_when_it_names_the_cwd() {
        let (_dir, base) = canonical_tempdir();
        let link = base.join("link");
        std::os::unix::fs::symlink(&base, &link).unwrap();
        let other = tempfile::TempDir::new().unwrap();
        let logical = |pwd: Option<&std::ffi::OsStr>| {
            ProcessCwd::new(base.clone(), pwd.map(ToOwned::to_owned)).logical
        };

        assert_eq!(logical(Some(link.as_os_str())), Some(link));
        assert_eq!(logical(Some(other.path().as_os_str())), None);
        assert_eq!(logical(Some(std::ffi::OsStr::new("."))), None);
        assert_eq!(logical(None), None);
    }

    #[cfg(unix)]
    #[test]
    fn test_from_configured_relative_root_admits_logical_cwd_spelling() {
        let (_dir, base) = canonical_tempdir();
        let real = base.join("real");
        std::fs::create_dir_all(real.join("rel")).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let roots = resolve_in_with_pwd(&[PathBuf::from("rel")], &real, &link).unwrap();
        assert!(roots.admits_lexically(&link.join("rel/f.rs")));

        let forged = resolve_in_with_pwd(&[PathBuf::from("rel")], &real, &base).unwrap();
        assert!(!forged.admits_lexically(&link.join("rel/f.rs")));
    }

    #[cfg(unix)]
    #[test]
    fn test_from_configured_empty_roots_admit_logical_cwd_spelling() {
        let (_dir, base) = canonical_tempdir();
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let roots = resolve_in_with_pwd(&[], &real, &link).unwrap();

        assert_eq!(roots.canonical(), [real]);
        assert!(roots.admits_lexically(&link.join("f.rs")));
    }

    /// S2: `link/..` climbs out of the root through a symlink; the alias
    /// would admit an unrelated tree, so it must not be recorded.
    #[cfg(unix)]
    #[test]
    fn test_from_configured_drops_alias_that_climbs_out_through_symlink() {
        let (_dir, base) = canonical_tempdir();
        let data = base.join("data/a/b");
        std::fs::create_dir_all(&data).unwrap();
        let home = base.join("home");
        std::fs::create_dir(&home).unwrap();
        let link = home.join("proj_link");
        std::os::unix::fs::symlink(&data, &link).unwrap();

        let roots = resolve_in(&[link.join("..")], &base).unwrap();

        assert_eq!(roots.canonical(), [base.join("data/a")]);
        assert!(roots.aliases.iter().all(|alias| !alias.starts_with(&home)));
        assert!(!roots.admits_lexically(&home.join("anything")));
    }

    #[cfg(unix)]
    #[test]
    fn test_from_configured_keeps_alias_under_symlinked_ancestor() {
        let (_dir, base) = canonical_tempdir();
        let real = base.join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let configured = link.join("sub");

        let roots = resolve_in(std::slice::from_ref(&configured), &base).unwrap();

        assert_eq!(roots.canonical(), [real.join("sub")]);
        assert!(roots.admits_lexically(&configured.join("a.rs")));
    }

    #[cfg(not(unix))]
    #[test]
    fn test_from_configured_keeps_absolute_root_as_alias() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().to_path_buf();

        let roots = WorkspaceRoots::from_configured(std::slice::from_ref(&raw)).unwrap();

        assert!(roots.admits_lexically(&raw.join("a.rs")));
    }

    #[cfg(unix)]
    fn fake_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(dir.path()).unwrap();
        (dir, base)
    }

    #[cfg(unix)]
    #[test]
    fn test_system_aliases_symlink_table() {
        let (_guard, base) = fake_root();
        let real = base.join("private/tmp/proj");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(base.join("private/tmp"), base.join("tmp")).unwrap();
        std::os::unix::fs::symlink(base.join("private/var"), base.join("var")).unwrap();
        std::os::unix::fs::symlink("private/tmp", base.join("rel")).unwrap();

        let roots =
            WorkspaceRoots::for_test(vec![real.clone()], vec![]).with_system_aliases_in(&base);

        assert!(roots.admits_lexically(&base.join("tmp/proj/a.rs")));
        assert!(roots.admits_lexically(&base.join("rel/proj/a.rs")));
        assert!(!roots.admits_lexically(&base.join("var/proj/a.rs")));
        assert!(!roots.admits_lexically(&base.join("tmp/other/a.rs")));
        assert!(roots.contains_canonical(&real.join("a.rs")));
    }

    #[cfg(unix)]
    #[test]
    fn test_system_aliases_reject_forged_alias() {
        let (_guard, base) = fake_root();
        let real = base.join("p/b/proj");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(base.join("elsewhere/deep")).unwrap();
        std::fs::create_dir_all(base.join("elsewhere/b/proj")).unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere/deep"), base.join("p/a")).unwrap();
        // Lexically `p/a/../b` is `p/b`, physically it is `elsewhere/b`.
        std::os::unix::fs::symlink("p/a/../b", base.join("x")).unwrap();

        let roots = WorkspaceRoots::for_test(vec![real], vec![]).with_system_aliases_in(&base);

        assert!(!roots.admits_lexically(&base.join("x/proj/a.rs")));
    }

    #[cfg(unix)]
    #[test]
    fn test_system_aliases_ignore_file_dangling_and_looping_links() {
        let (_guard, base) = fake_root();
        let real = base.join("gone/proj");
        let file = base.join("file");
        std::fs::write(&file, "").unwrap();
        std::os::unix::fs::symlink(&file, base.join("to_file")).unwrap();
        std::os::unix::fs::symlink(base.join("gone"), base.join("dangling")).unwrap();
        std::os::unix::fs::symlink("loop_b", base.join("loop_a")).unwrap();
        std::os::unix::fs::symlink("loop_a", base.join("loop_b")).unwrap();

        let roots = WorkspaceRoots::for_test(vec![real], vec![]).with_system_aliases_in(&base);

        assert!(roots.aliases.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_system_aliases_unreadable_dir_yields_none() {
        let (_guard, base) = fake_root();
        let roots = WorkspaceRoots::for_test(vec![base.join("r")], vec![])
            .with_system_aliases_in(&base.join("missing"));
        assert!(roots.aliases.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_system_aliases_dotdot_and_sibling_rejected_without_stat() {
        let roots = WorkspaceRoots::for_test(
            vec![PathBuf::from("/private/tmp/proj")],
            vec![PathBuf::from("/tmp/proj")],
        );
        assert!(roots.admits_lexically(&PathBuf::from("/tmp/proj/a.rs")));
        assert!(!roots.admits_lexically(&lexically_normalize(Path::new("/tmp/proj/../other/a"))));
        assert!(!roots.admits_lexically(Path::new("/tmp/projx/a.rs")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_system_aliases_real_tmp_on_macos() {
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let canonical = dunce::canonicalize(dir.path()).unwrap();
        assert!(canonical.starts_with("/private/tmp"));

        let roots = WorkspaceRoots::for_test(vec![canonical], vec![]).with_system_aliases();

        assert!(roots.admits_lexically(&dir.path().join("a.rs")));
        assert!(!roots.admits_lexically(Path::new("/tmp/other-dir/a.rs")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_admits_edit_uri_refuses_a_path_under_an_unreadable_ancestor() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, base) = canonical_tempdir();
        let sealed = base.join("sealed");
        std::fs::create_dir(&sealed).unwrap();
        let roots = WorkspaceRoots::from_configured(std::slice::from_ref(&base)).unwrap();
        let uri = Uri::from(format!("file://{}", sealed.join("new.rs").display()).as_str());
        assert!(roots.admits_edit_uri(&uri).await);

        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let admitted = roots.admits_edit_uri(&uri).await;
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(!admitted, "an unreadable ancestor must not be skipped over");
    }
}
