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

/// Liveness probing of HTTP GET (SSE) streams, selected by
/// `--http-stream-liveness`.
#[cfg(feature = "transport-http")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HttpStreamLiveness {
    /// Ping GET streams and close those whose client stops answering.
    Probe,
    /// Never probe; for clients that ignore server `ping` requests.
    Off,
}

#[cfg(feature = "transport-http")]
impl From<HttpStreamLiveness> for mcpls_core::StreamLiveness {
    fn from(value: HttpStreamLiveness) -> Self {
        match value {
            HttpStreamLiveness::Probe => Self::DEFAULT,
            HttpStreamLiveness::Off => Self::Disabled,
        }
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
    /// Must start with `/`, must not be `/`, and may contain only ASCII
    /// letters, digits and `-._~` in each segment. Validated at startup even
    /// without `--listen`. Only used when `--listen` is set.
    #[cfg(feature = "transport-http")]
    #[arg(
        long,
        value_name = "PATH",
        default_value = "/mcp",
        env = "MCPLS_HTTP_PATH"
    )]
    pub http_path: mcpls_core::HttpPath,

    /// Liveness probing of HTTP GET streams: `probe` or `off`.
    ///
    /// `probe` pings each GET stream with an MCP `ping` request and closes it
    /// when the client stops answering, which frees streams of vanished
    /// peers. Use `off` for a client that ignores server `ping` requests and
    /// would otherwise be disconnected periodically. `off` also disables the
    /// lease (15 to 30 minutes) that ends stateless `subscriptions/listen`
    /// streams so clients re-listen. Only meaningful when `--listen` is set.
    #[cfg(feature = "transport-http")]
    #[arg(
        long,
        value_enum,
        value_name = "MODE",
        default_value_t = HttpStreamLiveness::Probe,
        env = "MCPLS_HTTP_STREAM_LIVENESS"
    )]
    pub http_stream_liveness: HttpStreamLiveness,

    /// Browser origin allowed to reach the HTTP transport, besides loopback
    /// origins on the bound port (repeatable or comma-separated).
    ///
    /// Must be `http://` or `https://` with a host and optional port, and no
    /// path, query or user information; a missing port means the scheme
    /// default. The request's `Host` must be allowed as well; see
    /// `--http-allowed-host`. Only meaningful when `--listen` is set.
    #[cfg(feature = "transport-http")]
    #[arg(
        long = "http-allowed-origin",
        value_name = "ORIGIN",
        env = "MCPLS_HTTP_ALLOWED_ORIGINS",
        value_delimiter = ',',
        value_parser = parse_allowed_origin
    )]
    http_allowed_origins: Vec<Option<mcpls_core::AllowedOrigin>>,

    /// `Host` header value accepted by the HTTP transport, besides `localhost`,
    /// `127.0.0.1`, `::1` and the bound IP address (repeatable or
    /// comma-separated).
    ///
    /// A host name or IP address with an optional port, for example
    /// `mcp.example.com` or `mcp.example.com:8443`; without a port any port
    /// matches, with one a request must send that port. Ports 80, 443 and 0
    /// are rejected: clients omit the default ports, so list the host without
    /// a port. No wildcards, user information, scheme, path, trailing dot or non-ASCII
    /// (use punycode). For deployments
    /// reached through a name (a reverse proxy, a tunnel) or when binding to
    /// `0.0.0.0`; browser origins are allowed separately with
    /// `--http-allowed-origin`. Only meaningful when `--listen` is set.
    #[cfg(feature = "transport-http")]
    #[arg(
        long = "http-allowed-host",
        value_name = "HOST",
        env = "MCPLS_HTTP_ALLOWED_HOSTS",
        value_delimiter = ',',
        value_parser = parse_allowed_host
    )]
    http_allowed_hosts: Vec<Option<mcpls_core::AllowedHost>>,
}

#[cfg(feature = "transport-http")]
impl Args {
    /// The extra allowed `Host` values, without the empty segments an empty
    /// variable, `,,` or a trailing comma leave behind.
    #[must_use]
    pub fn allowed_hosts(&self) -> Vec<mcpls_core::AllowedHost> {
        self.http_allowed_hosts.iter().flatten().cloned().collect()
    }

    /// The extra allowed origins, without the empty segments an empty
    /// variable, `,,` or a trailing comma leave behind.
    #[must_use]
    pub fn allowed_origins(&self) -> Vec<mcpls_core::AllowedOrigin> {
        self.http_allowed_origins
            .iter()
            .flatten()
            .cloned()
            .collect()
    }
}

/// Parses one `--http-allowed-origin` value; a blank one is `None`, so
/// `MCPLS_HTTP_ALLOWED_ORIGINS=` and a trailing comma are no-ops.
#[cfg(feature = "transport-http")]
fn parse_allowed_origin(
    value: &str,
) -> Result<Option<mcpls_core::AllowedOrigin>, mcpls_core::InvalidAllowedOrigin> {
    if value.trim().is_empty() {
        Ok(None)
    } else {
        value.parse().map(Some)
    }
}

/// Parses one `--http-allowed-host` value; a blank one is `None`, so
/// `MCPLS_HTTP_ALLOWED_HOSTS=` and a trailing comma are no-ops.
#[cfg(feature = "transport-http")]
fn parse_allowed_host(
    value: &str,
) -> Result<Option<mcpls_core::AllowedHost>, mcpls_core::InvalidAllowedHost> {
    if value.trim().is_empty() {
        Ok(None)
    } else {
        value.parse().map(Some)
    }
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
            assert_eq!(args.http_path.as_str(), "/mcp");
        }

        #[test]
        fn test_http_path_custom() {
            let args = Args::parse_from(["mcpls", "--http-path", "/api/mcp"]);
            assert_eq!(args.http_path.as_str(), "/api/mcp");
        }

        #[test]
        fn test_http_path_rejected_by_clap() {
            for bad in [
                "/", "mcp", "", "/a//b", "/a/", "/a/../b", "/{id}", "/a*", "/a b",
            ] {
                let err = Args::try_parse_from(["mcpls", "--http-path", bad]).unwrap_err();
                assert_eq!(
                    err.kind(),
                    clap::error::ErrorKind::ValueValidation,
                    "{bad:?}"
                );
            }
        }

        #[test]
        fn test_http_stream_liveness_defaults_to_probe() {
            let args = Args::parse_from(["mcpls"]);
            assert_eq!(args.http_stream_liveness, HttpStreamLiveness::Probe);
            assert_eq!(
                mcpls_core::StreamLiveness::from(args.http_stream_liveness),
                mcpls_core::StreamLiveness::DEFAULT
            );
        }

        #[test]
        fn test_http_stream_liveness_off() {
            let args = Args::parse_from(["mcpls", "--http-stream-liveness", "off"]);
            assert_eq!(args.http_stream_liveness, HttpStreamLiveness::Off);
            assert_eq!(
                mcpls_core::StreamLiveness::from(args.http_stream_liveness),
                mcpls_core::StreamLiveness::Disabled
            );
        }

        #[test]
        fn test_http_stream_liveness_rejects_unknown_mode() {
            assert!(
                Args::try_parse_from(["mcpls", "--http-stream-liveness", "sometimes"]).is_err()
            );
        }

        #[test]
        fn test_http_allowed_origin_defaults_to_none() {
            let args = Args::parse_from(["mcpls"]);
            assert!(args.allowed_origins().is_empty());
        }

        #[test]
        fn test_http_allowed_origin_ignores_empty_segments() {
            for value in [
                "",
                ",,",
                " , ",
                ",https://a.example.com,",
                "https://a.example.com,",
            ] {
                let args = Args::try_parse_from(["mcpls", "--http-allowed-origin", value])
                    .unwrap_or_else(|e| panic!("{value:?} rejected: {e}"));
                let expected = usize::from(value.contains("a.example.com"));
                assert_eq!(args.allowed_origins().len(), expected, "{value:?}");
            }
        }

        #[test]
        fn test_http_allowed_origin_still_rejects_a_non_empty_invalid_segment() {
            for value in [
                "https://a.example.com,*",
                "ftp://x,",
                ",https://a.example.com/path",
            ] {
                assert!(
                    Args::try_parse_from(["mcpls", "--http-allowed-origin", value]).is_err(),
                    "{value:?}"
                );
            }
        }

        #[test]
        fn test_http_allowed_origin_repeats_and_splits_on_commas() {
            let args = Args::parse_from([
                "mcpls",
                "--http-allowed-origin",
                "https://a.example.com",
                "--http-allowed-origin",
                "http://b.example.com:8080, https://C.example.com",
            ]);
            let origins: Vec<String> = args
                .allowed_origins()
                .iter()
                .map(ToString::to_string)
                .collect();
            assert_eq!(
                origins,
                [
                    "https://a.example.com:443",
                    "http://b.example.com:8080",
                    "https://c.example.com:443"
                ]
            );
        }

        #[test]
        fn test_http_allowed_origin_rejected_by_clap() {
            for bad in [
                "*",
                "null",
                "ftp://a.example.com",
                "https://a.example.com/x",
            ] {
                let err =
                    Args::try_parse_from(["mcpls", "--http-allowed-origin", bad]).unwrap_err();
                assert_eq!(
                    err.kind(),
                    clap::error::ErrorKind::ValueValidation,
                    "{bad:?}"
                );
            }
        }

        #[test]
        fn test_http_allowed_host_defaults_to_none() {
            let args = Args::parse_from(["mcpls"]);
            assert!(args.allowed_hosts().is_empty());
        }

        #[test]
        fn test_http_allowed_host_ignores_empty_segments() {
            for value in ["", ",,", " , ", ",a.example.com,", "a.example.com,"] {
                let args = Args::try_parse_from(["mcpls", "--http-allowed-host", value])
                    .unwrap_or_else(|e| panic!("{value:?} rejected: {e}"));
                let expected = usize::from(value.contains("a.example.com"));
                assert_eq!(args.allowed_hosts().len(), expected, "{value:?}");
            }
        }

        #[test]
        fn test_http_allowed_host_repeats_and_splits_on_commas() {
            let args = Args::parse_from([
                "mcpls",
                "--http-allowed-host",
                "a.example.com",
                "--http-allowed-host",
                "B.example.com:8443, [::1]:9000",
            ]);
            let hosts: Vec<String> = args
                .allowed_hosts()
                .iter()
                .map(ToString::to_string)
                .collect();
            assert_eq!(hosts, ["a.example.com", "b.example.com:8443", "[::1]:9000"]);
        }

        #[test]
        fn test_http_allowed_host_rejected_by_clap() {
            for bad in [
                "*",
                "*.example.com",
                "user@example.com",
                "https://a.example.com",
                "a.example.com:",
                "a.example.com:99999",
                "a.example.com,*",
                "x.example:443",
                "[::1]:443",
                "1.2.3.4:80",
                "x.example:0",
                "x.example:0443",
                "a.example,b.example:443",
            ] {
                let err = Args::try_parse_from(["mcpls", "--http-allowed-host", bad]).unwrap_err();
                assert_eq!(
                    err.kind(),
                    clap::error::ErrorKind::ValueValidation,
                    "{bad:?}"
                );
            }
        }

        #[test]
        fn test_listen_ipv6() {
            let args = Args::parse_from(["mcpls", "--listen", "[::1]:4000"]);
            let expected: SocketAddr = "[::1]:4000".parse().unwrap();
            assert_eq!(args.listen, Some(expected));
        }
    }
}
