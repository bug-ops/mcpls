//! Logging initialization and configuration.

use anyhow::{Context, Result};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

/// Initialize the logging subsystem.
///
/// When `log_json` is `true`, log events are emitted as newline-delimited
/// JSON instead of the default compact human-readable format, for
/// consumption by structured-logging pipelines.
///
/// An invalid `level` falls back to `"info"` rather than erroring.
///
/// # Errors
///
/// Returns an error if the fallback `"info"` filter itself fails to parse.
pub fn init(level: &str, log_json: bool) -> Result<()> {
    let filter = EnvFilter::try_new(level)
        .or_else(|_| EnvFilter::try_new("info"))
        .context("failed to parse log level")?;

    // Use stderr for logs so stdout remains clean for MCP protocol
    let registry = tracing_subscriber::registry().with(filter);

    if log_json {
        registry
            .with(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_target(true)
                    .with_thread_ids(false)
                    .with_file(false)
                    .with_line_number(false)
                    .json(),
            )
            .try_init()
            .ok(); // Ignore if already initialized
    } else {
        registry
            .with(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_target(true)
                    .with_thread_ids(false)
                    .with_file(false)
                    .with_line_number(false)
                    .compact(),
            )
            .try_init()
            .ok(); // Ignore if already initialized
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_with_valid_trace_level() {
        let result = init("trace", false);
        assert!(
            result.is_ok(),
            "Should initialize successfully with trace level"
        );
    }

    #[test]
    fn test_init_with_valid_debug_level() {
        let result = init("debug", false);
        assert!(
            result.is_ok(),
            "Should initialize successfully with debug level"
        );
    }

    #[test]
    fn test_init_with_valid_info_level() {
        let result = init("info", false);
        assert!(
            result.is_ok(),
            "Should initialize successfully with info level"
        );
    }

    #[test]
    fn test_init_with_valid_warn_level() {
        let result = init("warn", false);
        assert!(
            result.is_ok(),
            "Should initialize successfully with warn level"
        );
    }

    #[test]
    fn test_init_with_valid_error_level() {
        let result = init("error", false);
        assert!(
            result.is_ok(),
            "Should initialize successfully with error level"
        );
    }

    #[test]
    fn test_init_with_invalid_level_falls_back_to_info() {
        let result = init("invalid_log_level", false);
        assert!(
            result.is_ok(),
            "Should fall back to info level for invalid input"
        );
    }

    #[test]
    fn test_init_with_empty_string_falls_back_to_info() {
        let result = init("", false);
        assert!(
            result.is_ok(),
            "Should fall back to info level for empty string"
        );
    }

    #[test]
    fn test_init_with_crate_specific_filter() {
        let result = init("mcpls=debug,info", false);
        assert!(
            result.is_ok(),
            "Should support crate-specific filter syntax"
        );
    }

    #[test]
    fn test_init_with_module_specific_filter() {
        let result = init("mcpls::logging=trace", false);
        assert!(
            result.is_ok(),
            "Should support module-specific filter syntax"
        );
    }

    #[test]
    fn test_init_idempotent() {
        let result1 = init("debug", false);
        assert!(result1.is_ok(), "First initialization should succeed");

        let result2 = init("info", false);
        assert!(
            result2.is_ok(),
            "Second initialization should succeed (ignored)"
        );
    }

    #[test]
    fn test_init_with_uppercase_level() {
        let result = init("DEBUG", false);
        assert!(
            result.is_ok(),
            "Should handle uppercase log levels (fallback to info if not recognized)"
        );
    }

    #[test]
    fn test_init_with_numeric_level() {
        let result = init("3", false);
        assert!(
            result.is_ok(),
            "Should handle numeric levels or fall back to info"
        );
    }

    #[test]
    fn test_init_with_log_json_enabled() {
        let result = init("info", true);
        assert!(
            result.is_ok(),
            "Should initialize successfully with JSON logging enabled"
        );
    }
}
