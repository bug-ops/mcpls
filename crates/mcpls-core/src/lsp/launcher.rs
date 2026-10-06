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
//! shell or interpreter given a command string). The trusted configuration is
//! the boundary, not this list.

use std::path::Path;

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
    ("bun", LaunchRule::Subcommands(&["x", "run"])),
    ("deno", LaunchRule::Subcommands(&["run", "x", "task"])),
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
];

/// Interpreters that run a program given on the command line, with the short
/// flag letters and long flags that introduce it. A match is by stem prefix so
/// `python3.12` counts as `python`.
const INLINE_EVAL: &[(&str, &[char], &[&str])] = &[
    ("node", &['e', 'p'], &["--eval", "--print"]),
    ("nodejs", &['e', 'p'], &["--eval", "--print"]),
    ("python", &['c'], &[]),
    ("perl", &['e', 'E'], &[]),
    ("ruby", &['e'], &[]),
    ("php", &['r'], &[]),
];

const NPM_SPECIFIER_PREFIX: &str = "npm:";

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

fn command_stem(command: &str) -> String {
    Path::new(command)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn launch_selects_workspace_code(command: &str, args: &[String], env_depth: usize) -> bool {
    let stem = command_stem(command);
    if stem == "env" {
        return env_wraps_workspace_launch(args, env_depth);
    }
    if SHELLS.contains(&stem.as_str()) && args.iter().any(|arg| is_command_flag(&stem, arg)) {
        return true;
    }
    if stem == "busybox" {
        return args.first().is_none_or(|applet| {
            env_depth >= MAX_ENV_DEPTH
                || launch_selects_workspace_code(
                    applet,
                    args.get(1..).unwrap_or_default(),
                    env_depth.saturating_add(1),
                )
        });
    }
    if args.iter().any(|arg| is_inline_eval_flag(&stem, arg)) {
        return true;
    }
    RUNNERS
        .iter()
        .find(|(name, _)| *name == stem)
        .is_some_and(|(_, rule)| match rule {
            LaunchRule::Always => true,
            LaunchRule::Subcommands(subcommands) => {
                args.iter().any(|arg| subcommands.contains(&arg.as_str()))
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
        _ => has_short_flag(&arg, &['c']),
    }
}

/// Whether `arg` is a single-dash cluster of letters holding one of `flags`.
fn has_short_flag(arg: &str, flags: &[char]) -> bool {
    arg.strip_prefix('-').is_some_and(|cluster| {
        !cluster.starts_with('-')
            && cluster.chars().all(char::is_alphabetic)
            && cluster.chars().any(|letter| flags.contains(&letter))
    })
}

/// Whether `arg` makes the interpreter `stem` run a program given inline.
fn is_inline_eval_flag(stem: &str, arg: &str) -> bool {
    INLINE_EVAL
        .iter()
        .filter(|(name, _, _)| stem.starts_with(name))
        .any(|(_, letters, long)| has_short_flag(arg, letters) || long.contains(&arg))
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
}
