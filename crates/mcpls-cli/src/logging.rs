//! Logging initialization and configuration.

use anyhow::{Context, Result};
use mcpls_core::escape_control;
use tracing::field::{Field, Visit};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::filter::Directive;
use tracing_subscriber::fmt::format::{FormatFields, Writer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

/// Directives appended after the user's level so rmcp's session-creation
/// event and worker span, which carry the secret `Mcp-Session-Id`, stay
/// below the default level. A more specific user directive still overrides.
const SESSION_ID_LOG_CAPS: [&str; 2] = [
    "rmcp::transport::streamable_http_server::session=warn",
    "rmcp::transport::worker=debug",
];

fn build_filter(level: &str) -> Result<EnvFilter> {
    let filter = EnvFilter::try_new(level)
        .or_else(|_| EnvFilter::try_new("info"))
        .context("failed to parse log level")?;
    SESSION_ID_LOG_CAPS.iter().try_fold(filter, |filter, cap| {
        let directive: Directive = cap
            .parse()
            .with_context(|| format!("invalid session log cap `{cap}`"))?;
        Ok(filter.add_directive(directive))
    })
}

/// Text-mode field formatter that control-escapes every field value, so
/// attacker-influenceable text (an LSP server's message or method name, a
/// client URI, a file name) cannot forge log lines or inject terminal
/// escapes. JSON mode already escapes control characters.
struct EscapingFields;

impl<'writer> FormatFields<'writer> for EscapingFields {
    fn format_fields<R: RecordFields>(
        &self,
        writer: Writer<'writer>,
        fields: R,
    ) -> std::fmt::Result {
        let mut visitor = EscapingVisitor {
            writer,
            first: true,
            result: Ok(()),
        };
        fields.record(&mut visitor);
        visitor.result
    }
}

struct EscapingVisitor<'writer> {
    writer: Writer<'writer>,
    first: bool,
    result: std::fmt::Result,
}

impl Visit for EscapingVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if self.result.is_err() {
            return;
        }
        let rendered = format!("{value:?}");
        let escaped = escape_control(&rendered);
        let separator = if self.first { "" } else { " " };
        self.first = false;
        self.result = if field.name() == "message" {
            write!(self.writer, "{separator}{escaped}")
        } else {
            write!(self.writer, "{separator}{}={escaped}", field.name())
        };
    }
}

/// Initialize the logging subsystem.
///
/// When `log_json` is `true`, log events are emitted as newline-delimited
/// JSON instead of the default compact human-readable format, for
/// consumption by structured-logging pipelines.
///
/// An invalid `level` falls back to `"info"` rather than erroring.
///
/// rmcp's session-id-bearing log targets are capped regardless of `level`; a
/// more specific directive (for example `rmcp::...::session::local=info`)
/// overrides the cap and re-exposes session ids.
///
/// # Errors
///
/// Returns an error if the fallback `"info"` filter itself fails to parse.
pub fn init(level: &str, log_json: bool) -> Result<()> {
    let filter = build_filter(level)?;

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
                    .fmt_fields(EscapingFields)
                    .compact(),
            )
            .try_init()
            .ok(); // Ignore if already initialized
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_text_logs_escape_control_characters_in_every_field() {
        let buf = SharedBuf::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .fmt_fields(EscapingFields)
                .compact(),
        );

        tracing::subscriber::with_default(subscriber, || {
            let uri = "file:///a\nERROR forged\u{1b}[31m";
            tracing::warn!("bad uri {uri} for {}", "x\ry");
            tracing::warn!(method = %uri, "dropped");
        });

        let output = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(output.lines().count(), 2, "got {output:?}");
        assert!(!output.contains('\u{1b}'), "got {output:?}");
        assert!(
            output.contains("\\nERROR forged\\u{1b}[31m"),
            "got {output:?}"
        );
        assert!(
            output.contains("method=file:///a\\nERROR"),
            "got {output:?}"
        );
    }

    #[test]
    fn test_session_id_never_logged_by_rmcp_targets_at_trace_level() {
        let buf = SharedBuf::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::registry()
            .with(build_filter("trace").unwrap())
            .with(
                fmt::layer()
                    .with_writer(move || writer.clone())
                    .with_ansi(false),
            );

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                target: "rmcp::transport::streamable_http_server::session::local",
                session_id = "SECRET-SESSION-ID",
                "create new session"
            );
            let span = tracing::trace_span!(
                target: "rmcp::transport::worker",
                "transport_worker",
                name = "streamable-http-session-SECRET-SESSION-ID"
            );
            let _entered = span.enter();
            tracing::trace!(target: "rmcp::transport::worker", "inside worker");
            tracing::info!(target: "mcpls_core", "kept");
        });

        let output = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(!output.contains("SECRET-SESSION-ID"), "got {output:?}");
        assert!(output.contains("kept"), "got {output:?}");
    }

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
