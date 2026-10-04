//! A file path received from an MCP client, parsed at the boundary.
//!
//! [`ClientPath`] makes the shapes that can never name a file (empty, NUL
//! byte) unrepresentable, so they are rejected as invalid parameters at the
//! edge instead of surfacing from `std::path::absolute` or `canonicalize`
//! deep inside path validation.

use std::path::{Path, PathBuf};

/// Why a path received from a client is not a usable file path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidClientPath {
    /// The path is empty.
    #[error("file path is empty")]
    Empty,
    /// The path contains a NUL byte, which no filesystem accepts.
    #[error("file path contains a NUL byte")]
    ContainsNul,
}

/// A non-empty, NUL-free path supplied by an MCP client.
///
/// The path may be relative or contain `.`/`..`; whether it lies inside the
/// workspace is decided later by `validate_path_against_roots`. The MCP tool
/// methods parse their `file_path` into a `ClientPath` first thing (not while
/// deserializing the parameters, which rmcp reports as a tool-result error
/// rather than a JSON-RPC error), so a malformed path is `-32602`.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
///
/// use mcpls_core::bridge::{ClientPath, InvalidClientPath};
///
/// let path = ClientPath::try_from(PathBuf::from("/ws/src/main.rs"))?;
/// assert_eq!(path.as_path(), std::path::Path::new("/ws/src/main.rs"));
/// assert_eq!(
///     ClientPath::try_from(PathBuf::new()),
///     Err(InvalidClientPath::Empty)
/// );
/// # Ok::<(), InvalidClientPath>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientPath(PathBuf);

impl ClientPath {
    /// The validated path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl TryFrom<PathBuf> for ClientPath {
    type Error = InvalidClientPath;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        if path.is_empty() {
            return Err(InvalidClientPath::Empty);
        }
        if path.as_os_str().as_encoded_bytes().contains(&0) {
            return Err(InvalidClientPath::ContainsNul);
        }
        Ok(Self(path))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_try_from_rejects_empty_and_nul() {
        assert_eq!(
            ClientPath::try_from(PathBuf::new()),
            Err(InvalidClientPath::Empty)
        );
        assert_eq!(
            ClientPath::try_from(PathBuf::from("/ws/a\0b.rs")),
            Err(InvalidClientPath::ContainsNul)
        );
    }

    #[test]
    fn test_try_from_accepts_absolute_relative_and_dotted() {
        for raw in ["/ws/a.rs", "a.rs", "../a.rs", "/ws/./a.rs"] {
            let path = ClientPath::try_from(PathBuf::from(raw)).unwrap();
            assert_eq!(path.as_path(), Path::new(raw));
        }
    }

    #[test]
    fn test_error_messages_name_the_cause() {
        assert!(InvalidClientPath::Empty.to_string().contains("empty"));
        assert!(InvalidClientPath::ContainsNul.to_string().contains("NUL"));
    }
}
