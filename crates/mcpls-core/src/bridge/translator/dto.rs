//! Public MCP-facing result/data-transfer types returned by the tool-call
//! handlers in the sibling domain modules.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Convert an LSP integer-valued enum (`SymbolKind`, `CompletionItemKind`,
/// `InlayHintKind`, ...) to its wire-format `u32`.
///
/// Infallible and needs no fallback value, unlike a `serde_json` roundtrip.
pub(super) fn lsp_kind_to_u32<T: Into<u32>>(kind: T) -> u32 {
    kind.into()
}

/// Position in a document (1-based for MCP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Position2D {
    /// Line number (1-based).
    pub line: u32,
    /// Character offset (1-based).
    pub character: u32,
}

/// A 1-based MCP position taken as input by `Translator::handle_*` methods.
///
/// Kept distinct from [`Position2D`] (which carries an *output* position back
/// to the caller) so passing a position into a handler always goes through a
/// named-field struct literal (`Position { line, character }`) instead of two
/// adjacent bare `u32` arguments -- a call site that swaps `line` and
/// `character` no longer compiles instead of silently sending a wrong
/// position to the LSP server (#322).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Line number (1-based).
    pub line: u32,
    /// Character offset (1-based).
    pub character: u32,
}

/// Range in a document (1-based for MCP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Range {
    /// Start position.
    pub start: Position2D,
    /// End position.
    pub end: Position2D,
}

/// Location in a document.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Location {
    /// URI of the document.
    pub uri: String,
    /// Range within the document.
    pub range: Range,
    /// Whether this location is not provably inside any configured
    /// workspace root (e.g. the standard library or a crates.io dependency).
    ///
    /// Advisory only, not a security/safety guarantee: read-only navigation
    /// results are never filtered by workspace containment (see
    /// `bridge::uri_in_workspace_roots`'s docs for why), so callers that
    /// want to apply their own policy toward out-of-workspace locations can
    /// check this flag. The underlying check is purely lexical -- it does
    /// not resolve symlinks -- so a location reached through a symlinked
    /// workspace root (e.g. macOS's `/var` -> `/private/var`, or a package
    /// manager's symlinked dependency store) can read `true` even though it
    /// is genuinely inside the workspace. Also always `true` when no
    /// workspace roots are configured, consistent with
    /// `bridge::uri_in_workspace_roots`'s fail-closed convention: without a
    /// configured root, nothing can be vouched for as inside the workspace.
    /// Omitted (defaults to `false`) when serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub out_of_workspace: bool,
}

/// `skip_serializing_if` predicate for a `bool` field that should be omitted
/// from the serialized output when `false`.
///
/// Takes `&bool` rather than `bool` because serde's `skip_serializing_if`
/// always calls the predicate with a field reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DefinitionResult {
    /// Locations of the definition.
    pub locations: Vec<Location>,
    /// Whether `locations` was capped below the LSP server's full response
    /// (see `MAX_NORMALIZED_LOCATIONS`, #474) -- if `true`, more locations
    /// exist than are returned here. Omitted (defaults to `false`) when
    /// serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// Result of a references request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReferencesResult {
    /// Locations of all references.
    pub locations: Vec<Location>,
    /// Whether `locations` was capped below the LSP server's full response
    /// (see `MAX_NORMALIZED_LOCATIONS`, #474) -- if `true`, more references
    /// exist than are returned here. Omitted (defaults to `false`) when
    /// serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// Diagnostic severity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
    /// Entries withheld from `changes` -- see [`DroppedEdits`]. A non-empty
    /// value means the rename is incomplete even if `changes` is non-empty,
    /// and callers must not treat this result as the full rename otherwise.
    #[serde(default, skip_serializing_if = "DroppedEdits::is_empty")]
    pub dropped: DroppedEdits,
}

/// Counts of `WorkspaceEdit` entries withheld during conversion to MCP DTOs,
/// broken down by reason (#475).
///
/// `convert_workspace_edit` silently discarded such entries with only a
/// `tracing` log line, so a client applying a [`RenameResult`] or
/// `WorkspaceEditDescription` straight to disk could not tell "nothing to
/// rename" apart from "some of the rename was withheld" -- this makes that
/// distinction visible in the result itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedEdits {
    /// Entries referencing a URI outside every configured workspace root.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub out_of_workspace: usize,
    /// `CreateFile`/`RenameFile`/`DeleteFile` document changes, which mcpls
    /// does not translate into MCP DTOs.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unsupported_file_operation: usize,
    /// `SnippetTextEdit` entries, which mcpls does not translate since it
    /// advertises no `snippetEditSupport`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unsupported_snippet_edit: usize,
}

impl DroppedEdits {
    /// Whether no entries were withheld.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

// Signature required by `#[serde(skip_serializing_if = "is_zero")]` on a `usize` field.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// A completion item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completion {
    /// Label of the completion.
    pub label: String,
    /// LSP numeric completion-item kind (e.g. 3 for Function).
    pub kind: Option<u32>,
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Symbol {
    /// Name of the symbol.
    pub name: String,
    /// LSP numeric symbol kind (e.g. 12 for Function).
    pub kind: u32,
    /// Range of the symbol.
    pub range: Range,
    /// Selection range (identifier location).
    pub selection_range: Range,
    /// Child symbols.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<Self>>,
}

/// Result of a document symbols request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
    /// LSP numeric symbol kind (e.g. 12 for Function).
    pub kind: u32,
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
    /// Whether more symbols matched than are returned in `symbols` -- set
    /// whenever any are dropped, whether by the caller's own smaller
    /// `limit` or by the server-side maximum it's clamped to (see
    /// `MAX_NORMALIZED_LOCATIONS`, #474); this does not distinguish which of
    /// the two caused it. Omitted (defaults to `false`) when serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
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
    /// Entries withheld from `changes` -- see [`DroppedEdits`].
    #[serde(default, skip_serializing_if = "DroppedEdits::is_empty")]
    pub dropped: DroppedEdits,
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
    /// Whether this item is not provably inside any configured workspace
    /// root -- see [`Location::out_of_workspace`] for the exact semantics
    /// and caveats (advisory only, lexical, symlink-unaware).
    #[serde(default, skip_serializing_if = "is_false")]
    pub out_of_workspace: bool,
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
    /// Whether `locations` was capped below the LSP server's full response
    /// (see `MAX_NORMALIZED_LOCATIONS`, #474) -- if `true`, more locations
    /// exist than are returned here. Omitted (defaults to `false`) when
    /// serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// A single inlay hint entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlayHintEntry {
    /// Position of the hint (1-based MCP).
    pub position: Position2D,
    /// Label text for the hint.
    pub label: String,
    /// LSP numeric inlay-hint kind (1 = Type, 2 = Parameter, or a
    /// server-defined custom value).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<u32>,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::lsp_kind_to_u32;

    /// #467 regression: the old `Option<u8>` narrowing silently dropped any
    /// `InlayHintKind::Custom(n)` with `n > 255` to `None`, indistinguishable
    /// from "server sent no kind". `lsp_kind_to_u32` must preserve the full
    /// `u32` value losslessly.
    #[test]
    fn test_lsp_kind_to_u32_preserves_custom_values_above_u8_range() {
        let kind = lsp_types::InlayHintKind::Custom(300);
        assert_eq!(lsp_kind_to_u32(kind), 300u32);
    }
}
