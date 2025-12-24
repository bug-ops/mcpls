//! Logging initialization and configuration.

use anyhow::{Context, Result};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

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
