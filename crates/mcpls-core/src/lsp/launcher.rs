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
//! `timeout`, `env`, ...) and unwrap `busybox` applets, and give up on what cannot
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
    CmdSwitch, EchoedArgument, EchoedPath, InlineFlag, LaunchTrigger, LauncherRefusal,
    LongCommandName, OperandKind, PowerShellParameter, RunnerSubcommand, ShellFlag,
    UnanalyzableLaunch,
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
    (
        "dotnet",
        LaunchRule::Subcommands(&[RunnerSubcommand::Run, RunnerSubcommand::Tool]),
    ),
    ("pipenv", LaunchRule::Subcommands(&[RunnerSubcommand::Run])),
    ("pixi", LaunchRule::Subcommands(&[RunnerSubcommand::Run])),
    ("swift", LaunchRule::Subcommands(&[RunnerSubcommand::Run])),
    (
        "stack",
        LaunchRule::Subcommands(&[RunnerSubcommand::Run, RunnerSubcommand::Exec]),
    ),
    (
        "cabal",
        LaunchRule::Subcommands(&[RunnerSubcommand::Run, RunnerSubcommand::Exec]),
    ),
];

/// The command-string grammar a shell speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellFamily {
    /// `sh` and its relatives: `-c`, `--command`.
    Posix,
    /// `cmd`: `/c`, `/k`, `/r`.
    Cmd,
    /// `powershell` and `pwsh`: `-Command` and its prefixes.
    PowerShell,
    /// `fish`: `-c` and `-C`, `--command` and `--init-command`.
    Fish,
    /// `nu`: `-c` and `-e`, `--commands` and `--execute`.
    Nu,
}

const SHELLS: &[(&str, ShellFamily)] = &[
    ("sh", ShellFamily::Posix),
    ("bash", ShellFamily::Posix),
    ("zsh", ShellFamily::Posix),
    ("dash", ShellFamily::Posix),
    ("ksh", ShellFamily::Posix),
    ("fish", ShellFamily::Fish),
    ("csh", ShellFamily::Posix),
    ("tcsh", ShellFamily::Posix),
    ("cmd", ShellFamily::Cmd),
    ("powershell", ShellFamily::PowerShell),
    ("pwsh", ShellFamily::PowerShell),
    ("ash", ShellFamily::Posix),
    ("hush", ShellFamily::Posix),
    ("mksh", ShellFamily::Posix),
    ("oksh", ShellFamily::Posix),
    ("yash", ShellFamily::Posix),
    ("posh", ShellFamily::Posix),
    ("elvish", ShellFamily::Posix),
    ("nu", ShellFamily::Nu),
    ("xonsh", ShellFamily::Posix),
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
    /// What only `env` has on top of the common grammar.
    env: Option<EnvHooks>,
}

/// The parts of `env`'s grammar the other wrappers lack: a lone `-` and
/// `NAME=value` assignments before the command, an option that splits a string
/// into arguments, and options that change the directory a relative program is
/// looked up in. A value-taking short option must stand alone (`-u NAME`), so
/// a value glued into a cluster (`-uNAME`, `-iC dir`) is refused.
#[derive(Clone, Copy)]
struct EnvHooks {
    chdir: &'static [OptionName],
    split_short: char,
    split_long: &'static str,
}

/// A value-taking option, as the table spells it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OptionName {
    Short(char),
    Long(&'static str),
}

/// How far an option reaches in the arguments, and which value-taking option
/// it is.
struct Step {
    width: usize,
    option: Option<OptionName>,
}

/// Where the command a wrapper starts is in its arguments.
struct Invocation {
    command: usize,
    changes_directory: bool,
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
            env: None,
        }
    }

    /// Where the command the wrapper starts is in `args`.
    fn command_index(&self, args: &[String]) -> Result<Invocation, UnanalyzableLaunch> {
        let mut index = 0;
        let mut changes_directory = false;
        while let Some(arg) = args.get(index) {
            if arg == "--" {
                index = index.saturating_add(1);
                break;
            }
            if arg == "-" && self.env.is_some() {
                index = index.saturating_add(1);
                if let Some(next) = args.get(index).filter(|next| next.starts_with('-')) {
                    let letter = next.chars().nth(1).unwrap_or('-');
                    return Err(UnanalyzableLaunch::UnknownOption(EchoedArgument::name(
                        &format!("-{letter}"),
                    )));
                }
                break;
            }
            let Some(step) = self.option_width(args, index)? else {
                break;
            };
            changes_directory |= step
                .option
                .zip(self.env)
                .is_some_and(|(option, hooks)| hooks.chdir.contains(&option));
            index = index.saturating_add(step.width);
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
        if self.env.is_some() {
            while let Some((name, _)) = args.get(index).and_then(|arg| arg.split_once('=')) {
                if name.eq_ignore_ascii_case("PATH") {
                    return Err(UnanalyzableLaunch::PathAssignment);
                }
                index = index.saturating_add(1);
            }
        }
        args.get(index)
            .map(|_| Invocation {
                command: index,
                changes_directory,
            })
            .ok_or(UnanalyzableLaunch::MissingCommand)
    }

    /// How many arguments the option at `args[index]` takes, or `None` when
    /// that argument is not an option.
    fn option_width(
        &self,
        args: &[String],
        index: usize,
    ) -> Result<Option<Step>, UnanalyzableLaunch> {
        let Some(arg) = args.get(index) else {
            return Ok(None);
        };
        let value_follows = || {
            if args.get(index.saturating_add(1)).is_some() {
                Ok(2)
            } else {
                Err(UnanalyzableLaunch::MissingValue(EchoedArgument::name(arg)))
            }
        };
        let step = |width, option| Ok(Some(Step { width, option }));
        if arg.starts_with("--") {
            let (name, attached) = long_name(arg);
            if self.env.is_some_and(|hooks| hooks.split_long == name) {
                return Err(UnanalyzableLaunch::SplitString);
            }
            return if let Some(&known) = self.long_values.iter().find(|long| **long == name) {
                let width = if attached { 1 } else { value_follows()? };
                step(width, Some(OptionName::Long(known)))
            } else if !attached && self.long_flags.contains(&name) {
                step(1, None)
            } else {
                Err(UnanalyzableLaunch::UnknownOption(EchoedArgument::name(arg)))
            };
        }
        let Some(cluster) = arg.strip_prefix('-').filter(|cluster| !cluster.is_empty()) else {
            return Ok(None);
        };
        if self.word_flags.contains(&arg.as_str()) {
            return step(1, None);
        }
        if self.word_values.contains(&arg.as_str()) {
            return step(value_follows()?, None);
        }
        for (offset, letter) in cluster.char_indices() {
            if self.short_flags.contains(&letter) {
                continue;
            }
            if self.short_values.contains(&letter) {
                if self.env.is_some() && cluster.len() > letter.len_utf8() {
                    return Err(self.unknown_short(cluster, letter));
                }
                let glued = offset.saturating_add(letter.len_utf8()) < cluster.len();
                let width = if glued { 1 } else { value_follows()? };
                return step(width, Some(OptionName::Short(letter)));
            }
            return Err(self.unknown_short(cluster, letter));
        }
        step(1, None)
    }

    /// The refusal for `letter` in `cluster`, which this grammar does not read:
    /// a split string when `env`'s `-S` is anywhere in the cluster.
    fn unknown_short(&self, cluster: &str, letter: char) -> UnanalyzableLaunch {
        if self
            .env
            .is_some_and(|hooks| cluster.contains(hooks.split_short))
        {
            UnanalyzableLaunch::SplitString
        } else {
            UnanalyzableLaunch::UnknownOption(EchoedArgument::name(&format!("-{letter}")))
        }
    }
}

/// The name of a long option and whether a value is attached with `=`.
fn long_name(arg: &str) -> (&str, bool) {
    arg.split_once('=')
        .map_or((arg, false), |(name, _)| (name, true))
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
    ExecWrapper {
        short_flags: &['i', '0', 'v'],
        short_values: &['u', 'C'],
        long_flags: &["--ignore-environment", "--null", "--debug"],
        long_values: &["--unset", "--chdir"],
        env: Some(EnvHooks {
            chdir: &[OptionName::Short('C'), OptionName::Long("--chdir")],
            split_short: 'S',
            split_long: "--split-string",
        }),
        ..ExecWrapper::named(&["env", "genv"])
    },
];

/// Interpreters that run a program given on the command line, with the short
/// flag letters and long flags that introduce it. A match is by stem prefix so
/// `python3.12` counts as `python`.
const INLINE_EVAL: &[InlineEval] = &[
    InlineEval {
        data_url_flags: DATA_URL_LOADERS,
        ..InlineEval::new("node", &['e', 'p'], &['r', 'C'], LONG_EVAL_PRINT)
    },
    InlineEval {
        data_url_flags: DATA_URL_LOADERS,
        ..InlineEval::new("bun", &['e', 'p'], &['r', 'c'], LONG_EVAL_PRINT)
    },
    InlineEval::new("lua", &['e'], &['l'], &[]),
    InlineEval::new("rscript", &['e'], &[], &[]),
    InlineEval::new(
        "julia",
        &['e', 'E'],
        &['L', 'J', 'C', 'O', 't', 'p', 'H'],
        LONG_EVAL_PRINT,
    ),
    InlineEval::new("osascript", &['e'], &['l', 's'], &[]),
    InlineEval {
        data_url_flags: DATA_URL_LOADERS,
        ..InlineEval::new("nodejs", &['e', 'p'], &['r', 'C'], LONG_EVAL_PRINT)
    },
    InlineEval::new("python", &['c'], &['m', 'W', 'X', 'Q'], &[]),
    InlineEval {
        module_letters: &['M', 'm'],
        debugger_letters: &['d'],
        ..InlineEval::new(
            "perl",
            &['e', 'E'],
            &['I', 'M', 'm', 'x', 'i', 'F', 'C', 'd', 'D'],
            &[],
        )
    },
    InlineEval::new(
        "ruby",
        &['e'],
        &['r', 'I', 'C', 'E', 'K', 'x', 'F', 'T'],
        &[],
    ),
    InlineEval::new("php", &['r'], &['d', 'c', 'f', 'z'], &[]),
];

const LONG_EVAL_PRINT: &[InlineFlag] = &[InlineFlag::Eval, InlineFlag::Print];

/// Long flags whose value is a module to load, which a `data:` URL turns into
/// an inline program.
const DATA_URL_LOADERS: &[InlineFlag] = &[
    InlineFlag::Import,
    InlineFlag::Loader,
    InlineFlag::ExperimentalLoader,
];

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
    /// Short flag letters whose value must be a module name with an optional
    /// import list: perl turns anything else into code (`-MPOSIX;code`).
    module_letters: &'static [char],
    /// Short flag letters whose value is a debugger module (`perl -d:Mod`).
    debugger_letters: &'static [char],
    /// Long flags whose value must not be a `data:` URL.
    data_url_flags: &'static [InlineFlag],
}

impl InlineEval {
    /// The flag by which `args[index]` gives this interpreter an inline
    /// program, possibly through the argument that follows it.
    fn flag_in(&self, args: &[String], index: usize) -> Option<InlineFlag> {
        let arg = args.get(index)?;
        let next = args.get(index.saturating_add(1)).map(String::as_str);
        let reached =
            |letter| self.program_letters.contains(&letter) || self.value_letters.contains(&letter);
        if let Some((letter, rest)) = scan_short(arg, &['-'], reached) {
            if self.program_letters.contains(&letter) {
                return Some(InlineFlag::Short(letter));
            }
            if self.debugger_letters.contains(&letter) && !is_debugger_spec(rest) {
                return Some(InlineFlag::Short(letter));
            }
            let module = if rest.is_empty() { next } else { Some(rest) };
            if self.module_letters.contains(&letter) && !module.is_some_and(is_module_spec) {
                return Some(InlineFlag::Short(letter));
            }
        }
        let (name, attached) = long_name(arg);
        if let Some(flag) = self
            .long
            .iter()
            .copied()
            .find(|flag| flag.long_name() == Some(name))
        {
            return Some(flag);
        }
        let name = name.replace('_', "-");
        let flag = self
            .data_url_flags
            .iter()
            .copied()
            .find(|flag| flag.long_name() == Some(name.as_str()))?;
        let value = if attached {
            arg.split_once('=').map(|(_, value)| value)
        } else {
            next
        };
        value.is_some_and(is_data_url).then_some(flag)
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
            module_letters: &[],
            debugger_letters: &[],
            data_url_flags: &[],
        }
    }
}

/// Prefix of an argument that names an npm package for a package runner.
pub const NPM_SPECIFIER_PREFIX: &str = "npm:";

/// Most wrappers followed before the launch is treated as unanalyzable.
const MAX_WRAPPER_DEPTH: usize = 8;

/// Binaries that run the applet named by their first argument.
const APPLET_BINARIES: &[&str] = &["busybox", "toybox"];

/// The binary of GNU coreutils and uutils: GNU dispatches on
/// `--coreutils-prog=NAME`, uutils on the bare applet name.
const COREUTILS: &str = "coreutils";

/// The option by which GNU `coreutils` names the applet to run.
const COREUTILS_PROG: &str = "--coreutils-prog";

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
        program: EchoedPath::program(command),
        trigger,
    }
}

fn unanalyzable(command: &str, reason: UnanalyzableLaunch) -> LauncherRefusal {
    LauncherRefusal::Unanalyzable {
        program: EchoedPath::program(command),
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
        if let Some(wrapper) = EXEC_WRAPPERS
            .iter()
            .find(|wrapper| stem.is_any(wrapper.names))
        {
            deepen(depth)?;
            if args.is_empty() {
                return Ok(());
            }
            let invocation = match wrapper.command_index(args) {
                Err(UnanalyzableLaunch::MissingCommand) if wrapper.env.is_some() => return Ok(()),
                other => other.map_err(|reason| unanalyzable(command, reason))?,
            };
            if invocation.changes_directory
                && args
                    .get(invocation.command)
                    .is_some_and(|program| is_relative_path(program))
            {
                return Err(unanalyzable(
                    command,
                    UnanalyzableLaunch::RelativeProgramAfterChdir,
                ));
            }
            return self.start(command, args, invocation.command, base, depth);
        }
        if stem.is_any(APPLET_BINARIES) || stem.is(COREUTILS) {
            deepen(depth)?;
            let Some((first, rest)) = args.split_first() else {
                return Err(unanalyzable(command, UnanalyzableLaunch::MissingCommand));
            };
            let applet = if stem.is(COREUTILS) {
                coreutils_applet(first).map_err(|reason| unanalyzable(command, reason))?
            } else {
                first
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
}

/// The applet `coreutils` runs for its first argument: the value of
/// `--coreutils-prog=`, or the argument itself for uutils. The shebang form
/// and any other spelling of the option cannot be analyzed.
fn coreutils_applet(first: &str) -> Result<&str, UnanalyzableLaunch> {
    let Some(rest) = first.strip_prefix(COREUTILS_PROG) else {
        return Ok(first);
    };
    match rest.strip_prefix('=') {
        Some("") => Err(UnanalyzableLaunch::BlankProgram),
        Some(applet) => Ok(applet),
        None => Err(UnanalyzableLaunch::UnknownOption(EchoedArgument::name(
            first,
        ))),
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
    if let Some((_, family)) = SHELLS
        .iter()
        .find(|(shell, _)| is_interpreter(stem.as_str(), shell))
        && let Some(flag) = args.iter().find_map(|arg| command_flag(*family, arg))
    {
        return Err(selects(command, LaunchTrigger::CommandString(flag)));
    }
    if let Some(flag) = INLINE_EVAL
        .iter()
        .filter(|eval| is_interpreter(stem.as_str(), eval.name))
        .find_map(|eval| (0..args.len()).find_map(|index| eval.flag_in(args, index)))
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
fn command_flag(family: ShellFamily, arg: &str) -> Option<ShellFlag> {
    let lowered = arg.to_ascii_lowercase();
    match family {
        ShellFamily::Cmd => [CmdSwitch::C, CmdSwitch::K, CmdSwitch::R]
            .into_iter()
            .find(|switch| lowered.starts_with(switch.as_str()))
            .map(ShellFlag::SlashC),
        ShellFamily::PowerShell => lowered
            .strip_prefix(['-', '/'])
            .map(|name| name.strip_prefix('-').unwrap_or(name))
            .map(|name| name.split([':', '=']).next().unwrap_or_default())
            .and_then(powershell_parameter)
            .map(ShellFlag::PowerShell),
        ShellFamily::Posix => grammar_flag(&POSIX_GRAMMAR, arg),
        ShellFamily::Fish => grammar_flag(&FISH_GRAMMAR, arg),
        ShellFamily::Nu => grammar_flag(&NU_GRAMMAR, arg),
    }
}

/// The flags of a shell family that give it a command string.
struct ShellGrammar {
    /// Short letters that do, with the flag they spell.
    short: &'static [(char, ShellFlag)],
    /// Short letters that take a value, which ends the scan of a cluster.
    value_letters: &'static [char],
    /// What a cluster starts with: `+c` sets the same option as `-c` in POSIX
    /// shells.
    prefixes: &'static [char],
    /// Long flags that do.
    long: &'static [LongCommandName],
}

const POSIX_GRAMMAR: ShellGrammar = ShellGrammar {
    short: &[('c', ShellFlag::DashC)],
    value_letters: &[],
    prefixes: &['-', '+'],
    long: &[LongCommandName::Command, LongCommandName::Commands],
};

const FISH_GRAMMAR: ShellGrammar = ShellGrammar {
    short: &[('c', ShellFlag::DashC), ('C', ShellFlag::DashCapitalC)],
    value_letters: &['d', 'o', 'D', 'f', 'p'],
    prefixes: &['-'],
    long: &[
        LongCommandName::Command,
        LongCommandName::Commands,
        LongCommandName::InitCommand,
    ],
};

const NU_GRAMMAR: ShellGrammar = ShellGrammar {
    short: &[('c', ShellFlag::DashC), ('e', ShellFlag::DashE)],
    value_letters: &['I'],
    prefixes: &['-'],
    long: &[
        LongCommandName::Command,
        LongCommandName::Commands,
        LongCommandName::Execute,
    ],
};

/// The flag of `grammar` that `arg` spells, in long form or reached by a short
/// letter.
fn grammar_flag(grammar: &ShellGrammar, arg: &str) -> Option<ShellFlag> {
    grammar
        .long
        .iter()
        .find(|name| long_flag_matches(arg, &[name.as_str()]))
        .map(|&name| ShellFlag::LongCommand(name))
        .or_else(|| {
            let reached = |letter| {
                grammar.short.iter().any(|&(short, _)| short == letter)
                    || grammar.value_letters.contains(&letter)
            };
            let (letter, _) = scan_short(arg, grammar.prefixes, reached)?;
            grammar
                .short
                .iter()
                .find(|&&(short, _)| short == letter)
                .map(|&(_, flag)| flag)
        })
}

/// The PowerShell parameter a lowercased `-name` abbreviates.
fn powershell_parameter(name: &str) -> Option<PowerShellParameter> {
    const PARAMETERS: [(&str, PowerShellParameter); 3] = [
        ("command", PowerShellParameter::Command),
        ("commandwithargs", PowerShellParameter::CommandWithArgs),
        ("encodedcommand", PowerShellParameter::EncodedCommand),
    ];
    match name {
        "ec" => return Some(PowerShellParameter::EncodedCommand),
        "cwa" => return Some(PowerShellParameter::CommandWithArgs),
        "" => return None,
        _ => {}
    }
    PARAMETERS
        .iter()
        .find(|(full, _)| full.starts_with(name))
        .map(|&(_, parameter)| parameter)
}

/// The first letter of a single-dash cluster that `reached` accepts, with the
/// rest of the cluster after it.
///
/// Whatever follows a flag letter is its value (`-c'code'`, `-ecode`), so the
/// argument carries inline code. Letters are read in order, digits are skipped
/// (`-0777e`, `-l0e`), and reading stops at the first letter that takes a value,
/// since the rest of the argument is then that option's value (`-mcoverage`,
/// `-rbundler/setup`).
fn scan_short<'a>(
    arg: &'a str,
    prefixes: &[char],
    reached: impl Fn(char) -> bool,
) -> Option<(char, &'a str)> {
    let cluster = arg
        .strip_prefix(prefixes)
        .filter(|cluster| !cluster.starts_with('-'))?;
    let (offset, letter) = cluster
        .char_indices()
        .take_while(|(_, letter)| letter.is_alphanumeric())
        .find(|&(_, letter)| reached(letter))?;
    let rest = cluster.get(offset.saturating_add(letter.len_utf8())..);
    Some((letter, rest.unwrap_or_default()))
}

/// Longest import list a perl `-M` value may carry.
const MAX_IMPORT_LIST_BYTES: usize = 1024;

/// Whether the rest of a perl `-d` cluster is empty, `t`, or a debugger
/// module (`:Mod`, `t:Mod`).
fn is_debugger_spec(rest: &str) -> bool {
    let rest = rest.strip_prefix('t').unwrap_or(rest);
    rest.is_empty() || rest.strip_prefix(':').is_some_and(is_module_spec)
}

/// Whether `value` is what perl accepts after `-M` or `-m` without turning it
/// into code: an optional `-`, a module name and an optional `=import,list`,
/// which perl quotes itself.
fn is_module_spec(value: &str) -> bool {
    let spec = value.strip_prefix('-').unwrap_or(value);
    let (module, imports) = spec.split_once('=').unwrap_or((spec, ""));
    if imports.len() > MAX_IMPORT_LIST_BYTES {
        return false;
    }
    let identifier = |part: &str| {
        part.chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    module.split("::").all(identifier)
}

/// Whether `value` is a `data:` URL, whose scheme is case-insensitive and may
/// follow leading control characters or spaces that URL parsing strips.
fn is_data_url(value: &str) -> bool {
    value
        .trim_start_matches(|c: char| c.is_ascii_control() || c == ' ')
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
}

/// Whether `arg` is one of the long `flags`, bare or with an `=value`.
fn long_flag_matches(arg: &str, flags: &[&str]) -> bool {
    flags.contains(&long_name(arg).0)
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
            std::assert_matches!(
                analyze("perl", &[flags, "code"]),
                Err(LauncherRefusal::SelectsWorkspaceCode {
                    trigger: LaunchTrigger::InlineProgram(InlineFlag::Short('e' | 'p')),
                    ..
                }),
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
            selects(
                "fish",
                LaunchTrigger::CommandString(ShellFlag::LongCommand(LongCommandName::Command))
            )
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
            std::assert_matches!(
                unanalyzable_reason("env", args),
                UnanalyzableLaunch::UnknownOption(_),
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
        std::assert_matches!(
            unanalyzable_reason("env", &["--chdir"]),
            UnanalyzableLaunch::MissingValue(_)
        );
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
        for (flag, switch) in [
            ("/c", CmdSwitch::C),
            ("/C", CmdSwitch::C),
            ("/k", CmdSwitch::K),
            ("/r", CmdSwitch::R),
            ("/ccmd", CmdSwitch::C),
            ("/Cserver", CmdSwitch::C),
        ] {
            assert_eq!(
                analyze("cmd", &[flag, "server"]).unwrap_err(),
                selects(
                    "cmd",
                    LaunchTrigger::CommandString(ShellFlag::SlashC(switch))
                ),
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
                LaunchTrigger::CommandString(ShellFlag::PowerShell(PowerShellParameter::Command))
            )
        );
        assert!(!launches("sh", &["server.sh"]));
        assert!(!launches("bash", &["--norc", "server.sh"]));
    }

    #[test]
    fn every_listed_shell_refuses_a_command_string() {
        for (shell, _) in SHELLS {
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
            std::assert_matches!(
                unanalyzable_reason(command, args),
                UnanalyzableLaunch::UnknownOption(_),
                "{command} {args:?}"
            );
        }
    }

    #[test]
    fn parsed_wrappers_refuse_a_missing_value_or_command() {
        std::assert_matches!(
            unanalyzable_reason("nice", &["-n"]),
            UnanalyzableLaunch::MissingValue(_)
        );
        std::assert_matches!(
            unanalyzable_reason("arch", &["-arch"]),
            UnanalyzableLaunch::MissingValue(_)
        );
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

    #[test]
    fn versioned_powershell_stems_use_the_powershell_grammar() {
        for command in ["pwsh7", "PowerShell7.exe", "/opt/pwsh7.2", "powershell5"] {
            for flag in ["-Command", "-c", "-ec", "/command"] {
                assert!(launches(command, &[flag, "x"]), "{command} {flag}");
            }
            assert!(
                !launches(command, &["-NoProfile", "server.ps1"]),
                "{command}"
            );
        }
    }

    #[test]
    fn the_refusal_names_the_spelling_that_matched() {
        let trigger = |command, arg| match analyze(command, &[arg, "x"]) {
            Err(LauncherRefusal::SelectsWorkspaceCode {
                trigger: LaunchTrigger::CommandString(flag),
                ..
            }) => flag.to_string(),
            other => panic!("expected a command string, got {other:?}"),
        };
        assert_eq!(trigger("cmd", "/k"), "/k");
        assert_eq!(trigger("cmd", "/R"), "/r");
        assert_eq!(trigger("nu", "--commands"), "--commands");
        assert_eq!(trigger("nu", "--command=x"), "--command");
        assert_eq!(trigger("pwsh", "-comm"), "-Command");
        assert_eq!(trigger("pwsh", "-commandw"), "-CommandWithArgs");
        assert_eq!(trigger("pwsh", "-ec"), "-EncodedCommand");
        assert_eq!(trigger("pwsh", "-e"), "-EncodedCommand");
    }

    #[test]
    fn env_and_the_other_wrappers_read_long_options_alike() {
        for command in ["env", "nice"] {
            let args = ["--bogus=1", "srv"];
            std::assert_matches!(
                unanalyzable_reason(command, &args),
                UnanalyzableLaunch::UnknownOption(_),
                "{command}"
            );
        }
        assert_eq!(analyze("env", &["--unset=A", "srv"]), Ok(vec![1]));
        assert_eq!(analyze("nice", &["--adjustment=5", "srv"]), Ok(vec![1]));
        assert_eq!(long_name("--a=b=c"), ("--a", true));
        assert_eq!(long_name("--a"), ("--a", false));
    }

    #[test]
    fn env_chdir_is_tracked_through_the_shared_parser() {
        for args in [
            &["--chdir=/d", "bin/srv"][..],
            &["-i", "-C", "/d", "bin/srv"],
        ] {
            assert_eq!(
                unanalyzable_reason("env", args),
                UnanalyzableLaunch::RelativeProgramAfterChdir,
                "{args:?}"
            );
        }
        assert_eq!(analyze("env", &["-i", "bin/srv"]), Ok(vec![1]));
    }

    #[test]
    fn powershell_cwa_alias_is_refused() {
        for command in ["pwsh", "pwsh7", "PowerShell7.exe"] {
            assert_eq!(
                analyze(command, &["-cwa", "& x"]).unwrap_err(),
                selects(
                    command,
                    LaunchTrigger::CommandString(ShellFlag::PowerShell(
                        PowerShellParameter::CommandWithArgs
                    ))
                ),
                "{command}"
            );
        }
    }

    #[test]
    fn fish_init_commands_are_refused_but_posix_capital_c_is_not() {
        for (arg, flag) in [
            ("-C", ShellFlag::DashCapitalC),
            ("-Ccd /ws", ShellFlag::DashCapitalC),
            ("-lC", ShellFlag::DashCapitalC),
            (
                "--init-command=cd /ws",
                ShellFlag::LongCommand(LongCommandName::InitCommand),
            ),
            (
                "--init-command",
                ShellFlag::LongCommand(LongCommandName::InitCommand),
            ),
        ] {
            assert_eq!(
                analyze("fish", &[arg, "x"]).unwrap_err(),
                selects("fish", LaunchTrigger::CommandString(flag)),
                "{arg}"
            );
        }
        for shell in ["bash", "sh", "zsh", "ksh"] {
            assert!(!launches(shell, &["-C", "script.sh"]), "{shell}");
            assert!(
                !launches(shell, &["--init-command=x", "script.sh"]),
                "{shell}"
            );
        }
        assert!(!launches("fish", &["-N", "server.fish"]));
        assert!(!launches("fish", &["-dcategory", "server.fish"]));
    }

    #[test]
    fn nu_execute_is_refused_but_other_shells_keep_dash_e() {
        for (arg, flag) in [
            ("-e", ShellFlag::DashE),
            ("-ecode", ShellFlag::DashE),
            (
                "--execute=^x",
                ShellFlag::LongCommand(LongCommandName::Execute),
            ),
            (
                "--execute",
                ShellFlag::LongCommand(LongCommandName::Execute),
            ),
        ] {
            assert_eq!(
                analyze("nu", &[arg, "x"]).unwrap_err(),
                selects("nu", LaunchTrigger::CommandString(flag)),
                "{arg}"
            );
        }
        assert!(!launches("bash", &["-e", "script.sh"]));
        assert!(!launches("fish", &["--execute=x", "server.fish"]));
        assert!(!launches("nu", &["-n", "server.nu"]));
    }

    #[test]
    fn gnu_coreutils_dispatches_on_the_coreutils_prog_option() {
        assert_eq!(
            unanalyzable_reason("coreutils", &["--coreutils-prog=env", "-S", "srv"]),
            UnanalyzableLaunch::SplitString
        );
        assert!(launches(
            "coreutils",
            &["--coreutils-prog=env", "PATH=/ws/bin", "srv"]
        ));
        assert!(launches(
            "/usr/bin/coreutils",
            &["--coreutils-prog=env", "sh", "-c", "x"]
        ));
        assert_eq!(
            analyze("coreutils", &["--coreutils-prog=nice", "srv"]),
            Ok(vec![1])
        );
        assert_eq!(
            analyze("coreutils", &["--coreutils-prog=env", "FOO=1", "srv"]),
            Ok(vec![2])
        );
        assert_eq!(analyze("coreutils", &["nice", "srv"]), Ok(vec![1]));
    }

    #[test]
    fn coreutils_forms_that_cannot_be_analyzed_are_refused() {
        for arg in [
            "--coreutils-prog-shebang=env",
            "--coreutils-prog",
            "--coreutils-progx=env",
        ] {
            std::assert_matches!(
                unanalyzable_reason("coreutils", &[arg, "srv"]),
                UnanalyzableLaunch::UnknownOption(_),
                "{arg}"
            );
        }
        assert_eq!(
            unanalyzable_reason("coreutils", &["--coreutils-prog=", "srv"]),
            UnanalyzableLaunch::BlankProgram
        );
        assert!(launches("busybox", &["env", "sh", "-c", "x"]));
        assert!(launches("toybox", &["nice", "npx", "srv"]));
    }

    #[test]
    fn perl_module_values_that_carry_code_are_refused() {
        for args in [
            &["-MPOSIX;do(q{/ws/Evil.pm})", "script.pl"][..],
            &["-mPOSIX;system('x')", "script.pl"],
            &["-M", "POSIX;system('x')", "script.pl"],
            &["-MPOSIX qw(); system('x')", "script.pl"],
            &["-M", "script.pl"],
            &["-M"],
            &["-lM-X;code", "script.pl"],
        ] {
            std::assert_matches!(
                analyze("perl", args),
                Err(LauncherRefusal::SelectsWorkspaceCode {
                    trigger: LaunchTrigger::InlineProgram(InlineFlag::Short('M' | 'm')),
                    ..
                }),
                "{args:?}"
            );
        }
    }

    #[test]
    fn perl_module_values_that_are_modules_are_admitted() {
        for args in [
            &["-MPOSIX", "script.pl"][..],
            &["-MList::Util=sum,max", "script.pl"],
            &["-M-strict", "script.pl"],
            &["-mFoo::Bar", "script.pl"],
            &["-M", "POSIX", "script.pl"],
            &["-Mstrict", "-Mwarnings", "script.pl"],
        ] {
            assert!(!launches("perl", args), "{args:?}");
        }
    }

    #[test]
    fn data_url_modules_given_to_node_flags_are_refused() {
        for command in ["node", "nodejs", "node22", "bun"] {
            for args in [
                &["--import=data:text/javascript,x", "srv.mjs"][..],
                &["--import", "data:text/javascript,x", "srv.mjs"],
                &["--loader=DATA:text/javascript,x", "srv.mjs"],
                &["--experimental-loader=data:text/javascript,x", "srv.mjs"],
                &["--experimental_loader=data:text/javascript,x", "srv.mjs"],
                &["--import=  data:text/javascript,x", "srv.mjs"],
            ] {
                std::assert_matches!(
                    analyze(command, args),
                    Err(LauncherRefusal::SelectsWorkspaceCode {
                        trigger: LaunchTrigger::InlineProgram(
                            InlineFlag::Import
                                | InlineFlag::Loader
                                | InlineFlag::ExperimentalLoader
                        ),
                        ..
                    }),
                    "{command} {args:?}"
                );
            }
        }
    }

    #[test]
    fn node_modules_that_are_not_data_urls_are_admitted() {
        for args in [
            &["--import=./register.mjs", "srv.mjs"][..],
            &["--import", "file:///opt/register.mjs", "srv.mjs"],
            &["--loader=ts-node/esm", "srv.ts"],
            &["--import"],
            &["--import=dat", "srv.mjs"],
        ] {
            assert!(!launches("node", args), "{args:?}");
        }
        assert!(!launches("python3", &["--import=data:x", "srv.py"]));
    }

    #[test]
    fn run_subcommands_of_more_toolchains_are_refused() {
        for (command, subcommand) in [
            ("dotnet", RunnerSubcommand::Run),
            ("pipenv", RunnerSubcommand::Run),
            ("pixi", RunnerSubcommand::Run),
            ("swift", RunnerSubcommand::Run),
            ("stack", RunnerSubcommand::Run),
            ("stack", RunnerSubcommand::Exec),
            ("cabal", RunnerSubcommand::Run),
            ("cabal", RunnerSubcommand::Exec),
        ] {
            assert_eq!(
                analyze(command, &[subcommand.as_str(), "server"]).unwrap_err(),
                selects(command, LaunchTrigger::Subcommand(subcommand)),
                "{command} {subcommand}"
            );
            assert!(!launches(command, &["--version"]), "{command}");
        }
        assert!(!launches("dotnet", &["build"]));
        assert!(!launches("swift", &["build"]));
    }

    #[test]
    fn posix_shells_refuse_a_command_string_behind_an_option_cluster_or_plus() {
        for shell in ["bash", "dash", "zsh", "ksh", "sh"] {
            for args in [
                &["-oc", "errexit", "x"][..],
                &["-eoc", "errexit", "x"],
                &["+c", "x"],
                &["+ec", "x"],
                &["-o", "errexit", "-c", "x"],
            ] {
                assert!(launches(shell, args), "{shell} {args:?}");
            }
            assert!(
                !launches(shell, &["-o", "pipefail", "script.sh"]),
                "{shell}"
            );
            assert!(!launches(shell, &["+e", "script.sh"]), "{shell}");
            assert!(!launches(shell, &["+x", "script.sh"]), "{shell}");
        }
    }

    #[test]
    fn powershell_accepts_double_dash_and_attached_values() {
        for flag in [
            "--command",
            "--c",
            "--commandwithargs",
            "--CommandWithArgs",
            "/cwa",
            "-CommandWithArgs",
            "-command:x",
            "-Command=x",
            "--cwa",
        ] {
            assert!(launches("pwsh", &[flag, "x"]), "{flag}");
        }
        assert!(!launches("pwsh", &["-NoProfile", "server.ps1"]));
    }

    #[test]
    fn perl_debugger_modules_must_be_module_names() {
        for arg in ["-d:Mod;BEGIN{system(1)}", "-dt:Mod;x", "-dx", "-d:"] {
            assert!(launches("perl", &[arg, "srv.pl"]), "{arg}");
        }
        for arg in ["-d", "-dt", "-d:Devel::NYTProf", "-dt:Foo=a,b"] {
            assert!(!launches("perl", &[arg, "srv.pl"]), "{arg}");
        }
        let long = format!("-MFoo={}", "a".repeat(MAX_IMPORT_LIST_BYTES + 1));
        assert!(launches("perl", &[&long, "srv.pl"]));
        let fits = format!("-MFoo={}", "a".repeat(MAX_IMPORT_LIST_BYTES));
        assert!(!launches("perl", &[&fits, "srv.pl"]));
    }

    #[test]
    fn refusals_echo_the_whole_program_path() {
        let Err(LauncherRefusal::SelectsWorkspaceCode { program, .. }) =
            analyze("/opt/Program Files/PowerShell/pwsh", &["-c", "x"])
        else {
            panic!("expected a refusal");
        };
        assert_eq!(program.as_str(), "/opt/Program Files/PowerShell/pwsh");
        let Err(LauncherRefusal::Unanalyzable { program, .. }) =
            analyze("/opt/Program Files/env", &["-z", "srv"])
        else {
            panic!("expected a refusal");
        };
        assert_eq!(program.as_str(), "/opt/Program Files/env");
    }
}
