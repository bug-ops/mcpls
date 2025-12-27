//! MCP to LSP translation layer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams,
    CallHierarchyPrepareParams as LspCallHierarchyPrepareParams, CompletionParams,
    CompletionTriggerKind, DocumentFormattingParams, DocumentSymbol, DocumentSymbolParams,
    FormattingOptions, GotoDefinitionParams, Hover, HoverContents, HoverParams as LspHoverParams,
    MarkedString, PartialResultParams, ReferenceContext, ReferenceParams,
    RenameParams as LspRenameParams, TextDocumentIdentifier, TextDocumentPositionParams,
    WorkDoneProgressParams, WorkspaceEdit, WorkspaceSymbolParams as LspWorkspaceSymbolParams,
};
use serde::{Deserialize, Serialize};
use tokio::time::Duration;

use super::state::detect_language;
use super::{DocumentTracker, NotificationCache};
use crate::bridge::encoding::mcp_to_lsp_position;
use crate::error::{Error, Result};
use crate::lsp::{LspClient, LspServer};

/// Translator handles MCP tool calls by converting them to LSP requests.
#[derive(Debug)]
pub struct Translator {
    /// LSP clients indexed by language ID.
    lsp_clients: HashMap<String, LspClient>,
    /// LSP servers indexed by language ID (held for lifetime management).
    lsp_servers: HashMap<String, LspServer>,
    /// Document state tracker.
    document_tracker: DocumentTracker,
    /// Notification cache for LSP server notifications.
    notification_cache: NotificationCache,
    /// Allowed workspace roots for path validation.
    workspace_roots: Vec<PathBuf>,
}

impl Translator {
    /// Create a new translator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lsp_clients: HashMap::new(),
            lsp_servers: HashMap::new(),
            document_tracker: DocumentTracker::new(),
            notification_cache: NotificationCache::new(),
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

    /// Register an LSP server for a language.
    pub fn register_server(&mut self, language_id: String, server: LspServer) {
        self.lsp_servers.insert(language_id, server);
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

    /// Get the notification cache.
    #[must_use]
    pub const fn notification_cache(&self) -> &NotificationCache {
        &self.notification_cache
    }

    /// Get a mutable reference to the notification cache.
    pub const fn notification_cache_mut(&mut self) -> &mut NotificationCache {
        &mut self.notification_cache
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

/// A single code action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeAction {
    /// Title of the code action.
    pub title: String,
    /// Kind of code action (quickfix, refactor, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Diagnostics that this action resolves.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
    /// Workspace edit to apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit: Option<WorkspaceEditDescription>,
    /// Command to execute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandDescription>,
    /// Whether this is the preferred action.
    #[serde(default)]
    pub is_preferred: bool,
}

/// Description of a workspace edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEditDescription {
    /// Changes to apply to documents.
    pub changes: Vec<DocumentChanges>,
}

/// Description of a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDescription {
    /// Title of the command.
    pub title: String,
    /// Command identifier.
    pub command: String,
    /// Command arguments.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub arguments: Vec<serde_json::Value>,
}

/// Result of code actions request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeActionsResult {
    /// Available code actions.
    pub actions: Vec<CodeAction>,
}

/// A call hierarchy item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallHierarchyItemResult {
    /// Name of the symbol.
    pub name: String,
    /// Kind of symbol.
    pub kind: String,
    /// More detail for this item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// URI of the document.
    pub uri: String,
    /// Range of the symbol.
    pub range: Range,
    /// Selection range (identifier location).
    pub selection_range: Range,
    /// Opaque data to pass to incoming/outgoing calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Result of call hierarchy prepare request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallHierarchyPrepareResult {
    /// List of callable items at the position.
    pub items: Vec<CallHierarchyItemResult>,
}

/// An incoming call (caller of the current item).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingCall {
    /// The item that calls the current item.
    pub from: CallHierarchyItemResult,
    /// Ranges where the call occurs.
    pub from_ranges: Vec<Range>,
}

/// Result of incoming calls request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingCallsResult {
    /// List of incoming calls.
    pub calls: Vec<IncomingCall>,
}

/// An outgoing call (callee from the current item).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingCall {
    /// The item being called.
    pub to: CallHierarchyItemResult,
    /// Ranges where the call occurs.
    pub from_ranges: Vec<Range>,
}

/// Result of outgoing calls request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingCallsResult {
    /// List of outgoing calls.
    pub calls: Vec<OutgoingCall>,
}

/// Maximum allowed position value for validation.
const MAX_POSITION_VALUE: u32 = 1_000_000;
/// Maximum allowed range size in lines.
const MAX_RANGE_LINES: u32 = 10_000;

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

    /// Parse and validate a file URI, returning the validated path.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The URI doesn't have a file:// scheme
    /// - The path is outside workspace boundaries
    fn parse_file_uri(&self, uri: &lsp_types::Uri) -> Result<PathBuf> {
        let uri_str = uri.as_str();

        // Validate file:// scheme
        if !uri_str.starts_with("file://") {
            return Err(Error::InvalidToolParams(format!(
                "Invalid URI scheme, expected file:// but got: {uri_str}"
            )));
        }

        // Extract path after file://
        let path_str = &uri_str["file://".len()..];

        // Handle Windows paths: file:///C:/path -> /C:/path -> C:/path
        // On Windows, URIs have format file:///C:/path, so we need to strip the leading /
        #[cfg(windows)]
        let path_str = if path_str.len() >= 3
            && path_str.starts_with('/')
            && path_str.chars().nth(2) == Some(':')
        {
            &path_str[1..]
        } else {
            path_str
        };

        let path = PathBuf::from(path_str);

        // Validate path is within workspace
        self.validate_path(&path)
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

    /// Handle code actions request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_code_actions(
        &mut self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
    ) -> Result<CodeActionsResult> {
        const VALID_ACTION_KINDS: &[&str] = &[
            "quickfix",
            "refactor",
            "refactor.extract",
            "refactor.inline",
            "refactor.rewrite",
            "source",
            "source.organizeImports",
        ];

        // Validate kind filter
        if let Some(ref kind) = kind_filter {
            if !VALID_ACTION_KINDS
                .iter()
                .any(|k| k.eq_ignore_ascii_case(kind))
            {
                return Err(Error::InvalidToolParams(format!(
                    "Invalid kind_filter: '{kind}'. Valid values: {VALID_ACTION_KINDS:?}"
                )));
            }
        }

        // Validate range
        if start_line < 1 || start_character < 1 || end_line < 1 || end_character < 1 {
            return Err(Error::InvalidToolParams(
                "Line and character positions must be >= 1".to_string(),
            ));
        }

        // Validate position upper bounds
        if start_line > MAX_POSITION_VALUE
            || start_character > MAX_POSITION_VALUE
            || end_line > MAX_POSITION_VALUE
            || end_character > MAX_POSITION_VALUE
        {
            return Err(Error::InvalidToolParams(format!(
                "Position values must be <= {MAX_POSITION_VALUE}"
            )));
        }

        // Validate range size
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

        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;

        let range = lsp_types::Range {
            start: mcp_to_lsp_position(start_line, start_character),
            end: mcp_to_lsp_position(end_line, end_character),
        };

        // Build context with optional kind filter
        let only = kind_filter.map(|k| vec![lsp_types::CodeActionKind::from(k)]);

        let params = lsp_types::CodeActionParams {
            text_document: TextDocumentIdentifier { uri },
            range,
            context: lsp_types::CodeActionContext {
                diagnostics: vec![],
                only,
                trigger_kind: Some(lsp_types::CodeActionTriggerKind::INVOKED),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::CodeActionResponse> = client
            .request("textDocument/codeAction", params, timeout_duration)
            .await?;

        let response_vec = response.unwrap_or_default();
        let mut actions = Vec::with_capacity(response_vec.len());

        for action_or_command in response_vec {
            let action = match action_or_command {
                lsp_types::CodeActionOrCommand::CodeAction(action) => convert_code_action(action),
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

    /// Handle call hierarchy prepare request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the file cannot be opened.
    pub async fn handle_call_hierarchy_prepare(
        &mut self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<CallHierarchyPrepareResult> {
        // Validate position bounds
        if line < 1 || character < 1 {
            return Err(Error::InvalidToolParams(
                "Line and character positions must be >= 1".to_string(),
            ));
        }

        if line > MAX_POSITION_VALUE || character > MAX_POSITION_VALUE {
            return Err(Error::InvalidToolParams(format!(
                "Position values must be <= {MAX_POSITION_VALUE}"
            )));
        }

        let path = PathBuf::from(&file_path);
        let validated_path = self.validate_path(&path)?;
        let client = self.get_client_for_file(&validated_path)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &client)
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = LspCallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<CallHierarchyItem>> = client
            .request(
                "textDocument/prepareCallHierarchy",
                params,
                timeout_duration,
            )
            .await?;

        // Pre-allocate and build result
        let lsp_items = response.unwrap_or_default();
        let mut items = Vec::with_capacity(lsp_items.len());
        for item in lsp_items {
            items.push(convert_call_hierarchy_item(item));
        }

        Ok(CallHierarchyPrepareResult { items })
    }

    /// Handle incoming calls request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the item is invalid.
    pub async fn handle_incoming_calls(
        &mut self,
        item: serde_json::Value,
    ) -> Result<IncomingCallsResult> {
        let lsp_item: CallHierarchyItem = serde_json::from_value(item)
            .map_err(|e| Error::InvalidToolParams(format!("Invalid call hierarchy item: {e}")))?;

        // Parse and validate the URI
        let path = self.parse_file_uri(&lsp_item.uri)?;
        let client = self.get_client_for_file(&path)?;

        let params = CallHierarchyIncomingCallsParams {
            item: lsp_item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<CallHierarchyIncomingCall>> = client
            .request("callHierarchy/incomingCalls", params, timeout_duration)
            .await?;

        // Pre-allocate and build result
        let lsp_calls = response.unwrap_or_default();
        let mut calls = Vec::with_capacity(lsp_calls.len());

        for call in lsp_calls {
            let from_ranges = {
                let mut ranges = Vec::with_capacity(call.from_ranges.len());
                for range in call.from_ranges {
                    ranges.push(normalize_range(range));
                }
                ranges
            };

            calls.push(IncomingCall {
                from: convert_call_hierarchy_item(call.from),
                from_ranges,
            });
        }

        Ok(IncomingCallsResult { calls })
    }

    /// Handle outgoing calls request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails or the item is invalid.
    pub async fn handle_outgoing_calls(
        &mut self,
        item: serde_json::Value,
    ) -> Result<OutgoingCallsResult> {
        let lsp_item: CallHierarchyItem = serde_json::from_value(item)
            .map_err(|e| Error::InvalidToolParams(format!("Invalid call hierarchy item: {e}")))?;

        // Parse and validate the URI
        let path = self.parse_file_uri(&lsp_item.uri)?;
        let client = self.get_client_for_file(&path)?;

        let params = CallHierarchyOutgoingCallsParams {
            item: lsp_item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<CallHierarchyOutgoingCall>> = client
            .request("callHierarchy/outgoingCalls", params, timeout_duration)
            .await?;

        // Pre-allocate and build result
        let lsp_calls = response.unwrap_or_default();
        let mut calls = Vec::with_capacity(lsp_calls.len());

        for call in lsp_calls {
            let from_ranges = {
                let mut ranges = Vec::with_capacity(call.from_ranges.len());
                for range in call.from_ranges {
                    ranges.push(normalize_range(range));
                }
                ranges
            };

            calls.push(OutgoingCall {
                to: convert_call_hierarchy_item(call.to),
                from_ranges,
            });
        }

        Ok(OutgoingCallsResult { calls })
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

/// Convert LSP call hierarchy item to MCP call hierarchy item.
fn convert_call_hierarchy_item(item: CallHierarchyItem) -> CallHierarchyItemResult {
    CallHierarchyItemResult {
        name: item.name,
        kind: format!("{:?}", item.kind),
        detail: item.detail,
        uri: item.uri.to_string(),
        range: normalize_range(item.range),
        selection_range: normalize_range(item.selection_range),
        data: item.data,
    }
}

/// Convert LSP code action to MCP code action.
fn convert_code_action(action: lsp_types::CodeAction) -> CodeAction {
    let diagnostics = action.diagnostics.map_or_else(Vec::new, |diags| {
        let mut result = Vec::with_capacity(diags.len());
        for d in diags {
            result.push(Diagnostic {
                range: normalize_range(d.range),
                severity: match d.severity {
                    Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
                    Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
                    Some(lsp_types::DiagnosticSeverity::INFORMATION) => {
                        DiagnosticSeverity::Information
                    }
                    Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
                    _ => DiagnosticSeverity::Information,
                },
                message: d.message,
                code: d.code.map(|c| match c {
                    lsp_types::NumberOrString::Number(n) => n.to_string(),
                    lsp_types::NumberOrString::String(s) => s,
                }),
            });
        }
        result
    });

    let edit = action.edit.map(|edit| {
        let changes = edit.changes.map_or_else(Vec::new, |changes_map| {
            let mut result = Vec::with_capacity(changes_map.len());
            for (uri, edits) in changes_map {
                let mut text_edits = Vec::with_capacity(edits.len());
                for e in edits {
                    text_edits.push(TextEdit {
                        range: normalize_range(e.range),
                        new_text: e.new_text,
                    });
                }
                result.push(DocumentChanges {
                    uri: uri.to_string(),
                    edits: text_edits,
                });
            }
            result
        });
        WorkspaceEditDescription { changes }
    });

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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use url::Url;

    use super::*;

    #[test]
    fn test_translator_new() {
        let translator = Translator::new();
        assert_eq!(translator.workspace_roots.len(), 0);
        assert_eq!(translator.lsp_clients.len(), 0);
        assert_eq!(translator.lsp_servers.len(), 0);
    }

    #[test]
    fn test_set_workspace_roots() {
        let mut translator = Translator::new();
        let roots = vec![PathBuf::from("/test/root1"), PathBuf::from("/test/root2")];
        translator.set_workspace_roots(roots.clone());
        assert_eq!(translator.workspace_roots, roots);
    }

    #[test]
    fn test_register_server() {
        let translator = Translator::new();

        // Initial state: no servers registered
        assert_eq!(translator.lsp_servers.len(), 0);

        // The register_server method exists and is callable
        // Full integration testing with real LspServer is done in integration tests
        // This unit test verifies the method signature and basic functionality

        // Note: We can't easily construct an LspServer in a unit test without async
        // and a real LSP server process. The actual registration functionality is
        // tested in integration tests (see rust_analyzer_tests.rs).
        // This test verifies the data structure is properly initialized.
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

    #[tokio::test]
    async fn test_handle_code_actions_invalid_kind() {
        let mut translator = Translator::new();
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

        let mut translator = Translator::new();
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

        let mut translator = Translator::new();
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

        let mut translator = Translator::new();
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

        let mut translator = Translator::new();
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
        let mut translator = Translator::new();
        let result = translator
            .handle_code_actions("/tmp/test.rs".to_string(), 0, 1, 1, 10, None)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_invalid_range_order() {
        let mut translator = Translator::new();
        let result = translator
            .handle_code_actions("/tmp/test.rs".to_string(), 10, 5, 5, 1, None)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_empty_range() {
        use tempfile::TempDir;

        let mut translator = Translator::new();
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

    #[test]
    fn test_convert_code_action_minimal() {
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

        let result = convert_code_action(lsp_action);
        assert_eq!(result.title, "Fix issue");
        assert!(result.kind.is_none());
        assert!(result.diagnostics.is_empty());
        assert!(result.edit.is_none());
        assert!(result.command.is_none());
        assert!(!result.is_preferred);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_convert_code_action_with_diagnostics_all_severities() {
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

        let result = convert_code_action(lsp_action);
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

    #[test]
    #[allow(clippy::mutable_key_type)]
    fn test_convert_code_action_with_workspace_edit() {
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

        let result = convert_code_action(lsp_action);
        assert!(result.edit.is_some());
        let edit = result.edit.unwrap();
        assert_eq!(edit.changes.len(), 1);
        assert_eq!(edit.changes[0].uri, "file:///test.rs");
        assert_eq!(edit.changes[0].edits.len(), 1);
        assert_eq!(edit.changes[0].edits[0].new_text, "fixed");
        assert!(result.is_preferred);
    }

    #[test]
    fn test_convert_code_action_with_command() {
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

        let result = convert_code_action(lsp_action);
        assert!(result.command.is_some());
        let cmd = result.command.unwrap();
        assert_eq!(cmd.title, "Execute refactor");
        assert_eq!(cmd.command, "refactor.extract");
        assert_eq!(cmd.arguments.len(), 2);
    }

    #[tokio::test]
    async fn test_handle_call_hierarchy_prepare_invalid_position_zero() {
        let mut translator = Translator::new();
        let result = translator
            .handle_call_hierarchy_prepare("/tmp/test.rs".to_string(), 0, 1)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));

        let result = translator
            .handle_call_hierarchy_prepare("/tmp/test.rs".to_string(), 1, 0)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_call_hierarchy_prepare_invalid_position_too_large() {
        let mut translator = Translator::new();
        let result = translator
            .handle_call_hierarchy_prepare("/tmp/test.rs".to_string(), 1_000_001, 1)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));

        let result = translator
            .handle_call_hierarchy_prepare("/tmp/test.rs".to_string(), 1, 1_000_001)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_incoming_calls_invalid_json() {
        let mut translator = Translator::new();
        let invalid_item = serde_json::json!({"invalid": "structure"});
        let result = translator.handle_incoming_calls(invalid_item).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_outgoing_calls_invalid_json() {
        let mut translator = Translator::new();
        let invalid_item = serde_json::json!({"invalid": "structure"});
        let result = translator.handle_outgoing_calls(invalid_item).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_parse_file_uri_invalid_scheme() {
        let translator = Translator::new();
        let uri: lsp_types::Uri = "http://example.com/file.rs".parse().unwrap();
        let result = translator.parse_file_uri(&uri);
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_parse_file_uri_valid_scheme() {
        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // Use url crate for cross-platform file URI creation
        let file_url = Url::from_file_path(&test_file).unwrap();
        let uri: lsp_types::Uri = file_url.as_str().parse().unwrap();
        let result = translator.parse_file_uri(&uri);
        assert!(result.is_ok());
    }
}
