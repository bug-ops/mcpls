//! Launchers that choose the server to run from files in the workspace.
//!
//! A package runner (`npx`), task runner (`make`) or toolchain wrapper
//! (`cargo run`) takes a command that lies outside the workspace and starts a
//! program the workspace selects: `./node_modules/.bin/<name>`, a `Makefile`
//! target, a `build.rs`. Untrusted-workspace mode cannot vet that program, so
//! it refuses these launches ([`launches_from_workspace`]).
//!
//! The rules are best-effort. They match the command's file stem and its
//! arguments, unwrap `env`, and give up on what cannot be analyzed (`env -S`,
//! a shell with `-c`). The trusted configuration is the boundary, not this list.

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

const NPM_SPECIFIER_PREFIX: &str = "npm:";

/// Most `env` wrappers followed before the launch is treated as unanalyzable.
const MAX_ENV_DEPTH: usize = 8;

/// `env` options that take a separate value.
const ENV_OPTIONS_WITH_VALUE: &[&str] = &["-u", "--unset", "-C", "--chdir"];

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
    if SHELLS.contains(&stem.as_str()) && args.iter().any(|arg| is_command_flag(arg)) {
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

/// Whether a shell argument introduces a command string (`-c`, `-lc`, `/c`,
/// `-Command`), which cannot be analyzed.
fn is_command_flag(arg: &str) -> bool {
    let arg = arg.to_ascii_lowercase();
    if arg == "/c" || arg == "/k" {
        return true;
    }
    arg.strip_prefix('-').is_some_and(|flags| {
        !flags.starts_with('-') && flags.chars().all(char::is_alphabetic) && flags.contains('c')
    })
}

/// Whether the command an `env` invocation starts, unwrapped, selects workspace
/// code. `env -S` splits a string into arguments and is refused as
/// unanalyzable, as is a chain of more than [`MAX_ENV_DEPTH`] wrappers.
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
        if arg.starts_with("-S") || arg.starts_with("--split-string") {
            return true;
        }
        if arg.starts_with('-') {
            if ENV_OPTIONS_WITH_VALUE.contains(&arg.as_str()) {
                rest.next();
            }
            continue;
        }
        if arg.contains('=') {
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
    fn shells_with_a_command_string_are_unanalyzable() {
        assert!(launches("sh", &["-c", "rust-analyzer"]));
        assert!(launches("/bin/bash", &["-lc", "rust-analyzer"]));
        assert!(launches("cmd.exe", &["/C", "server"]));
        assert!(launches("powershell", &["-Command", "server"]));
        assert!(!launches("sh", &["server.sh"]));
        assert!(!launches("bash", &["--norc", "server.sh"]));
    }
}
