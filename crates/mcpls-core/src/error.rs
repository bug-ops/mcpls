//! Error types for mcpls-core.
//!
//! This module defines the canonical error type for the library,
//! following the Microsoft Rust Guidelines for error handling.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::bridge::resources::ResourceUriError;
use crate::bridge::{
    Capability, InvalidClientPath, InvalidHierarchyItem, InvalidPosition, InvalidRange,
    MAX_SYMBOL_NAME_BYTES,
};
use crate::config::{
    BuiltinServer, DuplicateEnvKey, EntrySummary, FileKey, FilePattern, InvalidAutoSelection,
    LanguageId, ServerCommand, ServerId, ToolKind, UnsupportedFilePattern,
};
use crate::lsp::MAX_ERROR_MESSAGE_CALLER_BYTES;
pub use crate::redaction::RedactedText;
use crate::redaction::Redactions;
use crate::util::{SizeExceeded, TRUNCATION_MARKER, escape_control, truncate_str};

/// Explains a `plaintext` routing failure: which extension or file name had no
/// mapping and which `file_patterns` were configured. Empty for any other
/// language.
fn no_server_detail(language: &LanguageId, file: &FileKey, patterns: &[FilePattern]) -> String {
    if *language != LanguageId::PLAINTEXT {
        return String::new();
    }
    let (subject, remedy) = match file {
        FileKey::Extension(ext) => (
            format!("file extension '{ext}' is not mapped to any language"),
            "a `*.EXT` file_patterns entry or workspace.language_extensions".to_owned(),
        ),
        FileKey::Name(name) => (
            format!("file name '{name}' is not mapped to any language"),
            format!("a `**/{name}` file_patterns entry"),
        ),
        FileKey::Unmappable => (
            "the file has no usable extension or name".to_owned(),
            "a `*.EXT` file_patterns entry or workspace.language_extensions".to_owned(),
        ),
    };
    let configured = if patterns.is_empty() {
        "no file_patterns are configured".to_owned()
    } else {
        let list: Vec<&str> = patterns.iter().map(FilePattern::as_str).collect();
        format!("configured file_patterns: {}", list.join(", "))
    };
    format!(" ({subject}; {configured}; map it with {remedy})")
}

/// Host platform, as far as [`NotFoundGuidance`] cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    Windows,
    Other,
}

impl Platform {
    const CURRENT: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Other
    };
}

/// Display suffix explaining how to fix a missing LSP server executable.
struct NotFoundGuidance<'a>(&'a str, Platform);

impl fmt::Display for NotFoundGuidance<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(command, platform) = *self;
        if Path::new(command).parent().is_some_and(|p| !p.is_empty()) {
            return f.write_str("; check that the configured path exists");
        }
        write!(
            f,
            "; '{command}' is not on the PATH mcpls runs with -- if it is installed, add its \
             directory to the MCP client's PATH or set `command` to an absolute path"
        )?;
        let Some(builtin) = BuiltinServer::from_command(command) else {
            return Ok(());
        };
        if platform == Platform::Windows && builtin.is_npm_package() {
            write!(
                f,
                " (npm-installed servers need the `.cmd` name, e.g. `{command}.cmd`)"
            )?;
        }
        write!(f, "; otherwise install it: {}", builtin.install_hint())
    }
}

/// Substring rust-analyzer's raw error text carries when a position-based
/// request's `line`/`character` falls outside the target document. Shared
/// between [`sanitize_lsp_server_message`] (rewrites the message shown to the
/// caller) and [`Error::mcp_error_kind`] (classifies this shape of
/// [`Error::LspServerError`] as caller-fault) so the two stay in sync.
const INVALID_OFFSET_MARKER: &str = "Invalid offset LineCol";

/// Rewrites an LSP server's raw error message for display to the MCP caller,
/// replacing rust-analyzer's "Invalid offset" internal error with a clean,
/// client-appropriate message.
///
/// rust-analyzer returns this `Debug`-formatted internal error (embedding its
/// `LineCol` struct and the line index's byte length, e.g. `"Invalid offset
/// LineCol { line: 2291, col: 0 } (line index length: 100417)"`) when a
/// position-based request's `line` or `character` falls outside the target
/// document. Every other [`Error`] variant produces a clean message; this
/// function keeps [`Error::LspServerError`]'s `Display` impl consistent with
/// that convention instead of forwarding the upstream server's internals
/// verbatim.
///
/// Matches via `contains` rather than `starts_with`: rust-analyzer's error
/// travels through `anyhow`/`lsp_server` before reaching mcpls, so a future
/// upstream `.context(...)` wrapper (or a truncation prefix added on the
/// mcpls side) could prepend text ahead of `"Invalid offset LineCol"` without
/// mcpls's control -- `contains` keeps the guard robust to that at no extra
/// cost. Deliberately not also gated on the JSON-RPC error `code`: this error
/// class has been observed under both `-32603` (internal error) and `-32803`
/// (`RequestFailed`) across rust-analyzer versions, so a code condition would
/// make the guard more fragile, not less.
fn sanitize_lsp_server_message(message: &str) -> String {
    if message.contains(INVALID_OFFSET_MARKER) {
        "position out of range for this document".to_string()
    } else {
        escape_control(message).into_owned()
    }
}

/// The raw LSP error behind a position-out-of-range rewrite, carried as the
/// JSON-RPC `data` of the resulting `-32602` error.
///
/// [`Error::LspServerError`]'s `Display` replaces rust-analyzer's internal
/// "Invalid offset" text with a clean message; this keeps the original
/// reachable for the caller. The fields are private so the message can only
/// be built through [`Self::new`], which bounds it.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::RewrittenServerError;
///
/// let raw = RewrittenServerError::new(-32603, "Invalid offset LineCol { line: 9, col: 0 }");
/// assert_eq!(raw.code(), -32603);
/// assert_eq!(
///     serde_json::to_value(&raw).unwrap(),
///     serde_json::json!({"code": -32603, "raw_message": "Invalid offset LineCol { line: 9, col: 0 }"})
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RewrittenServerError {
    code: i32,
    raw_message: String,
}

impl RewrittenServerError {
    /// Build from the server's JSON-RPC `code` and raw message, truncating
    /// the message to the budget used for text forwarded to MCP callers.
    #[must_use]
    pub fn new(code: i32, raw_message: &str) -> Self {
        Self {
            code,
            raw_message: truncate_str(raw_message, MAX_ERROR_MESSAGE_CALLER_BYTES),
        }
    }

    /// The server's JSON-RPC error code.
    #[must_use]
    pub const fn code(&self) -> i32 {
        self.code
    }

    /// The server's raw message, bounded by [`Self::new`].
    #[must_use]
    pub fn raw_message(&self) -> &str {
        &self.raw_message
    }
}

/// Why a configured LSP server never registered during startup.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::StartupFailure;
///
/// assert!(StartupFailure::InitTaskPanicked.to_string().contains("panicked"));
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum StartupFailure {
    /// Spawning or initializing the server failed.
    Spawn(Arc<Error>),
    /// Starting this server, or the background initialization task as a whole,
    /// panicked before the server registered.
    InitTaskPanicked,
    /// The workspace is untrusted and this server was not started.
    RefusedUntrustedWorkspace(UntrustedRefusal),
}

/// An environment variable that names the user's home directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeVariable {
    /// `HOME`.
    Home,
    /// `USERPROFILE`.
    UserProfile,
}

impl HomeVariable {
    /// Every home variable untrusted mode manages.
    pub const ALL: [Self; 2] = [Self::Home, Self::UserProfile];

    /// The variable's name in the environment.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Home => "HOME",
            Self::UserProfile => "USERPROFILE",
        }
    }
}

impl fmt::Display for HomeVariable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What untrusted-workspace mode resolved to a path that is not valid UTF-8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedItem {
    /// The server's executable.
    Executable,
    /// The `PATH` handed to the server.
    SearchPath,
}

impl fmt::Display for ResolvedItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Executable => "executable",
            Self::SearchPath => "PATH",
        })
    }
}

/// A piece of configured launch text that may be shown to callers: an option
/// name or a program name, never a value, an inline program or an
/// assignment's value.
///
/// The text is cut at the first `=` or whitespace, escaped and bounded to
/// [`MAX_SYMBOL_NAME_BYTES`] (the escaped text is what is bounded), because
/// configured arguments can hold secrets and reach MCP clients unredacted.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::EchoedArgument;
///
/// assert_eq!(EchoedArgument::name("--token=abc").as_str(), "--token");
/// assert_eq!(EchoedArgument::name("API_KEY=abc").as_str(), "API_KEY");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoedArgument(String);

impl EchoedArgument {
    /// The option or program name `arg` spells: the text before the first `=`
    /// or whitespace, so an assignment's value or an argument glued to the
    /// name is not echoed. A short cluster is named by its offending letter
    /// (`-x`), built by the caller.
    #[must_use]
    pub fn name(arg: &str) -> Self {
        let name = arg
            .split_once(|c: char| c == '=' || c.is_whitespace())
            .map_or(arg, |(name, _)| name);
        Self(bounded_escaped(name, MAX_SYMBOL_NAME_BYTES))
    }

    /// The echoed text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `text` with control and deceptive characters escaped, then bounded to
/// `max_bytes` of escaped text. The cut falls between escaped characters, so
/// an escape sequence is never split.
fn bounded_escaped(text: &str, max_bytes: usize) -> String {
    let mut bounded = String::with_capacity(text.len().min(max_bytes));
    let mut buffer = [0; 4];
    for c in text.chars() {
        let escaped = escape_control(c.encode_utf8(&mut buffer));
        if bounded.len().saturating_add(escaped.len()) > max_bytes {
            bounded.push_str(TRUNCATION_MARKER);
            break;
        }
        bounded.push_str(&escaped);
    }
    bounded
}

/// Most bytes of a workspace-controlled path shown in a refusal.
const MAX_ECHOED_PATH_BYTES: usize = 1024;

/// A path the workspace names, shown whole, escaped and bounded.
///
/// The workspace names a program a launcher starts or a symlink target's final
/// component. Unlike [`EchoedArgument`], the text is never cut at a space or an
/// `=`, because a path is not an option with a value. It is bounded to
/// 1024 bytes of escaped text.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// use mcpls_core::error::EchoedPath;
///
/// let path = EchoedPath::new(Path::new("/opt/my tools/a=b/srv"));
/// assert_eq!(path.as_str(), "/opt/my tools/a=b/srv");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoedPath(String);

impl EchoedPath {
    /// The whole of `path`, escaped and bounded.
    #[must_use]
    pub fn new(path: &Path) -> Self {
        Self(bounded_escaped(
            &path.to_string_lossy(),
            MAX_ECHOED_PATH_BYTES,
        ))
    }

    /// The program an argument names: its whole path, unless the text reads
    /// as an assignment (an `=` before any path separator), which is cut at
    /// the `=` like [`EchoedArgument`] so a value that is not a program is not
    /// echoed.
    #[must_use]
    pub fn program(arg: &str) -> Self {
        let assignment = arg
            .split_once('=')
            .is_some_and(|(name, _)| !name.contains(std::path::is_separator));
        if assignment {
            Self(EchoedArgument::name(arg).0)
        } else {
            Self::new(Path::new(arg))
        }
    }

    /// The echoed text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EchoedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for EchoedArgument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A subcommand of a runner that starts a program the workspace chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerSubcommand {
    /// `run`.
    Run,
    /// `x`.
    X,
    /// `task`.
    Task,
    /// `eval`.
    Eval,
    /// `repl`.
    Repl,
    /// `tool`.
    Tool,
    /// `exec`.
    Exec,
}

impl RunnerSubcommand {
    /// The subcommand as written on a command line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::X => "x",
            Self::Task => "task",
            Self::Eval => "eval",
            Self::Repl => "repl",
            Self::Tool => "tool",
            Self::Exec => "exec",
        }
    }
}

impl fmt::Display for RunnerSubcommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The spelling of a long flag that gives a shell a command string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongCommandName {
    /// `--command`.
    Command,
    /// `--commands`.
    Commands,
    /// `--init-command`, which fish runs as a command string.
    InitCommand,
    /// `--execute`, which nushell runs as a command string.
    Execute,
}

impl LongCommandName {
    /// The flag as written on a command line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "--command",
            Self::Commands => "--commands",
            Self::InitCommand => "--init-command",
            Self::Execute => "--execute",
        }
    }
}

/// A switch of `cmd` that gives it a command string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdSwitch {
    /// `/c`.
    C,
    /// `/k`.
    K,
    /// `/r`.
    R,
}

impl CmdSwitch {
    /// The switch as written on a command line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::C => "/c",
            Self::K => "/k",
            Self::R => "/r",
        }
    }
}

/// A PowerShell parameter that gives it a command, however abbreviated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerShellParameter {
    /// `-Command`.
    Command,
    /// `-CommandWithArgs`.
    CommandWithArgs,
    /// `-EncodedCommand`.
    EncodedCommand,
}

impl PowerShellParameter {
    /// The full parameter name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "-Command",
            Self::CommandWithArgs => "-CommandWithArgs",
            Self::EncodedCommand => "-EncodedCommand",
        }
    }
}

/// The flag that gives a shell a command string, as the shell's family
/// spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFlag {
    /// `-c`, alone or inside a cluster such as `-lc`.
    DashC,
    /// fish's `-C`, which runs an init command string.
    DashCapitalC,
    /// nushell's `-e`, which executes a command string.
    DashE,
    /// A long flag such as `--command`.
    LongCommand(LongCommandName),
    /// `cmd`'s `/c`, `/k` or `/r`.
    SlashC(CmdSwitch),
    /// A PowerShell parameter or an abbreviation or alias of it.
    PowerShell(PowerShellParameter),
}

impl fmt::Display for ShellFlag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::DashC => "-c",
            Self::DashCapitalC => "-C",
            Self::DashE => "-e",
            Self::LongCommand(name) => name.as_str(),
            Self::SlashC(switch) => switch.as_str(),
            Self::PowerShell(parameter) => parameter.as_str(),
        })
    }
}

/// The flag that gives an interpreter a program to run inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineFlag {
    /// A short flag letter (`-e`, `-c`), alone or inside a cluster.
    Short(char),
    /// `--eval`.
    Eval,
    /// `--print`.
    Print,
    /// `--import`, given a `data:` URL.
    Import,
    /// `--loader`, given a `data:` URL.
    Loader,
    /// `--experimental-loader`, given a `data:` URL.
    ExperimentalLoader,
}

impl InlineFlag {
    /// The long spelling, for the long flags.
    #[must_use]
    pub const fn long_name(self) -> Option<&'static str> {
        match self {
            Self::Short(_) => None,
            Self::Eval => Some("--eval"),
            Self::Print => Some("--print"),
            Self::Import => Some("--import"),
            Self::Loader => Some("--loader"),
            Self::ExperimentalLoader => Some("--experimental-loader"),
        }
    }
}

impl fmt::Display for InlineFlag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Short(letter) => write!(f, "-{letter}"),
            Self::Eval | Self::Print | Self::Import | Self::Loader | Self::ExperimentalLoader => {
                f.write_str(self.long_name().unwrap_or_default())
            }
        }
    }
}

/// A positional operand a wrapper takes before the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandKind {
    /// `timeout`'s `DURATION`.
    Duration,
}

impl fmt::Display for OperandKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Duration => "DURATION",
        })
    }
}

/// What about a launcher lets the workspace choose the program that runs.
///
/// The values are matched from closed tables, never from configured text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LaunchTrigger {
    /// Every use of the program does (a package or task runner, or a wrapper
    /// whose options are not analyzed).
    Always,
    /// This subcommand does.
    Subcommand(RunnerSubcommand),
    /// This flag gives it a command string.
    CommandString(ShellFlag),
    /// This flag gives an interpreter a program to run inline.
    InlineProgram(InlineFlag),
    /// An argument names an npm package to run (`npm:`).
    NpmSpecifier,
}

impl fmt::Display for LaunchTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Always => f.write_str("every use of it does"),
            Self::Subcommand(name) => write!(f, "its `{name}` subcommand does"),
            Self::CommandString(flag) => {
                write!(
                    f,
                    "`{flag}` gives it a command string, which cannot be analyzed"
                )
            }
            Self::InlineProgram(flag) => write!(f, "`{flag}` gives it a program to run"),
            Self::NpmSpecifier => f.write_str("an `npm:` argument names a package to run"),
        }
    }
}

/// Why a launch cannot be analyzed, so untrusted mode cannot tell which
/// program it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnanalyzableLaunch {
    /// The wrapper has an option its option table does not list.
    UnknownOption(EchoedArgument),
    /// A listed option that takes a value is the last argument.
    MissingValue(EchoedArgument),
    /// No command follows the wrapper's options.
    MissingCommand,
    /// A positional operand before the command is not of the expected shape.
    MalformedOperand(OperandKind),
    /// `PATH` is assigned, which bypasses the sanitized search path.
    PathAssignment,
    /// A string is split into arguments (`env -S`).
    SplitString,
    /// A relative program follows a directory change.
    RelativeProgramAfterChdir,
    /// The program name is empty.
    BlankProgram,
}

impl fmt::Display for UnanalyzableLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(option) => write!(f, "unknown option '{option}'"),
            Self::MissingValue(option) => write!(f, "option '{option}' has no value"),
            Self::MissingCommand => f.write_str("no command follows its options"),
            Self::MalformedOperand(name) => write!(f, "its {name} operand is malformed"),
            Self::PathAssignment => f.write_str("it assigns PATH"),
            Self::SplitString => f.write_str("it splits a string into arguments"),
            Self::RelativeProgramAfterChdir => {
                f.write_str("it starts a relative program after changing directory")
            }
            Self::BlankProgram => f.write_str("the program name is empty"),
        }
    }
}

/// Why a launcher lets the workspace choose the program that runs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LauncherRefusal {
    /// A launcher selects workspace code.
    SelectsWorkspaceCode {
        /// The launcher's name.
        program: EchoedArgument,
        /// What about it does.
        trigger: LaunchTrigger,
    },
    /// The launch cannot be analyzed, which untrusted mode treats as unsafe.
    Unanalyzable {
        /// The launcher's name.
        program: EchoedArgument,
        /// Why it cannot be analyzed.
        reason: UnanalyzableLaunch,
    },
    /// Wrappers are nested deeper than the analysis follows.
    TooDeep,
}

impl fmt::Display for LauncherRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SelectsWorkspaceCode { program, trigger } => write!(
                f,
                "its launcher '{program}' chooses the server from files in the workspace \
                 ({trigger}), which untrusted mode never runs; install the server globally \
                 and give its absolute path as `command`"
            ),
            Self::Unanalyzable { program, reason } => write!(
                f,
                "its launcher '{program}' cannot be analyzed ({reason}), so untrusted mode \
                 cannot tell which program it starts; give the absolute path of the server \
                 as `command`"
            ),
            Self::TooDeep => f.write_str(
                "its launchers are nested too deeply to analyze, so untrusted mode cannot \
                 tell which program they start; give the absolute path of the server as \
                 `command`",
            ),
        }
    }
}

/// Why untrusted-workspace mode refused to start a server.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::BuiltinServer;
/// use mcpls_core::error::UntrustedRefusal;
///
/// let refusal = UntrustedRefusal::NotAllowed {
///     builtin: Some(BuiltinServer::RustAnalyzer),
/// };
/// assert!(refusal.to_string().contains("Cargo build scripts"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UntrustedRefusal {
    /// The server was not named with `--allow-server`.
    NotAllowed {
        /// The built-in server this is, whose workspace-code class is named
        /// in the message; `None` for a custom server.
        builtin: Option<BuiltinServer>,
    },
    /// The server's executable lies inside the workspace. Naming the server
    /// does not consent to running a binary the workspace supplies.
    WorkspaceExecutable {
        /// The canonical path of the executable.
        executable: PathBuf,
    },
    /// The executable was not found on a search path outside the workspace,
    /// so untrusted mode cannot tell what would run.
    UnresolvedExecutable {
        /// The configured `command`.
        command: ServerCommand,
    },
    /// The auto-selected TypeScript command resolved to an executable that is
    /// not typescript-language-server (a `PATH` entry symlinked to another
    /// program), so it cannot be hardened without dropping the tsserver pin.
    AutoSelectionTarget {
        /// The resolved executable the command would have been replaced with.
        executable: ServerCommand,
        /// Why the selection cannot follow the resolved executable.
        cause: InvalidAutoSelection,
    },
    /// The login home directory is unknown and the inherited `HOME` lies
    /// inside the workspace, where rustup, cargo and npm would read their
    /// configuration from.
    WorkspaceHome {
        /// The variable that names the home directory.
        variable: HomeVariable,
        /// The path the server would inherit, inside the workspace.
        home: PathBuf,
    },
    /// The login home directory is unknown and a home variable is empty, so
    /// tools resolve it against the working directory, which is the checkout.
    EmptyHome {
        /// The variable that is empty.
        variable: HomeVariable,
    },
    /// The login home directory is unknown and `HOME` is not set, so tools
    /// would resolve `~` against the working directory, which is the
    /// checkout.
    UnknownHome,
    /// The tsserver the TypeScript server would use lies inside the
    /// workspace.
    WorkspaceTsserver {
        /// The canonical path of the tsserver.
        tsserver: PathBuf,
    },
    /// The configured command is a launcher that chooses the server from
    /// files in the workspace (a package runner, task runner or toolchain
    /// wrapper), so what would run is under the workspace's control.
    ProjectLauncher {
        /// The configured `command`.
        command: ServerCommand,
        /// What about the launch the analysis refused.
        cause: LauncherRefusal,
    },
    /// An argument of the configured command names an executable file inside
    /// the workspace, which whatever launcher precedes it would start.
    WorkspaceExecutableArgument {
        /// The position of the argument in the configured `args`.
        index: usize,
        /// The canonical path of the executable.
        executable: PathBuf,
    },
    /// An argument of the configured command lies inside the workspace and
    /// cannot be read, so untrusted mode cannot tell whether it is an
    /// executable.
    UnreadableWorkspaceArgument {
        /// The position of the argument in the configured `args`.
        index: usize,
        /// The path the argument names, lexically normalized.
        path: PathBuf,
    },
    /// A program an exec wrapper or `env` starts was not found on a search
    /// path outside the workspace, so untrusted mode cannot tell what would
    /// run.
    UnresolvedWrappedProgram {
        /// The wrapped program, as configured.
        program: EchoedPath,
    },
    /// The vetted path of a program an exec wrapper starts contains `=`, which
    /// `env` would read as an assignment, so it cannot be passed on unchanged.
    WrappedProgramPathContainsEquals {
        /// The wrapped program, as configured.
        program: EchoedPath,
    },
    /// The configured command starts the TypeScript server through a launcher
    /// that untrusted mode cannot pin to a binary outside the workspace.
    UnpinnedTypescriptLauncher {
        /// The configured `command`.
        command: ServerCommand,
    },
    /// A resolved path is not valid UTF-8, so untrusted mode cannot pass it
    /// on unchanged.
    NonUtf8Path {
        /// What the path belongs to.
        what: ResolvedItem,
        /// The path, as resolved.
        path: PathBuf,
    },
    /// No directory outside the workspace is available for the server to
    /// start in.
    NoSafeWorkingDirectory,
}

impl fmt::Display for UntrustedRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAllowed {
                builtin: Some(builtin),
            } => write!(
                f,
                "the workspace is untrusted and it may run workspace code ({})",
                builtin.workspace_code()
            ),
            Self::NotAllowed { builtin: None } => f.write_str(
                "the workspace is untrusted and it is not a built-in server, so the workspace \
                 code it may run is unknown",
            ),
            Self::WorkspaceExecutable { executable } => write!(
                f,
                "its executable {} lies inside the workspace, which untrusted mode never runs",
                EchoedPath::new(executable)
            ),
            Self::WorkspaceExecutableArgument { index, executable } => write!(
                f,
                "its argument at index {index}, {}, is an executable inside the workspace, \
                 which untrusted mode never runs",
                EchoedPath::new(executable)
            ),
            Self::UnreadableWorkspaceArgument { index, path } => write!(
                f,
                "its argument at index {index}, {}, lies inside the workspace and cannot be \
                 read, so untrusted mode cannot tell whether it is an executable",
                EchoedPath::new(path)
            ),
            Self::UnresolvedExecutable { command } => write!(
                f,
                "its executable '{command}' was not found on a PATH outside the workspace, \
                 which untrusted mode requires"
            ),
            Self::AutoSelectionTarget { executable, cause } => write!(
                f,
                "its executable resolved to '{executable}' ({cause}), so untrusted mode \
                 cannot pin it; give the absolute path of the server as `command`"
            ),
            Self::UnknownHome => f.write_str(
                "HOME is not set and the login home directory is unknown, so untrusted mode \
                 cannot give it a safe one",
            ),
            Self::EmptyHome { variable } => write!(
                f,
                "its {variable} is empty, which resolves inside the workspace, and the login \
                 home directory is unknown, so untrusted mode cannot give it a safe one"
            ),
            Self::WorkspaceHome { variable, home } => write!(
                f,
                "its {variable}, {}, lies inside the workspace and the login home directory is \
                 unknown, so untrusted mode cannot give it a safe one",
                EchoedPath::new(home)
            ),
            Self::WorkspaceTsserver { tsserver } => write!(
                f,
                "the tsserver it would use, {}, lies inside the workspace, which untrusted \
                 mode never runs",
                EchoedPath::new(tsserver)
            ),
            Self::ProjectLauncher { cause, .. } => cause.fmt(f),
            Self::UnresolvedWrappedProgram { program } => write!(
                f,
                "the program '{program}' its launcher starts was not found where the child \
                 would look it up, which untrusted mode requires"
            ),
            Self::WrappedProgramPathContainsEquals { program } => write!(
                f,
                "the program '{program}' its launcher starts resolves to a path containing \
                 '=', which env reads as an assignment, so untrusted mode cannot pin it; give \
                 a program whose path has no '='"
            ),
            Self::UnpinnedTypescriptLauncher { command } => write!(
                f,
                "its launcher '{command}' starts the TypeScript server in a way untrusted \
                 mode cannot pin to a binary outside the workspace; install the server \
                 globally and give its absolute path as `command`"
            ),
            Self::NonUtf8Path { what, path } => write!(
                f,
                "its {what}, {}, is not valid UTF-8, so untrusted mode cannot pass it on",
                EchoedPath::new(path)
            ),
            Self::NoSafeWorkingDirectory => f.write_str(
                "no directory outside the workspace is available to start it in, which \
                 untrusted mode requires",
            ),
        }
    }
}

impl fmt::Display for StartupFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => error.fmt(f),
            Self::InitTaskPanicked => {
                f.write_str("the initialization task panicked (see the mcpls log)")
            }
            Self::RefusedUntrustedWorkspace(refusal) => refusal.fmt(f),
        }
    }
}

/// Details of a single server spawn failure.
#[derive(Debug, Clone)]
pub struct ServerSpawnFailure {
    /// Routing identity of the failed server.
    pub server_id: ServerId,
    /// Language ID of the failed server.
    pub language_id: LanguageId,
    /// Command that was attempted.
    pub command: ServerCommand,
    /// Why the server never registered.
    pub reason: StartupFailure,
}

impl fmt::Display for ServerSpawnFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} [{}] ({}): {}",
            self.server_id, self.language_id, self.command, self.reason
        )
    }
}

impl ServerSpawnFailure {
    /// The caller-facing text of [`Error::ServerFailedToStart`]. A refusal
    /// was a decision, not a failure, so it names the remedy instead of the
    /// startup-failure tail.
    pub(crate) const fn failed_to_start(&self) -> FailedToStart<'_> {
        FailedToStart(self)
    }
}

/// `Display` adapter for [`ServerSpawnFailure::failed_to_start`].
pub(crate) struct FailedToStart<'a>(&'a ServerSpawnFailure);

impl fmt::Display for FailedToStart<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let failure = self.0;
        let (id, language) = (&failure.server_id, &failure.language_id);
        match &failure.reason {
            StartupFailure::RefusedUntrustedWorkspace(refusal) => {
                write!(
                    f,
                    "LSP server '{id}' for language '{language}' was not started: {refusal}"
                )?;
                match refusal {
                    UntrustedRefusal::NotAllowed { .. } => {
                        write!(f, "; restart mcpls with `--allow-server {id}` to start it")
                    }
                    UntrustedRefusal::WorkspaceExecutable { .. }
                    | UntrustedRefusal::WorkspaceExecutableArgument { .. }
                    | UntrustedRefusal::UnreadableWorkspaceArgument { .. }
                    | UntrustedRefusal::UnresolvedExecutable { .. }
                    | UntrustedRefusal::AutoSelectionTarget { .. }
                    | UntrustedRefusal::WorkspaceHome { .. }
                    | UntrustedRefusal::EmptyHome { .. }
                    | UntrustedRefusal::UnknownHome
                    | UntrustedRefusal::WorkspaceTsserver { .. }
                    | UntrustedRefusal::ProjectLauncher { .. }
                    | UntrustedRefusal::UnresolvedWrappedProgram { .. }
                    | UntrustedRefusal::WrappedProgramPathContainsEquals { .. }
                    | UntrustedRefusal::UnpinnedTypescriptLauncher { .. }
                    | UntrustedRefusal::NonUtf8Path { .. }
                    | UntrustedRefusal::NoSafeWorkingDirectory => Ok(()),
                }
            }
            reason @ (StartupFailure::Spawn(_) | StartupFailure::InitTaskPanicked) => write!(
                f,
                "LSP server '{id}' for language '{language}' failed to start: {reason}; restart \
                 mcpls after fixing it (startup failures are not retried)"
            ),
        }
    }
}

/// `Display` adapter listing every failure, separated by `; `.
struct FailureList<'a>(&'a [ServerSpawnFailure]);

impl fmt::Display for FailureList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, failure) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            failure.fmt(f)?;
        }
        Ok(())
    }
}

/// Maximum candidates named in the `Display` text of an ambiguity error;
/// the structured payload carries all of them.
const MAX_DISPLAYED_CANDIDATES: usize = 10;

/// One symbol a name resolved to, with enough context to tell it apart from
/// its namesakes and to address it by position instead.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::SymbolCandidate;
///
/// let candidate = SymbolCandidate {
///     name: "new".to_string(),
///     kind: 6,
///     kind_name: "Method".to_string(),
///     container: Some("Config".to_string()),
///     line: 12,
///     character: 8,
/// };
/// assert_eq!(candidate.to_string(), "Method `new` in `Config` at 12:8");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolCandidate {
    /// The symbol's name as the server reports it.
    pub name: String,
    /// Numeric LSP `SymbolKind`.
    pub kind: u32,
    /// Readable name of [`Self::kind`].
    pub kind_name: String,
    /// Enclosing symbol, when the server reports one.
    pub container: Option<String>,
    /// 1-based line of the identifier.
    pub line: u32,
    /// 1-based character of the identifier.
    pub character: u32,
}

impl fmt::Display for SymbolCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} `{}`", self.kind_name, self.name)?;
        if let Some(container) = &self.container {
            write!(f, " in `{container}`")?;
        }
        write!(f, " at {}:{}", self.line, self.character)
    }
}

/// Why a symbol name could not be resolved to exactly one position.
///
/// Carried as the structured `data` of the `INVALID_PARAMS` error, tagged by
/// `resolution`.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::SymbolResolutionData;
///
/// let missing = SymbolResolutionData::NotDefinedInFile { name: "Config".to_string() };
/// assert_eq!(
///     serde_json::to_value(&missing).unwrap(),
///     serde_json::json!({"resolution": "not_defined_in_file", "name": "Config"})
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "resolution", rename_all = "snake_case")]
pub enum SymbolResolutionData {
    /// The name matches several symbols; nothing was picked.
    Ambiguous {
        /// The requested name.
        name: String,
        /// The matching symbols, capped.
        candidates: Vec<SymbolCandidate>,
        /// More symbols match than `candidates` lists.
        truncated: bool,
    },
    /// No symbol of that name (and kind/container) is defined in the file.
    NotFound {
        /// The requested name.
        name: String,
        /// Symbols of that name excluded by the kind or container filter.
        excluded_by_filters: usize,
    },
    /// The name occurs in the file but no symbol defines it there (an import
    /// or a plain reference).
    NotDefinedInFile {
        /// The requested name.
        name: String,
    },
    /// A symbol matched but its identifier position could not be verified
    /// against the document text, so no query was made.
    PositionUnverified {
        /// The requested name.
        name: String,
        /// The symbol that matched.
        candidate: SymbolCandidate,
    },
}

impl fmt::Display for SymbolResolutionData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ambiguous {
                name,
                candidates,
                truncated,
            } => {
                write!(
                    f,
                    "symbol `{name}` is ambiguous ({} candidates{}): ",
                    candidates.len(),
                    if *truncated { ", more not listed" } else { "" }
                )?;
                for (index, candidate) in
                    candidates.iter().take(MAX_DISPLAYED_CANDIDATES).enumerate()
                {
                    if index > 0 {
                        f.write_str("; ")?;
                    }
                    candidate.fmt(f)?;
                }
                if candidates.len() > MAX_DISPLAYED_CANDIDATES {
                    f.write_str("; ...")?;
                }
                f.write_str(
                    ". Narrow it with `symbol_kind` or `container`, or address it by line and character",
                )
            }
            Self::NotFound {
                name,
                excluded_by_filters,
            } => {
                write!(f, "no symbol named `{name}` is defined in this file")?;
                if *excluded_by_filters > 0 {
                    write!(
                        f,
                        " ({excluded_by_filters} with that name excluded by `symbol_kind`/`container`)"
                    )?;
                }
                f.write_str("; try `workspace_symbol_search` or address it by line and character")
            }
            Self::NotDefinedInFile { name } => write!(
                f,
                "`{name}` is not defined in this file (it only occurs as a reference or import); \
                 find its definition with `workspace_symbol_search` or address it by line and character"
            ),
            Self::PositionUnverified { name, candidate } => write!(
                f,
                "could not verify where the identifier `{name}` is ({candidate}), so nothing was \
                 queried; address it by line and character"
            ),
        }
    }
}

/// `Display` adapter listing server ids, separated by `, `.
struct IdList<'a>(&'a [ServerId]);

impl fmt::Display for IdList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, id) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "'{id}'")?;
        }
        Ok(())
    }
}

/// What a failed server startup wrote to stderr, bounded and sanitized.
///
/// Holds the first and last bytes of the output (or all of it when short), so
/// both the opening banner and the final error survive.
///
/// # Sanitizing
///
/// Control characters other than newline and tab are dropped, as are
/// characters that forge or reorder text in logs and terminals: the Unicode
/// line and paragraph separators, bidirectional controls and zero-width
/// characters.
///
/// # Redaction
///
/// Values that look secret are replaced with `[redacted:NAME]` before the text
/// leaves mcpls. A value counts as secret (and must be at least 8 bytes) when
/// it is:
/// - an environment variable value, from the server's configured `env` or
///   mcpls's own environment, whose name contains `TOKEN`, `KEY`, `SECRET`,
///   `PASSW`, `CRED` or `AUTH` (case-insensitive);
/// - the value of a `--flag=value` or `--flag value` argument whose flag name
///   matches the same patterns;
/// - a string under a matching key of `initialization_options`.
///
/// Other values, such as `RUSTUP_TOOLCHAIN`, stay visible so the cause of a
/// failure can still be read. Matching is by exact value: encoded forms (URL,
/// base64, JSON-escaped) are not found. A secret cut by the head/tail elision
/// is hidden only for the fragment of at least 4 bytes on either side of the
/// cut. Put secrets in a name-matching `env` entry rather than in other
/// places.
///
/// `Display` renders one line: trimmed non-empty lines joined with ` | `,
/// with `...` between head and tail when elided. The raw multi-line text is
/// available through [`Self::head`] and [`Self::tail`].
///
/// # Examples
///
/// ```
/// use mcpls_core::Error;
/// use mcpls_core::error::InitPhase;
///
/// fn server_output(err: &Error) -> Option<String> {
///     match err {
///         Error::LspInitFailed { stderr: Some(stderr), .. }
///         | Error::ServerExitedDuringInit { stderr: Some(stderr), .. } => {
///             Some(stderr.to_string())
///         }
///         _ => None,
///     }
/// }
///
/// let err = Error::LspInitFailed {
///     phase: InitPhase::Initialize,
///     cause: Box::new(Error::ServerTerminated),
///     hint: None,
///     stderr: None,
/// };
/// assert_eq!(server_output(&err), None);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StderrExcerpt {
    body: ExcerptBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExcerptBody {
    Complete(Box<str>),
    Elided { head: Box<str>, tail: Box<str> },
}

impl StderrExcerpt {
    /// Excerpt of output that fit entirely in the capture buffer; `None` when
    /// it holds no visible text.
    pub(crate) fn complete(bytes: &[u8], redactions: &Redactions) -> Option<Self> {
        let text = clean_stderr(&String::from_utf8_lossy(bytes), redactions);
        (!text.is_empty()).then(|| Self {
            body: ExcerptBody::Complete(text.into()),
        })
    }

    /// Excerpt of output whose middle was dropped. The cut can split a UTF-8
    /// sequence at the end of `head` and at the start of `tail`; both partial
    /// sequences are discarded rather than shown as replacement characters.
    /// A secret split by the cut is masked as far as `redactions` allows.
    pub(crate) fn elided(head: &[u8], tail: &[u8], redactions: &Redactions) -> Option<Self> {
        let head = clean_stderr(
            &redactions.mask_cut_head(&decode_cut_head(head)),
            redactions,
        );
        let tail_start = tail
            .iter()
            .position(|byte| byte & 0xC0 != 0x80)
            .unwrap_or(tail.len());
        let tail = clean_stderr(
            &redactions.mask_cut_tail(&String::from_utf8_lossy(
                tail.get(tail_start..).unwrap_or_default(),
            )),
            redactions,
        );
        match (head.is_empty(), tail.is_empty()) {
            (true, true) => None,
            (false, true) => Some(Self {
                body: ExcerptBody::Complete(head.into()),
            }),
            (true, false) => Some(Self {
                body: ExcerptBody::Complete(tail.into()),
            }),
            (false, false) => Some(Self {
                body: ExcerptBody::Elided {
                    head: head.into(),
                    tail: tail.into(),
                },
            }),
        }
    }

    /// The start of the output, or all of it when nothing was dropped.
    #[must_use]
    pub fn head(&self) -> &str {
        match &self.body {
            ExcerptBody::Complete(text) => text,
            ExcerptBody::Elided { head, .. } => head,
        }
    }

    /// The end of the output, present only when the middle was dropped.
    #[must_use]
    pub fn tail(&self) -> Option<&str> {
        match &self.body {
            ExcerptBody::Complete(_) => None,
            ExcerptBody::Elided { tail, .. } => Some(tail),
        }
    }

    /// Whether the middle of the output was dropped.
    #[must_use]
    pub const fn is_elided(&self) -> bool {
        matches!(self.body, ExcerptBody::Elided { .. })
    }
}

impl fmt::Display for StderrExcerpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn visible_lines(text: &str) -> impl Iterator<Item = &str> {
            text.lines().map(str::trim).filter(|line| !line.is_empty())
        }

        let mut parts: Vec<&str> = visible_lines(self.head()).collect();
        if let Some(tail) = self.tail() {
            parts.push("...");
            parts.extend(visible_lines(tail));
        }
        f.write_str(&parts.join(" | "))
    }
}

/// Decodes `head`, dropping a UTF-8 sequence cut off at its end.
fn decode_cut_head(head: &[u8]) -> String {
    match std::str::from_utf8(head) {
        Ok(text) => text.to_owned(),
        Err(e) if e.error_len().is_none() => {
            String::from_utf8_lossy(head.get(..e.valid_up_to()).unwrap_or_default()).into_owned()
        }
        Err(_) => String::from_utf8_lossy(head).into_owned(),
    }
}

/// Redacts secrets, drops control and deceptive format characters (other than
/// `\n`/`\t`: they could forge log lines or drive a terminal) and trims.
/// Redaction runs before and after the filter so a dropped character cannot
/// split a secret into a form that escapes it.
fn clean_stderr(text: &str, redactions: &Redactions) -> String {
    let filtered: String = redactions
        .apply(text)
        .chars()
        .filter(|c| {
            (!c.is_control() || matches!(c, '\n' | '\t'))
                && !crate::util::is_deceptive_format_char(*c)
        })
        .collect();
    redactions.apply(&filtered).trim().to_owned()
}

/// `Display` suffix appending a server's stderr excerpt to a startup error.
struct StderrSuffix<'a>(&'a Option<StderrExcerpt>);

impl fmt::Display for StderrSuffix<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .as_ref()
            .map_or(Ok(()), |excerpt| write!(f, "; stderr: {excerpt}"))
    }
}

/// Guidance attached to an `initialize` failure whose likely cause is known.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::InitFailureHint;
///
/// let text = InitFailureHint::NativeTypescriptOnly.to_string();
/// assert!(text.contains("TypeScript 7"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InitFailureHint {
    /// typescript-language-server found only TypeScript 7 or later, which
    /// ships no `tsserver` for it to start.
    NativeTypescriptOnly,
}

impl fmt::Display for InitFailureHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NativeTypescriptOnly => write!(
                f,
                "typescript-language-server found only TypeScript 7 or later, which ships no \
                 tsserver; install the TypeScript version the server supports next to it (for a \
                 global install: `{}`), or change the `command` of your existing `typescript` \
                 server entry to the absolute path of the `tsc` of a TypeScript 7 install \
                 outside the workspace, with `args = [\"--lsp\", \"--stdio\"]` (a `tsc` from the \
                 workspace, or a bare `tsc` that PATH may resolve into the workspace, is \
                 workspace-supplied code)",
                BuiltinServer::TypescriptLanguageServer.install_hint()
            ),
        }
    }
}

/// `Display` suffix appending an [`InitFailureHint`] to a startup error.
struct HintSuffix<'a>(&'a Option<InitFailureHint>);

impl fmt::Display for HintSuffix<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.map_or(Ok(()), |hint| write!(f, "; {hint}"))
    }
}

/// `Display` suffix for [`Error::ServerExitedDuringInit`] carrying the exit
/// status and, for builtin servers with a known early-exit cause, a hint.
struct EarlyExitDetail<'a>(&'a str, Option<i32>);

impl fmt::Display for EarlyExitDetail<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(command, exit_code) = *self;
        match exit_code {
            Some(code) => write!(f, " with exit code {code}")?,
            None => f.write_str(" (terminated by a signal)")?,
        }
        BuiltinServer::from_command(command)
            .and_then(BuiltinServer::early_exit_hint)
            .map_or(Ok(()), |hint| write!(f, "; {hint}"))
    }
}

/// A background task whose failure surfaces as [`Error::TaskFailed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackgroundTask {
    /// The MCP service loop that serves one client connection.
    McpService,
    /// The task that reads an LSP server's messages.
    LspReceiver,
    /// The blocking resolution of `subscriptions/listen` URIs.
    ListenResolution,
    /// The blocking canonicalization of a client path.
    PathValidation,
    /// Opening and verifying a file on the blocking pool.
    FileOpen,
    /// Planning which configured servers to start, on the blocking pool.
    ServerPlanning,
}

impl fmt::Display for BackgroundTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::McpService => "MCP service",
            Self::LspReceiver => "LSP receiver",
            Self::ListenResolution => "listen URI resolution",
            Self::PathValidation => "path validation",
            Self::FileOpen => "file open",
            Self::ServerPlanning => "server start planning",
        })
    }
}

/// A standard stream of a spawned LSP server process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StdioStream {
    /// Standard input.
    Stdin,
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl fmt::Display for StdioStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stdin => "stdin",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
}

/// A configuration value that is invalid in the context of the entry that holds it.
///
/// Field-level rules live in the types of the fields (a rejected value cannot be
/// built); this enum carries what only the surrounding entry or the whole
/// configuration can add, such as which server a bad value belongs to.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// A server entry lists a `file_patterns` form that maps to no extension.
    #[error("lsp_servers entry '{server}': {pattern}")]
    UnsupportedFilePattern {
        /// The id of the entry that holds the pattern.
        server: ServerId,
        /// The rejected pattern and the supported forms.
        #[source]
        pattern: UnsupportedFilePattern,
    },

    /// The config file's bytes are not UTF-8.
    #[error("config file is not valid UTF-8")]
    NotUtf8(#[source] std::string::FromUtf8Error),

    /// The config file is over its fixed size limit, which is not configurable.
    #[error("config file is {} bytes, over the fixed {} byte limit", .0.size, .0.max)]
    FileTooLarge(SizeExceeded),

    /// A server's `env` table sets one variable twice, which the host treats as
    /// one name.
    #[error("lsp_servers entry '{server}': {error}")]
    DuplicateEnvKey {
        /// The id of the entry that holds the table.
        server: ServerId,
        /// The colliding keys.
        error: DuplicateEnvKey,
    },

    /// A config path has no directory to resolve relative entries against.
    #[error("configuration path has no parent directory: {}", .path.display())]
    NoParentDirectory {
        /// The config path.
        path: PathBuf,
    },

    /// `selection = "auto"` on an entry that is not typescript-language-server.
    #[error(
        "selection = \"auto\" is only valid for typescript-language-server entries (language \
         '{language}'); remove `selection` to use `command` as written"
    )]
    SelectionAutoOnNonTypescript {
        /// The language of the offending entry.
        language: LanguageId,
    },

    /// An allowed server id names no configured server.
    #[error(
        "allowed server '{server}' is not a configured server (configured: {})",
        IdList(configured)
    )]
    UnknownAllowedServer {
        /// The allowed id that matches nothing.
        server: ServerId,
        /// The ids of the configured servers.
        configured: Vec<ServerId>,
    },

    /// Two applicable entries share one server id.
    #[error(
        "duplicate server id '{id}' in this workspace (used by both an entry with {first} and \
         one with {second}); add a unique `name` to each `[[lsp_servers]]` entry"
    )]
    DuplicateServerId {
        /// The shared id.
        id: ServerId,
        /// The first entry.
        first: Box<EntrySummary>,
        /// The second entry.
        second: Box<EntrySummary>,
    },

    /// A language has two servers that both omit `handles`.
    #[error(
        "language '{language}' has two catch-all servers ('{existing}' and '{id}'); at most one \
         server per language may omit `handles`"
    )]
    TwoCatchAllServers {
        /// The language both serve.
        language: LanguageId,
        /// The server registered first.
        existing: ServerId,
        /// The server that collides with it.
        id: ServerId,
    },

    /// Two servers of one language claim the same tool in `handles`.
    #[error("tool '{tool}' for language '{language}' is claimed by both '{existing}' and '{id}'")]
    ToolClaimedTwice {
        /// The contested tool.
        tool: ToolKind,
        /// The language both serve.
        language: LanguageId,
        /// The server registered first.
        existing: ServerId,
        /// The server that collides with it.
        id: ServerId,
    },

    /// A configured workspace root cannot be canonicalized.
    #[error(
        "workspace root '{}' resolved relative to '{}' as '{}' could not be canonicalized",
        .written.display(), .base_dir.display(), .probe.display()
    )]
    UnresolvableWorkspaceRoot {
        /// The root as written in the config.
        written: PathBuf,
        /// The directory a relative root is resolved against.
        base_dir: PathBuf,
        /// The path that was canonicalized.
        probe: PathBuf,
        /// Why canonicalization failed.
        #[source]
        source: std::io::Error,
    },
}

/// A step of the LSP initialization handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitPhase {
    /// The `initialize` request.
    Initialize,
    /// The `initialized` notification.
    Initialized,
    /// The `workspace/didChangeConfiguration` notification.
    DidChangeConfiguration,
}

impl fmt::Display for InitPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Initialize => "Initialize request failed",
            Self::Initialized => "initialized notification failed",
            Self::DidChangeConfiguration => "workspace/didChangeConfiguration notification failed",
        })
    }
}

/// The main error type for mcpls-core operations.
///
/// This enum is `#[non_exhaustive]`: downstream crates that match on it must
/// include a wildcard arm. New variants (such as [`Error::ServerInitializing`])
/// can then be added without further breaking changes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// LSP server failed to initialize.
    #[error("LSP server initialization failed: {phase}: {cause}{}{}", HintSuffix(.hint), StderrSuffix(.stderr))]
    LspInitFailed {
        /// The handshake step that failed.
        phase: InitPhase,
        /// Why the step failed, so a timeout and a rejection by the server
        /// stay distinguishable. Rendered by `Display` instead of being
        /// exposed through `source()`, so the chain is not printed twice.
        cause: Box<Self>,
        /// The likely cause and remedy, when one is known.
        hint: Option<InitFailureHint>,
        /// What the server wrote to stderr before failing, if anything.
        stderr: Option<StderrExcerpt>,
    },

    /// LSP server returned an error response.
    #[error("LSP server error: {code} - {}", sanitize_lsp_server_message(message))]
    LspServerError {
        /// JSON-RPC error code.
        code: i32,
        /// Raw error message from the server, kept verbatim for diagnostics
        /// (logging, `Debug`, pattern matching). The `Display` impl for this
        /// variant rewrites known-internal upstream text before it reaches
        /// an MCP caller, so this field is not always what the caller sees.
        message: String,
        /// Optional additional data from the JSON-RPC error object.
        data: Option<serde_json::Value>,
    },

    /// The MCP server could not complete its handshake with the client.
    #[error("failed to start MCP server: {0}")]
    McpServerStart(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// A background task panicked or was cancelled.
    #[error("{task} task failed: {source}")]
    TaskFailed {
        /// The task that failed.
        task: BackgroundTask,
        /// The join failure, carrying the panic payload or cancellation.
        #[source]
        source: tokio::task::JoinError,
    },

    /// A standard stream of the spawned LSP server could not be captured.
    #[error("failed to capture LSP server {0}")]
    StdioCapture(StdioStream),

    /// The HTTP transport could not bind its listener.
    #[error("failed to bind HTTP listener on {addr}: {source}")]
    HttpBind {
        /// Address the listener was asked to bind.
        addr: std::net::SocketAddr,
        /// Underlying I/O error, e.g. `AddrInUse`.
        #[source]
        source: std::io::Error,
    },

    /// Document was not found or could not be opened.
    #[error("document not found: {0}")]
    DocumentNotFound(PathBuf),

    /// No LSP server configured for the given language.
    #[error(
        "no LSP server configured for language: {language}{}",
        no_server_detail(.language, .file, .patterns)
    )]
    NoServerForLanguage {
        /// The language detected for the file.
        language: LanguageId,
        /// What identifies the file to the language map: its extension, its
        /// extensionless name, or [`FileKey::Unmappable`].
        file: FileKey,
        /// The `file_patterns` configured across all servers, so the error can
        /// show what was available to map the extension.
        patterns: Arc<[FilePattern]>,
    },

    /// A server is configured for the language, but no server claims this
    /// specific tool (either no server lists it in `handles` and there is no
    /// catch-all, or the server that claimed it failed to spawn with no live
    /// catch-all to rebind to).
    #[error("no server handles tool '{tool}' for language '{language_id}'")]
    NoServerForTool {
        /// Language ID the request was for.
        language_id: LanguageId,
        /// Tool that no server claims.
        tool: ToolKind,
    },

    /// The server configured for this request failed to start and, because
    /// startup failures are never retried, will not become available until
    /// mcpls is restarted.
    ///
    /// Boxed to keep [`Error`] small.
    #[error("{}", .0.failed_to_start())]
    ServerFailedToStart(Box<ServerSpawnFailure>),

    /// LSP server for the language is configured but still initializing.
    #[error(
        "LSP server '{server_id}' is still initializing (large project load in progress); wait and retry the request (this may take a few minutes on large projects)"
    )]
    ServerInitializing {
        /// Routing identity of the server that has not yet registered.
        server_id: ServerId,
    },

    /// The server was restarted on request while this call was in flight or
    /// being routed; the request never reached the replacement.
    #[error(
        "LSP server '{server_id}' was restarted while this request was in flight; retry the request"
    )]
    ServerRestarted {
        /// Routing identity of the restarted server.
        server_id: ServerId,
    },

    /// A symbol name could not be resolved to exactly one position.
    ///
    /// Boxed to keep [`Error`] small.
    #[error("{0}")]
    SymbolResolution(Box<SymbolResolutionData>),

    /// A restart request named servers that are not configured.
    #[error(
        "unknown LSP server(s) {}; configured servers: {}",
        IdList(unknown),
        IdList(configured)
    )]
    UnknownServers {
        /// The requested ids that matched no configured server.
        unknown: Vec<ServerId>,
        /// Every configured server id.
        configured: Vec<ServerId>,
    },

    /// A workspace-wide tool (one with no file to resolve a language from,
    /// e.g. `workspace_symbol_search`) could not be routed because at least
    /// one expected LSP server has not registered yet. Unlike
    /// [`Error::ServerInitializing`], resolution never narrowed down to a
    /// single candidate server, so no `server_id` is available.
    #[error(
        "LSP servers are still initializing (large project load in progress); wait and retry the request (this may take a few minutes on large projects)"
    )]
    WorkspaceServersInitializing,

    /// No LSP server is currently configured.
    #[error("no LSP server configured")]
    NoServerConfigured,

    /// At least one server is configured somewhere in the workspace, but
    /// none of them claims a workspace-wide tool that has no file to
    /// resolve a language from (e.g. `workspace_symbol_search`). The
    /// language-less counterpart of [`Error::NoServerForTool`].
    #[error("no server handles tool '{tool}' (no server's `handles` list or catch-all claims it)")]
    NoServerForWorkspaceTool {
        /// Tool that no server claims anywhere in the workspace.
        tool: ToolKind,
    },

    /// Configuration file not found.
    #[error("configuration file not found: {0}")]
    ConfigNotFound(PathBuf),

    /// Untrusted-workspace mode refused a config file that lies inside the
    /// workspace or the current directory, because the analyzed checkout
    /// controls it. A startup error; never reaches a tool response.
    #[error(
        "config file {} lies inside the workspace or the current directory, which untrusted mode does not trust; move it elsewhere",
        .path.display()
    )]
    ConfigInsideWorkspace {
        /// The canonical path of the config file.
        path: PathBuf,
    },

    /// Invalid configuration.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// I/O error; displays the OS text once, with no prefix.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// TOML deserialization error.
    #[error(transparent)]
    TomlDe(#[from] toml::de::Error),

    /// TOML serialization error.
    #[error(transparent)]
    TomlSer(#[from] toml::ser::Error),

    /// Request timeout, carrying the elapsed limit.
    #[error("request timed out after {0:?}")]
    Timeout(Duration),

    /// LSP server failed to spawn.
    #[error("failed to spawn LSP server '{command}': {source}")]
    ServerSpawnFailed {
        /// Command that failed to spawn.
        command: ServerCommand,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// LSP server executable was not found.
    ///
    /// Distinct from [`Error::ServerSpawnFailed`] so the message can carry
    /// PATH and install guidance.
    #[error("failed to spawn LSP server '{command}': {source}{}", NotFoundGuidance(.command.as_str(), Platform::CURRENT))]
    ServerNotFound {
        /// Command that could not be found.
        command: ServerCommand,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// LSP protocol error during message parsing.
    #[error("LSP protocol error: {}", escape_control(.0.as_str()))]
    LspProtocolError(RedactedText),

    /// None of the resource URIs a `subscriptions/listen` request named
    /// resolves inside the workspace.
    #[error("none of the requested resource URIs resolve inside the workspace")]
    NoResolvableListenUris,

    /// A client-supplied position is out of bounds.
    #[error(transparent)]
    InvalidPositionInput(#[from] InvalidPosition),

    /// A client-supplied range is malformed or too large.
    #[error(transparent)]
    InvalidRangeInput(#[from] InvalidRange),

    /// A client-supplied hierarchy item has an invalid range.
    #[error(transparent)]
    InvalidHierarchyItemInput(#[from] InvalidHierarchyItem),

    /// A client-supplied line lies beyond the end of the tracked document.
    ///
    /// Only the line is checked: an LSP server clamps a character past the
    /// end of its line to the line length (LSP 3.17, `Position`), so such a
    /// character is forwarded unchanged.
    #[error(
        "line {line} is beyond the end of the document (the document ends at line {last_line})"
    )]
    PositionBeyondDocument {
        /// The 1-based line the client supplied.
        line: std::num::NonZeroU32,
        /// The 1-based number of the document's last line.
        last_line: std::num::NonZeroU32,
    },

    /// A client-supplied `lsp-diagnostics://` resource URI was rejected.
    #[error(transparent)]
    ResourceUri(#[from] ResourceUriError),

    /// A path mcpls itself derived could not be represented as a URI.
    #[error("cannot convert path to URI: {}", .0.display())]
    PathToUri(PathBuf),

    /// Server process terminated unexpectedly.
    #[error("LSP server process terminated unexpectedly")]
    ServerTerminated,

    /// LSP server shutdown did not complete before its deadline.
    #[error("LSP server shutdown did not complete before its deadline")]
    ShutdownTimeout,

    /// LSP server process exited before completing the `initialize`
    /// handshake.
    #[error("LSP server '{command}' exited during initialization{}{}{}", EarlyExitDetail(.command.as_str(), *.exit_code), HintSuffix(.hint), StderrSuffix(.stderr))]
    ServerExitedDuringInit {
        /// Command that was spawned.
        command: ServerCommand,
        /// Exit code, or `None` if the process was terminated by a signal.
        exit_code: Option<i32>,
        /// The likely cause and remedy, when one is known.
        hint: Option<InitFailureHint>,
        /// What the server wrote to stderr before exiting, if anything.
        stderr: Option<StderrExcerpt>,
    },

    /// A crashed server could not be automatically respawned.
    ///
    /// Distinct from [`Self::ServerTerminated`] so a caller (or a log
    /// reader) can tell "the connection just died" apart from "mcpls tried
    /// to bring it back and could not" -- it is crash-looping and is being
    /// backed off.
    #[error(
        "LSP server '{server_id}' is unavailable: crash-looping, retry in {:.1}s",
        .retry_in.as_secs_f64()
    )]
    ServerUnavailable {
        /// Routing identity of the server that could not be respawned.
        server_id: ServerId,
        /// Remaining backoff before the next respawn attempt.
        retry_in: Duration,
    },

    /// Invalid tool parameters provided.
    #[error("invalid tool parameters: {0}")]
    InvalidToolParams(String),

    /// A client-supplied file path is empty or contains a NUL byte.
    #[error(transparent)]
    InvalidClientPath(#[from] InvalidClientPath),

    /// A client-supplied file path is malformed for the filesystem: it runs
    /// through a regular file, or has an invalid or over-long name.
    ///
    /// Produced only while validating the client path
    /// (`WorkspaceRoots::validate`); the same IO kinds raised later, while
    /// reading or opening an already validated file, stay [`Error::FileIo`]
    /// because there they are environmental.
    #[error("malformed file path {path:?}: {source}")]
    MalformedPath {
        /// The path as supplied by the client.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// File I/O error occurred.
    ///
    /// See [`Error::mcp_error_kind`] for the JSON-RPC classification: a
    /// `source.kind() == ErrorKind::NotFound` failure -- whether `path` was
    /// freshly supplied in this request or was tracked from an earlier one
    /// and has since been deleted/moved on disk -- is caller-fault; any
    /// other IO failure is not.
    #[error("file I/O error for {path:?}: {source}")]
    FileIo {
        /// Path to the file.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Path is outside allowed workspace boundaries.
    #[error("path outside workspace: {0}")]
    PathOutsideWorkspace(PathBuf),

    /// No workspace roots are configured, so path-taking operations are
    /// rejected outright rather than allowed unrestricted (fail closed).
    #[error("no workspace roots configured: refusing access to {0}")]
    NoWorkspaceRoots(PathBuf),

    /// Document limit exceeded.
    #[error(
        "document limit exceeded: {current}/{max} (raise workspace.max_documents in config to increase this)"
    )]
    DocumentLimitExceeded {
        /// Current number of documents.
        current: usize,
        /// Maximum allowed documents.
        max: std::num::NonZeroUsize,
    },

    /// Resource-subscription limit exceeded for the session.
    ///
    /// See [`Error::mcp_error_kind`] for the JSON-RPC classification: same
    /// shape as [`Self::DocumentLimitExceeded`] -- fires on aggregate
    /// per-session tracker state, not this request's params -- so it is
    /// classified the same way, not `InvalidParams`.
    #[error("subscription limit of {max} reached")]
    SubscriptionLimitReached {
        /// Maximum number of subscriptions allowed per session.
        max: usize,
    },

    /// Too many concurrent `subscriptions/listen` streams are open.
    ///
    /// Transient: a stream closing frees a slot, so it is classified as
    /// retryable (see [`Error::mcp_error_kind`]).
    #[error("listen stream limit of {max} reached; retry once another stream closes")]
    ListenStreamsExhausted {
        /// Maximum number of concurrent listen streams.
        max: usize,
    },

    /// A `subscriptions/listen` request asked for more resource URIs than a
    /// single stream may watch.
    #[error(
        "subscriptions/listen request exceeds the limit of {max} resource URIs or their total size budget"
    )]
    ListenFilterTooLarge {
        /// Maximum number of resource URIs per listen stream.
        max: usize,
    },

    /// File size limit exceeded.
    #[error(
        "file size limit exceeded: {} bytes, max {} bytes (raise workspace.max_file_size in config to increase this)",
        .0.size,
        .0.max
    )]
    FileSizeLimitExceeded(SizeExceeded),

    /// Path exists but does not refer to a regular file (e.g. a FIFO or a
    /// character/block device).
    ///
    /// mcpls refuses to read such paths: their reported size does not bound
    /// how much data reading them could produce, and opening some of them
    /// for reading can block indefinitely waiting for a peer. A Unix domain
    /// socket special file is not covered by this variant -- `open(2)` on
    /// one fails outright (`ENXIO`) before the file-type check that produces
    /// this error ever runs, so it surfaces as [`Self::FileIo`] from a
    /// document read and as [`Self::Io`] from the config loader.
    #[error("not a regular file: {0}")]
    NotARegularFile(PathBuf),

    /// All configured LSP servers failed to initialize.
    #[error("all LSP servers failed to initialize: {}", FailureList(failures))]
    AllServersFailedToInit {
        /// Details of each failure.
        failures: Vec<ServerSpawnFailure>,
    },

    /// The server routed for this request does not advertise support for the
    /// requested LSP capability (e.g. no `renameProvider` in its
    /// `ServerCapabilities`).
    #[error("server '{server_id}' does not support capability '{capability}'")]
    CapabilityNotSupported {
        /// Routing identity of the server that lacks the capability.
        server_id: ServerId,
        /// The missing LSP capability, naming the `ServerCapabilities` field
        /// mcpls checked.
        capability: Capability,
    },

    /// The routed server has an active signal indicating its initial
    /// workspace-load/indexing phase is still in progress, and the bounded
    /// wait for it to finish elapsed before it completed. Returned instead
    /// of an unqualified empty/`null` result so a caller cannot mistake
    /// "index not ready yet" for "this position/symbol genuinely has
    /// nothing here".
    #[error(
        "LSP server '{server_id}' is still indexing the workspace after {elapsed_secs}s; wait and retry the request"
    )]
    WorkspaceIndexing {
        /// Routing identity of the server still indexing.
        server_id: ServerId,
        /// How long mcpls waited for readiness before giving up.
        elapsed_secs: u64,
    },
}

/// Bespoke JSON-RPC code for [`Error::WorkspaceIndexing`].
///
/// Picked clear of rmcp's `-32002`/`-32020..-32022`; the range is convention, not a registry.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::WORKSPACE_INDEXING_ERROR_CODE;
///
/// assert_eq!(WORKSPACE_INDEXING_ERROR_CODE, -32050);
/// ```
pub const WORKSPACE_INDEXING_ERROR_CODE: i32 = -32050;

/// Bespoke JSON-RPC code for [`Error::ServerInitializing`].
///
/// Distinct from [`WORKSPACE_INDEXING_ERROR_CODE`] so a client can tell "the
/// server hasn't registered yet" apart from "the server registered but is
/// still indexing" -- both retryable, but for different reasons.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{SERVER_INITIALIZING_ERROR_CODE, WORKSPACE_INDEXING_ERROR_CODE};
///
/// assert_eq!(SERVER_INITIALIZING_ERROR_CODE, -32051);
/// assert_ne!(SERVER_INITIALIZING_ERROR_CODE, WORKSPACE_INDEXING_ERROR_CODE);
/// ```
pub const SERVER_INITIALIZING_ERROR_CODE: i32 = -32051;

/// Bespoke JSON-RPC code for a resource subscription request rejected
/// because it was served over rmcp's stateless per-request HTTP path (#482).
///
/// Same convention range as [`WORKSPACE_INDEXING_ERROR_CODE`]/
/// [`SERVER_INITIALIZING_ERROR_CODE`], next unused slot.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{
///     SERVER_INITIALIZING_ERROR_CODE, STATELESS_SUBSCRIPTION_ERROR_CODE,
///     WORKSPACE_INDEXING_ERROR_CODE,
/// };
///
/// assert_eq!(STATELESS_SUBSCRIPTION_ERROR_CODE, -32052);
/// assert_ne!(STATELESS_SUBSCRIPTION_ERROR_CODE, WORKSPACE_INDEXING_ERROR_CODE);
/// assert_ne!(STATELESS_SUBSCRIPTION_ERROR_CODE, SERVER_INITIALIZING_ERROR_CODE);
/// ```
pub const STATELESS_SUBSCRIPTION_ERROR_CODE: i32 = -32052;

/// Bespoke JSON-RPC code for [`Error::ListenStreamsExhausted`].
///
/// Same convention range as [`WORKSPACE_INDEXING_ERROR_CODE`], next unused
/// slot after [`STATELESS_SUBSCRIPTION_ERROR_CODE`].
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{
///     LISTEN_STREAMS_EXHAUSTED_ERROR_CODE, STATELESS_SUBSCRIPTION_ERROR_CODE,
/// };
///
/// assert_eq!(LISTEN_STREAMS_EXHAUSTED_ERROR_CODE, -32053);
/// assert_ne!(LISTEN_STREAMS_EXHAUSTED_ERROR_CODE, STATELESS_SUBSCRIPTION_ERROR_CODE);
/// ```
pub const LISTEN_STREAMS_EXHAUSTED_ERROR_CODE: i32 = -32053;

/// Bespoke JSON-RPC code for [`Error::ServerRestarted`].
///
/// Same convention range as [`WORKSPACE_INDEXING_ERROR_CODE`], next unused
/// slot after [`LISTEN_STREAMS_EXHAUSTED_ERROR_CODE`].
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{LISTEN_STREAMS_EXHAUSTED_ERROR_CODE, SERVER_RESTARTED_ERROR_CODE};
///
/// assert_eq!(SERVER_RESTARTED_ERROR_CODE, -32054);
/// assert_ne!(SERVER_RESTARTED_ERROR_CODE, LISTEN_STREAMS_EXHAUSTED_ERROR_CODE);
/// ```
pub const SERVER_RESTARTED_ERROR_CODE: i32 = -32054;

/// Structured `data` payload of a retryable JSON-RPC error.
///
/// Each variant pairs a bespoke error code ([`Self::code`]) with its own
/// payload, so a code can never be sent with another variant's data. Keys
/// are `snake_case`; the wire form is the variant's fields as a bare object.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ServerId;
/// use mcpls_core::error::{RetryableErrorData, WORKSPACE_INDEXING_ERROR_CODE};
///
/// let data = RetryableErrorData::WorkspaceIndexing {
///     server_id: ServerId::from_static("rust"),
///     elapsed_secs: 30,
/// };
/// assert_eq!(data.code(), WORKSPACE_INDEXING_ERROR_CODE);
/// assert_eq!(
///     serde_json::to_value(&data).unwrap(),
///     serde_json::json!({"server_id": "rust", "elapsed_secs": 30})
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum RetryableErrorData {
    /// The routed server is still indexing the workspace.
    WorkspaceIndexing {
        /// Server that is indexing.
        server_id: ServerId,
        /// Seconds spent waiting for indexing to finish before giving up.
        elapsed_secs: u64,
    },
    /// The routed server has not finished initializing.
    ServerInitializing {
        /// Server that is initializing.
        server_id: ServerId,
    },
    /// An expected server has not registered yet and no single candidate
    /// could be narrowed down, so there is no server to name. The braces
    /// keep the wire value an empty object rather than `null`.
    WorkspaceServersInitializing {},
    /// The concurrent `subscriptions/listen` stream limit is reached.
    ListenStreamsExhausted {
        /// Maximum number of concurrent listen streams.
        max_listen_streams: usize,
    },
    /// The routed server was restarted while the request was in flight.
    ServerRestarted {
        /// Server that was restarted.
        server_id: ServerId,
    },
}

impl RetryableErrorData {
    /// The bespoke JSON-RPC error code for this retryable condition.
    #[must_use]
    pub const fn code(&self) -> i32 {
        match self {
            Self::WorkspaceIndexing { .. } => WORKSPACE_INDEXING_ERROR_CODE,
            Self::ServerInitializing { .. } | Self::WorkspaceServersInitializing {} => {
                SERVER_INITIALIZING_ERROR_CODE
            }
            Self::ListenStreamsExhausted { .. } => LISTEN_STREAMS_EXHAUSTED_ERROR_CODE,
            Self::ServerRestarted { .. } => SERVER_RESTARTED_ERROR_CODE,
        }
    }
}

/// JSON-RPC error-code classification for an [`Error`], returned by
/// [`Error::mcp_error_kind`].
///
/// mcpls-core has no dependency on the MCP transport crate, so this carries
/// only plain data; `crate::mcp` is responsible for turning it into the
/// actual wire-level error type.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::{Error, McpErrorKind};
///
/// let err = Error::InvalidToolParams("missing `file_path`".to_string());
/// assert_eq!(err.mcp_error_kind(), McpErrorKind::InvalidParams);
/// ```
///
/// This enum is `#[non_exhaustive]`: match it with a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpErrorKind {
    /// Caller-fault: the request itself was invalid. Maps to JSON-RPC
    /// `-32602` (`INVALID_PARAMS`).
    InvalidParams,
    /// Caller-fault: the position fell outside the document and the server's
    /// raw error was rewritten for display. Maps to `-32602` with the
    /// original error as `data`.
    InvalidPosition(RewrittenServerError),
    /// Caller-fault: a symbol name did not resolve to exactly one position.
    /// Maps to `-32602` with the structured resolution outcome as `data`.
    SymbolResolution(SymbolResolutionData),
    /// A transient, retryable server-side condition, distinct from a crash.
    /// Maps to a bespoke JSON-RPC `code` with a structured `data` payload a
    /// caller can act on mechanically, rather than the generic
    /// `INTERNAL_ERROR`.
    Retryable(RetryableErrorData),
    /// An unexpected server-side failure. Maps to JSON-RPC `-32603`
    /// (`INTERNAL_ERROR`).
    Internal,
}

impl Error {
    /// Classify this error for JSON-RPC error-code mapping.
    ///
    /// Matched exhaustively with no wildcard arm: a newly added [`Error`]
    /// variant must be given an explicit classification here instead of
    /// silently defaulting to [`McpErrorKind::Internal`].
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::config::ServerId;
    /// use mcpls_core::error::{Error, McpErrorKind};
    ///
    /// let err = Error::WorkspaceIndexing {
    ///     server_id: ServerId::from_static("rust"),
    ///     elapsed_secs: 30,
    /// };
    /// let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
    ///     panic!("expected a retryable classification");
    /// };
    /// assert_eq!(data.code(), mcpls_core::error::WORKSPACE_INDEXING_ERROR_CODE);
    /// ```
    #[must_use]
    pub fn mcp_error_kind(&self) -> McpErrorKind {
        match self {
            Self::InvalidToolParams(_)
            | Self::UnknownServers { .. }
            | Self::InvalidClientPath(_)
            | Self::MalformedPath { .. }
            | Self::PathOutsideWorkspace(_)
            | Self::NotARegularFile(_)
            | Self::NoResolvableListenUris
            | Self::ResourceUri(_)
            | Self::InvalidPositionInput(_)
            | Self::InvalidRangeInput(_)
            | Self::InvalidHierarchyItemInput(_)
            | Self::PositionBeyondDocument { .. }
            | Self::ListenFilterTooLarge { .. }
            | Self::DocumentNotFound(_)
            | Self::FileSizeLimitExceeded { .. } => McpErrorKind::InvalidParams,

            // Frees up as soon as another listen stream closes.
            Self::ListenStreamsExhausted { max } => {
                McpErrorKind::Retryable(RetryableErrorData::ListenStreamsExhausted {
                    max_listen_streams: *max,
                })
            }

            // A path that doesn't exist -- whether freshly supplied in this
            // request or tracked from an earlier request and then
            // deleted/moved on disk since -- is caller-fault, same as
            // `DocumentNotFound`, and matches the MCP spec's expectation
            // that resource-not-found map to INVALID_PARAMS, not
            // INTERNAL_ERROR (rmcp's `read_resource` handling, SEP-2164).
            // Any other IO failure (permission denied, an invalid-input error
            // while reading an already validated file, etc.) reaching here is
            // a genuine server-side problem the caller cannot fix by changing
            // their request. A malformed client path is `MalformedPath`.
            Self::FileIo { source, .. } => {
                if source.kind() == std::io::ErrorKind::NotFound {
                    McpErrorKind::InvalidParams
                } else {
                    McpErrorKind::Internal
                }
            }

            Self::WorkspaceIndexing {
                server_id,
                elapsed_secs,
            } => McpErrorKind::Retryable(RetryableErrorData::WorkspaceIndexing {
                server_id: server_id.clone(),
                elapsed_secs: *elapsed_secs,
            }),
            Self::ServerInitializing { server_id } => {
                McpErrorKind::Retryable(RetryableErrorData::ServerInitializing {
                    server_id: server_id.clone(),
                })
            }
            Self::SymbolResolution(data) => McpErrorKind::SymbolResolution((**data).clone()),
            Self::ServerRestarted { server_id } => {
                McpErrorKind::Retryable(RetryableErrorData::ServerRestarted {
                    server_id: server_id.clone(),
                })
            }
            // Same condition as `ServerInitializing` -- an expected LSP
            // server hasn't registered yet, retry -- just without a single
            // candidate server narrowed down (see the variant's doc), so
            // there's no `server_id` to report.
            Self::WorkspaceServersInitializing => {
                McpErrorKind::Retryable(RetryableErrorData::WorkspaceServersInitializing {})
            }

            // Same recognized shape `sanitize_lsp_server_message` rewrites
            // for display: rust-analyzer reports this when a position-based
            // request's line/character falls outside the target document --
            // caller-fault. Every other `LspServerError` shape is a genuine
            // server-side problem and stays `Internal`.
            Self::LspServerError { code, message, .. }
                if message.contains(INVALID_OFFSET_MARKER) =>
            {
                McpErrorKind::InvalidPosition(RewrittenServerError::new(*code, message))
            }

            Self::LspInitFailed { .. }
            | Self::LspServerError { .. }
            | Self::McpServerStart(_)
            | Self::TaskFailed { .. }
            | Self::StdioCapture(_)
            | Self::PathToUri(_)
            | Self::HttpBind { .. }
            | Self::NoServerForLanguage { .. }
            | Self::NoServerForTool { .. }
            | Self::NoServerConfigured
            | Self::NoServerForWorkspaceTool { .. }
            | Self::ConfigNotFound(_)
            | Self::ConfigInsideWorkspace { .. }
            | Self::Config(_)
            | Self::Io(_)
            | Self::Json(_)
            | Self::TomlDe(_)
            | Self::TomlSer(_)
            | Self::Timeout(_)
            | Self::ServerSpawnFailed { .. }
            | Self::ServerNotFound { .. }
            | Self::LspProtocolError(_)
            | Self::ServerTerminated
            | Self::ShutdownTimeout
            | Self::ServerUnavailable { .. }
            | Self::ServerFailedToStart(_)
            | Self::ServerExitedDuringInit { .. }
            | Self::NoWorkspaceRoots(_)
            // Unlike `FileSizeLimitExceeded`, this fires on aggregate tracker
            // state, not this request's params -- it can succeed unchanged
            // once other documents close, so `InvalidParams` is wrong; not
            // `Retryable` either, since nothing evicts documents on a timer.
            | Self::DocumentLimitExceeded { .. }
            // Same shape as `DocumentLimitExceeded` above -- see this
            // variant's doc comment.
            | Self::SubscriptionLimitReached { .. }
            | Self::AllServersFailedToInit { .. }
            | Self::CapabilityNotSupported { .. } => McpErrorKind::Internal,
        }
    }

    /// A well-formed resource URI whose path cannot be resolved (deleted, outside
    /// the workspace): unsubscribing falls back to the recorded alias (#499).
    /// Every other failure is a malformed URI or an internal fault.
    pub(crate) const fn is_unresolvable_resource(&self) -> bool {
        match self {
            Self::MalformedPath { .. }
            | Self::FileIo { .. }
            | Self::PathOutsideWorkspace(..)
            | Self::NoWorkspaceRoots(..) => true,
            Self::LspInitFailed { .. }
            | Self::LspServerError { .. }
            | Self::McpServerStart(..)
            | Self::TaskFailed { .. }
            | Self::StdioCapture(..)
            | Self::HttpBind { .. }
            | Self::DocumentNotFound(..)
            | Self::NoServerForLanguage { .. }
            | Self::NoServerForTool { .. }
            | Self::ServerFailedToStart(..)
            | Self::ServerInitializing { .. }
            | Self::ServerRestarted { .. }
            | Self::SymbolResolution(..)
            | Self::UnknownServers { .. }
            | Self::WorkspaceServersInitializing
            | Self::NoServerConfigured
            | Self::NoServerForWorkspaceTool { .. }
            | Self::ConfigNotFound(..)
            | Self::ConfigInsideWorkspace { .. }
            | Self::Config(..)
            | Self::Io(..)
            | Self::Json(..)
            | Self::TomlDe(..)
            | Self::TomlSer(..)
            | Self::Timeout(..)
            | Self::ServerSpawnFailed { .. }
            | Self::ServerNotFound { .. }
            | Self::LspProtocolError(..)
            | Self::NoResolvableListenUris
            | Self::InvalidPositionInput(..)
            | Self::InvalidRangeInput(..)
            | Self::InvalidHierarchyItemInput(..)
            | Self::PositionBeyondDocument { .. }
            | Self::ResourceUri(..)
            | Self::PathToUri(..)
            | Self::ServerTerminated
            | Self::ShutdownTimeout
            | Self::ServerExitedDuringInit { .. }
            | Self::ServerUnavailable { .. }
            | Self::InvalidToolParams(..)
            | Self::InvalidClientPath(..)
            | Self::DocumentLimitExceeded { .. }
            | Self::SubscriptionLimitReached { .. }
            | Self::ListenStreamsExhausted { .. }
            | Self::ListenFilterTooLarge { .. }
            | Self::FileSizeLimitExceeded { .. }
            | Self::NotARegularFile(..)
            | Self::AllServersFailedToInit { .. }
            | Self::CapabilityNotSupported { .. }
            | Self::WorkspaceIndexing { .. } => false,
        }
    }
}

/// A specialized Result type for mcpls-core operations.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::config::{FileExtension, FileName};

    #[test]
    fn test_all_servers_failed_to_init_error() {
        let failures = vec![
            ServerSpawnFailure {
                server_id: ServerId::from_static("rust"),
                language_id: LanguageId::from_static("rust"),
                command: ServerCommand::from_static("rust-analyzer"),
                reason: StartupFailure::InitTaskPanicked,
            },
            ServerSpawnFailure {
                server_id: ServerId::from_static("python"),
                language_id: LanguageId::from_static("python"),
                command: ServerCommand::from_static("pyright"),
                reason: StartupFailure::InitTaskPanicked,
            },
        ];

        let err = Error::AllServersFailedToInit { failures };

        assert!(err.to_string().contains("all LSP servers failed"));

        // Verify failures are preserved
        if let Error::AllServersFailedToInit { failures: f } = err {
            assert_eq!(f.len(), 2);
            assert_eq!(f[0].language_id, "rust");
            assert_eq!(f[1].language_id, "python");
        } else {
            panic!("Expected AllServersFailedToInit error");
        }
    }

    #[test]
    fn test_server_spawn_failure_display_names_init_panic() {
        let failure = ServerSpawnFailure {
            server_id: ServerId::from_static("typescript"),
            language_id: LanguageId::from_static("typescript"),
            command: ServerCommand::from_static("tsserver"),
            reason: StartupFailure::InitTaskPanicked,
        };

        let display = failure.to_string();
        assert!(display.contains("typescript"));
        assert!(display.contains("tsserver"));
        assert!(display.contains("panicked"));
    }

    #[test]
    fn test_ambiguity_display_lists_at_most_ten_candidates_and_the_hint() {
        let candidates: Vec<SymbolCandidate> = (1..=12)
            .map(|line| SymbolCandidate {
                name: "new".to_string(),
                kind: 6,
                kind_name: "Method".to_string(),
                container: Some("Foo".to_string()),
                line,
                character: 1,
            })
            .collect();
        let text = SymbolResolutionData::Ambiguous {
            name: "new".to_string(),
            candidates,
            truncated: false,
        }
        .to_string();

        assert!(text.contains("12 candidates"), "{text}");
        assert!(
            text.contains("at 10:1") && !text.contains("at 11:1"),
            "{text}"
        );
        assert!(text.contains("; ..."), "{text}");
        assert!(text.contains("`symbol_kind` or `container`"), "{text}");
    }

    #[test]
    fn test_error_display_lsp_init_failed() {
        let err = Error::LspInitFailed {
            phase: InitPhase::Initialize,
            cause: Box::new(Error::Io(std::io::Error::other("server not found"))),
            hint: None,
            stderr: None,
        };
        assert_eq!(
            err.to_string(),
            "LSP server initialization failed: Initialize request failed: server not found"
        );
    }

    #[test]
    fn test_stderr_excerpt_none_when_nothing_visible() {
        assert_eq!(
            StderrExcerpt::complete(b"  \n\x1b\r\n ", &Redactions::default()),
            None
        );
        assert_eq!(StderrExcerpt::complete(b"", &Redactions::default()), None);
    }

    #[test]
    fn test_stderr_excerpt_strips_control_characters_but_keeps_newline_and_tab() {
        let excerpt =
            StderrExcerpt::complete(b"a\x1b[31mb\x00\r\nc\td", &Redactions::default()).unwrap();
        assert_eq!(excerpt.head(), "a[31mb\nc\td");
    }

    #[test]
    fn test_stderr_excerpt_display_is_single_line() {
        let excerpt =
            StderrExcerpt::complete(b"first\n\n  second  \nthird\n", &Redactions::default())
                .unwrap();
        assert_eq!(excerpt.to_string(), "first | second | third");
    }

    #[test]
    fn test_stderr_excerpt_elided_display_marks_the_gap() {
        let excerpt =
            StderrExcerpt::elided(b"start\nbanner", b"last\nerror", &Redactions::default())
                .unwrap();
        assert!(excerpt.is_elided());
        assert_eq!(excerpt.tail(), Some("last\nerror"));
        assert_eq!(excerpt.to_string(), "start | banner | ... | last | error");
    }

    #[test]
    fn test_stderr_excerpt_elided_drops_utf8_sequences_cut_by_the_split() {
        let text = "é".as_bytes();
        let head = [b"ok ".as_slice(), &text[..1]].concat();
        let tail = [&text[1..], b" end".as_slice()].concat();

        let excerpt = StderrExcerpt::elided(&head, &tail, &Redactions::default()).unwrap();

        assert_eq!(excerpt.head(), "ok");
        assert_eq!(excerpt.tail(), Some("end"));
    }

    fn secrets(pairs: &[(&str, &str)]) -> Redactions {
        Redactions::new(
            pairs
                .iter()
                .map(|(label, value)| ((*label).to_owned(), (*value).to_owned())),
        )
    }

    #[test]
    fn test_stderr_excerpt_redacts_secrets_with_their_label() {
        let excerpt = StderrExcerpt::complete(
            b"token=hunter2hunter2 id=abc",
            &secrets(&[("API_TOKEN", "hunter2hunter2"), ("SHORT_KEY", "abc")]),
        )
        .unwrap();
        assert_eq!(excerpt.head(), "token=[redacted:API_TOKEN] id=abc");
    }

    #[test]
    fn test_stderr_excerpt_redacts_longest_overlapping_secret_whole() {
        let excerpt = StderrExcerpt::complete(
            b"v=abcdefghXYZ12345",
            &secrets(&[("A_KEY", "abcdefgh"), ("B_KEY", "abcdefghXYZ12345")]),
        )
        .unwrap();
        assert_eq!(excerpt.head(), "v=[redacted:B_KEY]");
    }

    #[test]
    fn test_stderr_excerpt_keeps_non_secret_values_visible() {
        let excerpt = StderrExcerpt::complete(
            b"toolchain 'nightly-2024-01-01' is not installed",
            &secrets(&[("API_TOKEN", "hunter2hunter2")]),
        )
        .unwrap();
        assert_eq!(
            excerpt.head(),
            "toolchain 'nightly-2024-01-01' is not installed"
        );
    }

    #[test]
    fn test_stderr_excerpt_redacts_a_secret_split_by_a_control_character() {
        let excerpt = StderrExcerpt::complete(
            "tok=hunter2\u{200B}hunter2".as_bytes(),
            &secrets(&[("API_TOKEN", "hunter2hunter2")]),
        )
        .unwrap();
        assert_eq!(excerpt.head(), "tok=[redacted:API_TOKEN]");
    }

    /// A secret cut in two by the elision boundary: the fragments of at least
    /// four bytes on each side are masked.
    #[test]
    fn test_stderr_excerpt_masks_a_secret_split_across_the_elision_boundary() {
        let redactions = secrets(&[("API_TOKEN", "supersecretvalue")]);

        let excerpt =
            StderrExcerpt::elided(b"token=supersec", b"retvalue done", &redactions).unwrap();

        let shown = excerpt.to_string();
        assert!(!shown.contains("supersec"), "{shown}");
        assert!(!shown.contains("retvalue"), "{shown}");
        assert!(shown.contains("[redacted:API_TOKEN]"), "{shown}");
    }

    #[test]
    fn test_stderr_excerpt_strips_separators_bidi_and_zero_width_characters() {
        let text = "a\u{2028}b\u{2029}c\u{202E}d\u{2066}e\u{200B}f\u{FEFF}g\u{200F}h";

        let excerpt = StderrExcerpt::complete(text.as_bytes(), &Redactions::default()).unwrap();

        assert_eq!(excerpt.head(), "abcdefgh");
    }

    #[test]
    fn test_escape_control_and_stderr_agree_on_deceptive_characters() {
        let deceptive = [
            '\u{061C}',
            '\u{200B}',
            '\u{200C}',
            '\u{200D}',
            '\u{200E}',
            '\u{200F}',
            '\u{2028}',
            '\u{2029}',
            '\u{202A}',
            '\u{202B}',
            '\u{202C}',
            '\u{202D}',
            '\u{202E}',
            '\u{2060}',
            '\u{2066}',
            '\u{2067}',
            '\u{2068}',
            '\u{2069}',
            '\u{FEFF}',
            '\u{00AD}',
            '\u{180E}',
            '\u{2061}',
            '\u{2064}',
            '\u{206A}',
            '\u{206F}',
            '\u{FFF9}',
            '\u{FFFB}',
            '\u{E0000}',
            '\u{E0041}',
            '\u{E007F}',
        ];
        for c in deceptive {
            let text = format!("a{c}b");
            let escaped = crate::util::escape_control(&text);
            assert!(
                !escaped.contains(c),
                "escape_control kept U+{:04X}",
                c as u32
            );
            let excerpt = StderrExcerpt::complete(text.as_bytes(), &Redactions::default()).unwrap();
            assert_eq!(excerpt.head(), "ab", "stderr kept U+{:04X}", c as u32);
        }
    }

    #[test]
    fn test_init_errors_append_stderr_to_display() {
        let stderr = StderrExcerpt::complete(b"fatal: bad config", &Redactions::default());
        let failed = Error::LspInitFailed {
            phase: InitPhase::Initialize,
            cause: Box::new(Error::Io(std::io::Error::other("boom"))),
            hint: None,
            stderr: stderr.clone(),
        };
        assert_eq!(
            failed.to_string(),
            "LSP server initialization failed: Initialize request failed: boom; stderr: fatal: bad config"
        );
        let exited = Error::ServerExitedDuringInit {
            command: crate::config::ServerCommand::from_static("gopls"),
            exit_code: Some(2),
            hint: None,
            stderr,
        };
        assert_eq!(
            exited.to_string(),
            "LSP server 'gopls' exited during initialization with exit code 2; stderr: fatal: bad config"
        );
    }

    #[test]
    fn test_error_display_lsp_server_error() {
        let err = Error::LspServerError {
            code: -32600,
            message: "Invalid request".to_string(),
            data: None,
        };
        assert_eq!(
            err.to_string(),
            "LSP server error: -32600 - Invalid request"
        );
    }

    #[test]
    fn test_error_display_lsp_server_error_sanitizes_invalid_offset() {
        let err = Error::LspServerError {
            code: -32603,
            message: "Invalid offset LineCol { line: 2291, col: 0 } (line index length: 100417)"
                .to_string(),
            data: None,
        };
        assert_eq!(
            err.to_string(),
            "LSP server error: -32603 - position out of range for this document"
        );
    }

    #[test]
    fn test_error_display_lsp_server_error_sanitizes_wrapped_invalid_offset() {
        // Guards the `contains` (not `starts_with`) match: an upstream
        // wrapper (e.g. an `anyhow::Context`) or a future mcpls-side prefix
        // could prepend text ahead of rust-analyzer's raw message.
        let err = Error::LspServerError {
            code: -32803,
            message: "request handler panicked: Invalid offset LineCol { line: 5, col: 0 } \
                      (line index length: 3)"
                .to_string(),
            data: None,
        };
        assert_eq!(
            err.to_string(),
            "LSP server error: -32803 - position out of range for this document"
        );
    }

    #[test]
    fn test_error_display_lsp_server_error_passes_through_unrelated_message() {
        let err = Error::LspServerError {
            code: -32602,
            message: "Invalid params: expected object".to_string(),
            data: None,
        };
        assert_eq!(
            err.to_string(),
            "LSP server error: -32602 - Invalid params: expected object"
        );
    }

    #[test]
    fn test_error_display_document_not_found() {
        let err = Error::DocumentNotFound(PathBuf::from("/path/to/file.rs"));
        assert!(err.to_string().contains("document not found"));
        assert!(err.to_string().contains("file.rs"));
    }

    #[test]
    fn test_error_display_no_server_for_language() {
        let err = no_server_for_language("rust");
        assert_eq!(
            err.to_string(),
            "no LSP server configured for language: rust"
        );
    }

    #[test]
    fn test_error_display_workspace_servers_initializing() {
        let err = Error::WorkspaceServersInitializing;
        assert!(err.to_string().contains("still initializing"));
    }

    #[test]
    fn test_error_display_no_server_for_workspace_tool() {
        let err = Error::NoServerForWorkspaceTool {
            tool: crate::config::ToolKind::WorkspaceSymbols,
        };
        assert!(err.to_string().contains("workspace_symbols"));
        assert!(err.to_string().contains("no server's `handles` list"));
    }

    #[test]
    fn test_error_display_timeout() {
        assert_eq!(
            Error::Timeout(Duration::from_secs(30)).to_string(),
            "request timed out after 30s"
        );
        assert_eq!(
            Error::Timeout(Duration::from_millis(500)).to_string(),
            "request timed out after 500ms"
        );
    }

    #[test]
    fn test_error_display_document_limit() {
        let err = Error::DocumentLimitExceeded {
            current: 150,
            max: std::num::NonZeroUsize::new(100).unwrap(),
        };
        assert_eq!(
            err.to_string(),
            "document limit exceeded: 150/100 (raise workspace.max_documents in config to increase this)"
        );
    }

    #[test]
    fn test_error_display_file_size_limit() {
        let err = Error::FileSizeLimitExceeded(SizeExceeded {
            size: 20_000_000,
            max: std::num::NonZeroU64::new(10_000_000).unwrap(),
        });
        assert_eq!(
            err.to_string(),
            "file size limit exceeded: 20000000 bytes, max 10000000 bytes (raise workspace.max_file_size in config to increase this)"
        );
    }

    #[test]
    fn test_error_display_not_a_regular_file() {
        let err = Error::NotARegularFile(PathBuf::from("/tmp/some.fifo"));
        assert_eq!(err.to_string(), "not a regular file: /tmp/some.fifo");
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: Error = io_err.into();
        assert_matches!(err, Error::Io(_));
    }

    #[test]
    fn test_error_from_json() {
        let json_str = "{invalid json}";
        let json_err = serde_json::from_str::<serde_json::Value>(json_str).unwrap_err();
        let err: Error = json_err.into();
        assert_matches!(err, Error::Json(_));
    }

    /// #706: no entry of a config error's cause chain repeats the text of the
    /// entry behind it.
    #[test]
    fn test_config_error_chain_does_not_repeat_source_text() {
        let toml_err = toml::from_str::<toml::Value>("[invalid toml").unwrap_err();
        let non_utf8 = String::from_utf8(vec![0xff]).unwrap_err();
        let errors = [
            Error::from(toml_err),
            Error::from(ConfigError::NotUtf8(non_utf8)),
            Error::from(ConfigError::UnresolvableWorkspaceRoot {
                written: PathBuf::from("root"),
                base_dir: PathBuf::from("base"),
                probe: PathBuf::from("base/root"),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such directory"),
            }),
            Error::from(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "access refused",
            )),
        ];
        for err in errors {
            let mut chain = vec![err.to_string()];
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                chain.push(cause.to_string());
                source = cause.source();
            }
            for pair in chain.windows(2) {
                assert!(
                    !pair[0].contains(&pair[1]),
                    "'{}' repeats its source '{}'",
                    pair[0],
                    pair[1]
                );
            }
        }
    }

    #[test]
    fn test_error_from_toml_de() {
        let toml_str = "[invalid toml";
        let toml_err = toml::from_str::<toml::Value>(toml_str).unwrap_err();
        let err: Error = toml_err.into();
        assert_matches!(err, Error::TomlDe(_));
    }

    #[test]
    fn test_result_type_alias() {
        fn _returns_error() -> Result<i32> {
            Err(Error::Config(ConfigError::NoParentDirectory {
                path: PathBuf::new(),
            }))
        }

        let result: Result<i32> = Ok(42);
        assert!(result.is_ok());
        if let Ok(value) = result {
            assert_eq!(value, 42);
        }
    }

    fn not_found(command: &str) -> Error {
        Error::ServerNotFound {
            command: crate::config::ServerCommand::new(command).unwrap(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        }
    }

    #[test]
    fn test_server_not_found_bare_builtin_has_path_and_install_hint() {
        let msg = not_found("rust-analyzer").to_string();
        assert!(msg.contains("not on the PATH"), "{msg}");
        assert!(msg.contains("rustup component add rust-analyzer"), "{msg}");
    }

    #[test]
    fn test_server_not_found_bare_unknown_has_no_install_hint() {
        let msg = not_found("my-custom-lsp").to_string();
        assert!(msg.contains("not on the PATH"), "{msg}");
        assert!(!msg.contains("install it"), "{msg}");
        assert!(!msg.contains(".cmd"), "{msg}");
    }

    #[test]
    fn test_server_not_found_path_command_has_no_path_text() {
        let msg = not_found("/nonexistent/rust-analyzer").to_string();
        assert!(msg.contains("configured path exists"), "{msg}");
        assert!(!msg.contains("PATH"), "{msg}");
        assert!(!msg.contains("install it"), "{msg}");
    }

    #[test]
    fn test_lifecycle_errors_map_to_internal() {
        for err in [Error::ShutdownTimeout, not_found("x")] {
            assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal, "{err:?}");
        }
    }

    #[test]
    fn test_not_found_guidance_cmd_note_only_for_npm_builtins_on_windows() {
        let cases = [
            ("pyright-langserver", Platform::Windows, true),
            ("typescript-language-server", Platform::Windows, true),
            ("pyright-langserver", Platform::Other, false),
            ("rust-analyzer", Platform::Windows, false),
            ("gopls", Platform::Windows, false),
            ("my-custom-lsp", Platform::Windows, false),
        ];
        for (command, platform, expects_cmd_note) in cases {
            let msg = NotFoundGuidance(command, platform).to_string();
            assert_eq!(
                msg.contains(".cmd"),
                expects_cmd_note,
                "{command} {platform:?}: {msg}"
            );
        }
    }

    #[test]
    fn test_error_source_chain() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err = Error::ServerSpawnFailed {
            command: crate::config::ServerCommand::from_static("rust-analyzer"),
            source: io_err,
        };

        let source = std::error::Error::source(&err);
        assert!(source.is_some());
    }

    fn spawn_failure(id: &str, command: &str, error: Error) -> ServerSpawnFailure {
        ServerSpawnFailure {
            server_id: ServerId::new(id).unwrap(),
            language_id: LanguageId::new(id).unwrap(),
            command: ServerCommand::new(command).unwrap(),
            reason: StartupFailure::Spawn(Arc::new(error)),
        }
    }

    #[test]
    fn test_server_spawn_failure_display() {
        let failure = spawn_failure(
            "rust",
            "rust-analyzer",
            Error::LspInitFailed {
                phase: InitPhase::Initialize,
                cause: Box::new(Error::Io(std::io::Error::other("boom"))),
                hint: None,
                stderr: None,
            },
        );
        assert_eq!(
            failure.to_string(),
            "rust [rust] (rust-analyzer): LSP server initialization failed: Initialize request failed: boom"
        );
    }

    #[test]
    fn test_server_spawn_failure_clone_shares_reason() {
        let failure = spawn_failure("python", "pyright", not_found("pyright"));
        let cloned = failure.clone();
        assert_eq!(failure.language_id, cloned.language_id);
        assert_eq!(failure.to_string(), cloned.to_string());
    }

    #[test]
    fn test_server_failed_to_start_display_carries_not_found_guidance() {
        let err = Error::ServerFailedToStart(Box::new(spawn_failure(
            "rust",
            "rust-analyzer",
            not_found("rust-analyzer"),
        )));
        let msg = err.to_string();
        assert!(
            msg.contains("'rust' for language 'rust' failed to start"),
            "{msg}"
        );
        assert!(msg.contains("not on the PATH"), "{msg}");
        assert!(msg.contains("rustup component add rust-analyzer"), "{msg}");
        assert!(msg.contains("restart mcpls"), "{msg}");
    }

    #[test]
    fn test_server_failed_to_start_init_task_panicked_display() {
        let err = Error::ServerFailedToStart(Box::new(ServerSpawnFailure {
            server_id: ServerId::from_static("rust"),
            language_id: LanguageId::from_static("rust"),
            command: ServerCommand::from_static("rust-analyzer"),
            reason: StartupFailure::InitTaskPanicked,
        }));
        assert!(err.to_string().contains("initialization task panicked"));
    }

    #[test]
    fn test_startup_errors_map_to_internal() {
        let failure = spawn_failure("rust", "rust-analyzer", not_found("rust-analyzer"));
        for err in [
            Error::ServerFailedToStart(Box::new(failure.clone())),
            Error::AllServersFailedToInit {
                failures: vec![failure],
            },
            Error::ServerExitedDuringInit {
                command: crate::config::ServerCommand::from_static("x"),
                exit_code: Some(1),
                hint: None,
                stderr: None,
            },
            Error::ServerUnavailable {
                server_id: ServerId::from_static("rust"),
                retry_in: Duration::from_secs(1),
            },
        ] {
            assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal, "{err:?}");
        }
    }

    #[test]
    fn test_init_failure_hint_renders_between_detail_and_stderr() {
        let stderr = StderrExcerpt::complete(b"boom", &Redactions::default());
        let hint = Some(InitFailureHint::NativeTypescriptOnly);
        let failed = Error::LspInitFailed {
            phase: InitPhase::Initialize,
            cause: Box::new(Error::Io(std::io::Error::other("x"))),
            hint,
            stderr: stderr.clone(),
        }
        .to_string();
        let exited = Error::ServerExitedDuringInit {
            command: crate::config::ServerCommand::from_static("typescript-language-server"),
            exit_code: Some(1),
            hint,
            stderr,
        }
        .to_string();
        for text in [failed, exited] {
            let hint_at = text.find("TypeScript 7").unwrap();
            assert!(hint_at < text.find("; stderr: boom").unwrap(), "{text}");
            assert!(
                text.contains(BuiltinServer::TypescriptLanguageServer.install_hint()),
                "{text}"
            );
        }
    }

    #[test]
    fn test_server_exited_during_init_display_hints_only_for_rust_analyzer() {
        let hinted = Error::ServerExitedDuringInit {
            command: crate::config::ServerCommand::from_static("rust-analyzer"),
            exit_code: Some(1),
            hint: None,
            stderr: None,
        }
        .to_string();
        assert!(hinted.contains("exit code 1"), "{hinted}");
        assert!(
            hinted.contains("rustup component add rust-analyzer"),
            "{hinted}"
        );

        let plain = Error::ServerExitedDuringInit {
            command: crate::config::ServerCommand::from_static("gopls"),
            exit_code: None,
            hint: None,
            stderr: None,
        }
        .to_string();
        assert!(plain.contains("terminated by a signal"), "{plain}");
        assert!(!plain.contains("rustup"), "{plain}");
    }

    #[test]
    fn test_server_unavailable_display_names_retry_delay() {
        let err = Error::ServerUnavailable {
            server_id: ServerId::from_static("rust"),
            retry_in: Duration::from_secs(2),
        };
        assert_eq!(
            err.to_string(),
            "LSP server 'rust' is unavailable: crash-looping, retry in 2.0s"
        );
    }

    #[test]
    fn test_error_display_all_servers_failed_lists_each_failure() {
        let err = Error::AllServersFailedToInit {
            failures: vec![
                spawn_failure("rust", "rust-analyzer", not_found("rust-analyzer")),
                spawn_failure(
                    "python",
                    "pyright",
                    Error::LspInitFailed {
                        phase: InitPhase::Initialize,
                        cause: Box::new(Error::Io(std::io::Error::other("denied"))),
                        hint: None,
                        stderr: None,
                    },
                ),
            ],
        };
        let msg = err.to_string();
        assert!(
            msg.starts_with("all LSP servers failed to initialize: "),
            "{msg}"
        );
        assert!(msg.contains("rust [rust] (rust-analyzer)"), "{msg}");
        assert!(msg.contains("python [python] (pyright)"), "{msg}");
    }

    #[test]
    fn test_untrusted_refusal_paths_are_escaped_and_bounded() {
        let hostile = PathBuf::from(format!("/ws/evil{}", "\n\u{202e}".repeat(3000)));
        for refusal in [
            UntrustedRefusal::WorkspaceExecutable {
                executable: hostile.clone(),
            },
            UntrustedRefusal::WorkspaceTsserver {
                tsserver: hostile.clone(),
            },
            UntrustedRefusal::WorkspaceHome {
                variable: HomeVariable::Home,
                home: hostile.clone(),
            },
            UntrustedRefusal::NonUtf8Path {
                what: ResolvedItem::Executable,
                path: hostile,
            },
        ] {
            let text = refusal.to_string();
            assert!(!text.contains('\n'), "{text}");
            assert!(!text.contains('\u{202e}'), "{text}");
            assert!(text.len() < MAX_ECHOED_PATH_BYTES + 512, "{}", text.len());
        }
        let name = EchoedArgument::name(&"\u{202e}".repeat(5000));
        assert!(name.as_str().len() < MAX_SYMBOL_NAME_BYTES + 64);
    }

    #[test]
    fn test_untrusted_refusal_new_variants_name_their_cause() {
        let launcher = UntrustedRefusal::ProjectLauncher {
            command: ServerCommand::from_static("npx"),
            cause: LauncherRefusal::SelectsWorkspaceCode {
                program: EchoedArgument::name("npx"),
                trigger: LaunchTrigger::Always,
            },
        };
        assert!(launcher.to_string().contains("'npx'"), "{launcher}");
        let unpinned = UntrustedRefusal::UnpinnedTypescriptLauncher {
            command: ServerCommand::from_static("pnpm"),
        };
        assert!(unpinned.to_string().contains("'pnpm'"), "{unpinned}");
        let non_utf8 = UntrustedRefusal::NonUtf8Path {
            what: ResolvedItem::SearchPath,
            path: PathBuf::from("/odd"),
        };
        assert_eq!(
            non_utf8.to_string(),
            "its PATH, /odd, is not valid UTF-8, so untrusted mode cannot pass it on"
        );
        assert!(
            UntrustedRefusal::NoSafeWorkingDirectory
                .to_string()
                .contains("outside the workspace")
        );
    }

    fn no_server_for_language(language: &'static str) -> Error {
        Error::NoServerForLanguage {
            language: LanguageId::from_static(language),
            file: FileKey::Unmappable,
            patterns: Arc::default(),
        }
    }

    #[test]
    fn test_no_server_for_plaintext_names_extension_and_patterns() {
        let err = Error::NoServerForLanguage {
            language: LanguageId::PLAINTEXT,
            file: FileKey::Extension(FileExtension::from_static("cpp")),
            patterns: Arc::from([
                FilePattern::from_static("**/*.rs"),
                FilePattern::from_static("**/*.h"),
            ]),
        };
        let message = err.to_string();
        assert!(message.starts_with("no LSP server configured for language: plaintext ("));
        assert!(
            message.contains("file extension 'cpp' is not mapped"),
            "{message}"
        );
        assert!(
            message.contains("configured file_patterns: **/*.rs, **/*.h"),
            "{message}"
        );
    }

    #[test]
    fn test_no_server_for_plaintext_without_patterns_or_extension() {
        let err = Error::NoServerForLanguage {
            language: LanguageId::PLAINTEXT,
            file: FileKey::Unmappable,
            patterns: Arc::default(),
        };
        let message = err.to_string();
        assert!(
            message.contains("the file has no usable extension or name"),
            "{message}"
        );
        assert!(
            message.contains("no file_patterns are configured"),
            "{message}"
        );
    }

    #[test]
    fn test_no_server_for_other_language_text_is_unchanged() {
        let err = Error::NoServerForLanguage {
            language: LanguageId::from_static("nushell"),
            file: FileKey::Extension(FileExtension::from_static("nu")),
            patterns: Arc::from([FilePattern::from_static("**/*.rs")]),
        };
        assert_eq!(
            err.to_string(),
            "no LSP server configured for language: nushell"
        );
    }

    #[test]
    fn test_no_server_for_plaintext_name_suggests_a_name_pattern() {
        let err = Error::NoServerForLanguage {
            language: LanguageId::PLAINTEXT,
            file: FileKey::Name(FileName::from_static("Makefile")),
            patterns: Arc::from([FilePattern::from_static("**/*.rs")]),
        };
        let message = err.to_string();
        assert!(
            message.contains("file name 'Makefile' is not mapped"),
            "{message}"
        );
        assert!(
            message.contains("a `**/Makefile` file_patterns entry"),
            "{message}"
        );
        assert!(!message.contains("*.EXT"), "{message}");
    }

    #[test]
    fn test_error_display_capability_not_supported() {
        let err = Error::CapabilityNotSupported {
            server_id: ServerId::from_static("rust"),
            capability: Capability::Rename,
        };
        assert_eq!(
            err.to_string(),
            "server 'rust' does not support capability 'renameProvider'"
        );
    }

    #[test]
    fn test_error_display_workspace_indexing() {
        let err = Error::WorkspaceIndexing {
            server_id: ServerId::from_static("rust"),
            elapsed_secs: 30,
        };
        assert_eq!(
            err.to_string(),
            "LSP server 'rust' is still indexing the workspace after 30s; wait and retry the request"
        );
    }

    /// #479: caller-fault variants must classify as `InvalidParams`, not fall
    /// through to the generic `Internal` bucket.
    #[test]
    fn test_mcp_error_kind_caller_fault_variants_are_invalid_params() {
        let caller_fault_errors = vec![
            Error::InvalidToolParams("bad params".to_string()),
            Error::PathOutsideWorkspace(PathBuf::from("/etc/passwd")),
            Error::NotARegularFile(PathBuf::from("/dev/null")),
            Error::NoResolvableListenUris,
            Error::ResourceUri(ResourceUriError::InvalidScheme("x".to_string())),
            Error::DocumentNotFound(PathBuf::from("/missing.rs")),
            Error::FileSizeLimitExceeded(SizeExceeded {
                size: 100,
                max: std::num::NonZeroU64::new(10).unwrap(),
            }),
            Error::ListenFilterTooLarge { max: 1000 },
            Error::InvalidClientPath(InvalidClientPath::ContainsNul),
        ];

        for err in caller_fault_errors {
            assert_eq!(
                err.mcp_error_kind(),
                McpErrorKind::InvalidParams,
                "expected {err:?} to classify as InvalidParams"
            );
        }
    }

    #[tokio::test]
    async fn test_mcp_error_kind_internal_failures_are_internal() {
        let join_error = tokio::spawn(async { panic!("boom") }).await.unwrap_err();
        let internal_errors = vec![
            Error::TaskFailed {
                task: BackgroundTask::McpService,
                source: join_error,
            },
            Error::StdioCapture(StdioStream::Stdin),
            Error::PathToUri(PathBuf::from("/x")),
            Error::McpServerStart("handshake".into()),
        ];

        for err in internal_errors {
            assert_eq!(
                err.mcp_error_kind(),
                McpErrorKind::Internal,
                "expected {err:?} to classify as Internal"
            );
        }
    }

    #[tokio::test]
    async fn test_task_failed_keeps_the_panic_payload_as_source() {
        let join_error = tokio::spawn(async { panic!("secret boom") })
            .await
            .unwrap_err();
        let err = Error::TaskFailed {
            task: BackgroundTask::LspReceiver,
            source: join_error,
        };
        let source = std::error::Error::source(&err).unwrap();
        assert!(source.to_string().contains("secret boom"), "{source}");
        assert!(err.to_string().starts_with("LSP receiver task failed"));
    }

    #[test]
    fn test_mcp_error_kind_workspace_indexing_is_retryable_with_dedicated_code() {
        let err = Error::WorkspaceIndexing {
            server_id: ServerId::from_static("rust"),
            elapsed_secs: 30,
        };
        let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
            panic!("expected WorkspaceIndexing to classify as Retryable");
        };
        assert_eq!(data.code(), WORKSPACE_INDEXING_ERROR_CODE);
        assert_eq!(
            serde_json::to_value(&data).unwrap(),
            serde_json::json!({"server_id": "rust", "elapsed_secs": 30})
        );
    }

    #[test]
    fn test_mcp_error_kind_server_initializing_is_retryable_with_dedicated_code() {
        let err = Error::ServerInitializing {
            server_id: ServerId::from_static("python"),
        };
        let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
            panic!("expected ServerInitializing to classify as Retryable");
        };
        assert_eq!(data.code(), SERVER_INITIALIZING_ERROR_CODE);
        assert_eq!(
            serde_json::to_value(&data).unwrap(),
            serde_json::json!({"server_id": "python"})
        );
        assert_ne!(
            data.code(),
            WORKSPACE_INDEXING_ERROR_CODE,
            "ServerInitializing must be distinguishable on the wire from WorkspaceIndexing"
        );
    }

    /// `WorkspaceServersInitializing` is `ServerInitializing`'s counterpart
    /// for a resolution that never narrowed down to a single server (see the
    /// variant's doc comment), so it must be retryable too -- a client that
    /// auto-retries on the bespoke retryable code must not treat this as a
    /// hard failure just because no `server_id` was available.
    #[test]
    fn test_mcp_error_kind_workspace_servers_initializing_is_retryable() {
        let err = Error::WorkspaceServersInitializing;
        let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
            panic!("expected WorkspaceServersInitializing to classify as Retryable");
        };
        assert_eq!(data.code(), SERVER_INITIALIZING_ERROR_CODE);
        assert_eq!(
            serde_json::to_value(&data).unwrap(),
            serde_json::json!({}),
            "the empty struct variant must serialize as `{{}}`, not `null`"
        );
    }

    #[test]
    fn test_retryable_error_data_codes_and_snake_case_keys() {
        let cases = [
            (
                RetryableErrorData::WorkspaceIndexing {
                    server_id: ServerId::from_static("rust"),
                    elapsed_secs: 7,
                },
                WORKSPACE_INDEXING_ERROR_CODE,
                serde_json::json!({"server_id": "rust", "elapsed_secs": 7}),
            ),
            (
                RetryableErrorData::ServerInitializing {
                    server_id: ServerId::from_static("python"),
                },
                SERVER_INITIALIZING_ERROR_CODE,
                serde_json::json!({"server_id": "python"}),
            ),
            (
                RetryableErrorData::WorkspaceServersInitializing {},
                SERVER_INITIALIZING_ERROR_CODE,
                serde_json::json!({}),
            ),
            (
                RetryableErrorData::ListenStreamsExhausted {
                    max_listen_streams: 100,
                },
                LISTEN_STREAMS_EXHAUSTED_ERROR_CODE,
                serde_json::json!({"max_listen_streams": 100}),
            ),
        ];
        for (data, code, wire) in cases {
            assert_eq!(data.code(), code);
            assert_eq!(serde_json::to_value(&data).unwrap(), wire);
        }
    }

    #[test]
    fn test_mcp_error_kind_listen_streams_exhausted_is_retryable() {
        let err = Error::ListenStreamsExhausted { max: 100 };
        let McpErrorKind::Retryable(data) = err.mcp_error_kind() else {
            panic!("expected ListenStreamsExhausted to classify as Retryable");
        };
        assert_eq!(data.code(), LISTEN_STREAMS_EXHAUSTED_ERROR_CODE);
        assert_eq!(
            data,
            RetryableErrorData::ListenStreamsExhausted {
                max_listen_streams: 100
            }
        );
    }

    #[test]
    fn test_mcp_error_kind_unretained_variants_stay_internal() {
        let internal_errors = vec![
            no_server_for_language("python"),
            Error::NoServerForTool {
                language_id: LanguageId::from_static("rust"),
                tool: crate::config::ToolKind::Hover,
            },
            Error::CapabilityNotSupported {
                server_id: ServerId::from_static("rust"),
                capability: Capability::Rename,
            },
            Error::NoWorkspaceRoots(PathBuf::from("/tmp")),
            Error::DocumentLimitExceeded {
                current: 150,
                max: std::num::NonZeroUsize::new(100).unwrap(),
            },
            Error::SubscriptionLimitReached { max: 1000 },
        ];

        for err in internal_errors {
            assert_eq!(
                err.mcp_error_kind(),
                McpErrorKind::Internal,
                "expected {err:?} to classify as Internal"
            );
        }
    }

    /// #479 regression: a client-supplied path that doesn't exist (the
    /// common case behind `WorkspaceRoots::validate`'s `canonicalize()`
    /// failure) must classify as caller-fault, matching `DocumentNotFound`.
    #[test]
    fn test_mcp_error_kind_file_io_not_found_is_invalid_params() {
        let err = Error::FileIo {
            path: PathBuf::from("/no/such/file.rs"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file or directory"),
        };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::InvalidParams);
    }

    #[test]
    fn test_position_beyond_document_is_invalid_params_naming_the_line() {
        let err = Error::PositionBeyondDocument {
            line: std::num::NonZeroU32::new(7).unwrap(),
            last_line: std::num::NonZeroU32::new(3).unwrap(),
        };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::InvalidParams);
        assert_eq!(
            err.to_string(),
            "line 7 is beyond the end of the document (the document ends at line 3)"
        );
    }

    /// #575: a malformed client path is caller-fault, whatever IO kind the
    /// validation hit.
    #[test]
    fn test_mcp_error_kind_malformed_path_is_invalid_params() {
        use std::io::ErrorKind;

        for kind in [
            ErrorKind::NotADirectory,
            ErrorKind::InvalidFilename,
            ErrorKind::InvalidInput,
        ] {
            let err = Error::MalformedPath {
                path: PathBuf::from("/ws/main.rs/x"),
                source: std::io::Error::new(kind, "bad path"),
            };
            assert_eq!(
                err.mcp_error_kind(),
                McpErrorKind::InvalidParams,
                "{kind:?}"
            );
        }
    }

    /// The same IO kinds from a post-validation read or open are
    /// environmental, not caller-fault.
    #[test]
    fn test_mcp_error_kind_post_validation_file_io_invalid_input_stays_internal() {
        let err = Error::FileIo {
            path: PathBuf::from("/ws/main.rs"),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad input"),
        };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }

    /// Counterpart: a non-not-found IO failure (permission denied, etc.) is
    /// a genuine server-side problem, not something the caller can fix by
    /// changing their request.
    #[test]
    fn test_mcp_error_kind_file_io_other_kind_stays_internal() {
        let err = Error::FileIo {
            path: PathBuf::from("/root/secret.rs"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied"),
        };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }

    /// #496: an `LspServerError` carrying the recognized "position out of
    /// range" shape (same substring `sanitize_lsp_server_message` rewrites
    /// for display) is caller-fault, not a generic internal failure.
    #[test]
    fn test_mcp_error_kind_lsp_server_error_invalid_offset_is_invalid_params() {
        let err = Error::LspServerError {
            code: -32603,
            message: "Invalid offset LineCol { line: 2291, col: 0 } (line index length: 100417)"
                .to_string(),
            data: None,
        };
        assert_eq!(
            err.mcp_error_kind(),
            McpErrorKind::InvalidPosition(RewrittenServerError::new(
                -32603,
                "Invalid offset LineCol { line: 2291, col: 0 } (line index length: 100417)"
            ))
        );
    }

    /// #465: server-supplied control characters never reach an error `Display`
    /// raw, so a logged or echoed error cannot forge log lines.
    #[test]
    fn test_display_escapes_control_characters_from_the_server() {
        let server_error = Error::LspServerError {
            code: -32603,
            message: "boom\nERROR forged\x1b[31m".to_string(),
            data: None,
        };
        assert_eq!(
            server_error.to_string(),
            "LSP server error: -32603 - boom\\nERROR forged\\u{1b}[31m"
        );

        let protocol_error = Error::LspProtocolError(RedactedText::fixed("bad\nline"));
        assert_eq!(protocol_error.to_string(), "LSP protocol error: bad\\nline");
    }

    /// The raw message carried as `data` is bounded at construction.
    #[test]
    fn test_rewritten_server_error_bounds_raw_message() {
        let raw =
            RewrittenServerError::new(-32603, &"x".repeat(MAX_ERROR_MESSAGE_CALLER_BYTES * 2));
        assert!(raw.raw_message().len() < MAX_ERROR_MESSAGE_CALLER_BYTES * 2);
        assert!(raw.raw_message().ends_with("(truncated)"));
    }

    /// Counterpart: an `LspServerError` whose message doesn't match the
    /// recognized position-out-of-range shape is a genuine server-side
    /// problem and must stay `Internal`.
    #[test]
    fn test_mcp_error_kind_lsp_server_error_other_message_stays_internal() {
        let err = Error::LspServerError {
            code: -32603,
            message: "internal error".to_string(),
            data: None,
        };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }

    /// #496: `SubscriptionLimitReached` fires on aggregate per-session
    /// tracker state, not this request's params, so it must classify the
    /// same way as `DocumentLimitExceeded` -- not `InvalidParams`.
    #[test]
    fn test_mcp_error_kind_subscription_limit_reached_stays_internal() {
        let err = Error::SubscriptionLimitReached { max: 1000 };
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }

    fn refusal_failure(refusal: UntrustedRefusal) -> ServerSpawnFailure {
        ServerSpawnFailure {
            server_id: ServerId::from_static("rust"),
            language_id: LanguageId::new("rust").unwrap(),
            command: ServerCommand::from_static("rust-analyzer"),
            reason: StartupFailure::RefusedUntrustedWorkspace(refusal),
        }
    }

    #[test]
    fn test_not_allowed_refusal_names_the_server_class_and_the_remedy() {
        let err =
            Error::ServerFailedToStart(Box::new(refusal_failure(UntrustedRefusal::NotAllowed {
                builtin: Some(BuiltinServer::RustAnalyzer),
            })));
        assert_eq!(
            err.to_string(),
            "LSP server 'rust' for language 'rust' was not started: the workspace is untrusted \
             and it may run workspace code (Cargo build scripts and procedural macros); restart \
             mcpls with `--allow-server rust` to start it"
        );
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }

    #[test]
    fn test_workspace_executable_refusal_offers_no_allow_remedy() {
        let err = Error::ServerFailedToStart(Box::new(refusal_failure(
            UntrustedRefusal::WorkspaceExecutable {
                executable: PathBuf::from("/ws/bin/rust-analyzer"),
            },
        )));
        let text = err.to_string();
        assert!(
            text.contains("its executable /ws/bin/rust-analyzer lies inside the workspace"),
            "{text}"
        );
        assert!(!text.contains("--allow-server"), "{text}");
        assert!(!text.contains("not retried"), "{text}");
    }

    #[test]
    fn test_custom_server_refusal_says_its_workspace_code_is_unknown() {
        let text = UntrustedRefusal::NotAllowed { builtin: None }.to_string();
        assert!(text.contains("not a built-in server"), "{text}");
    }

    #[test]
    fn test_config_inside_workspace_is_a_startup_error_of_kind_internal() {
        let err = Error::ConfigInsideWorkspace {
            path: PathBuf::from("/ws/mcpls.toml"),
        };
        assert!(err.to_string().contains("/ws/mcpls.toml"));
        assert_eq!(err.mcp_error_kind(), McpErrorKind::Internal);
    }

    #[test]
    fn echoed_path_keeps_spaces_and_equals() {
        let path = EchoedPath::new(Path::new("/tmp/out/no such dir/a=b/srv"));
        assert_eq!(path.as_str(), "/tmp/out/no such dir/a=b/srv");
    }

    #[test]
    fn echoed_path_never_cuts_an_escape_in_half() {
        let escape = "\\u{202e}";
        let hostile = "\u{202e}".repeat(MAX_ECHOED_PATH_BYTES);
        let echoed = EchoedPath::new(Path::new(&hostile));
        let kept = echoed
            .as_str()
            .strip_suffix(TRUNCATION_MARKER)
            .expect("an over-long path is marked as truncated");
        assert!(kept.len() <= MAX_ECHOED_PATH_BYTES);
        assert!(
            !kept.is_empty() && kept.len().is_multiple_of(escape.len()),
            "{kept}"
        );
        assert_eq!(kept, escape.repeat(kept.len() / escape.len()));
    }

    #[test]
    fn echoed_program_cuts_an_assignment_but_keeps_a_path() {
        assert_eq!(EchoedPath::program("API_TOKEN=abc").as_str(), "API_TOKEN");
        assert_eq!(EchoedPath::program("X=/tmp/secret").as_str(), "X");
        assert_eq!(
            EchoedPath::program("/tmp/out/a=b/srv").as_str(),
            "/tmp/out/a=b/srv"
        );
        assert_eq!(EchoedPath::program("no such dir").as_str(), "no such dir");
    }

    #[test]
    fn echoed_path_within_the_bound_is_not_marked() {
        let path = "a".repeat(MAX_ECHOED_PATH_BYTES);
        assert_eq!(EchoedPath::new(Path::new(&path)).as_str(), path);
    }

    #[test]
    fn wrapped_program_refusals_name_the_program_its_launcher_starts() {
        let program = EchoedPath::new(Path::new("/tmp/out/a=b"));
        let text = UntrustedRefusal::WrappedProgramPathContainsEquals { program }.to_string();
        assert!(text.contains("'/tmp/out/a=b'"), "{text}");
        assert!(text.contains("the program '"), "{text}");
        assert!(!text.contains("its launcher '/tmp"), "{text}");
    }
}
