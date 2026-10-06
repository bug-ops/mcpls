//! Launchers that choose the server to run from files in the workspace.
//!
//! A package runner (`npx`), task runner (`make`) or toolchain wrapper
//! (`cargo run`) takes a command that lies outside the workspace and starts a
//! program the workspace selects: `./node_modules/.bin/<name>`, a `Makefile`
//! target, a `build.rs`. Untrusted-workspace mode cannot vet that program, so
//! it refuses these launches ([`analyze_launch`]).
//!
//! The rules are best-effort. They match the command's file stem and its
//! arguments, parse the options of a short list of exec wrappers (`nice`,
//! `timeout`, ...) and of `env`, unwrap `busybox`, and give up on what cannot
//! be analyzed (an option no table lists, a `PATH=` assignment, a shell or
//! interpreter given a command string). Wrappers whose grammar is large or
//! that change the root, the directory or the environment (`sudo`, `strace`,
//! `unshare`, `chroot`, ...) are refused outright. The lists are closed, not
//! exhaustive: a shell, interpreter or wrapper missing from them is admitted,
//! and the program it starts is not checked. The trusted configuration is the
//! boundary, not this list.

use std::path::Path;

use crate::config::CommandStem;
use crate::error::{
    EchoedArgument, InlineFlag, LaunchTrigger, LauncherRefusal, OperandKind, RunnerSubcommand,
    ShellFlag, UnanalyzableLaunch,
};

/// How a launcher's use selects workspace code.
#[derive(Debug, Clone, Copy)]
enum LaunchRule {
    /// Every use of the command does.
    Always,
    /// Only a use with one of these subcommands among its arguments does.
    Subcommands(&'static [RunnerSubcommand]),
}

/// Launchers that start an npm package by name.
///
/// The launcher refuses every runner in [`RUNNERS`]; the TypeScript pin only
/// needs the ones that can start an npm package, so it reads this subset. The
/// list is not complete: `corepack` and `bun x` also start packages and are
/// refused by the launcher without being listed here.
pub const NPM_PACKAGE_RUNNERS: &[&str] = &["npm", "npx", "bunx", "pnpx", "pnpm", "yarn"];

const RUNNERS: &[(&str, LaunchRule)] = &[
    ("uvx", LaunchRule::Always),
    ("corepack", LaunchRule::Always),
    ("make", LaunchRule::Always),
    ("just", LaunchRule::Always),
    ("task", LaunchRule::Always),
    ("rake", LaunchRule::Always),
    ("mvn", LaunchRule::Always),
    ("sbt", LaunchRule::Always),
    ("xargs", LaunchRule::Always),
    ("find", LaunchRule::Always),
    ("awk", LaunchRule::Always),
    ("script", LaunchRule::Always),
    ("su", LaunchRule::Always),
    ("flock", LaunchRule::Always),
    ("watch", LaunchRule::Always),
    ("gawk", LaunchRule::Always),
    ("mawk", LaunchRule::Always),
    ("nawk", LaunchRule::Always),
    ("sudo", LaunchRule::Always),
    ("sudo-rs", LaunchRule::Always),
    ("doas", LaunchRule::Always),
    ("run0", LaunchRule::Always),
    ("pkexec", LaunchRule::Always),
    ("runuser", LaunchRule::Always),
    ("setpriv", LaunchRule::Always),
    ("strace", LaunchRule::Always),
    ("unshare", LaunchRule::Always),
    ("chrt", LaunchRule::Always),
    ("taskset", LaunchRule::Always),
    ("ionice", LaunchRule::Always),
    ("chroot", LaunchRule::Always),
    ("nsenter", LaunchRule::Always),
    ("systemd-run", LaunchRule::Always),
    ("gosu", LaunchRule::Always),
    ("su-exec", LaunchRule::Always),
    ("chpst", LaunchRule::Always),
    ("setuidgid", LaunchRule::Always),
    ("envdir", LaunchRule::Always),
    ("runas", LaunchRule::Always),
    ("wsl", LaunchRule::Always),
    (
        "bun",
        LaunchRule::Subcommands(&[RunnerSubcommand::X, RunnerSubcommand::Run]),
    ),
    (
        "deno",
        LaunchRule::Subcommands(&[
            RunnerSubcommand::Run,
            RunnerSubcommand::X,
            RunnerSubcommand::Task,
            RunnerSubcommand::Eval,
            RunnerSubcommand::Repl,
        ]),
    ),
    ("cargo", LaunchRule::Subcommands(&[RunnerSubcommand::Run])),
    (
        "go",
        LaunchRule::Subcommands(&[RunnerSubcommand::Run, RunnerSubcommand::Tool]),
    ),
    (
        "uv",
        LaunchRule::Subcommands(&[RunnerSubcommand::Run, RunnerSubcommand::Tool]),
    ),
    ("pipx", LaunchRule::Subcommands(&[RunnerSubcommand::Run])),
    ("poetry", LaunchRule::Subcommands(&[RunnerSubcommand::Run])),
    ("pdm", LaunchRule::Subcommands(&[RunnerSubcommand::Run])),
    ("hatch", LaunchRule::Subcommands(&[RunnerSubcommand::Run])),
    ("bundle", LaunchRule::Subcommands(&[RunnerSubcommand::Exec])),
    ("dotnet", LaunchRule::Subcommands(&[RunnerSubcommand::Tool])),
];

const SHELLS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "csh",
    "tcsh",
    "cmd",
    "powershell",
    "pwsh",
    "ash",
    "hush",
    "mksh",
    "oksh",
    "yash",
    "posh",
    "elvish",
    "nu",
    "xonsh",
];

/// Whether `text` has the shape of the operand `kind`.
fn operand_accepts(kind: OperandKind, text: &str) -> bool {
    match kind {
        OperandKind::Duration => is_duration(text),
    }
}

/// The options of a program that starts the command given in its own
/// arguments, so the command can be told apart from the options.
///
/// A letter or name is listed only when every implementation that knows it
/// (GNU, BSD, busybox) gives it the same arity, and only when that arity is
/// none or one mandatory value: an option with an optional argument would
/// swallow the command and leave the real one unchecked, so it is never
/// listed. A letter one implementation does not know is safe to list, because
/// that implementation fails before it starts anything. An attached `--x=v`
/// on a flag-only option is refused. Everything unlisted (an unknown option, a
/// GNU abbreviation of a long option, the legacy `nice -5`) is refused as
/// unanalyzable. The first non-option after the operand is the command, as
/// with GNU `+` getopt semantics; `--` ends the options.
struct ExecWrapper {
    names: &'static [&'static str],
    short_flags: &'static [char],
    short_values: &'static [char],
    long_flags: &'static [&'static str],
    long_values: &'static [&'static str],
    /// Single-dash words that are whole options (`arch -arm64`).
    word_flags: &'static [&'static str],
    /// Single-dash words followed by a value (`arch -arch`).
    word_values: &'static [&'static str],
    operand: Option<OperandKind>,
}

impl ExecWrapper {
    const fn named(names: &'static [&'static str]) -> Self {
        Self {
            names,
            short_flags: &[],
            short_values: &[],
            long_flags: &[],
            long_values: &[],
            word_flags: &[],
            word_values: &[],
            operand: None,
        }
    }

    /// The index in `args` of the command the wrapper starts.
    fn command_index(&self, args: &[String]) -> Result<usize, UnanalyzableLaunch> {
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            if arg == "--" {
                index = index.saturating_add(1);
                break;
            }
            match self.option_width(args, index)? {
                Some(width) => index = index.saturating_add(width),
                None => break,
            }
        }
        if let Some(operand) = self.operand {
            if !args
                .get(index)
                .is_some_and(|text| operand_accepts(operand, text))
            {
                return Err(UnanalyzableLaunch::MalformedOperand(operand));
            }
            index = index.saturating_add(1);
        }
        args.get(index)
            .map(|_| index)
            .ok_or(UnanalyzableLaunch::MissingCommand)
    }

    /// How many arguments the option at `args[index]` takes, or `None` when
    /// that argument is not an option.
    fn option_width(
        &self,
        args: &[String],
        index: usize,
    ) -> Result<Option<usize>, UnanalyzableLaunch> {
        let Some(arg) = args.get(index) else {
            return Ok(None);
        };
        let value_follows = || {
            if args.get(index.saturating_add(1)).is_some() {
                Ok(Some(2))
            } else {
                Err(UnanalyzableLaunch::MissingValue(EchoedArgument::name(arg)))
            }
        };
        if arg.starts_with("--") {
            let (name, attached) = arg
                .split_once('=')
                .map_or((arg.as_str(), false), |(name, _)| (name, true));
            return if self.long_values.contains(&name) {
                if attached {
                    Ok(Some(1))
                } else {
                    value_follows()
                }
            } else if !attached && self.long_flags.contains(&name) {
                Ok(Some(1))
            } else {
                Err(UnanalyzableLaunch::UnknownOption(EchoedArgument::name(arg)))
            };
        }
        let Some(cluster) = arg.strip_prefix('-').filter(|cluster| !cluster.is_empty()) else {
            return Ok(None);
        };
        if self.word_flags.contains(&arg.as_str()) {
            return Ok(Some(1));
        }
        if self.word_values.contains(&arg.as_str()) {
            return value_follows();
        }
        for (offset, letter) in cluster.char_indices() {
            if self.short_flags.contains(&letter) {
                continue;
            }
            if self.short_values.contains(&letter) {
                let glued = offset.saturating_add(letter.len_utf8()) < cluster.len();
                return if glued { Ok(Some(1)) } else { value_follows() };
            }
            return Err(UnanalyzableLaunch::UnknownOption(EchoedArgument::name(
                &format!("-{letter}"),
            )));
        }
        Ok(Some(1))
    }
}

/// Programs that start the command given in their own arguments, with options
/// of their own in between, and whose option grammar is small and stable.
const EXEC_WRAPPERS: &[ExecWrapper] = &[
    ExecWrapper {
        short_flags: &['a', 'p', 'v', 'q', 'l'],
        short_values: &['f', 'o'],
        long_flags: &["--append", "--portability", "--verbose", "--quiet"],
        long_values: &["--format", "--output"],
        ..ExecWrapper::named(&["time", "gtime"])
    },
    ExecWrapper {
        short_values: &['n'],
        long_values: &["--adjustment"],
        ..ExecWrapper::named(&["nice", "gnice"])
    },
    ExecWrapper::named(&["nohup", "gnohup"]),
    ExecWrapper {
        short_flags: &['f', 'p', 'v'],
        short_values: &['k', 's'],
        long_flags: &["--foreground", "--preserve-status", "--verbose"],
        long_values: &["--kill-after", "--signal"],
        operand: Some(OperandKind::Duration),
        ..ExecWrapper::named(&["timeout", "gtimeout"])
    },
    ExecWrapper {
        short_flags: &['c', 'f', 'w'],
        long_flags: &["--ctty", "--fork", "--wait"],
        ..ExecWrapper::named(&["setsid"])
    },
    ExecWrapper {
        short_values: &['i', 'o', 'e'],
        long_values: &["--input", "--output", "--error"],
        ..ExecWrapper::named(&["stdbuf", "gstdbuf"])
    },
    ExecWrapper {
        short_flags: &['d', 'i', 'm', 's', 'u'],
        short_values: &['t', 'w'],
        ..ExecWrapper::named(&["caffeinate"])
    },
    ExecWrapper {
        word_flags: &["-32", "-64", "-x86_64", "-arm64", "-arm64e", "-i386"],
        word_values: &["-arch"],
        ..ExecWrapper::named(&["arch"])
    },
];

/// Interpreters that run a program given on the command line, with the short
/// flag letters and long flags that introduce it. A match is by stem prefix so
/// `python3.12` counts as `python`.
const INLINE_EVAL: &[InlineEval] = &[
    InlineEval::new("node", &['e', 'p'], &['r', 'C'], LONG_EVAL_PRINT),
    InlineEval::new("bun", &['e', 'p'], &['r', 'c'], LONG_EVAL_PRINT),
    InlineEval::new("lua", &['e'], &['l'], &[]),
    InlineEval::new("rscript", &['e'], &[], &[]),
    InlineEval::new(
        "julia",
        &['e', 'E'],
        &['L', 'J', 'C', 'O', 't', 'p', 'H'],
        LONG_EVAL_PRINT,
    ),
    InlineEval::new("osascript", &['e'], &['l', 's'], &[]),
    InlineEval::new("nodejs", &['e', 'p'], &['r', 'C'], LONG_EVAL_PRINT),
    InlineEval::new("python", &['c'], &['m', 'W', 'X', 'Q'], &[]),
    InlineEval::new(
        "perl",
        &['e', 'E'],
        &['I', 'M', 'm', 'x', 'i', 'F', 'C', 'd', 'D'],
        &[],
    ),
    InlineEval::new(
        "ruby",
        &['e'],
        &['r', 'I', 'C', 'E', 'K', 'x', 'F', 'T'],
        &[],
    ),
    InlineEval::new("php", &['r'], &['d', 'c', 'f', 'z'], &[]),
];

const LONG_EVAL_PRINT: &[InlineFlag] = &[InlineFlag::Eval, InlineFlag::Print];

/// How an interpreter is given a program on its command line.
struct InlineEval {
    /// The interpreter's name; a stem prefix match, so `python3.12` counts.
    name: &'static str,
    /// Short flag letters that introduce the program.
    program_letters: &'static [char],
    /// Short flag letters that take another value (a module, a path): what
    /// follows one in the same argument is that value, not more flags.
    value_letters: &'static [char],
    /// Long flags that introduce the program.
    long: &'static [InlineFlag],
}

impl InlineEval {
    /// The flag by which `arg` gives this interpreter an inline program.
    fn flag_in(&self, arg: &str) -> Option<InlineFlag> {
        if let Some(letter) = has_short_flag(arg, self.program_letters, self.value_letters) {
            return Some(InlineFlag::Short(letter));
        }
        let name = arg.split_once('=').map_or(arg, |(name, _)| name);
        self.long
            .iter()
            .copied()
            .find(|flag| flag.long_name() == Some(name))
    }

    const fn new(
        name: &'static str,
        program_letters: &'static [char],
        value_letters: &'static [char],
        long: &'static [InlineFlag],
    ) -> Self {
        Self {
            name,
            program_letters,
            value_letters,
            long,
        }
    }
}

/// Prefix of an argument that names an npm package for a package runner.
pub const NPM_SPECIFIER_PREFIX: &str = "npm:";

/// Most wrappers followed before the launch is treated as unanalyzable.
const MAX_WRAPPER_DEPTH: usize = 8;

/// `env` and its Homebrew GNU spelling.
const ENV_NAMES: &[&str] = &["env", "genv"];

/// Binaries that run the applet named by their first argument.
const MULTI_CALL_BINARIES: &[&str] = &["busybox", "toybox", "coreutils"];

/// `env` long options that take no value.
const ENV_LONG_FLAGS: &[&str] = &["--ignore-environment", "--null", "--debug"];

/// `env` long options that take a value, attached with `=` or separate.
const ENV_LONG_OPTIONS_WITH_VALUE: &[&str] = &["--unset", "--chdir"];

/// `env` short flags that take no value and may be clustered.
const ENV_SHORT_FLAGS: &[char] = &['i', '0', 'v'];

/// The indices into a launch's arguments of every program an exec wrapper or
/// `env` starts, outermost first. `busybox` applets are not programs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WrappedPrograms(Vec<usize>);

impl WrappedPrograms {
    /// The indices into the analyzed `args`, outermost program first.
    #[must_use]
    pub fn indices(&self) -> &[usize] {
        &self.0
    }
}

/// Checks that starting `command` with `args` does not let the workspace
/// choose the program that runs, and finds the programs the wrappers in it
/// start.
///
/// # Errors
///
/// The [`LauncherRefusal`] naming what selects workspace code or what cannot
/// be analyzed.
pub fn analyze_launch(command: &str, args: &[String]) -> Result<WrappedPrograms, LauncherRefusal> {
    if args.iter().any(|arg| arg.starts_with(NPM_SPECIFIER_PREFIX)) {
        return Err(selects(command, LaunchTrigger::NpmSpecifier));
    }
    let mut analysis = Analysis::default();
    analysis.command(command, args, 0, 0)?;
    Ok(WrappedPrograms(analysis.programs))
}

fn selects(command: &str, trigger: LaunchTrigger) -> LauncherRefusal {
    LauncherRefusal::SelectsWorkspaceCode {
        program: EchoedArgument::name(command),
        trigger,
    }
}

fn unanalyzable(command: &str, reason: UnanalyzableLaunch) -> LauncherRefusal {
    LauncherRefusal::Unanalyzable {
        program: EchoedArgument::name(command),
        reason,
    }
}

const fn deepen(depth: usize) -> Result<(), LauncherRefusal> {
    if depth >= MAX_WRAPPER_DEPTH {
        Err(LauncherRefusal::TooDeep)
    } else {
        Ok(())
    }
}

#[derive(Default)]
struct Analysis {
    programs: Vec<usize>,
}

impl Analysis {
    /// Analyzes `command` run with `args`, where `args[0]` is argument `base`
    /// of the launch.
    fn command(
        &mut self,
        command: &str,
        args: &[String],
        base: usize,
        depth: usize,
    ) -> Result<(), LauncherRefusal> {
        let stem = CommandStem::of(command);
        if stem.is_any(ENV_NAMES) {
            deepen(depth)?;
            return self.env(command, args, base, depth);
        }
        if let Some(wrapper) = EXEC_WRAPPERS
            .iter()
            .find(|wrapper| stem.is_any(wrapper.names))
        {
            deepen(depth)?;
            if args.is_empty() {
                return Ok(());
            }
            let index = wrapper
                .command_index(args)
                .map_err(|reason| unanalyzable(command, reason))?;
            return self.start(command, args, index, base, depth);
        }
        if stem.is_any(MULTI_CALL_BINARIES) {
            deepen(depth)?;
            let Some((applet, rest)) = args.split_first() else {
                return Err(unanalyzable(command, UnanalyzableLaunch::MissingCommand));
            };
            return self.command(
                applet,
                rest,
                base.saturating_add(1),
                depth.saturating_add(1),
            );
        }
        refuse_workspace_code(command, &stem, args)
    }

    /// Records `args[index]` as a program the wrapper `wrapper` starts and
    /// analyzes it.
    fn start(
        &mut self,
        wrapper: &str,
        args: &[String],
        index: usize,
        base: usize,
        depth: usize,
    ) -> Result<(), LauncherRefusal> {
        let Some(program) = args.get(index) else {
            return Err(unanalyzable(wrapper, UnanalyzableLaunch::MissingCommand));
        };
        if program.is_empty() {
            return Err(unanalyzable(wrapper, UnanalyzableLaunch::BlankProgram));
        }
        self.programs.push(base.saturating_add(index));
        let rest = args.get(index.saturating_add(1)..).unwrap_or_default();
        self.command(
            program,
            rest,
            base.saturating_add(index).saturating_add(1),
            depth.saturating_add(1),
        )
    }

    /// Analyzes the command an `env` invocation starts. Anything that could
    /// change which program runs or where it is looked up is refused as
    /// unanalyzable: `-S`, `-P`, an option this list does not know, a
    /// `-u`/`-C` value glued into a cluster, a `PATH=` assignment (it bypasses
    /// the sanitized `PATH`), a relative program after `-C`/`--chdir`, or a
    /// chain of more than [`MAX_WRAPPER_DEPTH`] wrappers.
    fn env(
        &mut self,
        command: &str,
        args: &[String],
        base: usize,
        depth: usize,
    ) -> Result<(), LauncherRefusal> {
        let refuse = |reason| unanalyzable(command, reason);
        let value_follows = |index: usize, arg: &str| {
            if args.get(index.saturating_add(1)).is_some() {
                Ok(())
            } else {
                Err(refuse(UnanalyzableLaunch::MissingValue(
                    EchoedArgument::name(arg),
                )))
            }
        };
        let mut changes_directory = false;
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            if arg == "--" {
                index = index.saturating_add(1);
                break;
            }
            if arg == "-" {
                index = index.saturating_add(1);
                if let Some(next) = args.get(index).filter(|next| next.starts_with('-')) {
                    let letter = next.chars().nth(1).unwrap_or('-');
                    return Err(refuse(UnanalyzableLaunch::UnknownOption(
                        EchoedArgument::name(&format!("-{letter}")),
                    )));
                }
                break;
            }
            if let Some(long) = arg.strip_prefix("--") {
                let name = format!("--{}", long.split('=').next().unwrap_or_default());
                if ENV_LONG_OPTIONS_WITH_VALUE.contains(&name.as_str()) {
                    changes_directory |= name == "--chdir";
                    if !long.contains('=') {
                        value_follows(index, arg)?;
                        index = index.saturating_add(1);
                    }
                } else if name == "--split-string" {
                    return Err(refuse(UnanalyzableLaunch::SplitString));
                } else if !ENV_LONG_FLAGS.contains(&name.as_str()) || long.contains('=') {
                    return Err(refuse(UnanalyzableLaunch::UnknownOption(
                        EchoedArgument::name(arg),
                    )));
                }
            } else if let Some(cluster) = arg.strip_prefix('-') {
                match cluster {
                    "u" | "C" => {
                        changes_directory |= cluster == "C";
                        value_follows(index, arg)?;
                        index = index.saturating_add(1);
                    }
                    _ => {
                        if let Some(letter) = cluster
                            .chars()
                            .find(|letter| !ENV_SHORT_FLAGS.contains(letter))
                        {
                            return Err(refuse(if cluster.contains('S') {
                                UnanalyzableLaunch::SplitString
                            } else {
                                UnanalyzableLaunch::UnknownOption(EchoedArgument::name(&format!(
                                    "-{letter}"
                                )))
                            }));
                        }
                    }
                }
            } else {
                break;
            }
            index = index.saturating_add(1);
        }
        while let Some((name, _)) = args.get(index).and_then(|arg| arg.split_once('=')) {
            if name.eq_ignore_ascii_case("PATH") {
                return Err(refuse(UnanalyzableLaunch::PathAssignment));
            }
            index = index.saturating_add(1);
        }
        let Some(program) = args.get(index) else {
            return Ok(());
        };
        if changes_directory && is_relative_path(program) {
            return Err(refuse(UnanalyzableLaunch::RelativeProgramAfterChdir));
        }
        self.start(command, args, index, base, depth)
    }
}

/// Whether `program` is looked up relative to the working directory: a path of
/// several components that is not absolute.
fn is_relative_path(program: &str) -> bool {
    let path = Path::new(program);
    !path.is_absolute() && path.components().count() > 1
}

/// Refuses a command that is not a wrapper when its name and arguments make it
/// run workspace code: a shell given a command string, an interpreter given an
/// inline program, a package or task runner.
fn refuse_workspace_code(
    command: &str,
    stem: &CommandStem,
    args: &[String],
) -> Result<(), LauncherRefusal> {
    if SHELLS
        .iter()
        .any(|shell| is_interpreter(stem.as_str(), shell))
        && let Some(flag) = args.iter().find_map(|arg| command_flag(stem.as_str(), arg))
    {
        return Err(selects(command, LaunchTrigger::CommandString(flag)));
    }
    if let Some(flag) = INLINE_EVAL
        .iter()
        .filter(|eval| is_interpreter(stem.as_str(), eval.name))
        .find_map(|eval| args.iter().find_map(|arg| eval.flag_in(arg)))
    {
        return Err(selects(command, LaunchTrigger::InlineProgram(flag)));
    }
    if stem.is_any(NPM_PACKAGE_RUNNERS) {
        return Err(selects(command, LaunchTrigger::Always));
    }
    match RUNNERS.iter().find(|(name, _)| stem.is(name)) {
        Some((_, LaunchRule::Always)) => Err(selects(command, LaunchTrigger::Always)),
        Some((_, LaunchRule::Subcommands(subcommands))) => subcommands
            .iter()
            .find(|subcommand| args.iter().any(|arg| arg == subcommand.as_str()))
            .map_or(Ok(()), |subcommand| {
                Err(selects(command, LaunchTrigger::Subcommand(*subcommand)))
            }),
        None => Ok(()),
    }
}

/// The flag by which a shell argument introduces a command string, which
/// cannot be analyzed: `-c` or a cluster holding it (`-lc`) for POSIX shells
/// (case-exact: `-C` is another option), `/c`, `/k` or `/r` for `cmd` (also
/// glued to the command, `/ccmd`), and the `-Command`, `-CommandWithArgs` and
/// `-EncodedCommand` parameters of PowerShell, which accepts any prefix of them
/// and the alias `-ec`. `cmd` and PowerShell compare case-insensitively.
fn command_flag(shell: &str, arg: &str) -> Option<ShellFlag> {
    let lowered = arg.to_ascii_lowercase();
    match shell {
        "cmd" => ["/c", "/k", "/r"]
            .iter()
            .any(|flag| lowered.starts_with(flag))
            .then_some(ShellFlag::SlashC),
        "powershell" | "pwsh" => lowered
            .strip_prefix(['-', '/'])
            .is_some_and(|name| {
                name == "ec"
                    || (!name.is_empty()
                        && ["command", "commandwithargs", "encodedcommand"]
                            .iter()
                            .any(|parameter| parameter.starts_with(name)))
            })
            .then_some(ShellFlag::PowerShellCommand),
        _ => {
            if long_flag_matches(arg, &["--command", "--commands"]) {
                Some(ShellFlag::LongCommand)
            } else {
                has_short_flag(arg, &['c'], &['o']).map(|_| ShellFlag::DashC)
            }
        }
    }
}

/// The flag letter of `flags` that a single-dash argument reaches: whatever
/// follows such a letter is its value (`-c'code'`, `-ecode`), so the argument
/// carries inline code. Letters are read in order, digits are skipped
/// (`-0777e`, `-l0e`), and reading stops at the first letter in `value_letters`,
/// since the rest of the argument is then that option's value (`-mcoverage`,
/// `-rbundler/setup`).
fn has_short_flag(arg: &str, flags: &[char], value_letters: &[char]) -> Option<char> {
    let cluster = arg
        .strip_prefix('-')
        .filter(|cluster| !cluster.starts_with('-'))?;
    cluster
        .chars()
        .take_while(|letter| letter.is_alphanumeric())
        .find(|letter| flags.contains(letter) || value_letters.contains(letter))
        .filter(|letter| flags.contains(letter))
}

/// Whether `arg` is one of the long `flags`, bare or with an `=value`.
fn long_flag_matches(arg: &str, flags: &[&str]) -> bool {
    let name = arg.split_once('=').map_or(arg, |(name, _)| name);
    flags.contains(&name)
}

/// Whether `stem` is the interpreter `name`, optionally followed by a version
/// (`python3.12`, `node18`), but not another tool sharing the prefix
/// (`nodemon`, `phpunit`).
fn is_interpreter(stem: &str, name: &str) -> bool {
    stem.strip_prefix(name)
        .is_some_and(|version| version.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

/// Whether `text` is `^[0-9]+(\.[0-9]+)?[smhd]?$`, a `timeout` duration.
fn is_duration(text: &str) -> bool {
    let number = text.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(text);
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    match number.split_once('.') {
        Some((whole, fraction)) => digits(whole) && digits(fraction),
        None => digits(number),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::MAX_SYMBOL_NAME_BYTES;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    fn analyze(command: &str, args: &[&str]) -> Result<Vec<usize>, LauncherRefusal> {
        analyze_launch(command, &strings(args)).map(|programs| programs.indices().to_vec())
    }

    fn launches(command: &str, args: &[&str]) -> bool {
        analyze(command, args).is_err()
    }

    fn unanalyzable_reason(command: &str, args: &[&str]) -> UnanalyzableLaunch {
        match analyze(command, args) {
            Err(LauncherRefusal::Unanalyzable { reason, .. }) => reason,
            other => panic!("expected an unanalyzable launch, got {other:?}"),
        }
    }

    #[test]
    fn always_rules_refuse_every_use() {
        let always = RUNNERS
            .iter()
            .filter(|(_, rule)| matches!(rule, LaunchRule::Always))
            .map(|(name, _)| *name)
            .chain(NPM_PACKAGE_RUNNERS.iter().copied());
        for runner in always {
            assert!(launches(runner, &[]), "{runner}");
            assert!(
                launches(&format!("/usr/local/bin/{runner}"), &["x"]),
                "{runner}"
            );
        }
    }

    #[test]
    fn subcommand_rules_refuse_only_the_listed_uses() {
        for (command, rule) in RUNNERS {
            let LaunchRule::Subcommands(subcommands) = rule else {
                continue;
            };
            for subcommand in *subcommands {
                assert_eq!(
                    analyze(command, &[subcommand.as_str(), "server"]).unwrap_err(),
                    selects(command, LaunchTrigger::Subcommand(*subcommand)),
                    "{command} {subcommand}"
                );
            }
            assert!(!launches(command, &["--version"]), "{command}");
        }
    }

    #[test]
    fn perl_clusters_with_digits_still_carry_an_inline_program() {
        for flags in ["-0777e", "-l0e", "-0e", "-l0777e", "-nle", "-pe", "-ne"] {
            assert!(
                matches!(
                    analyze("perl", &[flags, "code"]),
                    Err(LauncherRefusal::SelectsWorkspaceCode {
                        trigger: LaunchTrigger::InlineProgram(InlineFlag::Short('e' | 'p')),
                        ..
                    })
                ),
                "{flags}"
            );
        }
        assert!(launches("perl", &["-Mstrict", "-e", "code"]));
        assert!(launches("perl", &["-0", "-e", "code"]));
        assert!(!launches("perl", &["-Mstrict", "script.pl"]));
        assert!(!launches("perl", &["-0777", "script.pl"]));
        assert!(!launches("perl", &["-l0", "script.pl"]));
    }

    #[test]
    fn the_trigger_names_the_matched_flag() {
        assert_eq!(
            analyze("node", &["--print", "1"]).unwrap_err(),
            selects("node", LaunchTrigger::InlineProgram(InlineFlag::Print))
        );
        assert_eq!(
            analyze("bun", &["--eval=1"]).unwrap_err(),
            selects("bun", LaunchTrigger::InlineProgram(InlineFlag::Eval))
        );
        assert_eq!(
            analyze("ruby", &["-eputs 1"]).unwrap_err(),
            selects("ruby", LaunchTrigger::InlineProgram(InlineFlag::Short('e')))
        );
        assert_eq!(
            analyze("fish", &["--command=x"]).unwrap_err(),
            selects("fish", LaunchTrigger::CommandString(ShellFlag::LongCommand))
        );
        assert_eq!(
            analyze("bash", &["-lc", "x"]).unwrap_err(),
            selects("bash", LaunchTrigger::CommandString(ShellFlag::DashC))
        );
    }

    #[test]
    fn posix_shell_flags_are_case_exact_but_cmd_and_powershell_are_not() {
        assert!(!launches("bash", &["-C", "script.sh"]));
        assert!(!launches("sh", &["-C", "script.sh"]));
        assert!(launches("cmd", &["/C", "x"]));
        assert!(launches("pwsh", &["-COMMAND", "x"]));
    }

    #[test]
    fn deno_lsp_and_servers_started_directly_are_allowed() {
        assert!(!launches("deno", &["lsp"]));
        assert!(!launches("rust-analyzer", &[]));
        assert!(!launches("/usr/bin/gopls", &["serve"]));
        assert!(!launches("typescript-language-server", &["--stdio"]));
    }

    #[test]
    fn an_npm_specifier_argument_refuses_any_command() {
        assert_eq!(
            analyze("deno", &["lsp", "npm:typescript-language-server"]).unwrap_err(),
            selects("deno", LaunchTrigger::NpmSpecifier)
        );
        assert!(launches("node", &["npm:server"]));
    }

    #[test]
    fn windows_extensions_and_case_are_ignored() {
        assert!(launches("NPX.CMD", &[]));
        assert!(launches("yarn.exe", &[]));
    }

    #[test]
    fn env_is_unwrapped_to_the_command_it_starts() {
        assert!(launches("env", &["npx", "server"]));
        assert!(launches("env", &["-i", "-u", "BAR", "make"]));
        assert!(launches("/usr/bin/env", &["--", "cargo", "run"]));
        assert!(!launches("env", &["FOO=1", "rust-analyzer"]));
        assert!(!launches("env", &["-i", "cargo", "--version"]));
        assert!(!launches("env", &[]));
    }

    #[test]
    fn env_split_string_and_deep_chains_are_unanalyzable() {
        assert_eq!(
            unanalyzable_reason("env", &["-S", "rust-analyzer --flag"]),
            UnanalyzableLaunch::SplitString
        );
        assert_eq!(
            unanalyzable_reason("env", &["--split-string=rust-analyzer"]),
            UnanalyzableLaunch::SplitString
        );
        let mut chain: Vec<&str> = vec!["env"; MAX_WRAPPER_DEPTH + 1];
        chain.push("rust-analyzer");
        assert_eq!(
            analyze("env", &chain).unwrap_err(),
            LauncherRefusal::TooDeep
        );
    }

    #[test]
    fn env_options_that_change_what_runs_are_unanalyzable() {
        for args in [
            &["-iS", "npx srv"][..],
            &["-iC", "/dir", "rust-analyzer"],
            &["-vu", "X", "rust-analyzer"],
            &["-uX", "rust-analyzer"],
            &["-P", "/ws/bin", "rust-analyzer"],
            &["-Pi", "rust-analyzer"],
            &["--chdir", "/d", "--bogus", "rust-analyzer"],
            &["--ignore-environment=x", "rust-analyzer"],
            &["-z", "rust-analyzer"],
            &["PATH=/ws/bin", "rust-analyzer"],
            &["path=/ws/bin", "rust-analyzer"],
            &["FOO=1", "Path=/ws/bin", "rust-analyzer"],
        ] {
            assert!(launches("env", args), "{args:?}");
        }
        assert_eq!(
            unanalyzable_reason("env", &["PATH=/ws/bin", "srv"]),
            UnanalyzableLaunch::PathAssignment
        );
    }

    #[test]
    fn env_options_that_only_edit_the_environment_are_unwrapped() {
        for args in [
            &["-i", "rust-analyzer"][..],
            &["-0", "-v", "rust-analyzer"],
            &["-iv", "rust-analyzer"],
            &["-u", "FOO", "rust-analyzer"],
            &["-C", "/dir", "rust-analyzer"],
            &["--ignore-environment", "--null", "--debug", "rust-analyzer"],
            &["--unset", "FOO", "--chdir=/d", "rust-analyzer"],
            &["--unset=FOO", "rust-analyzer"],
            &["FOO=1", "PATHEXT=.exe", "rust-analyzer"],
        ] {
            assert!(!launches("env", args), "{args:?}");
        }
    }

    #[test]
    fn env_stops_option_parsing_at_a_lone_dash_and_the_first_assignment() {
        for args in [
            &["-C", "d", "-", "-u", "srv"][..],
            &["-", "-u", "X", "srv"],
            &["-", "-i", "srv"],
        ] {
            assert!(
                matches!(
                    unanalyzable_reason("env", args),
                    UnanalyzableLaunch::UnknownOption(_)
                ),
                "{args:?}"
            );
        }
        assert_eq!(analyze("env", &["-", "srv"]), Ok(vec![1]));
        assert_eq!(analyze("env", &["-i", "-", "A=1", "srv"]), Ok(vec![3]));
        assert_eq!(analyze("env", &["FOO=1", "-i", "srv"]), Ok(vec![1]));
    }

    #[test]
    fn env_after_a_lone_dash_echoes_only_the_offending_letter() {
        let Err(LauncherRefusal::Unanalyzable {
            reason: UnanalyzableLaunch::UnknownOption(option),
            ..
        }) = analyze("env", &["-", "-uSECRETTOKEN", "srv"])
        else {
            panic!("expected an unknown option");
        };
        assert_eq!(option.as_str(), "-u");
    }

    #[test]
    fn env_applies_assignments_after_the_end_of_options_marker() {
        assert_eq!(analyze("env", &["--", "FOO=1", "srv"]), Ok(vec![2]));
        assert_eq!(
            unanalyzable_reason("env", &["--", "PATH=/ws/bin", "srv"]),
            UnanalyzableLaunch::PathAssignment
        );
        assert_eq!(
            unanalyzable_reason("env", &["-i", "A=1", "path=/ws/bin", "srv"]),
            UnanalyzableLaunch::PathAssignment
        );
    }

    #[test]
    fn gnu_aliases_and_multi_call_binaries_are_unwrapped() {
        for (command, args, index) in [
            ("genv", &["FOO=1", "srv"][..], 1),
            ("gtimeout", &["5", "srv"], 1),
            ("gnice", &["-n", "5", "srv"], 2),
            ("gnohup", &["srv"], 0),
            ("gstdbuf", &["-oL", "srv"], 1),
            ("gtime", &["-p", "srv"], 1),
        ] {
            assert_eq!(analyze(command, args), Ok(vec![index]), "{command}");
        }
        assert!(launches("genv", &["sh", "-c", "x"]));
        assert!(launches("gtimeout", &["5", "npx", "srv"]));
        assert!(launches("toybox", &["sh", "-c", "x"]));
        assert!(launches("coreutils", &["env", "npx", "srv"]));
        assert_eq!(analyze("coreutils", &["nice", "srv"]), Ok(vec![1]));
    }

    #[test]
    fn user_switching_wrappers_are_refused_but_ast_grep_is_not() {
        for wrapper in [
            "gosu",
            "su-exec",
            "chpst",
            "setuidgid",
            "envdir",
            "runas",
            "wsl",
        ] {
            assert!(launches(wrapper, &["srv"]), "{wrapper}");
        }
        assert!(!launches("sg", &["lsp"]));
    }

    #[test]
    fn env_refuses_a_missing_option_value() {
        assert_eq!(
            unanalyzable_reason("env", &["-i", "-u"]),
            UnanalyzableLaunch::MissingValue(EchoedArgument::name("-u"))
        );
        assert!(matches!(
            unanalyzable_reason("env", &["--chdir"]),
            UnanalyzableLaunch::MissingValue(_)
        ));
    }

    #[test]
    fn env_refuses_a_relative_program_after_a_directory_change() {
        for args in [
            &["-C", "/d", "./srv"][..],
            &["--chdir=/d", "bin/srv"],
            &["--chdir", "/d", "--", "bin/srv"],
        ] {
            assert_eq!(
                unanalyzable_reason("env", args),
                UnanalyzableLaunch::RelativeProgramAfterChdir,
                "{args:?}"
            );
        }
        assert!(!launches("env", &["-C", "/d", "rust-analyzer"]));
        assert!(!launches("env", &["./srv"]));
    }

    #[test]
    fn a_blank_program_after_a_wrapper_is_refused() {
        assert_eq!(
            unanalyzable_reason("env", &["FOO=1", ""]),
            UnanalyzableLaunch::BlankProgram
        );
        assert_eq!(
            unanalyzable_reason("nice", &["-n", "5", ""]),
            UnanalyzableLaunch::BlankProgram
        );
    }

    #[test]
    fn cmd_command_flags_are_recognized_with_and_without_a_space() {
        for flag in ["/c", "/C", "/k", "/r", "/ccmd", "/Cserver"] {
            assert_eq!(
                analyze("cmd", &[flag, "server"]).unwrap_err(),
                selects("cmd", LaunchTrigger::CommandString(ShellFlag::SlashC)),
                "{flag}"
            );
        }
        assert!(!launches("cmd", &["/q", "server.bat"]));
    }

    #[test]
    fn powershell_command_parameters_are_refused_but_other_switches_are_not() {
        for flag in [
            "-c",
            "-Command",
            "-comm",
            "-CommandWithArgs",
            "-EncodedCommand",
            "-e",
            "-ec",
            "/command",
        ] {
            assert!(launches("pwsh", &[flag, "x"]), "{flag}");
            assert!(launches("powershell.exe", &[flag, "x"]), "{flag}");
        }
        for flag in ["-NonInteractive", "-NoProfile", "-NoLogo", "-Version"] {
            assert!(!launches("pwsh", &[flag, "server.ps1"]), "{flag}");
        }
    }

    #[test]
    fn tools_sharing_an_interpreter_prefix_are_not_interpreters() {
        assert!(!launches("nodemon", &["-e", "js"]));
        assert!(!launches("phpunit", &["-r"]));
        assert!(!launches("perltidy", &["-e"]));
        assert!(launches("python3.12", &["-c", "x"]));
        assert!(launches("node18", &["-e", "x"]));
    }

    #[test]
    fn fish_command_is_recognized_in_long_and_short_form() {
        assert!(launches("fish", &["--command", "server"]));
        assert!(launches("fish", &["-c", "server"]));
        assert!(!launches("fish", &["server.fish"]));
    }

    #[test]
    fn busybox_applets_are_unwrapped() {
        assert!(launches("busybox", &["sh", "-c", "rust-analyzer"]));
        assert!(launches("busybox", &["env", "npx", "server"]));
        assert_eq!(
            unanalyzable_reason("busybox", &[]),
            UnanalyzableLaunch::MissingCommand
        );
        assert_eq!(analyze("busybox", &["rust-analyzer"]), Ok(vec![]));
    }

    #[test]
    fn interpreters_given_an_inline_program_are_refused() {
        for (command, flag) in [
            ("node", "-e"),
            ("node", "--eval"),
            ("node", "-p"),
            ("node", "-pe"),
            ("python", "-c"),
            ("python3.12", "-c"),
            ("/usr/bin/perl", "-e"),
            ("ruby", "-e"),
            ("php", "-r"),
        ] {
            assert!(launches(command, &[flag, "code"]), "{command} {flag}");
            assert!(!launches(command, &["server.js"]), "{command}");
        }
        assert!(!launches("node", &["--version"]));
        assert_eq!(
            analyze("python3.12", &["-c", "x"]).unwrap_err(),
            selects(
                "python3.12",
                LaunchTrigger::InlineProgram(InlineFlag::Short('c'))
            )
        );
    }

    #[test]
    fn shells_with_a_command_string_are_unanalyzable() {
        assert!(launches("sh", &["-c", "rust-analyzer"]));
        assert!(launches("/bin/bash", &["-lc", "rust-analyzer"]));
        assert!(launches("cmd.exe", &["/C", "server"]));
        assert_eq!(
            analyze("powershell", &["-Command", "server"]).unwrap_err(),
            selects(
                "powershell",
                LaunchTrigger::CommandString(ShellFlag::PowerShellCommand)
            )
        );
        assert!(!launches("sh", &["server.sh"]));
        assert!(!launches("bash", &["--norc", "server.sh"]));
    }

    #[test]
    fn every_listed_shell_refuses_a_command_string() {
        for shell in SHELLS {
            let flag = if *shell == "cmd" { "/c" } else { "-c" };
            assert!(launches(shell, &[flag, "x"]), "{shell}");
            assert!(
                launches(&format!("/opt/bin/{shell}"), &[flag, "x"]),
                "{shell}"
            );
            assert!(!launches(shell, &["script"]), "{shell}");
        }
        assert!(launches("busybox", &["ash", "-c", "x"]));
        assert!(launches("busybox", &["hush", "-c", "x"]));
    }

    #[test]
    fn nu_refuses_both_command_spellings() {
        assert!(launches("nu", &["--commands", "x"]));
        assert!(launches("nu", &["--command", "x"]));
        assert!(launches("nu", &["-c", "x"]));
        assert!(!launches("nu", &["server.nu"]));
    }

    #[test]
    fn deno_eval_and_bun_inline_programs_are_refused() {
        assert!(launches("deno", &["eval", "code"]));
        for flag in ["-e", "-p", "--eval", "--print"] {
            assert!(launches("bun", &[flag, "code"]), "{flag}");
        }
        assert!(!launches("bun", &["server.ts"]));
        assert!(!launches("bunyan", &["-e"]));
    }

    #[test]
    fn other_inline_interpreters_are_refused() {
        for (command, flag) in [
            ("lua", "-e"),
            ("Rscript", "-e"),
            ("julia", "-e"),
            ("osascript", "-e"),
        ] {
            assert!(launches(command, &[flag, "code"]), "{command}");
            assert!(!launches(command, &["script"]), "{command}");
        }
    }

    #[test]
    fn programs_that_run_their_arguments_are_refused() {
        for command in ["xargs", "find", "awk", "gawk", "mawk", "nawk"] {
            assert!(launches(command, &["x"]), "{command}");
        }
    }

    #[test]
    fn wrappers_that_are_not_parsed_are_refused_outright() {
        for wrapper in [
            "sudo",
            "sudo-rs",
            "doas",
            "run0",
            "pkexec",
            "runuser",
            "setpriv",
            "strace",
            "unshare",
            "chrt",
            "taskset",
            "ionice",
            "chroot",
            "nsenter",
            "systemd-run",
            "su",
        ] {
            for args in [&["rust-analyzer"][..], &[]] {
                assert_eq!(
                    analyze(wrapper, args).unwrap_err(),
                    selects(wrapper, LaunchTrigger::Always),
                    "{wrapper} {args:?}"
                );
            }
        }
        assert!(launches("sudo", &["-u", "nobody", "gopls"]));
        assert!(launches("nice", &["sudo", "-u", "x", "srv"]));
    }

    #[test]
    fn parsed_wrappers_find_the_command_past_their_options() {
        for (command, args, index) in [
            ("nice", &["-n", "5", "gopls"][..], 2),
            ("nice", &["-n5", "gopls"], 1),
            ("nice", &["--adjustment=5", "gopls"], 1),
            ("nice", &["--adjustment", "5", "gopls"], 2),
            ("nice", &["gopls", "-logfile", "x"], 0),
            ("nice", &["--", "gopls"], 1),
            ("time", &["-p", "-o", "out", "srv"], 3),
            ("time", &["-pf", "%e", "srv"], 2),
            ("nohup", &["srv", "--stdio"], 0),
            ("timeout", &["5", "rust-analyzer", "make"], 1),
            ("timeout", &["-k", "1", "-s", "KILL", "1.5m", "srv"], 5),
            (
                "timeout",
                &["--kill-after=1", "--foreground", "5", "srv"],
                3,
            ),
            ("timeout", &["--", "5", "srv"], 2),
            ("setsid", &["-f", "srv"], 1),
            ("setsid", &["-cfw", "srv"], 1),
            ("stdbuf", &["-oL", "srv"], 1),
            ("stdbuf", &["-o", "L", "-e0", "srv"], 3),
            ("caffeinate", &["-i", "srv"], 1),
            ("caffeinate", &["-t", "60", "-dims", "srv"], 3),
            ("arch", &["-arm64", "srv"], 1),
            ("arch", &["-arch", "arm64", "srv"], 2),
        ] {
            assert_eq!(
                analyze(command, args),
                Ok(vec![index]),
                "{command} {args:?}"
            );
        }
    }

    #[test]
    fn what_follows_the_wrapped_command_belongs_to_it() {
        assert!(!launches("nice", &["jdtls", "-configuration", "/cfg/env"]));
        assert!(!launches("timeout", &["5", "srv", "-c", "x"]));
        assert!(!launches("nice", &["gopls", "make", "sudo"]));
        assert!(launches("nice", &["make"]));
        assert!(launches("time", &["-p", "sh", "-c", "x"]));
        assert!(launches("timeout", &["5", "npx", "srv"]));
    }

    #[test]
    fn parsed_wrappers_refuse_what_their_table_does_not_list() {
        for (command, args) in [
            ("nice", &["-5", "srv"][..]),
            ("nice", &["--adj", "5", "srv"]),
            ("nice", &["-x", "srv"]),
            ("nohup", &["--help"]),
            ("time", &["--verbose=1", "srv"]),
            ("timeout", &["--foreground=1", "5", "srv"]),
            ("setsid", &["--bogus", "srv"]),
            ("stdbuf", &["-x", "srv"]),
            ("arch", &["-e", "PATH=/ws/bin", "srv"]),
            ("arch", &["-arm64=x", "srv"]),
            ("caffeinate", &["-z", "srv"]),
        ] {
            assert!(
                matches!(
                    unanalyzable_reason(command, args),
                    UnanalyzableLaunch::UnknownOption(_)
                ),
                "{command} {args:?}"
            );
        }
    }

    #[test]
    fn parsed_wrappers_refuse_a_missing_value_or_command() {
        assert!(matches!(
            unanalyzable_reason("nice", &["-n"]),
            UnanalyzableLaunch::MissingValue(_)
        ));
        assert!(matches!(
            unanalyzable_reason("arch", &["-arch"]),
            UnanalyzableLaunch::MissingValue(_)
        ));
        for (command, args) in [
            ("nice", &["-n", "5"][..]),
            ("time", &["-p"]),
            ("nice", &["--"]),
            ("setsid", &["-f"]),
        ] {
            assert_eq!(
                unanalyzable_reason(command, args),
                UnanalyzableLaunch::MissingCommand,
                "{command}"
            );
        }
        assert_eq!(
            unanalyzable_reason("timeout", &["5"]),
            UnanalyzableLaunch::MissingCommand
        );
        assert_eq!(analyze("nice", &[]), Ok(vec![]));
    }

    #[test]
    fn timeout_checks_the_shape_of_its_duration() {
        for good in ["5", "0.5", "10s", "2m", "1.5h", "3d", "007"] {
            assert_eq!(analyze("timeout", &[good, "srv"]), Ok(vec![1]), "{good}");
        }
        for bad in [
            "srv", "5x", "1e3", ".5", "5.", "inf", "\u{665}", "1.2.3", "",
        ] {
            assert_eq!(
                unanalyzable_reason("timeout", &[bad, "srv"]),
                UnanalyzableLaunch::MalformedOperand(OperandKind::Duration),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn refusals_echo_only_bounded_names_never_values() {
        let long = format!("--{}=secret", "a".repeat(2 * MAX_SYMBOL_NAME_BYTES));
        let Err(LauncherRefusal::Unanalyzable {
            reason: UnanalyzableLaunch::UnknownOption(option),
            ..
        }) = analyze("nice", &[&long, "srv"])
        else {
            panic!("expected an unknown option");
        };
        assert!(!option.as_str().contains("secret"));
        assert!(option.as_str().len() <= MAX_SYMBOL_NAME_BYTES + 64);

        assert_eq!(EchoedArgument::name("API_TOKEN=abc").as_str(), "API_TOKEN");
        assert_eq!(EchoedArgument::name("srv --token abc").as_str(), "srv");
        assert_eq!(EchoedArgument::name("-x5").as_str(), "-x5");
        assert!(!EchoedArgument::name("a\nb").as_str().contains('\n'));
        let Err(LauncherRefusal::Unanalyzable { program, .. }) =
            analyze("env", &["PATH=/ws/bin", "srv"])
        else {
            panic!("expected a refusal");
        };
        assert_eq!(program.as_str(), "env");
    }

    #[test]
    fn inline_code_glued_to_the_flag_or_given_with_equals_is_refused() {
        for (command, arg) in [
            ("python3", "-cprint(1)"),
            ("perl", "-eprint 1"),
            ("ruby", "-eputs 1"),
            ("bun", "-econsole.log(1)"),
            ("lua", "-eprint(1)"),
            ("php", "-rphpinfo();"),
            ("node", "--eval=console.log(1)"),
            ("julia", "--eval=1"),
            ("bun", "--print=1"),
            ("fish", "-cmake"),
            ("fish", "--command=make"),
            ("bash", "-lcmake"),
        ] {
            assert!(launches(command, &[arg]), "{command} {arg}");
        }
        assert!(!launches("python3", &["-m", "pytest"]));
        assert!(!launches("node", &["--max-old-space-size=4096"]));
    }

    #[test]
    fn value_taking_letters_end_the_flag_scan() {
        assert!(!launches("python3", &["-mcoverage", "run"]));
        assert!(!launches("ruby", &["-rbundler/setup", "app.rb"]));
        assert!(!launches("node", &["-rts-node/register", "server.js"]));
        assert!(launches("python3", &["-Ic'import os'"]));
        assert!(launches("ruby", &["-weputs 1"]));
    }

    #[test]
    fn deno_repl_and_shell_runners_are_refused() {
        assert!(launches("deno", &["repl", "--eval=1"]));
        for command in ["script", "su", "flock", "watch"] {
            assert!(launches(command, &["-c", "x"]), "{command}");
        }
    }

    #[test]
    fn versioned_shells_are_shells() {
        for shell in ["ksh93", "bash5", "zsh5.9"] {
            assert!(launches(shell, &["-c", "x"]), "{shell}");
        }
        assert!(!launches("shellcheck", &["-c", "x"]));
    }

    #[test]
    fn nested_wrappers_are_followed_and_deep_chains_fail_closed() {
        assert!(launches("nice", &["time", "nohup", "sh", "-c", "x"]));
        assert!(launches("env", &["nice", "env", "time", "sh", "-c", "x"]));
        let mut chain: Vec<&str> = Vec::new();
        for _ in 0..MAX_WRAPPER_DEPTH {
            chain.extend(["env", "nice"]);
        }
        chain.push("rust-analyzer");
        assert_eq!(
            analyze("env", &chain).unwrap_err(),
            LauncherRefusal::TooDeep
        );
    }

    #[test]
    fn wrapped_program_indices_survive_every_layer() {
        assert_eq!(analyze("busybox", &["nice", "srv"]), Ok(vec![1]));
        assert_eq!(analyze("busybox", &["env", "FOO=1", "srv"]), Ok(vec![2]));
        assert_eq!(
            analyze("nice", &["env", "FOO=1", "timeout", "5", "srv"]),
            Ok(vec![0, 2, 4])
        );
        assert_eq!(
            analyze("env", &["nice", "busybox", "timeout", "5", "srv"]),
            Ok(vec![0, 1, 4])
        );
        assert_eq!(analyze("rust-analyzer", &["--stdio"]), Ok(vec![]));
    }

    #[test]
    fn a_sudo_inside_another_wrapper_is_still_refused() {
        assert_eq!(
            analyze("timeout", &["5", "sudo", "-u", "x", "srv"]).unwrap_err(),
            selects("sudo", LaunchTrigger::Always)
        );
    }
}
