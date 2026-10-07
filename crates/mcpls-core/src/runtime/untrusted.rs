//! Untrusted-workspace planning: which configured servers start, and in what
//! shape.
//!
//! [`plan_server_starts`] splits the applicable servers into those admitted
//! (hardened, in untrusted mode) and those refused, so a refused server never
//! becomes a [`ServerInitConfig`] and no startup, restart or respawn can reach
//! a spawn for it.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::info;

use crate::bridge::WorkspaceRoots;
use crate::config::{
    BuiltinServer, LspServerConfig, MarkerScan, ServerCommand, ServerConfig, WorkspaceTrust,
    login_home_dir,
};
use crate::error::{
    EchoedArgument, HomeVariable, LauncherRefusal, ResolvedItem, ServerSpawnFailure,
    StartupFailure, UnanalyzableLaunch, UntrustedRefusal,
};
use crate::lsp::command_path::{HostOs, ResolvedCommand};
use crate::lsp::tsserver_pin::UntrustedVetting;
use crate::lsp::{
    self, ChildWorkingDir, ManagedEnvVar, ParentEnv, ServerInitConfig, launcher, process_env,
    tsserver_pin,
};
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
    /// Splits the plan into the admitted servers, the configs of the refused
    /// ones (for routing) and the recorded refusals, in configuration order.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        Vec<ServerInitConfig>,
        Vec<LspServerConfig>,
        Vec<ServerSpawnFailure>,
    ) {
        let (configs, failures) = self
            .refused
            .into_iter()
            .map(|refused| (refused.config, refused.failure))
            .unzip();
        (self.admitted, configs, failures)
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

/// Refuses `configured` when its launcher lets the workspace choose the
/// program that runs (a package runner, task runner or toolchain wrapper), or
/// cannot be analyzed.
fn launcher_refusal(configured: &LspServerConfig) -> Option<UntrustedRefusal> {
    launcher::analyze_launch(configured.command.as_str(), &configured.args)
        .err()
        .map(|cause| project_launcher(configured, cause))
}

/// The refusal of `config`'s launcher for `cause`.
fn project_launcher(config: &LspServerConfig, cause: LauncherRefusal) -> UntrustedRefusal {
    UntrustedRefusal::ProjectLauncher {
        command: config.command.server_command().clone(),
        cause,
    }
}

/// The path `resolved` is spawned by, after untrusted mode vetted it.
///
/// Refuses an executable inside `boundary`. The spawn path is kept, not the
/// canonical one, because a program may act on the name it is started as;
/// the canonical path is used only when the directory it was found in is
/// itself workspace-controlled.
fn vetted_spawn_path(
    resolved: ResolvedCommand,
    boundary: &WorkspaceRoots,
) -> Result<String, UntrustedRefusal> {
    if boundary.contains_canonical(&resolved.canonical) {
        return Err(UntrustedRefusal::WorkspaceExecutable {
            executable: resolved.canonical,
        });
    }
    let in_workspace = resolved
        .spawn
        .parent()
        .is_none_or(|dir| boundary.contains_resolved_prefix(dir));
    let executable = if in_workspace {
        resolved.canonical
    } else {
        resolved.spawn
    };
    executable
        .into_os_string()
        .into_string()
        .map_err(|path| UntrustedRefusal::NonUtf8Path {
            what: ResolvedItem::Executable,
            path: PathBuf::from(path),
        })
}

/// The argument indices of the programs `effective`'s wrappers start, each
/// with the vetted absolute path that replaces it.
fn vetted_wrapped_programs(
    effective: &LspServerConfig,
    boundary: &WorkspaceRoots,
    parent_env: &dyn ParentEnv,
) -> Result<Vec<(usize, String)>, UntrustedRefusal> {
    let programs = launcher::analyze_launch(effective.command.as_str(), &effective.args)
        .map_err(|cause| project_launcher(effective, cause))?;
    programs
        .indices()
        .iter()
        .map(|&index| {
            let program = effective.args.get(index).map_or("", String::as_str);
            let unresolved = || UntrustedRefusal::UnresolvedWrappedProgram {
                program: EchoedArgument::name(program),
            };
            let resolved =
                lsp::command_path::resolve_named(Path::new(program), effective, parent_env)
                    .ok_or_else(unresolved)?;
            let path = vetted_spawn_path(resolved, boundary)?;
            if path.contains('=') {
                return Err(project_launcher(
                    effective,
                    LauncherRefusal::Unanalyzable {
                        program: EchoedArgument::name(program),
                        reason: UnanalyzableLaunch::PathContainsEquals,
                    },
                ));
            }
            Ok((index, path))
        })
        .collect()
}

/// `effective` pinned to what untrusted mode vetted: the executable resolved
/// to an absolute path, and a `PATH` without workspace, relative or empty
/// entries.
///
/// Spawning the resolved path, rather than the bare name, closes the window
/// between the check and a spawn (restart, respawn) in which a workspace
/// could add a binary. The sanitized `PATH` is what a `#!/usr/bin/env`
/// interpreter and the tools the server itself starts are looked up on. On
/// Windows the server is also told not to search the current directory for
/// executables it starts by name.
///
/// The programs an exec wrapper or `env` in the command line starts are vetted
/// the same way and replaced by their vetted absolute paths, so a wrapped
/// program cannot be swapped between the check and a spawn either.
///
/// Refuses when an executable is not found, or lies inside `boundary`;
/// naming the server in `--allow-server` is not consent to a binary the
/// workspace supplies.
fn harden_for_untrusted(
    effective: Cow<'_, LspServerConfig>,
    boundary: &WorkspaceRoots,
    login_home: Option<&Path>,
    parent_env: &dyn ParentEnv,
) -> Result<LspServerConfig, UntrustedRefusal> {
    let mut effective = effective.into_owned();
    let host = effective.env.host();
    let unresolved = || UntrustedRefusal::UnresolvedExecutable {
        command: effective.command.server_command().clone(),
    };
    let resolved =
        lsp::command_path::resolve_command(&effective, parent_env).ok_or_else(unresolved)?;
    let command = vetted_spawn_path(resolved, boundary)?;
    let wrapped = vetted_wrapped_programs(&effective, boundary, parent_env)?;
    let path =
        lsp::child_env_var(&effective, ManagedEnvVar::Path.name(), parent_env).unwrap_or_default();
    let path = lsp::command_path::or_system_path(
        host,
        lsp::command_path::path_outside(&path, boundary),
        parent_env,
    )
    .into_string()
    .map_err(|path| UntrustedRefusal::NonUtf8Path {
        what: ResolvedItem::SearchPath,
        path: PathBuf::from(path),
    })?;
    let home_env = home_overrides(&effective, boundary, login_home, parent_env)?;
    let hardened_command = ServerCommand::new(command).map_err(|_| unresolved())?;
    effective.command = effective
        .command
        .clone()
        .retarget(hardened_command.clone())
        .map_err(|cause| UntrustedRefusal::AutoSelectionTarget {
            executable: hardened_command,
            cause,
        })?;
    for (index, program) in wrapped {
        if let Some(arg) = effective.args.get_mut(index) {
            *arg = program;
        }
    }
    effective
        .env
        .insert(ManagedEnvVar::Path.name().to_owned(), path);
    for (name, value) in home_env {
        effective.env.insert(name, value);
    }
    if host == HostOs::Windows {
        effective.env.insert(
            ManagedEnvVar::NoDefaultCurrentDirectoryInExePath
                .name()
                .to_owned(),
            "1".to_owned(),
        );
    }
    Ok(effective)
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
    parent_env: &dyn ParentEnv,
) -> Result<Vec<(String, String)>, UntrustedRefusal> {
    let host = effective.env.host();
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
            if variable == HomeVariable::Home && host == HostOs::Other {
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

/// Where an untrusted server starts: the login home, else the temporary
/// directory, whichever lies outside `boundary`.
///
/// The server learns the workspace from `workspaceFolders`, not from its working
/// directory; starting in the checkout would let a name lookup or a relative
/// path resolve into it.
fn untrusted_working_dir(
    boundary: &WorkspaceRoots,
    login_home: Option<&Path>,
) -> Result<ChildWorkingDir, UntrustedRefusal> {
    working_dir_in(boundary, login_home, &std::env::temp_dir())
}

/// [`untrusted_working_dir`] with the temporary directory given.
///
/// The temporary directory is used only when no other user can write to it: a
/// shared one (`/tmp`) would let another user plant the files rustup, asdf and
/// similar look up from the working directory.
fn working_dir_in(
    boundary: &WorkspaceRoots,
    login_home: Option<&Path>,
    temp_dir: &Path,
) -> Result<ChildWorkingDir, UntrustedRefusal> {
    let private_temp = dunce::canonicalize(temp_dir)
        .ok()
        .filter(|dir| is_private_dir(dir));
    login_home
        .map(Path::to_path_buf)
        .into_iter()
        .chain(private_temp)
        .find(|dir| !boundary.contains_resolved_prefix(dir))
        .map(ChildWorkingDir::Fixed)
        .ok_or(UntrustedRefusal::NoSafeWorkingDirectory)
}

/// Whether neither the group nor others can write to `dir`. Every Windows
/// temporary directory is per-user, so only Unix is checked.
#[cfg(unix)]
fn is_private_dir(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(dir).is_ok_and(|meta| meta.permissions().mode() & 0o022 == 0)
}

#[cfg(not(unix))]
const fn is_private_dir(_dir: &Path) -> bool {
    true
}

/// Splits the configured servers into the ones worth starting for this run
/// and the ones untrusted-workspace mode refuses.
///
/// A server is applicable when its project markers are found under at least
/// one workspace root. A refused server gets no TypeScript selection and no
/// pin: nothing about it is inspected beyond the allowlist and its launcher,
/// except the executable checks of an allowed one.
///
/// Walks the file system, so async callers run it on the blocking pool.
pub fn plan_server_starts(
    config: &ServerConfig,
    roots: &WorkspaceRoots,
    redactions: &Arc<Redactions>,
) -> StartPlan {
    let mode = PlanMode::of(config, roots);
    let markers = MarkerScan::collect(
        roots.canonical(),
        &config.lsp_servers,
        config.workspace.heuristics_max_depth,
    );
    let mut plan = StartPlan::default();
    for lsp_config in &config.lsp_servers {
        if !markers.applies_to(lsp_config) {
            info!(
                "Skipping LSP server '{}' ({}): no project markers found",
                lsp_config.language_id, lsp_config.command
            );
            continue;
        }

        let admitted = admit(config, lsp_config, roots, &mode, redactions);
        match admitted {
            Ok(init) => plan.admitted.push(init),
            Err((command, refusal)) => plan.refused.push(refused(lsp_config, command, refusal)),
        }
    }
    plan
}

/// Whether a plan runs servers as configured or vets them for an untrusted
/// workspace.
enum PlanMode {
    Trusted,
    Untrusted {
        /// The directories no executable or working directory may lie in.
        boundary: WorkspaceRoots,
        /// The login home directory, when known.
        login_home: Option<PathBuf>,
    },
}

impl PlanMode {
    fn of(config: &ServerConfig, roots: &WorkspaceRoots) -> Self {
        if matches!(config.workspace_trust, WorkspaceTrust::Untrusted(_)) {
            let login_home = login_home_dir();
            let boundary = roots.untrusted_boundary(&config.workspace.roots, login_home.as_deref());
            Self::Untrusted {
                boundary,
                login_home,
            }
        } else {
            Self::Trusted
        }
    }
}

/// The init config for `lsp_config`, or the refusal and the command it names.
fn admit(
    config: &ServerConfig,
    lsp_config: &LspServerConfig,
    roots: &WorkspaceRoots,
    mode: &PlanMode,
    redactions: &Arc<Redactions>,
) -> Result<ServerInitConfig, (ServerCommand, UntrustedRefusal)> {
    let configured_command = lsp_config.command.server_command();
    let configured = |refusal| (configured_command.clone(), refusal);
    if let Some(refusal) = allowlist_refusal(&config.workspace_trust, lsp_config) {
        return Err(configured(refusal));
    }
    if matches!(mode, PlanMode::Untrusted { .. })
        && let Some(refusal) = launcher_refusal(lsp_config)
    {
        return Err(configured(refusal));
    }

    let selected = tsserver_pin::with_selected_typescript_server(lsp_config, roots, process_env);
    let (effective, working_dir, vetting) = match mode {
        PlanMode::Trusted => (selected.into_owned(), ChildWorkingDir::Inherit, None),
        PlanMode::Untrusted {
            boundary,
            login_home,
        } => {
            let login_home = login_home.as_deref();
            let hardened = harden_for_untrusted(selected, boundary, login_home, &process_env)
                .map_err(configured)?;
            let working_dir = untrusted_working_dir(boundary, login_home).map_err(configured)?;
            let vetting = UntrustedVetting::new(boundary.clone(), configured_command.clone());
            (hardened, working_dir, Some(vetting))
        }
    };
    let command = effective.command.server_command().clone();
    let plan = tsserver_pin::plan_typescript(effective, process_env);
    if let Some(vetting) = &vetting {
        plan.vet_untrusted(vetting)
            .map_err(|refusal| (command, refusal))?;
    }
    let (server_config, pinned) = plan.apply(roots);
    let init = ServerInitConfig::new(
        server_config,
        roots.clone(),
        config.workspace.position_encodings.clone(),
        Arc::clone(redactions),
    )
    .with_child_working_dir(working_dir);
    Ok(match pinned {
        Some(tsserver) => init.with_auto_pin(tsserver, vetting),
        None => init,
    })
}

/// A refusal of `configured`. The failure names `command`, the command that
/// would have been spawned (after TypeScript selection and hardening); the
/// routing config stays the one as configured.
fn refused(
    configured: &LspServerConfig,
    command: ServerCommand,
    refusal: UntrustedRefusal,
) -> RefusedServer {
    RefusedServer {
        config: configured.clone(),
        failure: ServerSpawnFailure {
            server_id: configured.id(),
            language_id: configured.language_id.clone(),
            command,
            reason: StartupFailure::RefusedUntrustedWorkspace(refusal),
        },
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use crate::config::{ServerCommand, ServerId};
    #[cfg(unix)]
    use crate::error::Error;

    fn rust_workspace() -> (tempfile::TempDir, WorkspaceRoots) {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();
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
    fn plan_skips_server_without_project_markers() {
        let dir = tempfile::TempDir::new().unwrap();
        let roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();

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

    fn mutually_exclusive_config() -> ServerConfig {
        ServerConfig::load_from(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/configs/mutually_exclusive_heuristics.toml"
        )))
        .expect("mutually exclusive heuristics must load, not just parse")
    }

    /// #174 §5/§12 (S3 regression): two servers for one language with mutually
    /// exclusive `heuristics.project_markers` still load, and only the one
    /// whose marker exists is planned and never collides with itself.
    #[test]
    fn plan_mutually_exclusive_heuristics_admit_only_the_matching_server() {
        let config = mutually_exclusive_config();
        assert_eq!(config.lsp_servers.len(), 2);
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("pyrightconfig.json"), "{}").unwrap();
        let roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();

        let plan = plan(&config, &roots);

        assert_eq!(plan.admitted.len(), 1);
        assert_eq!(
            plan.admitted[0].server_config().command,
            "pyright-langserver"
        );
        crate::config::ToolRouter::from_configs(
            plan.admitted
                .iter()
                .map(crate::lsp::ServerInitConfig::server_config),
        )
        .expect("a single applicable server must never be ambiguous");
    }

    /// #174 §5/§12 (S3 regression, other half): with both markers present both
    /// servers are planned and genuinely ambiguous, so the router rejects them.
    #[test]
    fn plan_mutually_exclusive_heuristics_both_present_are_ambiguous() {
        let config = mutually_exclusive_config();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("pyrightconfig.json"), "{}").unwrap();
        std::fs::write(dir.path().join("setup.cfg"), "").unwrap();
        let roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();

        let plan = plan(&config, &roots);

        assert_eq!(plan.admitted.len(), 2);
        let err = crate::config::ToolRouter::from_configs(
            plan.admitted
                .iter()
                .map(crate::lsp::ServerInitConfig::server_config),
        )
        .expect_err("two applicable nameless servers for one language must be ambiguous");
        std::assert_matches!(err, crate::error::Error::Config(_));
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
        assert_eq!(refused_ids(&plan), [ServerId::from_static("rust")]);
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
        go.command = ServerCommand::new(gopls.to_string_lossy().into_owned())
            .unwrap()
            .into();
        let config = config_with(
            vec![LspServerConfig::rust_analyzer(), go],
            WorkspaceTrust::untrusted([ServerId::from_static("go")]),
        );

        let plan = plan(&config, &roots);

        assert_eq!(admitted_ids(&plan), [ServerId::from_static("go")]);
        assert_eq!(refused_ids(&plan), [ServerId::from_static("rust")]);
    }

    #[test]
    fn plan_untrusted_marks_a_custom_server_as_not_builtin() {
        let (_dir, roots) = rust_workspace();
        let mut custom = LspServerConfig::rust_analyzer();
        custom.command = ServerCommand::from_static("my-rust-server").into();
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
        let roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();
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
        .unwrap()
        .into();

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
            let roots = WorkspaceRoots::from_paths(std::slice::from_ref(&workspace)).unwrap();
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
            config.command = ServerCommand::new(command.to_string()).unwrap().into();
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
            WorkspaceTrust::untrusted([ServerId::from_static("rust")])
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

            assert_eq!(admitted_ids(&plan), [ServerId::from_static("rust")]);
        }

        #[test]
        fn plan_admits_an_allowed_server_with_an_outside_executable() {
            let fx = fixture();
            executable(&fx.outside.join("rust-analyzer"));

            let plan = plan_allowing_rust(rust_with_path(&fx.outside), &fx);

            assert_eq!(admitted_ids(&plan), [ServerId::from_static("rust")]);
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
            assert_eq!(refused_ids(&plan), [ServerId::from_static("rust")]);
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

            assert_eq!(admitted_ids(&plan), [ServerId::from_static("rust")]);
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
            assert_eq!(admitted.env.get("PATH"), fx.outside.to_str());
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
                plan.admitted[0].server_config().env.get("HOME"),
                home.to_str()
            );
        }

        #[test]
        fn boundary_without_configured_roots_excludes_only_the_login_home() {
            let fx = fixture();
            let elsewhere = fx.outside.clone();

            let configured_roots =
                [crate::config::ConfiguredRoot::new(fx.workspace.clone()).unwrap()];
            let kept = fx.roots.untrusted_boundary(&[], Some(&elsewhere));
            let dropped = fx.roots.untrusted_boundary(&[], Some(&fx.workspace));
            let configured = fx
                .roots
                .untrusted_boundary(&configured_roots, Some(&fx.workspace));

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
            let boundary = WorkspaceRoots::from_paths(std::slice::from_ref(&fx.workspace)).unwrap();
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

            assert_eq!(hardened.env.get("HOME"), home.to_str());
            assert_eq!(hardened.env.get("USERPROFILE"), home.to_str());
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
            assert_eq!(known.env.get("HOME"), fx.outside.to_str());
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
            WorkspaceTrust::untrusted([ServerId::from_static("typescript")])
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

        fn host_executable(host: HostOs, fx: &Fixture) -> PathBuf {
            let name = if host == HostOs::Windows {
                "rust-analyzer.exe"
            } else {
                "rust-analyzer"
            };
            let exe = fx.outside.join(name);
            executable(&exe);
            exe
        }

        fn harden_on(
            host: HostOs,
            fx: &Fixture,
            env: &[(&str, &Path)],
        ) -> Result<LspServerConfig, UntrustedRefusal> {
            let exe = host_executable(host, fx);
            let env: Vec<(String, std::ffi::OsString)> = env
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.as_os_str().to_owned()))
                .collect();
            let parent_env = move |key: &str| {
                env.iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.clone())
            };
            let boundary = WorkspaceRoots::from_paths(std::slice::from_ref(&fx.workspace)).unwrap();
            let hardened = harden_for_untrusted(
                Cow::Owned(rust_on(host, exe.to_str().unwrap())),
                &boundary,
                Some(&fx.outside),
                &parent_env,
            )?;
            assert_eq!(hardened.env.host(), host);
            Ok(hardened)
        }

        fn rust_on(host: HostOs, command: &str) -> LspServerConfig {
            let mut config = rust_with(command);
            config.env = crate::config::ServerEnv::new(host);
            config
        }

        #[test]
        fn harden_on_windows_host_uses_the_windows_rules_on_any_build_target() {
            let fx = fixture();
            let inside_bin = fx.workspace.join("bin");
            std::fs::create_dir_all(&inside_bin).unwrap();
            let system_root = fx.outside.join("Win");
            let parent_env = {
                let (path, root) = (inside_bin, system_root.clone());
                move |key: &str| match key {
                    "PATH" => Some(path.clone().into_os_string()),
                    "SystemRoot" => Some(root.clone().into_os_string()),
                    _ => None,
                }
            };
            let exe = host_executable(HostOs::Windows, &fx);
            let boundary = WorkspaceRoots::from_paths(std::slice::from_ref(&fx.workspace)).unwrap();

            let hardened = harden_for_untrusted(
                Cow::Owned(rust_on(HostOs::Windows, exe.to_str().unwrap())),
                &boundary,
                None,
                &parent_env,
            )
            .unwrap();

            assert_eq!(hardened.env.host(), HostOs::Windows);
            assert_eq!(
                hardened.env.get("PATH"),
                system_root.join("System32").to_str()
            );
        }

        #[test]
        fn harden_unknown_home_is_refused_only_on_hosts_that_resolve_tilde() {
            let fx = fixture();
            let none = |_: &str| -> Option<std::ffi::OsString> { None };
            let exe = host_executable(HostOs::Windows, &fx);
            let boundary = WorkspaceRoots::from_paths(std::slice::from_ref(&fx.workspace)).unwrap();
            let on = |host, exe: &Path| {
                harden_for_untrusted(
                    Cow::Owned(rust_on(host, exe.to_str().unwrap())),
                    &boundary,
                    None,
                    &none,
                )
            };

            assert!(on(HostOs::Windows, &exe).is_ok());
            let unix_exe = host_executable(HostOs::Other, &fx);
            assert_eq!(
                on(HostOs::Other, &unix_exe).unwrap_err(),
                UntrustedRefusal::UnknownHome
            );
        }

        #[test]
        fn harden_refuses_an_auto_selected_command_that_resolves_to_another_program() {
            let fx = fixture();
            let other = fx.outside.join("some-other-tool");
            executable(&other);
            let bin = fx.workspace.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::os::unix::fs::symlink(&other, bin.join("typescript-language-server")).unwrap();
            let boundary = WorkspaceRoots::from_paths(std::slice::from_ref(&fx.workspace)).unwrap();
            let path = bin.into_os_string();
            let parent_env = move |key: &str| (key == "PATH").then(|| path.clone());

            let refused = harden_for_untrusted(
                Cow::Owned(LspServerConfig::typescript()),
                &boundary,
                Some(&fx.outside),
                &parent_env,
            )
            .unwrap_err();

            std::assert_matches!(
                refused,
                UntrustedRefusal::AutoSelectionTarget { executable, .. }
                    if executable.as_str().ends_with("some-other-tool")
            );
        }

        #[test]
        fn harden_on_windows_forbids_the_current_directory_in_executable_lookups() {
            let fx = fixture();
            let name = ManagedEnvVar::NoDefaultCurrentDirectoryInExePath.name();

            let windows = harden_on(HostOs::Windows, &fx, &[]).unwrap();
            let other = harden_on(HostOs::Other, &fx, &[]).unwrap();

            assert_eq!(windows.env.get(name), Some("1"));
            assert!(!other.env.contains_key(name));
        }

        #[test]
        fn harden_on_windows_overrides_a_configured_current_directory_setting() {
            let fx = fixture();
            let exe = host_executable(HostOs::Windows, &fx);
            let boundary = WorkspaceRoots::from_paths(std::slice::from_ref(&fx.workspace)).unwrap();
            let mut config = rust_on(HostOs::Windows, exe.to_str().unwrap());
            config.env.insert(
                "nodefaultcurrentdirectoryinexepath".to_owned(),
                "0".to_owned(),
            );

            let hardened = harden_for_untrusted(
                Cow::Owned(config),
                &boundary,
                Some(&fx.outside),
                &|_: &str| -> Option<std::ffi::OsString> { None },
            )
            .unwrap();
            assert_eq!(hardened.env.host(), HostOs::Windows);

            let name = ManagedEnvVar::NoDefaultCurrentDirectoryInExePath.name();
            assert_eq!(hardened.env.get(name), Some("1"));
            assert_eq!(
                hardened
                    .env
                    .iter()
                    .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                    .count(),
                1
            );
        }

        #[test]
        fn harden_refuses_a_search_path_that_is_not_utf8() {
            use std::os::unix::ffi::OsStrExt as _;

            let fx = fixture();
            let exe = fx.outside.join("rust-analyzer");
            executable(&exe);
            let non_utf8 = PathBuf::from(std::ffi::OsStr::from_bytes(b"/nonexistent/\xff"));
            let path = std::env::join_paths([fx.outside.clone(), non_utf8]).unwrap();
            let boundary = WorkspaceRoots::from_paths(std::slice::from_ref(&fx.workspace)).unwrap();

            let refused = harden_for_untrusted(
                Cow::Owned(rust_on(HostOs::Other, "rust-analyzer")),
                &boundary,
                Some(&fx.outside),
                &|key: &str| (key == "PATH").then(|| path.clone()),
            )
            .unwrap_err();

            std::assert_matches!(
                refused,
                UntrustedRefusal::NonUtf8Path { what: ResolvedItem::SearchPath, path }
                    if path.as_os_str().as_bytes().ends_with(b"\xff")
            );
        }

        #[test]
        fn working_dir_is_outside_the_boundary_and_prefers_the_login_home() {
            let fx = fixture();
            let home = fx.outside.join("home");
            std::fs::create_dir_all(&home).unwrap();

            let temp = private_temp(&fx);

            let with_home = working_dir_in(&fx.roots, Some(&home), &temp).unwrap();
            let without_home = working_dir_in(&fx.roots, None, &temp).unwrap();

            assert_eq!(with_home, ChildWorkingDir::Fixed(home));
            assert_eq!(
                without_home,
                ChildWorkingDir::Fixed(dunce::canonicalize(&temp).unwrap())
            );
        }

        #[test]
        fn working_dir_falls_back_to_the_temp_dir_when_the_login_home_is_inside() {
            let fx = fixture();
            let home = fx.workspace.join("home");
            std::fs::create_dir_all(&home).unwrap();

            let temp = private_temp(&fx);

            let dir = working_dir_in(&fx.roots, Some(&home), &temp).unwrap();

            assert_ne!(dir, ChildWorkingDir::Fixed(home));
        }

        fn temp_with_mode(fx: &Fixture, name: &str, mode: u32) -> PathBuf {
            use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

            let dir = fx.outside.join(name);
            std::fs::DirBuilder::new().mode(mode).create(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            dir
        }

        fn private_temp(fx: &Fixture) -> PathBuf {
            temp_with_mode(fx, "private-tmp", 0o700)
        }

        #[test]
        fn working_dir_is_refused_rather_than_a_directory_other_users_can_write() {
            let fx = fixture();
            for (name, mode) in [("shared", 0o1777), ("group", 0o770)] {
                let temp = temp_with_mode(&fx, name, mode);

                let refused = working_dir_in(&fx.roots, None, &temp).unwrap_err();

                assert_eq!(refused, UntrustedRefusal::NoSafeWorkingDirectory, "{name}");
            }
        }

        #[test]
        fn working_dir_is_refused_when_nothing_lies_outside_the_boundary() {
            let fx = fixture();
            let temp = private_temp(&fx);
            let boundary = WorkspaceRoots::from_paths(std::slice::from_ref(&temp)).unwrap();

            let refused = working_dir_in(&boundary, None, &temp).unwrap_err();

            assert_eq!(refused, UntrustedRefusal::NoSafeWorkingDirectory);
        }

        #[test]
        fn plan_starts_an_untrusted_server_outside_the_workspace_only() {
            let fx = fixture();
            let exe = fx.outside.join("rust-analyzer");
            executable(&exe);
            let config = rust_with(exe.to_str().unwrap());

            let untrusted = plan_allowing_rust(config.clone(), &fx);
            let trusted = plan(
                &config_with(vec![config], WorkspaceTrust::Trusted),
                &fx.roots,
            );

            let ChildWorkingDir::Fixed(dir) = untrusted.admitted[0].child_working_dir() else {
                panic!("an untrusted server must have a fixed working directory");
            };
            assert!(!fx.roots.contains_resolved_prefix(dir), "{dir:?}");
            assert_eq!(
                trusted.admitted[0].child_working_dir(),
                &ChildWorkingDir::Inherit
            );
        }

        fn refusal_of(plan: &StartPlan) -> Option<&UntrustedRefusal> {
            match &plan.refused.first()?.failure.reason {
                StartupFailure::RefusedUntrustedWorkspace(refusal) => Some(refusal),
                _ => None,
            }
        }

        fn rust_launched_by(command: &str, args: &[&str]) -> LspServerConfig {
            let mut config = rust_with(command);
            config.args = args.iter().map(ToString::to_string).collect();
            config
        }

        #[test]
        fn plan_untrusted_refuses_launchers_that_choose_the_server_from_the_workspace() {
            let fx = fixture();
            for (command, args) in [
                ("npx", &["rust-analyzer"][..]),
                ("make", &[][..]),
                ("cargo", &["run", "--bin", "server"][..]),
                ("env", &["FOO=1", "npx", "rust-analyzer"][..]),
                ("env", &["-S", "rust-analyzer --flag"][..]),
                ("sh", &["-c", "rust-analyzer"][..]),
                ("deno", &["run", "npm:rust-analyzer"][..]),
            ] {
                let plan = plan_allowing_rust(rust_launched_by(command, args), &fx);

                assert!(plan.admitted.is_empty(), "{command} {args:?}");
                std::assert_matches!(
                    refusal_of(&plan),
                    Some(UntrustedRefusal::ProjectLauncher { command: refused, .. })
                        if *refused == ServerCommand::new(command).unwrap(),
                    "{command} {args:?}"
                );
            }
        }

        /// A launch of `wrapper` (a fake binary in `fx.outside`) with `args`,
        /// looking programs up on `fx.outside` and the workspace `bin`.
        fn wrapped_launch(fx: &Fixture, wrapper: &str, args: &[&str]) -> LspServerConfig {
            let mut config = rust_launched_by(fx.outside.join(wrapper).to_str().unwrap(), args);
            let path = std::env::join_paths([&fx.outside, &fx.workspace.join("bin")]).unwrap();
            config
                .env
                .insert("PATH".into(), path.to_string_lossy().into_owned());
            config
        }

        fn outside_tools(fx: &Fixture, names: &[&str]) {
            for name in names {
                executable(&fx.outside.join(name));
            }
        }

        fn launched_args(plan: &StartPlan) -> Vec<String> {
            plan.admitted[0].server_config().args.clone()
        }

        #[test]
        fn plan_untrusted_refuses_a_wrapped_program_inside_the_workspace() {
            let fx = fixture();
            outside_tools(&fx, &["nice", "env", "timeout", "busybox"]);
            let srv = fx.workspace.join("bin/srv");
            executable(&srv);
            let srv = srv.to_str().unwrap();
            for (wrapper, args) in [
                ("nice", vec![srv]),
                ("nice", vec!["-n", "5", srv]),
                ("busybox", vec!["nice", srv]),
                ("env", vec![srv]),
                ("env", vec!["nice", "env", "FOO=1", "timeout", "5", srv]),
                ("timeout", vec!["5", "env", "nice", "busybox", "nice", srv]),
            ] {
                let plan = plan_allowing_rust(wrapped_launch(&fx, wrapper, &args), &fx);

                assert!(plan.admitted.is_empty(), "{wrapper} {args:?}");
                std::assert_matches!(
                    refusal_of(&plan),
                    Some(UntrustedRefusal::WorkspaceExecutable { executable })
                        if executable.ends_with("bin/srv"),
                    "{wrapper} {args:?}"
                );
            }
        }

        #[test]
        fn plan_untrusted_pins_a_wrapped_program_to_its_vetted_absolute_path() {
            let fx = fixture();
            outside_tools(&fx, &["nice", "env", "timeout", "srv"]);
            let outside = |name: &str| fx.outside.join(name).to_str().unwrap().to_owned();

            let plan =
                plan_allowing_rust(wrapped_launch(&fx, "nice", &["-n", "5", "srv", "-x"]), &fx);
            assert_eq!(launched_args(&plan), ["-n", "5", &outside("srv"), "-x"]);

            let plan = plan_allowing_rust(
                wrapped_launch(&fx, "nice", &["env", "FOO=1", "timeout", "5", "srv"]),
                &fx,
            );
            assert_eq!(
                launched_args(&plan),
                [
                    outside("env").as_str(),
                    "FOO=1",
                    &outside("timeout"),
                    "5",
                    &outside("srv")
                ]
            );
        }

        #[test]
        fn plan_untrusted_refuses_a_wrapped_program_when_the_workspace_shadows_it_on_path() {
            let fx = fixture();
            outside_tools(&fx, &["nice", "srv"]);
            executable(&fx.workspace.join("bin/srv"));
            let mut config = wrapped_launch(&fx, "nice", &["srv"]);
            let path = std::env::join_paths([&fx.workspace.join("bin"), &fx.outside]).unwrap();
            config
                .env
                .insert("PATH".into(), path.to_string_lossy().into_owned());

            let plan = plan_allowing_rust(config, &fx);

            std::assert_matches!(
                refusal_of(&plan),
                Some(UntrustedRefusal::WorkspaceExecutable { .. })
            );
        }

        #[test]
        fn plan_untrusted_refuses_a_wrapped_program_whose_path_env_would_read_as_an_assignment() {
            let fx = fixture();
            outside_tools(&fx, &["env"]);
            let odd = fx.outside.join("a=b");
            std::fs::create_dir_all(&odd).unwrap();
            executable(&odd.join("srv"));
            let mut config = wrapped_launch(&fx, "env", &["srv"]);
            let path = std::env::join_paths([&odd, &fx.outside]).unwrap();
            config
                .env
                .insert("PATH".into(), path.to_string_lossy().into_owned());

            let plan = plan_allowing_rust(config, &fx);

            std::assert_matches!(
                refusal_of(&plan),
                Some(UntrustedRefusal::ProjectLauncher {
                    cause: LauncherRefusal::Unanalyzable {
                        reason: UnanalyzableLaunch::PathContainsEquals,
                        ..
                    },
                    ..
                })
            );
        }

        #[test]
        fn plan_untrusted_refuses_a_wrapped_program_that_is_not_found() {
            let fx = fixture();
            outside_tools(&fx, &["nice"]);

            let plan = plan_allowing_rust(wrapped_launch(&fx, "nice", &["missing-srv"]), &fx);

            assert_eq!(
                refusal_of(&plan),
                Some(&UntrustedRefusal::UnresolvedWrappedProgram {
                    program: EchoedArgument::name("missing-srv")
                })
            );
        }

        #[test]
        fn plan_untrusted_does_not_echo_an_assignment_value_of_a_missing_program() {
            let fx = fixture();
            outside_tools(&fx, &["timeout"]);

            let plan = plan_allowing_rust(
                wrapped_launch(&fx, "timeout", &["5", "API_TOKEN=abc123", "srv"]),
                &fx,
            );

            let Some(refusal) = refusal_of(&plan) else {
                panic!("expected a refusal");
            };
            assert!(!refusal.to_string().contains("abc123"), "{refusal}");
        }

        #[test]
        fn plan_untrusted_refuses_wrapper_shapes_it_cannot_analyze() {
            let fx = fixture();
            outside_tools(&fx, &["timeout", "nice", "sudo", "srv"]);
            for (wrapper, args) in [
                ("timeout", &["srv", "x"][..]),
                ("nice", &["-5", "srv"]),
                ("sudo", &["-u", "x", "nice", "srv"]),
            ] {
                let plan = plan_allowing_rust(wrapped_launch(&fx, wrapper, args), &fx);

                assert!(plan.admitted.is_empty(), "{wrapper} {args:?}");
                std::assert_matches!(
                    refusal_of(&plan),
                    Some(UntrustedRefusal::ProjectLauncher { .. }),
                    "{wrapper} {args:?}"
                );
            }
        }

        #[test]
        fn plan_trusted_leaves_wrapper_arguments_alone() {
            let fx = fixture();
            outside_tools(&fx, &["nice", "srv"]);

            let plan = plan(
                &config_with(
                    vec![wrapped_launch(&fx, "nice", &["srv"])],
                    WorkspaceTrust::Trusted,
                ),
                &fx.roots,
            );

            assert_eq!(launched_args(&plan), ["srv"]);
        }

        #[test]
        fn plan_trusted_admits_launchers_that_choose_the_server_from_the_workspace() {
            let fx = fixture();

            let plan = plan(
                &config_with(
                    vec![rust_launched_by("npx", &["rust-analyzer"])],
                    WorkspaceTrust::Trusted,
                ),
                &fx.roots,
            );

            assert_eq!(admitted_ids(&plan), [ServerId::from_static("rust")]);
        }

        #[test]
        fn plan_untrusted_admits_deno_lsp_and_a_direct_server() {
            let fx = fixture();
            for (name, args) in [("deno", &["lsp"][..]), ("rust-analyzer", &[][..])] {
                let exe = fx.outside.join(name);
                executable(&exe);

                let plan = plan_allowing_rust(rust_launched_by(exe.to_str().unwrap(), args), &fx);

                assert_eq!(
                    admitted_ids(&plan),
                    [ServerId::from_static("rust")],
                    "{name}"
                );
            }
        }

        fn typescript_shim(fx: &Fixture, dir: &str) -> LspServerConfig {
            let shim = fx.outside.join(dir).join("typescript-language-server");
            std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
            executable(&shim);
            let mut config = LspServerConfig::typescript();
            config.command = ServerCommand::new(shim.to_str().unwrap().to_owned())
                .unwrap()
                .into();
            config
        }

        #[test]
        fn plan_untrusted_refuses_a_typescript_shim_it_cannot_pin() {
            let fx = fixture();
            typescript_workspace(&fx);
            let mut with_user_pin = typescript_shim(&fx, ".volta/bin");
            with_user_pin.initialization_options =
                Some(serde_json::json!({"tsserver": {"path": "/opt/ts/tsserver.js"}}));
            for config in [typescript_shim(&fx, ".volta/bin"), with_user_pin] {
                let command = config.command.server_command().clone();

                let plan = plan(
                    &config_with(vec![config], untrusted_allowing_typescript()),
                    &fx.roots,
                );

                assert!(plan.admitted.is_empty());
                assert_eq!(
                    refusal_of(&plan),
                    Some(&UntrustedRefusal::UnpinnedTypescriptLauncher { command })
                );
            }
        }

        #[test]
        fn plan_trusted_admits_a_typescript_shim_it_cannot_pin() {
            let fx = fixture();
            typescript_workspace(&fx);

            let plan = plan(
                &config_with(
                    vec![typescript_shim(&fx, ".volta/bin")],
                    WorkspaceTrust::Trusted,
                ),
                &fx.roots,
            );

            assert_eq!(admitted_ids(&plan), [ServerId::from_static("typescript")]);
            assert_eq!(plan.admitted[0].pinned_tsserver(), None);
        }

        #[test]
        fn plan_untrusted_refuses_a_typescript_script_named_by_a_relative_path() {
            let fx = fixture();
            typescript_workspace(&fx);
            let node = fx.outside.join("node");
            executable(&node);
            let mut config = LspServerConfig::typescript();
            config.command = ServerCommand::new(node.to_str().unwrap().to_owned())
                .unwrap()
                .into();
            config.args = vec!["node_modules/typescript-language-server/lib/cli.mjs".to_owned()];

            let plan = plan(
                &config_with(vec![config], untrusted_allowing_typescript()),
                &fx.roots,
            );

            std::assert_matches!(
                refusal_of(&plan),
                Some(UntrustedRefusal::UnpinnedTypescriptLauncher { .. })
            );
        }

        #[test]
        fn plan_pins_the_canonical_path_it_checked() {
            let fx = fixture();
            typescript_workspace(&fx);
            let real = fx.outside.join("real-typescript");
            std::fs::create_dir_all(real.join("lib")).unwrap();
            std::fs::write(real.join("lib/tsserver.js"), "").unwrap();
            std::fs::write(real.join("package.json"), r#"{"version": "5.4.0"}"#).unwrap();
            let install = typescript_package(&fx.outside, Some(&real), "", true);
            for trust in [WorkspaceTrust::Trusted, untrusted_allowing_typescript()] {
                let plan = plan(
                    &config_with(vec![typescript_with_path(&install)], trust),
                    &fx.roots,
                );

                assert_eq!(
                    plan.admitted[0].pinned_tsserver(),
                    Some(real.join("lib/tsserver.js"))
                );
            }
        }

        struct Respawnable {
            fx: Fixture,
            modules: PathBuf,
            init: ServerInitConfig,
        }

        fn planned_typescript(trust: WorkspaceTrust) -> Respawnable {
            let fx = fixture();
            typescript_workspace(&fx);
            let install = typescript_package(&fx.outside, None, "5.4.0", true);
            let plan = plan(
                &config_with(vec![typescript_with_path(&install)], trust),
                &fx.roots,
            );
            let init = plan.admitted.into_iter().next().unwrap();
            let modules = fx.outside.join("prefix/lib/node_modules");
            Respawnable { fx, modules, init }
        }

        fn typescript_install_at(dir: &Path) {
            std::fs::create_dir_all(dir.join("lib")).unwrap();
            std::fs::write(dir.join("lib/tsserver.js"), "").unwrap();
            std::fs::write(dir.join("package.json"), r#"{"version": "5.5.0"}"#).unwrap();
        }

        #[test]
        fn respawn_keeps_a_pin_that_still_exists() {
            let case = planned_typescript(WorkspaceTrust::Trusted);

            let respawn = case.init.plan_respawn().unwrap();

            assert_eq!(respawn.pinned_tsserver(), case.init.pinned_tsserver());
        }

        #[test]
        fn respawn_resolves_a_vanished_pin_again() {
            let case = planned_typescript(WorkspaceTrust::Trusted);
            std::fs::remove_dir_all(case.modules.join("typescript")).unwrap();
            let moved = case.fx.outside.join("prefix/node_modules/typescript");
            typescript_install_at(&moved);

            let respawn = case.init.plan_respawn().unwrap();

            assert_eq!(
                respawn.pinned_tsserver(),
                Some(dunce::canonicalize(moved.join("lib/tsserver.js")).unwrap())
            );
        }

        #[test]
        fn respawn_drops_a_vanished_pin_nothing_replaces() {
            let case = planned_typescript(WorkspaceTrust::Trusted);
            std::fs::remove_dir_all(case.modules.join("typescript")).unwrap();

            let respawn = case.init.plan_respawn().unwrap();

            assert_eq!(respawn.pinned_tsserver(), None);
        }

        #[test]
        fn respawn_never_second_guesses_a_pin_the_user_configured() {
            let fx = fixture();
            typescript_workspace(&fx);
            let install = typescript_package(&fx.outside, None, "5.4.0", true);
            let mut config = typescript_with_path(&install);
            let missing = fx.outside.join("gone/tsserver.js");
            config.initialization_options =
                Some(serde_json::json!({"tsserver": {"path": missing}}));
            let init = plan(
                &config_with(vec![config], WorkspaceTrust::Trusted),
                &fx.roots,
            )
            .admitted
            .into_iter()
            .next()
            .unwrap();

            let respawn = init.plan_respawn().unwrap();

            assert_eq!(respawn.pinned_tsserver(), Some(missing));
        }

        #[test]
        fn respawn_untrusted_refuses_a_pin_that_now_lies_inside_the_workspace() {
            let case = planned_typescript(untrusted_allowing_typescript());
            let inside = case.fx.workspace.join("ts");
            typescript_install_at(&inside);
            let typescript = case.modules.join("typescript");
            std::fs::remove_dir_all(&typescript).unwrap();
            std::os::unix::fs::symlink(&inside, &typescript).unwrap();

            let refused = case.init.plan_respawn().unwrap_err();

            let Error::ServerFailedToStart(failure) = refused else {
                panic!("expected a refusal, got {refused:?}");
            };
            std::assert_matches!(
                failure.reason,
                StartupFailure::RefusedUntrustedWorkspace(UntrustedRefusal::WorkspaceTsserver {
                    ref tsserver
                }) if tsserver.ends_with("ts/lib/tsserver.js")
            );
        }

        /// Startup refuses a launcher no tsserver can be pinned for; so does a
        /// respawn whose re-resolution now finds one (the install became a shim).
        #[test]
        fn respawn_untrusted_refuses_a_launcher_that_can_no_longer_be_pinned() {
            let case = planned_typescript(untrusted_allowing_typescript());
            std::fs::remove_dir_all(case.modules.join("typescript")).unwrap();
            let launcher = case
                .fx
                .outside
                .join("prefix/bin/typescript-language-server");
            std::fs::remove_file(&launcher).unwrap();
            executable(&launcher);

            let refused = case.init.plan_respawn().unwrap_err();

            let Error::ServerFailedToStart(failure) = refused else {
                panic!("expected a refusal, got {refused:?}");
            };
            std::assert_matches!(
                failure.reason,
                StartupFailure::RefusedUntrustedWorkspace(
                    UntrustedRefusal::UnpinnedTypescriptLauncher { .. }
                )
            );
        }

        #[test]
        fn respawn_trusted_keeps_going_when_the_launcher_can_no_longer_be_pinned() {
            let case = planned_typescript(WorkspaceTrust::Trusted);
            std::fs::remove_dir_all(case.modules.join("typescript")).unwrap();
            let launcher = case
                .fx
                .outside
                .join("prefix/bin/typescript-language-server");
            std::fs::remove_file(&launcher).unwrap();
            executable(&launcher);

            let respawn = case.init.plan_respawn().unwrap();

            assert_eq!(respawn.pinned_tsserver(), None);
        }

        #[test]
        fn respawn_trusted_accepts_a_pin_that_now_lies_inside_the_workspace() {
            let case = planned_typescript(WorkspaceTrust::Trusted);
            let inside = case.fx.workspace.join("ts");
            typescript_install_at(&inside);
            let typescript = case.modules.join("typescript");
            std::fs::remove_dir_all(&typescript).unwrap();
            std::os::unix::fs::symlink(&inside, &typescript).unwrap();

            let respawn = case.init.plan_respawn().unwrap();

            assert!(respawn.pinned_tsserver().is_some());
        }

        #[test]
        fn into_parts_keeps_configuration_order() {
            let fx = fixture();
            let config = config_with(
                vec![LspServerConfig::rust_analyzer(), LspServerConfig::gopls()],
                WorkspaceTrust::untrusted([ServerId::from_static("rust")]),
            );
            let exe = fx.outside.join("rust-analyzer");
            executable(&exe);
            let mut config = config;
            config.lsp_servers[0].command = ServerCommand::new(exe.to_str().unwrap().to_owned())
                .unwrap()
                .into();
            std::fs::write(fx.workspace.join("go.mod"), "").unwrap();

            let (admitted, refused, failures) = plan(&config, &fx.roots).into_parts();

            assert_eq!(admitted.len(), 1);
            assert_eq!(
                refused.iter().map(LspServerConfig::id).collect::<Vec<_>>(),
                [ServerId::from_static("go")]
            );
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].server_id, ServerId::from_static("go"));
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
    use tokio_util::sync::CancellationToken;

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
        let roots = WorkspaceRoots::from_paths(std::slice::from_ref(&workspace)).unwrap();
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
        cancel: CancellationToken,
    }

    fn run_plan(case: &Case) -> Running {
        let redactions = Arc::new(Redactions::for_servers(
            &case.config.lsp_servers,
            lsp::current_environment(),
        ));
        let plan = plan_server_starts(&case.config, &case.roots, &redactions);
        let (admitted, refused, refusals) = plan.into_parts();
        let (admitted_count, refused_count) = (admitted.len(), refused.len());
        let router = ToolRouter::from_configs(
            admitted
                .iter()
                .map(ServerInitConfig::server_config)
                .chain(&refused),
        )
        .unwrap();
        let translator = Arc::new(Translator::new().with_router(router));
        translator.record_refusals(&refusals);
        let cancel = CancellationToken::new();
        let task = spawn_lsp_servers_background(
            admitted,
            translator,
            Arc::new(Mutex::new(NotificationCache::new())),
            SubscriptionRegistry::new(),
            cancel.clone(),
            case.roots.clone(),
            ServerStartConcurrency::DEFAULT,
        );
        Running {
            admitted: admitted_count,
            refused: refused_count,
            task,
            cancel,
        }
    }

    impl Running {
        /// Whether the startup task ended by itself within the bound, then
        /// cancels and joins it so a wrongly admitted server cannot leak.
        async fn settled_within_bound(mut self) -> bool {
            let settled = tokio::time::timeout(std::time::Duration::from_secs(5), &mut self.task)
                .await
                .is_ok();
            self.cancel.cancel();
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
        running.cancel.cancel();
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
            config.command = ServerCommand::new(script.to_string_lossy().into_owned())
                .unwrap()
                .into();
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
        running.cancel.cancel();
        running.task.await.unwrap();
    }

    #[tokio::test]
    async fn untrusted_workspace_never_runs_a_path_interpreter_from_the_workspace() {
        let (case, interpreter_marker) =
            env_shebang_case(WorkspaceTrust::untrusted([ServerId::from_static("rust")]));

        let running = run_plan(&case);
        assert_eq!((running.admitted, running.refused), (1, 0));
        let started = marker_appears(&case.marker).await;
        let interpreter_ran = interpreter_marker.exists();
        running.cancel.cancel();
        running.task.await.unwrap();

        assert!(started, "the server outside the workspace must start");
        assert!(!interpreter_ran, "the workspace interpreter was run");
    }
}

#[cfg(test)]
#[cfg(windows)]
mod windows_spawn_tests {
    use super::*;
    use crate::config::ServerId;
    use crate::lsp::LspServer;

    /// A `.cmd` server outside the workspace records the working directory
    /// and the current-directory lookup switch it was started with.
    #[tokio::test]
    async fn untrusted_server_starts_outside_the_workspace_with_the_lookup_switch_set() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dunce::canonicalize(dir.path()).unwrap();
        let (workspace, outside) = (base.join("ws"), base.join("outside"));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(workspace.join("Cargo.toml"), "").unwrap();
        let record = base.join("record.txt");
        let script = outside.join("server.cmd");
        std::fs::write(
            &script,
            format!(
                "@echo off\r\necho %CD% > \"{0}\"\r\necho %NoDefaultCurrentDirectoryInExePath% >> \"{0}\"\r\n",
                record.display()
            ),
        )
        .unwrap();
        let mut server = LspServerConfig::rust_analyzer();
        server.command = ServerCommand::new(script.to_string_lossy().into_owned())
            .unwrap()
            .into();
        let config = ServerConfig {
            lsp_servers: vec![server],
            workspace_trust: WorkspaceTrust::untrusted([ServerId::from_static("rust")]),
            ..ServerConfig::default()
        };
        let roots = WorkspaceRoots::from_paths(std::slice::from_ref(&workspace)).unwrap();
        let redactions = Arc::new(Redactions::for_servers(
            &config.lsp_servers,
            lsp::current_environment(),
        ));
        let init = plan_server_starts(&config, &roots, &redactions)
            .admitted
            .into_iter()
            .next()
            .expect("the server is admitted");
        let ChildWorkingDir::Fixed(expected) = init.child_working_dir().clone() else {
            panic!("an untrusted server must have a fixed working directory");
        };

        let spawned = LspServer::spawn(init).await;

        assert!(spawned.is_err(), "the script never answers initialize");
        let recorded = std::fs::read_to_string(&record).unwrap();
        let mut lines = recorded.lines().map(str::trim);
        let cwd = lines.next().unwrap();
        assert!(
            cwd.eq_ignore_ascii_case(&expected.to_string_lossy()),
            "{cwd} != {}",
            expected.display()
        );
        assert!(!roots.contains_resolved_prefix(Path::new(cwd)), "{cwd}");
        assert_eq!(lines.next(), Some("1"));
    }
}
