//! Public MCP-facing result/data-transfer types returned by the tool-call
//! handlers in the sibling domain modules.

use serde::{Deserialize, Serialize};

/// Position in a document (1-based for MCP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position2D {
    /// Line number (1-based).
    pub line: u32,
    /// Character offset (1-based).
    pub character: u32,
}

/// Range in a document (1-based for MCP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// LSP numeric symbol kind (e.g. 12 for Function).
    pub kind: u32,
    /// More detail for this item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// URI of the document.
    pub uri: String,
    /// Range of the symbol.
    pub range: Range,
    /// Selection range (identifier location).
    ///
    /// Serialized as `selectionRange` (camelCase) so that the value returned by
    /// `prepare_call_hierarchy` round-trips correctly when the MCP client passes
    /// it back to `get_incoming_calls` / `get_outgoing_calls`, which deserialize
    /// it as `lsp_types::CallHierarchyItem` (camelCase).
    #[serde(rename = "selectionRange")]
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

/// Result of server logs request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerLogsResult {
    /// List of log entries.
    pub logs: Vec<crate::bridge::notifications::LogEntry>,
}

/// Result of server messages request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMessagesResult {
    /// List of server messages.
    pub messages: Vec<crate::bridge::notifications::ServerMessage>,
}

/// A single parameter in a signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureParameter {
    /// Label of the parameter.
    pub label: String,
    /// Optional documentation for the parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
}

/// A single signature overload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureInfo {
    /// Full label of the signature.
    pub label: String,
    /// Optional documentation for the signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    /// Parameters of the signature.
    pub parameters: Vec<SignatureParameter>,
}

/// Result of a signature help request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureHelpResult {
    /// Available signatures.
    pub signatures: Vec<SignatureInfo>,
    /// Index of the active signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_signature: Option<u32>,
    /// Index of the active parameter within the active signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

/// Result of a go-to-implementation or go-to-type-definition request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationsResult {
    /// Locations found.
    pub locations: Vec<Location>,
}

/// A single inlay hint entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlayHintEntry {
    /// Position of the hint (1-based MCP).
    pub position: Position2D,
    /// Label text for the hint.
    pub label: String,
    /// Hint kind (1 = Type, 2 = Parameter).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<u8>,
    /// Whether to add a space before the hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding_left: Option<bool>,
    /// Whether to add a space after the hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding_right: Option<bool>,
    /// Tooltip text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
}

/// Result of an inlay hints request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlayHintsResult {
    /// List of inlay hints.
    pub hints: Vec<InlayHintEntry>,
}
