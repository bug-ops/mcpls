//! Command-line argument parsing.

use clap::Parser;
use std::path::PathBuf;

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
    /// 1. $MCPLS_CONFIG environment variable
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
    fn test_log_level_arg() {
        let args = Args::parse_from(["mcpls", "--log-level", "debug"]);
        assert_eq!(args.log_level, "debug");
    }
}
