//! LSP server configuration types.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

use super::bounded_secs::TimeoutSecs;
use super::command_stem::CommandStem;
use super::host_os::HostOs;
use super::language_id::LanguageId;
#[cfg(test)]
use super::limits::DEFAULT_HEURISTICS_MAX_DEPTH;
use super::limits::SearchDepth;
use super::patterns::{FilePattern, ProjectMarker};
use super::routing::{ServerId, ToolSet};
use super::server_env::{EnvKey, ServerEnv};
use super::settings::LspSettings;
use super::text_newtype::impl_text_newtype;
use crate::bridge::IndexingPolicy;
use crate::error::ConfigError;

/// Directories excluded from recursive marker search.
/// These are well-known directories that should never contain project markers.
const EXCLUDED_DIRECTORIES: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    "build",
    "dist",
    ".cargo",
    ".rustup",
    "vendor",
    "coverage",
    ".next",
    ".nuxt",
];

/// Heuristics for determining if an LSP server should be spawned.
///
/// Used to prevent spawning servers in projects where they are not applicable
/// (e.g., rust-analyzer in a Python-only project).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerHeuristics {
    /// Files or directories that indicate this server is applicable.
    ///
    /// The server will spawn if ANY of these markers exist anywhere in the workspace tree
    /// (searched recursively up to `heuristics_max_depth`). Well-known directories like
    /// `node_modules`, `target`, `.git` are excluded from the search.
    ///
    /// If empty, the server will always attempt to spawn.
    #[serde(default)]
    pub project_markers: Vec<ProjectMarker>,
}

impl ServerHeuristics {
    /// Create heuristics with the given project markers.
    #[must_use]
    pub fn with_markers(markers: impl IntoIterator<Item = ProjectMarker>) -> Self {
        Self {
            project_markers: markers.into_iter().collect(),
        }
    }
}

/// Calls `visit` with the name of every entry under `workspace_root`, within
/// `max_depth`, skipping well-known generated directories and gitignored ones,
/// until it breaks.
fn walk_names(
    workspace_root: &Path,
    max_depth: SearchDepth,
    mut visit: impl FnMut(&str) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let mut builder = WalkBuilder::new(workspace_root);
    // `standard_filters(false)` (bulk setter, last-write-wins) must run first or it
    // undoes the overrides below; `.git_ignore(true)` itself is then a no-op outside
    // an actual git repo (`require_git` defaults true).
    builder
        .standard_filters(false)
        .max_depth(Some(max_depth.get()))
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false)
        .filter_entry(|entry| {
            // Skip excluded directories entirely (prevents descending into them)
            if entry.file_type().is_some_and(|ft| ft.is_dir())
                && let Some(name) = entry.file_name().to_str()
                && EXCLUDED_DIRECTORIES.contains(&name)
            {
                return false;
            }
            true
        });

    for entry in builder.build().flatten() {
        if let Some(name) = entry.path().file_name().and_then(OsStr::to_str) {
            visit(name)?;
        }
    }
    ControlFlow::Continue(())
}

/// The project markers found under the workspace roots, collected with one
/// tree walk per root however many servers are configured.
///
/// A server whose markers exist nowhere would otherwise cost a full walk of
/// its own. Blocking: run it off the async workers.
#[derive(Debug, Clone, Default)]
pub struct MarkerScan {
    found: Vec<ProjectMarker>,
}

impl MarkerScan {
    /// Looks for every marker any of `servers` asks for under `roots`: the
    /// roots themselves first, then one walk per root while a marker is still
    /// missing, stopping as soon as all are found.
    #[must_use]
    pub fn collect<'a>(
        roots: &[PathBuf],
        servers: impl IntoIterator<Item = &'a LspServerConfig>,
        max_depth: SearchDepth,
    ) -> Self {
        let mut missing: Vec<&ProjectMarker> = Vec::new();
        for marker in servers
            .into_iter()
            .filter_map(|server| server.heuristics.as_ref())
            .flat_map(|heuristics| &heuristics.project_markers)
        {
            if !missing.contains(&marker) {
                missing.push(marker);
            }
        }
        let mut found = Vec::new();
        for root in roots {
            if missing.is_empty() {
                break;
            }
            missing.retain(|marker| {
                let present = root.join(marker.as_str()).exists();
                if present {
                    found.push((*marker).clone());
                }
                !present
            });
            if missing.is_empty() {
                break;
            }
            let all_found = walk_names(root, max_depth, |name| {
                if let Some(index) = missing.iter().position(|marker| *marker == name) {
                    found.push(missing.swap_remove(index).clone());
                }
                if missing.is_empty() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .is_break();
            if all_found {
                break;
            }
        }
        Self { found }
    }

    /// Whether `server` applies to the scanned workspace: it has no markers,
    /// or at least one was found.
    #[must_use]
    pub fn applies_to(&self, server: &LspServerConfig) -> bool {
        server.heuristics.as_ref().is_none_or(|heuristics| {
            heuristics.project_markers.is_empty()
                || heuristics
                    .project_markers
                    .iter()
                    .any(|marker| self.found.contains(marker))
        })
    }
}

/// Whether mcpls may replace a server entry's `command` and `args` at startup.
///
/// Records provenance and consent, not the chosen server: only the generated
/// default TypeScript entry carries [`Self::Auto`], so a hand-written entry
/// is never rewritten.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ServerSelection;
///
/// assert_eq!(ServerSelection::default(), ServerSelection::Explicit);
/// assert_eq!(serde_json::to_string(&ServerSelection::Auto).unwrap(), "\"auto\"");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerSelection {
    /// Start exactly the configured `command` and `args`.
    #[default]
    Explicit,
    /// Start the native TypeScript server (`tsc --lsp --stdio`) instead of
    /// the configured `typescript-language-server` when a TypeScript 7
    /// install outside every workspace root is found. Valid only on
    /// `typescript-language-server` entries.
    Auto,
}

impl ServerSelection {
    /// Whether this is the default [`Self::Explicit`] selection, so the
    /// serializer omits the key from entries that carry it.
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde skip_serializing_if passes a reference"
    )]
    #[must_use]
    pub(crate) const fn is_explicit(&self) -> bool {
        matches!(self, Self::Explicit)
    }
}

/// A server command was empty or whitespace-only.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("command cannot be blank")]
pub struct InvalidServerCommand;

/// The non-blank executable name or path of an LSP server.
///
/// Deserializes from a TOML string and rejects a blank one at load time.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ServerCommand;
///
/// let command = ServerCommand::new("rust-analyzer").unwrap();
/// assert_eq!(command, "rust-analyzer");
/// assert!(ServerCommand::new("  ").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServerCommand(Cow<'static, str>);

impl_text_newtype!(ServerCommand, InvalidServerCommand, non_blank, "command");

impl AsRef<OsStr> for ServerCommand {
    fn as_ref(&self) -> &OsStr {
        OsStr::new(self.as_str())
    }
}

impl AsRef<Path> for ServerCommand {
    fn as_ref(&self) -> &Path {
        Path::new(self.as_str())
    }
}

/// `selection = "auto"` on a command that is not typescript-language-server.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("selection = \"auto\" is only valid for typescript-language-server entries")]
pub struct InvalidAutoSelection;

/// The command of a server entry together with the consent to replace it.
///
/// Binds [`ServerCommand`] to [`ServerSelection`] so that [`ServerSelection::Auto`]
/// can only accompany a typescript-language-server command; the combination
/// `rust-analyzer` plus `auto` cannot be built. The fields are private, so a
/// command cannot be swapped without re-checking the selection: use
/// [`Self::retarget`].
///
/// Reads like the [`ServerCommand`] it wraps (`as_str`, `Display`, `AsRef`,
/// comparison with `&str`).
///
/// # Examples
///
/// ```
/// use mcpls_core::config::{LaunchCommand, ServerCommand, ServerSelection};
///
/// let tsls = ServerCommand::new("typescript-language-server").unwrap();
/// let auto = LaunchCommand::auto(tsls).unwrap();
/// assert_eq!(auto.selection(), ServerSelection::Auto);
///
/// let rust = ServerCommand::new("rust-analyzer").unwrap();
/// assert!(LaunchCommand::auto(rust.clone()).is_err());
/// assert!(LaunchCommand::explicit(rust).is_explicit());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchCommand {
    command: ServerCommand,
    selection: ServerSelection,
}

impl LaunchCommand {
    /// `command` started exactly as written.
    #[must_use]
    pub const fn explicit(command: ServerCommand) -> Self {
        Self {
            command,
            selection: ServerSelection::Explicit,
        }
    }

    /// `command` that mcpls may replace with the native TypeScript server.
    ///
    /// # Errors
    ///
    /// [`InvalidAutoSelection`] unless `command` is typescript-language-server.
    pub fn auto(command: ServerCommand) -> Result<Self, InvalidAutoSelection> {
        Self::new(command, ServerSelection::Auto)
    }

    /// `command` with `selection`.
    ///
    /// # Errors
    ///
    /// [`InvalidAutoSelection`] for [`ServerSelection::Auto`] on a command that
    /// is not typescript-language-server.
    pub fn new(
        command: ServerCommand,
        selection: ServerSelection,
    ) -> Result<Self, InvalidAutoSelection> {
        if selection == ServerSelection::Auto
            && !BuiltinServer::TypescriptLanguageServer.matches_command(command.as_str())
        {
            return Err(InvalidAutoSelection);
        }
        Ok(Self { command, selection })
    }

    /// The default entry's command: typescript-language-server, auto-selected.
    const fn typescript() -> Self {
        Self {
            command: ServerCommand::from_static(BuiltinServer::TypescriptLanguageServer.command()),
            selection: ServerSelection::Auto,
        }
    }

    /// This launch with `command` in place of the current one, keeping the
    /// selection.
    ///
    /// # Errors
    ///
    /// [`InvalidAutoSelection`] when the selection is auto and `command` is not
    /// typescript-language-server.
    pub fn retarget(self, command: ServerCommand) -> Result<Self, InvalidAutoSelection> {
        Self::new(command, self.selection)
    }

    /// The command as configured.
    #[must_use]
    pub const fn server_command(&self) -> &ServerCommand {
        &self.command
    }

    /// Whether mcpls may replace the command.
    #[must_use]
    pub const fn selection(&self) -> ServerSelection {
        self.selection
    }

    /// Whether the command is started exactly as written.
    #[must_use]
    pub const fn is_explicit(&self) -> bool {
        self.selection.is_explicit()
    }

    /// The command text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.command.as_str()
    }

    fn into_parts(self) -> (ServerCommand, ServerSelection) {
        (self.command, self.selection)
    }
}

impl From<ServerCommand> for LaunchCommand {
    fn from(command: ServerCommand) -> Self {
        Self::explicit(command)
    }
}

impl std::fmt::Display for LaunchCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.command, f)
    }
}

impl AsRef<OsStr> for LaunchCommand {
    fn as_ref(&self) -> &OsStr {
        self.command.as_ref()
    }
}

impl AsRef<Path> for LaunchCommand {
    fn as_ref(&self) -> &Path {
        self.command.as_ref()
    }
}

impl PartialEq<str> for LaunchCommand {
    fn eq(&self, other: &str) -> bool {
        self.command == *other
    }
}

impl PartialEq<&str> for LaunchCommand {
    fn eq(&self, other: &&str) -> bool {
        self.command == *other
    }
}

/// Configuration for a single LSP server.
///
/// Serialization goes through a raw mirror of this struct, which reports an
/// invalid `file_patterns` entry with the id of the server that holds it and
/// builds the [`LaunchCommand`] from the `command` and `selection` keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "RawLspServerConfig", into = "RawLspServerConfig")]
pub struct LspServerConfig {
    /// Language identifier (e.g., "rust", "python", "typescript").
    pub language_id: LanguageId,

    /// Command to start the LSP server, with whether mcpls may replace it.
    pub command: LaunchCommand,

    /// Arguments to pass to the LSP server command.
    pub args: Vec<String>,

    /// Environment variables for the LSP server process.
    pub env: ServerEnv,

    /// File patterns this server handles (glob patterns).
    pub file_patterns: Vec<FilePattern>,

    /// LSP initialization options (server-specific).
    ///
    /// For typescript-language-server, `None` lets mcpls pin the tsserver
    /// bundled next to the server (`tsserver.path`) so a workspace-supplied
    /// tsserver is not run. Setting `tsserver.path` here overrides the pin,
    /// for example to opt back in to the workspace's TypeScript. Setting any
    /// other options disables the pin: mcpls does not merge into user options.
    /// TypeScript 7 and later ship no tsserver, so there is nothing to pin: with
    /// only that installed the server fails to initialize, and the error says
    /// so. A native `tsc --lsp --stdio` command gets no pin and no generated
    /// options. See `SECURITY.md` for the trust model.
    pub initialization_options: Option<serde_json::Value>,

    /// Per-server settings pushed after `initialized` via
    /// `workspace/didChangeConfiguration` and served on
    /// `workspace/configuration`. Top-level dotted keys are expanded into
    /// nested objects; keys inside values are left untouched.
    pub settings: Option<LspSettings>,

    /// Handshake timeout in seconds: bounds the `initialize` request during
    /// server startup. Does not affect individual tool-call requests sent
    /// after initialization; see [`Self::request_timeout_seconds`] for that.
    /// The LSP server's `shutdown` request during teardown uses a separate,
    /// fixed 5-second timeout that is not configurable by this field.
    pub timeout_seconds: TimeoutSecs,

    /// Per-request timeout in seconds, applied to each LSP request issued
    /// while translating an MCP tool call (hover, definition, references, etc.).
    ///
    /// This bounds a single request attempt, not a whole tool call: on a
    /// `-32801` (`ContentModified`) or `-32802` (`ServerCancelled`) response,
    /// [`crate::lsp::LspClient::request`] retries up to 4 attempts with
    /// backoff (one shared budget across both codes), so the worst-case
    /// latency for one tool call is `4 * request_timeout_seconds + 3.5`
    /// seconds. Completion requests are further capped at 10 seconds
    /// regardless of this value; see
    /// [`crate::lsp::LspClient::completion_timeout`].
    pub request_timeout_seconds: TimeoutSecs,

    /// Heuristics for determining if this server should be spawned.
    /// If not specified, the server will always attempt to spawn.
    pub heuristics: Option<ServerHeuristics>,

    /// Human-readable server identity used as the routing key.
    ///
    /// Defaults to `language_id` when omitted (see [`Self::id`]). Must be
    /// unique across all applicable servers in a workspace, regardless of
    /// language: this is what lets two servers share one `language_id`
    /// (e.g. pyright and pylsp both for `python`) without one silently
    /// overwriting the other in the maps keyed by [`ServerId`].
    pub name: Option<ServerId>,

    /// Tools this server handles.
    ///
    /// `None` means this server is a catch-all: it serves every tool not
    /// explicitly claimed by another server for the same language.
    /// `Some(list)` restricts the server to exactly those tools.
    pub handles: Option<ToolSet>,

    /// Workspace-indexing readiness policy for this server (P4 escape
    /// hatch). Defaults to [`IndexingPolicy::Auto`]: readiness is tracked
    /// normally from whatever signals the server sends (rust-analyzer's
    /// `experimental/serverStatus`, or a generic `$/progress` sequence).
    /// Set to `"disabled"` for a server whose signal shape doesn't fit this
    /// tracker's assumptions -- see [`IndexingPolicy::Disabled`].
    pub indexing: IndexingPolicy,
}

/// The deserialized form of [`LspServerConfig`], before the entry-level checks.
///
/// Mirrors the public struct field for field; the `TryFrom` impl below
/// destructures and builds both exhaustively, so a field added to one side
/// only fails to compile.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLspServerConfig {
    language_id: LanguageId,
    command: ServerCommand,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<EnvKey, String>,
    #[serde(default)]
    file_patterns: Vec<String>,
    #[serde(default)]
    initialization_options: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settings: Option<LspSettings>,
    #[serde(default)]
    timeout_seconds: TimeoutSecs,
    #[serde(default)]
    request_timeout_seconds: TimeoutSecs,
    #[serde(default)]
    heuristics: Option<ServerHeuristics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<ServerId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handles: Option<ToolSet>,
    #[serde(default, skip_serializing_if = "IndexingPolicy::is_auto")]
    indexing: IndexingPolicy,
    #[serde(default, skip_serializing_if = "ServerSelection::is_explicit")]
    selection: ServerSelection,
}

impl From<LspServerConfig> for RawLspServerConfig {
    fn from(config: LspServerConfig) -> Self {
        let LspServerConfig {
            language_id,
            command,
            args,
            env,
            file_patterns,
            initialization_options,
            settings,
            timeout_seconds,
            request_timeout_seconds,
            heuristics,
            name,
            handles,
            indexing,
        } = config;
        let (command, selection) = command.into_parts();
        Self {
            language_id,
            command,
            args,
            env: env.into(),
            file_patterns: file_patterns.into_iter().map(String::from).collect(),
            initialization_options,
            settings,
            timeout_seconds,
            request_timeout_seconds,
            heuristics,
            name,
            handles,
            indexing,
            selection,
        }
    }
}

impl TryFrom<RawLspServerConfig> for LspServerConfig {
    type Error = ConfigError;

    fn try_from(raw: RawLspServerConfig) -> Result<Self, Self::Error> {
        let RawLspServerConfig {
            language_id,
            command,
            args,
            env,
            file_patterns,
            initialization_options,
            settings,
            timeout_seconds,
            request_timeout_seconds,
            heuristics,
            name,
            handles,
            indexing,
            selection,
        } = raw;
        let command = LaunchCommand::new(command, selection).map_err(|InvalidAutoSelection| {
            ConfigError::SelectionAutoOnNonTypescript {
                language: language_id.clone(),
            }
        })?;
        let env = ServerEnv::from_entries(env, HostOs::CURRENT).map_err(|error| {
            ConfigError::DuplicateEnvKey {
                server: entry_id(name.as_ref(), &language_id),
                error,
            }
        })?;
        let file_patterns = file_patterns
            .iter()
            .map(|pattern| {
                FilePattern::parse(pattern).map_err(|pattern| ConfigError::UnsupportedFilePattern {
                    server: entry_id(name.as_ref(), &language_id),
                    pattern,
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            language_id,
            command,
            args,
            env,
            file_patterns,
            initialization_options,
            settings,
            timeout_seconds,
            request_timeout_seconds,
            heuristics,
            name,
            handles,
            indexing,
        })
    }
}

/// Language servers mcpls ships a default configuration for.
///
/// Single source of truth for each builtin's executable name and its install
/// hint, so spawn-failure guidance cannot drift from the defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinServer {
    /// rust-analyzer for Rust.
    RustAnalyzer,
    /// pyright for Python.
    Pyright,
    /// typescript-language-server for TypeScript.
    TypescriptLanguageServer,
    /// gopls for Go.
    Gopls,
    /// clangd for C and C++.
    Clangd,
    /// zls for Zig.
    Zls,
}

impl BuiltinServer {
    /// Every builtin server.
    pub const ALL: [Self; 6] = [
        Self::RustAnalyzer,
        Self::Pyright,
        Self::TypescriptLanguageServer,
        Self::Gopls,
        Self::Clangd,
        Self::Zls,
    ];

    /// Executable name looked up on `PATH`.
    #[must_use]
    pub const fn command(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
            Self::Pyright => "pyright-langserver",
            Self::TypescriptLanguageServer => "typescript-language-server",
            Self::Gopls => "gopls",
            Self::Clangd => "clangd",
            Self::Zls => "zls",
        }
    }

    /// The workspace-supplied code this server can run, for the message that
    /// explains why untrusted mode refused it.
    #[must_use]
    pub const fn workspace_code(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "Cargo build scripts and procedural macros",
            Self::Pyright => {
                "the project's Python environment, plugins and configured interpreters"
            }
            Self::TypescriptLanguageServer => {
                "the workspace's tsserver and tsconfig plugins, unless pinned"
            }
            Self::Gopls => "the Go toolchain, including toolchain downloads requested by go.mod",
            Self::Clangd => "commands from compile_commands.json and .clangd configuration",
            Self::Zls => "build.zig through its build runner",
        }
    }

    /// One-line instruction for installing this server.
    #[must_use]
    pub const fn install_hint(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rustup component add rust-analyzer",
            Self::Pyright => "npm install -g pyright",
            Self::TypescriptLanguageServer => {
                "npm install -g typescript-language-server typescript@6"
            }
            Self::Gopls => "go install golang.org/x/tools/gopls@latest",
            Self::Clangd => {
                "install clangd via your package manager (e.g. apt install clangd, brew install llvm) or see https://clangd.llvm.org/installation"
            }
            Self::Zls => {
                "install a zls matching your Zig version: https://zigtools.org/zls/install/"
            }
        }
    }

    /// Likely cause of this server exiting before completing the `initialize`
    /// handshake, if one is known.
    ///
    /// Only rust-analyzer has one: `rustup` installs a `rust-analyzer` proxy
    /// even when the component is not installed, and the proxy exits at once.
    #[must_use]
    pub const fn early_exit_hint(self) -> Option<&'static str> {
        match self {
            Self::RustAnalyzer => Some(
                "the `rust-analyzer` found on PATH may be a rustup proxy without the component \
                 installed -- run `rustup component add rust-analyzer`",
            ),
            Self::Pyright
            | Self::TypescriptLanguageServer
            | Self::Gopls
            | Self::Clangd
            | Self::Zls => None,
        }
    }

    /// Whether this server is distributed through npm, and so is installed
    /// as a `.cmd` shim on Windows.
    #[must_use]
    pub const fn is_npm_package(self) -> bool {
        matches!(self, Self::Pyright | Self::TypescriptLanguageServer)
    }

    /// Whether `command` launches this builtin: its file stem is this
    /// server's executable name, so an absolute path or a Windows `.cmd` shim
    /// matches. Case-insensitive on Windows.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::config::BuiltinServer;
    ///
    /// let tsls = BuiltinServer::TypescriptLanguageServer;
    /// assert!(tsls.matches_command("typescript-language-server"));
    /// assert!(tsls.matches_command("/usr/local/bin/typescript-language-server"));
    /// assert!(tsls.matches_command("typescript-language-server.cmd"));
    /// assert!(!tsls.matches_command("tsc"));
    /// ```
    #[must_use]
    pub fn matches_command(self, command: &str) -> bool {
        CommandStem::of(command).is(self.command())
    }

    /// Look up a builtin by its exact executable name.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::config::BuiltinServer;
    ///
    /// assert_eq!(BuiltinServer::from_command("gopls"), Some(BuiltinServer::Gopls));
    /// assert_eq!(BuiltinServer::from_command("/usr/bin/gopls"), None);
    /// ```
    #[must_use]
    pub fn from_command(command: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.command() == command)
    }
}

impl LspServerConfig {
    /// The routing identity of this server: `name` if set, otherwise `language_id`.
    ///
    /// This is the key used across `Translator`'s client/server maps, so two
    /// servers for the same language must set distinct `name`s or they
    /// collide (see `ToolRouter::from_configs` for the enforcement).
    #[must_use]
    pub fn id(&self) -> ServerId {
        entry_id(self.name.as_ref(), &self.language_id)
    }

    /// Build a built-in server config, filling in every field not passed as
    /// a parameter.
    fn builtin(
        language_id: LanguageId,
        server: BuiltinServer,
        args: &[&str],
        file_patterns: &[&'static str],
        markers: impl IntoIterator<Item = &'static str>,
    ) -> Self {
        Self {
            language_id,
            command: ServerCommand::from_static(server.command()).into(),
            args: args.iter().map(ToString::to_string).collect(),
            env: crate::config::ServerEnv::default(),
            file_patterns: file_patterns
                .iter()
                .map(|pattern| FilePattern::from_static(pattern))
                .collect(),
            initialization_options: None,
            settings: None,
            timeout_seconds: TimeoutSecs::DEFAULT,
            request_timeout_seconds: TimeoutSecs::DEFAULT,
            heuristics: Some(ServerHeuristics::with_markers(
                markers.into_iter().map(ProjectMarker::from_static),
            )),
            name: None,
            handles: None,
            indexing: IndexingPolicy::Auto,
        }
    }

    /// Create a default configuration for rust-analyzer.
    #[must_use]
    pub fn rust_analyzer() -> Self {
        Self::builtin(
            const { LanguageId::from_static("rust") },
            BuiltinServer::RustAnalyzer,
            &[],
            &["**/*.rs"],
            ["Cargo.toml", "rust-toolchain.toml"],
        )
    }

    /// Create a default configuration for pyright.
    #[must_use]
    pub fn pyright() -> Self {
        Self::builtin(
            const { LanguageId::from_static("python") },
            BuiltinServer::Pyright,
            &["--stdio"],
            &["**/*.py"],
            [
                "pyproject.toml",
                "setup.py",
                "requirements.txt",
                "pyrightconfig.json",
            ],
        )
    }

    /// Create a default configuration for TypeScript language server.
    ///
    /// `initialization_options` stays `None` here; at startup mcpls fills in
    /// the tsserver pin when it can resolve the server's bundled tsserver (see
    /// [`Self::initialization_options`]). The entry is [`ServerSelection::Auto`],
    /// so mcpls may start the native TypeScript server instead when TypeScript 7
    /// is installed outside the workspace.
    #[must_use]
    pub fn typescript() -> Self {
        Self {
            command: LaunchCommand::typescript(),
            ..Self::builtin(
                const { LanguageId::from_static("typescript") },
                BuiltinServer::TypescriptLanguageServer,
                &["--stdio"],
                &["**/*.ts", "**/*.tsx"],
                ["package.json", "tsconfig.json", "jsconfig.json"],
            )
        }
    }

    /// Create a default configuration for gopls.
    #[must_use]
    pub fn gopls() -> Self {
        Self::builtin(
            const { LanguageId::from_static("go") },
            BuiltinServer::Gopls,
            &["serve"],
            &["**/*.go"],
            ["go.mod", "go.sum"],
        )
    }

    /// Create a default configuration for clangd.
    #[must_use]
    pub fn clangd() -> Self {
        Self::builtin(
            const { LanguageId::from_static("cpp") },
            BuiltinServer::Clangd,
            &[],
            &["**/*.c", "**/*.cpp", "**/*.h", "**/*.hpp"],
            [
                "CMakeLists.txt",
                "compile_commands.json",
                "Makefile",
                ".clangd",
            ],
        )
    }

    /// Create a default configuration for zls.
    #[must_use]
    pub fn zls() -> Self {
        Self::builtin(
            const { LanguageId::from_static("zig") },
            BuiltinServer::Zls,
            &[],
            &["**/*.zig"],
            ["build.zig", "build.zig.zon"],
        )
    }
}

/// The id a server entry is known by: its `name`, else its language.
fn entry_id(name: Option<&ServerId>, language: &LanguageId) -> ServerId {
    name.cloned()
        .unwrap_or_else(|| ServerId::from(language.clone()))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::config::ToolRouter;

    fn heuristics_with<const N: usize>(markers: [&'static str; N]) -> ServerHeuristics {
        ServerHeuristics::with_markers(markers.map(ProjectMarker::from_static))
    }

    #[test]
    fn test_server_command_rejects_blank_and_round_trips() {
        assert_eq!(ServerCommand::new(""), Err(InvalidServerCommand));
        assert_eq!(ServerCommand::new(" \t"), Err(InvalidServerCommand));
        let command: ServerCommand = serde_json::from_str("\"gopls\"").unwrap();
        assert_eq!(command, "gopls");
        assert_eq!(serde_json::to_string(&command).unwrap(), "\"gopls\"");
        assert!(serde_json::from_str::<ServerCommand>("\"  \"").is_err());
    }

    #[test]
    #[should_panic(expected = "command must be ASCII and not blank")]
    fn test_server_command_from_static_panics_on_blank() {
        let _ = ServerCommand::from_static(" ");
    }

    #[test]
    #[should_panic(expected = "command must be ASCII and not blank")]
    fn test_server_command_from_static_panics_on_non_ascii_blank() {
        let _ = ServerCommand::from_static("\u{a0}");
    }

    #[test]
    fn test_matches_command_accepts_stem_and_rejects_other_servers() {
        let tsls = BuiltinServer::TypescriptLanguageServer;
        for command in [
            "typescript-language-server",
            "/usr/local/bin/typescript-language-server",
            "typescript-language-server.cmd",
        ] {
            assert!(tsls.matches_command(command), "{command}");
        }
        for command in ["tsc", "", "rust-analyzer", "typescript-language-server-x"] {
            assert!(!tsls.matches_command(command), "{command}");
        }
    }

    #[test]
    fn test_selection_serde_and_defaults() {
        assert!(ServerSelection::default().is_explicit());
        assert_eq!(
            serde_json::from_str::<ServerSelection>("\"auto\"").unwrap(),
            ServerSelection::Auto
        );
        assert!(serde_json::from_str::<ServerSelection>("\"native\"").is_err());
        assert_eq!(
            LspServerConfig::typescript().command.selection(),
            ServerSelection::Auto
        );
        assert_eq!(
            LspServerConfig::rust_analyzer().command.selection(),
            ServerSelection::Explicit
        );
    }

    #[test]
    fn test_default_typescript_command_is_a_valid_auto_selection() {
        let command = LspServerConfig::typescript().command;
        assert_eq!(
            LaunchCommand::auto(command.server_command().clone()),
            Ok(command)
        );
    }

    #[test]
    fn test_retarget_keeps_the_selection_and_rechecks_auto() {
        let tsls = |text: &str| ServerCommand::new(text).unwrap();
        let auto = LaunchCommand::auto(tsls("typescript-language-server")).unwrap();

        let moved = auto
            .clone()
            .retarget(tsls("/opt/bin/typescript-language-server"))
            .unwrap();
        assert_eq!(moved.selection(), ServerSelection::Auto);
        assert_eq!(moved, "/opt/bin/typescript-language-server");
        assert_eq!(
            auto.retarget(tsls("rust-analyzer")),
            Err(InvalidAutoSelection)
        );

        let explicit = LaunchCommand::explicit(tsls("rust-analyzer"));
        assert!(explicit.retarget(tsls("anything")).is_ok());
    }

    #[test]
    fn test_selection_key_is_written_only_for_auto() {
        let typescript = toml::to_string(&LspServerConfig::typescript()).unwrap();
        assert!(typescript.contains("selection = \"auto\""), "{typescript}");
        let rust = toml::to_string(&LspServerConfig::rust_analyzer()).unwrap();
        assert!(!rust.contains("selection"), "{rust}");
    }

    #[test]
    fn test_early_exit_hint_only_for_rust_analyzer() {
        for builtin in BuiltinServer::ALL {
            assert_eq!(
                builtin.early_exit_hint().is_some(),
                builtin == BuiltinServer::RustAnalyzer,
                "{builtin:?}"
            );
        }
    }

    #[test]
    fn test_rust_analyzer_defaults() {
        let config = LspServerConfig::rust_analyzer();

        assert_eq!(config.language_id, "rust");
        assert_eq!(config.command, "rust-analyzer");
        assert_eq!(config.args.len(), 0);
        assert!(config.env.is_empty());
        assert_eq!(config.file_patterns, ["**/*.rs"]);
        assert!(config.initialization_options.is_none());
        assert_eq!(config.timeout_seconds, TimeoutSecs::DEFAULT);
    }

    #[test]
    fn test_pyright_defaults() {
        let config = LspServerConfig::pyright();

        assert_eq!(config.language_id, "python");
        assert_eq!(config.command, "pyright-langserver");
        assert_eq!(config.args, vec!["--stdio"]);
        assert!(config.env.is_empty());
        assert_eq!(config.file_patterns, ["**/*.py"]);
        assert!(config.initialization_options.is_none());
        assert_eq!(config.timeout_seconds, TimeoutSecs::DEFAULT);
    }

    #[test]
    fn test_typescript_defaults() {
        let config = LspServerConfig::typescript();

        assert_eq!(config.language_id, "typescript");
        assert_eq!(config.command, "typescript-language-server");
        assert_eq!(config.args, vec!["--stdio"]);
        assert!(config.env.is_empty());
        assert_eq!(config.file_patterns, ["**/*.ts", "**/*.tsx"]);
        assert!(config.initialization_options.is_none());
        assert_eq!(config.timeout_seconds, TimeoutSecs::DEFAULT);
    }

    #[test]
    fn test_custom_config() {
        let mut env = ServerEnv::default();
        env.insert(EnvKey::from_static("RUST_LOG"), "debug".to_string());

        let config = LspServerConfig {
            language_id: LanguageId::from_static("custom"),
            command: ServerCommand::from_static("custom-lsp").into(),
            args: vec!["--flag".to_string()],
            env: env.clone(),
            file_patterns: vec![FilePattern::from_static("**/*.custom")],
            initialization_options: Some(serde_json::json!({"key": "value"})),
            settings: None,
            timeout_seconds: TimeoutSecs::new(60).unwrap(),
            request_timeout_seconds: TimeoutSecs::new(45).unwrap(),
            heuristics: None,
            name: None,
            handles: None,
            indexing: crate::bridge::IndexingPolicy::Auto,
        };

        assert_eq!(config.language_id, "custom");
        assert_eq!(config.command, "custom-lsp");
        assert_eq!(config.args, vec!["--flag"]);
        assert_eq!(config.env.get("RUST_LOG"), Some("debug"));
        assert_eq!(config.file_patterns, ["**/*.custom"]);
        assert!(config.initialization_options.is_some());
        assert_eq!(config.timeout_seconds.get(), 60);
    }

    #[test]
    fn test_serde_roundtrip() {
        let original = LspServerConfig::rust_analyzer();

        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: LspServerConfig = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.language_id, original.language_id);
        assert_eq!(deserialized.command, original.command);
        assert_eq!(deserialized.args, original.args);
        assert_eq!(deserialized.timeout_seconds, original.timeout_seconds);
        assert_eq!(
            deserialized.request_timeout_seconds,
            original.request_timeout_seconds
        );
    }

    #[test]
    fn test_indexing_defaults_to_auto_when_omitted() {
        assert_eq!(
            LspServerConfig::rust_analyzer().indexing,
            crate::bridge::IndexingPolicy::Auto
        );
    }

    /// P4: `[[lsp_servers]] indexing = "disabled"` must round-trip through
    /// TOML, the format `[[lsp_servers]]` entries are actually configured in.
    #[test]
    fn test_indexing_disabled_round_trips_through_toml() {
        let toml_str = r#"
            language_id = "rust"
            command = "rust-analyzer"
            indexing = "disabled"
        "#;
        let config: LspServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.indexing, crate::bridge::IndexingPolicy::Disabled);

        let serialized = toml::to_string(&config).unwrap();
        assert!(serialized.contains(r#"indexing = "disabled""#));
        let round_tripped: LspServerConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(
            round_tripped.indexing,
            crate::bridge::IndexingPolicy::Disabled
        );
    }

    /// A `[[lsp_servers]]` entry that omits `indexing` entirely must default
    /// to `Auto`, not fail `deny_unknown_fields`/require the field.
    #[test]
    fn test_indexing_omitted_from_toml_defaults_to_auto() {
        let toml_str = r#"
            language_id = "rust"
            command = "rust-analyzer"
        "#;
        let config: LspServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.indexing, crate::bridge::IndexingPolicy::Auto);
    }

    /// M4: a config at the default `IndexingPolicy::Auto` must not write
    /// `indexing = "auto"` into the serialized TOML -- every other optional
    /// field on this struct is skipped at its default, and generating
    /// ~30 builtin server entries each carrying a redundant `indexing =
    /// "auto"` line would be a visible regression to `mcpls.toml`'s
    /// generated default.
    #[test]
    fn test_indexing_auto_is_omitted_from_serialized_toml() {
        let config = LspServerConfig::rust_analyzer();
        assert_eq!(config.indexing, crate::bridge::IndexingPolicy::Auto);

        let serialized = toml::to_string(&config).unwrap();
        assert!(
            !serialized.contains("indexing"),
            "the default IndexingPolicy::Auto must be omitted, not serialized as \
             indexing = \"auto\""
        );
    }

    #[test]
    fn test_clone() {
        let config = LspServerConfig::rust_analyzer();
        let cloned = config.clone();

        assert_eq!(cloned.language_id, config.language_id);
        assert_eq!(cloned.command, config.command);
        assert_eq!(cloned.timeout_seconds, config.timeout_seconds);
    }

    #[test]
    fn test_empty_env() {
        let config = LspServerConfig::rust_analyzer();
        assert!(config.env.is_empty());
    }

    #[test]
    fn test_multiple_file_patterns() {
        let config = LspServerConfig::typescript();
        assert_eq!(config.file_patterns.len(), 2);
        assert!(
            config
                .file_patterns
                .contains(&FilePattern::from_static("**/*.ts"))
        );
        assert!(
            config
                .file_patterns
                .contains(&FilePattern::from_static("**/*.tsx"))
        );
    }

    #[test]
    fn test_initialization_options_none_by_default() {
        let configs = vec![
            LspServerConfig::rust_analyzer(),
            LspServerConfig::pyright(),
            LspServerConfig::typescript(),
        ];

        for config in configs {
            assert!(config.initialization_options.is_none());
        }
    }

    // Heuristics tests
    #[test]
    fn test_heuristics_empty_always_applicable() {
        let heuristics = ServerHeuristics::default();
        let tmp = TempDir::new().unwrap();
        assert!(applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_heuristics_marker_present() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();

        let heuristics = heuristics_with(["Cargo.toml"]);
        assert!(applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_heuristics_marker_absent() {
        let tmp = TempDir::new().unwrap();
        let heuristics = heuristics_with(["Cargo.toml"]);
        assert!(!applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_heuristics_any_marker_matches() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("setup.py"), "").unwrap();

        let heuristics = heuristics_with(["pyproject.toml", "setup.py", "requirements.txt"]);
        assert!(applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_should_spawn_without_heuristics() {
        let config = LspServerConfig {
            language_id: LanguageId::from_static("test"),
            command: ServerCommand::from_static("test-lsp").into(),
            args: vec![],
            env: crate::config::ServerEnv::default(),
            file_patterns: vec![],
            initialization_options: None,
            settings: None,
            timeout_seconds: TimeoutSecs::new(30).unwrap(),
            request_timeout_seconds: TimeoutSecs::new(30).unwrap(),
            heuristics: None,
            name: None,
            handles: None,
            indexing: crate::bridge::IndexingPolicy::Auto,
        };

        let tmp = TempDir::new().unwrap();
        assert!(spawns(&config, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_should_spawn_with_heuristics() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();

        let config = LspServerConfig::rust_analyzer();
        assert!(spawns(&config, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_should_not_spawn_without_markers() {
        let tmp = TempDir::new().unwrap();
        let config = LspServerConfig::rust_analyzer();
        assert!(!spawns(&config, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_heuristics_serde_roundtrip() {
        let heuristics = heuristics_with(["Cargo.toml", "rust-toolchain.toml"]);
        let json = serde_json::to_string(&heuristics).unwrap();
        let deserialized: ServerHeuristics = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.project_markers, heuristics.project_markers);
    }

    #[test]
    fn test_default_rust_analyzer_heuristics() {
        let config = LspServerConfig::rust_analyzer();
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&ProjectMarker::from_static("Cargo.toml")));
    }

    #[test]
    fn test_gopls_defaults() {
        let config = LspServerConfig::gopls();

        assert_eq!(config.language_id, "go");
        assert_eq!(config.command, "gopls");
        assert_eq!(config.args, vec!["serve"]);
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&ProjectMarker::from_static("go.mod")));
        assert!(markers.contains(&ProjectMarker::from_static("go.sum")));
    }

    #[test]
    fn test_clangd_defaults() {
        let config = LspServerConfig::clangd();

        assert_eq!(config.language_id, "cpp");
        assert_eq!(config.command, "clangd");
        assert_eq!(config.args.len(), 0);
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&ProjectMarker::from_static("CMakeLists.txt")));
        assert!(markers.contains(&ProjectMarker::from_static("compile_commands.json")));
    }

    #[test]
    fn test_zls_defaults() {
        let config = LspServerConfig::zls();

        assert_eq!(config.language_id, "zig");
        assert_eq!(config.command, "zls");
        assert_eq!(config.args.len(), 0);
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&ProjectMarker::from_static("build.zig")));
        assert!(markers.contains(&ProjectMarker::from_static("build.zig.zon")));
    }

    // Recursive scanning tests
    #[test]
    fn test_recursive_empty_markers_always_applicable() {
        let heuristics = ServerHeuristics::default();
        let tmp = TempDir::new().unwrap();
        assert!(applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_recursive_marker_at_root() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();

        let heuristics = heuristics_with(["Cargo.toml"]);
        assert!(applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_recursive_nested_python_project() {
        let tmp = TempDir::new().unwrap();
        // Create Rust project at root
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        // Create nested Python project
        let python_dir = tmp.path().join("python");
        std::fs::create_dir(&python_dir).unwrap();
        std::fs::write(python_dir.join("pyproject.toml"), "").unwrap();

        let heuristics = heuristics_with(["pyproject.toml", "setup.py"]);
        assert!(applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_recursive_deeply_nested_marker() {
        let tmp = TempDir::new().unwrap();
        // Create a deeply nested structure
        let deep_path = tmp.path().join("level1").join("level2").join("level3");
        std::fs::create_dir_all(&deep_path).unwrap();
        std::fs::write(deep_path.join("go.mod"), "").unwrap();

        let heuristics = heuristics_with(["go.mod"]);
        assert!(applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_recursive_no_marker_found() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src").join("main.rs"), "").unwrap();

        let heuristics = heuristics_with(["Cargo.toml"]);
        assert!(!applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_recursive_max_depth_respected() {
        let tmp = TempDir::new().unwrap();
        // Create marker at depth 5
        let deep_path = tmp.path().join("a").join("b").join("c").join("d").join("e");
        std::fs::create_dir_all(&deep_path).unwrap();
        std::fs::write(deep_path.join("Cargo.toml"), "").unwrap();

        let heuristics = heuristics_with(["Cargo.toml"]);
        // With max_depth=3, should not find marker at depth 5
        assert!(!applies(
            &heuristics,
            tmp.path(),
            SearchDepth::new(3).unwrap()
        ));
        // With max_depth=10 (default), should find it
        assert!(applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_recursive_excludes_node_modules() {
        let tmp = TempDir::new().unwrap();
        // Create package.json inside node_modules (should be ignored)
        let node_modules = tmp.path().join("node_modules").join("some-package");
        std::fs::create_dir_all(&node_modules).unwrap();
        std::fs::write(node_modules.join("package.json"), "").unwrap();

        let heuristics = heuristics_with(["package.json"]);
        assert!(!applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_recursive_excludes_target_directory() {
        let tmp = TempDir::new().unwrap();
        // Create Cargo.toml inside target (should be ignored)
        let target = tmp.path().join("target").join("debug");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("Cargo.toml"), "").unwrap();

        let heuristics = heuristics_with(["Cargo.toml"]);
        assert!(!applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_recursive_excludes_git_directory() {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join(".git").join("hooks");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("Cargo.toml"), "").unwrap();

        let heuristics = heuristics_with(["Cargo.toml"]);
        assert!(!applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_recursive_excludes_pycache() {
        let tmp = TempDir::new().unwrap();
        let pycache = tmp.path().join("__pycache__");
        std::fs::create_dir_all(&pycache).unwrap();
        std::fs::write(pycache.join("pyproject.toml"), "").unwrap();

        let heuristics = heuristics_with(["pyproject.toml"]);
        assert!(!applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_recursive_excludes_venv() {
        let tmp = TempDir::new().unwrap();
        let venv = tmp.path().join(".venv").join("lib");
        std::fs::create_dir_all(&venv).unwrap();
        std::fs::write(venv.join("setup.py"), "").unwrap();

        let heuristics = heuristics_with(["setup.py"]);
        assert!(!applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    /// #476: `standard_filters(false)` is a bulk setter that resets
    /// `.git_ignore` (among others); it must run before the individual
    /// overrides, or it silently negates the `.git_ignore(true)` set
    /// afterward and the walk descends into gitignored directories.
    #[test]
    fn test_recursive_respects_gitignore_in_marker_search() {
        let tmp = TempDir::new().unwrap();
        // A bare `.git` directory is enough for the `ignore` crate to treat
        // this fixture as a git repository and start honoring `.gitignore`.
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "ignored/\n").unwrap();

        let ignored_dir = tmp.path().join("ignored");
        std::fs::create_dir_all(&ignored_dir).unwrap();
        std::fs::write(ignored_dir.join("Cargo.toml"), "").unwrap();

        let heuristics = heuristics_with(["Cargo.toml"]);
        assert!(
            !applies(&heuristics, tmp.path(), SearchDepth::DEFAULT),
            "marker inside a gitignored directory must not make the server applicable"
        );
    }

    #[test]
    fn test_recursive_finds_marker_outside_excluded() {
        let tmp = TempDir::new().unwrap();
        // Create excluded dir with marker
        let node_modules = tmp.path().join("node_modules");
        std::fs::create_dir_all(&node_modules).unwrap();
        std::fs::write(node_modules.join("package.json"), "").unwrap();
        // Create valid marker in src
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("package.json"), "").unwrap();

        let heuristics = heuristics_with(["package.json"]);
        assert!(applies(&heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_recursive_monorepo_structure() {
        let tmp = TempDir::new().unwrap();
        // Create monorepo with multiple language projects
        let rust_pkg = tmp.path().join("packages").join("rust-lib");
        let python_pkg = tmp.path().join("packages").join("python-bindings");
        let ts_pkg = tmp.path().join("packages").join("typescript-client");

        std::fs::create_dir_all(&rust_pkg).unwrap();
        std::fs::create_dir_all(&python_pkg).unwrap();
        std::fs::create_dir_all(&ts_pkg).unwrap();

        std::fs::write(rust_pkg.join("Cargo.toml"), "").unwrap();
        std::fs::write(python_pkg.join("pyproject.toml"), "").unwrap();
        std::fs::write(ts_pkg.join("package.json"), "").unwrap();

        // All should be detected
        let rust_heuristics = heuristics_with(["Cargo.toml"]);
        let python_heuristics = heuristics_with(["pyproject.toml"]);
        let ts_heuristics = heuristics_with(["package.json"]);

        assert!(applies(&rust_heuristics, tmp.path(), SearchDepth::DEFAULT));
        assert!(applies(
            &python_heuristics,
            tmp.path(),
            SearchDepth::DEFAULT
        ));
        assert!(applies(&ts_heuristics, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_should_spawn_recursive() {
        let tmp = TempDir::new().unwrap();
        // Create nested Python project in Rust workspace
        let python_dir = tmp.path().join("bindings").join("python");
        std::fs::create_dir_all(&python_dir).unwrap();
        std::fs::write(python_dir.join("pyproject.toml"), "").unwrap();

        let config = LspServerConfig::pyright();
        assert!(spawns(&config, tmp.path(), SearchDepth::DEFAULT));
    }

    #[test]
    fn test_should_spawn_with_custom_max_depth() {
        let tmp = TempDir::new().unwrap();
        let deep_path = tmp.path().join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&deep_path).unwrap();
        std::fs::write(deep_path.join("Cargo.toml"), "").unwrap();

        let config = LspServerConfig::rust_analyzer();
        // Shallow depth should not find it
        assert!(!spawns(&config, tmp.path(), SearchDepth::new(2).unwrap()));
        // Default depth should find it
        assert!(spawns(&config, tmp.path(), SearchDepth::DEFAULT));
    }

    fn applies(heuristics: &ServerHeuristics, root: &Path, depth: SearchDepth) -> bool {
        let server = LspServerConfig {
            heuristics: Some(heuristics.clone()),
            ..LspServerConfig::rust_analyzer()
        };
        spawns(&server, root, depth)
    }

    fn spawns(server: &LspServerConfig, root: &Path, depth: SearchDepth) -> bool {
        scan(&[root], std::slice::from_ref(server), depth).applies_to(server)
    }

    fn scan(roots: &[&Path], servers: &[LspServerConfig], depth: SearchDepth) -> MarkerScan {
        let roots: Vec<PathBuf> = roots.iter().map(|root| root.to_path_buf()).collect();
        MarkerScan::collect(&roots, servers, depth)
    }

    fn all_builtins() -> Vec<LspServerConfig> {
        vec![
            LspServerConfig::rust_analyzer(),
            LspServerConfig::pyright(),
            LspServerConfig::typescript(),
            LspServerConfig::gopls(),
            LspServerConfig::clangd(),
            LspServerConfig::zls(),
        ]
    }

    #[test]
    fn test_marker_scan_finds_the_markers_of_every_server_in_one_pass() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        let python = tmp.path().join("bindings").join("python");
        std::fs::create_dir_all(&python).unwrap();
        std::fs::write(python.join("pyproject.toml"), "").unwrap();
        let servers = all_builtins();

        let found = scan(&[tmp.path()], &servers, SearchDepth::DEFAULT);

        let applying: Vec<_> = servers
            .iter()
            .filter(|server| found.applies_to(server))
            .map(|server| server.language_id.as_str())
            .collect();
        assert_eq!(applying, ["rust", "python"]);
    }

    #[test]
    fn test_marker_scan_skips_excluded_directories_and_markers_below_the_depth() {
        let tmp = TempDir::new().unwrap();
        for (dir, file) in [
            ("a/b", "go.mod"),
            ("c", "package.json"),
            ("node_modules/x", "Cargo.toml"),
            ("d/e/f/g/h/i/j/k/l/m/n", "build.zig"),
        ] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
            std::fs::write(tmp.path().join(dir).join(file), "").unwrap();
        }
        let servers = all_builtins();
        let found = scan(&[tmp.path()], &servers, SearchDepth::DEFAULT);

        let applying: Vec<_> = servers
            .iter()
            .filter(|server| found.applies_to(server))
            .map(|server| server.language_id.as_str())
            .collect();
        assert_eq!(applying, ["typescript", "go"]);
    }

    #[test]
    fn test_marker_scan_unions_the_roots() {
        let (first, second) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        std::fs::write(first.path().join("Cargo.toml"), "").unwrap();
        let nested = second.path().join("svc");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("go.mod"), "").unwrap();
        let servers = all_builtins();

        let found = scan(
            &[first.path(), second.path()],
            &servers,
            SearchDepth::DEFAULT,
        );

        assert!(found.applies_to(&LspServerConfig::rust_analyzer()));
        assert!(found.applies_to(&LspServerConfig::gopls()));
        assert!(!found.applies_to(&LspServerConfig::zls()));
    }

    #[test]
    fn test_marker_scan_honours_the_depth_limit() {
        let tmp = TempDir::new().unwrap();
        let deep = tmp.path().join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("Cargo.toml"), "").unwrap();
        let servers = [LspServerConfig::rust_analyzer()];

        let shallow = scan(&[tmp.path()], &servers, SearchDepth::new(2).unwrap());
        let default = scan(&[tmp.path()], &servers, SearchDepth::DEFAULT);

        assert!(!shallow.applies_to(&servers[0]));
        assert!(default.applies_to(&servers[0]));
    }

    #[test]
    fn test_marker_scan_applies_a_server_without_markers_everywhere() {
        let tmp = TempDir::new().unwrap();
        let mut no_markers = LspServerConfig::rust_analyzer();
        no_markers.heuristics = None;
        let mut empty_markers = LspServerConfig::rust_analyzer();
        empty_markers.heuristics = Some(ServerHeuristics::default());

        let found = scan(&[tmp.path()], &[], SearchDepth::DEFAULT);

        assert!(found.applies_to(&no_markers));
        assert!(found.applies_to(&empty_markers));
        assert!(!found.applies_to(&LspServerConfig::rust_analyzer()));
    }

    #[test]
    fn test_default_heuristics_max_depth() {
        assert_eq!(DEFAULT_HEURISTICS_MAX_DEPTH, 10);
    }

    #[test]
    fn test_excluded_directories_constant() {
        assert!(EXCLUDED_DIRECTORIES.contains(&"node_modules"));
        assert!(EXCLUDED_DIRECTORIES.contains(&"target"));
        assert!(EXCLUDED_DIRECTORIES.contains(&".git"));
        assert!(EXCLUDED_DIRECTORIES.contains(&"__pycache__"));
        assert!(EXCLUDED_DIRECTORIES.contains(&".venv"));
    }

    const NATIVE_ENTRY: &str = r#"[[lsp_servers]]
language_id = "typescript"
command = "/home/me/ts7/node_modules/.bin/tsc"
args = ["--lsp", "--stdio"]
file_patterns = ["**/*.ts", "**/*.tsx"]"#;

    fn repo_file(relative: &str) -> String {
        let manifest = std::env::var_os("CARGO_MANIFEST_DIR").map_or_else(
            || std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            Into::into,
        );
        std::fs::read_to_string(manifest.join("../..").join(relative))
            .unwrap()
            .replace("\r\n", "\n")
    }

    fn native_entries(extra: &str) -> Vec<LspServerConfig> {
        let text = format!("{NATIVE_ENTRY}\n{extra}");
        toml::from_str::<crate::config::ServerConfig>(&text)
            .unwrap()
            .lsp_servers
    }

    #[test]
    fn test_docs_agree_with_typescript_install_hint() {
        let hint = BuiltinServer::TypescriptLanguageServer.install_hint();
        let pin = hint.split_whitespace().last().unwrap();
        assert!(pin.starts_with("typescript@"), "{hint}");

        let mut checked = 0;
        for file in [
            "README.md",
            "book/src/getting-started/installation.md",
            "book/src/guide/language-servers.md",
            "book/src/advanced/typescript.md",
        ] {
            for line in repo_file(file).lines() {
                if !(line.contains("npm install -g") && line.contains("typescript")) {
                    continue;
                }
                checked += 1;
                assert!(
                    line.split(|c: char| !(c.is_alphanumeric() || matches!(c, '@' | '.' | '-')))
                        .any(|word| word == pin),
                    "{file}: {line}"
                );
                if line.contains("typescript-language-server") {
                    assert!(line.contains(hint), "{file}: {line}");
                }
            }
        }
        assert!(checked >= 4, "only {checked} install lines found");
    }

    #[test]
    fn test_documented_native_entry_is_in_the_book() {
        assert!(repo_file("book/src/advanced/typescript.md").contains(NATIVE_ENTRY));
    }

    #[test]
    fn test_native_entry_replacing_default_typescript_entry_routes() {
        let router = ToolRouter::from_configs(&native_entries("")).unwrap();
        assert!(router.has_language("typescript"));
    }

    #[test]
    fn test_native_entry_appended_to_default_is_rejected() {
        let mut configs = vec![LspServerConfig::typescript()];
        configs.extend(native_entries(""));
        let err = ToolRouter::from_configs(&configs).unwrap_err().to_string();
        assert!(err.contains("duplicate server id"), "{err}");
    }

    #[test]
    fn test_named_native_entry_appended_to_default_is_rejected() {
        let mut named = native_entries("");
        named[0].name = Some(ServerId::from_static("native-ts"));
        let mut configs = vec![LspServerConfig::typescript()];
        configs.extend(named);
        let err = ToolRouter::from_configs(&configs).unwrap_err().to_string();
        assert!(err.contains("two catch-all servers"), "{err}");
    }
}
