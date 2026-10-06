//! Executable lookup shared by untrusted-mode planning, the tsserver pin and
//! native server selection.
//!
//! Mirrors how the standard library finds a spawned command: a command with
//! more than one path component is used as given, a bare name is searched on
//! the `PATH` the child would see. This is the only `PATH` walker, so the file
//! untrusted mode checks is the file the pin and selection inspect.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::bridge::WorkspaceRoots;
use crate::config::LspServerConfig;
use crate::lsp::{ManagedEnvVar, ParentEnv, child_env_var};

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
    fn spawn_name(self, path: &Path) -> PathBuf {
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

/// The executable regular file `path` names when spawned on `host`.
fn spawn_target(host: HostOs, path: &Path) -> Option<PathBuf> {
    let target = host.spawn_name(path);
    host.is_executable_file(&target).then_some(target)
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
/// working directory; a bare name walks the child's `PATH` in order, relative
/// entries again resolving against the process working directory. `PATH` is
/// read as the child sees it: the config's `env` override, else `parent_env`.
pub fn resolve_command(
    host: HostOs,
    config: &LspServerConfig,
    parent_env: impl ParentEnv,
) -> Option<ResolvedCommand> {
    resolve_named_on(host, Path::new(&config.command), config, parent_env)
}

/// [`resolve_command`] for `name`: a program the child would look up on its own
/// `PATH`, such as `node` or `tsc`, or a command given as a path.
pub fn resolve_named(
    name: &Path,
    config: &LspServerConfig,
    parent_env: impl ParentEnv,
) -> Option<ResolvedCommand> {
    resolve_named_on(HostOs::CURRENT, name, config, parent_env)
}

/// [`resolve_named`] with the spawn rules of `host`: on Windows a bare name
/// matches `<name>.exe`, and a name with an extension (`tsc.cmd`) matches
/// as given.
pub fn resolve_named_on(
    host: HostOs,
    name: &Path,
    config: &LspServerConfig,
    parent_env: impl ParentEnv,
) -> Option<ResolvedCommand> {
    let found = if name.components().count() > 1 {
        spawn_target(host, name)
    } else {
        let path_var = child_env_var(config, ManagedEnvVar::Path.name(), host, parent_env)?;
        std::env::split_paths(&path_var).find_map(|dir| spawn_target(host, &dir.join(name)))
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

/// `path_var`, or a fixed system search path of `host` when nothing is left of
/// it: an empty `PATH` makes `execvp`, and `env` through it, search the current
/// directory, which is what [`path_outside`] exists to avoid. On Windows the
/// system directory is `SystemRoot` from `parent_env`.
pub fn or_system_path(host: HostOs, path_var: OsString, parent_env: impl ParentEnv) -> OsString {
    if !path_var.is_empty() {
        return path_var;
    }
    let dirs = match host {
        HostOs::Windows => vec![
            PathBuf::from(parent_env("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
                .join("System32"),
        ],
        HostOs::Other => vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
    };
    std::env::join_paths(dirs).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_or_system_path_replaces_only_an_empty_path() {
        let kept = OsString::from("/opt/tools");
        assert_eq!(or_system_path(HostOs::Other, kept.clone(), |_| None), kept);
        assert!(!or_system_path(HostOs::CURRENT, OsString::new(), |_| None).is_empty());
    }

    #[test]
    fn test_or_system_path_follows_the_host_not_the_build_target() {
        let windows = or_system_path(HostOs::Windows, OsString::new(), |key| {
            (key == "SystemRoot").then(|| OsString::from("/win"))
        });
        let other = or_system_path(HostOs::Other, OsString::new(), |_| None);
        assert_eq!(
            windows,
            PathBuf::from("/win").join("System32").into_os_string()
        );
        assert!(other.to_string_lossy().contains("/usr/bin"));
    }

    #[test]
    fn test_windows_spawn_name_adds_exe_only_without_an_extension() {
        let name = |path: &str, host: HostOs| host.spawn_name(Path::new(path));
        assert_eq!(
            name("tools/rust-analyzer", HostOs::Windows),
            Path::new("tools/rust-analyzer.exe")
        );
        assert_eq!(
            name("tools/server.cmd", HostOs::Windows),
            Path::new("tools/server.cmd")
        );
        assert_eq!(
            name("tools/rust-analyzer", HostOs::Other),
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
        let boundary = WorkspaceRoots::from_paths(std::slice::from_ref(&workspace)).unwrap();
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
            config.command = ServerCommand::new(command.to_string()).unwrap().into();
            config
        }

        fn path_env(dir: &Path) -> impl ParentEnv {
            let path = std::env::join_paths([dir]).unwrap();
            move |key| (key == "PATH").then(|| path.clone())
        }

        #[test]
        fn test_resolve_command_finds_bare_name_on_path() {
            let dir = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            executable(&root.join("bin/rust-analyzer"));
            let resolved = resolve_command(
                HostOs::CURRENT,
                &config("rust-analyzer"),
                path_env(&root.join("bin")),
            );
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
            let resolved = resolve_command(HostOs::CURRENT, &config("tool"), |key| {
                (key == "PATH").then(|| path.clone())
            });
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
            let resolved = resolve_command(
                HostOs::CURRENT,
                &config("tool"),
                path_env(&root.join("bin")),
            )
            .unwrap();
            assert_eq!(resolved.canonical, root.join("real/tool"));
            assert_eq!(resolved.spawn, root.join("bin/tool"));
        }

        #[test]
        fn test_resolve_command_uses_config_env_path_over_parent() {
            let dir = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            executable(&root.join("bin/tool"));
            let mut config = config("tool");
            config.env.insert(
                "PATH".into(),
                root.join("bin").to_str().unwrap().into(),
                crate::lsp::HostOs::CURRENT,
            );
            let resolved = resolve_command(HostOs::CURRENT, &config, |_| {
                Some(OsString::from("/nonexistent"))
            });
            assert_eq!(resolved.map(|r| r.canonical), Some(root.join("bin/tool")));
        }

        #[test]
        fn test_resolve_command_uses_absolute_command_directly() {
            let dir = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            executable(&root.join("x.sh"));
            let resolved = resolve_command(
                HostOs::CURRENT,
                &config(root.join("x.sh").to_str().unwrap()),
                |_| None,
            );
            assert_eq!(resolved.map(|r| r.canonical), Some(root.join("x.sh")));
        }

        #[test]
        fn test_resolve_named_finds_a_program_other_than_the_command() {
            let dir = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            executable(&root.join("bin/node"));
            let resolved = resolve_named(
                Path::new("node"),
                &config("rust-analyzer"),
                path_env(&root.join("bin")),
            );
            assert_eq!(resolved.map(|r| r.spawn), Some(root.join("bin/node")));
        }

        #[test]
        fn test_resolve_command_returns_none_without_a_match() {
            assert_eq!(
                resolve_command(HostOs::CURRENT, &config("definitely-not-installed"), |_| {
                    None
                }),
                None
            );
            let dir = tempfile::tempdir().unwrap();
            assert_eq!(
                resolve_command(HostOs::CURRENT, &config("missing"), path_env(dir.path())),
                None
            );
        }
    }
}
