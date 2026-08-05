//! Rename, format-document, and code-actions handlers.

use lsp_types::{
    DocumentFormattingParams, FormattingOptions, PartialResultParams,
    RenameParams as LspRenameParams, TextDocumentIdentifier, TextDocumentPositionParams,
    WorkDoneProgressParams, WorkspaceEdit,
};

use super::Translator;
use super::diagnostics::diagnostic_to_mcp;
use super::dto::{
    CodeAction, CodeActionsResult, CommandDescription, DocumentChanges, FormatDocumentResult,
    RenameResult, TextEdit, WorkspaceEditDescription,
};
use super::encoding_ctx::EncodingCtx;
use super::routing::{MAX_POSITION_VALUE, MAX_RANGE_LINES};
use crate::config::ToolKind;
use crate::error::{Error, Result};

/// Convert LSP range to MCP range (0-based to 1-based).
/// Validate parameters for `handle_code_actions`.
fn validate_code_action_params(
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
    kind_filter: Option<&str>,
) -> Result<()> {
    const VALID_ACTION_KINDS: &[&str] = &[
        "quickfix",
        "refactor",
        "refactor.extract",
        "refactor.inline",
        "refactor.rewrite",
        "source",
        "source.organizeImports",
    ];

    if let Some(kind) = kind_filter
        && !VALID_ACTION_KINDS
            .iter()
            .any(|k| k.eq_ignore_ascii_case(kind))
    {
        return Err(Error::InvalidToolParams(format!(
            "Invalid kind_filter: '{kind}'. Valid values: {VALID_ACTION_KINDS:?}"
        )));
    }

    if start_line < 1 || start_character < 1 || end_line < 1 || end_character < 1 {
        return Err(Error::InvalidToolParams(
            "Line and character positions must be >= 1".to_string(),
        ));
    }

    if start_line > MAX_POSITION_VALUE
        || start_character > MAX_POSITION_VALUE
        || end_line > MAX_POSITION_VALUE
        || end_character > MAX_POSITION_VALUE
    {
        return Err(Error::InvalidToolParams(format!(
            "Position values must be <= {MAX_POSITION_VALUE}"
        )));
    }

    if end_line.saturating_sub(start_line) > MAX_RANGE_LINES {
        return Err(Error::InvalidToolParams(format!(
            "Range size must be <= {MAX_RANGE_LINES} lines"
        )));
    }

    if start_line > end_line || (start_line == end_line && start_character > end_character) {
        return Err(Error::InvalidToolParams(
            "Start position must be before or equal to end position".to_string(),
        ));
    }

    Ok(())
}

/// Maximum length, in bytes, of a `rename_symbol` `new_name` parameter.
///
/// `new_name` is forwarded to the routed LSP server as-is with no inherent
/// bound of its own -- unlike `workspace_symbol_search`'s `query` (see
/// `validate_workspace_symbol_params`), it previously relied entirely on
/// outer transport limits (#309). No real identifier approaches this length
/// in any language mcpls targets.
pub(super) const MAX_NEW_NAME_LENGTH: usize = 1_000;

/// Validate parameters for `handle_rename`.
fn validate_rename_params(new_name: &str) -> Result<()> {
    if new_name.len() > MAX_NEW_NAME_LENGTH {
        return Err(Error::InvalidToolParams(format!(
            "new_name too long: {} bytes (max {MAX_NEW_NAME_LENGTH})",
            new_name.len()
        )));
    }
    Ok(())
}

/// Convert LSP code action to MCP code action. `uri` is the queried
/// document's own URI, used for the action's `diagnostics` (always scoped to
/// the requested document); `edit.changes` carries its own per-file URIs.
async fn convert_code_action(
    action: lsp_types::CodeAction,
    ctx: &EncodingCtx,
    uri: &lsp_types::Uri,
) -> CodeAction {
    let diagnostics = match action.diagnostics {
        Some(diags) => {
            let mut result = Vec::with_capacity(diags.len());
            for d in &diags {
                result.push(diagnostic_to_mcp(d, ctx, uri).await);
            }
            result
        }
        None => Vec::new(),
    };

    let edit = match action.edit {
        Some(edit) => {
            let changes = match edit.changes {
                Some(changes_map) => {
                    let mut result = Vec::with_capacity(changes_map.len());
                    for (uri, edits) in changes_map {
                        let mut text_edits = Vec::with_capacity(edits.len());
                        for e in edits {
                            text_edits.push(TextEdit {
                                range: ctx.normalize_range(&uri, e.range).await,
                                new_text: e.new_text,
                            });
                        }
                        result.push(DocumentChanges {
                            uri: uri.to_string(),
                            edits: text_edits,
                        });
                    }
                    result
                }
                None => Vec::new(),
            };
            Some(WorkspaceEditDescription { changes })
        }
        None => None,
    };

    let command = action.command.map(|cmd| {
        let arguments = cmd.arguments.unwrap_or_else(Vec::new);
        CommandDescription {
            title: cmd.title,
            command: cmd.command,
            arguments,
        }
    });

    CodeAction {
        title: action.title,
        kind: action.kind.map(|k| k.as_str().to_string()),
        diagnostics,
        edit,
        command,
        is_preferred: action.is_preferred.unwrap_or(false),
    }
}

impl Translator {
    /// Handle rename request.
    ///
    /// # Errors
    ///
    /// Returns an error if `new_name` exceeds the maximum allowed length,
    /// the LSP request fails, the file cannot be opened, or the routed
    /// server does not advertise `renameProvider` support.
    pub async fn handle_rename(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<RenameResult> {
        validate_rename_params(&new_name)?;

        let (server_id, client, uri) = self
            .prepare_gated_document(&file_path, ToolKind::Rename, "renameProvider", |caps| {
                matches!(
                    caps.rename_provider,
                    Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                )
            })
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = LspRenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            new_name,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response: Option<WorkspaceEdit> = client
            .request("textDocument/rename", params, client.request_timeout())
            .await?;

        let changes = if let Some(edit) = response {
            let mut result_changes = Vec::new();

            // Prefer the legacy `changes` map (HashMap<Uri, Vec<TextEdit>>).
            if let Some(changes_map) = edit.changes {
                for (uri, edits) in changes_map {
                    let mut text_edits = Vec::with_capacity(edits.len());
                    for e in edits {
                        text_edits.push(TextEdit {
                            range: ctx.normalize_range(&uri, e.range).await,
                            new_text: e.new_text,
                        });
                    }
                    result_changes.push(DocumentChanges {
                        uri: uri.to_string(),
                        edits: text_edits,
                    });
                }
            }

            // Also handle `documentChanges` (array format returned by rust-analyzer).
            if result_changes.is_empty() {
                let text_doc_edits = match edit.document_changes {
                    Some(lsp_types::DocumentChanges::Edits(edits)) => edits,
                    Some(lsp_types::DocumentChanges::Operations(ops)) => ops
                        .into_iter()
                        .filter_map(|op| match op {
                            lsp_types::DocumentChangeOperation::Edit(e) => Some(e),
                            lsp_types::DocumentChangeOperation::Op(_) => None,
                        })
                        .collect(),
                    None => vec![],
                };
                for tde in text_doc_edits {
                    let edit_uri = &tde.text_document.uri;
                    let mut text_edits = Vec::with_capacity(tde.edits.len());
                    for one_of in tde.edits {
                        text_edits.push(match one_of {
                            lsp_types::OneOf::Left(te) => TextEdit {
                                range: ctx.normalize_range(edit_uri, te.range).await,
                                new_text: te.new_text,
                            },
                            lsp_types::OneOf::Right(ate) => TextEdit {
                                range: ctx.normalize_range(edit_uri, ate.text_edit.range).await,
                                new_text: ate.text_edit.new_text,
                            },
                        });
                    }
                    result_changes.push(DocumentChanges {
                        uri: edit_uri.to_string(),
                        edits: text_edits,
                    });
                }
            }

            result_changes
        } else {
            vec![]
        };

        Ok(RenameResult { changes })
    }

    /// Handle format document request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `documentFormattingProvider` support.
    pub async fn handle_format_document(
        &self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<FormatDocumentResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::FormatDocument,
                "documentFormattingProvider",
                |caps| {
                    matches!(
                        caps.document_formatting_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let response_uri = uri.clone();

        let params = DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri },
            options: FormattingOptions {
                tab_size,
                insert_spaces,
                ..Default::default()
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response: Option<Vec<lsp_types::TextEdit>> = client
            .request("textDocument/formatting", params, client.request_timeout())
            .await?;

        let edits = response.unwrap_or_default();

        let mut result_edits = Vec::with_capacity(edits.len());
        for edit in edits {
            result_edits.push(TextEdit {
                range: ctx.normalize_range(&response_uri, edit.range).await,
                new_text: edit.new_text,
            });
        }
        let result = FormatDocumentResult {
            edits: result_edits,
        };

        Ok(result)
    }

    /// Handle code actions request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `codeActionProvider` support.
    pub async fn handle_code_actions(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
    ) -> Result<CodeActionsResult> {
        validate_code_action_params(
            start_line,
            start_character,
            end_line,
            end_character,
            kind_filter.as_deref(),
        )?;

        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::CodeActions,
                "codeActionProvider",
                |caps| {
                    matches!(
                        caps.code_action_provider,
                        Some(
                            lsp_types::CodeActionProviderCapability::Simple(true)
                                | lsp_types::CodeActionProviderCapability::Options(_)
                        )
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let response_uri = uri.clone();

        let range = lsp_types::Range {
            start: ctx.to_lsp(&uri, start_line, start_character).await,
            end: ctx.to_lsp(&uri, end_line, end_character).await,
        };

        // Build context with optional kind filter
        let only = kind_filter.map(|k| vec![lsp_types::CodeActionKind::from(k)]);

        // Pass empty diagnostics context — rust-analyzer generates code actions
        // based on cursor position and its internal analysis state, not on the
        // passed diagnostics.  Passing stale cached diagnostics (which may lack
        // the internal `data` field ra uses for fix mapping) suppresses results.
        let context_diagnostics: Vec<lsp_types::Diagnostic> = vec![];

        let params = lsp_types::CodeActionParams {
            text_document: TextDocumentIdentifier { uri },
            range,
            context: lsp_types::CodeActionContext {
                diagnostics: context_diagnostics,
                only,
                trigger_kind: Some(lsp_types::CodeActionTriggerKind::INVOKED),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<lsp_types::CodeActionResponse> = client
            .request("textDocument/codeAction", params, client.request_timeout())
            .await?;
        let response_vec = response.unwrap_or_default();
        let mut actions = Vec::with_capacity(response_vec.len());

        for action_or_command in response_vec {
            let action = match action_or_command {
                lsp_types::CodeActionOrCommand::CodeAction(action) => {
                    convert_code_action(action, &ctx, &response_uri).await
                }
                lsp_types::CodeActionOrCommand::Command(cmd) => {
                    let arguments = cmd.arguments.unwrap_or_else(Vec::new);
                    CodeAction {
                        title: cmd.title.clone(),
                        kind: None,
                        diagnostics: Vec::new(),
                        edit: None,
                        command: Some(CommandDescription {
                            title: cmd.title,
                            command: cmd.command,
                            arguments,
                        }),
                        is_preferred: false,
                    }
                }
            };
            actions.push(action);
        }

        Ok(CodeActionsResult { actions })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::fs;

    use super::*;
    use crate::bridge::translator::dto::DiagnosticSeverity;
    use crate::bridge::translator::testing::*;

    /// #309: `new_name` has no inherent bound of its own and is forwarded to
    /// the LSP server as-is, so it must be rejected before that happens.
    #[test]
    fn test_validate_rename_params_rejects_oversized_new_name() {
        let new_name = "a".repeat(MAX_NEW_NAME_LENGTH + 1);
        let result = validate_rename_params(&new_name);
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[test]
    fn test_validate_rename_params_accepts_name_at_exact_limit() {
        let new_name = "a".repeat(MAX_NEW_NAME_LENGTH);
        assert!(validate_rename_params(&new_name).is_ok());
    }

    #[test]
    fn test_validate_rename_params_accepts_typical_identifier() {
        assert!(validate_rename_params("my_variable").is_ok());
    }

    /// #309: length checks have no lower bound -- an empty `new_name` is
    /// syntactically valid input for this validator (semantic rejection of
    /// an empty rename target, if desired, is a separate concern).
    #[test]
    fn test_validate_rename_params_accepts_empty_string() {
        assert!(validate_rename_params("").is_ok());
    }

    #[tokio::test]
    async fn test_handle_code_actions_invalid_kind() {
        let translator = Translator::new();
        let result = translator
            .handle_code_actions(
                "/tmp/test.rs".to_string(),
                1,
                1,
                1,
                10,
                Some("invalid_kind".to_string()),
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_quickfix() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("quickfix".to_string()),
            )
            .await;
        // Will fail due to no LSP server, but validates kind is accepted
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_refactor() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("refactor".to_string()),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_refactor_extract() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("refactor.extract".to_string()),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_source() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("source.organizeImports".to_string()),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_invalid_range_zero() {
        let translator = Translator::new();
        let result = translator
            .handle_code_actions("/tmp/test.rs".to_string(), 0, 1, 1, 10, None)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_invalid_range_order() {
        let translator = Translator::new();
        let result = translator
            .handle_code_actions("/tmp/test.rs".to_string(), 10, 5, 5, 1, None)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_empty_range() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // Empty range (same position) should be valid
        let result = translator
            .handle_code_actions(test_file.to_str().unwrap().to_string(), 1, 5, 1, 5, None)
            .await;
        // Will fail due to no LSP server, but validates range is accepted
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_convert_code_action_minimal() {
        let lsp_action = lsp_types::CodeAction {
            title: "Fix issue".to_string(),
            kind: None,
            diagnostics: None,
            edit: None,
            command: None,
            is_preferred: None,
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action, &test_ctx(), &test_uri()).await;
        assert_eq!(result.title, "Fix issue");
        assert!(result.kind.is_none());
        assert!(result.diagnostics.is_empty());
        assert!(result.edit.is_none());
        assert!(result.command.is_none());
        assert!(!result.is_preferred);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn test_convert_code_action_with_diagnostics_all_severities() {
        let lsp_diagnostics = vec![
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
                message: "Error message".to_string(),
                code: Some(lsp_types::NumberOrString::Number(1)),
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
                message: "Warning message".to_string(),
                code: Some(lsp_types::NumberOrString::String("W001".to_string())),
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
                message: "Info message".to_string(),
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
                message: "Hint message".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
        ];

        let lsp_action = lsp_types::CodeAction {
            title: "Fix all issues".to_string(),
            kind: Some(lsp_types::CodeActionKind::QUICKFIX),
            diagnostics: Some(lsp_diagnostics),
            edit: None,
            command: None,
            is_preferred: None,
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action, &test_ctx(), &test_uri()).await;
        assert_eq!(result.diagnostics.len(), 4);
        assert!(matches!(
            result.diagnostics[0].severity,
            DiagnosticSeverity::Error
        ));
        assert!(matches!(
            result.diagnostics[1].severity,
            DiagnosticSeverity::Warning
        ));
        assert!(matches!(
            result.diagnostics[2].severity,
            DiagnosticSeverity::Information
        ));
        assert!(matches!(
            result.diagnostics[3].severity,
            DiagnosticSeverity::Hint
        ));
        assert_eq!(result.diagnostics[0].code, Some("1".to_string()));
        assert_eq!(result.diagnostics[1].code, Some("W001".to_string()));
    }

    #[tokio::test]
    #[allow(clippy::mutable_key_type)]
    async fn test_convert_code_action_with_workspace_edit() {
        use std::collections::HashMap;
        use std::str::FromStr;

        let uri = lsp_types::Uri::from_str("file:///test.rs").unwrap();
        let mut changes_map = HashMap::new();
        changes_map.insert(
            uri,
            vec![lsp_types::TextEdit {
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
                new_text: "fixed".to_string(),
            }],
        );

        let lsp_action = lsp_types::CodeAction {
            title: "Apply fix".to_string(),
            kind: Some(lsp_types::CodeActionKind::QUICKFIX),
            diagnostics: None,
            edit: Some(lsp_types::WorkspaceEdit {
                changes: Some(changes_map),
                document_changes: None,
                change_annotations: None,
            }),
            command: None,
            is_preferred: Some(true),
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action, &test_ctx(), &test_uri()).await;
        assert!(result.edit.is_some());
        let edit = result.edit.unwrap();
        assert_eq!(edit.changes.len(), 1);
        assert_eq!(edit.changes[0].uri, "file:///test.rs");
        assert_eq!(edit.changes[0].edits.len(), 1);
        assert_eq!(edit.changes[0].edits[0].new_text, "fixed");
        assert!(result.is_preferred);
    }

    #[tokio::test]
    async fn test_convert_code_action_with_command() {
        let lsp_action = lsp_types::CodeAction {
            title: "Run command".to_string(),
            kind: Some(lsp_types::CodeActionKind::REFACTOR),
            diagnostics: None,
            edit: None,
            command: Some(lsp_types::Command {
                title: "Execute refactor".to_string(),
                command: "refactor.extract".to_string(),
                arguments: Some(vec![serde_json::json!("arg1"), serde_json::json!(42)]),
            }),
            is_preferred: None,
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action, &test_ctx(), &test_uri()).await;
        assert!(result.command.is_some());
        let cmd = result.command.unwrap();
        assert_eq!(cmd.title, "Execute refactor");
        assert_eq!(cmd.command, "refactor.extract");
        assert_eq!(cmd.arguments.len(), 2);
    }
}
