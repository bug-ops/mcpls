//! Command-line argument parsing.

use std::path::PathBuf;

use clap::Parser;

/// Universal MCP to LSP Bridge
///
/// Exposes Language Server Protocol capabilities as MCP tools,
/// enabling AI agents to access semantic code intelligence.
#[derive(Debug, Parser)]
#[command(name = "mcpls")]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
pub struct Args {
    /// Path to configuration file
    ///
    /// If not specified, searches for mcpls.toml in:
    /// 1. `$MCPLS_CONFIG` environment variable
    /// 2. Current directory
    /// 3. ~/.config/mcpls/mcpls.toml
    #[arg(short, long, value_name = "FILE", env = "MCPLS_CONFIG")]
    pub config: Option<PathBuf>,

    /// Logging level
    ///
    /// Valid values: trace, debug, info, warn, error
    #[arg(short, long, default_value = "info", env = "MCPLS_LOG")]
    pub log_level: String,

    /// Output logs as JSON (for structured logging)
    #[arg(long, default_value = "false", env = "MCPLS_LOG_JSON")]
    pub log_json: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_args() {
        let args = Args::parse_from(["mcpls"]);
        assert!(args.config.is_none());
        assert_eq!(args.log_level, "info");
        assert!(!args.log_json);
    }

    #[test]
    fn test_config_arg() {
        let args = Args::parse_from(["mcpls", "--config", "/path/to/config.toml"]);
        assert_eq!(args.config, Some(PathBuf::from("/path/to/config.toml")));
    }

    #[test]
    fn test_config_short_flag() {
        let args = Args::parse_from(["mcpls", "-c", "/path/to/config.toml"]);
        assert_eq!(
            args.config,
            Some(PathBuf::from("/path/to/config.toml")),
            "Short flag -c should work for config"
        );
    }

    #[test]
    fn test_log_level_arg() {
        let args = Args::parse_from(["mcpls", "--log-level", "debug"]);
        assert_eq!(args.log_level, "debug");
    }

    #[test]
    fn test_log_level_short_flag() {
        let args = Args::parse_from(["mcpls", "-l", "trace"]);
        assert_eq!(
            args.log_level, "trace",
            "Short flag -l should work for log-level"
        );
    }

    #[test]
    fn test_log_level_all_valid_values() {
        let valid_levels = ["trace", "debug", "info", "warn", "error"];

        for level in &valid_levels {
            let args = Args::parse_from(["mcpls", "--log-level", level]);
            assert_eq!(
                args.log_level, *level,
                "Log level {level} should be accepted"
            );
        }
    }

    #[test]
    fn test_log_json_flag() {
        let args = Args::parse_from(["mcpls", "--log-json"]);
        assert!(args.log_json, "Flag --log-json should enable JSON logging");
        assert_eq!(
            args.log_level, "info",
            "Default log level should still be info"
        );
    }

    #[test]
    fn test_log_json_default_false() {
        let args = Args::parse_from(["mcpls"]);
        assert!(!args.log_json, "JSON logging should be disabled by default");
    }

    #[test]
    fn test_all_args_combined() {
        let args = Args::parse_from([
            "mcpls",
            "--config",
            "/custom/config.toml",
            "--log-level",
            "debug",
            "--log-json",
        ]);

        assert_eq!(args.config, Some(PathBuf::from("/custom/config.toml")));
        assert_eq!(args.log_level, "debug");
        assert!(args.log_json);
    }

    #[test]
    fn test_config_with_relative_path() {
        let args = Args::parse_from(["mcpls", "--config", "./mcpls.toml"]);
        assert_eq!(args.config, Some(PathBuf::from("./mcpls.toml")));
    }

    #[test]
    fn test_config_with_home_path() {
        let args = Args::parse_from(["mcpls", "--config", "~/.config/mcpls/mcpls.toml"]);
        assert_eq!(
            args.config,
            Some(PathBuf::from("~/.config/mcpls/mcpls.toml"))
        );
    }

    #[test]
    fn test_log_level_case_sensitive() {
        let args = Args::parse_from(["mcpls", "--log-level", "DEBUG"]);
        assert_eq!(
            args.log_level, "DEBUG",
            "Log level should preserve case (validation happens later)"
        );
    }

    #[test]
    fn test_args_with_mixed_short_long_flags() {
        let args = Args::parse_from([
            "mcpls",
            "-c",
            "/path/to/config.toml",
            "-l",
            "warn",
            "--log-json",
        ]);

        assert_eq!(args.config, Some(PathBuf::from("/path/to/config.toml")));
        assert_eq!(args.log_level, "warn");
        assert!(args.log_json);
    }
}
