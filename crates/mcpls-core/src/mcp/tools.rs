//! MCP tool parameter definitions.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::bridge::{
    CodeActionKindFilter, FoldingKindFilter, HierarchyItem, InvalidPosition, KindFilter,
    KindFilterInput, LogLevel, MAX_RESTART_SERVER_IDS, MAX_SERVER_ID_BYTES, MAX_SYMBOL_NAME_BYTES,
    Position, RestartTarget, ResultContext, ServerIds, SymbolKindFilter, SymbolName, SymbolQuery,
    SymbolTarget, TabSize,
};
use crate::config::ServerId;

/// Schema description of the opt-in `context` input shared by every tool that
/// can attach enclosing symbols.
const CONTEXT_DESCRIPTION: &str = "Extra context per returned item: `none` (default) or `enclosing_symbol` to attach the innermost containing symbol (name path, kind, range). Costs one documentSymbol request per distinct file, capped per call.";

/// Shared position parameters (file path plus 1-based line/character) used by
/// every tool that operates at a single point in a file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PositionParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: PathBuf,
    /// Line number (1-based).
    #[schemars(description = "Line number (1-based).")]
    pub line: u32,
    /// Character/column number (1-based).
    #[schemars(description = "Character/column number (1-based).")]
    pub character: u32,
}

/// Shared range parameters (1-based start/end line and character) used by
/// every tool that operates over a range in a file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RangeParams {
    /// Start line (1-based).
    #[schemars(description = "Start line (1-based).")]
    pub start_line: u32,
    /// Start character (1-based).
    #[schemars(description = "Start character (1-based).")]
    pub start_character: u32,
    /// End line (1-based).
    #[schemars(description = "End line (1-based).")]
    pub end_line: u32,
    /// End character (1-based).
    #[schemars(description = "End character (1-based).")]
    pub end_character: u32,
}

/// Wire form of [`SymbolTargetParams`]: a file plus either a position or a
/// symbol name.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(
    description = "A file plus the symbol to act on: give either `line` and `character`, or `symbol_name`."
)]
struct SymbolTargetWire {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    file_path: PathBuf,
    /// Line number (1-based); give with `character`, instead of `symbol_name`.
    #[schemars(
        description = "Line number (1-based). Give with `character`, instead of `symbol_name`."
    )]
    #[serde(default)]
    line: Option<u32>,
    /// Character/column number (1-based); give with `line`.
    #[schemars(
        description = "Character/column number (1-based). Give with `line`, instead of `symbol_name`."
    )]
    #[serde(default)]
    character: Option<u32>,
    /// Name of a symbol defined in the file, instead of a position.
    #[schemars(
        description = "Name of a symbol defined in this file, instead of `line`/`character`. May be qualified (`Type::method`, `Type.method`). If it matches several symbols the call fails and lists them."
    )]
    #[serde(default)]
    symbol_name: Option<String>,
    /// Narrow `symbol_name` to this kind.
    #[schemars(
        description = "With `symbol_name`: keep only symbols of this kind, by name (function, method, class, struct, ...) or numeric LSP SymbolKind value."
    )]
    #[serde(default)]
    symbol_kind: Option<KindFilterInput<SymbolKindFilter>>,
    /// Narrow `symbol_name` to symbols inside this container.
    #[schemars(
        description = "With `symbol_name`: keep only symbols directly inside a container (type, impl, class, module) of this name."
    )]
    #[serde(default)]
    container: Option<String>,
}

/// A file and the symbol in it a tool acts on, addressed by position or by
/// name. Exactly one addressing form is representable: a request with both,
/// neither, half a position, or qualifiers without a name fails to
/// deserialize and is reported as invalid parameters.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(try_from = "SymbolTargetWire")]
#[schemars(with = "SymbolTargetWire")]
pub struct SymbolTargetParams {
    /// Absolute path to the file.
    pub file_path: PathBuf,
    /// The symbol to act on, as the client wrote it.
    pub target: SymbolTargetInput,
}

/// A symbol addressed by position or by name, before the position is checked.
///
/// The position stays raw so a bad value is rejected as invalid parameters
/// (JSON-RPC `-32602`) by [`Self::into_target`] in the tool method, not as a
/// deserialization failure, which MCP reports as a tool-result error.
#[derive(Debug, Clone)]
pub enum SymbolTargetInput {
    /// A 1-based position, unchecked.
    Position {
        /// Line number (1-based).
        line: u32,
        /// Character offset (1-based).
        character: u32,
    },
    /// A symbol name with optional qualifiers.
    Name(SymbolQuery),
}

impl SymbolTargetInput {
    /// Checks the position and yields the typed target.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidPosition`] for a position below 1 or above
    /// [`MAX_POSITION_VALUE`](crate::bridge::MAX_POSITION_VALUE).
    pub fn into_target(self) -> Result<SymbolTarget, InvalidPosition> {
        match self {
            Self::Position { line, character } => {
                Position::from_client(line, character).map(SymbolTarget::Position)
            }
            Self::Name(query) => Ok(SymbolTarget::Name(query)),
        }
    }
}

impl TryFrom<SymbolTargetWire> for SymbolTargetParams {
    type Error = String;

    fn try_from(wire: SymbolTargetWire) -> Result<Self, Self::Error> {
        let target = match (wire.line, wire.character, wire.symbol_name) {
            (Some(line), Some(character), None) => {
                if wire.symbol_kind.is_some() || wire.container.is_some() {
                    return Err(
                        "`symbol_kind` and `container` apply only with `symbol_name`".to_string(),
                    );
                }
                SymbolTargetInput::Position { line, character }
            }
            (None, None, Some(name)) => {
                let kind = wire
                    .symbol_kind
                    .map(|kind| match kind {
                        KindFilterInput::Known(kind) => Ok(kind.kind()),
                        KindFilterInput::Rejected(rejected) => {
                            Err(if rejected.as_str().len() > MAX_SYMBOL_NAME_BYTES {
                                "`symbol_kind` is too long".to_string()
                            } else {
                                SymbolKindFilter::rejection_message(rejected.as_str())
                            })
                        }
                    })
                    .transpose()?;
                SymbolTargetInput::Name(SymbolQuery {
                    name: SymbolName::try_new(name).map_err(|e| e.to_string())?,
                    kind,
                    container: wire
                        .container
                        .map(SymbolName::try_new)
                        .transpose()
                        .map_err(|e| e.to_string())?,
                })
            }
            (Some(_), Some(_), Some(_)) => {
                return Err(
                    "give either `line` and `character`, or `symbol_name`, not both".to_string(),
                );
            }
            (None, None, None) => {
                return Err("give `line` and `character`, or `symbol_name`".to_string());
            }
            _ => {
                return Err(
                    "`line` and `character` must be given together and not with `symbol_name`"
                        .to_string(),
                );
            }
        };
        Ok(Self {
            file_path: wire.file_path,
            target,
        })
    }
}

#[cfg(test)]
impl From<PositionParams> for SymbolTargetParams {
    fn from(position: PositionParams) -> Self {
        Self {
            file_path: position.file_path,
            target: SymbolTargetInput::Position {
                line: position.line,
                character: position.character,
            },
        }
    }
}

/// Parameters for the `get_references` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for finding all references to a symbol.")]
pub struct ReferencesParams {
    /// The symbol to find references to.
    #[serde(flatten)]
    pub target: SymbolTargetParams,
    /// Whether to include the declaration in the results.
    #[schemars(description = "Whether to include the declaration in the results.")]
    #[serde(default)]
    pub include_declaration: bool,
    /// Optional extra context for each returned item.
    #[schemars(description = CONTEXT_DESCRIPTION)]
    #[serde(default)]
    pub context: ResultContext,
}

/// Parameters for the `get_definition`, `go_to_implementation` and
/// `go_to_type_definition` tools.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for navigating from a symbol to related locations.")]
pub struct NavigationParams {
    /// The symbol to navigate from.
    #[serde(flatten)]
    pub target: SymbolTargetParams,
    /// Optional extra context for each returned item.
    #[schemars(description = CONTEXT_DESCRIPTION)]
    #[serde(default)]
    pub context: ResultContext,
}

#[cfg(test)]
impl From<PositionParams> for NavigationParams {
    fn from(position: PositionParams) -> Self {
        Self {
            target: position.into(),
            context: ResultContext::None,
        }
    }
}

/// Parameters for the `go_to_declaration` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for navigating from a position to its declaration.")]
pub struct DeclarationParams {
    /// The position to navigate from.
    #[serde(flatten)]
    pub position: PositionParams,
    /// Optional extra context for each returned item.
    #[schemars(description = CONTEXT_DESCRIPTION)]
    #[serde(default)]
    pub context: ResultContext,
}

#[cfg(test)]
impl From<PositionParams> for DeclarationParams {
    fn from(position: PositionParams) -> Self {
        Self {
            position,
            context: ResultContext::None,
        }
    }
}

/// Parameters for the `get_diagnostics` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting diagnostics (errors, warnings) for a file.")]
pub struct DiagnosticsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: PathBuf,
    /// Optional extra context for each returned item.
    #[schemars(description = CONTEXT_DESCRIPTION)]
    #[serde(default)]
    pub context: ResultContext,
}

/// Parameters for the `rename_symbol` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for renaming a symbol across the workspace.")]
pub struct RenameParams {
    /// The symbol to rename.
    #[serde(flatten)]
    pub target: SymbolTargetParams,
    /// New name for the symbol.
    #[schemars(description = "New name for the symbol.")]
    pub new_name: String,
}

/// Parameters for the `get_completions` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting code completion suggestions.")]
pub struct CompletionsParams {
    /// Position in the file to operate on.
    #[serde(flatten)]
    pub position: PositionParams,
    /// Optional trigger character (e.g., '.', ':', '->').
    #[schemars(description = "Optional trigger character (e.g., '.', ':', '->').")]
    pub trigger: Option<String>,
}

/// Parameters for the `get_folding_ranges` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting the foldable regions of a file.")]
pub struct FoldingRangesParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: PathBuf,
    /// Which regions to return.
    #[schemars(
        description = "`all` (default), `comment`, `imports` or `region`; regions without a kind match only `all`."
    )]
    #[serde(default)]
    pub kind: FoldingKindFilter,
}

/// Parameters for the `get_document_symbols` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting all symbols in a document.")]
pub struct DocumentSymbolsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: PathBuf,
}

/// Parameters for the `format_document` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for formatting a document.")]
pub struct FormatDocumentParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: PathBuf,
    /// Tab size for formatting (1 to 32, default: 4).
    #[schemars(description = "Tab size for formatting (1 to 32, default: 4).")]
    #[serde(default)]
    pub tab_size: TabSize,
    /// Whether to use spaces instead of tabs (default: true).
    #[schemars(description = "Whether to use spaces instead of tabs (default: true).")]
    #[serde(default = "default_insert_spaces")]
    pub insert_spaces: bool,
}

const fn default_insert_spaces() -> bool {
    true
}

/// Parameters for the `workspace_symbol_search` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for searching symbols across the workspace.")]
pub struct WorkspaceSymbolParams {
    /// Search query for symbol names (supports partial matching).
    #[schemars(description = "Search query for symbol names (supports partial matching).")]
    pub query: String,
    /// Optional filter by symbol kind: a name (function, class, variable,
    /// etc.) or the numeric LSP `SymbolKind` value from a result's `kind`
    /// field. A name is validated against the known kinds; a numeric value
    /// is accepted as-is with no validation (it may name a server-specific
    /// custom kind), so a value that matches no symbol returns an empty
    /// result rather than an error.
    #[schemars(
        description = "Optional filter by symbol kind: a name (function, class, variable, etc.) \
                        or the numeric LSP SymbolKind value from a result's kind field. A name \
                        is validated against the known kinds; a numeric value is accepted as-is \
                        with no validation, so one that matches no symbol returns an empty \
                        result rather than an error."
    )]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind_filter: Option<KindFilterInput<SymbolKindFilter>>,
    /// Maximum results to return (default: 100).
    #[schemars(description = "Maximum results to return (default: 100).")]
    #[serde(default = "default_max_results")]
    pub limit: u32,
}

const fn default_max_results() -> u32 {
    100
}

/// Parameters for the `get_code_actions` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(
    description = "Parameters for getting available code actions (quick fixes, refactorings) for a range."
)]
pub struct CodeActionsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: PathBuf,
    /// Range in the file to operate on.
    #[serde(flatten)]
    pub range: RangeParams,
    /// Optional filter by action kind (quickfix, refactor, source, etc.).
    #[schemars(
        description = "Optional filter by action kind, in any case; sent to the server in its canonical spelling."
    )]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind_filter: Option<KindFilterInput<CodeActionKindFilter>>,
}

/// Parameters for the `get_incoming_calls` and `get_outgoing_calls` tools.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(
    description = "Parameters for getting incoming or outgoing calls for a call hierarchy item."
)]
pub struct CallHierarchyCallsParams {
    /// The call hierarchy item to get calls for (from prepare response).
    #[schemars(
        description = "The call hierarchy item to get calls for, exactly as returned by prepare_call_hierarchy, get_incoming_calls or get_outgoing_calls."
    )]
    pub item: HierarchyItem,
}

/// Parameters for the `get_supertypes` and `get_subtypes` tools.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(
    description = "Parameters for getting the supertypes or subtypes of a type hierarchy item."
)]
pub struct TypeHierarchyWalkParams {
    /// The type hierarchy item to walk from (from a prepare, supertypes or subtypes response).
    #[schemars(
        description = "The type hierarchy item to walk from, exactly as returned by prepare_type_hierarchy, get_supertypes or get_subtypes."
    )]
    pub item: HierarchyItem,
}

/// Parameters for the `format_range` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for formatting a range of a document.")]
pub struct FormatRangeParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: PathBuf,
    /// Range in the file to format.
    #[serde(flatten)]
    pub range: RangeParams,
    /// Tab size for formatting (1 to 32, default: 4).
    #[schemars(description = "Tab size for formatting (1 to 32, default: 4).")]
    #[serde(default)]
    pub tab_size: TabSize,
    /// Whether to use spaces instead of tabs (default: true).
    #[schemars(description = "Whether to use spaces instead of tabs (default: true).")]
    #[serde(default = "default_insert_spaces")]
    pub insert_spaces: bool,
}

/// Parameters for the `get_cached_diagnostics` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(
    description = "Parameters for getting cached diagnostics from LSP server notifications."
)]
pub struct CachedDiagnosticsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: PathBuf,
}

/// Parameters for the `get_server_logs` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting recent LSP server log messages.")]
pub struct ServerLogsParams {
    /// Maximum number of log entries to return (default: 50).
    #[schemars(description = "Maximum number of log entries to return (default: 50).")]
    #[serde(default = "default_log_limit")]
    pub limit: usize,
    /// Minimum log level to include: error, warning, info, debug.
    #[schemars(description = "Minimum log level to include: error, warning, info, debug.")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_level: Option<LogLevel>,
}

const fn default_log_limit() -> usize {
    50
}

/// Parameters for the `get_server_messages` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(
    description = "Parameters for getting recent LSP server messages (showMessage notifications)."
)]
pub struct ServerMessagesParams {
    /// Maximum number of messages to return (default: 20).
    #[schemars(description = "Maximum number of messages to return (default: 20).")]
    #[serde(default = "default_message_limit")]
    pub limit: usize,
}

const fn default_message_limit() -> usize {
    20
}

/// Parameters for the `get_tool_support` tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for reporting which tools are usable for which languages.")]
pub struct ToolSupportParams {
    /// Restrict the report to the language of this file.
    #[schemars(description = "Absolute path to a file; restricts the report to its language.")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<PathBuf>,
}

/// Wire form of [`RestartServerParams`]: exactly one of the two fields selects
/// the servers.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(
    description = "Parameters for restarting LSP servers. Give exactly one of `servers` or `all`."
)]
struct RestartServerWire {
    /// Ids of the servers to restart.
    #[schemars(
        description = "Ids of the servers to restart; an unknown id is rejected with the list of configured ids. Non-empty."
    )]
    #[schemars(length(max = MAX_RESTART_SERVER_IDS), inner(length(max = MAX_SERVER_ID_BYTES)))]
    #[serde(default)]
    servers: Option<Vec<String>>,
    /// Restart every configured server.
    #[schemars(description = "Set to true to restart every configured server.")]
    #[serde(default)]
    all: bool,
}

/// Parameters for the `restart_server` tool, parsed into a [`RestartTarget`]
/// at deserialization so a bare call, an empty list, or both selectors are
/// rejected as invalid parameters.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(try_from = "RestartServerWire")]
#[schemars(with = "RestartServerWire")]
pub struct RestartServerParams {
    /// Which servers to restart.
    pub target: RestartTarget,
}

impl TryFrom<RestartServerWire> for RestartServerParams {
    type Error = String;

    fn try_from(wire: RestartServerWire) -> Result<Self, Self::Error> {
        let target = match (wire.servers, wire.all) {
            (Some(_), true) => return Err("give either `servers` or `all`, not both".to_string()),
            (None, false) => {
                return Err(
                    "give `servers` (a non-empty list of server ids) or `all: true`".to_string(),
                );
            }
            (None, true) => RestartTarget::All,
            (Some(ids), false) => {
                let ids = ids.into_iter().map(ServerId::from).collect();
                RestartTarget::Servers(ServerIds::try_new(ids).map_err(|e| e.to_string())?)
            }
        };
        Ok(Self { target })
    }
}

/// Parameters for the `get_inlay_hints` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting inlay hints in a range.")]
pub struct InlayHintsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: PathBuf,
    /// Range in the file to operate on.
    #[serde(flatten)]
    pub range: RangeParams,
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::path::Path;

    use super::*;

    /// `#[serde(flatten)]` must keep `PositionParams`/`RangeParams` fields at
    /// the top level of the wire format, since MCP clients send flat JSON
    /// objects with no knowledge of the Rust-side nesting.
    #[test]
    fn flattened_params_serialize_to_flat_json() {
        let position = PositionParams {
            file_path: PathBuf::from("/a.rs"),
            line: 1,
            character: 2,
        };
        let json = serde_json::to_value(&position).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"file_path": "/a.rs", "line": 1, "character": 2})
        );

        let inlay = InlayHintsParams {
            file_path: PathBuf::from("/b.rs"),
            range: RangeParams {
                start_line: 1,
                start_character: 2,
                end_line: 3,
                end_character: 4,
            },
        };
        let json = serde_json::to_value(&inlay).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "file_path": "/b.rs",
                "start_line": 1,
                "start_character": 2,
                "end_line": 3,
                "end_character": 4,
            })
        );
    }

    #[test]
    fn restart_params_accept_servers_or_all() {
        let one: RestartServerParams =
            serde_json::from_value(serde_json::json!({"servers": ["rust", "rust", "py"]})).unwrap();
        let RestartTarget::Servers(ids) = one.target else {
            panic!("expected a server list");
        };
        assert_eq!(ids.as_slice().len(), 2);

        let all: RestartServerParams =
            serde_json::from_value(serde_json::json!({"all": true})).unwrap();
        assert_eq!(all.target, RestartTarget::All);
    }

    #[test]
    fn restart_params_reject_ambiguous_or_empty_selectors() {
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"all": false}),
            serde_json::json!({"servers": []}),
            serde_json::json!({"servers": [" "]}),
            serde_json::json!({"servers": ["rust"], "all": true}),
            serde_json::json!({"servers": (0..=MAX_RESTART_SERVER_IDS).map(|i| format!("s{i}")).collect::<Vec<_>>()}),
            serde_json::json!({"servers": ["x".repeat(MAX_SERVER_ID_BYTES + 1)]}),
        ] {
            assert!(
                serde_json::from_value::<RestartServerParams>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn restart_params_schema_exposes_wire_fields() {
        let schema = schemars::schema_for!(RestartServerParams);
        let properties = schema
            .as_object()
            .unwrap()
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap();
        assert!(properties.contains_key("servers"));
        assert!(properties.contains_key("all"));
        assert!(!properties.contains_key("target"));
        let servers = &properties["servers"];
        assert_eq!(servers["maxItems"], MAX_RESTART_SERVER_IDS);
        assert_eq!(servers["items"]["maxLength"], MAX_SERVER_ID_BYTES);
    }

    /// A flat JSON object (what an MCP client actually sends) must deserialize
    /// into the nested Rust shape produced by `#[serde(flatten)]`.
    #[test]
    fn flat_json_deserializes_into_flattened_params() {
        let json = serde_json::json!({"file_path": "/a.rs", "line": 1, "character": 2});
        let references: ReferencesParams = serde_json::from_value(json).unwrap();
        assert_eq!(references.target.file_path.as_path(), Path::new("/a.rs"));
        assert_matches!(
            references.target.target,
            SymbolTargetInput::Position {
                line: 1,
                character: 2
            }
        );
        assert!(!references.include_declaration);

        let json = serde_json::json!({
            "file_path": "/a.rs",
            "symbol_name": "parse",
            "symbol_kind": "function",
            "container": "Config",
            "include_declaration": true,
        });
        let references: ReferencesParams = serde_json::from_value(json).unwrap();
        assert!(references.include_declaration);
        let SymbolTargetInput::Name(query) = references.target.target else {
            panic!("expected a name target");
        };
        assert_eq!(query.name.as_str(), "parse");
        assert_eq!(query.kind, Some(lsp_types::SymbolKind::Function));
        assert_eq!(query.container.unwrap().as_str(), "Config");
    }

    #[test]
    fn symbol_target_accepts_exactly_one_addressing_form() {
        let parse = |json: serde_json::Value| serde_json::from_value::<SymbolTargetParams>(json);
        assert!(
            parse(serde_json::json!({"file_path": "/a.rs", "line": 1, "character": 1})).is_ok()
        );
        assert!(parse(serde_json::json!({"file_path": "/a.rs", "symbol_name": "f"})).is_ok());
        for bad in [
            serde_json::json!({"file_path": "/a.rs"}),
            serde_json::json!({"file_path": "/a.rs", "line": 1}),
            serde_json::json!({"file_path": "/a.rs", "character": 1}),
            serde_json::json!({"file_path": "/a.rs", "line": 1, "character": 1, "symbol_name": "f"}),
            serde_json::json!({"file_path": "/a.rs", "line": 1, "symbol_name": "f"}),
            serde_json::json!({"file_path": "/a.rs", "line": 1, "character": 1, "container": "T"}),
            serde_json::json!({"file_path": "/a.rs", "symbol_name": " "}),
            serde_json::json!({"file_path": "/a.rs", "symbol_name": "f", "symbol_kind": "nope"}),
            serde_json::json!({"file_path": "/a.rs", "symbol_name": "f", "container": ""}),
        ] {
            assert!(parse(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn symbol_kind_accepts_names_and_numeric_values() {
        let kind = |value: &str| {
            let json =
                serde_json::json!({"file_path": "/a.rs", "symbol_name": "f", "symbol_kind": value});
            let params: SymbolTargetParams = serde_json::from_value(json).unwrap();
            let SymbolTargetInput::Name(query) = params.target else {
                panic!("expected a name target");
            };
            query.kind
        };
        assert_eq!(kind("Method"), Some(lsp_types::SymbolKind::Method));
        assert_eq!(kind("6"), Some(lsp_types::SymbolKind::Method));
        assert_eq!(kind("METHOD"), Some(lsp_types::SymbolKind::Method));
    }

    #[test]
    fn format_range_params_are_flat_and_default_options() {
        let json = serde_json::json!({
            "file_path": "/a.rs",
            "start_line": 1,
            "start_character": 1,
            "end_line": 2,
            "end_character": 3,
        });
        let params: FormatRangeParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.range.end_line, 2);
        assert_eq!(params.tab_size, TabSize::default());
        assert!(params.insert_spaces);
    }

    #[test]
    fn format_params_reject_tab_sizes_outside_the_bounds() {
        let params = |tab_size: u32| {
            serde_json::from_value::<FormatDocumentParams>(
                serde_json::json!({ "file_path": "/a.rs", "tab_size": tab_size }),
            )
        };
        assert_eq!(
            params(crate::bridge::MAX_TAB_SIZE).unwrap().tab_size.get(),
            crate::bridge::MAX_TAB_SIZE
        );
        assert_eq!(params(1).unwrap().tab_size.get(), 1);
        for rejected in [0, crate::bridge::MAX_TAB_SIZE + 1, u32::MAX] {
            let error = params(rejected).unwrap_err().to_string();
            assert!(
                error.contains("tab_size must be between 1 and 32"),
                "{error}"
            );
        }
    }

    #[test]
    fn format_range_params_reject_tab_sizes_outside_the_bounds() {
        let params = |tab_size: u32| {
            serde_json::from_value::<FormatRangeParams>(serde_json::json!({
                "file_path": "/a.rs",
                "start_line": 1,
                "start_character": 1,
                "end_line": 2,
                "end_character": 3,
                "tab_size": tab_size,
            }))
        };
        assert_eq!(params(32).unwrap().tab_size.get(), 32);
        for rejected in [0, 33] {
            let error = params(rejected).unwrap_err().to_string();
            assert!(
                error.contains("tab_size must be between 1 and 32"),
                "{error}"
            );
        }
    }

    #[test]
    fn folding_range_params_default_to_all_kinds_and_reject_unknown_ones() {
        let params = |kind: Option<&str>| {
            let mut json = serde_json::json!({ "file_path": "/a.rs" });
            if let Some(kind) = kind {
                json["kind"] = kind.into();
            }
            serde_json::from_value::<FoldingRangesParams>(json)
        };
        assert_eq!(params(None).unwrap().kind, FoldingKindFilter::All);
        assert_eq!(
            params(Some("imports")).unwrap().kind,
            FoldingKindFilter::Imports
        );
        assert!(params(Some("unspecified")).is_err());
        assert!(params(Some("Imports")).is_err());
    }

    #[test]
    fn type_hierarchy_walk_params_require_a_typed_item() {
        let untyped = serde_json::json!({"item": {"invalid": "structure"}});
        assert!(serde_json::from_value::<TypeHierarchyWalkParams>(untyped).is_err());

        let schema = schemars::schema_for!(TypeHierarchyWalkParams);
        let item = &schema.as_object().unwrap()["properties"]["item"];
        assert!(item.get("$ref").is_some());
    }

    /// The generated JSON schema must expose `PositionParams`/`RangeParams`
    /// fields as top-level properties, not nested under `position`/`range` --
    /// otherwise MCP clients would see a schema that no longer matches the
    /// flat wire format.
    #[test]
    fn generated_schema_exposes_flattened_fields_at_top_level() {
        let schema = schemars::schema_for!(ReferencesParams);
        let properties = schema
            .as_object()
            .unwrap()
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap();
        assert!(properties.contains_key("file_path"));
        assert!(properties.contains_key("line"));
        assert!(properties.contains_key("character"));
        assert!(properties.contains_key("symbol_name"));
        assert!(properties.contains_key("symbol_kind"));
        assert!(properties.contains_key("container"));
        assert!(properties.contains_key("include_declaration"));
        assert!(!properties.contains_key("target"));

        let schema = schemars::schema_for!(InlayHintsParams);
        let properties = schema
            .as_object()
            .unwrap()
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap();
        assert!(properties.contains_key("file_path"));
        assert!(properties.contains_key("start_line"));
        assert!(properties.contains_key("start_character"));
        assert!(properties.contains_key("end_line"));
        assert!(properties.contains_key("end_character"));
        assert!(!properties.contains_key("range"));
    }
}
