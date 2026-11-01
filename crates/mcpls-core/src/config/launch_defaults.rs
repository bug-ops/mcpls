//! Launch arguments mcpls appends for servers whose defaults hide results.
//!
//! clangd cuts references, workspace symbols and completions at built-in
//! limits and writes a stderr line per request. mcpls cannot see those
//! cut-offs, so a partial answer would read as complete. For a clangd launch
//! it appends lifted limits (mcpls then applies its own flagged caps) and a
//! quiet log level, but never an option the user already wrote.

use std::fmt;
use std::num::NonZeroUsize;

use super::{BuiltinServer, LspServerConfig};
use crate::bridge::MAX_NORMALIZED_LOCATIONS;
use crate::spelling::spelled_enum;

/// Extra references clangd must be allowed beyond the mcpls cap, so a cut at
/// the cap stays visible.
///
/// Measured with clangd 21.0.0 (Apple build): `--limit-references=N` returns
/// `N - 1` locations with `includeDeclaration: false` and `N` with `true`
/// (deficit at most 1), so the margin is that deficit plus 2.
const REFERENCES_MARGIN: usize = 3;

const fn nonzero(count: usize) -> NonZeroUsize {
    match NonZeroUsize::new(count) {
        Some(count) => count,
        None => panic!("a launch limit must be non-zero"),
    }
}

spelled_enum! {
    /// A clangd command-line option mcpls may append, without its dashes.
    LaunchOption {
        /// Stops collecting `textDocument/references` at this many results.
        LimitReferences => "limit-references",
        /// Caps workspace symbols and completion items.
        LimitResults => "limit-results",
        /// Verbosity of clangd's stderr log.
        Log => "log",
    }
}

/// The value an appended option carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchValue {
    Count(NonZeroUsize),
    /// `--log=error`: the quietest level.
    LogError,
}

impl fmt::Display for LaunchValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count(count) => write!(f, "{count}"),
            Self::LogError => f.write_str("error"),
        }
    }
}

impl LaunchOption {
    /// The value mcpls appends when the user wrote none.
    const fn default_value(self) -> LaunchValue {
        match self {
            Self::LimitReferences => {
                LaunchValue::Count(nonzero(MAX_NORMALIZED_LOCATIONS + REFERENCES_MARGIN))
            }
            Self::LimitResults => LaunchValue::Count(nonzero(MAX_NORMALIZED_LOCATIONS + 1)),
            Self::Log => LaunchValue::LogError,
        }
    }
}

/// One option mcpls appends to a launch, rendered as `--name=value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendedOption {
    option: LaunchOption,
    value: LaunchValue,
}

impl AppendedOption {
    /// The option's name, for logs that must not show other arguments.
    pub const fn name(&self) -> LaunchOption {
        self.option
    }
}

impl fmt::Display for AppendedOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "--{}={}", self.option, self.value)
    }
}

/// The option names a user wrote in `args`, in every spelling clangd accepts.
struct WrittenOptions<'a> {
    response_file: bool,
    names: Vec<&'a str>,
}

impl<'a> WrittenOptions<'a> {
    fn of(args: &'a [String]) -> Self {
        let response_file = args.iter().any(|arg| arg.starts_with('@'));
        let names = args
            .iter()
            .filter_map(|arg| {
                let flag = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-'))?;
                Some(flag.split_once('=').map_or(flag, |(name, _)| name))
            })
            .collect();
        Self {
            response_file,
            names,
        }
    }

    /// A response file may hold any option, so it counts as supplying all.
    fn supplies(&self, option: LaunchOption) -> bool {
        self.response_file || self.names.contains(&option.as_str())
    }
}

/// The options to append after `config.args` when launching `config`.
///
/// Empty unless the command is clangd (by file stem; wrappers and versioned
/// names such as `clangd-18` get nothing). An option the user already wrote
/// as `--name`, `-name`, `--name=v` or `-name=v` is never repeated, because
/// clangd before LLVM 15 rejects a repeated option; an `@file` argument
/// suppresses every default.
pub fn appended(config: &LspServerConfig) -> Vec<AppendedOption> {
    if !BuiltinServer::Clangd.matches_command(config.command.as_str()) {
        return Vec::new();
    }
    let written = WrittenOptions::of(&config.args);
    LaunchOption::ALL
        .into_iter()
        .filter(|option| !written.supplies(*option))
        .map(|option| AppendedOption {
            option,
            value: option.default_value(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ServerArgs, ServerCommand};

    fn clangd_with(command: &str, args: &[&str]) -> LspServerConfig {
        let mut config = LspServerConfig::clangd();
        config.command = ServerCommand::new(command).unwrap().into();
        config.args = ServerArgs::new(args.iter().copied()).unwrap();
        config
    }

    fn rendered(config: &LspServerConfig) -> Vec<String> {
        appended(config).iter().map(ToString::to_string).collect()
    }

    #[test]
    fn test_default_clangd_launch_appends_all_three_options() {
        assert_eq!(
            rendered(&LspServerConfig::clangd()),
            [
                format!("--limit-references={}", MAX_NORMALIZED_LOCATIONS + 3),
                format!("--limit-results={}", MAX_NORMALIZED_LOCATIONS + 1),
                "--log=error".to_owned(),
            ]
        );
    }

    #[test]
    fn test_limits_leave_room_to_report_truncation_past_the_cap() {
        const {
            assert!(REFERENCES_MARGIN >= 2);
        }
        let LaunchValue::Count(references) = LaunchOption::LimitReferences.default_value() else {
            unreachable!("limit-references carries a count");
        };
        let LaunchValue::Count(results) = LaunchOption::LimitResults.default_value() else {
            unreachable!("limit-results carries a count");
        };
        assert!(references.get() - 1 > MAX_NORMALIZED_LOCATIONS);
        assert!(results.get() > MAX_NORMALIZED_LOCATIONS);
    }

    #[test]
    fn test_every_spelling_of_a_written_option_suppresses_it() {
        for option in LaunchOption::ALL {
            let name = option.as_str();
            for written in [
                vec![format!("--{name}=5")],
                vec![format!("-{name}=5")],
                vec![format!("--{name}"), "5".to_owned()],
                vec![format!("-{name}"), "5".to_owned()],
            ] {
                let args: Vec<&str> = written.iter().map(String::as_str).collect();
                let config = clangd_with("clangd", &args);
                assert!(
                    appended(&config)
                        .iter()
                        .all(|appended| appended.name() != option),
                    "{written:?}"
                );
                assert_eq!(appended(&config).len(), LaunchOption::ALL.len() - 1);
            }
        }
    }

    #[test]
    fn test_near_miss_names_and_values_do_not_suppress() {
        let config = clangd_with(
            "clangd",
            &[
                "--limit-references-extra=1",
                "--logger",
                "--limit-results",
                "limit-results",
            ],
        );
        let names: Vec<_> = appended(&config).iter().map(AppendedOption::name).collect();
        assert_eq!(names, [LaunchOption::LimitReferences, LaunchOption::Log]);
    }

    #[test]
    fn test_response_file_suppresses_every_default() {
        assert_eq!(appended(&clangd_with("clangd", &["@opts.rsp"])), []);
    }

    #[test]
    fn test_only_a_clangd_command_gets_defaults() {
        for command in ["gopls", "clangd-18", "npx", "rust-analyzer"] {
            assert!(appended(&clangd_with(command, &[])).is_empty(), "{command}");
        }
        for command in ["/usr/bin/clangd", "clangd.exe", "CLANGD"] {
            assert_eq!(appended(&clangd_with(command, &[])).len(), 3, "{command}");
        }
    }
}
