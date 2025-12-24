//! MCP tool definitions and handlers.
//!
//! This module defines the MCP tools that expose LSP capabilities
//! to AI agents.

mod handlers;
mod tools;

pub use handlers::ToolHandler;
pub use tools::{
    CompletionsParams, DefinitionParams, DiagnosticsParams, DocumentSymbolsParams,
    FormatDocumentParams, HoverParams, ReferencesParams, RenameParams,
};
