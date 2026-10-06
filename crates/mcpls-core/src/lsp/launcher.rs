//! Launchers that choose the server to run from files in the workspace.
//!
//! A package runner (`npx`), task runner (`make`) or toolchain wrapper
//! (`cargo run`) takes a command that lies outside the workspace and starts a
//! program the workspace selects: `./node_modules/.bin/<name>`, a `Makefile`
//! target, a `build.rs`. Untrusted-workspace mode cannot vet that program, so
//! it refuses these launches ([`launches_from_workspace`]).
//!
//! The rules are best-effort. They match the command's file stem and its
//! arguments, unwrap `env` and `busybox`, and give up on what cannot be
//! analyzed (an `env` option this list does not know, a `PATH=` assignment, a
//! shell or interpreter given a command string). The lists are closed, not
//! exhaustive: a shell or interpreter missing from them is admitted. The
//! trusted configuration is the boundary, not this list.

use crate::config::CommandStem;

/// How a launcher's use selects workspace code.
#[derive(Debug, Clone, Copy)]
enum LaunchRule {
    /// Every use of the command does.
    Always,
    /// Only a use with one of these subcommands among its arguments does.
    Subcommands(&'static [&'static str]),
}

const RUNNERS: &[(&str, LaunchRule)] = &[
    ("npm", LaunchRule::Always),
    ("npx", LaunchRule::Always),
    ("bunx", LaunchRule::Always),
    ("pnpx", LaunchRule::Always),
    ("pnpm", LaunchRule::Always),
    ("yarn", LaunchRule::Always),
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
    ("bun", LaunchRule::Subcommands(&["x", "run"])),
    (
        "deno",
        LaunchRule::Subcommands(&["run", "x", "task", "eval", "repl"]),
    ),
    ("cargo", LaunchRule::Subcommands(&["run"])),
    ("go", LaunchRule::Subcommands(&["run", "tool"])),
    ("uv", LaunchRule::Subcommands(&["run", "tool"])),
    ("pipx", LaunchRule::Subcommands(&["run"])),
    ("poetry", LaunchRule::Subcommands(&["run"])),
    ("pdm", LaunchRule::Subcommands(&["run"])),
    ("hatch", LaunchRule::Subcommands(&["run"])),
    ("bundle", LaunchRule::Subcommands(&["exec"])),
    ("dotnet", LaunchRule::Subcommands(&["tool"])),
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

/// Programs that start the command given in their own arguments, with options
/// of their own in between. A launch is refused when any argument starts a
/// command the rules above refuse.
const EXEC_WRAPPERS: &[&str] = &[
    "time",
    "nice",
    "nohup",
    "timeout",
    "setsid",
    "stdbuf",
    "ionice",
    "chrt",
    "taskset",
    "sudo",
    "doas",
    "arch",
    "caffeinate",
    "chroot",
    "unshare",
    "nsenter",
    "strace",
    "systemd-run",
];

/// Interpreters that run a program given on the command line, with the short
/// flag letters and long flags that introduce it. A match is by stem prefix so
/// `python3.12` counts as `python`.
const INLINE_EVAL: &[InlineEval] = &[
    InlineEval::new("node", &['e', 'p'], &['r', 'C'], &["--eval", "--print"]),
    InlineEval::new("bun", &['e', 'p'], &['r', 'c'], &["--eval", "--print"]),
    InlineEval::new("lua", &['e'], &['l'], &[]),
    InlineEval::new("rscript", &['e'], &[], &[]),
    InlineEval::new(
        "julia",
        &['e', 'E'],
        &['L', 'J', 'C', 'O', 't', 'p', 'H'],
        &["--eval", "--print"],
    ),
    InlineEval::new("osascript", &['e'], &['l', 's'], &[]),
    InlineEval::new("nodejs", &['e', 'p'], &['r', 'C'], &["--eval", "--print"]),
    InlineEval::new("python", &['c'], &['m', 'W', 'X', 'Q'], &[]),
    InlineEval::new(
        "perl",
        &['e', 'E'],
        &['I', 'M', 'm', 'x', 'i', 'l', '0', 'F', 'C', 'd', 'D'],
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
    long: &'static [&'static str],
}

impl InlineEval {
    const fn new(
        name: &'static str,
        program_letters: &'static [char],
        value_letters: &'static [char],
        long: &'static [&'static str],
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

/// Most `env` wrappers followed before the launch is treated as unanalyzable.
const MAX_ENV_DEPTH: usize = 8;

/// `env` long options that take no value.
const ENV_LONG_FLAGS: &[&str] = &["--ignore-environment", "--null", "--debug"];

/// `env` long options that take a value, attached with `=` or separate.
const ENV_LONG_OPTIONS_WITH_VALUE: &[&str] = &["--unset", "--chdir"];

/// `env` short flags that take no value and may be clustered.
const ENV_SHORT_FLAGS: &[char] = &['i', '0', 'v'];

/// Whether starting `command` with `args` lets the workspace choose the
/// program that runs.
#[must_use]
pub fn launches_from_workspace(command: &str, args: &[String]) -> bool {
    args.iter().any(|arg| arg.starts_with(NPM_SPECIFIER_PREFIX))
        || launch_selects_workspace_code(command, args, 0)
}

fn launch_selects_workspace_code(command: &str, args: &[String], env_depth: usize) -> bool {
    let stem = CommandStem::of(command);
    if stem.is("env") {
        return env_wraps_workspace_launch(args, env_depth);
    }
    if SHELLS
        .iter()
        .any(|shell| is_interpreter(stem.as_str(), shell))
        && args.iter().any(|arg| is_command_flag(stem.as_str(), arg))
    {
        return true;
    }
    if stem.is_any(&["sudo", "doas"]) && args.iter().any(|arg| sudo_runs_a_string(arg)) {
        return true;
    }
    if stem.is_any(EXEC_WRAPPERS) {
        return wrapped_command_selects_workspace_code(args, env_depth);
    }
    if stem.is("busybox") {
        return args.first().is_none_or(|applet| {
            env_depth >= MAX_ENV_DEPTH
                || launch_selects_workspace_code(
                    applet,
                    args.get(1..).unwrap_or_default(),
                    env_depth.saturating_add(1),
                )
        });
    }
    if args
        .iter()
        .any(|arg| is_inline_eval_flag(stem.as_str(), arg))
    {
        return true;
    }
    RUNNERS
        .iter()
        .find(|(name, _)| stem.is(name))
        .is_some_and(|(_, rule)| match rule {
            LaunchRule::Always => true,
            LaunchRule::Subcommands(subcommands) => {
                args.iter().any(|arg| subcommands.contains(&arg.as_str()))
            }
        })
}

/// Whether any argument of an exec wrapper starts a command the rules refuse.
///
/// The wrapper's own options and the command's position are not parsed:
/// every suffix of the arguments is checked as a command. A nested wrapper is
/// covered by the suffixes behind it, and its own rule (`sudo`/`doas` running
/// a shell string or setting a variable) is applied where it appears. Chains
/// deeper than [`MAX_ENV_DEPTH`] are refused.
fn wrapped_command_selects_workspace_code(args: &[String], env_depth: usize) -> bool {
    if env_depth >= MAX_ENV_DEPTH {
        return true;
    }
    let inner_depth = env_depth.saturating_add(1);
    args.iter().enumerate().any(|(index, command)| {
        let rest = args.get(index.saturating_add(1)..).unwrap_or_default();
        let stem = CommandStem::of(command);
        if stem.is_any(EXEC_WRAPPERS) {
            stem.is_any(&["sudo", "doas"]) && rest.iter().any(|arg| sudo_runs_a_string(arg))
        } else {
            launch_selects_workspace_code(command, rest, inner_depth)
        }
    })
}

/// Whether a shell argument introduces a command string, which cannot be
/// analyzed: `-c` or a cluster holding it (`-lc`) for POSIX shells, `/c`, `/k`
/// or `/r` for `cmd` (also glued to the command, `/ccmd`), and the
/// `-Command`, `-CommandWithArgs` and `-EncodedCommand` parameters of
/// PowerShell, which accepts any prefix of them and the alias `-ec`.
fn is_command_flag(shell: &str, arg: &str) -> bool {
    let arg = arg.to_ascii_lowercase();
    match shell {
        "cmd" => ["/c", "/k", "/r"].iter().any(|flag| arg.starts_with(flag)),
        "powershell" | "pwsh" => arg.strip_prefix(['-', '/']).is_some_and(|name| {
            name == "ec"
                || (!name.is_empty()
                    && ["command", "commandwithargs", "encodedcommand"]
                        .iter()
                        .any(|parameter| parameter.starts_with(name)))
        }),
        _ => {
            long_flag_matches(&arg, &["--command", "--commands"])
                || has_short_flag(&arg, &['c'], &['o'])
        }
    }
}

/// Whether `arg` is a single-dash argument whose flag letters reach one of
/// `flags`: whatever follows such a letter is its value (`-c'code'`,
/// `-ecode`), so the argument carries inline code. Letters are read in order
/// and reading stops at the first one in `value_letters`, since the rest of the
/// argument is then that option's value (`-mcoverage`, `-rbundler/setup`).
fn has_short_flag(arg: &str, flags: &[char], value_letters: &[char]) -> bool {
    arg.strip_prefix('-').is_some_and(|cluster| {
        !cluster.starts_with('-')
            && cluster
                .chars()
                .take_while(|letter| letter.is_alphabetic())
                .find(|letter| flags.contains(letter) || value_letters.contains(letter))
                .is_some_and(|letter| flags.contains(&letter))
    })
}

/// Whether `arg` is one of the long `flags`, bare or with an `=value`.
fn long_flag_matches(arg: &str, flags: &[&str]) -> bool {
    let name = arg.split_once('=').map_or(arg, |(name, _)| name);
    flags.contains(&name)
}

/// Whether a `sudo`/`doas` argument runs a shell string or sets a variable
/// for the command: `-s`, `-i`, `--shell`, `--login`, or `NAME=value`.
fn sudo_runs_a_string(arg: &str) -> bool {
    has_short_flag(
        arg,
        &['s', 'i'],
        &['u', 'g', 'C', 'h', 'p', 'r', 't', 'T', 'U'],
    ) || long_flag_matches(arg, &["--shell", "--login"])
        || arg.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty() && !name.starts_with('-') && !name.contains('/')
        })
}

/// Whether `stem` is the interpreter `name`, optionally followed by a version
/// (`python3.12`, `node18`), but not another tool sharing the prefix
/// (`nodemon`, `phpunit`).
fn is_interpreter(stem: &str, name: &str) -> bool {
    stem.strip_prefix(name)
        .is_some_and(|version| version.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

/// Whether `arg` makes the interpreter `stem` run a program given inline.
fn is_inline_eval_flag(stem: &str, arg: &str) -> bool {
    INLINE_EVAL
        .iter()
        .filter(|eval| is_interpreter(stem, eval.name))
        .any(|eval| {
            has_short_flag(arg, eval.program_letters, eval.value_letters)
                || long_flag_matches(arg, eval.long)
        })
}

/// Whether the command an `env` invocation starts, unwrapped, selects workspace
/// code. Anything that could change which program runs or where it is looked
/// up is refused as unanalyzable: `-S`, `-P`, an option this list does not
/// know, a `-u`/`-C` value glued into a cluster, a `PATH=` assignment (it
/// bypasses the sanitized `PATH`), or a chain of more than [`MAX_ENV_DEPTH`]
/// wrappers.
fn env_wraps_workspace_launch(args: &[String], env_depth: usize) -> bool {
    if env_depth >= MAX_ENV_DEPTH {
        return true;
    }
    let inner_depth = env_depth.saturating_add(1);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--" {
            return rest.next().is_some_and(|command| {
                launch_selects_workspace_code(command, rest.as_slice(), inner_depth)
            });
        }
        if let Some(long) = arg.strip_prefix("--") {
            let name = format!("--{}", long.split('=').next().unwrap_or_default());
            if ENV_LONG_OPTIONS_WITH_VALUE.contains(&name.as_str()) {
                if !long.contains('=') {
                    rest.next();
                }
            } else if !ENV_LONG_FLAGS.contains(&name.as_str()) || long.contains('=') {
                return true;
            }
            continue;
        }
        if let Some(cluster) = arg.strip_prefix('-') {
            match cluster {
                "u" | "C" => {
                    rest.next();
                }
                _ if cluster.chars().all(|flag| ENV_SHORT_FLAGS.contains(&flag)) => {}
                _ => return true,
            }
            continue;
        }
        if let Some((name, _)) = arg.split_once('=') {
            if name.eq_ignore_ascii_case("PATH") {
                return true;
            }
            continue;
        }
        return launch_selects_workspace_code(arg, rest.as_slice(), inner_depth);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launches(command: &str, args: &[&str]) -> bool {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        launches_from_workspace(command, &args)
    }

    #[test]
    fn always_rules_refuse_every_use() {
        for runner in [
            "npm", "npx", "bunx", "pnpx", "pnpm", "yarn", "uvx", "corepack", "make", "just",
            "task", "rake", "mvn", "sbt",
        ] {
            assert!(launches(runner, &[]), "{runner}");
            assert!(
                launches(&format!("/usr/local/bin/{runner}"), &["x"]),
                "{runner}"
            );
        }
    }

    #[test]
    fn subcommand_rules_refuse_only_the_listed_uses() {
        for (command, subcommand) in [
            ("bun", "x"),
            ("bun", "run"),
            ("deno", "run"),
            ("deno", "x"),
            ("deno", "task"),
            ("cargo", "run"),
            ("go", "run"),
            ("go", "tool"),
            ("uv", "run"),
            ("uv", "tool"),
            ("pipx", "run"),
            ("poetry", "run"),
            ("pdm", "run"),
            ("hatch", "run"),
            ("bundle", "exec"),
            ("dotnet", "tool"),
        ] {
            assert!(
                launches(command, &[subcommand, "server"]),
                "{command} {subcommand}"
            );
            assert!(!launches(command, &["--version"]), "{command}");
        }
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
        assert!(launches("deno", &["lsp", "npm:typescript-language-server"]));
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
        assert!(launches("env", &["FOO=1", "-i", "-u", "BAR", "make"]));
        assert!(launches("/usr/bin/env", &["--", "cargo", "run"]));
        assert!(!launches("env", &["FOO=1", "rust-analyzer"]));
        assert!(!launches("env", &["-i", "cargo", "--version"]));
        assert!(!launches("env", &[]));
    }

    #[test]
    fn env_split_string_and_deep_chains_are_unanalyzable() {
        assert!(launches("env", &["-S", "rust-analyzer --flag"]));
        assert!(launches("env", &["--split-string=rust-analyzer"]));
        let mut chain: Vec<&str> = vec!["env"; MAX_ENV_DEPTH + 1];
        chain.push("rust-analyzer");
        assert!(launches("env", &chain));
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
    fn cmd_command_flags_are_recognized_with_and_without_a_space() {
        for flag in ["/c", "/C", "/k", "/r", "/ccmd", "/Cserver"] {
            assert!(launches("cmd", &[flag, "server"]), "{flag}");
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
        assert!(launches("busybox", &[]));
        assert!(!launches("busybox", &["rust-analyzer"]));
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
    }

    #[test]
    fn shells_with_a_command_string_are_unanalyzable() {
        assert!(launches("sh", &["-c", "rust-analyzer"]));
        assert!(launches("/bin/bash", &["-lc", "rust-analyzer"]));
        assert!(launches("cmd.exe", &["/C", "server"]));
        assert!(launches("powershell", &["-Command", "server"]));
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
    fn exec_wrappers_refuse_when_the_wrapped_command_would_be_refused() {
        for wrapper in EXEC_WRAPPERS {
            assert!(launches(wrapper, &["sh", "-c", "x"]), "{wrapper}");
            assert!(launches(wrapper, &["5", "npx", "srv"]), "{wrapper}");
            assert!(
                launches(wrapper, &["-n", "5", "ash", "-c", "x"]),
                "{wrapper}"
            );
            assert!(!launches(wrapper, &["rust-analyzer"]), "{wrapper}");
            assert!(!launches(wrapper, &[]), "{wrapper}");
        }
        assert!(launches("sudo", &["-u", "root", "env", "-S", "x"]));
        assert!(!launches("timeout", &["5", "rust-analyzer", "--stdio"]));
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
    fn sudo_inside_another_wrapper_keeps_its_own_rules() {
        assert!(launches("nice", &["sudo", "-s", "make"]));
        assert!(launches("timeout", &["5", "sudo", "PATH=/ws/bin", "srv"]));
        assert!(!launches("nice", &["sudo", "-u", "nobody", "gopls"]));
    }

    #[test]
    fn deno_repl_and_shell_runners_are_refused() {
        assert!(launches("deno", &["repl", "--eval=1"]));
        for command in ["script", "su", "flock", "watch"] {
            assert!(launches(command, &["-c", "x"]), "{command}");
        }
    }

    #[test]
    fn sudo_shell_and_assignments_are_refused() {
        assert!(launches("sudo", &["-s", "make"]));
        assert!(launches("sudo", &["-i"]));
        assert!(launches("doas", &["--shell"]));
        assert!(launches("sudo", &["PATH=/ws/bin", "srv"]));
        assert!(!launches("sudo", &["-u", "nobody", "gopls"]));
    }

    #[test]
    fn more_exec_wrappers_are_scanned_and_versioned_shells_are_shells() {
        for wrapper in [
            "arch",
            "caffeinate",
            "chroot",
            "unshare",
            "nsenter",
            "strace",
            "systemd-run",
        ] {
            assert!(launches(wrapper, &["sh", "-c", "x"]), "{wrapper}");
        }
        for shell in ["ksh93", "bash5", "zsh5.9"] {
            assert!(launches(shell, &["-c", "x"]), "{shell}");
        }
        assert!(!launches("shellcheck", &["-c", "x"]));
    }

    #[test]
    fn nested_wrappers_are_scanned_and_deep_chains_fail_closed() {
        assert!(launches("nice", &["time", "nohup", "sh", "-c", "x"]));
        assert!(launches("env", &["nice", "env", "time", "sh", "-c", "x"]));
        let mut chain: Vec<&str> = Vec::new();
        for _ in 0..MAX_ENV_DEPTH {
            chain.extend(["env", "nice"]);
        }
        chain.push("rust-analyzer");
        assert!(launches("env", &chain));
    }
}
