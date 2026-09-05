//! Completions, signature help, and inlay hints handlers.

use lsp_types::{
    CompletionParams, CompletionTriggerKind, InlayHintLabel, InlayHintParams, PartialResultParams,
    SignatureHelpParams as LspSignatureHelpParams, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams,
};

use super::Translator;
use super::dto::{
    Completion, CompletionsResult, InlayHintEntry, InlayHintsResult, Position, SignatureHelpResult,
    SignatureInfo, SignatureParameter,
};
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
    /// the LSP request fails, the file cannot be opened, or the routed
    /// server does not advertise `completionProvider` support.
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
                "completionProvider",
                |caps| caps.completion_provider.is_some(),
            )
            .await?;
        let lsp_position = self
            .encoding_ctx(&server_id)
            .to_lsp(&uri, line, character)
            .await;

        let context = trigger.map(|trigger_char| lsp_types::CompletionContext {
            trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
            trigger_character: Some(trigger_char),
        });

        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context,
        };

        let response: Option<lsp_types::CompletionResponse> = client
            .request(
                "textDocument/completion",
                params,
                client.completion_timeout(),
            )
            .await?;

        let items = match response {
            Some(lsp_types::CompletionResponse::Array(items)) => items,
            Some(lsp_types::CompletionResponse::List(list)) => list.items,
            None => vec![],
        };

        let result = CompletionsResult {
            items: items
                .into_iter()
                .map(|item| Completion {
                    label: item.label,
                    kind: item.kind.map(|k| format!("{k:?}")),
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
                "signatureHelpProvider",
                |caps| caps.signature_help_provider.is_some(),
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

        let response: Option<lsp_types::SignatureHelp> = client
            .request(
                "textDocument/signatureHelp",
                params,
                client.request_timeout(),
            )
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
                                    lsp_types::ParameterLabel::Simple(s) => s,
                                    lsp_types::ParameterLabel::LabelOffsets([start, end]) => {
                                        format!("[{start},{end}]")
                                    }
                                },
                                documentation: p.documentation.map(extract_documentation),
                            })
                            .collect(),
                    })
                    .collect(),
                active_signature: sig_help.active_signature,
                active_parameter: sig_help.active_parameter,
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
                "inlayHintProvider",
                |caps| {
                    matches!(
                        caps.inlay_hint_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
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

        let response: Option<Vec<lsp_types::InlayHint>> = client
            .request("textDocument/inlayHint", params, client.request_timeout())
            .await?;

        let mut hints = Vec::new();
        for hint in response.unwrap_or_default() {
            let position = ctx.to_mcp(&response_uri, hint.position).await;
            let label = match hint.label {
                InlayHintLabel::String(s) => s,
                InlayHintLabel::LabelParts(parts) => parts
                    .into_iter()
                    .map(|p| p.value)
                    .collect::<Vec<_>>()
                    .concat(),
            };
            let tooltip = hint.tooltip.map(|t| match t {
                lsp_types::InlayHintTooltip::String(s) => s,
                lsp_types::InlayHintTooltip::MarkupContent(m) => m.value,
            });
            hints.push(InlayHintEntry {
                position,
                label,
                kind: hint.kind.and_then(|k| {
                    serde_json::to_value(k)
                        .ok()
                        .and_then(|v| v.as_i64())
                        .and_then(|n| u8::try_from(n).ok())
                }),
                padding_left: hint.padding_left,
                padding_right: hint.padding_right,
                tooltip,
            });
        }

        Ok(InlayHintsResult { hints })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
}
