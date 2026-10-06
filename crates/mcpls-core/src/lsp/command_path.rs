//! Executable lookup shared by the tsserver pin and native server selection.
//!
//! Mirrors how the standard library finds a spawned command: a command with
//! more than one path component is used as given, a bare name is searched on
//! the `PATH` the child would see.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::bridge::WorkspaceRoots;
use crate::config::LspServerConfig;
use crate::lsp::child_env_var;

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

/// The executable regular file `path` names when spawned: on Windows a path
/// without an extension is spawned as `<path>.exe`.
fn spawn_target(path: &Path) -> Option<PathBuf> {
    let target = windows_spawn_name(path, cfg!(windows));
    is_executable_file(&target).then_some(target)
}

/// `path` as spawned: on Windows a path without an extension gets `.exe`.
fn windows_spawn_name(path: &Path, windows: bool) -> PathBuf {
    if windows && path.extension().is_none() {
        path.with_added_extension("exe")
    } else {
        path.to_path_buf()
    }
}

/// The executable a spawn of a server's `command` runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCommand {
    /// The absolute path found, not canonicalized: a program may act on the
    /// name it is started as (`rustup` proxies, `busybox`).
    pub spawn: PathBuf,
    /// The canonical location of `spawn`, where the file really lives.
    pub canonical: PathBuf,
}

/// The executable the standard library would spawn for `config`'s `command`,
/// or `None` when none is found.
///
/// A command with more than one path component resolves against the process
/// working directory, since the child is given no `current_dir`; a bare name
/// walks the child's `PATH` in order, relative entries again resolving against
/// the process working directory. `PATH` is read as the child sees it: the
/// config's `env` override, else `parent_env`.
pub fn resolve_command(
    config: &LspServerConfig,
    parent_env: impl Fn(&str) -> Option<OsString>,
) -> Option<ResolvedCommand> {
    let command = Path::new(&config.command);
    let found = if command.components().count() > 1 {
        spawn_target(command)
    } else {
        let path_var = child_env_var(config, "PATH", parent_env)?;
        std::env::split_paths(&path_var).find_map(|dir| spawn_target(&dir.join(command)))
    }?;
    let canonical = dunce::canonicalize(&found).ok()?;
    let spawn = std::path::absolute(&found).ok()?;
    Some(ResolvedCommand { spawn, canonical })
}

/// `path_var` without the entries a workspace controls: empty and relative
/// entries (which resolve against the working directory) and entries that lie
/// under `boundary`, judged by the longest existing prefix so a directory the
/// workspace creates later is excluded too.
///
/// An interpreter a script asks for through `#!/usr/bin/env`, or a tool a
/// server starts itself, is looked up on this variable rather than the one
/// the user set.
pub fn path_outside(path_var: &OsStr, boundary: &WorkspaceRoots) -> OsString {
    let kept = std::env::split_paths(path_var)
        .filter(|dir| dir.is_absolute() && !boundary.contains_resolved_prefix(dir));
    // The entries come from `split_paths`, so none contains the separator.
    std::env::join_paths(kept).unwrap_or_default()
}

/// `path_var`, or a fixed system search path when nothing is left of it:
/// an empty `PATH` makes `execvp`, and `env` through it, search the current
/// directory, which is what [`path_outside`] exists to avoid.
pub fn or_system_path(path_var: OsString) -> OsString {
    if !path_var.is_empty() {
        return path_var;
    }
    let system = if cfg!(windows) {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
            .join("System32")
    } else {
        PathBuf::from("/usr/bin")
    };
    let mut dirs = vec![system];
    if !cfg!(windows) {
        dirs.push(PathBuf::from("/bin"));
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_or_system_path_replaces_only_an_empty_path() {
        let kept = OsString::from("/opt/tools");
        assert_eq!(or_system_path(kept.clone()), kept);
        assert!(!or_system_path(OsString::new()).is_empty());
    }

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

    #[test]
    fn test_windows_spawn_name_adds_exe_only_without_an_extension() {
        let name = |path: &str, windows| windows_spawn_name(Path::new(path), windows);
        assert_eq!(
            name("tools/rust-analyzer", true),
            Path::new("tools/rust-analyzer.exe")
        );
        assert_eq!(
            name("tools/server.cmd", true),
            Path::new("tools/server.cmd")
        );
        assert_eq!(
            name("tools/rust-analyzer", false),
            Path::new("tools/rust-analyzer")
        );
    }

    #[test]
    fn test_path_outside_drops_workspace_relative_and_empty_entries() {
        let dir = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(dir.path()).unwrap();
        let (workspace, outside) = (base.join("ws"), base.join("outside"));
        std::fs::create_dir_all(workspace.join("bin")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let boundary = WorkspaceRoots::from_configured(std::slice::from_ref(&workspace)).unwrap();
        let path = std::env::join_paths([
            workspace.join("bin"),
            workspace.join("not-created-yet/bin"),
            PathBuf::from("relative/bin"),
            PathBuf::new(),
            outside.clone(),
        ])
        .unwrap();

        let kept = path_outside(&path, &boundary);

        assert_eq!(std::env::split_paths(&kept).collect::<Vec<_>>(), [outside]);
    }

    #[cfg(unix)]
    mod resolve {
        use std::os::unix::fs::PermissionsExt as _;

        use super::super::*;
        use crate::config::ServerCommand;

        fn executable(path: &Path) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn config(command: &str) -> LspServerConfig {
            let mut config = LspServerConfig::rust_analyzer();
            config.command = ServerCommand::new(command.to_string()).unwrap();
            config
        }

        fn path_env(dir: &Path) -> impl Fn(&str) -> Option<OsString> {
            let path = std::env::join_paths([dir]).unwrap();
            move |key| (key == "PATH").then(|| path.clone())
        }

        #[test]
        fn test_resolve_command_finds_bare_name_on_path() {
            let dir = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            executable(&root.join("bin/rust-analyzer"));
            let resolved = resolve_command(&config("rust-analyzer"), path_env(&root.join("bin")));
            assert_eq!(
                resolved.map(|r| r.canonical),
                Some(root.join("bin/rust-analyzer"))
            );
        }

        #[test]
        fn test_resolve_command_skips_non_executable_entries() {
            let dir = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            std::fs::create_dir_all(root.join("first")).unwrap();
            std::fs::write(root.join("first/tool"), "").unwrap();
            executable(&root.join("second/tool"));
            let path = std::env::join_paths([root.join("first"), root.join("second")]).unwrap();
            let resolved =
                resolve_command(&config("tool"), |key| (key == "PATH").then(|| path.clone()));
            assert_eq!(
                resolved.map(|r| r.canonical),
                Some(root.join("second/tool"))
            );
        }

        #[test]
        fn test_resolve_command_canonicalizes_symlinks() {
            let dir = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            executable(&root.join("real/tool"));
            std::fs::create_dir_all(root.join("bin")).unwrap();
            std::os::unix::fs::symlink(root.join("real/tool"), root.join("bin/tool")).unwrap();
            let resolved = resolve_command(&config("tool"), path_env(&root.join("bin"))).unwrap();
            assert_eq!(resolved.canonical, root.join("real/tool"));
            assert_eq!(resolved.spawn, root.join("bin/tool"));
        }

        #[test]
        fn test_resolve_command_uses_config_env_path_over_parent() {
            let dir = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            executable(&root.join("bin/tool"));
            let mut config = config("tool");
            config
                .env
                .insert("PATH".into(), root.join("bin").to_str().unwrap().into());
            let resolved = resolve_command(&config, |_| Some(OsString::from("/nonexistent")));
            assert_eq!(resolved.map(|r| r.canonical), Some(root.join("bin/tool")));
        }

        #[test]
        fn test_resolve_command_uses_absolute_command_directly() {
            let dir = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            executable(&root.join("x.sh"));
            let resolved = resolve_command(&config(root.join("x.sh").to_str().unwrap()), |_| None);
            assert_eq!(resolved.map(|r| r.canonical), Some(root.join("x.sh")));
        }

        #[test]
        fn test_resolve_command_returns_none_without_a_match() {
            assert_eq!(
                resolve_command(&config("definitely-not-installed"), |_| None),
                None
            );
            let dir = tempfile::tempdir().unwrap();
            assert_eq!(
                resolve_command(&config("missing"), path_env(dir.path())),
                None
            );
        }
    }
}
