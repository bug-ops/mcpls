//! MCP to LSP translation layer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::{
    CompletionParams, CompletionTriggerKind, DocumentFormattingParams, DocumentSymbol,
    DocumentSymbolParams, FormattingOptions, GotoDefinitionParams, Hover, HoverContents,
    HoverParams as LspHoverParams, MarkedString, PartialResultParams, ReferenceContext,
    ReferenceParams, RenameParams as LspRenameParams, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams, WorkspaceEdit,
    WorkspaceSymbolParams as LspWorkspaceSymbolParams,
};
use serde::{Deserialize, Serialize};
use tokio::time::Duration;

use super::DocumentTracker;
use super::state::detect_language;
use crate::bridge::encoding::mcp_to_lsp_position;
use crate::error::{Error, Result};
use crate::lsp::LspClient;

/// Translator handles MCP tool calls by converting them to LSP requests.
#[derive(Debug)]
pub struct Translator {
    /// LSP clients indexed by language ID.
    lsp_clients: HashMap<String, LspClient>,
    /// Document state tracker.
    document_tracker: DocumentTracker,
    /// Allowed workspace roots for path validation.
    workspace_roots: Vec<PathBuf>,
}

impl Translator {
    /// Create a new translator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lsp_clients: HashMap::new(),
            document_tracker: DocumentTracker::new(),
            workspace_roots: vec![],
        }
    }

    /// Set the workspace roots for path validation.
    pub fn set_workspace_roots(&mut self, roots: Vec<PathBuf>) {
        self.workspace_roots = roots;
    }

    /// Register an LSP client for a language.
    pub fn register_client(&mut self, language_id: String, client: LspClient) {
        self.lsp_clients.insert(language_id, client);
    }

    /// Get the document tracker.
    #[must_use]
    pub const fn document_tracker(&self) -> &DocumentTracker {
        &self.document_tracker
    }

    /// Get a mutable reference to the document tracker.
    pub const fn document_tracker_mut(&mut self) -> &mut DocumentTracker {
        &mut self.document_tracker
    }

    // TODO: These methods will be implemented in Phase 3-5
    // Initialize and shutdown are now handled by LspServer in lifecycle.rs

    // Future implementation will use LspServer instead of LspClient directly
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}

/// Position in a document (1-based for MCP).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position2D {
    /// Line number (1-based).
    pub line: u32,
    /// Character offset (1-based).
    pub character: u32,
}

/// Range in a document (1-based for MCP).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Range {
    /// Start position.
    pub start: Position2D,
    /// End position.
    pub end: Position2D,
}

/// Location in a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    /// URI of the document.
    pub uri: String,
    /// Range within the document.
    pub range: Range,
}

/// Result of a hover request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoverResult {
    /// Hover contents as markdown string.
    pub contents: String,
    /// Optional range the hover applies to.
    pub range: Option<Range>,
}

/// Result of a definition request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefinitionResult {
    /// Locations of the definition.
    pub locations: Vec<Location>,
}

/// Result of a references request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencesResult {
    /// Locations of all references.
    pub locations: Vec<Location>,
}

/// Diagnostic severity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    /// Error diagnostic.
    Error,
    /// Warning diagnostic.
    Warning,
    /// Informational diagnostic.
    Information,
    /// Hint diagnostic.
    Hint,
}

/// A single diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Range where the diagnostic applies.
    pub range: Range,
    /// Severity of the diagnostic.
    pub severity: DiagnosticSeverity,
    /// Diagnostic message.
    pub message: String,
    /// Optional diagnostic code.
    pub code: Option<String>,
}

/// Result of a diagnostics request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsResult {
    /// List of diagnostics for the document.
    pub diagnostics: Vec<Diagnostic>,
}

/// A text edit operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEdit {
    /// Range to replace.
    pub range: Range,
    /// New text.
    pub new_text: String,
}

/// Changes to a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChanges {
    /// URI of the document.
    pub uri: String,
    /// List of edits to apply.
    pub edits: Vec<TextEdit>,
}

/// Result of a rename request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameResult {
    /// Changes to apply across documents.
    pub changes: Vec<DocumentChanges>,
}

/// A completion item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completion {
    /// Label of the completion.
    pub label: String,
    /// Kind of completion.
    pub kind: Option<String>,
    /// Detail information.
    pub detail: Option<String>,
    /// Documentation.
    pub documentation: Option<String>,
}

/// Result of a completions request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionsResult {
    /// List of completion items.
    pub items: Vec<Completion>,
}

/// A document symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    /// Name of the symbol.
    pub name: String,
    /// Kind of symbol.
    pub kind: String,
    /// Range of the symbol.
    pub range: Range,
    /// Selection range (identifier location).
    pub selection_range: Range,
    /// Child symbols.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<Self>>,
}

/// Result of a document symbols request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSymbolsResult {
    /// List of symbols in the document.
    pub symbols: Vec<Symbol>,
}

/// Result of a format document request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatDocumentResult {
    /// List of edits to format the document.
    pub edits: Vec<TextEdit>,
}

/// A workspace symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSymbol {
    /// Name of the symbol.
    pub name: String,
    /// Kind of symbol.
    pub kind: String,
    /// Location of the symbol.
    pub location: Location,
    /// Optional container name (parent scope).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
}

/// Result of workspace symbol search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSymbolResult {
    /// List of symbols found.
    pub symbols: Vec<WorkspaceSymbol>,
}

impl Translator {
    /// Validate that a path is within allowed workspace boundaries.
    ///
    /// # Errors
    ///
    /// Returns `Error::PathOutsideWorkspace` if the path is outside all workspace roots.
    fn validate_path(&self, path: &Path) -> Result<PathBuf> {
        let canonical = path.canonicalize().map_err(|e| Error::FileIo {
            path: path.to_path_buf(),
            source: e,
        })?;

        // If no workspace roots configured, allow any path (backward compatibility)
        if self.workspace_roots.is_empty() {
            return Ok(canonical);
        }

        // Check if path is within any workspace root
        for root in &self.workspace_roots {
            if let Ok(canonical_root) = root.canonicalize() {
                if canonical.starts_with(&canonical_root) {
                    return Ok(canonical);
                }
            }
        }

        Err(Error::PathOutsideWorkspace(path.to_path_buf()))
    }

    /// Get a cloned LSP client for a file path based on language detection.
    fn get_client_for_file(&self, path: &Path) -> Result<LspClient> {
        let language_id = detect_language(path);
        self.lsp_clients
            .get(&language_id)
            .cloned()
            .ok_or(Error::NoServerForLanguage(language_id))
    }

    /// Handle hover request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_hover(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<HoverResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = LspHoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Hover> = client
            .request("textDocument/hover", params, timeout_duration)
            .await?;

        let result = match response {
            Some(hover) => {
                let contents = extract_hover_contents(hover.contents);
                let range = hover.range.map(normalize_range);
                HoverResult { contents, range }
            }
            None => HoverResult {
                contents: "No hover information available".to_string(),
                range: None,
            },
        };

        Ok(result)
    }

    /// Handle definition request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_definition(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<DefinitionResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::GotoDefinitionResponse> = client
            .request("textDocument/definition", params, timeout_duration)
            .await?;

        let locations = match response {
            Some(lsp_types::GotoDefinitionResponse::Scalar(loc)) => vec![loc],
            Some(lsp_types::GotoDefinitionResponse::Array(locs)) => locs,
            Some(lsp_types::GotoDefinitionResponse::Link(links)) => links
                .into_iter()
                .map(|link| lsp_types::Location {
                    uri: link.target_uri,
                    range: link.target_selection_range,
                })
                .collect(),
            None => vec![],
        };

        let result = DefinitionResult {
            locations: locations
                .into_iter()
                .map(|loc| Location {
                    uri: loc.uri.to_string(),
                    range: normalize_range(loc.range),
                })
                .collect(),
        };

        Ok(result)
    }

    /// Handle references request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_references(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<ReferencesResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration,
            },
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<lsp_types::Location>> = client
            .request("textDocument/references", params, timeout_duration)
            .await?;

        let locations = response.unwrap_or_default();

        let result = ReferencesResult {
            locations: locations
                .into_iter()
                .map(|loc| Location {
                    uri: loc.uri.to_string(),
                    range: normalize_range(loc.range),
                })
                .collect(),
        };

        Ok(result)
    }

    /// Handle diagnostics request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_diagnostics(&mut self, file_path: String) -> Result<DiagnosticsResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;

        let params = lsp_types::DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier { uri },
            identifier: None,
            previous_result_id: None,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: lsp_types::DocumentDiagnosticReportResult = client
            .request("textDocument/diagnostic", params, timeout_duration)
            .await?;

        let diagnostics = match response {
            lsp_types::DocumentDiagnosticReportResult::Report(report) => match report {
                lsp_types::DocumentDiagnosticReport::Full(full) => {
                    full.full_document_diagnostic_report.items
                }
                lsp_types::DocumentDiagnosticReport::Unchanged(_) => vec![],
            },
            lsp_types::DocumentDiagnosticReportResult::Partial(_) => vec![],
        };

        let result = DiagnosticsResult {
            diagnostics: diagnostics
                .into_iter()
                .map(|diag| Diagnostic {
                    range: normalize_range(diag.range),
                    severity: match diag.severity {
                        Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
                        Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
                        Some(lsp_types::DiagnosticSeverity::INFORMATION) => {
                            DiagnosticSeverity::Information
                        }
                        Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
                        _ => DiagnosticSeverity::Information,
                    },
                    message: diag.message,
                    code: diag.code.map(|c| match c {
                        lsp_types::NumberOrString::Number(n) => n.to_string(),
                        lsp_types::NumberOrString::String(s) => s,
                    }),
                })
                .collect(),
        };

        Ok(result)
    }

    /// Handle rename request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_rename(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<RenameResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = LspRenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            new_name,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<WorkspaceEdit> = client
            .request("textDocument/rename", params, timeout_duration)
            .await?;

        let changes = if let Some(edit) = response {
            let mut result_changes = Vec::new();

            if let Some(changes_map) = edit.changes {
                for (uri, edits) in changes_map {
                    result_changes.push(DocumentChanges {
                        uri: uri.to_string(),
                        edits: edits
                            .into_iter()
                            .map(|edit| TextEdit {
                                range: normalize_range(edit.range),
                                new_text: edit.new_text,
                            })
                            .collect(),
                    });
                }
            }

            result_changes
        } else {
            vec![]
        };

        Ok(RenameResult { changes })
    }

    /// Handle completions request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_completions(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
        trigger: Option<String>,
    ) -> Result<CompletionsResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

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

        let timeout_duration = Duration::from_secs(10);
        let response: Option<lsp_types::CompletionResponse> = client
            .request("textDocument/completion", params, timeout_duration)
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

    /// Handle document symbols request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_document_symbols(
        &mut self,
        file_path: String,
    ) -> Result<DocumentSymbolsResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;

        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::DocumentSymbolResponse> = client
            .request("textDocument/documentSymbol", params, timeout_duration)
            .await?;

        let symbols = match response {
            Some(lsp_types::DocumentSymbolResponse::Flat(symbols)) => symbols
                .into_iter()
                .map(|sym| Symbol {
                    name: sym.name,
                    kind: format!("{:?}", sym.kind),
                    range: normalize_range(sym.location.range),
                    selection_range: normalize_range(sym.location.range),
                    children: None,
                })
                .collect(),
            Some(lsp_types::DocumentSymbolResponse::Nested(symbols)) => {
                symbols.into_iter().map(convert_document_symbol).collect()
            }
            None => vec![],
        };

        Ok(DocumentSymbolsResult { symbols })
    }

    /// Handle format document request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_format_document(
        &mut self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<FormatDocumentResult> {
        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;

        let params = DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri },
            options: FormattingOptions {
                tab_size,
                insert_spaces,
                ..Default::default()
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<lsp_types::TextEdit>> = client
            .request("textDocument/formatting", params, timeout_duration)
            .await?;

        let edits = response.unwrap_or_default();

        let result = FormatDocumentResult {
            edits: edits
                .into_iter()
                .map(|edit| TextEdit {
                    range: normalize_range(edit.range),
                    new_text: edit.new_text,
                })
                .collect(),
        };

        Ok(result)
    }

    /// Handle workspace symbol search.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or no server is configured.
    pub async fn handle_workspace_symbol(
        &mut self,
        query: String,
        kind_filter: Option<String>,
        limit: u32,
    ) -> Result<WorkspaceSymbolResult> {
        const MAX_QUERY_LENGTH: usize = 1000;
        const VALID_SYMBOL_KINDS: &[&str] = &[
            "File",
            "Module",
            "Namespace",
            "Package",
            "Class",
            "Method",
            "Property",
            "Field",
            "Constructor",
            "Enum",
            "Interface",
            "Function",
            "Variable",
            "Constant",
            "String",
            "Number",
            "Boolean",
            "Array",
            "Object",
            "Key",
            "Null",
            "EnumMember",
            "Struct",
            "Event",
            "Operator",
            "TypeParameter",
        ];

        // Validate query length
        if query.len() > MAX_QUERY_LENGTH {
            return Err(Error::InvalidToolParams(format!(
                "Query too long: {} chars (max {MAX_QUERY_LENGTH})",
                query.len()
            )));
        }

        // Validate kind filter
        if let Some(ref kind) = kind_filter {
            if !VALID_SYMBOL_KINDS
                .iter()
                .any(|k| k.eq_ignore_ascii_case(kind))
            {
                return Err(Error::InvalidToolParams(format!(
                    "Invalid kind_filter: '{kind}'. Valid values: {VALID_SYMBOL_KINDS:?}"
                )));
            }
        }

        // Workspace search requires at least one LSP client
        let client = self
            .lsp_clients
            .values()
            .next()
            .cloned()
            .ok_or(Error::NoServerConfigured)?;

        let params = LspWorkspaceSymbolParams {
            query,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<lsp_types::SymbolInformation>> = client
            .request("workspace/symbol", params, timeout_duration)
            .await?;

        let mut symbols: Vec<WorkspaceSymbol> = response
            .unwrap_or_default()
            .into_iter()
            .map(|sym| WorkspaceSymbol {
                name: sym.name,
                kind: format!("{:?}", sym.kind),
                location: Location {
                    uri: sym.location.uri.to_string(),
                    range: normalize_range(sym.location.range),
                },
                container_name: sym.container_name,
            })
            .collect();

        // Apply kind filter if specified
        if let Some(kind) = kind_filter {
            symbols.retain(|s| s.kind.eq_ignore_ascii_case(&kind));
        }

        // Limit results
        symbols.truncate(limit as usize);

        Ok(WorkspaceSymbolResult { symbols })
    }
}

/// Extract hover contents as markdown string.
fn extract_hover_contents(contents: HoverContents) -> String {
    match contents {
        HoverContents::Scalar(marked_string) => marked_string_to_string(marked_string),
        HoverContents::Array(marked_strings) => marked_strings
            .into_iter()
            .map(marked_string_to_string)
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(markup) => markup.value,
    }
}

/// Convert a marked string to a plain string.
fn marked_string_to_string(marked: MarkedString) -> String {
    match marked {
        MarkedString::String(s) => s,
        MarkedString::LanguageString(ls) => format!("```{}\n{}\n```", ls.language, ls.value),
    }
}

/// Convert LSP range to MCP range (0-based to 1-based).
const fn normalize_range(range: lsp_types::Range) -> Range {
    Range {
        start: Position2D {
            line: range.start.line + 1,
            character: range.start.character + 1,
        },
        end: Position2D {
            line: range.end.line + 1,
            character: range.end.character + 1,
        },
    }
}

/// Convert LSP document symbol to MCP symbol.
fn convert_document_symbol(symbol: DocumentSymbol) -> Symbol {
    Symbol {
        name: symbol.name,
        kind: format!("{:?}", symbol.kind),
        range: normalize_range(symbol.range),
        selection_range: normalize_range(symbol.selection_range),
        children: symbol
            .children
            .map(|children| children.into_iter().map(convert_document_symbol).collect()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_translator_new() {
        let translator = Translator::new();
        assert_eq!(translator.workspace_roots.len(), 0);
        assert_eq!(translator.lsp_clients.len(), 0);
    }

    #[test]
    fn test_set_workspace_roots() {
        let mut translator = Translator::new();
        let roots = vec![PathBuf::from("/test/root1"), PathBuf::from("/test/root2")];
        translator.set_workspace_roots(roots.clone());
        assert_eq!(translator.workspace_roots, roots);
    }

    #[test]
    fn test_validate_path_no_workspace_roots() {
        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // With no workspace roots, any valid path should be accepted
        let result = translator.validate_path(&test_file);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_within_workspace() {
        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().to_path_buf();
        translator.set_workspace_roots(vec![workspace_root]);

        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator.validate_path(&test_file);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_outside_workspace() {
        let mut translator = Translator::new();
        let temp_dir1 = TempDir::new().unwrap();
        let temp_dir2 = TempDir::new().unwrap();

        // Set workspace root to temp_dir1
        translator.set_workspace_roots(vec![temp_dir1.path().to_path_buf()]);

        // Create file in temp_dir2 (outside workspace)
        let test_file = temp_dir2.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator.validate_path(&test_file);
        assert!(matches!(result, Err(Error::PathOutsideWorkspace(_))));
    }

    #[test]
    fn test_normalize_range() {
        let lsp_range = lsp_types::Range {
            start: lsp_types::Position {
                line: 0,
                character: 0,
            },
            end: lsp_types::Position {
                line: 2,
                character: 5,
            },
        };

        let mcp_range = normalize_range(lsp_range);
        assert_eq!(mcp_range.start.line, 1);
        assert_eq!(mcp_range.start.character, 1);
        assert_eq!(mcp_range.end.line, 3);
        assert_eq!(mcp_range.end.character, 6);
    }

    #[test]
    fn test_extract_hover_contents_string() {
        let marked_string = lsp_types::MarkedString::String("Test hover".to_string());
        let contents = lsp_types::HoverContents::Scalar(marked_string);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "Test hover");
    }

    #[test]
    fn test_extract_hover_contents_language_string() {
        let marked_string = lsp_types::MarkedString::LanguageString(lsp_types::LanguageString {
            language: "rust".to_string(),
            value: "fn main() {}".to_string(),
        });
        let contents = lsp_types::HoverContents::Scalar(marked_string);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "```rust\nfn main() {}\n```");
    }

    #[test]
    fn test_extract_hover_contents_markup() {
        let markup = lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::Markdown,
            value: "# Documentation".to_string(),
        };
        let contents = lsp_types::HoverContents::Markup(markup);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "# Documentation");
    }

    #[tokio::test]
    async fn test_handle_workspace_symbol_no_server() {
        let mut translator = Translator::new();
        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;
        assert!(matches!(result, Err(Error::NoServerConfigured)));
    }
}
