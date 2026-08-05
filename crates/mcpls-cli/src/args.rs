//! Command-line argument parsing.

use std::path::PathBuf;

use clap::Parser;

/// Parses a boolean flag/env value, accepting common truthy and falsy
/// spellings beyond the strict `"true"`/`"false"` that `str::parse::<bool>`
/// allows.
///
/// Environment variables rarely follow Rust's `bool` literal syntax, so this
/// parser also accepts (case-insensitively) `1`/`0`, `yes`/`no`, `y`/`n`, and
/// `on`/`off`. Any other value is rejected with a message naming the input.
///
/// The input is not trimmed: a whitespace-padded value (e.g. `" true "`) or
/// an empty string is rejected, not coerced. This matters for
/// `Environment=MCPLS_LOG_JSON=` (systemd) or `-e MCPLS_LOG_JSON=` (Docker)
/// with no value after the `=`, which hard-fails startup rather than being
/// treated as unset.
pub fn parse_bool_flag(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Ok(true),
        "0" | "false" | "no" | "n" | "off" => Ok(false),
        other => Err(format!(
            "invalid boolean value '{other}' (expected one of: 1, 0, true, false, yes, no, y, n, on, off)"
        )),
    }
}

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
    /// 2. Current directory (only loaded with `--trust-project-config`)
    /// 3. Platform config file: `$XDG_CONFIG_HOME/mcpls/mcpls.toml`, else
    ///    `~/.config/mcpls/mcpls.toml` (Linux); `~/Library/Application
    ///    Support/mcpls/mcpls.toml` (macOS); `%APPDATA%\mcpls\mcpls.toml` (Windows)
    #[arg(short, long, value_name = "FILE", env = "MCPLS_CONFIG")]
    pub config: Option<PathBuf>,

    /// Trust and load a `mcpls.toml` found in the current directory.
    ///
    /// A project-local config discovered this way (as opposed to one passed
    /// explicitly via `--config`/`MCPLS_CONFIG`) can control the LSP server
    /// `command`/`args` mcpls spawns, so it is ignored by default to avoid
    /// arbitrary code execution when running mcpls against an untrusted
    /// checkout. Pass this flag only for repositories you trust. Via
    /// `MCPLS_TRUST_PROJECT_CONFIG`, accepted values are `1`/`0`, `true`/
    /// `false`, `yes`/`no`, `y`/`n`, and `on`/`off` (case-insensitive); any
    /// other value is a parse error at startup.
    #[arg(long, env = "MCPLS_TRUST_PROJECT_CONFIG", value_parser = parse_bool_flag)]
    pub trust_project_config: bool,

    /// Logging level
    ///
    /// Valid values: trace, debug, info, warn, error
    #[arg(short, long, default_value = "info", env = "MCPLS_LOG")]
    pub log_level: String,

    /// Output logs as JSON (for structured logging)
    ///
    /// Via `MCPLS_LOG_JSON`, accepted values are `1`/`0`, `true`/`false`,
    /// `yes`/`no`, `y`/`n`, and `on`/`off` (case-insensitive).
    #[arg(long, default_value = "false", env = "MCPLS_LOG_JSON", value_parser = parse_bool_flag)]
    pub log_json: bool,

    /// Listen address for HTTP transport (e.g. 127.0.0.1:3000).
    ///
    /// When set, the MCP server binds this address and serves over Streamable
    /// HTTP instead of stdio. Requires the `transport-http` feature.
    #[cfg(feature = "transport-http")]
    #[arg(long, value_name = "ADDR", env = "MCPLS_LISTEN")]
    pub listen: Option<std::net::SocketAddr>,

    /// URL path the MCP service is mounted at (default `/mcp`).
    ///
    /// Only meaningful when `--listen` is set.
    #[cfg(feature = "transport-http")]
    #[arg(
        long,
        value_name = "PATH",
        default_value = "/mcp",
        env = "MCPLS_HTTP_PATH"
    )]
    pub http_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bool_flag_accepts_truthy_spellings() {
        for value in ["1", "true", "TRUE", "yes", "YES", "y", "Y", "on", "On"] {
            assert_eq!(
                parse_bool_flag(value),
                Ok(true),
                "expected {value:?} to parse as true"
            );
        }
    }

    #[test]
    fn test_parse_bool_flag_accepts_falsy_spellings() {
        for value in ["0", "false", "FALSE", "no", "NO", "n", "N", "off", "Off"] {
            assert_eq!(
                parse_bool_flag(value),
                Ok(false),
                "expected {value:?} to parse as false"
            );
        }
    }

    #[test]
    fn test_parse_bool_flag_rejects_invalid_values() {
        for value in ["banana", "2", "", "truee", "yesno"] {
            assert!(
                parse_bool_flag(value).is_err(),
                "expected {value:?} to be rejected"
            );
        }
    }

    #[test]
    fn test_default_args() {
        let args = Args::parse_from(["mcpls"]);
        assert!(args.config.is_none());
        assert_eq!(args.log_level, "info");
        assert!(!args.log_json);
    }

    #[test]
    fn test_trust_project_config_default_false() {
        let args = Args::parse_from(["mcpls"]);
        assert!(!args.trust_project_config);
    }

    #[test]
    fn test_trust_project_config_flag() {
        let args = Args::parse_from(["mcpls", "--trust-project-config"]);
        assert!(args.trust_project_config);
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

    #[cfg(feature = "transport-http")]
    #[allow(clippy::unwrap_used)]
    mod http_transport_tests {
        use std::net::SocketAddr;

        use super::*;

        #[test]
        fn test_listen_flag_parses_addr() {
            let args = Args::parse_from(["mcpls", "--listen", "127.0.0.1:3000"]);
            let expected: SocketAddr = "127.0.0.1:3000".parse().unwrap();
            assert_eq!(args.listen, Some(expected));
        }

        #[test]
        fn test_listen_default_is_none() {
            let args = Args::parse_from(["mcpls"]);
            assert!(args.listen.is_none());
        }

        #[test]
        fn test_http_path_default() {
            let args = Args::parse_from(["mcpls"]);
            assert_eq!(args.http_path, "/mcp");
        }

        #[test]
        fn test_http_path_custom() {
            let args = Args::parse_from(["mcpls", "--http-path", "/api/mcp"]);
            assert_eq!(args.http_path, "/api/mcp");
        }

        #[test]
        fn test_listen_ipv6() {
            let args = Args::parse_from(["mcpls", "--listen", "[::1]:4000"]);
            let expected: SocketAddr = "[::1]:4000".parse().unwrap();
            assert_eq!(args.listen, Some(expected));
        }
    }
}
