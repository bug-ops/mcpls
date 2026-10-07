//! The host operating system, as far as spawning and executable lookup differ.

use std::path::{Path, PathBuf};

/// The host operating system, as far as spawning and executable lookup differ.
///
/// Passed explicitly so the Windows rules run, and are tested, on every OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    /// Windows: any regular file is executable, and `.cmd` shims exist.
    Windows,
    /// Every other host: a file is executable by its execute bit.
    Other,
}

impl HostOs {
    /// The host this build runs on.
    pub const CURRENT: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Other
    };

    /// Whether `path` is a regular file this host may execute: on Windows any
    /// file, elsewhere one with an execute bit.
    #[must_use]
    pub fn is_executable_file(self, path: &Path) -> bool {
        match self {
            Self::Windows => path.is_file(),
            Self::Other => is_unix_executable(path),
        }
    }

    /// `path` as spawned: on Windows a path without an extension gets `.exe`.
    pub(crate) fn spawn_name(self, path: &Path) -> PathBuf {
        if self == Self::Windows && path.extension().is_none() {
            path.with_added_extension("exe")
        } else {
            path.to_path_buf()
        }
    }
}

#[cfg(unix)]
fn is_unix_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_unix_executable(path: &Path) -> bool {
    path.is_file()
}
