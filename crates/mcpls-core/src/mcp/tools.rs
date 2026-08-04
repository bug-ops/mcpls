//! MCP tool parameter definitions.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Shared position parameters (file path plus 1-based line/character) used by
/// every tool that operates at a single point in a file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PositionParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
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

/// Parameters for the `get_hover` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting hover information at a position in a file.")]
pub struct HoverParams {
    /// Position in the file to operate on.
    #[serde(flatten)]
    pub position: PositionParams,
}

/// Parameters for the `get_definition` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting the definition location of a symbol.")]
pub struct DefinitionParams {
    /// Position in the file to operate on.
    #[serde(flatten)]
    pub position: PositionParams,
}

/// Parameters for the `get_references` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for finding all references to a symbol.")]
pub struct ReferencesParams {
    /// Position in the file to operate on.
    #[serde(flatten)]
    pub position: PositionParams,
    /// Whether to include the declaration in the results.
    #[schemars(description = "Whether to include the declaration in the results.")]
    #[serde(default)]
    pub include_declaration: bool,
}

/// Parameters for the `get_diagnostics` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting diagnostics (errors, warnings) for a file.")]
pub struct DiagnosticsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
}

/// Parameters for the `rename_symbol` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for renaming a symbol across the workspace.")]
pub struct RenameParams {
    /// Position in the file to operate on.
    #[serde(flatten)]
    pub position: PositionParams,
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

/// Parameters for the `get_document_symbols` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting all symbols in a document.")]
pub struct DocumentSymbolsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
}

/// Parameters for the `format_document` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for formatting a document.")]
pub struct FormatDocumentParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    /// Tab size for formatting (default: 4).
    #[schemars(description = "Tab size for formatting (default: 4).")]
    #[serde(default = "default_tab_size")]
    pub tab_size: u32,
    /// Whether to use spaces instead of tabs (default: true).
    #[schemars(description = "Whether to use spaces instead of tabs (default: true).")]
    #[serde(default = "default_insert_spaces")]
    pub insert_spaces: bool,
}

const fn default_tab_size() -> u32 {
    4
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
    /// Optional filter by symbol kind (function, class, variable, etc.).
    #[schemars(description = "Optional filter by symbol kind (function, class, variable, etc.).")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind_filter: Option<String>,
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
    pub file_path: String,
    /// Range in the file to operate on.
    #[serde(flatten)]
    pub range: RangeParams,
    /// Optional filter by action kind (quickfix, refactor, source, etc.).
    #[schemars(description = "Optional filter by action kind (quickfix, refactor, source, etc.).")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind_filter: Option<String>,
}

/// Parameters for the `prepare_call_hierarchy` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for preparing call hierarchy at a position.")]
pub struct CallHierarchyPrepareParams {
    /// Position in the file to operate on.
    #[serde(flatten)]
    pub position: PositionParams,
}

/// Parameters for the `get_incoming_calls` and `get_outgoing_calls` tools.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(
    description = "Parameters for getting incoming or outgoing calls for a call hierarchy item."
)]
pub struct CallHierarchyCallsParams {
    /// The call hierarchy item to get calls for (from prepare response).
    #[schemars(description = "The call hierarchy item to get calls for (from prepare response).")]
    pub item: serde_json::Value,
}

/// Parameters for the `get_cached_diagnostics` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(
    description = "Parameters for getting cached diagnostics from LSP server notifications."
)]
pub struct CachedDiagnosticsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
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
    pub min_level: Option<String>,
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

/// Parameters for the `get_signature_help` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting signature help at a position in a file.")]
pub struct SignatureHelpParams {
    /// Position in the file to operate on.
    #[serde(flatten)]
    pub position: PositionParams,
}

/// Parameters for the `go_to_implementation` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for navigating to implementations of a symbol.")]
pub struct GoToImplementationParams {
    /// Position in the file to operate on.
    #[serde(flatten)]
    pub position: PositionParams,
}

/// Parameters for the `go_to_type_definition` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for navigating to the type definition of an expression.")]
pub struct GoToTypeDefinitionParams {
    /// Position in the file to operate on.
    #[serde(flatten)]
    pub position: PositionParams,
}

/// Parameters for the `get_inlay_hints` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Parameters for getting inlay hints in a range.")]
pub struct InlayHintsParams {
    /// Absolute path to the file.
    #[schemars(description = "Absolute path to the file.")]
    pub file_path: String,
    /// Range in the file to operate on.
    #[serde(flatten)]
    pub range: RangeParams,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `#[serde(flatten)]` must keep `PositionParams`/`RangeParams` fields at
    /// the top level of the wire format, since MCP clients send flat JSON
    /// objects with no knowledge of the Rust-side nesting.
    #[test]
    fn flattened_params_serialize_to_flat_json() {
        let hover = HoverParams {
            position: PositionParams {
                file_path: "/a.rs".to_string(),
                line: 1,
                character: 2,
            },
        };
        let json = serde_json::to_value(&hover).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"file_path": "/a.rs", "line": 1, "character": 2})
        );

        let inlay = InlayHintsParams {
            file_path: "/b.rs".to_string(),
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

    /// A flat JSON object (what an MCP client actually sends) must deserialize
    /// into the nested Rust shape produced by `#[serde(flatten)]`.
    #[test]
    fn flat_json_deserializes_into_flattened_params() {
        let json = serde_json::json!({"file_path": "/a.rs", "line": 1, "character": 2});
        let hover: HoverParams = serde_json::from_value(json).unwrap();
        assert_eq!(hover.position.file_path, "/a.rs");
        assert_eq!(hover.position.line, 1);
        assert_eq!(hover.position.character, 2);
    }

    /// The generated JSON schema must expose `PositionParams`/`RangeParams`
    /// fields as top-level properties, not nested under `position`/`range` --
    /// otherwise MCP clients would see a schema that no longer matches the
    /// flat wire format.
    #[test]
    fn generated_schema_exposes_flattened_fields_at_top_level() {
        let schema = schemars::schema_for!(HoverParams);
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
        assert!(!properties.contains_key("position"));

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
