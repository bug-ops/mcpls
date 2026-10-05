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
//!
//! TypeScript 7 and later ship a native compiler service and no
//! `lib/tsserver.js`, so there is nothing to pin. Detection of that case only
//! reads size-capped package manifests and checks file existence; it never
//! runs, or asks anything of, workspace code.

use std::ffi::OsString;
use std::fmt;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bridge::WorkspaceRoots;
use crate::config::{BuiltinServer, LspServerConfig};
use crate::error::InitFailureHint;
use crate::lsp::{LspNotification, child_env_var};
use crate::util::read_regular_file_bounded;

const SERVER_STEM: &str = "typescript-language-server";
const NODE_MODULES: &str = "node_modules";
const TYPESCRIPT_RELATIVE: &str = "node_modules/typescript";
const TSSERVER_IN_PACKAGE: &str = "lib/tsserver.js";
const TSSERVER_RELATIVE: &str = "node_modules/typescript/lib/tsserver.js";
/// Upper bound for a `package.json` read from a directory that may be
/// workspace-supplied.
const MAX_MANIFEST_BYTES: NonZeroU64 = match NonZeroU64::new(64 * 1024) {
    Some(max) => max,
    None => panic!("the manifest limit must be non-zero"),
};
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
    /// Only TypeScript 7 or later, which ships no tsserver, is installed next
    /// to the server package.
    NativeTypescriptNextToServer,
}

impl fmt::Display for UnresolvedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServerNotOnPath => {
                f.write_str("typescript-language-server was not found on PATH")
            }
            Self::UnsupportedLauncher => f.write_str(
                "typescript-language-server is started through an unsupported launcher or shim",
            ),
            Self::NoTypescriptNextToServer => f.write_str(
                "no valid typescript package is installed next to typescript-language-server",
            ),
            Self::NativeTypescriptNextToServer => write!(
                f,
                "only TypeScript 7 or later, which ships no tsserver, is installed next to \
                 typescript-language-server; install a JavaScript-based TypeScript (`{}`) or \
                 configure `tsc --lsp --stdio`",
                BuiltinServer::TypescriptLanguageServer.install_hint()
            ),
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
    version: String,
}

/// The leading numeric component of a package version, so `7.0.1-rc` is 7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TypescriptMajor(u32);

impl TypescriptMajor {
    /// First major version that ships the native compiler service instead of
    /// `lib/tsserver.js`.
    const FIRST_NATIVE: Self = Self(7);

    fn parse(version: &str) -> Option<Self> {
        let digits = version
            .trim_start()
            .split(|c: char| !c.is_ascii_digit())
            .next()?;
        digits.parse().ok().map(Self)
    }

    const fn is_native(self) -> bool {
        self.0 >= Self::FIRST_NATIVE.0
    }
}

/// What a `node_modules/typescript` directory offers typescript-language-server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypescriptState {
    /// A valid install that ships `lib/tsserver.js`.
    Tsserver,
    /// A valid TypeScript 7 or later install without `lib/tsserver.js`.
    Native,
    /// Missing, invalid, unreadable or too large to trust.
    Absent,
}

/// Reads `<package>/package.json` through the bounded, non-blocking reader,
/// because the directory can be workspace-supplied.
fn read_manifest(package: &Path) -> Option<PackageManifest> {
    let bytes =
        read_regular_file_bounded(&package.join("package.json"), MAX_MANIFEST_BYTES).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn typescript_state(package: &Path) -> TypescriptState {
    let Some(manifest) = read_manifest(package) else {
        return TypescriptState::Absent;
    };
    if package.join(TSSERVER_IN_PACKAGE).is_file() {
        TypescriptState::Tsserver
    } else if TypescriptMajor::parse(&manifest.version).is_some_and(TypescriptMajor::is_native) {
        TypescriptState::Native
    } else {
        TypescriptState::Absent
    }
}

fn is_valid_typescript_install(tsserver: &Path) -> bool {
    tsserver
        .parent()
        .and_then(Path::parent)
        .is_some_and(|package| read_manifest(package).is_some())
}

/// Node's lookup path for a package: every ancestor of `start` (itself
/// included) that is not named `node_modules`.
fn lookup_dirs(start: &Path) -> impl Iterator<Item = &Path> {
    start
        .ancestors()
        .filter(|dir| dir.file_name().is_none_or(|name| name != NODE_MODULES))
}

/// Whether the nearest usable `typescript` install in node's lookup from
/// `start` is TypeScript 7 or later; a nearer install with a tsserver wins.
fn native_typescript_visible_from(start: &Path) -> bool {
    lookup_dirs(start)
        .map(|dir| typescript_state(&dir.join(TYPESCRIPT_RELATIVE)))
        .find(|state| *state != TypescriptState::Absent)
        == Some(TypescriptState::Native)
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
    lookup_dirs(package_dir)
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
// TODO(#634): auto-select the native tsc (spec config/002 FR-007, FR-011..013); needs default-entry provenance
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
    Some(bundled_tsserver(&package_dir).map_or_else(
        || {
            let reason = if native_typescript_visible_from(&package_dir) {
                UnresolvedReason::NativeTypescriptNextToServer
            } else {
                UnresolvedReason::NoTypescriptNextToServer
            };
            TsserverResolution::Unresolved(reason)
        },
        TsserverResolution::Pinned,
    ))
}

/// The guidance to attach to an `initialize` failure of `config`, when the
/// cause is that only TypeScript 7 or later is available to
/// typescript-language-server.
///
/// `effective_options` are the options the server was started with; a
/// `tsserver.path` in them means the failure is not a missing tsserver. The
/// workspace roots are checked because the server falls back to the
/// workspace's TypeScript when none sits next to it. A shim or script
/// launcher never gets the hint, since what the server would find is unknown.
///
/// Only reads manifests and checks file existence; it runs on the failure
/// path only.
// TODO(#604): shim installs (the default on Windows) get no TypeScript 7 init-failure hint
pub fn init_failure_hint(
    config: &LspServerConfig,
    effective_options: Option<&serde_json::Value>,
    workspace_roots: &[PathBuf],
    parent_env: impl Fn(&str) -> Option<OsString>,
) -> Option<InitFailureHint> {
    if configured_tsserver_path(effective_options).is_some() {
        return None;
    }
    let native = match resolve(config, parent_env)? {
        TsserverResolution::Unresolved(UnresolvedReason::NativeTypescriptNextToServer) => true,
        TsserverResolution::Unresolved(UnresolvedReason::NoTypescriptNextToServer) => {
            workspace_roots
                .iter()
                .any(|root| native_typescript_visible_from(root))
        }
        TsserverResolution::Pinned(_)
        | TsserverResolution::Unresolved(
            UnresolvedReason::ServerNotOnPath | UnresolvedReason::UnsupportedLauncher,
        ) => false,
    };
    native.then_some(InitFailureHint::NativeTypescriptOnly)
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
    workspace_roots: &WorkspaceRoots,
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
            let canonical = dunce::canonicalize(&tsserver).unwrap_or_else(|_| tsserver.clone());
            if workspace_roots.contains_canonical(&canonical) {
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
                "tsserver not pinned: {reason}; a workspace-supplied tsserver may run"
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

#[cfg(test)]
mod major_tests {
    use super::TypescriptMajor;

    #[test]
    fn test_typescript_major_parses_leading_digits() {
        let major = TypescriptMajor::parse;
        assert_eq!(major("7.0.1-rc.1"), Some(TypescriptMajor(7)));
        assert_eq!(major("10.1.0"), Some(TypescriptMajor(10)));
        assert_eq!(major("6.9.9"), Some(TypescriptMajor(6)));
        assert_eq!(major("next"), None);
        assert_eq!(major(""), None);
        assert!(TypescriptMajor(7).is_native());
        assert!(!TypescriptMajor(6).is_native());
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::{assert_matches, fs};

    use super::*;
    use crate::config::BuiltinServer;

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
        assert_eq!(
            pinned_initialization_options(&config, &WorkspaceRoots::default(), |_| None),
            None
        );
    }

    #[test]
    fn test_symlinked_typescript_resolving_into_workspace_is_warned() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let layout = global_install(true);
        let workspace = tempfile::tempdir().unwrap();
        let ws = dunce::canonicalize(workspace.path()).unwrap();
        fs::create_dir_all(ws.join("ts/lib")).unwrap();
        fs::write(ws.join("ts/lib/tsserver.js"), "").unwrap();
        fs::write(ws.join("ts/package.json"), r#"{"version": "5.0.0"}"#).unwrap();
        let link = layout.base.join("prefix/lib/node_modules/typescript");
        fs::remove_dir_all(&link).unwrap();
        std::os::unix::fs::symlink(ws.join("ts"), &link).unwrap();

        let captured = crate::test_lsp::CapturedLogs::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
        let options = pinned_initialization_options(
            &config(SERVER_STEM),
            &WorkspaceRoots::from_configured(std::slice::from_ref(&ws)).unwrap(),
            env_with_path(&layout.bin),
        );
        drop(guard);

        assert!(options.is_some());
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
    fn test_inside_workspace_server_is_pinned_with_warning() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let layout = global_install(true);
        let captured = crate::test_lsp::CapturedLogs::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
        let options = pinned_initialization_options(
            &config(SERVER_STEM),
            &WorkspaceRoots::from_configured(std::slice::from_ref(&layout.base)).unwrap(),
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
        let options = pinned_initialization_options(
            &config(SERVER_STEM),
            &WorkspaceRoots::default(),
            env_with_path(&layout.bin),
        )
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
        let options = pinned_initialization_options(
            &config,
            &WorkspaceRoots::default(),
            env_with_path(&layout.bin),
        );
        assert_eq!(options, Some(user));
    }

    #[test]
    fn test_user_options_without_path_skip_pin() {
        let layout = global_install(true);
        let mut config = config(SERVER_STEM);
        let user = serde_json::json!({"preferences": {}});
        config.initialization_options = Some(user.clone());
        let options = pinned_initialization_options(
            &config,
            &WorkspaceRoots::default(),
            env_with_path(&layout.bin),
        );
        assert_eq!(options, Some(user));
    }

    #[test]
    fn test_unresolved_leaves_options_none() {
        let layout = global_install(false);
        let options = pinned_initialization_options(
            &config(SERVER_STEM),
            &WorkspaceRoots::default(),
            env_with_path(&layout.bin),
        );
        assert_eq!(options, None);
    }

    #[test]
    fn test_other_server_options_untouched() {
        let mut config = LspServerConfig::rust_analyzer();
        config.initialization_options = Some(serde_json::json!({"a": 1}));
        let options = pinned_initialization_options(&config, &WorkspaceRoots::default(), |_| None);
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

    /// Writes `<dir>/node_modules/typescript` with `version` and, when
    /// `with_tsserver` is set, a `lib/tsserver.js`; returns the package dir.
    fn write_typescript(dir: &Path, version: &str, with_tsserver: bool) -> PathBuf {
        let package = dir.join(TYPESCRIPT_RELATIVE);
        fs::create_dir_all(package.join("lib")).unwrap();
        fs::write(
            package.join("package.json"),
            format!(r#"{{"version": "{version}"}}"#),
        )
        .unwrap();
        if with_tsserver {
            fs::write(package.join(TSSERVER_IN_PACKAGE), "").unwrap();
        }
        package
    }

    fn hint(
        layout: &Layout,
        config: &LspServerConfig,
        roots: &[PathBuf],
    ) -> Option<InitFailureHint> {
        init_failure_hint(config, None, roots, env_with_path(&layout.bin))
    }

    #[test]
    fn test_resolve_native_typescript_next_to_server() {
        let layout = global_install(false);
        write_typescript(&layout.base.join("prefix/lib"), "7.0.2", false);
        let resolved = resolve(&config(SERVER_STEM), env_with_path(&layout.bin));
        assert_eq!(
            resolved,
            Some(TsserverResolution::Unresolved(
                UnresolvedReason::NativeTypescriptNextToServer
            ))
        );
        assert_eq!(
            hint(&layout, &config(SERVER_STEM), &[]),
            Some(InitFailureHint::NativeTypescriptOnly)
        );
    }

    #[test]
    fn test_typescript_seven_shipping_tsserver_is_still_pinned() {
        let layout = global_install(false);
        write_typescript(&layout.base.join("prefix/lib"), "7.0.2", true);
        let resolved = resolve(&config(SERVER_STEM), env_with_path(&layout.bin));
        assert_matches!(resolved, Some(TsserverResolution::Pinned(_)));
        assert_eq!(hint(&layout, &config(SERVER_STEM), &[]), None);
    }

    #[test]
    fn test_hint_for_native_typescript_at_workspace_root() {
        let layout = global_install(false);
        let root = layout.base.join("ws");
        write_typescript(&root, "7.0.2", false);
        assert_eq!(
            hint(&layout, &config(SERVER_STEM), std::slice::from_ref(&root)),
            Some(InitFailureHint::NativeTypescriptOnly)
        );
    }

    #[test]
    fn test_hint_for_native_typescript_above_workspace_root() {
        let layout = global_install(false);
        let repo = layout.base.join("repo");
        write_typescript(&repo, "7.0.2", false);
        let root = repo.join("packages/app");
        fs::create_dir_all(&root).unwrap();
        assert_eq!(
            hint(&layout, &config(SERVER_STEM), std::slice::from_ref(&root)),
            Some(InitFailureHint::NativeTypescriptOnly)
        );
    }

    #[test]
    fn test_nearest_typescript_with_tsserver_wins_over_native_above() {
        let layout = global_install(false);
        let repo = layout.base.join("repo");
        write_typescript(&repo, "7.0.2", false);
        let root = repo.join("packages/app");
        write_typescript(&root, "5.4.0", true);
        assert_eq!(
            hint(&layout, &config(SERVER_STEM), std::slice::from_ref(&root)),
            None
        );
    }

    #[test]
    fn test_no_hint_for_old_typescript_at_workspace_root() {
        let layout = global_install(false);
        let root = layout.base.join("ws");
        write_typescript(&root, "5.4.0", true);
        assert_eq!(
            hint(&layout, &config(SERVER_STEM), std::slice::from_ref(&root)),
            None
        );
    }

    #[test]
    fn test_no_hint_when_tsserver_path_is_configured() {
        let layout = global_install(false);
        write_typescript(&layout.base.join("prefix/lib"), "7.0.2", false);
        let options = serde_json::json!({"tsserver": {"path": "/custom/tsserver.js"}});
        assert_eq!(
            init_failure_hint(
                &config(SERVER_STEM),
                Some(&options),
                &[],
                env_with_path(&layout.bin)
            ),
            None
        );
    }

    #[test]
    fn test_no_hint_for_unsupported_launcher_or_other_server() {
        let layout = global_install(false);
        let root = layout.base.join("ws");
        write_typescript(&root, "7.0.2", false);
        let roots = std::slice::from_ref(&root);
        assert_eq!(
            hint(&layout, &config("typescript-language-server.cmd"), roots),
            None
        );
        assert_eq!(hint(&layout, &config("pyright-langserver"), roots), None);
    }

    #[test]
    fn test_oversize_manifest_is_treated_as_absent() {
        let layout = global_install(false);
        let root = layout.base.join("ws");
        let package = write_typescript(&root, "7.0.2", false);
        let padding = " ".repeat(usize::try_from(MAX_MANIFEST_BYTES.get()).unwrap());
        fs::write(
            package.join("package.json"),
            format!(r#"{{"version": "7.0.2"{padding}}}"#),
        )
        .unwrap();
        assert_eq!(
            hint(&layout, &config(SERVER_STEM), std::slice::from_ref(&root)),
            None
        );
    }

    #[test]
    fn test_native_command_gets_no_pin() {
        let mut config = config("tsc");
        config.args = vec!["--lsp".into(), "--stdio".into()];
        assert_eq!(resolve(&config, |_| None), None);
        assert_eq!(
            pinned_initialization_options(&config, &WorkspaceRoots::default(), |_| None),
            None
        );
    }

    #[test]
    fn test_native_next_to_server_warning_names_typescript_7() {
        let text = UnresolvedReason::NativeTypescriptNextToServer.to_string();
        assert!(text.contains("TypeScript 7"), "{text}");
        assert!(
            text.contains(BuiltinServer::TypescriptLanguageServer.install_hint()),
            "{text}"
        );
    }

    /// A `package.json` that is a FIFO must not hang resolution or the hint.
    #[test]
    fn test_fifo_manifest_does_not_block() {
        let layout = global_install(false);
        let next_to_server = write_typescript(&layout.base.join("prefix/lib"), "7.0.2", true);
        let root = layout.base.join("ws");
        let in_workspace = write_typescript(&root, "7.0.2", false);
        for package in [&next_to_server, &in_workspace] {
            let manifest = package.join("package.json");
            fs::remove_file(&manifest).unwrap();
            let status = std::process::Command::new("mkfifo")
                .arg(&manifest)
                .status()
                .unwrap();
            assert!(status.success(), "mkfifo must succeed to set up this test");
        }
        let bin = layout.bin;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let env = env_with_path(&bin);
            let resolved = resolve(&config(SERVER_STEM), &env);
            let hinted = init_failure_hint(&config(SERVER_STEM), None, &[root], &env);
            tx.send((resolved, hinted)).unwrap();
        });
        let (resolved, hinted) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("a FIFO manifest must not block"));
        assert_eq!(
            resolved,
            Some(TsserverResolution::Unresolved(
                UnresolvedReason::NoTypescriptNextToServer
            ))
        );
        assert_eq!(hinted, None);
    }

    /// An end-to-end failure: the server exits during `initialize` while only
    /// TypeScript 7 is installed in the workspace.
    #[tokio::test]
    async fn test_spawn_failure_carries_native_typescript_hint() {
        use std::os::unix::fs::PermissionsExt as _;

        use crate::lsp::{LspServer, ServerInitConfig};

        let layout = global_install(false);
        let cli = layout
            .base
            .join("prefix/lib/node_modules")
            .join(SERVER_STEM)
            .join("lib/cli.mjs");
        fs::write(&cli, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).unwrap();
        let root = layout.base.join("ws");
        write_typescript(&root, "7.0.2", false);

        let server_config = config(layout.bin.join(SERVER_STEM).to_str().unwrap());
        let err = LspServer::spawn(ServerInitConfig {
            server_config,
            workspace_roots: vec![root],
            initialization_options: None,
            position_encodings: crate::config::PositionEncodings::DEFAULT,
            redactions: std::sync::Arc::default(),
        })
        .await
        .map(|_| ())
        .unwrap_err();

        assert_matches!(
            err,
            crate::Error::ServerExitedDuringInit {
                hint: Some(InitFailureHint::NativeTypescriptOnly),
                ..
            }
        );
        assert!(err.to_string().contains("TypeScript 7"), "{err}");
    }
}
