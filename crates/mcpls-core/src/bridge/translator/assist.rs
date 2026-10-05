//! Completions, signature help, and inlay hints handlers.

use lsp_types::{
    CompletionParams, CompletionTriggerKind, InlayHintParams, PartialResultParams,
    SignatureHelpParams as LspSignatureHelpParams, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams,
};

use super::Translator;
use super::dto::{
    Completion, CompletionsResult, InlayHintEntry, InlayHintsResult, Position, PositionRange,
    SignatureHelpResult, SignatureInfo, SignatureParameter, lsp_kind_to_u32,
};
use super::navigation::ItemBudget;
use super::routing::{Capability, IndexingGate};
use crate::bridge::ClientPath;
use crate::bridge::encoding::{LabelOffsets, PositionEncoding};
use crate::error::{Error, Result};

/// Extract hover contents as markdown string.
/// Convert LSP `Documentation` to a plain string.
fn extract_documentation(doc: lsp_types::Documentation) -> String {
    match doc {
        lsp_types::Documentation::String(s) => s,
        lsp_types::Documentation::MarkupContent(m) => m.value,
    }
}

/// Most parameters kept per signature; the rest are dropped (logged).
const MAX_SIGNATURE_PARAMETERS: usize = 256;

/// Total resolved tuple-label bytes allowed per signature, as a multiple of
/// the signature label's length: generous enough for overlapping or repeated
/// spans (variadic or default-value ranges), bounded against amplification.
const RESOLVED_LABEL_BUDGET_MULTIPLE: usize = 4;

/// Converts a signature's LSP parameters to their MCP form, resolving
/// offset-pair labels against `signature_label` in `encoding`'s code units.
///
/// An offset pair that cannot be resolved exactly yields `label: None`
/// (logged) rather than a wrong or raw `[start,end]` label (#511). The
/// requested offsets are resolved in one pass per signature, the parameter
/// count is capped at [`MAX_SIGNATURE_PARAMETERS`], and the resolved bytes in
/// total are capped at [`RESOLVED_LABEL_BUDGET_MULTIPLE`] times the label's
/// length, so many overlapping pairs cannot amplify one response.
fn signature_parameters(
    params: Vec<lsp_types::ParameterInformation>,
    signature_label: &str,
    encoding: PositionEncoding,
) -> Vec<SignatureParameter> {
    if params.len() > MAX_SIGNATURE_PARAMETERS {
        tracing::warn!(
            reported = params.len(),
            cap = MAX_SIGNATURE_PARAMETERS,
            "signature parameter count exceeds the cap; truncating"
        );
    }
    let offsets = LabelOffsets::new(
        signature_label,
        params
            .iter()
            .take(MAX_SIGNATURE_PARAMETERS)
            .filter_map(|p| match p.label {
                lsp_types::ParameterInformationLabel::Tuple((start, end)) => {
                    Some(<[u32; 2]>::from((start, end)))
                }
                lsp_types::ParameterInformationLabel::String(_) => None,
            })
            .flatten(),
        encoding,
    );
    let mut resolved_budget = signature_label
        .len()
        .saturating_mul(RESOLVED_LABEL_BUDGET_MULTIPLE);
    params
        .into_iter()
        .take(MAX_SIGNATURE_PARAMETERS)
        .map(|param| {
            let label = match param.label {
                lsp_types::ParameterInformationLabel::String(s) => Some(s),
                lsp_types::ParameterInformationLabel::Tuple((start, end)) => {
                    let resolved = offsets
                        .substring(start, end)
                        .filter(|text| text.len() <= resolved_budget);
                    if let Some(text) = resolved {
                        resolved_budget = resolved_budget.saturating_sub(text.len());
                    } else {
                        tracing::warn!(
                            start,
                            end,
                            encoding = encoding.to_lsp(),
                            "signature parameter label offsets do not resolve within the \
                             signature label; omitting the label"
                        );
                    }
                    resolved.map(str::to_string)
                }
            };
            SignatureParameter {
                label,
                documentation: param.documentation.map(extract_documentation),
            }
        })
        .collect()
}

/// Maximum length, in bytes, of a `get_completions` `trigger` parameter.
///
/// The LSP spec defines `triggerCharacter` as a single character, but
/// `CompletionsParams.trigger` is still an unbounded free-form `String`
/// forwarded to the LSP server as `trigger_character` with no cap of its
/// own (#309 M3) -- the same forwarding-without-a-cap shape `new_name` and
/// `query` had. 8 bytes comfortably covers any single Unicode codepoint (at
/// most 4 bytes in UTF-8) with margin, while still rejecting anything that
/// isn't plausibly "one character".
pub(super) const MAX_TRIGGER_CHARACTER_BYTES: usize = 8;

/// Validate parameters for `handle_completions`.
fn validate_completions_params(trigger: Option<&str>) -> Result<()> {
    if let Some(trigger) = trigger
        && trigger.len() > MAX_TRIGGER_CHARACTER_BYTES
    {
        return Err(Error::InvalidToolParams(format!(
            "trigger too long: {} bytes (max {MAX_TRIGGER_CHARACTER_BYTES})",
            trigger.len()
        )));
    }
    Ok(())
}

impl Translator {
    /// Handle completions request.
    ///
    /// # Errors
    ///
    /// Returns an error if `trigger` exceeds the maximum allowed length,
    /// the LSP request fails, the file cannot be opened, the routed server
    /// does not advertise `completionProvider` support, or the server is
    /// still indexing the workspace (see `wait_for_indexing_ready`).
    pub async fn handle_completions(
        &self,
        file_path: ClientPath,
        position: Position,
        trigger: Option<String>,
    ) -> Result<CompletionsResult> {
        validate_completions_params(trigger.as_deref())?;

        let doc = self
            .prepare_gated_document(&file_path, Capability::Completions, IndexingGate::Required)
            .await?;
        let (server_id, client, uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let lsp_position = ctx.to_lsp(uri, position).await;

        let context = trigger.map(|trigger_char| lsp_types::CompletionContext {
            trigger_kind: CompletionTriggerKind::TriggerCharacter,
            trigger_character: Some(trigger_char),
        });

        let params = CompletionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context,
        };

        let response = client
            .request_typed::<lsp_types::CompletionRequest>(params, client.completion_timeout())
            .await?;

        let items = match response {
            Some(lsp_types::CompletionResponse::CompletionItemList(items)) => items,
            Some(lsp_types::CompletionResponse::CompletionList(list)) => list.items,
            None => vec![],
        };

        let result = CompletionsResult {
            items: items
                .into_iter()
                .map(|item| Completion {
                    label: item.label,
                    kind: item.kind.map(lsp_kind_to_u32),
                    detail: item.detail,
                    documentation: item.documentation.map(|doc| match doc {
                        lsp_types::Documentation::String(s) => s,
                        lsp_types::Documentation::MarkupContent(m) => m.value,
                    }),
                })
                .collect(),
            positions_degraded: ctx.positions_degraded(),
        };

        Ok(result)
    }

    /// Handle signature help request (`textDocument/signatureHelp`).
    ///
    /// Returns parameter signatures and documentation while typing a function call.
    /// `context` is omitted (None) — the server infers trigger state from position.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `signatureHelpProvider` support.
    pub async fn handle_signature_help(
        &self,
        file_path: ClientPath,
        position: Position,
    ) -> Result<SignatureHelpResult> {
        let doc = self
            .prepare_gated_document(
                &file_path,
                Capability::SignatureHelp,
                IndexingGate::NotRequired,
            )
            .await?;
        let (server_id, client, uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let lsp_position = ctx.to_lsp(uri, position).await;

        let params = LspSignatureHelpParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            context: None,
        };

        let response = client
            .request_typed::<lsp_types::SignatureHelpRequest>(params, client.request_timeout())
            .await?;

        let result = match response {
            Some(sig_help) => SignatureHelpResult {
                signatures: sig_help
                    .signatures
                    .into_iter()
                    .map(|sig| SignatureInfo {
                        parameters: signature_parameters(
                            sig.parameters.unwrap_or_default(),
                            &sig.label,
                            ctx.encoding,
                        ),
                        documentation: sig.documentation.map(extract_documentation),
                        label: sig.label,
                    })
                    .collect(),
                active_signature: sig_help.active_signature,
                active_parameter: sig_help.active_parameter.and_then(|ap| match ap {
                    lsp_types::ActiveParameter::Int(n) => Some(n),
                    lsp_types::ActiveParameter::Null => None,
                }),
                positions_degraded: ctx.positions_degraded(),
            },
            None => SignatureHelpResult {
                signatures: vec![],
                active_signature: None,
                active_parameter: None,
                positions_degraded: ctx.positions_degraded(),
            },
        };

        Ok(result)
    }

    /// Handle inlay hints request (`textDocument/inlayHint`).
    ///
    /// Returns inferred type and parameter annotations the editor would render inline.
    /// Output positions are in MCP 1-based form.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `inlayHintProvider` support.
    pub async fn handle_inlay_hints(
        &self,
        file_path: ClientPath,
        range: PositionRange,
    ) -> Result<InlayHintsResult> {
        let (start, end) = (range.start(), range.end());
        let doc = self
            .prepare_gated_document(
                &file_path,
                Capability::InlayHints,
                IndexingGate::NotRequired,
            )
            .await?;
        let (server_id, client, uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let response_uri = uri.clone();

        let lsp_start = ctx.to_lsp(uri, start).await;
        let lsp_end = ctx.to_lsp(uri, end).await;

        let params = InlayHintParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            range: lsp_types::Range {
                start: lsp_start,
                end: lsp_end,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::InlayHintRequest>(params, client.request_timeout())
            .await?;

        let mut budget = ItemBudget::new();
        let lsp_hints = budget.admit(response.unwrap_or_default());
        let mut hints = Vec::with_capacity(lsp_hints.len());
        for hint in lsp_hints {
            let position = ctx.to_mcp(&response_uri, hint.position).await;
            let label = match hint.label {
                lsp_types::Label::String(s) => s,
                lsp_types::Label::InlayHintLabelPartList(parts) => parts
                    .into_iter()
                    .map(|p| p.value)
                    .collect::<Vec<_>>()
                    .concat(),
            };
            let tooltip = hint.tooltip.map(|t| match t {
                lsp_types::Tooltip::String(s) => s,
                lsp_types::Tooltip::MarkupContent(m) => m.value,
            });
            hints.push(InlayHintEntry {
                position,
                label,
                kind: hint.kind.map(lsp_kind_to_u32),
                padding_left: hint.padding_left,
                padding_right: hint.padding_right,
                tooltip,
            });
        }

        Ok(InlayHintsResult {
            hints,
            truncated: budget.truncated(),
            positions_degraded: ctx.positions_degraded(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, fs};

    use super::*;
    use crate::bridge::translator::testing::*;
    use crate::test_lsp::client_path;

    /// #309 M3: `trigger` has no cap of its own even though the LSP spec
    /// defines it as a single character.
    #[test]
    fn test_validate_completions_params_rejects_oversized_trigger() {
        let trigger = "a".repeat(MAX_TRIGGER_CHARACTER_BYTES + 1);
        let result = validate_completions_params(Some(&trigger));
        assert_matches!(result, Err(Error::InvalidToolParams(_)));
    }

    #[test]
    fn test_validate_completions_params_accepts_typical_trigger_char() {
        assert!(validate_completions_params(Some(".")).is_ok());
    }

    #[test]
    fn test_validate_completions_params_accepts_none() {
        assert!(validate_completions_params(None).is_ok());
    }

    /// End-to-end: `handle_completions` must surface
    /// `Error::WorkspaceIndexing` -- not an empty result -- while the routed
    /// server is still `Loading`, without reaching the fake LSP server.
    #[tokio::test(start_paused = true)]
    async fn test_handle_completions_returns_workspace_indexing_error_when_loading() {
        use std::sync::Arc;

        use tempfile::TempDir;
        use tokio::sync::Mutex;

        use crate::bridge::NotificationCache;
        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            completion_provider: Some(lsp_types::CompletionOptions::default()),
            ..Default::default()
        };
        let (translator, _server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = translator.with_notification_cache(cache);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let err = translator
            .handle_completions(
                client_path(path.to_string_lossy().into_owned()),
                Position::at(1, 1),
                None,
            )
            .await
            .unwrap_err();

        assert_matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        );
    }

    /// Companion: when the cache reports `Ready`, `handle_completions` must
    /// dispatch normally.
    #[tokio::test]
    async fn test_handle_completions_dispatches_when_indexing_ready() {
        use std::sync::Arc;

        use tempfile::TempDir;
        use tokio::io::BufReader;
        use tokio::sync::Mutex;

        use crate::bridge::NotificationCache;
        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            completion_provider: Some(lsp_types::CompletionOptions::default()),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        let translator = Arc::new(translator.with_notification_cache(cache));

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_completions(client_path(path), Position::at(1, 1), None)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/completion");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([]),
        )
        .await;

        let result = handle.await.unwrap().unwrap();
        assert!(result.items.is_empty());
    }

    /// #467 M2/tester gap 1: exercises the actual `handle_inlay_hints`
    /// conversion site (`assist.rs:266`, not just the bare `lsp_kind_to_u32`
    /// helper) with a `kind` value above `u8::MAX` -- the old `Option<u8>`
    /// roundtrip silently dropped this to `None`.
    #[tokio::test]
    async fn test_handle_inlay_hints_preserves_custom_kind_above_u8_range() {
        use std::sync::Arc;

        use tempfile::TempDir;
        use tokio::io::BufReader;

        use crate::bridge::translator::testing::pos;
        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            inlay_hint_provider: Some(lsp_types::InlayHintProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_inlay_hints(client_path(path), span(pos(1, 1), pos(1, 13)))
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/inlayHint");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "position": {"line": 0, "character": 5},
                "label": "custom",
                "kind": 300,
            }]),
        )
        .await;

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result.hints.len(), 1);
        assert_eq!(
            result.hints[0].kind,
            Some(300u32),
            "a custom InlayHintKind above u8::MAX must not be truncated or dropped"
        );
    }

    async fn inlay_hints_with_response(count: usize) -> InlayHintsResult {
        use std::sync::Arc;

        use tempfile::TempDir;
        use tokio::io::BufReader;

        use crate::bridge::translator::testing::pos;
        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            inlay_hint_provider: Some(lsp_types::InlayHintProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_inlay_hints(client_path(path), span(pos(1, 1), pos(1, 13)))
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/inlayHint");

        let hints: Vec<_> = (0..count)
            .map(|_| serde_json::json!({"position": {"line": 0, "character": 5}, "label": "h"}))
            .collect();
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::Value::Array(hints),
        )
        .await;

        handle.await.unwrap().unwrap()
    }

    /// #487: inlay hints beyond `MAX_NORMALIZED_LOCATIONS` are dropped and
    /// reported via `truncated`.
    #[tokio::test]
    async fn test_handle_inlay_hints_caps_item_count_and_reports_truncation() {
        use crate::bridge::translator::navigation::MAX_NORMALIZED_LOCATIONS;

        let result = inlay_hints_with_response(MAX_NORMALIZED_LOCATIONS + 500).await;
        assert_eq!(result.hints.len(), MAX_NORMALIZED_LOCATIONS);
        assert!(result.truncated);
    }

    /// #487: exactly `MAX_NORMALIZED_LOCATIONS` hints is not truncation.
    #[tokio::test]
    async fn test_handle_inlay_hints_at_cap_is_not_truncated() {
        use crate::bridge::translator::navigation::MAX_NORMALIZED_LOCATIONS;

        let result = inlay_hints_with_response(MAX_NORMALIZED_LOCATIONS).await;
        assert_eq!(result.hints.len(), MAX_NORMALIZED_LOCATIONS);
        assert!(!result.truncated);
    }

    async fn utf8_degradation(
        content: &str,
        line: u32,
        character: u32,
        method: &str,
    ) -> serde_json::Value {
        use std::sync::Arc;

        use tempfile::TempDir;
        use tokio::io::BufReader;

        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            completion_provider: Some(lsp_types::CompletionOptions::default()),
            signature_help_provider: Some(lsp_types::SignatureHelpOptions::default()),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities_and_encoding(
            &dir,
            &server_id,
            caps,
            lsp_types::PositionEncodingKind::UTF8,
        );
        let path = dir.path().join("main.rs");
        fs::write(&path, content).unwrap();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            let method = method.to_string();
            tokio::spawn(async move {
                if method == "textDocument/completion" {
                    translator
                        .handle_completions(client_path(path), pos(line, character), None)
                        .await
                        .map(|r| serde_json::to_value(r).unwrap())
                } else {
                    translator
                        .handle_signature_help(client_path(path), pos(line, character))
                        .await
                        .map(|r| serde_json::to_value(r).unwrap())
                }
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], method);
        let response = if method == "textDocument/completion" {
            serde_json::json!([])
        } else {
            serde_json::Value::Null
        };
        write_response(&mut server.read_half_stdin, &request["id"], response).await;

        handle.await.unwrap().unwrap()
    }

    /// S1 regression: the empty line after a final newline is a real LSP
    /// line, so column 1 on it converts exactly and must not be flagged.
    #[tokio::test]
    async fn test_handle_completions_trailing_empty_line_is_not_degraded() {
        let wire = utf8_degradation("fn main() {}\n", 2, 1, "textDocument/completion").await;
        assert!(wire.get("positions_degraded").is_none());
    }

    #[tokio::test]
    async fn test_handle_completions_line_past_eof_with_column_is_request_degraded() {
        let wire = utf8_degradation("fn main() {}", 5, 3, "textDocument/completion").await;
        assert_eq!(wire["positions_degraded"], "request");
    }

    #[tokio::test]
    async fn test_handle_signature_help_line_past_eof_with_column_is_request_degraded() {
        let wire = utf8_degradation("fn main() {}", 5, 3, "textDocument/signatureHelp").await;
        assert_eq!(wire["positions_degraded"], "request");
    }

    #[tokio::test]
    async fn test_handle_signature_help_in_range_position_is_not_degraded() {
        let wire = utf8_degradation("fn main() {}", 1, 4, "textDocument/signatureHelp").await;
        assert!(wire.get("positions_degraded").is_none());
    }

    fn tuple_param(start: u32, end: u32) -> lsp_types::ParameterInformation {
        lsp_types::ParameterInformation {
            label: lsp_types::ParameterInformationLabel::Tuple((start, end)),
            documentation: None,
        }
    }

    fn one_param(
        param: lsp_types::ParameterInformation,
        label: &str,
        encoding: PositionEncoding,
    ) -> SignatureParameter {
        signature_parameters(vec![param], label, encoding)
            .pop()
            .unwrap()
    }

    /// #511: tuple offsets are in the negotiated encoding's units, so the
    /// same parameter resolves to the same substring under each of them.
    #[test]
    fn test_signature_parameter_tuple_label_resolves_per_encoding() {
        let label = "é𝄞(a: u8)";
        for (encoding, start, end) in [
            (PositionEncoding::Utf8, 7, 12),
            (PositionEncoding::Utf16, 4, 9),
            (PositionEncoding::Utf32, 3, 8),
        ] {
            let param = one_param(tuple_param(start, end), label, encoding);
            assert_eq!(param.label.as_deref(), Some("a: u8"), "{encoding:?}");
        }
    }

    /// #511: offsets that do not resolve exactly yield no label rather than a
    /// wrong substring or the raw pair.
    #[test]
    fn test_signature_parameter_invalid_tuple_offsets_yield_no_label() {
        let label = "é𝄞(a: u8)";
        for (encoding, start, end) in [
            (PositionEncoding::Utf16, 0, 99),
            (PositionEncoding::Utf16, 9, 4),
            (PositionEncoding::Utf16, 2, 4),
            (PositionEncoding::Utf8, 1, 3),
            (PositionEncoding::Utf32, 3, 99),
        ] {
            let param = one_param(tuple_param(start, end), label, encoding);
            assert_eq!(param.label, None, "{encoding:?} {start}..{end}");
        }
    }

    #[test]
    fn test_signature_parameter_string_label_passes_through() {
        let param = one_param(
            lsp_types::ParameterInformation {
                label: lsp_types::ParameterInformationLabel::String("a: u8".to_string()),
                documentation: None,
            },
            "f(a: u8)",
            PositionEncoding::Utf16,
        );
        assert_eq!(param.label.as_deref(), Some("a: u8"));
    }

    #[test]
    fn test_signature_parameter_unresolved_label_is_omitted_when_serialized() {
        let param = one_param(tuple_param(5, 1), "abcdef", PositionEncoding::Utf16);
        let wire = serde_json::to_value(param).unwrap();
        assert!(wire.get("label").is_none());
    }

    /// Security L1: many parameters all spanning the whole label are capped in
    /// count and in total resolved bytes.
    #[test]
    fn test_signature_parameters_whole_label_pairs_are_bounded() {
        let label = "x".repeat(1000);
        let params = vec![tuple_param(0, 1000); MAX_SIGNATURE_PARAMETERS * 4];
        let out = signature_parameters(params, &label, PositionEncoding::Utf16);
        assert_eq!(out.len(), MAX_SIGNATURE_PARAMETERS);
        let resolved: usize = out
            .iter()
            .filter_map(|p| p.label.as_ref())
            .map(String::len)
            .sum();
        assert!(
            resolved <= label.len() * RESOLVED_LABEL_BUDGET_MULTIPLE,
            "resolved {resolved} bytes"
        );
        assert_eq!(out[0].label.as_deref(), Some(label.as_str()));
        assert!(out[RESOLVED_LABEL_BUDGET_MULTIPLE].label.is_none());
    }

    /// Overlapping and repeated spans within the budget all resolve.
    #[test]
    fn test_signature_parameters_overlapping_ranges_resolve() {
        let label = "f(a, b, ...rest)";
        let params = vec![
            tuple_param(2, 3),
            tuple_param(2, 3),
            tuple_param(2, 15),
            tuple_param(8, 15),
        ];
        let out = signature_parameters(params, label, PositionEncoding::Utf16);
        let labels: Vec<_> = out.iter().map(|p| p.label.as_deref()).collect();
        assert_eq!(
            labels,
            [Some("a"), Some("a"), Some("a, b, ...rest"), Some("...rest")]
        );
    }
}
