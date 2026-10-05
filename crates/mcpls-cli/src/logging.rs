//! Logging initialization and configuration.

use std::borrow::Cow;
use std::str::FromStr;

use anyhow::{Context, Result};
use mcpls_core::{escape_control, needs_control_escape};
use thiserror::Error;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::filter::{Directive, LevelFilter};
use tracing_subscriber::fmt::format::{FormatFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, fmt};

/// Directives appended after the user's level so rmcp's session-creation
/// event and worker span, which carry the secret `Mcp-Session-Id`, stay
/// below the default level. A more specific user directive still overrides.
const SESSION_ID_LOG_CAPS: [&str; 2] = [
    "rmcp::transport::streamable_http_server::session=warn",
    "rmcp::transport::worker=debug",
];

/// A validated `--log-level` / `MCPLS_LOG` value.
///
/// Accepts the `tracing` filter syntax, but a bare comma-separated word must
/// be a level (`trace`, `debug`, `info`, `warn`, `error`, `off`, any case):
/// `EnvFilter` would otherwise read a misspelt level as a target name and
/// silently disable every other log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFilter(String);

/// Why a log filter string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidLogFilter {
    /// The string is empty.
    #[error("log filter must not be empty")]
    Empty,
    /// A bare word that is not a level.
    #[error(
        "invalid log level '{0}' (expected one of: trace, debug, info, warn, error, off, or a `target=level` directive)"
    )]
    NotALevel(String),
    /// The `tracing` filter syntax is malformed.
    #[error("invalid log filter: {0}")]
    Syntax(String),
}

impl LogFilter {
    /// The filter text, already validated.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for LogFilter {
    type Err = InvalidLogFilter;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(InvalidLogFilter::Empty);
        }
        EnvFilter::builder()
            .parse(s)
            .map_err(|err| InvalidLogFilter::Syntax(err.to_string()))?;
        let bare_non_level = s.split(',').map(str::trim).find(|directive| {
            !directive.contains(['=', '[', ']', '{', '}'])
                && directive.parse::<LevelFilter>().is_err()
        });
        if let Some(word) = bare_non_level {
            return Err(InvalidLogFilter::NotALevel(word.to_owned()));
        }
        Ok(Self(s.to_owned()))
    }
}

fn build_filter(level: &LogFilter) -> Result<EnvFilter> {
    let filter = EnvFilter::builder()
        .parse(level.as_str())
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
/// escapes. JSON mode does the same through [`EscapingJson`].
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

/// Line written instead of a JSON log line whose text cannot be escaped.
const SUPPRESSED_LINE: &str =
    "{\"level\":\"ERROR\",\"fields\":{\"message\":\"log line suppressed: unescapable field\"}}\n";

/// JSON-mode event formatter that control-escapes every string, so JSON
/// output is as safe as text mode: serde escapes only C0 controls, leaving
/// C1 controls, line separators and bidi marks raw.
struct EscapingJson<F>(F);

impl<S, N, F> FormatEvent<S, N> for EscapingJson<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let mut line = String::new();
        self.0.format_event(ctx, Writer::new(&mut line), event)?;
        let escaped = escape_json_strings(&line);
        writer.write_str(escaped.as_deref().unwrap_or(SUPPRESSED_LINE))
    }
}

/// Rewrites the string literals of one JSON log line that hold a backslash
/// or a character [`needs_control_escape`] flags: each is decoded, escaped
/// with [`escape_control`] and re-encoded. Every other byte, including key
/// order, is copied as is. `None` when a literal cannot be decoded.
fn escape_json_strings(line: &str) -> Option<Cow<'_, str>> {
    // The line terminator is the formatter's own, not text a server controls.
    let body = line.strip_suffix('\n').unwrap_or(line);
    if !body.contains('\\') && !body.chars().any(needs_control_escape) {
        return Some(Cow::Borrowed(line));
    }
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find('"') {
        out.push_str(&rest[..start]);
        let end = start.checked_add(json_literal_len(&rest[start..])?)?;
        out.push_str(&escape_json_literal(&rest[start..end])?);
        rest = &rest[end..];
    }
    out.push_str(rest);
    Some(Cow::Owned(out))
}

/// Byte length of the string literal at the start of `text`, quotes included.
fn json_literal_len(text: &str) -> Option<usize> {
    let mut bytes = text.bytes().enumerate().skip(1);
    while let Some((index, byte)) = bytes.next() {
        match byte {
            b'\\' => {
                bytes.next();
            }
            b'"' => return index.checked_add(1),
            _ => {}
        }
    }
    None
}

/// `literal` (quotes included) with its text control-escaped; borrowed when
/// escaping changes nothing, so the original bytes survive.
fn escape_json_literal(literal: &str) -> Option<Cow<'_, str>> {
    let inner = literal.get(1..literal.len().checked_sub(1)?)?;
    if !inner.contains('\\') && !inner.chars().any(needs_control_escape) {
        return Some(Cow::Borrowed(literal));
    }
    let decoded: String = serde_json::from_str(literal).ok()?;
    match escape_control(&decoded) {
        Cow::Borrowed(_) => Some(Cow::Borrowed(literal)),
        Cow::Owned(escaped) => serde_json::to_string(&escaped).ok().map(Cow::Owned),
    }
}

fn json_format() -> fmt::format::Format<fmt::format::Json> {
    fmt::format()
        .json()
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
}

/// Initialize the logging subsystem.
///
/// When `log_json` is `true`, log events are emitted as newline-delimited
/// JSON instead of the default compact human-readable format, for
/// consumption by structured-logging pipelines.
///
/// rmcp's session-id-bearing log targets are capped regardless of `level`; a
/// more specific directive (for example `rmcp::...::session::local=info`)
/// overrides the cap and re-exposes session ids.
///
/// # Errors
///
/// Returns an error if the filter or one of the built-in session-id caps fails
/// to parse.
pub fn init(level: &LogFilter, log_json: bool) -> Result<()> {
    let filter = build_filter(level)?;

    // Use stderr for logs so stdout remains clean for MCP protocol
    let registry = tracing_subscriber::registry().with(filter);

    if log_json {
        registry
            .with(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .json()
                    .event_format(EscapingJson(json_format())),
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
    use std::assert_matches;
    use std::sync::{Arc, Mutex};

    use super::*;

    fn filter(text: &str) -> LogFilter {
        text.parse().unwrap()
    }

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

    fn logs_with<F>(format: F, emit: impl FnOnce()) -> String
    where
        F: FormatEvent<tracing_subscriber::Registry, fmt::format::JsonFields>
            + Send
            + Sync
            + 'static,
    {
        let buf = SharedBuf::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .with_writer(move || writer.clone())
                .json()
                .event_format(format),
        );
        tracing::subscriber::with_default(subscriber, emit);
        String::from_utf8(buf.0.lock().unwrap().clone()).unwrap()
    }

    fn json_logs(emit: impl FnOnce()) -> String {
        logs_with(EscapingJson(json_format()), emit)
    }

    /// Emits a line holding an invalid JSON escape instead of an event.
    struct InvalidEscape;

    impl<S, N> FormatEvent<S, N> for InvalidEscape
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
        N: for<'a> FormatFields<'a> + 'static,
    {
        fn format_event(
            &self,
            _: &FmtContext<'_, S, N>,
            mut writer: Writer<'_>,
            _: &Event<'_>,
        ) -> std::fmt::Result {
            writer.write_str("{\"a\":\"bad \\q escape\"}\n")
        }
    }

    #[test]
    fn test_json_logs_write_the_fixed_line_when_a_literal_cannot_be_decoded() {
        let output = logs_with(EscapingJson(InvalidEscape), || tracing::info!("x"));

        assert_eq!(output, SUPPRESSED_LINE);
        assert!(output.ends_with('\n'));
        serde_json::from_str::<serde_json::Value>(&output).unwrap();
    }

    /// Debug fields (`\"`) and Windows paths (`\\`) take the decode path but
    /// need no escaping, so the line is identical to the unwrapped formatter's.
    #[test]
    fn test_json_logs_keep_literals_with_only_quotes_and_backslashes_unchanged() {
        let emit = || {
            tracing::info!(path = ?r"C:\Users\dev\x.rs", quoted = %"say \"hi\"", "msg \"q\"");
        };
        let plain = logs_with(json_format().without_time(), emit);
        let wrapped = logs_with(EscapingJson(json_format().without_time()), emit);

        assert_eq!(wrapped, plain);
        assert!(wrapped.contains(r"\\"), "{wrapped}");
    }

    #[test]
    fn test_escape_json_strings_borrows_a_clean_line_with_its_newline() {
        assert_matches!(
            escape_json_strings("{\"a\":\"clean\"}\n"),
            Some(Cow::Borrowed(_))
        );
        assert_matches!(
            escape_json_strings("{\"a\":\"say \\\"hi\\\"\"}\n"),
            Some(Cow::Owned(_))
        );
    }

    #[test]
    fn test_json_logs_escape_control_characters_in_event_and_span_fields() {
        let hostile = "a\nb\u{202E}c\u{85}d\u{E0001}e\"q\\p";
        let output = json_logs(|| {
            let span = tracing::info_span!("work", file = hostile);
            let _entered = span.enter();
            tracing::warn!(method = hostile, "bad {hostile}");
        });

        assert_eq!(output.lines().count(), 1, "got {output:?}");
        for raw in ['\u{202E}', '\u{85}', '\u{E0001}'] {
            assert!(!output.contains(raw), "{raw:?} in {output:?}");
        }
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        let expected = "a\\nb\\u{202e}c\\u{85}d\\u{e0001}e\"q\\p";
        assert_eq!(value["fields"]["method"], expected);
        assert_eq!(value["fields"]["message"], format!("bad {expected}"));
        assert_eq!(value["span"]["file"], expected);
        assert_eq!(value["spans"][0]["file"], expected);
    }

    #[test]
    fn test_json_logs_keep_clean_lines_and_key_order_stable() {
        let clean = json_logs(|| tracing::info!(method = "plain", "hello"));
        let escaped = json_logs(|| tracing::info!(method = "pl\nain", "hello"));

        let key_order = |line: &str| {
            let mut found: Vec<_> = [
                "timestamp",
                "level",
                "fields",
                "target",
                "method",
                "message",
            ]
            .into_iter()
            .filter_map(|key| line.find(&format!("\"{key}\":")).map(|at| (at, key)))
            .collect();
            found.sort_unstable();
            found.into_iter().map(|(_, key)| key).collect::<Vec<_>>()
        };
        assert_eq!(key_order(&clean), key_order(&escaped));
        assert_eq!(key_order(&clean).len(), 6, "got {clean:?}");
        assert!(clean.contains("\"method\":\"plain\""), "got {clean:?}");
    }

    #[test]
    fn test_escape_json_strings_fails_closed_on_unterminated_literal() {
        assert_eq!(escape_json_strings("{\"a\\\\b"), None);
        assert_eq!(
            escape_json_strings("{\"a\":\"x\\ny\"}\n").as_deref(),
            Some("{\"a\":\"x\\\\ny\"}\n")
        );
    }

    #[test]
    fn test_session_id_never_logged_by_rmcp_targets_at_trace_level() {
        let buf = SharedBuf::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::registry()
            .with(build_filter(&filter("trace")).unwrap())
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
    fn test_log_filter_rejects_misspelt_level_and_empty() {
        assert_eq!(
            "debgu".parse::<LogFilter>(),
            Err(InvalidLogFilter::NotALevel("debgu".into()))
        );
        assert_eq!(
            "mcpls=debug,warnn".parse::<LogFilter>(),
            Err(InvalidLogFilter::NotALevel("warnn".into()))
        );
        assert_eq!("".parse::<LogFilter>(), Err(InvalidLogFilter::Empty));
        assert_matches!(
            "foo=bar=baz".parse::<LogFilter>(),
            Err(InvalidLogFilter::Syntax(_))
        );
    }

    #[test]
    fn test_log_filter_accepts_levels_and_directives() {
        for accepted in [
            "DEBUG",
            "off",
            "3",
            "mcpls_core=debug,info",
            "info,rmcp::transport::streamable_http_server::session::local=info",
            "mcpls[span]=debug",
        ] {
            assert!(accepted.parse::<LogFilter>().is_ok(), "{accepted}");
        }
    }

    #[test]
    fn test_init_with_crate_specific_filter() {
        let result = init(&filter("mcpls=debug,info"), false);
        assert!(
            result.is_ok(),
            "Should support crate-specific filter syntax"
        );
    }

    #[test]
    fn test_init_with_module_specific_filter() {
        let result = init(&filter("mcpls::logging=trace"), false);
        assert!(
            result.is_ok(),
            "Should support module-specific filter syntax"
        );
    }

    #[test]
    fn test_init_idempotent() {
        let result1 = init(&filter("debug"), false);
        assert!(result1.is_ok(), "First initialization should succeed");

        let result2 = init(&filter("info"), false);
        assert!(
            result2.is_ok(),
            "Second initialization should succeed (ignored)"
        );
    }

    #[test]
    fn test_init_accepts_every_level_spelling() {
        for level in ["trace", "debug", "info", "warn", "error", "DEBUG", "3"] {
            assert!(init(&filter(level), false).is_ok(), "{level}");
        }
    }

    #[test]
    fn test_init_with_log_json_enabled() {
        let result = init(&filter("info"), true);
        assert!(
            result.is_ok(),
            "Should initialize successfully with JSON logging enabled"
        );
    }
}
