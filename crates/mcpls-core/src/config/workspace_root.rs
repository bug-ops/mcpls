//! A configured workspace root path that is never empty.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bridge::join_relative_root;

/// A `workspace.roots` entry was empty.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("workspace.roots entries cannot be empty")]
pub struct InvalidWorkspaceRoot;

/// A non-empty `workspace.roots` entry, kept as written.
///
/// An empty path is `is_relative()` and joins onto a base directory as that
/// directory, which is almost never what an empty string in a config file was
/// meant to say, so it is rejected when the value is built or deserialized.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ConfiguredRoot;
///
/// let root = ConfiguredRoot::new("/work/project").unwrap();
/// assert_eq!(root.as_path(), std::path::Path::new("/work/project"));
/// assert!(ConfiguredRoot::new("").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "PathBuf", into = "PathBuf")]
pub struct ConfiguredRoot(PathBuf);

impl ConfiguredRoot {
    /// Builds a root from any path.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidWorkspaceRoot`] if `root` is empty.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, InvalidWorkspaceRoot> {
        let root = root.into();
        if root.is_empty() {
            return Err(InvalidWorkspaceRoot);
        }
        Ok(Self(root))
    }

    /// The root as written.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// This root joined onto `base`, keeping the root as written when the
    /// join would leave nothing (a bare drive prefix onto an empty base).
    #[must_use]
    pub(crate) fn join_onto(&self, base: &Path) -> Self {
        let joined = join_relative_root(base, &self.0);
        if joined.is_empty() {
            self.clone()
        } else {
            Self(joined)
        }
    }
}

impl TryFrom<PathBuf> for ConfiguredRoot {
    type Error = InvalidWorkspaceRoot;

    fn try_from(root: PathBuf) -> Result<Self, Self::Error> {
        Self::new(root)
    }
}

impl From<ConfiguredRoot> for PathBuf {
    fn from(root: ConfiguredRoot) -> Self {
        root.0
    }
}

impl AsRef<Path> for ConfiguredRoot {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rejects_empty_and_keeps_the_path_as_written() {
        assert_eq!(ConfiguredRoot::new(""), Err(InvalidWorkspaceRoot));
        let root = ConfiguredRoot::new("rel/dir").unwrap();
        assert_eq!(root.as_path(), Path::new("rel/dir"));
        assert_eq!(PathBuf::from(root), PathBuf::from("rel/dir"));
    }

    #[test]
    fn test_deserialization_applies_the_same_rule() {
        assert!(serde_json::from_str::<ConfiguredRoot>("\"\"").is_err());
        let root: ConfiguredRoot = serde_json::from_str("\"a\"").unwrap();
        assert_eq!(serde_json::to_string(&root).unwrap(), "\"a\"");
    }

    #[test]
    fn test_join_onto_appends_the_relative_tail() {
        let root = ConfiguredRoot::new("proj").unwrap();
        assert_eq!(
            root.join_onto(Path::new("base")).as_path(),
            Path::new("base").join("proj")
        );
    }
}
