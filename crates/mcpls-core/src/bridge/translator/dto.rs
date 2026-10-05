//! Public MCP-facing result/data-transfer types returned by the tool-call
//! handlers in the sibling domain modules.

use std::num::NonZeroU32;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::enclosing::{ContextualDiagnostic, ContextualLocation, EnrichmentSummary};
use crate::redaction::{Redactions, ServerText};

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

/// Largest line or character value a client may supply.
pub const MAX_POSITION_VALUE: u32 = 1_000_000;

/// Largest line span of a range given to a range-taking tool.
pub const MAX_RANGE_LINES: u32 = 10_000;

/// Why a client-supplied position was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InvalidPosition {
    /// A line or character below 1; positions are 1-based.
    #[error("Line and character positions must be >= 1")]
    ZeroBased,
    /// A line or character above [`MAX_POSITION_VALUE`].
    #[error("Position values must be <= {MAX_POSITION_VALUE}")]
    TooLarge,
}

/// Why a client-supplied range was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InvalidRange {
    /// The start position is invalid.
    #[error("invalid range start: {0}")]
    Start(InvalidPosition),
    /// The end position is invalid.
    #[error("invalid range end: {0}")]
    End(InvalidPosition),
    /// The start lies after the end.
    #[error("Start position must be before or equal to end position")]
    Reversed,
    /// The range spans more than [`MAX_RANGE_LINES`] lines.
    #[error("Range size must be <= {MAX_RANGE_LINES} lines")]
    TooManyLines,
}

/// A 1-based MCP position taken as input by `Translator::handle_*` methods.
///
/// Kept distinct from [`Position2D`] (which carries an *output* position back
/// to the caller) and made of two [`NonZeroU32`]s, so a zero position cannot
/// exist and a call site that swaps `line` and `character` still names them
/// (#322). A position typed by a client goes through [`Self::from_client`],
/// the only fallible constructor; positions derived from server output
/// convert infallibly through `Position::from_server_output`.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::{InvalidPosition, Position};
///
/// let position = Position::from_client(3, 5)?;
/// assert_eq!((position.line().get(), position.character().get()), (3, 5));
/// assert_eq!(Position::from_client(0, 1), Err(InvalidPosition::ZeroBased));
/// # Ok::<(), InvalidPosition>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    line: NonZeroU32,
    character: NonZeroU32,
}

impl Position {
    /// Builds a position from client-supplied 1-based numbers.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidPosition::ZeroBased`] for a zero line or character
    /// and [`InvalidPosition::TooLarge`] above [`MAX_POSITION_VALUE`].
    pub const fn from_client(line: u32, character: u32) -> Result<Self, InvalidPosition> {
        let (Some(line), Some(character)) = (NonZeroU32::new(line), NonZeroU32::new(character))
        else {
            return Err(InvalidPosition::ZeroBased);
        };
        if line.get() > MAX_POSITION_VALUE || character.get() > MAX_POSITION_VALUE {
            return Err(InvalidPosition::TooLarge);
        }
        Ok(Self { line, character })
    }

    /// Line number (1-based).
    #[must_use]
    pub const fn line(self) -> NonZeroU32 {
        self.line
    }

    /// Character offset (1-based).
    #[must_use]
    pub const fn character(self) -> NonZeroU32 {
        self.character
    }

    /// Zero-based LSP line.
    #[must_use]
    pub const fn lsp_line(self) -> u32 {
        self.line.get().saturating_sub(1)
    }

    /// Zero-based character offset, still in UTF-16 units.
    #[must_use]
    pub const fn lsp_character(self) -> u32 {
        self.character.get().saturating_sub(1)
    }

    /// Reuses a position taken from server output, which is already 1-based
    /// MCP form, as a handler input position. Deliberately not a `From`
    /// impl: a position typed by a client must go through
    /// [`Self::from_client`]. A zero, which converted LSP output never
    /// carries, clamps to 1.
    pub(crate) fn from_server_output(position: &Position2D) -> Self {
        let clamp = |value: u32| NonZeroU32::new(value).unwrap_or(NonZeroU32::MIN);
        Self {
            line: clamp(position.line),
            character: clamp(position.character),
        }
    }

    /// Builds a fixture position, panicking on an invalid one.
    #[cfg(test)]
    #[allow(clippy::expect_used)]
    pub(crate) fn at(line: u32, character: u32) -> Self {
        Self::from_client(line, character).expect("fixture position is valid")
    }
}

/// An ordered pair of positions, `start <= end`.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::{InvalidRange, Position, PositionRange};
///
/// let start = Position::from_client(1, 1).unwrap();
/// let end = Position::from_client(2, 4).unwrap();
/// assert!(PositionRange::new(start, end).is_ok());
/// assert_eq!(PositionRange::new(end, start), Err(InvalidRange::Reversed));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionRange {
    start: Position,
    end: Position,
}

impl PositionRange {
    /// Builds an ordered range.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidRange::Reversed`] when `start` lies after `end`.
    pub fn new(start: Position, end: Position) -> Result<Self, InvalidRange> {
        let ordered = (start.line, start.character) <= (end.line, end.character);
        if ordered {
            Ok(Self { start, end })
        } else {
            Err(InvalidRange::Reversed)
        }
    }

    /// Builds a range from client-supplied 1-based numbers.
    ///
    /// # Errors
    ///
    /// Returns the invalid endpoint or [`InvalidRange::Reversed`].
    pub fn from_client(
        (start_line, start_character): (u32, u32),
        (end_line, end_character): (u32, u32),
    ) -> Result<Self, InvalidRange> {
        let start =
            Position::from_client(start_line, start_character).map_err(InvalidRange::Start)?;
        let end = Position::from_client(end_line, end_character).map_err(InvalidRange::End)?;
        Self::new(start, end)
    }

    /// First position of the range.
    #[must_use]
    pub const fn start(self) -> Position {
        self.start
    }

    /// Last position of the range.
    #[must_use]
    pub const fn end(self) -> Position {
        self.end
    }
}

/// A [`PositionRange`] spanning at most [`MAX_RANGE_LINES`] lines, taken by
/// the tools whose server-side cost grows with the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedRange(PositionRange);

impl BoundedRange {
    /// The underlying ordered range.
    #[must_use]
    pub const fn range(self) -> PositionRange {
        self.0
    }
}

impl TryFrom<PositionRange> for BoundedRange {
    type Error = InvalidRange;

    fn try_from(range: PositionRange) -> Result<Self, Self::Error> {
        let span = range.end.line.get().saturating_sub(range.start.line.get());
        if span > MAX_RANGE_LINES {
            Err(InvalidRange::TooManyLines)
        } else {
            Ok(Self(range))
        }
    }
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
    /// results are never filtered by workspace containment (a legitimate
    /// result routinely points into the standard library or a dependency),
    /// so callers that want to apply their own policy toward out-of-workspace
    /// locations can check this flag. The check is purely lexical -- it does
    /// not resolve symlinks. A root counts together with the spellings it
    /// was configured or launched under (for example a symlinked path or the
    /// shell's logical working directory), but a location reached through any
    /// other symlink (e.g. a package manager's symlinked dependency store)
    /// can read `true` even though it is genuinely inside the workspace. Also
    /// always `true` when no workspace roots are configured: without a
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

/// How a non-UTF-16 server's `character` offsets became inexact in a result.
// Variant order matters: derived `Ord` makes `Request` > `Response`, so `Option::max` keeps the worse.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PositionDegradation {
    /// Only offsets in the returned data may be inexact. The result still
    /// describes the symbol that was asked about; keep it and treat the
    /// returned `character` values as approximate.
    Response,
    /// The queried position was sent to the server unconverted, so the result
    /// may describe a different symbol. Do not trust it.
    Request,
}

/// Result of a hover request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HoverResult {
    /// Hover contents as markdown string.
    pub contents: String,
    /// Optional range the hover applies to.
    pub range: Option<Range>,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// Result of a definition request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DefinitionResult {
    /// Locations of the definition.
    pub locations: Vec<ContextualLocation>,
    /// Whether `locations` was capped below the LSP server's full response
    /// (see `MAX_NORMALIZED_LOCATIONS`, #474) -- if `true`, more locations
    /// exist than are returned here. Omitted (defaults to `false`) when
    /// serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "PositionDegradation")]
    pub positions_degraded: Option<PositionDegradation>,
    /// Set only when `context: "enclosing_symbol"` was requested and at least one
    /// item was looked up: how many files were enriched or skipped, and whether
    /// the file cap or time budget cut it short.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "EnrichmentSummary")]
    pub enrichment: Option<EnrichmentSummary>,
}

/// Result of a references request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReferencesResult {
    /// Locations of all references.
    pub locations: Vec<ContextualLocation>,
    /// Whether `locations` was capped below the LSP server's full response
    /// (see `MAX_NORMALIZED_LOCATIONS`, #474) -- if `true`, more references
    /// exist than are returned here. Omitted (defaults to `false`) when
    /// serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "PositionDegradation")]
    pub positions_degraded: Option<PositionDegradation>,
    /// Set only when `context: "enclosing_symbol"` was requested and at least one
    /// item was looked up: how many files were enriched or skipped, and whether
    /// the file cap or time budget cut it short.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "EnrichmentSummary")]
    pub enrichment: Option<EnrichmentSummary>,
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
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "PositionDegradation")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// Result of a `get_diagnostics` request: [`DiagnosticsResult`] plus the
/// opt-in enclosing-symbol context.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DocumentDiagnosticsResult {
    /// List of diagnostics for the document.
    pub diagnostics: Vec<ContextualDiagnostic>,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "PositionDegradation")]
    pub positions_degraded: Option<PositionDegradation>,
    /// Set only when `context: "enclosing_symbol"` was requested and the file has
    /// diagnostics: how many files were enriched or skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "EnrichmentSummary")]
    pub enrichment: Option<EnrichmentSummary>,
}

/// A text edit operation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TextEdit {
    /// Range to replace.
    pub range: Range,
    /// New text.
    pub new_text: String,
}

/// Changes to a document.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DocumentChanges {
    /// URI of the document.
    pub uri: String,
    /// List of edits to apply.
    pub edits: Vec<TextEdit>,
}

/// Result of a rename request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RenameResult {
    /// Changes to apply across documents.
    pub changes: Vec<DocumentChanges>,
    /// Entries withheld from `changes` -- see [`DroppedEdits`]. A non-empty
    /// value means the rename is incomplete even if `changes` is non-empty,
    /// and callers must not treat this result as the full rename otherwise.
    #[serde(default, skip_serializing_if = "DroppedEdits::is_empty")]
    pub dropped: DroppedEdits,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// Counts of `WorkspaceEdit` entries withheld during conversion to MCP DTOs,
/// broken down by reason (#475).
///
/// `convert_workspace_edit` silently discarded such entries with only a
/// `tracing` log line, so a client applying a [`RenameResult`] or
/// `WorkspaceEditDescription` straight to disk could not tell "nothing to
/// rename" apart from "some of the rename was withheld" -- this makes that
/// distinction visible in the result itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    /// Edits not admitted because the response exceeded
    /// `MAX_NORMALIZED_LOCATIONS` (#487), including whole files skipped once
    /// the budget was spent.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub exceeds_item_cap: usize,
    /// `documentChanges` text-document edits ignored because a non-empty
    /// `changes` map took precedence and did not already name their URI
    /// (#498).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub shadowed_by_changes: usize,
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompletionsResult {
    /// List of completion items.
    pub items: Vec<Completion>,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions_degraded: Option<PositionDegradation>,
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
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "PositionDegradation")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// Result of a format document request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FormatDocumentResult {
    /// List of edits to format the document.
    pub edits: Vec<TextEdit>,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// A workspace symbol.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// A single code action.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceEditDescription {
    /// Changes to apply to documents.
    pub changes: Vec<DocumentChanges>,
    /// Entries withheld from `changes` -- see [`DroppedEdits`].
    #[serde(default, skip_serializing_if = "DroppedEdits::is_empty")]
    pub dropped: DroppedEdits,
}

/// Description of a command.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CodeActionsResult {
    /// Available code actions.
    pub actions: Vec<CodeAction>,
    /// Whether `actions`, their diagnostics, or their edits were capped below
    /// the LSP server's full response (see `MAX_NORMALIZED_LOCATIONS`, #487);
    /// a per-action `dropped.exceeds_item_cap` pinpoints dropped edits.
    /// Omitted (defaults to `false`) when serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// Result of call hierarchy prepare request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CallHierarchyPrepareResult {
    /// List of callable items at the position.
    pub items: Vec<HierarchyItem>,
    /// Whether `items` was capped below the LSP server's full response (see
    /// `MAX_NORMALIZED_LOCATIONS`, #516). Omitted (defaults to `false`) when
    /// serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// An incoming call (caller of the current item).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IncomingCall {
    /// The item that calls the current item.
    pub from: HierarchyItem,
    /// Ranges where the call occurs.
    pub from_ranges: Vec<Range>,
}

/// Result of incoming calls request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IncomingCallsResult {
    /// List of incoming calls.
    pub calls: Vec<IncomingCall>,
    /// Whether `calls` (together with their `from_ranges`) was capped below
    /// the LSP server's full response (see `MAX_NORMALIZED_LOCATIONS`, #487).
    /// Omitted (defaults to `false`) when serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// An outgoing call (callee from the current item).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OutgoingCall {
    /// The item being called.
    pub to: HierarchyItem,
    /// Ranges where the call occurs.
    pub from_ranges: Vec<Range>,
}

/// Result of outgoing calls request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OutgoingCallsResult {
    /// List of outgoing calls.
    pub calls: Vec<OutgoingCall>,
    /// Whether `calls` (together with their `from_ranges`) was capped below
    /// the LSP server's full response (see `MAX_NORMALIZED_LOCATIONS`, #487).
    /// Omitted (defaults to `false`) when serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// Result of server logs request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ServerLogsResult {
    /// List of log entries.
    pub logs: Vec<crate::bridge::notifications::LogEntry>,
}

/// Result of server messages request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ServerMessagesResult {
    /// List of server messages.
    pub messages: Vec<crate::bridge::notifications::ServerMessage>,
}

/// A single parameter in a signature.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SignatureParameter {
    /// Label of the parameter, or `None` when the server gave an offset pair
    /// into the signature label that does not resolve to a substring of it
    /// (out of range, inside a character, or reversed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Optional documentation for the parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
}

/// A single signature overload.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SignatureHelpResult {
    /// Available signatures.
    pub signatures: Vec<SignatureInfo>,
    /// Index of the active signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_signature: Option<u32>,
    /// Index of the active parameter within the active signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// Result of a go-to-implementation or go-to-type-definition request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LocationsResult {
    /// Locations found.
    pub locations: Vec<ContextualLocation>,
    /// Whether `locations` was capped below the LSP server's full response
    /// (see `MAX_NORMALIZED_LOCATIONS`, #474) -- if `true`, more locations
    /// exist than are returned here. Omitted (defaults to `false`) when
    /// serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions_degraded: Option<PositionDegradation>,
    /// Set only when `context: "enclosing_symbol"` was requested and at least one
    /// item was looked up: how many files were enriched or skipped, and whether
    /// the file cap or time budget cut it short.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "EnrichmentSummary")]
    pub enrichment: Option<EnrichmentSummary>,
}

/// A single inlay hint entry.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InlayHintsResult {
    /// List of inlay hints.
    pub hints: Vec<InlayHintEntry>,
    /// Whether `hints` was capped below the LSP server's full response (see
    /// `MAX_NORMALIZED_LOCATIONS`, #487). Omitted (defaults to `false`) when
    /// serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// A call or type hierarchy item.
///
/// Returned by the hierarchy tools and accepted back as the typed `item`
/// input of the walking tools (`get_incoming_calls`, `get_outgoing_calls`,
/// `get_supertypes`, `get_subtypes`). LSP's call and type hierarchy items carry the same fields, so one shape
/// serves both. `data` is opaque to the caller and meaningful only to the
/// server that produced the item.
///
/// Lines and columns above [`MAX_POSITION_VALUE`] cannot be passed back, so
/// an item from a `prepare_*` tool whose range crosses that column (for
/// example on a minified line) is rejected by the walking tools.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::HierarchyItem;
///
/// let item: HierarchyItem = serde_json::from_value(serde_json::json!({
///     "name": "Base", "kind": 5, "uri": "file:///a.cpp",
///     "range": {"start": {"line": 1, "character": 1}, "end": {"line": 2, "character": 1}},
///     "selectionRange": {"start": {"line": 1, "character": 7}, "end": {"line": 1, "character": 11}},
/// }))
/// .unwrap();
/// assert_eq!(item.name, "Base");
/// assert!(serde_json::from_value::<HierarchyItem>(serde_json::json!({})).is_err());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HierarchyItem {
    /// Name of the symbol.
    pub name: String,
    /// LSP numeric symbol kind (e.g. 12 for Function, 5 for Class).
    pub kind: u32,
    /// More detail for this item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// URI of the document.
    pub uri: String,
    /// Range of the symbol.
    pub range: Range,
    /// Selection range (identifier location).
    ///
    /// Serialized as `selectionRange` (camelCase) so that a returned item
    /// round-trips when the MCP client passes it back to a walking tool.
    #[serde(rename = "selectionRange")]
    pub selection_range: Range,
    /// Opaque data to pass back unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// Whether this item is not provably inside any configured workspace
    /// root -- see [`Location::out_of_workspace`] for the exact semantics
    /// and caveats (advisory only, lexical, symlink-unaware). Ignored on
    /// input.
    #[serde(default, skip_serializing_if = "is_false")]
    pub out_of_workspace: bool,
}

/// Why a client-supplied [`HierarchyItem`] was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InvalidHierarchyItem {
    /// The item's `range` is invalid.
    #[error("invalid hierarchy item range: {0}")]
    Range(InvalidRange),
    /// The item's `selectionRange` is invalid.
    #[error("invalid hierarchy item selectionRange: {0}")]
    SelectionRange(InvalidRange),
}

/// A [`HierarchyItem`] whose ranges passed client-input validation.
///
/// Taken by the walking handlers instead of the raw wire DTO, so a zero,
/// oversized or reversed range cannot reach the LSP conversion. Built only by
/// [`Self::from_client`].
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::{CheckedHierarchyItem, HierarchyItem, InvalidHierarchyItem};
///
/// let wire = |line: u32| -> HierarchyItem {
///     serde_json::from_value(serde_json::json!({
///         "name": "Base", "kind": 5, "uri": "file:///a.cpp",
///         "range": {"start": {"line": line, "character": 1}, "end": {"line": 2, "character": 1}},
///         "selectionRange": {"start": {"line": 1, "character": 7}, "end": {"line": 1, "character": 11}},
///     }))
///     .unwrap()
/// };
/// assert!(CheckedHierarchyItem::from_client(wire(1)).is_ok());
/// assert!(matches!(
///     CheckedHierarchyItem::from_client(wire(0)),
///     Err(InvalidHierarchyItem::Range(_))
/// ));
/// ```
#[derive(Debug, Clone)]
pub struct CheckedHierarchyItem {
    pub(super) name: String,
    pub(super) kind: u32,
    pub(super) detail: Option<String>,
    pub(super) uri: String,
    pub(super) range: PositionRange,
    pub(super) selection_range: PositionRange,
    pub(super) data: Option<serde_json::Value>,
}

impl CheckedHierarchyItem {
    /// Validates the ranges of a client-supplied item.
    ///
    /// # Errors
    ///
    /// Returns which range was zero-based, above [`MAX_POSITION_VALUE`] or
    /// reversed. Containment of `selectionRange` in `range` is not checked.
    pub fn from_client(item: HierarchyItem) -> Result<Self, InvalidHierarchyItem> {
        let range = checked_range(&item.range).map_err(InvalidHierarchyItem::Range)?;
        let selection_range =
            checked_range(&item.selection_range).map_err(InvalidHierarchyItem::SelectionRange)?;
        Ok(Self {
            name: item.name,
            kind: item.kind,
            detail: item.detail,
            uri: item.uri,
            range,
            selection_range,
            data: item.data,
        })
    }

    /// The item's document URI, as the client sent it.
    #[must_use]
    pub fn uri(&self) -> &str {
        &self.uri
    }
}

fn checked_range(range: &Range) -> Result<PositionRange, InvalidRange> {
    PositionRange::from_client(
        (range.start.line, range.start.character),
        (range.end.line, range.end.character),
    )
}

/// Result of a type hierarchy prepare, supertypes or subtypes request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TypeHierarchyResult {
    /// Type hierarchy items at the position, or the supertypes/subtypes of
    /// the queried item.
    pub items: Vec<HierarchyItem>,
    /// Whether `items` was capped below the LSP server's full response (see
    /// `MAX_NORMALIZED_LOCATIONS`). Omitted (defaults to `false`) when
    /// serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "PositionDegradation")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// Whether the symbol at a position can be renamed, as answered by
/// `textDocument/prepareRename`.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::PrepareRenameOutcome;
///
/// let outcome = PrepareRenameOutcome::NotRenameable { server_message: None };
/// assert_eq!(
///     serde_json::to_value(&outcome).unwrap(),
///     serde_json::json!({"status": "not_renameable"})
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PrepareRenameOutcome {
    /// The identifier can be renamed.
    Renameable {
        /// Range of the identifier that a rename would replace.
        range: Range,
        /// Current name of the identifier, when the server supplies it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    /// The server accepts a rename here but leaves the identifier range to the
    /// client's own word-selection rule; no range is invented.
    DefaultBehavior,
    /// The position cannot be renamed. Distinct from a transport failure and
    /// from a server that does not support rename preparation.
    NotRenameable {
        /// The server's own explanation, when it reported one as an error.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_message: Option<String>,
    },
}

/// Result of a prepare rename request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PrepareRenameResult {
    /// The rename-preparation verdict.
    #[serde(flatten)]
    pub outcome: PrepareRenameOutcome,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "PositionDegradation")]
    pub positions_degraded: Option<PositionDegradation>,
}

/// How a document highlight occurrence uses its symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DocumentHighlightKind {
    /// A textual occurrence; also what a server that omits the kind means.
    Text,
    /// A read access, such as reading a variable.
    Read,
    /// A write access, such as assigning a variable.
    Write,
}

/// One occurrence of a symbol within a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DocumentHighlightEntry {
    /// Range of the occurrence.
    pub range: Range,
    /// How the occurrence uses the symbol.
    pub kind: DocumentHighlightKind,
}

/// Result of a document highlights request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DocumentHighlightsResult {
    /// Occurrences of the symbol at the position within the file.
    pub highlights: Vec<DocumentHighlightEntry>,
    /// Whether `highlights` was capped below the LSP server's full response
    /// (see `MAX_NORMALIZED_LOCATIONS`). Omitted (defaults to `false`) when
    /// serialized.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Set only when some `character` offsets in this result are inexact (non-UTF-16
    /// servers only); omitted when all are exact. Tells whether the queried position
    /// or only the returned offsets are affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "PositionDegradation")]
    pub positions_degraded: Option<PositionDegradation>,
}

impl ServerText for Location {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            uri,
            range: _,
            out_of_workspace: _,
        } = self;
        redactions.note_payload(uri);
    }
}

impl ServerText for HoverResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            contents,
            range: _,
            positions_degraded: _,
        } = self;
        redactions.redact_in_place(contents);
    }
}

impl ServerText for DefinitionResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            locations,
            truncated: _,
            positions_degraded: _,
            enrichment: _,
        } = self;
        locations.redact_server_text(redactions);
    }
}

impl ServerText for ReferencesResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            locations,
            truncated: _,
            positions_degraded: _,
            enrichment: _,
        } = self;
        locations.redact_server_text(redactions);
    }
}

impl ServerText for LocationsResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            locations,
            truncated: _,
            positions_degraded: _,
            enrichment: _,
        } = self;
        locations.redact_server_text(redactions);
    }
}

impl ServerText for Diagnostic {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            range: _,
            severity: _,
            message,
            code,
        } = self;
        redactions.redact_in_place(message);
        if let Some(code) = code {
            redactions.redact_in_place(code);
        }
    }
}

// Document diagnostics are redacted where they enter the cache or the pull path.
impl ServerText for DiagnosticsResult {
    fn redact_server_text(&mut self, _redactions: &Redactions) {
        let Self {
            diagnostics: _,
            positions_degraded: _,
        } = self;
    }
}

impl ServerText for DocumentDiagnosticsResult {
    fn redact_server_text(&mut self, _redactions: &Redactions) {
        let Self {
            diagnostics: _,
            positions_degraded: _,
            enrichment: _,
        } = self;
    }
}

impl ServerText for TextEdit {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self { range: _, new_text } = self;
        redactions.note_payload(new_text);
    }
}

impl ServerText for DocumentChanges {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self { uri, edits } = self;
        redactions.note_payload(uri);
        edits.redact_server_text(redactions);
    }
}

impl ServerText for RenameResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            changes,
            dropped: _,
            positions_degraded: _,
        } = self;
        changes.redact_server_text(redactions);
    }
}

impl ServerText for Completion {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            label,
            kind: _,
            detail,
            documentation,
        } = self;
        redactions.note_payload(label);
        for prose in [detail, documentation].into_iter().flatten() {
            redactions.redact_in_place(prose);
        }
    }
}

impl ServerText for CompletionsResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            items,
            positions_degraded: _,
        } = self;
        items.redact_server_text(redactions);
    }
}

impl ServerText for Symbol {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            name,
            kind: _,
            range: _,
            selection_range: _,
            children,
        } = self;
        redactions.note_payload(name);
        if let Some(children) = children {
            children.redact_server_text(redactions);
        }
    }
}

impl ServerText for DocumentSymbolsResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            symbols,
            positions_degraded: _,
        } = self;
        symbols.redact_server_text(redactions);
    }
}

impl ServerText for FormatDocumentResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            edits,
            positions_degraded: _,
        } = self;
        edits.redact_server_text(redactions);
    }
}

impl ServerText for WorkspaceSymbol {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            name,
            kind: _,
            location,
            container_name,
        } = self;
        redactions.note_payload(name);
        if let Some(container) = container_name {
            redactions.note_payload(container);
        }
        location.redact_server_text(redactions);
    }
}

impl ServerText for WorkspaceSymbolResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            symbols,
            truncated: _,
            positions_degraded: _,
        } = self;
        symbols.redact_server_text(redactions);
    }
}

impl ServerText for CodeAction {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            title,
            kind: _,
            diagnostics,
            edit,
            command,
            is_preferred: _,
        } = self;
        redactions.redact_in_place(title);
        diagnostics.redact_server_text(redactions);
        edit.redact_server_text(redactions);
        command.redact_server_text(redactions);
    }
}

impl ServerText for WorkspaceEditDescription {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            changes,
            dropped: _,
        } = self;
        changes.redact_server_text(redactions);
    }
}

impl ServerText for CommandDescription {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            title,
            command,
            arguments,
        } = self;
        redactions.redact_in_place(title);
        redactions.note_payload(command);
        for argument in arguments {
            redactions.note_payload_json(argument);
        }
    }
}

impl ServerText for CodeActionsResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            actions,
            truncated: _,
            positions_degraded: _,
        } = self;
        actions.redact_server_text(redactions);
    }
}

impl ServerText for CallHierarchyPrepareResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            items,
            truncated: _,
            positions_degraded: _,
        } = self;
        items.redact_server_text(redactions);
    }
}

impl ServerText for IncomingCall {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            from,
            from_ranges: _,
        } = self;
        from.redact_server_text(redactions);
    }
}

impl ServerText for IncomingCallsResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            calls,
            truncated: _,
            positions_degraded: _,
        } = self;
        calls.redact_server_text(redactions);
    }
}

impl ServerText for OutgoingCall {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self { to, from_ranges: _ } = self;
        to.redact_server_text(redactions);
    }
}

impl ServerText for OutgoingCallsResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            calls,
            truncated: _,
            positions_degraded: _,
        } = self;
        calls.redact_server_text(redactions);
    }
}

// Logs and messages are redacted when the notification enters the cache.
impl ServerText for ServerLogsResult {
    fn redact_server_text(&mut self, _redactions: &Redactions) {
        let Self { logs: _ } = self;
    }
}

impl ServerText for ServerMessagesResult {
    fn redact_server_text(&mut self, _redactions: &Redactions) {
        let Self { messages: _ } = self;
    }
}

impl ServerText for SignatureParameter {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            label,
            documentation,
        } = self;
        for prose in [label, documentation].into_iter().flatten() {
            redactions.redact_in_place(prose);
        }
    }
}

impl ServerText for SignatureInfo {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            label,
            documentation,
            parameters,
        } = self;
        redactions.redact_in_place(label);
        if let Some(documentation) = documentation {
            redactions.redact_in_place(documentation);
        }
        parameters.redact_server_text(redactions);
    }
}

impl ServerText for SignatureHelpResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            signatures,
            active_signature: _,
            active_parameter: _,
            positions_degraded: _,
        } = self;
        signatures.redact_server_text(redactions);
    }
}

impl ServerText for InlayHintEntry {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            position: _,
            label,
            kind: _,
            padding_left: _,
            padding_right: _,
            tooltip,
        } = self;
        redactions.redact_in_place(label);
        if let Some(tooltip) = tooltip {
            redactions.redact_in_place(tooltip);
        }
    }
}

impl ServerText for InlayHintsResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            hints,
            truncated: _,
            positions_degraded: _,
        } = self;
        hints.redact_server_text(redactions);
    }
}

impl ServerText for HierarchyItem {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            name,
            kind: _,
            detail,
            uri,
            range: _,
            selection_range: _,
            data,
            out_of_workspace: _,
        } = self;
        redactions.note_payload(name);
        if let Some(detail) = detail {
            redactions.note_payload(detail);
        }
        redactions.note_payload(uri);
        if let Some(data) = data {
            redactions.note_payload_json(data);
        }
    }
}

impl ServerText for TypeHierarchyResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            items,
            truncated: _,
            positions_degraded: _,
        } = self;
        items.redact_server_text(redactions);
    }
}

impl ServerText for PrepareRenameOutcome {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        match self {
            Self::Renameable {
                range: _,
                placeholder,
            } => {
                if let Some(placeholder) = placeholder {
                    redactions.note_payload(placeholder);
                }
            }
            Self::DefaultBehavior => {}
            Self::NotRenameable { server_message } => {
                if let Some(message) = server_message {
                    redactions.redact_in_place(message);
                }
            }
        }
    }
}

impl ServerText for PrepareRenameResult {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            outcome,
            positions_degraded: _,
        } = self;
        outcome.redact_server_text(redactions);
    }
}

impl ServerText for DocumentHighlightsResult {
    fn redact_server_text(&mut self, _redactions: &Redactions) {
        let Self {
            highlights: _,
            truncated: _,
            positions_degraded: _,
        } = self;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    fn hierarchy_wire(range: [u32; 4], selection: [u32; 4]) -> HierarchyItem {
        let at = |line, character| serde_json::json!({"line": line, "character": character});
        serde_json::from_value(serde_json::json!({
            "name": "x", "kind": 5, "uri": "file:///a.rs",
            "range": {"start": at(range[0], range[1]), "end": at(range[2], range[3])},
            "selectionRange": {"start": at(selection[0], selection[1]), "end": at(selection[2], selection[3])},
        }))
        .unwrap()
    }

    #[test]
    fn checked_hierarchy_item_accepts_valid_ranges() {
        let item = CheckedHierarchyItem::from_client(hierarchy_wire([1, 1, 2, 1], [1, 7, 1, 9]));
        assert_eq!(item.unwrap().uri(), "file:///a.rs");
    }

    #[test]
    fn checked_hierarchy_item_rejects_bad_range_and_names_it() {
        for bad in [
            [0, 1, 1, 1],
            [1, 0, 1, 1],
            [1, 1, 1, 1_000_001],
            [1, 1, u32::MAX, 1],
            [2, 1, 1, 1],
        ] {
            let err =
                CheckedHierarchyItem::from_client(hierarchy_wire(bad, [1, 1, 1, 1])).unwrap_err();
            assert!(matches!(err, InvalidHierarchyItem::Range(_)), "{bad:?}");
            let err =
                CheckedHierarchyItem::from_client(hierarchy_wire([1, 1, 9, 1], bad)).unwrap_err();
            assert!(
                matches!(err, InvalidHierarchyItem::SelectionRange(_)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn checked_hierarchy_item_rejects_selection_column_above_max() {
        let err = CheckedHierarchyItem::from_client(hierarchy_wire(
            [1, 1, 2, 1],
            [1, 1_000_001, 1, 1_000_001],
        ))
        .unwrap_err();
        assert_eq!(
            err,
            InvalidHierarchyItem::SelectionRange(InvalidRange::Start(InvalidPosition::TooLarge))
        );
    }

    use super::*;

    const SECRET: &str = "SuperSecretValue123";

    fn range() -> Range {
        Range {
            start: Position2D {
                line: 1,
                character: 1,
            },
            end: Position2D {
                line: 1,
                character: 2,
            },
        }
    }

    #[test]
    fn test_server_text_redacts_prose_and_leaves_edit_payload() {
        let set = Redactions::new([("API_TOKEN".to_owned(), SECRET.to_owned())]);
        let mut actions = vec![CodeAction {
            title: format!("fix {SECRET}"),
            kind: None,
            diagnostics: vec![Diagnostic {
                range: range(),
                severity: DiagnosticSeverity::Error,
                message: format!("bad {SECRET}"),
                code: Some(format!("E-{SECRET}")),
            }],
            edit: Some(WorkspaceEditDescription {
                changes: vec![DocumentChanges {
                    uri: format!("file:///{SECRET}.rs"),
                    edits: vec![TextEdit {
                        range: range(),
                        new_text: format!("let k = \"{SECRET}\";"),
                    }],
                }],
                dropped: DroppedEdits::default(),
            }),
            command: None,
            is_preferred: false,
        }];

        actions.redact_server_text(&set);

        let action = &actions[0];
        assert_eq!(action.title, "fix [redacted:API_TOKEN]");
        assert_eq!(action.diagnostics[0].message, "bad [redacted:API_TOKEN]");
        assert_eq!(
            action.diagnostics[0].code.as_deref(),
            Some("E-[redacted:API_TOKEN]")
        );
        let changes = &action.edit.as_ref().unwrap().changes[0];
        assert!(changes.uri.contains(SECRET));
        assert!(changes.edits[0].new_text.contains(SECRET));
    }

    #[test]
    fn test_server_text_completion_keeps_label_and_redacts_detail_and_documentation() {
        let set = Redactions::new([("API_TOKEN".to_owned(), SECRET.to_owned())]);
        let mut completion = Completion {
            label: format!("use_{SECRET}"),
            kind: Some(3),
            detail: Some(format!("fn {SECRET}()")),
            documentation: Some(format!("docs {SECRET}")),
        };

        completion.redact_server_text(&set);

        assert_eq!(completion.label, format!("use_{SECRET}"));
        assert_eq!(
            completion.detail.as_deref(),
            Some("fn [redacted:API_TOKEN]()")
        );
        assert_eq!(
            completion.documentation.as_deref(),
            Some("docs [redacted:API_TOKEN]")
        );
    }

    #[test]
    fn test_server_text_redacts_signature_label_documentation_and_parameters() {
        let set = Redactions::new([("API_TOKEN".to_owned(), SECRET.to_owned())]);
        let mut result = SignatureHelpResult {
            signatures: vec![SignatureInfo {
                label: format!("f({SECRET})"),
                documentation: Some(format!("doc {SECRET}")),
                parameters: vec![SignatureParameter {
                    label: Some(format!("p {SECRET}")),
                    documentation: Some(format!("pd {SECRET}")),
                }],
            }],
            active_signature: Some(0),
            active_parameter: Some(0),
            positions_degraded: None,
        };

        result.redact_server_text(&set);

        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains(SECRET), "{json}");
        assert_eq!(json.matches("[redacted:API_TOKEN]").count(), 4, "{json}");
    }

    #[test]
    fn test_server_text_redacts_inlay_label_and_tooltip() {
        let set = Redactions::new([("API_TOKEN".to_owned(), SECRET.to_owned())]);
        let mut hint = InlayHintEntry {
            position: Position2D {
                line: 1,
                character: 1,
            },
            label: format!(": {SECRET}"),
            kind: None,
            padding_left: None,
            padding_right: None,
            tooltip: Some(format!("tip {SECRET}")),
        };

        hint.redact_server_text(&set);

        assert_eq!(hint.label, ": [redacted:API_TOKEN]");
        assert_eq!(hint.tooltip.as_deref(), Some("tip [redacted:API_TOKEN]"));
    }

    #[test]
    fn test_server_text_command_redacts_title_and_keeps_command_and_arguments() {
        let set = Redactions::new([("API_TOKEN".to_owned(), SECRET.to_owned())]);
        let mut command = CommandDescription {
            title: format!("run {SECRET}"),
            command: format!("cmd.{SECRET}"),
            arguments: vec![serde_json::json!({ "key": SECRET })],
        };

        command.redact_server_text(&set);

        assert_eq!(command.title, "run [redacted:API_TOKEN]");
        assert_eq!(command.command, format!("cmd.{SECRET}"));
        assert_eq!(
            command.arguments,
            vec![serde_json::json!({ "key": SECRET })]
        );
    }

    #[test]
    fn test_server_text_leaves_symbol_and_hierarchy_identifiers() {
        let set = Redactions::new([("API_TOKEN".to_owned(), SECRET.to_owned())]);
        let mut symbols = DocumentSymbolsResult {
            symbols: vec![Symbol {
                name: format!("sym_{SECRET}"),
                kind: 12,
                range: range(),
                selection_range: range(),
                children: Some(vec![Symbol {
                    name: format!("child_{SECRET}"),
                    kind: 12,
                    range: range(),
                    selection_range: range(),
                    children: None,
                }]),
            }],
            positions_degraded: None,
        };
        let mut item = HierarchyItem {
            name: format!("call_{SECRET}"),
            kind: 12,
            detail: Some(format!("detail {SECRET}")),
            uri: format!("file:///{SECRET}.rs"),
            range: range(),
            selection_range: range(),
            data: Some(serde_json::json!({ "id": SECRET })),
            out_of_workspace: false,
        };
        let mut type_item = HierarchyItem {
            name: format!("type_{SECRET}"),
            kind: 5,
            detail: None,
            uri: format!("file:///{SECRET}.rs"),
            range: range(),
            selection_range: range(),
            data: None,
            out_of_workspace: false,
        };
        let before = (
            serde_json::to_string(&symbols).unwrap(),
            serde_json::to_string(&item).unwrap(),
            serde_json::to_string(&type_item).unwrap(),
        );

        symbols.redact_server_text(&set);
        item.redact_server_text(&set);
        type_item.redact_server_text(&set);

        let after = (
            serde_json::to_string(&symbols).unwrap(),
            serde_json::to_string(&item).unwrap(),
            serde_json::to_string(&type_item).unwrap(),
        );
        assert_eq!(before, after);
    }

    #[test]
    fn test_server_text_with_no_secrets_rewrites_nothing() {
        let mut hover = HoverResult {
            contents: format!("doc {SECRET}"),
            range: None,
            positions_degraded: None,
        };

        hover.redact_server_text(&Redactions::default());

        assert_eq!(hover.contents, format!("doc {SECRET}"));
    }

    #[test]
    fn test_server_text_does_not_touch_already_redacted_diagnostics() {
        let set = Redactions::new([("API_TOKEN".to_owned(), SECRET.to_owned())]);
        let mut result = DocumentDiagnosticsResult {
            diagnostics: vec![ContextualDiagnostic::from(Diagnostic {
                range: range(),
                severity: DiagnosticSeverity::Error,
                message: format!("echo {SECRET}"),
                code: None,
            })],
            positions_degraded: None,
            enrichment: None,
        };

        result.redact_server_text(&set);

        assert_eq!(result.diagnostics[0].message, format!("echo {SECRET}"));
    }

    #[test]
    fn test_server_text_redacts_hover_and_prepare_rename_message() {
        let set = Redactions::new([("API_TOKEN".to_owned(), SECRET.to_owned())]);
        let mut hover = HoverResult {
            contents: format!("doc {SECRET}"),
            range: None,
            positions_degraded: None,
        };
        let mut outcome = PrepareRenameOutcome::NotRenameable {
            server_message: Some(format!("no {SECRET}")),
        };

        hover.redact_server_text(&set);
        outcome.redact_server_text(&set);

        assert_eq!(hover.contents, "doc [redacted:API_TOKEN]");
        assert_eq!(
            outcome,
            PrepareRenameOutcome::NotRenameable {
                server_message: Some("no [redacted:API_TOKEN]".to_owned())
            }
        );
    }

    #[test]
    fn test_position_degradation_request_outranks_response() {
        assert!(PositionDegradation::Request > PositionDegradation::Response);
        assert_eq!(
            Some(PositionDegradation::Response).max(Some(PositionDegradation::Request)),
            Some(PositionDegradation::Request)
        );
        assert_eq!(
            None.max(Some(PositionDegradation::Response)),
            Some(PositionDegradation::Response)
        );
    }

    #[test]
    fn test_position_degradation_serializes_as_snake_case_string() {
        assert_eq!(
            serde_json::to_value(PositionDegradation::Request).unwrap(),
            "request"
        );
        assert_eq!(
            serde_json::to_value(PositionDegradation::Response).unwrap(),
            "response"
        );
    }

    #[test]
    fn test_positions_degraded_key_omitted_when_none() {
        let result = super::DocumentSymbolsResult {
            symbols: Vec::new(),
            positions_degraded: None,
        };
        let value = serde_json::to_value(&result).unwrap();
        assert!(value.get("positions_degraded").is_none());

        let result = super::DocumentSymbolsResult {
            positions_degraded: Some(PositionDegradation::Response),
            ..result
        };
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["positions_degraded"], "response");
    }

    /// #467 regression: the old `Option<u8>` narrowing silently dropped any
    /// `InlayHintKind::Custom(n)` with `n > 255` to `None`, indistinguishable
    /// from "server sent no kind". `lsp_kind_to_u32` must preserve the full
    /// `u32` value losslessly.
    #[test]
    fn test_lsp_kind_to_u32_preserves_custom_values_above_u8_range() {
        let kind = lsp_types::InlayHintKind::Custom(300);
        assert_eq!(lsp_kind_to_u32(kind), 300u32);
    }

    #[test]
    fn test_position_from_client_bounds() {
        assert_eq!(Position::from_client(0, 1), Err(InvalidPosition::ZeroBased));
        assert_eq!(Position::from_client(1, 0), Err(InvalidPosition::ZeroBased));
        assert_eq!(
            Position::from_client(MAX_POSITION_VALUE + 1, 1),
            Err(InvalidPosition::TooLarge)
        );
        assert_eq!(
            Position::from_client(1, MAX_POSITION_VALUE + 1),
            Err(InvalidPosition::TooLarge)
        );
        let max = Position::from_client(MAX_POSITION_VALUE, MAX_POSITION_VALUE).unwrap();
        assert_eq!((max.lsp_line(), max.lsp_character()), (999_999, 999_999));
    }

    #[test]
    fn test_position_from_output_position_is_not_capped_and_clamps_zero() {
        let big = Position::from_server_output(&Position2D {
            line: MAX_POSITION_VALUE + 7,
            character: 2_000_000,
        });
        assert_eq!(big.line().get(), MAX_POSITION_VALUE + 7);
        let clamped = Position::from_server_output(&Position2D {
            line: 0,
            character: 0,
        });
        assert_eq!((clamped.line().get(), clamped.character().get()), (1, 1));
    }

    #[test]
    fn test_position_range_orders_and_bounds() {
        let at = Position::at;
        assert!(PositionRange::new(at(1, 5), at(1, 5)).is_ok());
        assert_eq!(
            PositionRange::new(at(2, 1), at(1, 9)),
            Err(InvalidRange::Reversed)
        );
        assert_eq!(
            PositionRange::new(at(1, 5), at(1, 4)),
            Err(InvalidRange::Reversed)
        );
        assert_eq!(
            PositionRange::from_client((0, 1), (1, 1)),
            Err(InvalidRange::Start(InvalidPosition::ZeroBased))
        );
        assert_eq!(
            PositionRange::from_client((1, 1), (1, 0)),
            Err(InvalidRange::End(InvalidPosition::ZeroBased))
        );

        let within = PositionRange::new(at(1, 1), at(1 + MAX_RANGE_LINES, 1)).unwrap();
        assert!(BoundedRange::try_from(within).is_ok());
        let beyond = PositionRange::new(at(1, 1), at(2 + MAX_RANGE_LINES, 1)).unwrap();
        assert_eq!(
            BoundedRange::try_from(beyond),
            Err(InvalidRange::TooManyLines)
        );
    }
}
