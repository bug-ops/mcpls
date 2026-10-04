//! Workspace root set shared by every path-validation site.
//!
//! A [`WorkspaceRoots`] holds the canonical roots plus the lexical aliases a
//! client may legitimately name them by (configured form, logical `$PWD`).
//! Aliases are precomputed once so [`validate_path_against_roots`] can reject
//! an out-of-workspace path without touching the filesystem.
//!
//! [`validate_path_against_roots`]: crate::bridge::validate_path_against_roots

use std::path::{Component, Path, PathBuf, Prefix, PrefixComponent};
use std::sync::Arc;

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

    fn eq(self, a: Component<'_>, b: Component<'_>) -> bool {
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
        .all(|r| components.next().is_some_and(|c| rule.eq(c, r)))
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

/// Canonicalizes the longest existing ancestor of `path` and appends the
/// remainder, so a not-yet-created (or since-deleted) file still gets a
/// canonical form.
///
/// Uses [`dunce::canonicalize`] so Windows paths stay free of the `\\?\`
/// prefix when possible. Returns `None` when no ancestor can be canonicalized.
pub fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    path.ancestors().find_map(|ancestor| {
        let canonical = dunce::canonicalize(ancestor).ok()?;
        let rest = path.strip_prefix(ancestor).ok()?;
        Some(lexically_normalize(&canonical.join(rest)))
    })
}

/// The canonical workspace roots plus the lexical aliases they may be named by.
///
/// A path is admitted by `validate_path_against_roots` only if it passes the
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
    /// The caller vouches that `canonical` is already canonical (the
    /// containment check trusts it); [`Self::resolve`] is the public
    /// constructor that guarantees it. Aliases that are relative, empty after
    /// normalization, duplicated, or equal to a canonical root are dropped.
    #[must_use]
    pub(crate) fn new(canonical: Vec<PathBuf>, aliases: Vec<PathBuf>) -> Self {
        let mut kept: Vec<PathBuf> = Vec::new();
        for alias in aliases {
            if !alias.is_absolute() {
                continue;
            }
            let alias = lexically_normalize(dunce::simplified(&alias));
            if alias.as_os_str().is_empty() || canonical.contains(&alias) || kept.contains(&alias) {
                continue;
            }
            kept.push(alias);
        }
        Self {
            canonical: canonical.into(),
            aliases: kept.into(),
        }
    }

    /// Builds a root set from raw roots, canonicalizing each one.
    ///
    /// Touches the filesystem once per root. The raw form is kept as a
    /// lexical alias, so a client naming the root the way it was configured
    /// (for example a `/var` temp dir that canonicalizes to `/private/var`)
    /// is still admitted.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::bridge::WorkspaceRoots;
    ///
    /// let roots = WorkspaceRoots::resolve(vec![std::env::temp_dir()]);
    /// assert!(!roots.is_empty());
    /// ```
    #[must_use]
    pub fn resolve(roots: Vec<PathBuf>) -> Self {
        let canonical = roots
            .iter()
            .map(|root| canonicalize_existing_prefix(root).unwrap_or_else(|| root.clone()))
            .collect();
        Self::new(canonical, roots)
    }

    /// The canonical roots, in configuration order.
    #[must_use]
    pub fn canonical(&self) -> &[PathBuf] {
        &self.canonical
    }

    /// A shared handle to the canonical roots for lock-free consumers.
    #[must_use]
    pub fn canonical_shared(&self) -> Arc<[PathBuf]> {
        Arc::clone(&self.canonical)
    }

    /// Whether no root is configured (path validation then fails closed).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.canonical.is_empty()
    }

    /// Whether the absolute, lexically normalized `path` lies under a
    /// canonical root or one of its aliases. Never touches the filesystem.
    pub(crate) fn admits_lexically(&self, normalized: &Path) -> bool {
        self.canonical
            .iter()
            .chain(self.aliases.iter())
            .any(|root| is_within(normalized, root, CaseRule::LEXICAL))
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
    use super::*;

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

    /// An absolute path for the host platform (`/p` on Unix, `C:\p` on Windows).
    fn abs(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{}", path.replace('/', "\\")))
        } else {
            PathBuf::from(path)
        }
    }

    #[test]
    fn test_new_drops_relative_duplicate_and_canonical_aliases() {
        let roots = WorkspaceRoots::new(
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
        let roots = WorkspaceRoots::new(vec![abs("/real/ws")], vec![abs("/alias/ws")]);
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
    }

    #[test]
    fn test_canonicalize_existing_prefix_handles_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dunce::canonicalize(dir.path()).unwrap();
        let result = canonicalize_existing_prefix(&dir.path().join("gone/x.rs")).unwrap();
        assert_eq!(result, canonical.join("gone/x.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_keeps_symlinked_form_as_alias() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let roots = WorkspaceRoots::resolve(vec![link.clone()]);

        assert_eq!(roots.canonical(), [dunce::canonicalize(&real).unwrap()]);
        assert!(roots.admits_lexically(&link.join("a.rs")));
    }

    #[test]
    fn test_contains_canonical_is_case_exact() {
        let roots = WorkspaceRoots::new(vec![PathBuf::from("/Real")], vec![]);
        assert!(roots.contains_canonical(Path::new("/Real/a")));
        assert!(!roots.contains_canonical(Path::new("/real/a")));
    }

    #[cfg(windows)]
    #[test]
    fn test_verbatim_unc_equals_unc() {
        let roots = WorkspaceRoots::new(vec![PathBuf::from(r"\\server\share\ws")], vec![]);
        assert!(roots.contains_canonical(Path::new(r"\\?\UNC\server\share\ws\a.rs")));
        assert!(!roots.contains_canonical(Path::new(r"\\?\UNC\server\other\ws\a.rs")));
    }

    #[cfg(windows)]
    #[test]
    fn test_verbatim_disk_equals_disk() {
        let roots = WorkspaceRoots::new(vec![PathBuf::from(r"C:\ws")], vec![]);
        assert!(roots.contains_canonical(Path::new(r"\\?\C:\ws\a.rs")));
    }
}
