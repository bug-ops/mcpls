//! Untrusted-workspace planning: which configured servers start, and in what
//! shape.
//!
//! [`plan_server_starts`] splits the applicable servers into those admitted
//! (hardened, in untrusted mode) and those refused, so a refused server never
//! becomes a [`ServerInitConfig`] and no startup, restart or respawn can reach
//! a spawn for it.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::info;

use crate::bridge::WorkspaceRoots;
use crate::config::{
    BuiltinServer, LspServerConfig, ServerCommand, ServerConfig, WorkspaceTrust, login_home_dir,
};
use crate::error::{HomeVariable, ServerSpawnFailure, StartupFailure, UntrustedRefusal};
use crate::lsp::{self, ServerInitConfig};
use crate::redaction::Redactions;

/// A server untrusted-workspace mode kept from starting, with the config it
/// would have started from so routing still knows which tools it claims.
#[derive(Debug, Clone)]
pub struct RefusedServer {
    /// The configured entry, before any TypeScript selection or pin.
    pub config: LspServerConfig,
    /// The recorded refusal, reported when a tool call routes to the server.
    pub failure: ServerSpawnFailure,
}

/// The servers worth starting for this run and the ones refused.
///
/// Refused servers never become a [`ServerInitConfig`], so neither startup,
/// restart nor respawn can reach a spawn for them.
#[derive(Debug, Default)]
pub struct StartPlan {
    /// Servers to start, with their roots, position encodings and (for the
    /// TypeScript server) the pinned `tsserver` they are initialized with.
    pub admitted: Vec<ServerInitConfig>,
    /// Applicable servers untrusted mode refused.
    pub refused: Vec<RefusedServer>,
}

impl StartPlan {
    /// The recorded refusals, in configuration order.
    #[must_use]
    pub fn failures(&self) -> Vec<ServerSpawnFailure> {
        self.refused
            .iter()
            .map(|refused| refused.failure.clone())
            .collect()
    }
}

/// Refuses `configured` unless the user allowed it, as configured and before
/// any TypeScript selection.
fn allowlist_refusal(
    trust: &WorkspaceTrust,
    configured: &LspServerConfig,
) -> Option<UntrustedRefusal> {
    (!trust.allows(&configured.id())).then(|| UntrustedRefusal::NotAllowed {
        builtin: BuiltinServer::ALL
            .into_iter()
            .find(|builtin| builtin.matches_command(configured.command.as_str())),
    })
}

/// `effective` pinned to what untrusted mode vetted: the executable resolved
/// to an absolute path, and a `PATH` without workspace, relative or empty
/// entries.
///
/// Spawning the resolved path, rather than the bare name, closes the window
/// between the check and a spawn (restart, respawn) in which a workspace
/// could add a binary. The sanitized `PATH` is what a `#!/usr/bin/env`
/// interpreter and the tools the server itself starts are looked up on.
///
/// Refuses when the executable is not found, or lies inside `boundary`;
/// naming the server in `--allow-server` is not consent to a binary the
/// workspace supplies.
fn harden_for_untrusted(
    effective: Cow<'_, LspServerConfig>,
    boundary: &WorkspaceRoots,
    login_home: Option<&Path>,
    parent_env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<LspServerConfig, UntrustedRefusal> {
    let mut effective = effective.into_owned();
    normalize_env_keys(&mut effective.env, cfg!(windows));
    let unresolved = |command: &str| UntrustedRefusal::UnresolvedExecutable {
        command: command.to_owned(),
    };
    let resolved = lsp::command_path::resolve_command(&effective, parent_env)
        .ok_or_else(|| unresolved(effective.command.as_str()))?;
    if boundary.contains_canonical(&resolved.canonical) {
        return Err(UntrustedRefusal::WorkspaceExecutable {
            executable: resolved.canonical,
        });
    }
    let in_workspace = resolved
        .spawn
        .parent()
        .is_none_or(|dir| boundary.contains_resolved_prefix(dir));
    let command = if in_workspace {
        &resolved.canonical
    } else {
        &resolved.spawn
    }
    .to_str()
    .ok_or_else(|| unresolved(effective.command.as_str()))?
    .to_owned();
    let path = lsp::child_env_var(&effective, "PATH", parent_env).unwrap_or_default();
    let path = lsp::command_path::or_system_path(lsp::command_path::path_outside(&path, boundary))
        .into_string()
        .map_err(|_| unresolved(&command))?;
    let home_env = home_overrides(&effective, boundary, login_home, parent_env)?;
    effective.command =
        ServerCommand::new(command).map_err(|_| unresolved(effective.command.as_str()))?;
    effective.env.insert("PATH".to_owned(), path);
    effective.env.extend(home_env);
    Ok(effective)
}

/// The environment variables untrusted mode manages for a server.
const MANAGED_ENV: [&str; 3] = ["PATH", "HOME", "USERPROFILE"];

/// Gives each managed variable in `env` one exact spelling.
///
/// Environment names are case-insensitive on Windows, so a server's `Path` and
/// the `PATH` this mode sets would otherwise be two keys, and which one the
/// child sees is undefined. With `case_insensitive` a differently spelled key
/// is renamed to the canonical one (an existing canonical key wins).
fn normalize_env_keys(env: &mut HashMap<String, String>, case_insensitive: bool) {
    if !case_insensitive {
        return;
    }
    for name in MANAGED_ENV {
        let aliases: Vec<String> = env
            .keys()
            .filter(|key| key.as_str() != name && key.eq_ignore_ascii_case(name))
            .cloned()
            .collect();
        for alias in aliases {
            if let Some(value) = env.remove(&alias) {
                env.entry(name.to_owned()).or_insert(value);
            }
        }
    }
}

/// The home-directory variables untrusted mode sets for a server that does
/// not set them itself: the login home, so a `HOME` or `USERPROFILE` that
/// names the workspace cannot steer rustup, cargo or npm configuration.
///
/// When the login home is unknown, or cannot be written into the UTF-8
/// environment, nothing can be substituted: an inherited variable that is
/// empty or lies inside `boundary` is refused, as is an unset `HOME` on Unix
/// (tools would resolve `~` against the working directory), and any other
/// value is passed on.
fn home_overrides(
    effective: &LspServerConfig,
    boundary: &WorkspaceRoots,
    login_home: Option<&Path>,
    parent_env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Vec<(String, String)>, UntrustedRefusal> {
    let unset = HomeVariable::ALL
        .into_iter()
        .filter(|variable| !effective.env.contains_key(variable.name()));
    if let Some(login_home) = login_home.and_then(Path::to_str) {
        return Ok(unset
            .map(|variable| (variable.name().to_owned(), login_home.to_owned()))
            .collect());
    }
    for variable in unset {
        let Some(value) = parent_env(variable.name()) else {
            if variable == HomeVariable::Home && cfg!(unix) {
                return Err(UntrustedRefusal::UnknownHome);
            }
            continue;
        };
        if value.is_empty() {
            return Err(UntrustedRefusal::EmptyHome { variable });
        }
        let home = PathBuf::from(value);
        if boundary.contains_resolved_prefix(&home) {
            return Err(UntrustedRefusal::WorkspaceHome { variable, home });
        }
    }
    Ok(Vec::new())
}

/// Splits the configured servers into the ones worth starting for this run
/// and the ones untrusted-workspace mode refuses.
///
/// A server is applicable when its project markers are found under at least
/// one workspace root. A refused server gets no TypeScript selection and no
/// pin: nothing about it is inspected beyond the allowlist, except the
/// executable checks of an allowed one.
pub fn plan_server_starts(
    config: &ServerConfig,
    roots: &WorkspaceRoots,
    redactions: &Arc<Redactions>,
) -> StartPlan {
    let max_depth = config.workspace.heuristics_max_depth;
    let untrusted = matches!(config.workspace_trust, WorkspaceTrust::Untrusted(_));
    let login_home = untrusted.then(login_home_dir).flatten();
    let boundary = untrusted.then(|| {
        roots.untrusted_boundary(!config.workspace.roots.is_empty(), login_home.as_deref())
    });
    let mut plan = StartPlan::default();
    for lsp_config in &config.lsp_servers {
        let should_spawn = roots
            .canonical()
            .iter()
            .any(|root| lsp_config.should_spawn(root, max_depth));

        if !should_spawn {
            info!(
                "Skipping LSP server '{}' ({}): no project markers found",
                lsp_config.language_id, lsp_config.command
            );
            continue;
        }

        if let Some(refusal) = allowlist_refusal(&config.workspace_trust, lsp_config) {
            plan.refused
                .push(refused(lsp_config, lsp_config.command.as_str(), refusal));
            continue;
        }

        let selected =
            lsp::tsserver_pin::with_selected_typescript_server(lsp_config, roots, |key| {
                std::env::var_os(key)
            });
        let effective = match &boundary {
            None => selected.into_owned(),
            Some(boundary) => {
                match harden_for_untrusted(selected, boundary, login_home.as_deref(), &|key| {
                    std::env::var_os(key)
                }) {
                    Ok(hardened) => hardened,
                    Err(refusal) => {
                        plan.refused.push(refused(
                            lsp_config,
                            lsp_config.command.as_str(),
                            refusal,
                        ));
                        continue;
                    }
                }
            }
        };
        let workspace_tsserver = boundary.as_ref().and_then(|boundary| {
            lsp::tsserver_pin::pinned_inside_workspace(&effective, boundary, |key| {
                std::env::var_os(key)
            })
        });
        if let Some(tsserver) = workspace_tsserver {
            let refusal = UntrustedRefusal::WorkspaceTsserver { tsserver };
            plan.refused
                .push(refused(lsp_config, effective.command.as_str(), refusal));
            continue;
        }
        let mut server_config = effective;
        server_config.initialization_options =
            lsp::tsserver_pin::pinned_initialization_options(&server_config, roots, |key| {
                std::env::var_os(key)
            });
        plan.admitted.push(ServerInitConfig::new(
            server_config,
            roots.clone(),
            config.workspace.position_encodings.clone(),
            Arc::clone(redactions),
        ));
    }
    plan
}

/// A refusal of `configured`. The failure names `command`, the command that
/// would have been spawned (after TypeScript selection and hardening); the
/// routing config stays the one as configured.
fn refused(
    configured: &LspServerConfig,
    command: &str,
    refusal: UntrustedRefusal,
) -> RefusedServer {
    RefusedServer {
        config: configured.clone(),
        failure: ServerSpawnFailure {
            server_id: configured.id(),
            language_id: configured.language_id.clone(),
            command: command.to_owned(),
            reason: StartupFailure::RefusedUntrustedWorkspace(refusal),
        },
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use crate::config::{ServerCommand, ServerId};

    fn rust_workspace() -> (tempfile::TempDir, WorkspaceRoots) {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let roots = WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap();
        (dir, roots)
    }

    fn config_with(servers: Vec<LspServerConfig>, trust: WorkspaceTrust) -> ServerConfig {
        ServerConfig {
            lsp_servers: servers,
            workspace_trust: trust,
            ..ServerConfig::default()
        }
    }

    fn config_with_rust_analyzer() -> ServerConfig {
        config_with(
            vec![LspServerConfig::rust_analyzer()],
            WorkspaceTrust::Trusted,
        )
    }

    fn plan(config: &ServerConfig, roots: &WorkspaceRoots) -> StartPlan {
        let redactions = Arc::new(Redactions::for_servers(
            &config.lsp_servers,
            lsp::current_environment(),
        ));
        plan_server_starts(config, roots, &redactions)
    }

    fn admitted_ids(plan: &StartPlan) -> Vec<ServerId> {
        plan.admitted
            .iter()
            .map(|init| init.server_config().id())
            .collect()
    }

    fn refused_ids(plan: &StartPlan) -> Vec<ServerId> {
        plan.refused
            .iter()
            .map(|refused| refused.failure.server_id.clone())
            .collect()
    }

    #[test]
    fn normalize_env_keys_renames_differently_cased_keys_when_case_insensitive() {
        let mut env = HashMap::from([
            ("Path".to_owned(), "/a".to_owned()),
            ("home".to_owned(), "/h".to_owned()),
            ("Other".to_owned(), "x".to_owned()),
        ]);

        normalize_env_keys(&mut env, true);

        assert_eq!(env.get("PATH").map(String::as_str), Some("/a"));
        assert_eq!(env.get("HOME").map(String::as_str), Some("/h"));
        assert!(!env.contains_key("Path") && !env.contains_key("home"));
        assert_eq!(env.get("Other").map(String::as_str), Some("x"));
    }

    #[test]
    fn normalize_env_keys_keeps_an_existing_canonical_key_and_leaves_case_sensitive_maps() {
        let mut env = HashMap::from([
            ("PATH".to_owned(), "/exact".to_owned()),
            ("Path".to_owned(), "/other".to_owned()),
        ]);
        let untouched = env.clone();

        normalize_env_keys(&mut env, false);
        assert_eq!(env, untouched);

        normalize_env_keys(&mut env, true);
        assert_eq!(env.len(), 1);
        assert_eq!(env.get("PATH").map(String::as_str), Some("/exact"));
    }

    #[test]
    fn plan_skips_server_without_project_markers() {
        let dir = tempfile::TempDir::new().unwrap();
        let roots = WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap();

        let plan = plan(&config_with_rust_analyzer(), &roots);

        assert!(plan.admitted.is_empty() && plan.refused.is_empty());
    }

    #[test]
    fn plan_keeps_server_with_project_markers() {
        let (_dir, roots) = rust_workspace();

        let plan = plan(&config_with_rust_analyzer(), &roots);

        assert_eq!(plan.admitted.len(), 1);
        assert!(plan.refused.is_empty());
        assert_eq!(
            plan.admitted[0].workspace_roots().canonical(),
            roots.canonical()
        );
    }

    #[test]
    fn plan_untrusted_without_allowlist_refuses_every_applicable_server() {
        let (_dir, roots) = rust_workspace();
        let config = config_with(
            vec![LspServerConfig::rust_analyzer(), LspServerConfig::gopls()],
            WorkspaceTrust::untrusted([]),
        );

        let plan = plan(&config, &roots);

        assert!(plan.admitted.is_empty());
        assert_eq!(refused_ids(&plan), [ServerId::from("rust")]);
        std::assert_matches!(
            &plan.refused[0].failure.reason,
            StartupFailure::RefusedUntrustedWorkspace(UntrustedRefusal::NotAllowed {
                builtin: Some(BuiltinServer::RustAnalyzer)
            })
        );
    }

    #[test]
    fn plan_untrusted_admits_only_the_allowlisted_server() {
        let (dir, roots) = rust_workspace();
        std::fs::write(dir.path().join("go.mod"), "").unwrap();
        let tools = tempfile::TempDir::new().unwrap();
        let gopls = tools
            .path()
            .join(if cfg!(windows) { "gopls.exe" } else { "gopls" });
        std::fs::write(&gopls, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&gopls, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut go = LspServerConfig::gopls();
        go.command = ServerCommand::new(gopls.to_string_lossy().into_owned()).unwrap();
        let config = config_with(
            vec![LspServerConfig::rust_analyzer(), go],
            WorkspaceTrust::untrusted([ServerId::from("go")]),
        );

        let plan = plan(&config, &roots);

        assert_eq!(admitted_ids(&plan), [ServerId::from("go")]);
        assert_eq!(refused_ids(&plan), [ServerId::from("rust")]);
    }

    #[test]
    fn plan_untrusted_marks_a_custom_server_as_not_builtin() {
        let (_dir, roots) = rust_workspace();
        let mut custom = LspServerConfig::rust_analyzer();
        custom.command = ServerCommand::from_static("my-rust-server");
        let config = config_with(vec![custom], WorkspaceTrust::untrusted([]));

        let plan = plan(&config, &roots);

        std::assert_matches!(
            &plan.refused[0].failure.reason,
            StartupFailure::RefusedUntrustedWorkspace(UntrustedRefusal::NotAllowed {
                builtin: None
            })
        );
    }

    #[test]
    fn plan_untrusted_does_not_refuse_a_server_heuristics_skip() {
        let dir = tempfile::TempDir::new().unwrap();
        let roots = WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap();
        let config = config_with(
            vec![LspServerConfig::rust_analyzer()],
            WorkspaceTrust::untrusted([]),
        );

        let plan = plan(&config, &roots);

        assert!(plan.admitted.is_empty() && plan.refused.is_empty());
    }

    #[test]
    fn plan_trusted_runs_no_executable_check() {
        let (dir, roots) = rust_workspace();
        let mut config = LspServerConfig::rust_analyzer();
        config.command = ServerCommand::new(
            dir.path()
                .join("bin/rust-analyzer")
                .to_string_lossy()
                .into_owned(),
        )
        .unwrap();

        let plan = plan(&config_with(vec![config], WorkspaceTrust::Trusted), &roots);

        assert_eq!(plan.admitted.len(), 1);
        assert!(plan.refused.is_empty());
    }

    #[cfg(unix)]
    mod executable_tests {
        use std::os::unix::fs::PermissionsExt as _;
        use std::path::{Component, Path, PathBuf};

        use super::*;
        use crate::config::ServerCommand;

        struct Fixture {
            _dir: tempfile::TempDir,
            workspace: PathBuf,
            outside: PathBuf,
            roots: WorkspaceRoots,
        }

        fn fixture() -> Fixture {
            let dir = tempfile::TempDir::new().unwrap();
            let base = dunce::canonicalize(dir.path()).unwrap();
            let (workspace, outside) = (base.join("ws"), base.join("outside"));
            std::fs::create_dir_all(workspace.join("bin")).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(workspace.join("Cargo.toml"), "").unwrap();
            let roots = WorkspaceRoots::from_configured(std::slice::from_ref(&workspace)).unwrap();
            Fixture {
                _dir: dir,
                workspace,
                outside,
                roots,
            }
        }

        fn executable(path: &Path) {
            std::fs::write(path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn relative_from_cwd(target: &Path) -> PathBuf {
            let cwd = dunce::canonicalize(std::env::current_dir().unwrap()).unwrap();
            let up: PathBuf = cwd
                .components()
                .filter(|part| matches!(part, Component::Normal(_)))
                .map(|_| "..")
                .collect();
            up.join(target.strip_prefix("/").unwrap())
        }

        fn rust_with(command: &str) -> LspServerConfig {
            let mut config = LspServerConfig::rust_analyzer();
            config.command = ServerCommand::new(command.to_string()).unwrap();
            config
        }

        fn rust_with_path(path: &Path) -> LspServerConfig {
            let mut config = rust_with("rust-analyzer");
            config
                .env
                .insert("PATH".into(), path.to_string_lossy().into_owned());
            config
        }

        fn untrusted_allowing_rust() -> WorkspaceTrust {
            WorkspaceTrust::untrusted([ServerId::from("rust")])
        }

        fn refused_executable(plan: &StartPlan) -> Option<PathBuf> {
            match &plan.refused.first()?.failure.reason {
                StartupFailure::RefusedUntrustedWorkspace(
                    UntrustedRefusal::WorkspaceExecutable { executable },
                ) => Some(executable.clone()),
                _ => None,
            }
        }

        fn plan_allowing_rust(config: LspServerConfig, fx: &Fixture) -> StartPlan {
            plan(
                &config_with(vec![config], untrusted_allowing_rust()),
                &fx.roots,
            )
        }

        #[test]
        fn plan_refuses_an_allowed_server_with_an_absolute_workspace_executable() {
            let fx = fixture();
            let exe = fx.workspace.join("bin/rust-analyzer");
            executable(&exe);

            let plan = plan_allowing_rust(rust_with(exe.to_str().unwrap()), &fx);

            assert!(plan.admitted.is_empty());
            assert_eq!(refused_executable(&plan), Some(exe));
        }

        #[test]
        fn plan_refuses_a_relative_workspace_executable() {
            let fx = fixture();
            let exe = fx.workspace.join("bin/rust-analyzer");
            executable(&exe);

            let relative = relative_from_cwd(&exe);
            let plan = plan_allowing_rust(rust_with(relative.to_str().unwrap()), &fx);

            assert_eq!(refused_executable(&plan), Some(exe));
        }

        #[test]
        fn plan_refuses_a_path_entry_inside_the_workspace() {
            let fx = fixture();
            let exe = fx.workspace.join("bin/rust-analyzer");
            executable(&exe);
            let bin = fx.workspace.join("bin");
            for entry in [bin.clone(), relative_from_cwd(&bin)] {
                let plan = plan_allowing_rust(rust_with_path(&entry), &fx);

                assert_eq!(refused_executable(&plan), Some(exe.clone()), "{entry:?}");
            }
        }

        #[test]
        fn plan_admits_a_workspace_symlink_to_an_outside_binary() {
            let fx = fixture();
            let real = fx.outside.join("rust-analyzer");
            executable(&real);
            std::os::unix::fs::symlink(&real, fx.workspace.join("bin/rust-analyzer")).unwrap();

            let plan = plan_allowing_rust(rust_with_path(&fx.workspace.join("bin")), &fx);

            assert_eq!(admitted_ids(&plan), [ServerId::from("rust")]);
        }

        #[test]
        fn plan_admits_an_allowed_server_with_an_outside_executable() {
            let fx = fixture();
            executable(&fx.outside.join("rust-analyzer"));

            let plan = plan_allowing_rust(rust_with_path(&fx.outside), &fx);

            assert_eq!(admitted_ids(&plan), [ServerId::from("rust")]);
            assert!(plan.refused.is_empty());
        }

        #[test]
        fn plan_not_allowed_wins_over_the_executable_check() {
            let fx = fixture();
            let exe = fx.workspace.join("bin/rust-analyzer");
            executable(&exe);
            let config = rust_with(exe.to_str().unwrap());

            let plan = plan(
                &config_with(vec![config], WorkspaceTrust::untrusted([])),
                &fx.roots,
            );

            assert_eq!(refused_executable(&plan), None);
            assert_eq!(refused_ids(&plan), [ServerId::from("rust")]);
        }

        #[test]
        fn plan_trusted_admits_a_workspace_executable() {
            let fx = fixture();
            let exe = fx.workspace.join("bin/rust-analyzer");
            executable(&exe);
            let config = rust_with(exe.to_str().unwrap());

            let plan = plan(
                &config_with(vec![config], WorkspaceTrust::Trusted),
                &fx.roots,
            );

            assert_eq!(admitted_ids(&plan), [ServerId::from("rust")]);
        }

        #[test]
        fn plan_spawns_the_resolved_executable_with_a_path_outside_the_workspace() {
            let fx = fixture();
            let exe = fx.outside.join("rust-analyzer");
            executable(&exe);
            let mut config = rust_with("rust-analyzer");
            let path = std::env::join_paths([
                fx.workspace.join("bin"),
                PathBuf::from("relative/bin"),
                fx.outside.clone(),
            ])
            .unwrap();
            config
                .env
                .insert("PATH".into(), path.to_string_lossy().into_owned());

            let plan = plan_allowing_rust(config, &fx);

            let admitted = &plan.admitted[0].server_config();
            assert_eq!(admitted.command, exe.to_str().unwrap());
            assert_eq!(
                admitted.env.get("PATH").map(String::as_str),
                fx.outside.to_str()
            );
        }

        #[test]
        fn plan_spawns_the_canonical_target_of_a_workspace_symlink() {
            let fx = fixture();
            let real = fx.outside.join("rust-analyzer");
            executable(&real);
            std::os::unix::fs::symlink(&real, fx.workspace.join("bin/rust-analyzer")).unwrap();

            let plan = plan_allowing_rust(rust_with_path(&fx.workspace.join("bin")), &fx);

            assert_eq!(
                plan.admitted[0].server_config().command,
                real.to_str().unwrap()
            );
        }

        #[test]
        fn plan_refuses_an_executable_that_cannot_be_resolved() {
            let fx = fixture();

            let plan = plan_allowing_rust(rust_with_path(&fx.outside), &fx);

            assert!(plan.admitted.is_empty());
            std::assert_matches!(
                &plan.refused[0].failure.reason,
                StartupFailure::RefusedUntrustedWorkspace(
                    UntrustedRefusal::UnresolvedExecutable { command }
                ) if command == "rust-analyzer"
            );
        }

        #[test]
        fn plan_trusted_leaves_the_command_and_environment_untouched() {
            let fx = fixture();
            executable(&fx.outside.join("rust-analyzer"));
            let config = rust_with("rust-analyzer");

            let plan = plan(
                &config_with(vec![config], WorkspaceTrust::Trusted),
                &fx.roots,
            );

            let admitted = &plan.admitted[0].server_config();
            assert_eq!(admitted.command, "rust-analyzer");
            assert!(admitted.env.is_empty());
        }

        #[test]
        fn plan_never_leaves_an_empty_path_when_every_entry_is_stripped() {
            let fx = fixture();
            let exe = fx.outside.join("rust-analyzer");
            executable(&exe);
            let mut config = rust_with(exe.to_str().unwrap());
            config.env.insert(
                "PATH".into(),
                fx.workspace.join("bin").to_string_lossy().into_owned(),
            );

            let plan = plan_allowing_rust(config, &fx);

            let path = plan.admitted[0].server_config().env.get("PATH").unwrap();
            assert_eq!(path, "/usr/bin:/bin");
        }

        #[test]
        fn plan_gives_an_untrusted_server_the_login_home_not_the_inherited_one() {
            let fx = fixture();
            let exe = fx.outside.join("rust-analyzer");
            executable(&exe);

            let plan = plan_allowing_rust(rust_with(exe.to_str().unwrap()), &fx);

            let home = login_home_dir().unwrap();
            assert_eq!(
                plan.admitted[0]
                    .server_config()
                    .env
                    .get("HOME")
                    .map(String::as_str),
                home.to_str()
            );
        }

        #[test]
        fn boundary_without_configured_roots_excludes_only_the_login_home() {
            let fx = fixture();
            let elsewhere = fx.outside.clone();

            let kept = fx.roots.untrusted_boundary(false, Some(&elsewhere));
            let dropped = fx.roots.untrusted_boundary(false, Some(&fx.workspace));
            let configured = fx.roots.untrusted_boundary(true, Some(&fx.workspace));

            assert_eq!(kept.canonical(), fx.roots.canonical());
            assert!(dropped.canonical().is_empty());
            assert_eq!(configured.canonical(), fx.roots.canonical());
        }

        fn harden_with(
            fx: &Fixture,
            env: &[(&str, &Path)],
            login_home: Option<&Path>,
        ) -> Result<LspServerConfig, UntrustedRefusal> {
            let exe = fx.outside.join("rust-analyzer");
            executable(&exe);
            let env: Vec<(String, std::ffi::OsString)> = env
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.as_os_str().to_owned()))
                .collect();
            let parent_env = move |key: &str| {
                env.iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.clone())
            };
            let boundary =
                WorkspaceRoots::from_configured(std::slice::from_ref(&fx.workspace)).unwrap();
            harden_for_untrusted(
                Cow::Owned(rust_with(exe.to_str().unwrap())),
                &boundary,
                login_home,
                &parent_env,
            )
        }

        #[test]
        fn harden_gives_a_child_a_sanitized_path_even_when_mcpls_has_none() {
            let fx = fixture();

            let hardened = harden_with(&fx, &[], Some(&fx.outside)).unwrap();

            assert_eq!(hardened.env.get("PATH").unwrap(), "/usr/bin:/bin");
        }

        #[test]
        fn harden_replaces_the_home_variables_with_the_login_home() {
            let fx = fixture();
            let home = fx.outside.join("login-home");
            std::fs::create_dir_all(&home).unwrap();

            let hardened = harden_with(&fx, &[("HOME", &fx.workspace)], Some(&home)).unwrap();

            assert_eq!(hardened.env.get("HOME").map(String::as_str), home.to_str());
            assert_eq!(
                hardened.env.get("USERPROFILE").map(String::as_str),
                home.to_str()
            );
        }

        #[test]
        fn harden_refuses_a_workspace_home_when_the_login_home_is_unknown() {
            let fx = fixture();
            let inside = fx.workspace.join("home");

            let refused = harden_with(&fx, &[("HOME", &inside)], None).unwrap_err();
            let passed = harden_with(&fx, &[("HOME", &fx.outside)], None).unwrap();

            std::assert_matches!(refused, UntrustedRefusal::WorkspaceHome { .. });
            assert!(!passed.env.contains_key("HOME"));
        }

        #[test]
        fn harden_refuses_a_workspace_home_when_the_login_home_is_not_utf8() {
            use std::os::unix::ffi::OsStrExt as _;

            let fx = fixture();
            let non_utf8 = PathBuf::from(std::ffi::OsStr::from_bytes(b"/home/\xff"));
            let inside = fx.workspace.join("home");

            let refused = harden_with(&fx, &[("HOME", &inside)], Some(&non_utf8)).unwrap_err();

            std::assert_matches!(refused, UntrustedRefusal::WorkspaceHome { .. });
        }

        #[cfg(unix)]
        #[test]
        fn harden_refuses_an_absent_home_when_the_login_home_is_unknown() {
            let fx = fixture();

            let refused = harden_with(&fx, &[], None).unwrap_err();
            let known = harden_with(&fx, &[], Some(&fx.outside)).unwrap();

            std::assert_matches!(refused, UntrustedRefusal::UnknownHome);
            assert_eq!(
                known.env.get("HOME").map(String::as_str),
                fx.outside.to_str()
            );
        }

        #[test]
        fn harden_refuses_an_empty_home_when_the_login_home_is_unknown() {
            let fx = fixture();

            for variable in HomeVariable::ALL {
                let mut env = vec![(variable.name(), Path::new(""))];
                if variable != HomeVariable::Home {
                    env.push(("HOME", fx.outside.as_path()));
                }
                let refused = harden_with(&fx, &env, None).unwrap_err();

                assert_eq!(refused, UntrustedRefusal::EmptyHome { variable });
                assert!(refused.to_string().contains("empty"), "{refused}");
            }
        }

        #[test]
        fn harden_refuses_a_workspace_userprofile_when_the_login_home_is_unknown() {
            let fx = fixture();
            let inside = fx.workspace.join("profile");

            let env = [
                ("HOME", fx.outside.as_path()),
                ("USERPROFILE", inside.as_path()),
            ];
            let refused = harden_with(&fx, &env, None).unwrap_err();

            std::assert_matches!(refused, UntrustedRefusal::WorkspaceHome { .. });
        }

        struct TypescriptInstall {
            bin: PathBuf,
        }

        fn typescript_workspace(fx: &Fixture) {
            std::fs::write(fx.workspace.join("package.json"), "{}").unwrap();
            std::fs::write(fx.workspace.join("tsconfig.json"), "{}").unwrap();
        }

        fn typescript_package(
            outside: &Path,
            link_target: Option<&Path>,
            version: &str,
            with_tsserver: bool,
        ) -> TypescriptInstall {
            let modules = outside.join("prefix/lib/node_modules");
            let package = modules.join("typescript-language-server");
            std::fs::create_dir_all(package.join("lib")).unwrap();
            std::fs::write(package.join("package.json"), "{}").unwrap();
            let cli = package.join("lib/cli.mjs");
            executable(&cli);
            let typescript = modules.join("typescript");
            if let Some(target) = link_target {
                std::os::unix::fs::symlink(target, &typescript).unwrap();
            } else {
                std::fs::create_dir_all(typescript.join("lib")).unwrap();
                std::fs::create_dir_all(typescript.join("bin")).unwrap();
                std::fs::write(
                    typescript.join("package.json"),
                    format!(r#"{{"version": "{version}"}}"#),
                )
                .unwrap();
                if with_tsserver {
                    std::fs::write(typescript.join("lib/tsserver.js"), "").unwrap();
                } else {
                    executable(&typescript.join("bin/tsc"));
                }
            }
            let bin = outside.join("prefix/bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::os::unix::fs::symlink(&cli, bin.join("typescript-language-server")).unwrap();
            TypescriptInstall { bin }
        }

        fn typescript_with_path(install: &TypescriptInstall) -> LspServerConfig {
            let mut config = LspServerConfig::typescript();
            let path = std::env::join_paths([
                install.bin.as_path(),
                Path::new("/usr/bin"),
                Path::new("/bin"),
            ])
            .unwrap();
            config
                .env
                .insert("PATH".into(), path.to_string_lossy().into_owned());
            config
        }

        fn untrusted_allowing_typescript() -> WorkspaceTrust {
            WorkspaceTrust::untrusted([ServerId::from("typescript")])
        }

        fn plan_logging(config: &ServerConfig, roots: &WorkspaceRoots) -> (StartPlan, Vec<String>) {
            use tracing_subscriber::layer::SubscriberExt as _;

            let captured = crate::test_lsp::CapturedLogs::default();
            let guard = tracing::subscriber::set_default(
                tracing_subscriber::registry().with(captured.clone()),
            );
            let plan = plan(config, roots);
            drop(guard);
            (plan, captured.messages())
        }

        #[test]
        fn plan_admitted_typescript_entry_is_pinned_in_both_modes() {
            for trust in [WorkspaceTrust::Trusted, untrusted_allowing_typescript()] {
                let fx = fixture();
                typescript_workspace(&fx);
                let install = typescript_package(&fx.outside, None, "5.4.0", true);
                let tsserver = dunce::canonicalize(
                    fx.outside
                        .join("prefix/lib/node_modules/typescript/lib/tsserver.js"),
                )
                .unwrap();

                let plan = plan(
                    &config_with(vec![typescript_with_path(&install)], trust),
                    &fx.roots,
                );

                assert_eq!(
                    lsp::tsserver_pin::configured_tsserver_path(
                        plan.admitted[0]
                            .server_config()
                            .initialization_options
                            .as_ref()
                    ),
                    Some(tsserver)
                );
            }
        }

        #[test]
        fn plan_admitted_typescript_entry_selects_the_native_server() {
            let fx = fixture();
            typescript_workspace(&fx);
            let install = typescript_package(&fx.outside, None, "7.0.1", false);

            let (plan, logs) = plan_logging(
                &config_with(
                    vec![typescript_with_path(&install)],
                    untrusted_allowing_typescript(),
                ),
                &fx.roots,
            );

            let admitted = &plan.admitted[0].server_config();
            assert!(
                admitted.command.as_str().ends_with("typescript/bin/tsc"),
                "{}",
                admitted.command
            );
            assert_eq!(admitted.args, ["--lsp", "--stdio"]);
            assert!(
                logs.iter().any(|m| m.contains("native TypeScript server")),
                "{logs:?}"
            );
        }

        #[test]
        fn plan_refused_typescript_entry_never_reaches_selection() {
            let fx = fixture();
            typescript_workspace(&fx);
            let install = typescript_package(&fx.outside, None, "7.0.1", false);
            let configured = typescript_with_path(&install);

            let (plan, logs) = plan_logging(
                &config_with(vec![configured.clone()], WorkspaceTrust::untrusted([])),
                &fx.roots,
            );

            assert!(plan.admitted.is_empty());
            assert_eq!(plan.refused[0].failure.command, configured.command.as_str());
            assert!(
                !logs.iter().any(|m| m.contains("TypeScript server")
                    || m.contains("keeping typescript-language-server")),
                "selection ran for a refused server: {logs:?}"
            );
        }

        #[test]
        fn plan_untrusted_refuses_a_pin_that_lies_inside_the_workspace() {
            let fx = fixture();
            typescript_workspace(&fx);
            let ws_typescript = fx.workspace.join("ts");
            std::fs::create_dir_all(ws_typescript.join("lib")).unwrap();
            std::fs::write(ws_typescript.join("lib/tsserver.js"), "").unwrap();
            std::fs::write(
                ws_typescript.join("package.json"),
                r#"{"version": "5.4.0"}"#,
            )
            .unwrap();
            let install = typescript_package(&fx.outside, Some(&ws_typescript), "", true);

            let untrusted = plan(
                &config_with(
                    vec![typescript_with_path(&install)],
                    untrusted_allowing_typescript(),
                ),
                &fx.roots,
            );
            let trusted = plan(
                &config_with(
                    vec![typescript_with_path(&install)],
                    WorkspaceTrust::Trusted,
                ),
                &fx.roots,
            );

            assert!(untrusted.admitted.is_empty());
            std::assert_matches!(
                &untrusted.refused[0].failure.reason,
                StartupFailure::RefusedUntrustedWorkspace(
                    UntrustedRefusal::WorkspaceTsserver { tsserver }
                ) if tsserver.ends_with("ts/lib/tsserver.js")
            );
            assert_eq!(
                trusted.admitted.len(),
                1,
                "trusted mode keeps pinning with a warning"
            );
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod refusal_spawn_tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use tokio::sync::Mutex;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::bridge::{NotificationCache, Translator};
    use crate::config::{
        ServerCommand, ServerId, ServerStartConcurrency, ToolRouter, WorkspaceTrust,
    };
    use crate::mcp::SubscriptionRegistry;
    use crate::runtime::startup::spawn_lsp_servers_background;
    use crate::test_lsp::{answer_initialize_script, sh_script_init_config};

    struct Case {
        _dir: tempfile::TempDir,
        workspace: PathBuf,
        outside: PathBuf,
        marker: PathBuf,
        config: ServerConfig,
        roots: WorkspaceRoots,
    }

    fn executable(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn case_with(
        trust: WorkspaceTrust,
        server: impl FnOnce(&Path, &Path) -> LspServerConfig,
    ) -> Case {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dunce::canonicalize(dir.path()).unwrap();
        let (workspace, outside) = (base.join("ws"), base.join("outside"));
        std::fs::create_dir_all(workspace.join("bin")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(workspace.join("Cargo.toml"), "").unwrap();
        let marker = base.join("started");
        let server = server(&outside, &marker);
        let roots = WorkspaceRoots::from_configured(std::slice::from_ref(&workspace)).unwrap();
        Case {
            _dir: dir,
            workspace,
            outside,
            marker,
            config: ServerConfig {
                lsp_servers: vec![server],
                workspace_trust: trust,
                ..ServerConfig::default()
            },
            roots,
        }
    }

    fn case(trust: WorkspaceTrust) -> Case {
        case_with(trust, |outside, marker| {
            sh_script_init_config(outside, &answer_initialize_script(Some(marker), None))
                .server_config()
                .clone()
        })
    }

    struct Running {
        admitted: usize,
        refused: usize,
        task: JoinHandle<()>,
        cancel_tx: tokio::sync::watch::Sender<bool>,
    }

    fn run_plan(case: &Case) -> Running {
        let redactions = Arc::new(Redactions::for_servers(
            &case.config.lsp_servers,
            lsp::current_environment(),
        ));
        let plan = plan_server_starts(&case.config, &case.roots, &redactions);
        let refusals = plan.failures();
        let StartPlan { admitted, refused } = plan;
        let (admitted_count, refused_count) = (admitted.len(), refused.len());
        let router = ToolRouter::from_configs(
            admitted
                .iter()
                .map(ServerInitConfig::server_config)
                .chain(refused.iter().map(|r| &r.config)),
        )
        .unwrap();
        let translator = Arc::new(Translator::new().with_router(router));
        translator.record_refusals(&refusals);
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let task = spawn_lsp_servers_background(
            admitted,
            translator,
            Arc::new(Mutex::new(NotificationCache::new())),
            SubscriptionRegistry::new(),
            cancel_rx,
            case.roots.clone(),
            ServerStartConcurrency::DEFAULT,
        );
        Running {
            admitted: admitted_count,
            refused: refused_count,
            task,
            cancel_tx,
        }
    }

    impl Running {
        /// Whether the startup task ended by itself within the bound, then
        /// cancels and joins it so a wrongly admitted server cannot leak.
        async fn settled_within_bound(mut self) -> bool {
            let settled = tokio::time::timeout(std::time::Duration::from_secs(5), &mut self.task)
                .await
                .is_ok();
            self.cancel_tx.send_replace(true);
            if !settled {
                self.task.await.unwrap();
            }
            settled
        }
    }

    async fn marker_appears(marker: &Path) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !marker.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
    }

    #[tokio::test]
    async fn trusted_workspace_spawns_the_server() {
        let case = case(WorkspaceTrust::Trusted);

        let running = run_plan(&case);

        assert_eq!((running.admitted, running.refused), (1, 0));
        assert!(marker_appears(&case.marker).await);
        running.cancel_tx.send_replace(true);
        running.task.await.unwrap();
    }

    #[tokio::test]
    async fn untrusted_workspace_never_spawns_a_refused_server() {
        let case = case(WorkspaceTrust::untrusted([]));

        let running = run_plan(&case);
        let counts = (running.admitted, running.refused);
        let settled = running.settled_within_bound().await;

        assert_eq!(counts, (0, 1));
        assert!(settled, "a server was started");
        assert!(!case.marker.exists());
    }

    /// A server script outside the workspace whose `#!/usr/bin/env sh`
    /// interpreter resolves through a `PATH` that starts in the workspace.
    fn env_shebang_case(trust: WorkspaceTrust) -> (Case, PathBuf) {
        let case = case_with(trust, |outside, marker| {
            let script = outside.join("server.sh");
            let body = answer_initialize_script(Some(marker), None);
            executable(&script, &format!("#!/usr/bin/env sh\n{body}"));
            let mut config = LspServerConfig::rust_analyzer();
            config.command = ServerCommand::new(script.to_string_lossy().into_owned()).unwrap();
            config
        });
        let interpreter_marker = case.outside.join("interpreter-ran");
        executable(
            &case.workspace.join("bin/sh"),
            &format!(
                "#!/bin/sh\ntouch '{}'\nexit 1\n",
                interpreter_marker.display()
            ),
        );
        let mut case = case;
        let path = format!("{}:/usr/bin:/bin", case.workspace.join("bin").display());
        case.config.lsp_servers[0].env.insert("PATH".into(), path);
        (case, interpreter_marker)
    }

    #[tokio::test]
    async fn trusted_workspace_runs_a_path_interpreter_from_the_workspace() {
        let (case, interpreter_marker) = env_shebang_case(WorkspaceTrust::Trusted);

        let running = run_plan(&case);

        assert!(
            marker_appears(&interpreter_marker).await,
            "the control must reach the workspace sh"
        );
        running.cancel_tx.send_replace(true);
        running.task.await.unwrap();
    }

    #[tokio::test]
    async fn untrusted_workspace_never_runs_a_path_interpreter_from_the_workspace() {
        let (case, interpreter_marker) =
            env_shebang_case(WorkspaceTrust::untrusted([ServerId::from("rust")]));

        let running = run_plan(&case);
        assert_eq!((running.admitted, running.refused), (1, 0));
        let started = marker_appears(&case.marker).await;
        let interpreter_ran = interpreter_marker.exists();
        running.cancel_tx.send_replace(true);
        running.task.await.unwrap();

        assert!(started, "the server outside the workspace must start");
        assert!(!interpreter_ran, "the workspace interpreter was run");
    }
}
