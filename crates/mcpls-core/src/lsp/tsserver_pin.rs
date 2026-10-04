//! Pins `typescript-language-server` to the tsserver it would bundle.
//!
//! The server picks a TypeScript compiler service in the order user setting,
//! workspace, bundled, so an analyzed checkout can run its own
//! `node_modules/typescript/lib/tsserver.js` (#566). Passing the bundled
//! tsserver as `initializationOptions.tsserver.path` removes the workspace
//! from that choice. Resolution mirrors node's own lookup for the server's
//! `typescript` dependency, starting from the server's package directory.
//!
//! Only symlink installs are covered (npm, nvm, bun, Homebrew with a global
//! `typescript` peer). Windows `.cmd` shims and script launchers (pnpm,
//! Volta, asdf/mise) stay unresolved and are reported with a warning, never
//! with a startup failure.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::LspServerConfig;
use crate::lsp::{LspNotification, child_env_var};

const SERVER_STEM: &str = "typescript-language-server";
const NODE_MODULES: &str = "node_modules";
const TSSERVER_RELATIVE: &str = "node_modules/typescript/lib/tsserver.js";
/// Method of the notification the server sends after `initialized`.
const TYPESCRIPT_VERSION_METHOD: &str = "$/typescriptVersion";

/// Why no out-of-workspace tsserver could be pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedReason {
    /// The server executable was not found on the child's effective `PATH`.
    ServerNotOnPath,
    /// A Windows shim or script launcher, whose package directory is not
    /// reachable through the executable.
    UnsupportedLauncher,
    /// No valid `typescript` package (with a `package.json` `version`) is
    /// installed next to the server package.
    NoTypescriptNextToServer,
}

impl UnresolvedReason {
    const fn describe(self) -> &'static str {
        match self {
            Self::ServerNotOnPath => "typescript-language-server was not found on PATH",
            Self::UnsupportedLauncher => {
                "typescript-language-server is started through an unsupported launcher or shim"
            }
            Self::NoTypescriptNextToServer => {
                "no valid typescript package is installed next to typescript-language-server"
            }
        }
    }
}

/// Outcome of resolving the tsserver to pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TsserverResolution {
    /// Absolute path of the bundled `tsserver.js`.
    Pinned(PathBuf),
    /// No pin was possible; the server falls back to its own resolution.
    Unresolved(UnresolvedReason),
}

#[derive(Debug, Serialize)]
struct TsserverPath<'a> {
    path: &'a Path,
}

/// The only `initializationOptions` mcpls generates for the TypeScript server.
#[derive(Debug, Serialize)]
struct TsserverInitOptions<'a> {
    tsserver: TsserverPath<'a>,
}

/// Where the server says it took its tsserver from, as reported by
/// `$/typescriptVersion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TsserverSource {
    /// The configured `tsserver.path`.
    UserSetting,
    /// A `typescript` install found in the workspace.
    Workspace,
    /// The `typescript` install next to the server.
    Bundled,
    /// A source added by a newer server release.
    #[serde(other)]
    Unknown,
}

/// Parameters of `$/typescriptVersion`.
#[derive(Debug, Deserialize)]
pub struct TypescriptVersionParams {
    /// Display name of the TypeScript API version in use.
    pub version: String,
    /// Where the server took that TypeScript from.
    pub source: TsserverSource,
}

fn is_typescript_language_server(command: &str) -> bool {
    Path::new(command)
        .file_stem()
        .is_some_and(|stem| stem == SERVER_STEM)
}

/// Whether `arg` names the server package or script, as in `npx
/// typescript-language-server@5` or `node .../typescript-language-server/lib/cli.mjs`.
fn mentions_server(arg: &str) -> bool {
    Path::new(arg)
        .components()
        .any(|part| part.as_os_str().to_string_lossy().starts_with(SERVER_STEM))
}

/// The `package.json` field typescript-language-server requires of a
/// `typescript` install before it accepts that install's tsserver.
#[derive(Deserialize)]
struct PackageManifest {
    #[allow(dead_code)] // Parsing fails without the field, which is the validity check.
    version: String,
}

fn is_valid_typescript_install(tsserver: &Path) -> bool {
    tsserver
        .parent()
        .and_then(Path::parent)
        .map(|package| package.join("package.json"))
        .and_then(|manifest| std::fs::read(manifest).ok())
        .is_some_and(|bytes| serde_json::from_slice::<PackageManifest>(&bytes).is_ok())
}

fn is_windows_launcher(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        ["cmd", "bat", "ps1"]
            .iter()
            .any(|launcher| ext.eq_ignore_ascii_case(launcher))
    })
}

fn find_executable(command: &Path, path_var: Option<&OsString>) -> Option<PathBuf> {
    if command.components().count() > 1 {
        return command.is_file().then(|| command.to_path_buf());
    }
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
}

fn package_dir_of(executable: &Path) -> Option<PathBuf> {
    let real = dunce::canonicalize(executable).ok()?;
    real.ancestors()
        .find(|dir| {
            dir.file_name().is_some_and(|name| name == SERVER_STEM)
                && dir.join("package.json").is_file()
        })
        .map(Path::to_path_buf)
}

/// Node's lookup of the `typescript` dependency: `node_modules` of every
/// ancestor of the package directory, skipping ancestors that are themselves
/// named `node_modules`.
///
/// The first match decides, as in node. It must also be a valid install
/// (`package.json` with a `version`), because the server ignores an invalid
/// one and silently falls through to the workspace's tsserver.
fn bundled_tsserver(package_dir: &Path) -> Option<PathBuf> {
    package_dir
        .ancestors()
        .filter(|dir| dir.file_name().is_none_or(|name| name != NODE_MODULES))
        .map(|dir| dir.join(TSSERVER_RELATIVE))
        .find(|candidate| candidate.is_file())
        .filter(|tsserver| is_valid_typescript_install(tsserver))
}

/// Resolves the tsserver `config`'s server would bundle, or `None` when
/// `config` does not launch typescript-language-server.
///
/// A launcher that only names the server in its arguments (`npx`, `bunx`,
/// `node cli.mjs`, wrappers) is `UnsupportedLauncher`, never silently
/// unrelated.
///
/// `PATH` is read as the child sees it: the config's `env` override, else
/// `parent_env`.
// TODO(#604): pin npx/bunx/node launchers, .cmd and script shims (pnpm, Volta, asdf/mise)
pub fn resolve(
    config: &LspServerConfig,
    parent_env: impl Fn(&str) -> Option<OsString>,
) -> Option<TsserverResolution> {
    if !is_typescript_language_server(&config.command) {
        return config
            .args
            .iter()
            .any(|arg| mentions_server(arg))
            .then_some(TsserverResolution::Unresolved(
                UnresolvedReason::UnsupportedLauncher,
            ));
    }
    let command = Path::new(&config.command);
    if is_windows_launcher(command) {
        return Some(TsserverResolution::Unresolved(
            UnresolvedReason::UnsupportedLauncher,
        ));
    }
    let path_var = child_env_var(config, "PATH", parent_env);
    let Some(executable) = find_executable(command, path_var.as_ref()) else {
        return Some(TsserverResolution::Unresolved(
            UnresolvedReason::ServerNotOnPath,
        ));
    };
    let Some(package_dir) = package_dir_of(&executable) else {
        return Some(TsserverResolution::Unresolved(
            UnresolvedReason::UnsupportedLauncher,
        ));
    };
    Some(bundled_tsserver(&package_dir).map_or(
        TsserverResolution::Unresolved(UnresolvedReason::NoTypescriptNextToServer),
        TsserverResolution::Pinned,
    ))
}

fn has_user_tsserver_path(options: &serde_json::Value) -> bool {
    options.pointer("/tsserver/path").is_some()
}

/// The `initialization_options` to send for `config`, with the bundled
/// tsserver pinned when `config` launches typescript-language-server.
///
/// A user-supplied `tsserver.path` always wins. User options without one are
/// left untouched, because merging would silently change their meaning; the
/// skipped pin is logged. Failure to resolve a tsserver never prevents the
/// server from starting.
pub fn pinned_initialization_options(
    config: &LspServerConfig,
    workspace_roots: &[PathBuf],
    parent_env: impl Fn(&str) -> Option<OsString>,
) -> Option<serde_json::Value> {
    let user = config.initialization_options.clone();
    if user.as_ref().is_some_and(has_user_tsserver_path) {
        return user;
    }
    let Some(resolution) = resolve(config, parent_env) else {
        return user;
    };
    if user.is_some() {
        tracing::warn!(
            server = %config.language_id,
            "tsserver pin skipped: initialization_options set without tsserver.path, \
             so a workspace-supplied tsserver may run"
        );
        return user;
    }
    match resolution {
        TsserverResolution::Pinned(tsserver) => {
            if workspace_roots
                .iter()
                .any(|root| tsserver.starts_with(root))
            {
                tracing::warn!(
                    server = %config.language_id,
                    tsserver = %tsserver.display(),
                    "typescript-language-server is installed inside the workspace; \
                     pinning it does not make the workspace trusted"
                );
            }
            serde_json::to_value(TsserverInitOptions {
                tsserver: TsserverPath { path: &tsserver },
            })
            .inspect_err(|err| tracing::warn!(%err, "tsserver pin could not be serialized"))
            .ok()
        }
        TsserverResolution::Unresolved(reason) => {
            tracing::warn!(
                server = %config.language_id,
                "tsserver not pinned: {}; a workspace-supplied tsserver may run",
                reason.describe()
            );
            None
        }
    }
}

/// The `tsserver.path` configured in `options`, if any.
pub fn configured_tsserver_path(options: Option<&serde_json::Value>) -> Option<PathBuf> {
    options?
        .pointer("/tsserver/path")?
        .as_str()
        .map(PathBuf::from)
}

/// Warns when a server configured with a tsserver path reports another source
/// in its `$/typescriptVersion` notification; other notifications are ignored.
///
/// Catches a stale pin on respawn, where the server silently falls through to
/// the workspace's tsserver.
pub fn warn_if_pin_ignored(configured: Option<&Path>, notif: &LspNotification, server: &str) {
    let (Some(configured), LspNotification::Other { method, params }) = (configured, notif) else {
        return;
    };
    if method.as_ref() != TYPESCRIPT_VERSION_METHOD {
        return;
    }
    if let Some(ignored) = pin_ignored(params.as_ref()) {
        tracing::warn!(
            server,
            configured = %configured.display(),
            source = ?ignored.source,
            version = %ignored.version,
            "typescript-language-server ignored the configured tsserver.path"
        );
    }
}

/// The version report when the server took its tsserver from anywhere but the
/// configured `tsserver.path`.
fn pin_ignored(params: Option<&serde_json::Value>) -> Option<TypescriptVersionParams> {
    let parsed = serde_json::from_value::<TypescriptVersionParams>(params?.clone()).ok()?;
    (parsed.source != TsserverSource::UserSetting).then_some(parsed)
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use super::*;

    struct Layout {
        _dir: tempfile::TempDir,
        base: PathBuf,
        bin: PathBuf,
        tsserver: PathBuf,
    }

    /// `<base>/prefix/lib/node_modules/{typescript-language-server,typescript}`
    /// plus `<base>/prefix/bin/typescript-language-server` symlinked into the package.
    fn global_install(with_typescript: bool) -> Layout {
        let dir = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(dir.path()).unwrap();
        let modules = base.join("prefix/lib/node_modules");
        let package = modules.join(SERVER_STEM);
        fs::create_dir_all(package.join("lib")).unwrap();
        fs::write(package.join("package.json"), "{}").unwrap();
        fs::write(package.join("lib/cli.mjs"), "").unwrap();
        let tsserver = modules.join("typescript/lib/tsserver.js");
        if with_typescript {
            fs::create_dir_all(tsserver.parent().unwrap()).unwrap();
            fs::write(&tsserver, "").unwrap();
            fs::write(
                modules.join("typescript/package.json"),
                r#"{"version": "5.0.0"}"#,
            )
            .unwrap();
        }
        let bin = base.join("prefix/bin");
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(package.join("lib/cli.mjs"), bin.join(SERVER_STEM)).unwrap();
        Layout {
            _dir: dir,
            base,
            bin,
            tsserver,
        }
    }

    fn config(command: &str) -> LspServerConfig {
        let mut config = LspServerConfig::typescript();
        config.command = command.to_string();
        config
    }

    fn env_with_path(path: &Path) -> impl Fn(&str) -> Option<OsString> {
        let path = path.as_os_str().to_owned();
        move |key| (key == "PATH").then(|| path.clone())
    }

    #[test]
    fn test_resolve_pins_global_peer_typescript() {
        let layout = global_install(true);
        let resolved = resolve(&config(SERVER_STEM), env_with_path(&layout.bin));
        assert_eq!(resolved, Some(TsserverResolution::Pinned(layout.tsserver)));
    }

    #[test]
    fn test_resolve_absolute_command_path() {
        let layout = global_install(true);
        let command = layout.bin.join(SERVER_STEM);
        let resolved = resolve(&config(command.to_str().unwrap()), |_| None);
        assert_eq!(resolved, Some(TsserverResolution::Pinned(layout.tsserver)));
    }

    #[test]
    fn test_resolve_prefers_own_node_modules() {
        let layout = global_install(true);
        let package = layout
            .base
            .join("prefix/lib/node_modules")
            .join(SERVER_STEM);
        let nested = package.join(TSSERVER_RELATIVE);
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        fs::write(&nested, "").unwrap();
        fs::write(
            package.join("node_modules/typescript/package.json"),
            r#"{"version": "5.1.0"}"#,
        )
        .unwrap();
        let resolved = resolve(&config(SERVER_STEM), env_with_path(&layout.bin));
        assert_eq!(resolved, Some(TsserverResolution::Pinned(nested)));
    }

    #[test]
    fn test_resolve_config_env_path_overrides_parent() {
        let layout = global_install(true);
        let mut config = config(SERVER_STEM);
        config
            .env
            .insert("PATH".into(), layout.bin.to_str().unwrap().into());
        let resolved = resolve(&config, |_| Some(OsString::from("/nonexistent")));
        assert_eq!(resolved, Some(TsserverResolution::Pinned(layout.tsserver)));
    }

    #[test]
    fn test_resolve_server_not_on_path() {
        let layout = global_install(true);
        let resolved = resolve(&config(SERVER_STEM), env_with_path(&layout.base));
        assert_eq!(
            resolved,
            Some(TsserverResolution::Unresolved(
                UnresolvedReason::ServerNotOnPath
            ))
        );
    }

    #[test]
    fn test_resolve_without_typescript_is_unresolved() {
        let layout = global_install(false);
        let resolved = resolve(&config(SERVER_STEM), env_with_path(&layout.bin));
        assert_eq!(
            resolved,
            Some(TsserverResolution::Unresolved(
                UnresolvedReason::NoTypescriptNextToServer
            ))
        );
    }

    #[test]
    fn test_resolve_script_launcher_is_unsupported() {
        let layout = global_install(true);
        let shim_dir = layout.base.join("shims");
        fs::create_dir_all(&shim_dir).unwrap();
        fs::write(shim_dir.join(SERVER_STEM), "#!/bin/sh\n").unwrap();
        let resolved = resolve(&config(SERVER_STEM), env_with_path(&shim_dir));
        assert_eq!(
            resolved,
            Some(TsserverResolution::Unresolved(
                UnresolvedReason::UnsupportedLauncher
            ))
        );
    }

    #[test]
    fn test_resolve_cmd_shim_is_unsupported() {
        let resolved = resolve(&config("typescript-language-server.cmd"), |_| None);
        assert_eq!(
            resolved,
            Some(TsserverResolution::Unresolved(
                UnresolvedReason::UnsupportedLauncher
            ))
        );
    }

    #[test]
    fn test_resolve_ignores_other_servers() {
        assert_eq!(resolve(&config("pyright-langserver"), |_| None), None);
    }

    #[test]
    fn test_resolve_rejects_typescript_without_version() {
        let layout = global_install(true);
        let manifest = layout
            .tsserver
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("package.json");
        for body in ["{}", "not json"] {
            fs::write(&manifest, body).unwrap();
            let resolved = resolve(&config(SERVER_STEM), env_with_path(&layout.bin));
            assert_eq!(
                resolved,
                Some(TsserverResolution::Unresolved(
                    UnresolvedReason::NoTypescriptNextToServer
                ))
            );
        }
        fs::remove_file(&manifest).unwrap();
        let resolved = resolve(&config(SERVER_STEM), env_with_path(&layout.bin));
        assert_eq!(
            resolved,
            Some(TsserverResolution::Unresolved(
                UnresolvedReason::NoTypescriptNextToServer
            ))
        );
    }

    #[test]
    fn test_resolve_flags_launchers_naming_server_in_args() {
        for (command, args) in [
            ("npx", vec!["typescript-language-server", "--stdio"]),
            ("bunx", vec!["typescript-language-server@5.1.3", "--stdio"]),
            (
                "node",
                vec!["/opt/lib/node_modules/typescript-language-server/lib/cli.mjs"],
            ),
        ] {
            let mut config = config(command);
            config.args = args.into_iter().map(String::from).collect();
            assert_eq!(
                resolve(&config, |_| None),
                Some(TsserverResolution::Unresolved(
                    UnresolvedReason::UnsupportedLauncher
                )),
                "{command}"
            );
        }
    }

    #[test]
    fn test_unsupported_launcher_options_stay_unpinned() {
        let mut config = config("npx");
        config.args = vec!["typescript-language-server".into(), "--stdio".into()];
        assert_eq!(pinned_initialization_options(&config, &[], |_| None), None);
    }

    #[test]
    fn test_inside_workspace_server_is_pinned_with_warning() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let layout = global_install(true);
        let captured = crate::test_lsp::CapturedLogs::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
        let options = pinned_initialization_options(
            &config(SERVER_STEM),
            std::slice::from_ref(&layout.base),
            env_with_path(&layout.bin),
        );
        drop(guard);

        assert_eq!(
            configured_tsserver_path(options.as_ref()),
            Some(layout.tsserver)
        );
        assert!(
            captured
                .messages()
                .iter()
                .any(|m| m.contains("installed inside the workspace")),
            "{:?}",
            captured.messages()
        );
    }

    #[test]
    fn test_pinned_options_serialize_tsserver_path() {
        let layout = global_install(true);
        let options =
            pinned_initialization_options(&config(SERVER_STEM), &[], env_with_path(&layout.bin))
                .unwrap();
        assert_eq!(
            configured_tsserver_path(Some(&options)),
            Some(layout.tsserver)
        );
    }

    #[test]
    fn test_user_tsserver_path_wins() {
        let layout = global_install(true);
        let mut config = config(SERVER_STEM);
        let user = serde_json::json!({"tsserver": {"path": "/custom/tsserver.js"}});
        config.initialization_options = Some(user.clone());
        let options = pinned_initialization_options(&config, &[], env_with_path(&layout.bin));
        assert_eq!(options, Some(user));
    }

    #[test]
    fn test_user_options_without_path_skip_pin() {
        let layout = global_install(true);
        let mut config = config(SERVER_STEM);
        let user = serde_json::json!({"preferences": {}});
        config.initialization_options = Some(user.clone());
        let options = pinned_initialization_options(&config, &[], env_with_path(&layout.bin));
        assert_eq!(options, Some(user));
    }

    #[test]
    fn test_unresolved_leaves_options_none() {
        let layout = global_install(false);
        let options =
            pinned_initialization_options(&config(SERVER_STEM), &[], env_with_path(&layout.bin));
        assert_eq!(options, None);
    }

    #[test]
    fn test_other_server_options_untouched() {
        let mut config = LspServerConfig::rust_analyzer();
        config.initialization_options = Some(serde_json::json!({"a": 1}));
        let options = pinned_initialization_options(&config, &[], |_| None);
        assert_eq!(options, Some(serde_json::json!({"a": 1})));
    }

    #[test]
    fn test_pin_ignored_only_for_other_sources() {
        let report = |source: &str| {
            pin_ignored(Some(
                &serde_json::json!({"version": "5.0", "source": source}),
            ))
        };
        assert!(report("user-setting").is_none());
        assert!(report("workspace").is_some());
        assert!(report("bundled").is_some());
        assert!(pin_ignored(None).is_none());
    }

    #[test]
    fn test_version_params_parse_sources() {
        let parse = |source: &str| {
            serde_json::from_value::<TypescriptVersionParams>(
                serde_json::json!({"version": "5.0", "source": source}),
            )
            .unwrap()
            .source
        };
        assert_eq!(parse("user-setting"), TsserverSource::UserSetting);
        assert_eq!(parse("workspace"), TsserverSource::Workspace);
        assert_eq!(parse("bundled"), TsserverSource::Bundled);
        assert_eq!(parse("future"), TsserverSource::Unknown);
    }
}
