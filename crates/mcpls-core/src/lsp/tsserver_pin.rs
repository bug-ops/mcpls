//! Pins `typescript-language-server` to the tsserver it would bundle.
//!
//! The server picks a TypeScript compiler service in the order user setting,
//! workspace, bundled, so an analyzed checkout can run its own
//! `node_modules/typescript/lib/tsserver.js` (#566). Passing the bundled
//! tsserver as `initializationOptions.tsserver.path` removes the workspace
//! from that choice. Resolution mirrors node's own lookup for the server's
//! `typescript` dependency, starting from the server's package directory.
//!
//! Covered installs: symlinked executables (npm, nvm, bun, Homebrew with a
//! global `typescript` peer), npm `.cmd`/`.ps1`/extensionless shims next to
//! `node_modules/typescript-language-server`, pnpm global installs, and
//! `node` or `bun` running the server's absolute `cli.mjs`. Shims are never
//! executed or parsed. Package runners (`npx`, `bunx`, `pnpm dlx`,
//! `deno npm:`) and version-manager shims (Volta, asdf, mise) stay
//! unresolved and are reported with a warning, never with a startup
//! failure; `initialization_options.tsserver.path` pins them by hand.
//!
//! TypeScript 7 and later ship a native compiler service and no
//! `lib/tsserver.js`, so there is nothing to pin. Detection of that case only
//! reads size-capped package manifests and checks file existence; it never
//! runs, or asks anything of, workspace code.
//!
//! An entry with `selection = "auto"` may instead start the native `tsc` of a
//! TypeScript 7 install outside every workspace root ([`select_typescript_server`]).

use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bridge::WorkspaceRoots;
use crate::config::{BuiltinServer, LaunchCommand, LspServerConfig, ServerCommand, ServerId};
use crate::error::InitFailureHint;
use crate::lsp::command_path::{HostOs, resolve_named, resolve_named_on};
use crate::lsp::{LspNotification, ParentEnv};
use crate::util::read_regular_file_bounded;

const SERVER_STEM: &str = "typescript-language-server";
const NODE_MODULES: &str = "node_modules";
const TYPESCRIPT_RELATIVE: &str = "node_modules/typescript";
const TSSERVER_IN_PACKAGE: &str = "lib/tsserver.js";
const TSSERVER_RELATIVE: &str = "node_modules/typescript/lib/tsserver.js";
const NATIVE_TSC_NAME: &str = "tsc";
const NATIVE_TSC_WINDOWS_SHIM: &str = "tsc.cmd";
const NODE_NAME: &str = "node";
const TYPESCRIPT_STEM: &str = "typescript";
const NATIVE_BIN_DIR: &str = "bin";
const NATIVE_TSC_ARGS: [&str; 2] = ["--lsp", "--stdio"];
const NPM_SPECIFIER_PREFIX: &str = "npm:";
const SCRIPT_INTERPRETERS: [&str; 2] = ["node", "bun"];
const PACKAGE_RUNNERS: [&str; 7] = ["npx", "bunx", "pnpx", "pnpm", "yarn", "npm", "deno"];
const PNPM_GLOBAL_DIR: &str = "global";
/// Most `global/<store version>` entries a pnpm install is searched through;
/// more is treated as ambiguous.
const MAX_PNPM_GLOBAL_ENTRIES: usize = 16;
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
    /// A shim, wrapper or version-manager launcher whose package directory
    /// is not reachable through the executable.
    UnsupportedLauncher,
    /// A package runner (`npx`, `bunx`, `pnpm dlx`, `deno npm:`) that
    /// resolves the server outside mcpls' reach.
    PackageRunner,
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
                "typescript-language-server is started through an unsupported launcher or shim; \
                 set `initialization_options.tsserver.path` to pin a tsserver",
            ),
            Self::PackageRunner => f.write_str(
                "typescript-language-server is started through a package runner (npx, bunx, \
                 pnpm dlx, deno npm:); install it globally or set \
                 `initialization_options.tsserver.path` to pin a tsserver",
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

/// Whether `arg` names the server package or script, as in `npx
/// typescript-language-server@5`, `deno run npm:typescript-language-server` or
/// `node .../typescript-language-server/lib/cli.mjs`.
fn mentions_server(arg: &str) -> bool {
    let arg = arg.strip_prefix(NPM_SPECIFIER_PREFIX).unwrap_or(arg);
    Path::new(arg)
        .components()
        .any(|part| part.as_os_str().to_string_lossy().starts_with(SERVER_STEM))
}

fn command_stem_is(command: &str, names: &[&str]) -> bool {
    Path::new(command)
        .file_stem()
        .and_then(OsStr::to_str)
        .is_some_and(|stem| names.iter().any(|name| stem.eq_ignore_ascii_case(name)))
}

/// How the configured command reaches typescript-language-server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Launch<'a> {
    /// The server executable itself: a symlink, an npm shim or a pnpm shim.
    Server(&'a Path),
    /// `node` or `bun` running the server's absolute script.
    Script(&'a Path),
    /// A package runner that fetches or resolves the server on its own.
    PackageRunner,
    /// Any other command that names the server in its arguments.
    UnknownWrapper,
}

/// The launch kind of `config`, or `None` when it does not involve
/// typescript-language-server.
fn classify(config: &LspServerConfig) -> Option<Launch<'_>> {
    if BuiltinServer::TypescriptLanguageServer.matches_command(config.command.as_str()) {
        return Some(Launch::Server(Path::new(&config.command)));
    }
    if !config.args.iter().any(|arg| mentions_server(arg)) {
        return None;
    }
    let script = command_stem_is(config.command.as_str(), &SCRIPT_INTERPRETERS)
        .then(|| {
            config
                .args
                .iter()
                .map(Path::new)
                .find(|arg| arg.is_absolute() && arg.to_str().is_some_and(mentions_server))
        })
        .flatten();
    if let Some(script) = script {
        return Some(Launch::Script(script));
    }
    let runner = command_stem_is(config.command.as_str(), &PACKAGE_RUNNERS)
        || config
            .args
            .iter()
            .any(|arg| arg.starts_with(NPM_SPECIFIER_PREFIX));
    Some(if runner {
        Launch::PackageRunner
    } else {
        Launch::UnknownWrapper
    })
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

/// The nearest usable `typescript` package in node's lookup from `start`
/// when it is TypeScript 7 or later; a nearer install with a tsserver wins.
fn nearest_native_typescript(start: &Path) -> Option<PathBuf> {
    lookup_dirs(start)
        .map(|dir| dir.join(TYPESCRIPT_RELATIVE))
        .map(|package| (typescript_state(&package), package))
        .find(|(state, _)| *state != TypescriptState::Absent)
        .and_then(|(state, package)| (state == TypescriptState::Native).then_some(package))
}

/// An npm package whose install layout [`InstallLayout`] can follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NpmPackage {
    TypescriptLanguageServer,
    Typescript,
}

impl NpmPackage {
    const fn stem(self) -> &'static str {
        match self {
            Self::TypescriptLanguageServer => SERVER_STEM,
            Self::Typescript => TYPESCRIPT_STEM,
        }
    }
}

/// The canonical directory of `package` containing (or equal to) `path`.
fn package_dir_of(path: &Path, package: NpmPackage) -> Option<PathBuf> {
    let real = dunce::canonicalize(path).ok()?;
    real.ancestors()
        .find(|dir| {
            dir.file_name().is_some_and(|name| name == package.stem())
                && dir.join("package.json").is_file()
        })
        .map(Path::to_path_buf)
}

/// How an installed server executable leads to its package directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallLayout {
    /// The executable is a symlink into the package (npm, nvm, bun, Homebrew).
    Symlink,
    /// A shim in the directory that holds `node_modules` (npm on Windows).
    NpmShim,
    /// A shim in a pnpm home whose packages live under `global/<version>`.
    PnpmGlobal,
}

impl InstallLayout {
    const ALL: [Self; 3] = [Self::Symlink, Self::NpmShim, Self::PnpmGlobal];

    fn locate(self, executable: &Path, package: NpmPackage) -> Option<PathBuf> {
        match self {
            Self::Symlink => package_dir_of(executable, package),
            Self::NpmShim => {
                package_dir_of(&package_in_modules(executable.parent()?, package), package)
            }
            Self::PnpmGlobal => {
                pnpm_global_package(&executable.parent()?.join(PNPM_GLOBAL_DIR), package)
            }
        }
    }
}

fn package_in_modules(dir: &Path, package: NpmPackage) -> PathBuf {
    dir.join(NODE_MODULES).join(package.stem())
}

/// The one `package` under a pnpm `global` directory; more than
/// [`MAX_PNPM_GLOBAL_ENTRIES`] entries, or more than one distinct match, is
/// ambiguous and yields `None`.
fn pnpm_global_package(global: &Path, package: NpmPackage) -> Option<PathBuf> {
    let entries = std::fs::read_dir(global)
        .ok()?
        .take(MAX_PNPM_GLOBAL_ENTRIES + 1)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if entries.len() > MAX_PNPM_GLOBAL_ENTRIES {
        return None;
    }
    let mut packages = entries
        .iter()
        .filter_map(|entry| package_dir_of(&package_in_modules(&entry.path(), package), package));
    let package = packages.next()?;
    packages.all(|other| other == package).then_some(package)
}

fn locate_package(executable: &Path, package: NpmPackage) -> Option<PathBuf> {
    InstallLayout::ALL
        .into_iter()
        .find_map(|layout| layout.locate(executable, package))
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

/// A [`TsserverResolution`] plus the native `typescript` package that
/// decided a `NativeTypescriptNextToServer` outcome.
struct ResolvedServer {
    resolution: TsserverResolution,
    native_package: Option<PathBuf>,
}

impl ResolvedServer {
    const fn unresolved(reason: UnresolvedReason) -> Self {
        Self {
            resolution: TsserverResolution::Unresolved(reason),
            native_package: None,
        }
    }
}

fn server_package_dir(
    config: &LspServerConfig,
    launch: Launch<'_>,
    parent_env: impl ParentEnv,
) -> Result<PathBuf, UnresolvedReason> {
    let unsupported = UnresolvedReason::UnsupportedLauncher;
    match launch {
        Launch::PackageRunner => Err(UnresolvedReason::PackageRunner),
        Launch::UnknownWrapper => Err(unsupported),
        Launch::Script(script) => {
            package_dir_of(script, NpmPackage::TypescriptLanguageServer).ok_or(unsupported)
        }
        Launch::Server(command) => {
            let executable = resolve_named(command, config, parent_env)
                .ok_or(UnresolvedReason::ServerNotOnPath)?;
            locate_package(&executable.spawn, NpmPackage::TypescriptLanguageServer)
                .ok_or(unsupported)
        }
    }
}

fn inspect(config: &LspServerConfig, parent_env: impl ParentEnv) -> Option<ResolvedServer> {
    let launch = classify(config)?;
    let package_dir = match server_package_dir(config, launch, parent_env) {
        Ok(dir) => dir,
        Err(reason) => return Some(ResolvedServer::unresolved(reason)),
    };
    if let Some(tsserver) = bundled_tsserver(&package_dir) {
        return Some(ResolvedServer {
            resolution: TsserverResolution::Pinned(tsserver),
            native_package: None,
        });
    }
    let native_package = nearest_native_typescript(&package_dir);
    let reason = if native_package.is_some() {
        UnresolvedReason::NativeTypescriptNextToServer
    } else {
        UnresolvedReason::NoTypescriptNextToServer
    };
    Some(ResolvedServer {
        resolution: TsserverResolution::Unresolved(reason),
        native_package,
    })
}

/// Resolves the tsserver `config`'s server would bundle, or `None` when
/// `config` does not launch typescript-language-server.
///
/// A launcher that only names the server in its arguments is
/// `PackageRunner` (`npx`, `bunx`, `deno npm:`), pinned through its absolute
/// script (`node`, `bun`), or `UnsupportedLauncher` (wrappers); it is never
/// silently unrelated.
///
/// `PATH` is read as the child sees it: the config's `env` override, else
/// `parent_env`.
pub fn resolve(config: &LspServerConfig, parent_env: impl ParentEnv) -> Option<TsserverResolution> {
    inspect(config, parent_env).map(|resolved| resolved.resolution)
}

/// A `tsc` proven to be the native TypeScript 7 compiler outside every
/// workspace root.
///
/// Built only by [`Self::from_candidate`], the single gate every
/// auto-selection candidate passes through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeTsc {
    tsc: ServerCommand,
    launch: NativeLaunch,
}

/// How a [`NativeTsc`] is started.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NativeLaunch {
    /// The `tsc` file is executed itself (a native binary, or a `node`
    /// shebang script on Unix).
    Direct,
    /// The `tsc` script is passed to this `node`, as a shebang cannot be
    /// executed on Windows.
    ThroughNode(ServerCommand),
}

/// Why a `tsc` candidate was not accepted as a [`NativeTsc`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateRejected {
    /// The path does not resolve to `<typescript package>/bin/tsc` of a
    /// native TypeScript install, or cannot be canonicalized.
    NotNativeTsc,
    /// The canonical path lies inside a workspace root.
    InsideWorkspace,
    /// The file is not a regular file with an execute bit.
    NotExecutable,
    /// The canonical path is not valid UTF-8 and cannot become a `command`.
    NonUtf8Path,
}

impl From<CandidateRejected> for TsserverKept {
    fn from(rejected: CandidateRejected) -> Self {
        match rejected {
            CandidateRejected::NotNativeTsc => Self::NoNativeOutsideWorkspace,
            CandidateRejected::InsideWorkspace => Self::NativeInsideWorkspace,
            CandidateRejected::NotExecutable => Self::NotExecutable,
            CandidateRejected::NonUtf8Path => Self::NonUtf8Path,
        }
    }
}

/// The `typescript` package directory when `canonical` is its `bin/tsc` and
/// the package is a native (TypeScript 7 or later) install.
fn native_tsc_in(package: &Path) -> PathBuf {
    package.join(NATIVE_BIN_DIR).join(NATIVE_TSC_NAME)
}

fn native_package_of(canonical: &Path) -> Option<&Path> {
    let name_is = |path: &Path, expected: &str| path.file_name().is_some_and(|n| n == expected);
    let bin = canonical.parent()?;
    let package = bin.parent()?;
    (name_is(canonical, NATIVE_TSC_NAME)
        && name_is(bin, NATIVE_BIN_DIR)
        && name_is(package, "typescript")
        && typescript_state(package) == TypescriptState::Native)
        .then_some(package)
}

/// Whether the canonical `canonical` of `path`, or the directory `path` was
/// found in, lies inside a root of `roots`.
fn lies_inside_workspace(path: &Path, canonical: &Path, roots: &WorkspaceRoots) -> bool {
    let parent_inside = path
        .parent()
        .and_then(|parent| dunce::canonicalize(parent).ok())
        .is_some_and(|parent| roots.contains_canonical(&parent));
    roots.contains_canonical(canonical) || parent_inside
}

impl NativeTsc {
    /// Accepts `path` only when it canonicalizes to the `bin/tsc` of a native
    /// `typescript` package, as an executable regular file with a UTF-8 path.
    /// Neither the canonical path nor the directory the candidate was found in
    /// may lie inside a root of `roots`, so a workspace directory on `PATH`
    /// cannot steer which outside binary runs.
    ///
    /// # Errors
    ///
    /// The rejection that decided: canonicalization failure and a shape or
    /// version mismatch are [`CandidateRejected::NotNativeTsc`].
    fn from_candidate(
        path: &Path,
        roots: &WorkspaceRoots,
        host: HostOs,
    ) -> Result<Self, CandidateRejected> {
        let canonical = dunce::canonicalize(path).map_err(|_| CandidateRejected::NotNativeTsc)?;
        if native_package_of(&canonical).is_none() {
            return Err(CandidateRejected::NotNativeTsc);
        }
        if lies_inside_workspace(path, &canonical, roots) {
            return Err(CandidateRejected::InsideWorkspace);
        }
        if !host.is_executable_file(&canonical) {
            return Err(CandidateRejected::NotExecutable);
        }
        let tsc = canonical.to_str().ok_or(CandidateRejected::NonUtf8Path)?;
        let tsc = ServerCommand::new(tsc).map_err(|_| CandidateRejected::NotExecutable)?;
        Ok(Self {
            tsc,
            launch: NativeLaunch::Direct,
        })
    }

    /// The canonical path of the native `tsc`.
    #[must_use]
    pub fn path(&self) -> &Path {
        Path::new(&self.tsc)
    }

    /// The `command` and `args` that start the native server.
    fn launch_parts(&self) -> (ServerCommand, Vec<String>) {
        let lsp_args = NATIVE_TSC_ARGS.iter().map(ToString::to_string);
        match &self.launch {
            NativeLaunch::Direct => (self.tsc.clone(), lsp_args.collect()),
            NativeLaunch::ThroughNode(node) => (
                node.clone(),
                std::iter::once(self.tsc.to_string())
                    .chain(lsp_args)
                    .collect(),
            ),
        }
    }
}

/// Why `typescript-language-server` was kept although the entry allows the
/// native server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsserverKept {
    /// A tsserver next to `typescript-language-server` can be pinned, so a
    /// JavaScript-based TypeScript is installed next to the server.
    TsserverPinned,
    /// The entry sets `initialization_options.tsserver.path`, which always wins.
    UserTsserverPath,
    /// No native TypeScript install outside every workspace root was found.
    NoNativeOutsideWorkspace,
    /// The only native `tsc` found lies inside a workspace root and is never
    /// run without the user's own configuration.
    NativeInsideWorkspace,
    /// The native `tsc` is not an executable regular file.
    NotExecutable,
    /// The native `tsc` path is not valid UTF-8.
    NonUtf8Path,
    /// The native `tsc` is a `node` script and `node` is not on the effective
    /// `PATH`.
    NodeNotOnPath,
    /// On Windows, the `node` that would run the native `tsc` lies inside a
    /// workspace root and is never run without the user's own configuration.
    NodeInsideWorkspace,
    /// The server is started through an unsupported shim, wrapper or package
    /// runner.
    UnsupportedLauncher,
}

impl fmt::Display for TsserverKept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TsserverPinned => {
                "a JavaScript tsserver next to typescript-language-server can be pinned"
            }
            Self::UserTsserverPath => "initialization_options.tsserver.path is set",
            Self::NoNativeOutsideWorkspace => {
                "no TypeScript 7 install outside the workspace was found"
            }
            Self::NativeInsideWorkspace => {
                "the only TypeScript 7 install found is inside the workspace"
            }
            Self::NotExecutable => "the TypeScript 7 `tsc` is not an executable file",
            Self::NonUtf8Path => "the TypeScript 7 `tsc` path is not valid UTF-8",
            Self::NodeNotOnPath => "the TypeScript 7 `tsc` needs `node`, which is not on PATH",
            Self::NodeInsideWorkspace => {
                "the `node` for the TypeScript 7 `tsc` is inside the workspace"
            }
            Self::UnsupportedLauncher => {
                "typescript-language-server is started through an unsupported launcher or shim"
            }
        })
    }
}

/// The server flavor chosen for an auto-selecting TypeScript entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypescriptServerChoice {
    /// Keep `typescript-language-server`.
    Tsserver(TsserverKept),
    /// Start `tsc --lsp --stdio` from this install.
    Native(NativeTsc),
}

/// `binary` as the child would find it on its effective `PATH`.
fn find_on_child_path(
    host: HostOs,
    config: &LspServerConfig,
    parent_env: impl ParentEnv,
    binary: &str,
) -> Option<PathBuf> {
    resolve_named_on(host, Path::new(binary), config, parent_env).map(|resolved| resolved.spawn)
}

/// The `bin/tsc` of the native `typescript` package a `tsc` on the child's
/// `PATH` belongs to. On Windows the npm shim is `tsc.cmd`, found explicitly
/// because a bare `tsc` only matches `tsc.exe`, and mapped to its package
/// through the install layout.
fn native_tsc_on_path(
    host: HostOs,
    config: &LspServerConfig,
    parent_env: impl ParentEnv,
) -> Option<PathBuf> {
    match host {
        HostOs::Windows => {
            let shim = find_on_child_path(host, config, parent_env, NATIVE_TSC_WINDOWS_SHIM)?;
            locate_package(&shim, NpmPackage::Typescript).map(|package| native_tsc_in(&package))
        }
        HostOs::Other => find_on_child_path(host, config, parent_env, NATIVE_TSC_NAME),
    }
}

/// How `tsc` is started on `host`, or why it cannot be.
///
/// On Windows `node` comes from the child's `PATH` and must pass the check the
/// `tsc` passed: canonical, and neither it nor the directory it was found in
/// inside a root of `roots`.
fn native_launch(
    host: HostOs,
    tsc: &Path,
    roots: &WorkspaceRoots,
    config: &LspServerConfig,
    parent_env: impl ParentEnv,
) -> Result<NativeLaunch, TsserverKept> {
    let node = || find_on_child_path(host, config, &parent_env, NODE_NAME);
    match host {
        HostOs::Windows => {
            let found = node().ok_or(TsserverKept::NodeNotOnPath)?;
            let canonical = dunce::canonicalize(&found).map_err(|_| TsserverKept::NodeNotOnPath)?;
            if lies_inside_workspace(&found, &canonical, roots) {
                return Err(TsserverKept::NodeInsideWorkspace);
            }
            let node = canonical.to_str().ok_or(TsserverKept::NonUtf8Path)?;
            let node = ServerCommand::new(node).map_err(|_| TsserverKept::NodeNotOnPath)?;
            Ok(NativeLaunch::ThroughNode(node))
        }
        HostOs::Other if needs_node(tsc) && node().is_none() => Err(TsserverKept::NodeNotOnPath),
        HostOs::Other => Ok(NativeLaunch::Direct),
    }
}

/// Chooses the server flavor for `config`, or `None` when the entry is
/// explicit or does not launch typescript-language-server.
///
/// A user-set `initialization_options.tsserver.path` always keeps the server.
/// Otherwise a TypeScript 7 install next to the server, else a `tsc` on the
/// child's `PATH`, is considered, and only when no JavaScript tsserver next to
/// the server can be pinned. A candidate inside any root of `roots` is
/// never selected.
pub fn select_typescript_server(
    config: &LspServerConfig,
    roots: &WorkspaceRoots,
    parent_env: impl ParentEnv,
) -> Option<TypescriptServerChoice> {
    select_typescript_server_on(config, roots, parent_env, HostOs::CURRENT)
}

/// [`select_typescript_server`] with the executable rules of `host`, so the
/// Windows path is exercised on every OS.
fn select_typescript_server_on(
    config: &LspServerConfig,
    roots: &WorkspaceRoots,
    parent_env: impl ParentEnv,
    host: HostOs,
) -> Option<TypescriptServerChoice> {
    if config.command.is_explicit() {
        return None;
    }
    let kept = |reason| Some(TypescriptServerChoice::Tsserver(reason));
    if UserTsserverPath::of(config.initialization_options.as_ref()) != UserTsserverPath::Absent {
        return kept(TsserverKept::UserTsserverPath);
    }
    let resolved = inspect(config, &parent_env)?;
    let candidate = match resolved.resolution {
        TsserverResolution::Pinned(_) => return kept(TsserverKept::TsserverPinned),
        TsserverResolution::Unresolved(
            UnresolvedReason::UnsupportedLauncher | UnresolvedReason::PackageRunner,
        ) => return kept(TsserverKept::UnsupportedLauncher),
        TsserverResolution::Unresolved(UnresolvedReason::NativeTypescriptNextToServer) => resolved
            .native_package
            .map(|package| native_tsc_in(&package)),
        TsserverResolution::Unresolved(
            UnresolvedReason::NoTypescriptNextToServer | UnresolvedReason::ServerNotOnPath,
        ) => native_tsc_on_path(host, config, &parent_env),
    };
    let Some(candidate) = candidate else {
        return kept(TsserverKept::NoNativeOutsideWorkspace);
    };
    Some(match NativeTsc::from_candidate(&candidate, roots, host) {
        Ok(mut tsc) => match native_launch(host, tsc.path(), roots, config, &parent_env) {
            Ok(launch) => {
                tsc.launch = launch;
                TypescriptServerChoice::Native(tsc)
            }
            Err(kept) => TypescriptServerChoice::Tsserver(kept),
        },
        Err(rejected) => TypescriptServerChoice::Tsserver(rejected.into()),
    })
}

/// Whether `tsc` starts with a `node` shebang, as the npm `bin/tsc` launcher
/// does; a native binary does not.
fn needs_node(tsc: &Path) -> bool {
    use std::io::Read as _;

    let mut head = [0_u8; 128];
    let Ok(mut file) = std::fs::File::open(tsc) else {
        return false;
    };
    let read = file.read(&mut head).unwrap_or(0);
    let first_line = head
        .get(..read)
        .and_then(|bytes| bytes.split(|b| *b == b'\n').next())
        .unwrap_or_default();
    first_line.starts_with(b"#!") && first_line.windows(4).any(|w| w == b"node")
}

/// `config` with the native TypeScript server substituted when
/// [`select_typescript_server`] chose it: `command` becomes the `tsc` path and
/// `args` become `--lsp --stdio`; everything else is kept.
///
/// Logs the choice and its reason at info level and never fails, so a failed
/// selection falls back to `config` unchanged.
pub fn with_selected_typescript_server<'a>(
    config: &'a LspServerConfig,
    roots: &WorkspaceRoots,
    parent_env: impl ParentEnv,
) -> Cow<'a, LspServerConfig> {
    match select_typescript_server(config, roots, parent_env) {
        Some(TypescriptServerChoice::Native(tsc)) => {
            tracing::info!(
                server = %config.language_id,
                tsc = %tsc.path().display(),
                "starting the native TypeScript server (`tsc --lsp --stdio`): TypeScript 7 found outside the workspace"
            );
            let mut native = config.clone();
            let (command, args) = tsc.launch_parts();
            native.command = LaunchCommand::explicit(command);
            native.args = args;
            Cow::Owned(native)
        }
        Some(TypescriptServerChoice::Tsserver(reason)) => {
            tracing::info!(
                server = %config.language_id,
                "keeping typescript-language-server: {reason}"
            );
            Cow::Borrowed(config)
        }
        None => Cow::Borrowed(config),
    }
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
pub fn init_failure_hint(
    config: &LspServerConfig,
    workspace_roots: &WorkspaceRoots,
    parent_env: impl ParentEnv,
) -> Option<InitFailureHint> {
    if UserTsserverPath::of(config.initialization_options.as_ref()) != UserTsserverPath::Absent {
        return None;
    }
    let native = match resolve(config, parent_env)? {
        TsserverResolution::Unresolved(UnresolvedReason::NativeTypescriptNextToServer) => true,
        TsserverResolution::Unresolved(UnresolvedReason::NoTypescriptNextToServer) => {
            workspace_roots
                .canonical()
                .iter()
                .any(|root| nearest_native_typescript(root).is_some())
        }
        TsserverResolution::Pinned(_)
        | TsserverResolution::Unresolved(
            UnresolvedReason::ServerNotOnPath
            | UnresolvedReason::UnsupportedLauncher
            | UnresolvedReason::PackageRunner,
        ) => false,
    };
    native.then_some(InitFailureHint::NativeTypescriptOnly)
}

/// JSON pointer to the tsserver path inside `initialization_options`.
const TSSERVER_PATH_POINTER: &str = "/tsserver/path";

/// What `initialization_options.tsserver.path` says, read in one place for
/// selection, pinning and the untrusted-mode check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserTsserverPath {
    /// The options carry no `tsserver.path`.
    Absent,
    /// The user pinned a tsserver, which always wins.
    Path(PathBuf),
    /// `tsserver.path` is present but not a string; it counts as user-set, so
    /// nothing is pinned or selected over it.
    Invalid,
}

impl UserTsserverPath {
    /// Reads `tsserver.path` out of `options`.
    #[must_use]
    pub fn of(options: Option<&serde_json::Value>) -> Self {
        options
            .and_then(|options| options.pointer(TSSERVER_PATH_POINTER))
            .map_or(Self::Absent, |value| {
                value
                    .as_str()
                    .map_or(Self::Invalid, |path| Self::Path(PathBuf::from(path)))
            })
    }

    /// The configured path, when it is a valid one.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Path(path) => Some(path),
            Self::Absent | Self::Invalid => None,
        }
    }
}

/// The `tsserver.path` configured in `options`, if any.
#[must_use]
pub fn configured_tsserver_path(options: Option<&serde_json::Value>) -> Option<PathBuf> {
    UserTsserverPath::of(options).path().map(Path::to_path_buf)
}

/// The tsserver decision for one server, resolved once.
///
/// The path untrusted mode checks against the workspace is, by construction,
/// the canonical path [`Self::apply`] sends, so a filesystem change between the
/// check and the pin cannot put an unchecked path on the wire.
#[derive(Debug)]
pub struct TypescriptPlan {
    config: LspServerConfig,
    user: UserTsserverPath,
    resolution: Option<TsserverResolution>,
}

/// Plans the tsserver for `config`, which is the config that will be spawned
/// (after any native-server selection and untrusted hardening).
///
/// The launch is always resolved, even when the user pinned a tsserver, so
/// untrusted mode can still tell that a launcher is unpinnable. `PATH` is
/// read as the child sees it.
#[must_use]
pub fn plan_typescript(config: LspServerConfig, parent_env: impl ParentEnv) -> TypescriptPlan {
    let user = UserTsserverPath::of(config.initialization_options.as_ref());
    let resolution = resolve(&config, parent_env).map(|resolution| match resolution {
        TsserverResolution::Pinned(tsserver) => dunce::canonicalize(&tsserver).map_or(
            TsserverResolution::Unresolved(UnresolvedReason::NoTypescriptNextToServer),
            TsserverResolution::Pinned,
        ),
        unresolved @ TsserverResolution::Unresolved(_) => unresolved,
    });
    TypescriptPlan {
        config,
        user,
        resolution,
    }
}

impl TypescriptPlan {
    /// The canonical tsserver that would be pinned, when it lies inside
    /// `boundary`: the pin then names workspace code, which untrusted mode
    /// refuses.
    ///
    /// `None` when a user-set `tsserver.path` wins, when nothing is pinned, or
    /// when the pin lies outside `boundary`.
    #[must_use]
    pub fn pin_inside(&self, boundary: &WorkspaceRoots) -> Option<PathBuf> {
        if self.user != UserTsserverPath::Absent {
            return None;
        }
        let Some(TsserverResolution::Pinned(tsserver)) = &self.resolution else {
            return None;
        };
        boundary
            .contains_canonical(tsserver)
            .then(|| tsserver.clone())
    }

    /// Whether the server is launched in a way no tsserver can be pinned for:
    /// a package runner, or a wrapper or version-manager shim that hides the
    /// install. A user-set `tsserver.path` does not make the launcher itself
    /// vetted.
    #[must_use]
    pub const fn has_unpinnable_launcher(&self) -> bool {
        matches!(
            self.resolution,
            Some(TsserverResolution::Unresolved(
                UnresolvedReason::UnsupportedLauncher | UnresolvedReason::PackageRunner
            ))
        )
    }

    /// The config to spawn, with the bundled tsserver pinned when this plan
    /// launches typescript-language-server, and the pinned path.
    ///
    /// A user-supplied `tsserver.path` always wins. User options without one are
    /// left untouched, because merging would silently change their meaning; the
    /// skipped pin is logged. Failure to resolve a tsserver never prevents the
    /// server from starting.
    #[must_use]
    pub fn apply(self, roots: &WorkspaceRoots) -> (LspServerConfig, Option<PathBuf>) {
        let Self {
            mut config,
            user,
            resolution,
        } = self;
        let server = &config.language_id;
        if user == UserTsserverPath::Invalid {
            tracing::warn!(%server, "initialization_options.tsserver.path is not a string");
        }
        let Some(resolution) = resolution.filter(|_| user == UserTsserverPath::Absent) else {
            return (config, None);
        };
        if config.initialization_options.is_some() {
            tracing::warn!(
                %server,
                "tsserver pin skipped: initialization_options set without tsserver.path, \
                 so a workspace-supplied tsserver may run"
            );
            return (config, None);
        }
        let tsserver = match resolution {
            TsserverResolution::Pinned(tsserver) => tsserver,
            TsserverResolution::Unresolved(reason) => {
                tracing::warn!(
                    %server,
                    "tsserver not pinned: {reason}; a workspace-supplied tsserver may run"
                );
                return (config, None);
            }
        };
        if roots.contains_canonical(&tsserver) {
            tracing::warn!(
                %server,
                tsserver = %tsserver.display(),
                "typescript-language-server is installed inside the workspace; \
                 pinning it does not make the workspace trusted"
            );
        }
        let options = serde_json::to_value(TsserverInitOptions {
            tsserver: TsserverPath { path: &tsserver },
        })
        .inspect_err(|err| tracing::warn!(%err, "tsserver pin could not be serialized"))
        .ok();
        let pinned = options.is_some().then_some(tsserver);
        config.initialization_options = options;
        (config, pinned)
    }
}

/// Warns when a server configured with a tsserver path reports another source
/// in its `$/typescriptVersion` notification; other notifications are ignored.
///
/// Catches a stale pin on respawn, where the server silently falls through to
/// the workspace's tsserver.
pub fn warn_if_pin_ignored(configured: Option<&Path>, notif: &LspNotification, server: &ServerId) {
    let (Some(configured), LspNotification::Other { method, params }) = (configured, notif) else {
        return;
    };
    if method.as_ref() != TYPESCRIPT_VERSION_METHOD {
        return;
    }
    if let Some(ignored) = pin_ignored(params.as_ref()) {
        tracing::warn!(
            %server,
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
fn pinned_initialization_options(
    config: &LspServerConfig,
    roots: &WorkspaceRoots,
    parent_env: impl ParentEnv,
) -> Option<serde_json::Value> {
    plan_typescript(config.clone(), parent_env)
        .apply(roots)
        .0
        .initialization_options
}

/// The default TypeScript entry started through `command`, auto-selected only
/// when `command` is typescript-language-server.
#[cfg(test)]
fn typescript_entry(command: &str) -> LspServerConfig {
    let mut config = LspServerConfig::typescript();
    let command = ServerCommand::new(command).unwrap();
    config.command =
        LaunchCommand::auto(command.clone()).unwrap_or_else(|_| LaunchCommand::explicit(command));
    config
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

#[cfg(test)]
mod launch_tests {
    use std::{assert_matches, fs};

    use super::*;

    struct Install {
        _dir: tempfile::TempDir,
        base: PathBuf,
    }

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn write_executable(path: &Path, body: &str) {
        // Windows spawns an extensionless command as `<command>.exe`.
        let path = &if cfg!(windows) && path.extension().is_none() {
            path.with_added_extension("exe")
        } else {
            path.to_path_buf()
        };
        write(path, body);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn typescript(dir: &Path, version: &str, with_tsserver: bool) -> PathBuf {
        let package = dir.join("node_modules/typescript");
        write(
            &package.join("package.json"),
            &format!(r#"{{"version": "{version}"}}"#),
        );
        let tsserver = package.join(TSSERVER_IN_PACKAGE);
        if with_tsserver {
            write(&tsserver, "");
        }
        tsserver
    }

    fn server_package(modules_parent: &Path) {
        let package = modules_parent.join("node_modules").join(SERVER_STEM);
        write(&package.join("package.json"), "{}");
        write(&package.join("lib/cli.mjs"), "");
    }

    fn install() -> Install {
        let dir = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(dir.path()).unwrap();
        Install { _dir: dir, base }
    }

    fn config(command: &str, args: &[&str]) -> LspServerConfig {
        let mut config = super::typescript_entry(command);
        config.args = args.iter().map(ToString::to_string).collect();
        config
    }

    fn env_with_path(dir: &Path) -> impl ParentEnv {
        let path = std::env::join_paths([dir]).unwrap();
        move |key| (key == "PATH").then(|| path.clone())
    }

    fn npm_shim_install(shim: &str, version: &str, with_tsserver: bool) -> (Install, PathBuf) {
        let install = install();
        let npm = install.base.join("npm");
        write_executable(&npm.join(shim), "");
        server_package(&npm);
        let tsserver = typescript(&npm, version, with_tsserver);
        (install, tsserver)
    }

    fn pnpm_home(install: &Install) -> PathBuf {
        let home = install.base.join("pnpm");
        write_executable(&home.join(SERVER_STEM), "");
        home
    }

    fn windows_select(
        env_dir: &Path,
        roots: &WorkspaceRoots,
        host: HostOs,
    ) -> Option<TypescriptServerChoice> {
        select_typescript_server_on(
            &config(SERVER_STEM, &[]),
            roots,
            env_with_path(env_dir),
            host,
        )
    }

    /// An npm global prefix on Windows: the `tsc.cmd` shim, `node.exe` and the
    /// TypeScript 7 package under `node_modules`.
    fn windows_npm_prefix(with_node: bool) -> (Install, PathBuf, PathBuf) {
        let install = install();
        let prefix = install.base.join("npm");
        write(&prefix.join(NATIVE_TSC_WINDOWS_SHIM), "");
        typescript(&prefix, "7.0.2", false);
        let tsc = native_tsc_in(&prefix.join(TYPESCRIPT_RELATIVE));
        write(&tsc, "#!/usr/bin/env node\n");
        if with_node {
            write(&prefix.join("node.exe"), "");
        }
        (install, prefix, tsc)
    }

    #[test]
    fn test_windows_selects_the_tsc_cmd_shim_package_and_launches_through_node() {
        let (_install, prefix, tsc) = windows_npm_prefix(true);

        let choice = windows_select(&prefix, &WorkspaceRoots::default(), HostOs::Windows);

        let Some(TypescriptServerChoice::Native(native)) = choice else {
            panic!("expected the native server, got {choice:?}");
        };
        let (command, args) = native.launch_parts();
        assert_eq!(command, prefix.join("node.exe").to_str().unwrap());
        assert_eq!(args, [tsc.to_str().unwrap(), "--lsp", "--stdio"]);
    }

    #[test]
    fn test_windows_keeps_the_server_without_node_on_path() {
        let (_install, prefix, _tsc) = windows_npm_prefix(false);

        assert_eq!(
            windows_select(&prefix, &WorkspaceRoots::default(), HostOs::Windows),
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::NodeNotOnPath
            ))
        );
    }

    #[test]
    fn test_windows_maps_a_pnpm_home_shim_through_global() {
        let install = install();
        let home = install.base.join("pnpm");
        write(&home.join(NATIVE_TSC_WINDOWS_SHIM), "");
        write(&home.join("node.exe"), "");
        let store = home.join("global/5");
        typescript(&store, "7.0.2", false);
        write(&native_tsc_in(&store.join(TYPESCRIPT_RELATIVE)), "");

        let choice = windows_select(&home, &WorkspaceRoots::default(), HostOs::Windows);

        assert_matches!(choice, Some(TypescriptServerChoice::Native(_)));
    }

    #[test]
    fn test_windows_never_selects_a_shim_inside_the_workspace() {
        let (install, prefix, _tsc) = windows_npm_prefix(true);
        let roots = WorkspaceRoots::from_paths(std::slice::from_ref(&install.base)).unwrap();

        assert_eq!(
            windows_select(&prefix, &roots, HostOs::Windows),
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::NativeInsideWorkspace
            ))
        );
    }

    #[test]
    fn test_windows_never_runs_a_node_from_inside_the_workspace() {
        let (install, prefix, _tsc) = windows_npm_prefix(false);
        let workspace = install.base.join("ws");
        write(&workspace.join("node.exe"), "");
        let roots = WorkspaceRoots::from_paths(std::slice::from_ref(&workspace)).unwrap();
        let path = std::env::join_paths([&prefix, &workspace]).unwrap();
        let env = move |key: &str| (key == "PATH").then(|| path.clone());

        let choice =
            select_typescript_server_on(&config(SERVER_STEM, &[]), &roots, env, HostOs::Windows);

        assert_eq!(
            choice,
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::NodeInsideWorkspace
            ))
        );
    }

    #[test]
    fn test_other_hosts_do_not_look_for_the_tsc_cmd_shim() {
        let (_install, prefix, _tsc) = windows_npm_prefix(true);

        assert_eq!(
            windows_select(&prefix, &WorkspaceRoots::default(), HostOs::Other),
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::NoNativeOutsideWorkspace
            ))
        );
    }

    const fn unsupported() -> TsserverResolution {
        TsserverResolution::Unresolved(UnresolvedReason::UnsupportedLauncher)
    }

    #[test]
    fn test_classify_covers_every_launch_kind() {
        let cli = if cfg!(windows) {
            "C:\\lib\\typescript-language-server\\lib\\cli.mjs"
        } else {
            "/lib/typescript-language-server/lib/cli.mjs"
        };
        let cases = [
            (config(SERVER_STEM, &["--stdio"]), "server"),
            (config("node", &[cli, "--stdio"]), "script"),
            (config("bun", &[cli]), "script"),
            (config("npx", &[SERVER_STEM, "--stdio"]), "runner"),
            (config("bunx", &[SERVER_STEM]), "runner"),
            (config("pnpm", &["dlx", SERVER_STEM]), "runner"),
            (config("yarn", &["dlx", SERVER_STEM]), "runner"),
            (
                config("deno", &["run", "npm:typescript-language-server"]),
                "runner",
            ),
            (
                config("node", &["lib/typescript-language-server/cli.mjs"]),
                "wrapper",
            ),
            (config("my-wrapper", &[SERVER_STEM]), "wrapper"),
        ];
        for (config, expected) in &cases {
            let actual = match classify(config) {
                Some(Launch::Server(_)) => "server",
                Some(Launch::Script(_)) => "script",
                Some(Launch::PackageRunner) => "runner",
                Some(Launch::UnknownWrapper) => "wrapper",
                None => "none",
            };
            assert_eq!(actual, *expected, "{} {:?}", config.command, config.args);
        }
        assert_eq!(classify(&config("pyright-langserver", &["--stdio"])), None);
        assert_eq!(classify(&config("deno", &["run", "main.ts"])), None);
    }

    #[test]
    fn test_npm_shims_are_pinned() {
        for shim in [
            "typescript-language-server.cmd",
            "typescript-language-server.ps1",
            SERVER_STEM,
        ] {
            let (install, tsserver) = npm_shim_install(shim, "5.4.0", true);
            let resolved = resolve(&config(shim, &[]), env_with_path(&install.base.join("npm")));
            assert_eq!(
                resolved,
                Some(TsserverResolution::Pinned(tsserver)),
                "{shim}"
            );
        }
    }

    #[test]
    fn test_shim_without_server_package_is_unsupported() {
        let install = install();
        let npm = install.base.join("npm");
        write_executable(&npm.join("typescript-language-server.cmd"), "");
        let resolved = resolve(
            &config("typescript-language-server.cmd", &[]),
            env_with_path(&npm),
        );
        assert_eq!(resolved, Some(unsupported()));
    }

    #[test]
    fn test_pnpm_global_install_is_pinned() {
        let install = install();
        let home = pnpm_home(&install);
        let store = home.join("global/5");
        server_package(&store);
        let tsserver = typescript(&store, "5.4.0", true);
        let resolved = resolve(&config(SERVER_STEM, &[]), env_with_path(&home));
        assert_eq!(resolved, Some(TsserverResolution::Pinned(tsserver)));
    }

    #[test]
    fn test_pnpm_global_with_two_server_packages_is_ambiguous() {
        let install = install();
        let home = pnpm_home(&install);
        for version in ["4", "5"] {
            let store = home.join("global").join(version);
            server_package(&store);
            typescript(&store, "5.4.0", true);
        }
        let resolved = resolve(&config(SERVER_STEM, &[]), env_with_path(&home));
        assert_eq!(resolved, Some(unsupported()));
    }

    #[test]
    fn test_pnpm_global_with_too_many_entries_is_ambiguous() {
        let install = install();
        let home = pnpm_home(&install);
        let store = home.join("global/0");
        server_package(&store);
        typescript(&store, "5.4.0", true);
        for index in 1..=MAX_PNPM_GLOBAL_ENTRIES {
            fs::create_dir_all(home.join("global").join(format!("extra{index}"))).unwrap();
        }
        let resolved = resolve(&config(SERVER_STEM, &[]), env_with_path(&home));
        assert_eq!(resolved, Some(unsupported()));
    }

    #[test]
    fn test_pnpm_global_with_exactly_the_entry_cap_is_still_pinned() {
        let install = install();
        let home = pnpm_home(&install);
        let store = home.join("global/0");
        server_package(&store);
        let tsserver = typescript(&store, "5.4.0", true);
        for index in 1..MAX_PNPM_GLOBAL_ENTRIES {
            fs::create_dir_all(home.join("global").join(format!("extra{index}"))).unwrap();
        }
        let resolved = resolve(&config(SERVER_STEM, &[]), env_with_path(&home));
        assert_eq!(resolved, Some(TsserverResolution::Pinned(tsserver)));
    }

    #[cfg(unix)]
    #[test]
    fn test_shims_are_never_executed() {
        let install = install();
        let npm = install.base.join("npm");
        let marker = install.base.join("shim-ran");
        for shim in [SERVER_STEM, "typescript-language-server.cmd"] {
            write_executable(
                &npm.join(shim),
                &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
            );
        }
        server_package(&npm);
        typescript(&npm, "5.4.0", true);
        let home = pnpm_home(&install);
        write_executable(
            &home.join(SERVER_STEM),
            &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        );

        for dir in [&npm, &home] {
            resolve(&config(SERVER_STEM, &[]), env_with_path(dir));
            resolve(
                &config("typescript-language-server.cmd", &[]),
                env_with_path(dir),
            );
            init_failure_hint(
                &config(SERVER_STEM, &[]),
                &WorkspaceRoots::default(),
                env_with_path(dir),
            );
            pinned_initialization_options(
                &config(SERVER_STEM, &[]),
                &WorkspaceRoots::default(),
                env_with_path(dir),
            );
        }

        assert!(!marker.exists(), "a shim was executed");
    }

    #[cfg(unix)]
    #[test]
    fn test_non_executable_server_on_path_does_not_steer_the_pin() {
        let install = install();
        let decoy = install.base.join("decoy");
        write(&decoy.join(SERVER_STEM), "");
        server_package(&decoy);
        let decoy_tsserver = typescript(&decoy, "5.4.0", true);
        let real = install.base.join("real");
        write_executable(&real.join(SERVER_STEM), "");
        server_package(&real);
        let real_tsserver = typescript(&real, "5.4.0", true);
        let path = std::env::join_paths([&decoy, &real]).unwrap();

        let resolved = resolve(&config(SERVER_STEM, &[]), |key| {
            (key == "PATH").then(|| path.clone())
        });

        assert_eq!(resolved, Some(TsserverResolution::Pinned(real_tsserver)));
        assert_ne!(resolved, Some(TsserverResolution::Pinned(decoy_tsserver)));
    }

    #[test]
    fn test_absolute_node_script_is_pinned() {
        let install = install();
        let npm = install.base.join("npm");
        server_package(&npm);
        let tsserver = typescript(&npm, "5.4.0", true);
        let cli = npm
            .join("node_modules")
            .join(SERVER_STEM)
            .join("lib/cli.mjs");
        for interpreter in ["node", "bun"] {
            let config = config(interpreter, &[cli.to_str().unwrap(), "--stdio"]);
            assert_eq!(
                resolve(&config, |_| None),
                Some(TsserverResolution::Pinned(tsserver.clone())),
                "{interpreter}"
            );
        }
    }

    #[test]
    fn test_package_runner_warning_names_user_tsserver_path() {
        for config in [
            config("npx", &[SERVER_STEM, "--stdio"]),
            config("deno", &["run", "npm:typescript-language-server"]),
        ] {
            assert_eq!(
                resolve(&config, |_| None),
                Some(TsserverResolution::Unresolved(
                    UnresolvedReason::PackageRunner
                ))
            );
        }
        for reason in [
            UnresolvedReason::PackageRunner,
            UnresolvedReason::UnsupportedLauncher,
        ] {
            assert!(
                reason
                    .to_string()
                    .contains("initialization_options.tsserver.path")
            );
        }
    }

    #[test]
    fn test_init_failure_hint_covers_shim_install_with_typescript_seven() {
        let (install, _) = npm_shim_install(SERVER_STEM, "7.0.1", false);
        let hint = init_failure_hint(
            &config(SERVER_STEM, &[]),
            &WorkspaceRoots::default(),
            env_with_path(&install.base.join("npm")),
        );
        assert_eq!(hint, Some(InitFailureHint::NativeTypescriptOnly));
    }

    #[test]
    fn test_package_runner_gets_no_init_failure_hint() {
        let hint = init_failure_hint(
            &config("npx", &[SERVER_STEM]),
            &WorkspaceRoots::default(),
            |_| None,
        );
        assert_eq!(hint, None);
    }

    #[cfg(unix)]
    #[test]
    fn test_selection_auto_picks_native_for_pnpm_shim_with_typescript_seven() {
        use std::assert_matches;
        use std::os::unix::fs::PermissionsExt as _;

        let install = install();
        let home = pnpm_home(&install);
        let store = home.join("global/5");
        server_package(&store);
        typescript(&store, "7.0.1", false);
        let tsc = store.join("node_modules/typescript/bin/tsc");
        write(&tsc, "#!/bin/sh\n");
        fs::set_permissions(&tsc, fs::Permissions::from_mode(0o755)).unwrap();

        let auto = config(SERVER_STEM, &[]);
        let choice =
            select_typescript_server(&auto, &WorkspaceRoots::default(), env_with_path(&home));
        assert_matches!(choice, Some(TypescriptServerChoice::Native(_)));
    }

    #[cfg(unix)]
    #[test]
    fn test_pnpm_store_symlink_resolves_sibling_typescript() {
        let install = install();
        let home = pnpm_home(&install);
        let store = home.join("global/5/node_modules/.pnpm/typescript-language-server@5.1.3");
        server_package(&store);
        let tsserver = typescript(&store, "5.4.0", true);
        let global_modules = home.join("global/5/node_modules");
        std::os::unix::fs::symlink(
            store.join("node_modules").join(SERVER_STEM),
            global_modules.join(SERVER_STEM),
        )
        .unwrap();
        let resolved = resolve(&config(SERVER_STEM, &[]), env_with_path(&home));
        assert_eq!(resolved, Some(TsserverResolution::Pinned(tsserver)));
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::{assert_matches, fs};

    use super::*;
    use crate::config::{BuiltinServer, ServerCommand};

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
        fs::set_permissions(
            package.join("lib/cli.mjs"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
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
        super::typescript_entry(command)
    }

    fn env_with_path(path: &Path) -> impl ParentEnv {
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
        let resolved = resolve(&config, |_| Some(std::ffi::OsString::from("/nonexistent")));
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
        fs::set_permissions(
            shim_dir.join(SERVER_STEM),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let resolved = resolve(&config(SERVER_STEM), env_with_path(&shim_dir));
        assert_eq!(
            resolved,
            Some(TsserverResolution::Unresolved(
                UnresolvedReason::UnsupportedLauncher
            ))
        );
    }

    #[test]
    fn test_resolve_cmd_shim_not_on_path() {
        let resolved = resolve(&config("typescript-language-server.cmd"), |_| None);
        assert_eq!(
            resolved,
            Some(TsserverResolution::Unresolved(
                UnresolvedReason::ServerNotOnPath
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
        for (command, args, reason) in [
            (
                "npx",
                vec!["typescript-language-server", "--stdio"],
                UnresolvedReason::PackageRunner,
            ),
            (
                "bunx",
                vec!["typescript-language-server@5.1.3", "--stdio"],
                UnresolvedReason::PackageRunner,
            ),
            (
                "node",
                vec!["/opt/lib/node_modules/typescript-language-server/lib/cli.mjs"],
                UnresolvedReason::UnsupportedLauncher,
            ),
        ] {
            let mut config = config(command);
            config.args = args.into_iter().map(String::from).collect();
            assert_eq!(
                resolve(&config, |_| None),
                Some(TsserverResolution::Unresolved(reason)),
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
            &WorkspaceRoots::from_paths(std::slice::from_ref(&ws)).unwrap(),
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
            &WorkspaceRoots::from_paths(std::slice::from_ref(&layout.base)).unwrap(),
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
        let roots = WorkspaceRoots::for_test(roots.to_vec(), vec![]);
        init_failure_hint(config, &roots, env_with_path(&layout.bin))
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
        let mut pinned = config(SERVER_STEM);
        pinned.initialization_options = Some(options);
        assert_eq!(
            init_failure_hint(
                &pinned,
                &WorkspaceRoots::default(),
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
            crate::test_lsp::make_fifo(&manifest);
        }
        let bin = layout.bin;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let env = env_with_path(&bin);
            let resolved = resolve(&config(SERVER_STEM), &env);
            let roots = WorkspaceRoots::for_test(vec![root], vec![]);
            let hinted = init_failure_hint(&config(SERVER_STEM), &roots, &env);
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
        let err = LspServer::spawn(ServerInitConfig::new(
            server_config,
            WorkspaceRoots::for_test(vec![root], vec![]),
            crate::config::PositionEncodings::DEFAULT,
            std::sync::Arc::default(),
        ))
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

    /// Writes an executable `<package>/bin/tsc` and returns its path.
    fn write_native_tsc(package: &Path, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let tsc = native_tsc_in(package);
        fs::create_dir_all(tsc.parent().unwrap()).unwrap();
        fs::write(&tsc, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tsc, fs::Permissions::from_mode(mode)).unwrap();
        tsc
    }

    fn native_tsc(path: &Path) -> NativeTsc {
        NativeTsc {
            tsc: ServerCommand::new(path.to_str().unwrap()).unwrap(),
            launch: NativeLaunch::Direct,
        }
    }

    fn roots(paths: &[&Path]) -> WorkspaceRoots {
        let paths: Vec<PathBuf> = paths.iter().map(|path| path.to_path_buf()).collect();
        WorkspaceRoots::from_paths(&paths).unwrap()
    }

    fn select(
        layout: &Layout,
        config: &LspServerConfig,
        roots: &WorkspaceRoots,
    ) -> Option<TypescriptServerChoice> {
        select_typescript_server(config, roots, env_with_path(&layout.bin))
    }

    fn native_install_next_to_server(layout: &Layout) -> PathBuf {
        let package = write_typescript(&layout.base.join("prefix/lib"), "7.0.2", false);
        write_native_tsc(&package, 0o755)
    }

    #[test]
    fn test_auto_selects_native_tsc_next_to_server() {
        let layout = global_install(false);
        let tsc = dunce::canonicalize(native_install_next_to_server(&layout)).unwrap();
        let choice = select(&layout, &config(SERVER_STEM), &WorkspaceRoots::default());
        assert_eq!(
            choice,
            Some(TypescriptServerChoice::Native(native_tsc(&tsc)))
        );

        let base = config(SERVER_STEM);
        let effective = with_selected_typescript_server(
            &base,
            &WorkspaceRoots::default(),
            env_with_path(&layout.bin),
        );
        assert_eq!(effective.command, tsc.to_str().unwrap());
        assert_eq!(effective.args, ["--lsp", "--stdio"]);
        assert_eq!(effective.language_id, base.language_id);
        assert!(effective.command.is_explicit());
    }

    #[test]
    fn test_auto_keeps_tsserver_when_javascript_typescript_is_installed() {
        let layout = global_install(true);
        let base = config(SERVER_STEM);
        assert_eq!(
            select(&layout, &base, &WorkspaceRoots::default()),
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::TsserverPinned
            ))
        );
        let effective = with_selected_typescript_server(
            &base,
            &WorkspaceRoots::default(),
            env_with_path(&layout.bin),
        );
        assert_matches!(effective, Cow::Borrowed(_));
    }

    #[test]
    fn test_explicit_entry_is_never_replaced() {
        let layout = global_install(false);
        native_install_next_to_server(&layout);
        let mut base = config(SERVER_STEM);
        base.command =
            crate::config::LaunchCommand::explicit(base.command.server_command().clone());
        assert_eq!(select(&layout, &base, &WorkspaceRoots::default()), None);
        let effective = with_selected_typescript_server(
            &base,
            &WorkspaceRoots::default(),
            env_with_path(&layout.bin),
        );
        assert_matches!(effective, Cow::Borrowed(_));
        assert_eq!(effective.command, SERVER_STEM);
    }

    #[test]
    fn test_native_tsc_inside_workspace_is_never_selected() {
        let layout = global_install(false);
        native_install_next_to_server(&layout);
        let choice = select(&layout, &config(SERVER_STEM), &roots(&[&layout.base]));
        assert_eq!(
            choice,
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::NativeInsideWorkspace
            ))
        );
    }

    #[test]
    fn test_native_tsc_inside_symlinked_workspace_root_is_never_selected() {
        let layout = global_install(false);
        native_install_next_to_server(&layout);
        let alias = tempfile::tempdir().unwrap();
        let link = alias.path().join("ws-link");
        std::os::unix::fs::symlink(&layout.base, &link).unwrap();
        let choice = select(&layout, &config(SERVER_STEM), &roots(&[&link]));
        assert_eq!(
            choice,
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::NativeInsideWorkspace
            ))
        );
    }

    #[test]
    fn test_non_executable_native_tsc_is_kept_out() {
        let layout = global_install(false);
        let package = write_typescript(&layout.base.join("prefix/lib"), "7.0.2", false);
        write_native_tsc(&package, 0o644);
        assert_eq!(
            select(&layout, &config(SERVER_STEM), &WorkspaceRoots::default()),
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::NotExecutable
            ))
        );
    }

    #[test]
    fn test_path_tsc_is_selected_when_server_is_not_on_path() {
        let layout = global_install(false);
        let tsc = dunce::canonicalize(native_install_next_to_server(&layout)).unwrap();
        let tsc_bin = layout.base.join("tsc-bin");
        fs::create_dir_all(&tsc_bin).unwrap();
        std::os::unix::fs::symlink(&tsc, tsc_bin.join("tsc")).unwrap();
        let choice = select_typescript_server(
            &config(SERVER_STEM),
            &WorkspaceRoots::default(),
            env_with_path(&tsc_bin),
        );
        assert_eq!(
            choice,
            Some(TypescriptServerChoice::Native(native_tsc(&tsc)))
        );
    }

    #[test]
    fn test_path_tsc_that_is_not_a_typescript_bin_is_rejected() {
        let layout = global_install(false);
        let wrapper_dir = layout.base.join("wrappers");
        fs::create_dir_all(&wrapper_dir).unwrap();
        let wrapper = wrapper_dir.join("tsc");
        fs::write(&wrapper, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let choice = select_typescript_server(
            &config(SERVER_STEM),
            &WorkspaceRoots::default(),
            env_with_path(&wrapper_dir),
        );
        assert_eq!(
            choice,
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::NoNativeOutsideWorkspace
            ))
        );
    }

    #[test]
    fn test_path_tsc_of_old_typescript_is_rejected() {
        let layout = global_install(false);
        let package = write_typescript(&layout.base.join("global"), "5.4.0", true);
        let tsc = write_native_tsc(&package, 0o755);
        let tsc_bin = layout.base.join("tsc-bin");
        fs::create_dir_all(&tsc_bin).unwrap();
        std::os::unix::fs::symlink(&tsc, tsc_bin.join("tsc")).unwrap();
        let choice = select_typescript_server(
            &config(SERVER_STEM),
            &WorkspaceRoots::default(),
            env_with_path(&tsc_bin),
        );
        assert_eq!(
            choice,
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::NoNativeOutsideWorkspace
            ))
        );
    }

    #[test]
    fn test_cmd_shim_launcher_keeps_tsserver() {
        let layout = global_install(false);
        let cmd = layout.bin.join("typescript-language-server.cmd");
        fs::write(&cmd, "").unwrap();
        fs::set_permissions(&cmd, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(
            select(
                &layout,
                &config("typescript-language-server.cmd"),
                &WorkspaceRoots::default()
            ),
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::UnsupportedLauncher
            ))
        );
    }

    #[test]
    fn test_user_tsserver_path_keeps_typescript_language_server() {
        let layout = global_install(false);
        native_install_next_to_server(&layout);
        let mut base = config(SERVER_STEM);
        base.initialization_options =
            Some(serde_json::json!({"tsserver": {"path": "/opt/ts6/lib/tsserver.js"}}));
        assert_eq!(
            select(&layout, &base, &WorkspaceRoots::default()),
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::UserTsserverPath
            ))
        );
        base.initialization_options = Some(serde_json::json!({"preferences": {}}));
        assert_matches!(
            select(&layout, &base, &WorkspaceRoots::default()),
            Some(TypescriptServerChoice::Native(_))
        );
    }

    #[test]
    fn test_path_entry_inside_workspace_symlinked_to_outside_tsc_is_rejected() {
        let layout = global_install(false);
        let tsc = native_install_next_to_server(&layout);
        let ws = layout.base.join("ws");
        let ws_bin = ws.join("bin");
        fs::create_dir_all(&ws_bin).unwrap();
        std::os::unix::fs::symlink(&tsc, ws_bin.join("tsc")).unwrap();
        let outside = layout.base.join("prefix");
        let choice =
            select_typescript_server(&config(SERVER_STEM), &roots(&[&ws]), env_with_path(&ws_bin));
        assert_eq!(
            choice,
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::NativeInsideWorkspace
            ))
        );
        assert!(!roots(&[&ws]).contains_canonical(&outside));
    }

    #[test]
    fn test_node_script_tsc_needs_node_on_path() {
        use std::os::unix::fs::PermissionsExt as _;

        let layout = global_install(false);
        let tsc = native_install_next_to_server(&layout);
        fs::write(&tsc, "#!/usr/bin/env node\n").unwrap();
        fs::set_permissions(&tsc, fs::Permissions::from_mode(0o755)).unwrap();
        let base = config(SERVER_STEM);
        assert_eq!(
            select(&layout, &base, &WorkspaceRoots::default()),
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::NodeNotOnPath
            ))
        );

        let node_dir = layout.base.join("node-bin");
        fs::create_dir_all(&node_dir).unwrap();
        let node = node_dir.join("node");
        fs::write(&node, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([&layout.bin, &node_dir]).unwrap();
        let choice = select_typescript_server(
            &base,
            &WorkspaceRoots::default(),
            env_with_path(Path::new(&path)),
        );
        assert_matches!(choice, Some(TypescriptServerChoice::Native(_)));
    }

    #[test]
    fn test_selection_ignores_non_typescript_servers() {
        let rust = LspServerConfig::rust_analyzer();
        assert!(crate::config::LaunchCommand::auto(rust.command.server_command().clone()).is_err());
        assert_eq!(
            select_typescript_server(&rust, &WorkspaceRoots::default(), |_| None),
            None
        );
    }

    #[test]
    fn test_user_tsserver_path_distinguishes_absent_path_and_invalid() {
        let of = |value: serde_json::Value| UserTsserverPath::of(Some(&value));

        assert_eq!(UserTsserverPath::of(None), UserTsserverPath::Absent);
        assert_eq!(of(serde_json::json!({})), UserTsserverPath::Absent);
        assert_eq!(
            of(serde_json::json!({"tsserver": {"path": "/ts/tsserver.js"}})),
            UserTsserverPath::Path(PathBuf::from("/ts/tsserver.js"))
        );
        assert_eq!(
            of(serde_json::json!({"tsserver": {"path": 42}})),
            UserTsserverPath::Invalid
        );
    }

    #[test]
    fn test_invalid_tsserver_path_blocks_native_selection_and_pinning() {
        let layout = global_install(true);
        let mut auto = config(SERVER_STEM);
        auto.initialization_options = Some(serde_json::json!({"tsserver": {"path": 42}}));

        let choice = select_typescript_server(
            &auto,
            &WorkspaceRoots::default(),
            env_with_path(&layout.bin),
        );
        let pinned = pinned_initialization_options(
            &auto,
            &WorkspaceRoots::default(),
            env_with_path(&layout.bin),
        );

        assert_eq!(
            choice,
            Some(TypescriptServerChoice::Tsserver(
                TsserverKept::UserTsserverPath
            ))
        );
        assert_eq!(pinned, auto.initialization_options);
    }

    #[test]
    fn test_plan_pins_the_canonical_path_and_checks_the_same_one() {
        let layout = global_install(true);
        let roots = WorkspaceRoots::default();
        let plan = plan_typescript(config(SERVER_STEM), env_with_path(&layout.bin));
        let boundary = WorkspaceRoots::from_paths(std::slice::from_ref(&layout.base)).unwrap();

        assert_eq!(plan.pin_inside(&boundary), Some(layout.tsserver.clone()));
        let (applied, pinned) = plan.apply(&roots);

        assert_eq!(pinned.as_deref(), Some(layout.tsserver.as_path()));
        assert_eq!(
            configured_tsserver_path(applied.initialization_options.as_ref()),
            Some(layout.tsserver)
        );
    }

    #[test]
    fn test_plan_flags_a_version_manager_shim_as_an_unpinnable_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let shims = dunce::canonicalize(dir.path()).unwrap().join(".volta/bin");
        fs::create_dir_all(&shims).unwrap();
        let shim = shims.join(SERVER_STEM);
        fs::write(&shim, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        let plan = plan_typescript(config(SERVER_STEM), env_with_path(&shims));

        assert!(plan.has_unpinnable_launcher());
        assert_eq!(
            plan.apply(&WorkspaceRoots::default())
                .0
                .initialization_options,
            None
        );
    }

    #[test]
    fn test_plan_of_a_pinnable_install_has_a_pinnable_launcher() {
        let layout = global_install(true);

        let plan = plan_typescript(config(SERVER_STEM), env_with_path(&layout.bin));

        assert!(!plan.has_unpinnable_launcher());
    }

    #[test]
    fn test_plan_flags_a_package_runner_as_an_unpinnable_launcher() {
        let mut runner = config("npx");
        runner.args = vec![SERVER_STEM.to_owned()];

        let plan = plan_typescript(runner, |_| None);

        assert!(plan.has_unpinnable_launcher());
    }

    #[test]
    fn test_plan_ignores_servers_that_are_not_typescript() {
        let rust = LspServerConfig::rust_analyzer();

        let plan = plan_typescript(rust, |_| None);

        assert!(!plan.has_unpinnable_launcher());
        assert_eq!(plan.pin_inside(&WorkspaceRoots::default()), None);
    }
}
