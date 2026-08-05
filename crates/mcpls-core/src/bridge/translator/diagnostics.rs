//! Diagnostics pull/push merging, cache-derived diagnostics, and server
//! log/message retrieval.

use std::path::PathBuf;
use std::sync::Arc;

use lsp_types::{PartialResultParams, TextDocumentIdentifier, WorkDoneProgressParams};
use serde::Serialize;
use tokio::sync::Mutex;

use super::Translator;
use super::dto::{
    Diagnostic, DiagnosticSeverity, DiagnosticsResult, Position2D, Range, ServerLogsResult,
    ServerMessagesResult,
};
use super::encoding_ctx::EncodingCtx;
use super::routing::validate_path_against_roots;
use crate::bridge::encoding::PositionEncoding;
use crate::bridge::{DiagnosticInfo, DocumentTracker, NotificationCache, path_to_uri};
use crate::config::ToolKind;
use crate::error::{Error, Result};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticRequestParams {
    text_document: TextDocumentIdentifier,
    #[serde(skip_serializing_if = "Option::is_none")]
    identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_result_id: Option<String>,
    #[serde(flatten)]
    work_done_progress_params: WorkDoneProgressParams,
    #[serde(flatten)]
    partial_result_params: PartialResultParams,
}

fn diagnostic_request_params(text_document: TextDocumentIdentifier) -> DiagnosticRequestParams {
    DiagnosticRequestParams {
        text_document,
        identifier: None,
        previous_result_id: None,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

/// Convert an LSP diagnostic into the MCP-facing `Diagnostic` shape.
///
/// Shared by both the pull-model (`handle_diagnostics`) and cache-derived
/// (`diagnostics_from_cache_entry`) diagnostic paths, so their output never
/// diverges in formatting — `merge_diagnostics`'s dedup logic depends on
/// both sides mapping severity/code identically.
pub(super) async fn diagnostic_to_mcp(
    diag: &lsp_types::Diagnostic,
    ctx: &EncodingCtx,
    uri: &lsp_types::Uri,
) -> Diagnostic {
    Diagnostic {
        range: ctx.normalize_range(uri, diag.range).await,
        severity: match diag.severity {
            Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
            Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
            Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
            // INFORMATION and None (no severity reported) both fall here.
            _ => DiagnosticSeverity::Information,
        },
        message: diag.message.clone(),
        code: diag.code.as_ref().map(|c| match c {
            lsp_types::NumberOrString::Number(n) => n.to_string(),
            lsp_types::NumberOrString::String(s) => s.clone(),
        }),
    }
}

impl Translator {
    /// Resolve the LSP-side cache key (URI string) for a cached-diagnostics lookup.
    ///
    /// Split out from the cache read itself so callers (e.g. the
    /// `get_cached_diagnostics` MCP tool) can do the path `canonicalize()` and
    /// workspace-boundary check *before* taking the `NotificationCache` lock —
    /// that lock is also needed by `diagnostics_pump` to store incoming
    /// notifications, so nothing that isn't a plain map lookup should run
    /// while it's held.
    ///
    /// # Errors
    ///
    /// Returns an error if the path is invalid or outside workspace boundaries.
    pub fn cached_diagnostics_uri(workspace_roots: &[PathBuf], file_path: &str) -> Result<String> {
        let path = PathBuf::from(file_path);
        let validated_path = validate_path_against_roots(&path, workspace_roots)?;

        // Use path_to_uri (strips \\?\ on Windows) so the key matches what
        // rust-analyzer stores in publishDiagnostics notifications.
        Ok(path_to_uri(&validated_path)?.to_string())
    }

    /// Handle diagnostics request.
    ///
    /// Merges the LSP pull-model response (`textDocument/diagnostic`) with
    /// whatever is already cached from `textDocument/publishDiagnostics` push
    /// notifications for the same file, so this returns the same diagnostics
    /// `get_cached_diagnostics` would for the file at the same point in time
    /// (see #244 — rust-analyzer's pull endpoint omits flycheck/clippy-sourced
    /// diagnostics, and empirically also some native ones, that are only ever
    /// delivered via the push path). If the pull request itself fails (e.g. a
    /// push-only server answering `-32601`, or a timeout), a non-empty cache
    /// entry is returned as a cache-only result instead of propagating the
    /// error, since the cache is not required to be fresher than the pull
    /// response to be useful here.
    ///
    /// The cache is read only after the pull request settles (success or
    /// failure) and held only for the lookup itself — never across the LSP
    /// round-trip — matching the lock-ordering discipline documented on
    /// `cached_diagnostics_uri`. Like `get_cached_diagnostics`, the cache is
    /// treated as eventually consistent: a cached entry may reflect a
    /// slightly older document version than the fresh pull result if an edit
    /// landed inside the server's flycheck debounce window.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP pull request fails and the cache holds no
    /// diagnostics for the file either, or if the file cannot be opened.
    pub async fn handle_diagnostics(
        &self,
        file_path: String,
        notification_cache: &Mutex<NotificationCache>,
    ) -> Result<DiagnosticsResult> {
        let (server_id, client, uri) = self
            .prepare_document(&file_path, ToolKind::Diagnostics)
            .await?;
        let ctx = self.encoding_ctx(&server_id);

        let params = diagnostic_request_params(TextDocumentIdentifier { uri: uri.clone() });

        let pull_response: Result<lsp_types::DocumentDiagnosticReportResult> = client
            .request("textDocument/diagnostic", params, client.request_timeout())
            .await;

        let diag_info = {
            let cache = notification_cache.lock().await;
            cache.get_diagnostics(uri.as_str()).cloned()
        };

        match pull_response {
            Ok(response) => {
                let items = match response {
                    lsp_types::DocumentDiagnosticReportResult::Report(report) => match report {
                        lsp_types::DocumentDiagnosticReport::Full(full) => {
                            full.full_document_diagnostic_report.items
                        }
                        lsp_types::DocumentDiagnosticReport::Unchanged(_) => vec![],
                    },
                    lsp_types::DocumentDiagnosticReportResult::Partial(_) => vec![],
                };
                let mut diagnostics = Vec::with_capacity(items.len());
                for d in &items {
                    diagnostics.push(diagnostic_to_mcp(d, &ctx, &uri).await);
                }
                let pull = DiagnosticsResult { diagnostics };
                Ok(Self::merge_diagnostics(
                    pull,
                    diag_info.as_ref(),
                    ctx.encoding,
                    &self.document_tracker,
                )
                .await)
            }
            Err(e) => {
                let cache_only = Self::diagnostics_from_cache_entry(
                    diag_info.as_ref(),
                    ctx.encoding,
                    &self.document_tracker,
                )
                .await;
                if cache_only.diagnostics.is_empty() {
                    Err(e)
                } else {
                    Ok(cache_only)
                }
            }
        }
    }

    /// Convert a cached diagnostics entry into the MCP-facing result shape.
    ///
    /// Takes an already-cloned `Option<&DiagnosticInfo>` (out of the
    /// `NotificationCache` lock) rather than the cache itself, so this
    /// mapping — which is not a bounded operation for a large diagnostics set
    /// — never runs while the cache is locked.
    ///
    /// `encoding` is the negotiated encoding of the server that published
    /// these diagnostics; pass `PositionEncoding::Utf16` when no live server
    /// context is available (e.g. a cache-only read with no resolved owner).
    #[must_use]
    pub async fn diagnostics_from_cache_entry(
        diag_info: Option<&DiagnosticInfo>,
        encoding: PositionEncoding,
        tracker: &Arc<DocumentTracker>,
    ) -> DiagnosticsResult {
        let diagnostics = match diag_info {
            Some(diag_info) => {
                let ctx = EncodingCtx {
                    encoding,
                    tracker: tracker.clone(),
                };
                let mut result = Vec::with_capacity(diag_info.diagnostics.len());
                for d in &diag_info.diagnostics {
                    result.push(diagnostic_to_mcp(d, &ctx, &diag_info.uri).await);
                }
                result
            }
            None => Vec::new(),
        };

        DiagnosticsResult { diagnostics }
    }

    /// Merge push-model diagnostics from the notification cache into a
    /// pull-model (`textDocument/diagnostic`) result.
    ///
    /// rust-analyzer's pull endpoint omits diagnostics that are only ever
    /// delivered via `textDocument/publishDiagnostics` push notifications —
    /// not just flycheck/clippy lints, but empirically (verified against a
    /// live rust-analyzer 1.97.1 session, see #244) some native diagnostics
    /// too. Those are cached separately in `NotificationCache`.
    ///
    /// Where the *same* logical problem is reported through both paths, the
    /// two representations were observed to differ in both `range` and
    /// rendered `message`. Captured example, a "not all trait items
    /// implemented" (E0046) error for one `impl` block: pull reported range
    /// `(96,7)-(96,12)` (the trait name) with message "not all trait items
    /// implemented, missing: `fn hello`"; the push notification for the same
    /// error reported range `(95,1)-(95,32)` (the impl block) with message
    /// "not all trait items implemented, missing: `hello`\nmissing `hello`
    /// in implementation" — same `code`/`severity`, adjacent but distinct
    /// ranges, different message text. Exact field equality never dedups
    /// cases like that.
    ///
    /// Given that, a cache entry is treated as a duplicate of a pull entry
    /// when both carry a `code`, the `(severity, code)` pair matches, *and*
    /// the two ranges are either overlapping or start within
    /// `DUPLICATE_RANGE_PROXIMITY_LINES` lines of each other — close
    /// enough to be the same underlying model divergence, not two distinct
    /// occurrences of the same error class (e.g. two unrelated `E0308`
    /// mismatches at different call sites in one file, one caught only
    /// natively and one only by flycheck). Diagnostics with no `code` fall
    /// back to full-field equality, since there is no cheaper stable
    /// identity available for them.
    ///
    /// Output is sorted by `(start.line, start.character)` so merged
    /// cache-only entries don't land out of document order after the
    /// pull-model ones.
    #[must_use]
    pub async fn merge_diagnostics(
        mut pull: DiagnosticsResult,
        diag_info: Option<&DiagnosticInfo>,
        encoding: PositionEncoding,
        tracker: &Arc<DocumentTracker>,
    ) -> DiagnosticsResult {
        /// Start-line distance within which same-code, same-severity
        /// diagnostics from the two models are still considered the same
        /// underlying problem. Derived from the captured E0046 case above
        /// (1 line apart); wide enough to absorb span drift between
        /// rust-analyzer's own spans and rustc's, narrow enough that two
        /// genuinely distinct same-code errors elsewhere in a file are not
        /// collapsed into one.
        const DUPLICATE_RANGE_PROXIMITY_LINES: u32 = 3;

        fn position_le(a: &Position2D, b: &Position2D) -> bool {
            (a.line, a.character) <= (b.line, b.character)
        }

        fn ranges_close(a: &Range, b: &Range) -> bool {
            let overlaps = position_le(&a.start, &b.end) && position_le(&b.start, &a.end);
            overlaps || a.start.line.abs_diff(b.start.line) <= DUPLICATE_RANGE_PROXIMITY_LINES
        }

        fn is_duplicate(pull: &[Diagnostic], candidate: &Diagnostic) -> bool {
            pull.iter().any(|p| match (&candidate.code, &p.code) {
                (Some(c), Some(pc)) if c == pc && p.severity == candidate.severity => {
                    ranges_close(&p.range, &candidate.range)
                }
                _ => p == candidate,
            })
        }

        let cached = Self::diagnostics_from_cache_entry(diag_info, encoding, tracker)
            .await
            .diagnostics;
        let new_diagnostics: Vec<_> = cached
            .into_iter()
            .filter(|c| !is_duplicate(&pull.diagnostics, c))
            .collect();
        pull.diagnostics.extend(new_diagnostics);
        pull.diagnostics
            .sort_by_key(|d| (d.range.start.line, d.range.start.character));
        pull
    }

    /// Handle server logs request.
    ///
    /// # Errors
    ///
    /// Returns an error if the `min_level` parameter is invalid.
    pub fn handle_server_logs(
        cache: &NotificationCache,
        limit: usize,
        min_level: Option<String>,
    ) -> Result<ServerLogsResult> {
        use crate::bridge::notifications::LogLevel;

        let min_level_filter = if let Some(level_str) = min_level {
            let level = match level_str.to_lowercase().as_str() {
                "error" => LogLevel::Error,
                "warning" => LogLevel::Warning,
                "info" => LogLevel::Info,
                "debug" => LogLevel::Debug,
                _ => {
                    return Err(Error::InvalidToolParams(format!(
                        "Invalid min_level: '{level_str}'. Valid values: error, warning, info, debug"
                    )));
                }
            };
            Some(level)
        } else {
            None
        };

        let all_logs = cache.logs();

        let logs: Vec<_> = all_logs
            .iter()
            .filter(|log| {
                min_level_filter.is_none_or(|min| match min {
                    LogLevel::Error => matches!(log.level, LogLevel::Error),
                    LogLevel::Warning => matches!(log.level, LogLevel::Error | LogLevel::Warning),
                    LogLevel::Info => !matches!(log.level, LogLevel::Debug),
                    LogLevel::Debug => true,
                })
            })
            .take(limit)
            .cloned()
            .collect();

        Ok(ServerLogsResult { logs })
    }

    /// Handle server messages request.
    ///
    /// # Errors
    ///
    /// This method does not return errors.
    pub fn handle_server_messages(
        cache: &NotificationCache,
        limit: usize,
    ) -> Result<ServerMessagesResult> {
        let all_messages = cache.messages();
        let messages: Vec<_> = all_messages.iter().take(limit).cloned().collect();
        Ok(ServerMessagesResult { messages })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::time::timeout;
    use url::Url;

    use super::*;
    use crate::bridge::translator::testing::*;
    use crate::config::{ServerId, ToolRouter};

    #[test]
    fn test_diagnostic_request_params_omit_optional_null_fields() {
        let uri = "file:///test.ts".parse().unwrap();
        let params = diagnostic_request_params(TextDocumentIdentifier { uri });
        let value = serde_json::to_value(params).unwrap();

        assert_eq!(value["textDocument"]["uri"], "file:///test.ts");
        assert!(value.get("identifier").is_none());
        assert!(value.get("previousResultId").is_none());
    }

    #[tokio::test]
    async fn test_handle_cached_diagnostics_empty() {
        let cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let cache_key =
            Translator::cached_diagnostics_uri(&[], test_file.to_str().unwrap()).unwrap();
        let diag_info = cache.get_diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(
            diag_info.as_ref(),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;
        assert_eq!(diags.diagnostics.len(), 0);
    }

    #[test]
    fn test_handle_server_logs_with_filter() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        // Add some logs
        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        // Test with error filter
        let result = Translator::handle_server_logs(&cache, 10, Some("error".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 1);
        assert_eq!(logs.logs[0].message, "error msg");

        // Test with warning filter (includes error and warning)
        let result = Translator::handle_server_logs(&cache, 10, Some("warning".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 2);

        // Test with info filter (excludes debug)
        let result = Translator::handle_server_logs(&cache, 10, Some("info".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 3);

        // Test with debug filter (includes all)
        let result = Translator::handle_server_logs(&cache, 10, Some("debug".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 4);

        // Test with invalid filter
        let result = Translator::handle_server_logs(&cache, 10, Some("invalid".to_string()));
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[test]
    fn test_handle_server_messages_limit() {
        use crate::bridge::notifications::MessageType;

        let mut cache = NotificationCache::new();

        // Add some messages
        for i in 0..10 {
            cache.store_message(MessageType::Info, format!("message {i}"));
        }

        // Test limit
        let result = Translator::handle_server_messages(&cache, 5);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 5);
        assert_eq!(messages.messages[0].message, "message 0");
        assert_eq!(messages.messages[4].message, "message 4");

        // Test limit larger than available
        let result = Translator::handle_server_messages(&cache, 100);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 10);
    }

    #[tokio::test]
    async fn test_handle_cached_diagnostics_with_data() {
        let mut cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: Some(lsp_types::NumberOrString::String("E001".to_string())),
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&ServerId::from("rust"), &uri, Some(1), vec![diagnostic]);

        let cache_key =
            Translator::cached_diagnostics_uri(&[], test_file.to_str().unwrap()).unwrap();
        let diag_info = cache.get_diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(
            diag_info.as_ref(),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;
        assert_eq!(diags.diagnostics.len(), 1);
        assert_eq!(diags.diagnostics[0].message, "test error");
        assert_eq!(diags.diagnostics[0].code, Some("E001".to_string()));
        assert!(matches!(
            diags.diagnostics[0].severity,
            DiagnosticSeverity::Error
        ));
        assert_eq!(diags.diagnostics[0].range.start.line, 1);
        assert_eq!(diags.diagnostics[0].range.start.character, 1);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn test_handle_cached_diagnostics_multiple_severities() {
        let mut cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostics = vec![
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: "error".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 1,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 1,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                message: "warning".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 2,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 2,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::INFORMATION),
                message: "info".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 3,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 3,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::HINT),
                message: "hint".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
        ];

        cache.store_diagnostics(&ServerId::from("rust"), &uri, Some(1), diagnostics);

        let cache_key =
            Translator::cached_diagnostics_uri(&[], test_file.to_str().unwrap()).unwrap();
        let diag_info = cache.get_diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(
            diag_info.as_ref(),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;
        assert_eq!(diags.diagnostics.len(), 4);
        assert!(matches!(
            diags.diagnostics[0].severity,
            DiagnosticSeverity::Error
        ));
        assert!(matches!(
            diags.diagnostics[1].severity,
            DiagnosticSeverity::Warning
        ));
        assert!(matches!(
            diags.diagnostics[2].severity,
            DiagnosticSeverity::Information
        ));
        assert!(matches!(
            diags.diagnostics[3].severity,
            DiagnosticSeverity::Hint
        ));
    }

    #[tokio::test]
    async fn test_handle_cached_diagnostics_with_numeric_code() {
        let mut cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: Some(lsp_types::NumberOrString::Number(42)),
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&ServerId::from("rust"), &uri, Some(1), vec![diagnostic]);

        let cache_key =
            Translator::cached_diagnostics_uri(&[], test_file.to_str().unwrap()).unwrap();
        let diag_info = cache.get_diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(
            diag_info.as_ref(),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;
        assert_eq!(diags.diagnostics.len(), 1);
        assert_eq!(diags.diagnostics[0].code, Some("42".to_string()));
    }

    #[test]
    fn test_handle_cached_diagnostics_invalid_path() {
        let result = Translator::cached_diagnostics_uri(&[], "/nonexistent/path/file.rs");
        assert!(matches!(result, Err(Error::FileIo { .. })));
    }

    #[tokio::test]
    async fn test_merge_diagnostics_cache_only_appends_to_empty_pull() {
        let pull = DiagnosticsResult {
            diagnostics: vec![],
        };
        let cache = diag_info(vec![lsp_diag(
            0,
            10,
            lsp_types::DiagnosticSeverity::WARNING,
            "unused import: `std::fmt`",
            None,
        )]);

        let merged = Translator::merge_diagnostics(
            pull,
            Some(&cache),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;

        assert_eq!(merged.diagnostics.len(), 1);
        assert_eq!(merged.diagnostics[0].message, "unused import: `std::fmt`");
        assert!(matches!(
            merged.diagnostics[0].severity,
            DiagnosticSeverity::Warning
        ));
    }

    #[tokio::test]
    async fn test_merge_diagnostics_exact_duplicate_not_repeated() {
        // Same range/severity/message/code as the cache entry below, expressed
        // in the 1-based MCP shape `diagnostics_from_cache_entry` would produce.
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 11,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "mismatched types".to_string(),
            code: Some("E0308".to_string()),
        };
        let pull = DiagnosticsResult {
            diagnostics: vec![pull_diag.clone()],
        };
        let cache = diag_info(vec![lsp_diag(
            0,
            10,
            lsp_types::DiagnosticSeverity::ERROR,
            "mismatched types",
            Some("E0308"),
        )]);

        let merged = Translator::merge_diagnostics(
            pull,
            Some(&cache),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;

        assert_eq!(merged.diagnostics.len(), 1);
        assert_eq!(merged.diagnostics[0], pull_diag);
    }

    #[tokio::test]
    async fn test_merge_diagnostics_no_cache_entry_returns_pull_unchanged() {
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 5,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "syntax error".to_string(),
            code: None,
        };
        let pull = DiagnosticsResult {
            diagnostics: vec![pull_diag.clone()],
        };

        let merged =
            Translator::merge_diagnostics(pull, None, PositionEncoding::Utf16, &test_tracker())
                .await;

        assert_eq!(merged.diagnostics, vec![pull_diag]);
    }

    #[tokio::test]
    async fn test_merge_diagnostics_multiple_distinct_cache_entries_all_appear() {
        let pull = DiagnosticsResult {
            diagnostics: vec![],
        };
        let cache = diag_info(vec![
            lsp_diag(
                0,
                10,
                lsp_types::DiagnosticSeverity::WARNING,
                "unused import: `std::fmt`",
                None,
            ),
            lsp_diag(
                5,
                8,
                lsp_types::DiagnosticSeverity::WARNING,
                "function `helper` is never used",
                None,
            ),
        ]);

        let merged = Translator::merge_diagnostics(
            pull,
            Some(&cache),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;

        assert_eq!(merged.diagnostics.len(), 2);
        assert!(
            merged
                .diagnostics
                .iter()
                .any(|d| d.message == "unused import: `std::fmt`")
        );
        assert!(
            merged
                .diagnostics
                .iter()
                .any(|d| d.message == "function `helper` is never used")
        );
    }

    #[tokio::test]
    async fn test_merge_diagnostics_same_range_different_message_not_deduped() {
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 11,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "mismatched types".to_string(),
            code: None,
        };
        let pull = DiagnosticsResult {
            diagnostics: vec![pull_diag],
        };
        // Same range and severity as the pull diagnostic, but a different
        // message — must be treated as a distinct diagnostic, not a duplicate.
        let cache = diag_info(vec![lsp_diag(
            0,
            10,
            lsp_types::DiagnosticSeverity::ERROR,
            "expected `i32`, found `&str`",
            None,
        )]);

        let merged = Translator::merge_diagnostics(
            pull,
            Some(&cache),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;

        assert_eq!(merged.diagnostics.len(), 2);
    }

    /// Pins a cross-model duplicate shape verified empirically against a live
    /// rust-analyzer 1.97.1 session (#244): the pull and push diagnostics for
    /// the *same* "not all trait items implemented" (E0046) error had
    /// different ranges (trait name vs. impl block) and different messages
    /// (terse vs. rustc's full rendering), but shared `code` and `severity`.
    /// Exact-field dedup would report this twice; the `(severity, code)`
    /// fingerprint must collapse it to one entry.
    #[tokio::test]
    async fn test_merge_diagnostics_same_code_different_range_and_message_deduped() {
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 96,
                    character: 7,
                },
                end: Position2D {
                    line: 96,
                    character: 12,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "not all trait items implemented, missing: `fn hello`".to_string(),
            code: Some("E0046".to_string()),
        };
        let pull = DiagnosticsResult {
            diagnostics: vec![pull_diag.clone()],
        };
        // Same code and severity, but a different range and a longer,
        // differently-worded message -- the rustc-rendered push side of the
        // same underlying error.
        let cache = diag_info(vec![lsp_diag(
            94,
            31,
            lsp_types::DiagnosticSeverity::ERROR,
            "not all trait items implemented, missing: `hello`\nmissing `hello` in implementation",
            Some("E0046"),
        )]);

        let merged = Translator::merge_diagnostics(
            pull,
            Some(&cache),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;

        assert_eq!(merged.diagnostics.len(), 1);
        assert_eq!(merged.diagnostics[0], pull_diag);
    }

    /// Regression: `merge_diagnostics`'s `(severity, code)` fingerprint alone
    /// is coarser than full-field equality and cannot tell apart two
    /// genuinely distinct diagnostics that happen to share `code` and
    /// `severity` -- e.g. two separate `E0308` mismatched-type errors at
    /// different locations in the same file, one caught only by native
    /// (pull) analysis and a second, unrelated one caught only by
    /// flycheck/cargo check (cache), such as an error inside macro-expanded
    /// code the native pass did not evaluate. This previously caused the
    /// cache-only entry to be silently dropped -- reproducing #244's exact
    /// failure mode, just relocated from "no merge" to "over-eager dedup".
    ///
    /// The range-proximity check on `is_duplicate` (see `merge_diagnostics`)
    /// closes this: these two diagnostics are 45 lines apart, far outside
    /// `DUPLICATE_RANGE_PROXIMITY_LINES`, so both must survive the merge.
    #[tokio::test]
    async fn test_merge_diagnostics_same_code_distinct_diagnostics_at_different_locations_both_kept()
     {
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 5,
                    character: 9,
                },
                end: Position2D {
                    line: 5,
                    character: 20,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "mismatched types: expected `i32`, found `&str`".to_string(),
            code: Some("E0308".to_string()),
        };
        let pull = DiagnosticsResult {
            diagnostics: vec![pull_diag.clone()],
        };
        // A second, unrelated E0308 at a completely different location with
        // a completely different message -- a real, distinct diagnostic,
        // not a duplicate of pull_diag.
        let cache = diag_info(vec![lsp_diag(
            49,
            22,
            lsp_types::DiagnosticSeverity::ERROR,
            "mismatched types: expected `String`, found `Vec<u8>`",
            Some("E0308"),
        )]);

        let merged = Translator::merge_diagnostics(
            pull,
            Some(&cache),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;

        assert_eq!(merged.diagnostics.len(), 2);
        assert_eq!(merged.diagnostics[0], pull_diag);
        assert_eq!(
            merged.diagnostics[1].message,
            "mismatched types: expected `String`, found `Vec<u8>`"
        );
    }

    #[test]
    fn test_handle_server_logs_no_filter() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, None);
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 4);
    }

    #[test]
    fn test_handle_server_logs_error_filter_strict() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("error".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 1);
        assert_eq!(logs.logs[0].message, "error msg");
    }

    #[test]
    fn test_handle_server_logs_warning_filter_includes_errors() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("warning".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 2);
    }

    #[test]
    fn test_handle_server_logs_info_filter_excludes_debug() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("info".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 2);
    }

    #[test]
    fn test_handle_server_logs_debug_filter_includes_all() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("debug".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 4);
    }

    #[test]
    fn test_handle_server_logs_limit_applies_after_filter() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        for i in 0..10 {
            cache.store_log(LogLevel::Error, format!("error {i}"));
        }

        let result = Translator::handle_server_logs(&cache, 5, Some("error".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 5);
        assert_eq!(logs.logs[0].message, "error 0");
        assert_eq!(logs.logs[4].message, "error 4");
    }

    #[test]
    fn test_handle_server_logs_case_insensitive_level() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("ERROR".to_string()));
        assert!(result.is_ok());

        let result = Translator::handle_server_logs(&cache, 10, Some("Error".to_string()));
        assert!(result.is_ok());

        let result = Translator::handle_server_logs(&cache, 10, Some("eRrOr".to_string()));
        assert!(result.is_ok());
    }

    #[test]
    fn test_handle_server_messages_empty() {
        let cache = NotificationCache::new();

        let result = Translator::handle_server_messages(&cache, 10);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 0);
    }

    #[test]
    fn test_handle_server_messages_with_different_types() {
        use crate::bridge::notifications::MessageType;

        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Error, "error".to_string());
        cache.store_message(MessageType::Warning, "warning".to_string());
        cache.store_message(MessageType::Info, "info".to_string());
        cache.store_message(MessageType::Log, "log".to_string());

        let result = Translator::handle_server_messages(&cache, 10);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 4);
        assert_eq!(messages.messages[0].message, "error");
        assert_eq!(messages.messages[1].message, "warning");
        assert_eq!(messages.messages[2].message, "info");
        assert_eq!(messages.messages[3].message, "log");
    }

    #[test]
    fn test_handle_server_messages_zero_limit() {
        use crate::bridge::notifications::MessageType;

        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Info, "test".to_string());

        let result = Translator::handle_server_messages(&cache, 0);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 0);
    }

    #[test]
    fn test_handle_cached_diagnostics_path_outside_workspace() {
        let temp_dir1 = TempDir::new().unwrap();
        let temp_dir2 = TempDir::new().unwrap();

        let workspace_roots = vec![temp_dir1.path().to_path_buf()];

        let test_file = temp_dir2.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result =
            Translator::cached_diagnostics_uri(&workspace_roots, test_file.to_str().unwrap());
        assert!(matches!(result, Err(Error::PathOutsideWorkspace(_))));
    }

    /// S1 regression (#244): a push-only server (or one that times out)
    /// answering `textDocument/diagnostic` with an LSP error must not
    /// discard diagnostics `handle_diagnostics` already knows about from the
    /// cache -- it should return the cache-only result instead of `Err`.
    #[tokio::test]
    async fn test_handle_diagnostics_pull_error_falls_back_to_nonempty_cache() {
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("rs".to_string(), "rust".to_string());

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([(
                    ServerId::from("rust"),
                    "rust".to_string(),
                )]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let (client, mut server) = fake_lsp_client();
        translator.register_client("rust".to_string(), client);

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        // Prime the cache under the exact URI handle_diagnostics will look
        // up (path_to_uri over the canonicalized path, same as
        // document_tracker uses to open the document).
        let canonical = path.canonicalize().unwrap();
        let uri = path_to_uri(&canonical).unwrap();
        let notification_cache = Mutex::new(NotificationCache::new());
        {
            let mut cache = notification_cache.lock().await;
            cache.store_diagnostics(
                &ServerId::from("rust"),
                &uri,
                Some(1),
                vec![lsp_diag(
                    0,
                    4,
                    lsp_types::DiagnosticSeverity::WARNING,
                    "unused import: `std::fmt`",
                    None,
                )],
            );
        }

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_diagnostics(path_str, &notification_cache)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        write_error_response(
            &mut server.read_half_stdin,
            &diag_request["id"],
            -32601,
            "method not found",
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap();

        let diagnostics = result.expect("cache-only fallback should succeed despite pull error");
        assert_eq!(diagnostics.diagnostics.len(), 1);
        assert_eq!(
            diagnostics.diagnostics[0].message,
            "unused import: `std::fmt`"
        );
    }

    /// S1 counterpart: when the cache is also empty, the pull error must
    /// still propagate -- there is nothing to fall back to.
    #[tokio::test]
    async fn test_handle_diagnostics_pull_error_and_empty_cache_propagates_error() {
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("rs".to_string(), "rust".to_string());

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([(
                    ServerId::from("rust"),
                    "rust".to_string(),
                )]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let (client, mut server) = fake_lsp_client();
        translator.register_client("rust".to_string(), client);

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let notification_cache = Mutex::new(NotificationCache::new());

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_diagnostics(path_str, &notification_cache)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        write_error_response(
            &mut server.read_half_stdin,
            &diag_request["id"],
            -32601,
            "method not found",
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap();

        assert!(
            result.is_err(),
            "pull error with no cache data must propagate, got {result:?}"
        );
    }
}
