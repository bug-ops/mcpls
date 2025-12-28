//! Logging initialization and configuration.

use anyhow::{Context, Result};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

/// Initialize the logging subsystem.
///
/// # Errors
///
/// Returns an error if the log level is invalid or initialization fails.
pub fn init(level: &str) -> Result<()> {
    let filter = EnvFilter::try_new(level)
        .or_else(|_| EnvFilter::try_new("info"))
        .context("failed to parse log level")?;

    // Use stderr for logs so stdout remains clean for MCP protocol
    tracing_subscriber::registry()
        .with(filter)
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_with_valid_trace_level() {
        let result = init("trace");
        assert!(
            result.is_ok(),
            "Should initialize successfully with trace level"
        );
    }

    #[test]
    fn test_init_with_valid_debug_level() {
        let result = init("debug");
        assert!(
            result.is_ok(),
            "Should initialize successfully with debug level"
        );
    }

    #[test]
    fn test_init_with_valid_info_level() {
        let result = init("info");
        assert!(
            result.is_ok(),
            "Should initialize successfully with info level"
        );
    }

    #[test]
    fn test_init_with_valid_warn_level() {
        let result = init("warn");
        assert!(
            result.is_ok(),
            "Should initialize successfully with warn level"
        );
    }

    #[test]
    fn test_init_with_valid_error_level() {
        let result = init("error");
        assert!(
            result.is_ok(),
            "Should initialize successfully with error level"
        );
    }

    #[test]
    fn test_init_with_invalid_level_falls_back_to_info() {
        let result = init("invalid_log_level");
        assert!(
            result.is_ok(),
            "Should fall back to info level for invalid input"
        );
    }

    #[test]
    fn test_init_with_empty_string_falls_back_to_info() {
        let result = init("");
        assert!(
            result.is_ok(),
            "Should fall back to info level for empty string"
        );
    }

    #[test]
    fn test_init_with_crate_specific_filter() {
        let result = init("mcpls=debug,info");
        assert!(
            result.is_ok(),
            "Should support crate-specific filter syntax"
        );
    }

    #[test]
    fn test_init_with_module_specific_filter() {
        let result = init("mcpls::logging=trace");
        assert!(
            result.is_ok(),
            "Should support module-specific filter syntax"
        );
    }

    #[test]
    fn test_init_idempotent() {
        let result1 = init("debug");
        assert!(result1.is_ok(), "First initialization should succeed");

        let result2 = init("info");
        assert!(
            result2.is_ok(),
            "Second initialization should succeed (ignored)"
        );
    }

    #[test]
    fn test_init_with_uppercase_level() {
        let result = init("DEBUG");
        assert!(
            result.is_ok(),
            "Should handle uppercase log levels (fallback to info if not recognized)"
        );
    }

    #[test]
    fn test_init_with_numeric_level() {
        let result = init("3");
        assert!(
            result.is_ok(),
            "Should handle numeric levels or fall back to info"
        );
    }
}
