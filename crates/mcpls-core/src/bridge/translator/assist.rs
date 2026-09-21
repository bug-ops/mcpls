//! Completions, signature help, and inlay hints handlers.

use lsp_types::{
    CompletionParams, CompletionTriggerKind, InlayHintParams, PartialResultParams,
    SignatureHelpParams as LspSignatureHelpParams, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams,
};

use super::Translator;
use super::dto::{
    Completion, CompletionsResult, InlayHintEntry, InlayHintsResult, Position, SignatureHelpResult,
    SignatureInfo, SignatureParameter, lsp_kind_to_u32,
};
use super::routing::{Capability, IndexingGate};
use crate::config::ToolKind;
use crate::error::{Error, Result};

/// Extract hover contents as markdown string.
/// Convert LSP `Documentation` to a plain string.
fn extract_documentation(doc: lsp_types::Documentation) -> String {
    match doc {
        lsp_types::Documentation::String(s) => s,
        lsp_types::Documentation::MarkupContent(m) => m.value,
    }
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
        file_path: String,
        position: Position,
        trigger: Option<String>,
    ) -> Result<CompletionsResult> {
        let Position { line, character } = position;
        validate_completions_params(trigger.as_deref())?;

        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::Completions,
                Capability::Completions,
                IndexingGate::Required,
            )
            .await?;
        let lsp_position = self
            .encoding_ctx(&server_id)
            .to_lsp(&uri, line, character)
            .await;

        let context = trigger.map(|trigger_char| lsp_types::CompletionContext {
            trigger_kind: CompletionTriggerKind::TriggerCharacter,
            trigger_character: Some(trigger_char),
        });

        let params = CompletionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
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
        file_path: String,
        position: Position,
    ) -> Result<SignatureHelpResult> {
        let Position { line, character } = position;
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::SignatureHelp,
                Capability::SignatureHelp,
                IndexingGate::NotRequired,
            )
            .await?;
        let lsp_position = self
            .encoding_ctx(&server_id)
            .to_lsp(&uri, line, character)
            .await;

        let params = LspSignatureHelpParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
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
                        label: sig.label,
                        documentation: sig.documentation.map(extract_documentation),
                        parameters: sig
                            .parameters
                            .unwrap_or_default()
                            .into_iter()
                            .map(|p| SignatureParameter {
                                label: match p.label {
                                    lsp_types::ParameterInformationLabel::String(s) => s,
                                    lsp_types::ParameterInformationLabel::Tuple((start, end)) => {
                                        format!("[{start},{end}]")
                                    }
                                },
                                documentation: p.documentation.map(extract_documentation),
                            })
                            .collect(),
                    })
                    .collect(),
                active_signature: sig_help.active_signature,
                active_parameter: sig_help.active_parameter.and_then(|ap| match ap {
                    lsp_types::ActiveParameter::Int(n) => Some(n),
                    lsp_types::ActiveParameter::Null => None,
                }),
            },
            None => SignatureHelpResult {
                signatures: vec![],
                active_signature: None,
                active_parameter: None,
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
        file_path: String,
        start: Position,
        end: Position,
    ) -> Result<InlayHintsResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::InlayHints,
                Capability::InlayHints,
                IndexingGate::NotRequired,
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let response_uri = uri.clone();

        let lsp_start = ctx.to_lsp(&uri, start.line, start.character).await;
        let lsp_end = ctx.to_lsp(&uri, end.line, end.character).await;

        let params = InlayHintParams {
            text_document: TextDocumentIdentifier { uri },
            range: lsp_types::Range {
                start: lsp_start,
                end: lsp_end,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::InlayHintRequest>(params, client.request_timeout())
            .await?;

        let mut hints = Vec::new();
        for hint in response.unwrap_or_default() {
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
            positions_degraded: ctx.positions_degraded(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::fs;

    use super::*;
    use crate::bridge::translator::testing::*;

    /// #309 M3: `trigger` has no cap of its own even though the LSP spec
    /// defines it as a single character.
    #[test]
    fn test_validate_completions_params_rejects_oversized_trigger() {
        let trigger = "a".repeat(MAX_TRIGGER_CHARACTER_BYTES + 1);
        let result = validate_completions_params(Some(&trigger));
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
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
                path.to_string_lossy().to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
                None,
            )
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        ));
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
                    .handle_completions(
                        path,
                        Position {
                            line: 1,
                            character: 1,
                        },
                        None,
                    )
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
                    .handle_inlay_hints(path, pos(1, 1), pos(1, 13))
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
}
