//! Vocabulary describing why a launch was refused or could not be analyzed.
//!
//! These types model the decisions of `lsp/launcher.rs` and
//! `runtime/untrusted.rs`.

use std::fmt;
use std::path::PathBuf;

use super::{EchoedArgument, EchoedPath};
use crate::config::{BuiltinServer, EnvTooLarge, InvalidAutoSelection, ServerCommand};
use crate::spelling::spelled_enum;

spelled_enum! {
    /// An environment variable that names the user's home directory.
    HomeVariable {
        /// `HOME`.
        Home => "HOME",
        /// `USERPROFILE`.
        UserProfile => "USERPROFILE",
    }
}

/// What untrusted-workspace mode resolved to a path it could not pass on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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

spelled_enum! {
    /// A subcommand of a runner that starts a program the workspace chooses.
    RunnerSubcommand {
        /// `run`.
        Run => "run",
        /// `x`.
        X => "x",
        /// `task`.
        Task => "task",
        /// `eval`.
        Eval => "eval",
        /// `repl`.
        Repl => "repl",
        /// `tool`.
        Tool => "tool",
        /// `exec`.
        Exec => "exec",
        /// cabal's `v1-run`.
        V1Run => "v1-run",
        /// cabal's `v2-run`.
        V2Run => "v2-run",
        /// cabal's `new-run`.
        NewRun => "new-run",
        /// cabal's `v1-exec`.
        V1Exec => "v1-exec",
        /// cabal's `v2-exec`.
        V2Exec => "v2-exec",
        /// cabal's `new-exec`.
        NewExec => "new-exec",
        /// stack's `runghc`.
        Runghc => "runghc",
        /// stack's `runhaskell`.
        Runhaskell => "runhaskell",
        /// stack's `script`.
        Script => "script",
        /// cargo's `r` shorthand for `run`.
        R => "r",
        /// cargo's `t` shorthand for `test`.
        T => "t",
        /// `test`, which runs the project's tests.
        Test => "test",
        /// cargo's `bench`.
        Bench => "bench",
        /// stack's `ghci`.
        Ghci => "ghci",
        /// cabal's `v1-repl`.
        V1Repl => "v1-repl",
        /// cabal's `v2-repl`.
        V2Repl => "v2-repl",
        /// cabal's `new-repl`.
        NewRepl => "new-repl",
    }
}

spelled_enum! {
    /// The spelling of a long flag that gives a shell a command string.
    LongCommandName {
        /// `--command`.
        Command => "--command",
        /// `--commands`.
        Commands => "--commands",
        /// `--init-command`, which fish runs as a command string.
        InitCommand => "--init-command",
        /// `--execute`, which nushell runs as a command string.
        Execute => "--execute",
    }
}

spelled_enum! {
    /// A PowerShell parameter that gives it a command, however abbreviated.
    PowerShellParameter {
        /// `-Command`.
        Command => "-Command",
        /// `-CommandWithArgs`.
        CommandWithArgs => "-CommandWithArgs",
        /// `-EncodedCommand`.
        EncodedCommand => "-EncodedCommand",
    }
}

/// The flag that gives a shell a command string, as the shell's family
/// spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShellFlag {
    /// `-c`, alone or inside a cluster such as `-lc`.
    DashC,
    /// `+c`, which POSIX shells read like `-c`.
    PlusC,
    /// fish's `-C`, which runs an init command string.
    DashCapitalC,
    /// nushell's `-e`, which executes a command string.
    DashE,
    /// A long flag such as `--command`.
    LongCommand(LongCommandName),
    /// A PowerShell parameter or an abbreviation or alias of it.
    PowerShell(PowerShellParameter),
}

/// How a [`ShellFlag`] is written on a command line.
enum FlagSpelling {
    /// A prefix character and a letter (`-c`, `+c`).
    Short(char, char),
    /// A whole word (`--command`, `-Command`).
    Word(&'static str),
}

impl ShellFlag {
    const fn spelling(self) -> FlagSpelling {
        match self {
            Self::DashC => FlagSpelling::Short('-', 'c'),
            Self::PlusC => FlagSpelling::Short('+', 'c'),
            Self::DashCapitalC => FlagSpelling::Short('-', 'C'),
            Self::DashE => FlagSpelling::Short('-', 'e'),
            Self::LongCommand(name) => FlagSpelling::Word(name.as_str()),
            Self::PowerShell(parameter) => FlagSpelling::Word(parameter.as_str()),
        }
    }

    /// The prefix character and letter of a short flag, or `None` for a flag
    /// written as a whole word.
    ///
    /// This is the one place a short flag's spelling is written, so a flag
    /// cannot be reported with a prefix the user did not use.
    #[must_use]
    pub(crate) const fn short_spelling(self) -> Option<(char, char)> {
        match self.spelling() {
            FlagSpelling::Short(prefix, letter) => Some((prefix, letter)),
            FlagSpelling::Word(_) => None,
        }
    }
}

impl fmt::Display for ShellFlag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.spelling() {
            FlagSpelling::Short(prefix, letter) => write!(f, "{prefix}{letter}"),
            FlagSpelling::Word(word) => f.write_str(word),
        }
    }
}

spelled_enum! {
    /// The spelling of a long flag that gives an interpreter a program to run.
    LongInlineFlag {
        /// `--eval`.
        Eval => "--eval",
        /// `--print`.
        Print => "--print",
        /// `--import`, given a `data:` URL.
        Import => "--import",
        /// `--loader`, given a `data:` URL.
        Loader => "--loader",
        /// `--experimental-loader`, given a `data:` URL.
        ExperimentalLoader => "--experimental-loader",
        /// php's `--run`, the long form of `-r`.
        Run => "--run",
        /// php's `--process-begin`, the long form of `-B`.
        ProcessBegin => "--process-begin",
        /// php's `--process-code`, the long form of `-R`.
        ProcessCode => "--process-code",
        /// php's `--process-end`, the long form of `-E`.
        ProcessEnd => "--process-end",
        /// php's `--process-file`, the long form of `-F`.
        ProcessFile => "--process-file",
    }
}

/// The flag that gives an interpreter a program to run inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InlineFlag {
    /// A short flag letter (`-e`, `-c`), alone or inside a cluster.
    Short(char),
    /// A long flag such as `--eval`.
    Long(LongInlineFlag),
}

impl fmt::Display for InlineFlag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Short(letter) => write!(f, "-{letter}"),
            Self::Long(flag) => f.write_str(flag.as_str()),
        }
    }
}

/// A positional operand a wrapper takes before the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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

/// Why a shell or interpreter would read its program from standard input,
/// which carries the LSP frames mcpls writes to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StdinSource {
    /// No script, module or other program source is given.
    NoProgram,
    /// `-s`, alone or inside a cluster.
    DashS,
    /// The script operand is `-`.
    DashOperand,
    /// The script operand lies under `/dev/` or `/proc/`, where a path can
    /// name the process's standard input.
    DeviceOperand,
    /// The script operand has a `..` component, which can reach a device path.
    ParentTraversal,
    /// The script operand resolves, where the server starts, to a device,
    /// FIFO or socket, so it may be an alias of a device path.
    SpecialFile,
}

impl fmt::Display for StdinSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoProgram => "no script is given, so it reads its program from standard input",
            Self::DashS => "`-s` makes it read its program from standard input",
            Self::DashOperand => "a `-` operand makes it read its program from standard input",
            Self::DeviceOperand => {
                "its script is under /dev or /proc, where a path can name standard input"
            }
            Self::ParentTraversal => "its script path has a `..` component, which can reach /dev",
            Self::SpecialFile => {
                "its script is a device, FIFO or socket, which may be an alias of standard input"
            }
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
    /// A bare argument is read as its command string (Windows PowerShell).
    ImplicitCommandString,
    /// This flag gives an interpreter a program to run inline.
    InlineProgram(InlineFlag),
    /// An argument names an npm package to run (`npm:`).
    NpmSpecifier,
    /// It reads its program from standard input, which carries LSP frames.
    ProgramFromStdin(StdinSource),
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
            Self::ImplicitCommandString => f.write_str(
                "it reads a bare argument as a command string, which cannot be analyzed",
            ),
            Self::InlineProgram(flag) => write!(f, "`{flag}` gives it a program to run"),
            Self::NpmSpecifier => f.write_str("an `npm:` argument names a package to run"),
            Self::ProgramFromStdin(source) => write!(f, "{source}"),
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
    /// A relative script operand follows a directory change.
    RelativeOperandAfterChdir,
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
            Self::RelativeOperandAfterChdir => {
                f.write_str("it reads a relative script after changing directory")
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
        program: EchoedPath,
        /// What about it does.
        trigger: LaunchTrigger,
    },
    /// The launch cannot be analyzed, which untrusted mode treats as unsafe.
    Unanalyzable {
        /// The launcher's name.
        program: EchoedPath,
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
    /// The vetted path of a program that `env` starts contains `=`, which
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
    /// A resolved path is blank, so there is nothing to start.
    BlankResolvedPath {
        /// What the path belongs to.
        what: ResolvedItem,
    },
    /// A resolved path contains a NUL, which the operating system cannot pass
    /// in a path or environment variable.
    NulInResolvedPath {
        /// What the path belongs to.
        what: ResolvedItem,
    },
    /// No directory outside the workspace is available for the server to
    /// start in.
    NoSafeWorkingDirectory,
    /// The variables untrusted mode sets for the server do not fit in its
    /// `env` table.
    EnvTooLarge {
        /// Which limit the table would be over.
        error: EnvTooLarge,
    },
}

impl fmt::Display for UntrustedRefusal {
    #[expect(
        clippy::too_many_lines,
        reason = "one message per refusal, and the refusals are the table"
    )]
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
            Self::BlankResolvedPath { what } => write!(
                f,
                "its {what} resolved to a blank path, which cannot be started"
            ),
            Self::NulInResolvedPath { what } => {
                write!(
                    f,
                    "its {what} resolved to a path containing NUL, which cannot be passed on"
                )
            }
            Self::NoSafeWorkingDirectory => f.write_str(
                "no directory outside the workspace is available to start it in, which \
                 untrusted mode requires",
            ),
            Self::EnvTooLarge { error } => write!(
                f,
                "the variables untrusted mode sets do not fit in its environment: {error}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::util::{MAX_ECHOED_NAME_BYTES, MAX_ECHOED_PATH_BYTES};

    #[test]
    fn shell_flags_display_the_prefix_they_are_spelled_with() {
        assert_eq!(ShellFlag::PlusC.short_spelling(), Some(('+', 'c')));
        assert_eq!(ShellFlag::PlusC.to_string(), "+c");
        assert_eq!(ShellFlag::DashC.to_string(), "-c");
        assert_eq!(ShellFlag::DashCapitalC.short_spelling(), Some(('-', 'C')));
        let long = ShellFlag::LongCommand(LongCommandName::Command);
        assert_eq!(long.short_spelling(), None);
        assert_eq!(long.to_string(), "--command");
    }

    #[test]
    fn wrapped_program_refusals_name_the_program_its_launcher_starts() {
        let program = EchoedPath::new(Path::new("/tmp/out/a=b"));
        let text = UntrustedRefusal::WrappedProgramPathContainsEquals { program }.to_string();
        assert!(text.contains("'/tmp/out/a=b'"), "{text}");
        assert!(text.contains("the program '"), "{text}");
        assert!(!text.contains("its launcher '/tmp"), "{text}");
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
        assert!(name.as_str().len() < MAX_ECHOED_NAME_BYTES + 64);
    }

    #[test]
    fn test_untrusted_refusal_new_variants_name_their_cause() {
        let launcher = UntrustedRefusal::ProjectLauncher {
            command: ServerCommand::from_static("npx"),
            cause: LauncherRefusal::SelectsWorkspaceCode {
                program: EchoedPath::program("npx"),
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
}
