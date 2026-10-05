//! Executable lookup shared by the tsserver pin and native server selection.
//!
//! Mirrors how the standard library finds a spawned command: a command with
//! more than one path component is used as given, a bare name is searched on
//! the `PATH` the child would see.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The first executable regular file named `command`, as given or on
/// `path_var`; a file without the execute bit is skipped, as spawn skips it.
pub fn find_executable(command: &Path, path_var: Option<&OsString>) -> Option<PathBuf> {
    if command.components().count() > 1 {
        return is_executable_file(command).then(|| command.to_path_buf());
    }
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(command))
        .find(|candidate| is_executable_file(candidate))
}

/// Whether `path` is a regular file the process may execute.
#[cfg(unix)]
pub fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Whether `path` is a regular file the process may execute.
#[cfg(not(unix))]
pub fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}

    #[cfg(unix)]
    #[test]
    fn test_find_executable_skips_files_without_the_execute_bit() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        std::fs::write(&tool, "").unwrap();
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(find_executable(Path::new("tool"), Some(&path_var)), None);
        assert_eq!(find_executable(&tool, None), None);
    }

    #[test]
    fn test_find_executable_searches_path_for_bare_names() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        std::fs::write(&tool, "").unwrap();
        make_executable(&tool);
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(
            find_executable(Path::new("tool"), Some(&path_var)),
            Some(tool)
        );
        assert_eq!(find_executable(Path::new("missing"), Some(&path_var)), None);
        assert_eq!(find_executable(Path::new("tool"), None), None);
    }

    #[test]
    fn test_find_executable_uses_multi_component_command_as_given() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        std::fs::write(&tool, "").unwrap();
        make_executable(&tool);
        assert_eq!(find_executable(&tool, None), Some(tool.clone()));
        assert_eq!(find_executable(&dir.path().join("none"), None), None);
    }
}
