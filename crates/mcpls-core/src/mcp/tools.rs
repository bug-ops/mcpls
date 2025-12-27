//! MCP tool parameter definitions.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Parameters for the `get_hover` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HoverParams {
    /// Absolute path to the file.
    pub file_path: String,
    /// Line number (1-based).
    pub line: u32,
    /// Character/column number (1-based).
    pub character: u32,
}

/// Parameters for the `get_definition` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DefinitionParams {
    /// Absolute path to the file.
    pub file_path: String,
    /// Line number (1-based).
    pub line: u32,
    /// Character/column number (1-based).
    pub character: u32,
}

/// Parameters for the `get_references` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReferencesParams {
    /// Absolute path to the file.
    pub file_path: String,
    /// Line number (1-based).
    pub line: u32,
    /// Character/column number (1-based).
    pub character: u32,
    /// Whether to include the declaration in the results.
    #[serde(default)]
    pub include_declaration: bool,
}

/// Parameters for the `get_diagnostics` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsParams {
    /// Absolute path to the file.
    pub file_path: String,
}

/// Parameters for the `rename_symbol` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RenameParams {
    /// Absolute path to the file.
    pub file_path: String,
    /// Line number (1-based).
    pub line: u32,
    /// Character/column number (1-based).
    pub character: u32,
    /// New name for the symbol.
    pub new_name: String,
}

/// Parameters for the `get_completions` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompletionsParams {
    /// Absolute path to the file.
    pub file_path: String,
    /// Line number (1-based).
    pub line: u32,
    /// Character/column number (1-based).
    pub character: u32,
    /// Optional trigger character.
    pub trigger: Option<String>,
}

/// Parameters for the `get_document_symbols` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DocumentSymbolsParams {
    /// Absolute path to the file.
    pub file_path: String,
}

/// Parameters for the `format_document` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FormatDocumentParams {
    /// Absolute path to the file.
    pub file_path: String,
    /// Tab size for formatting.
    #[serde(default = "default_tab_size")]
    pub tab_size: u32,
    /// Whether to use spaces instead of tabs.
    #[serde(default = "default_insert_spaces")]
    pub insert_spaces: bool,
}

const fn default_tab_size() -> u32 {
    4
}

const fn default_insert_spaces() -> bool {
    true
}
