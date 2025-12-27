//! MCP tool definitions and handlers.
//!
//! This module defines the MCP tools that expose LSP capabilities
//! to AI agents.

mod handlers;
mod server;
mod tools;

pub use handlers::{ToolHandler, ToolHandlers};
pub use server::McplsServer;
pub use tools::{
    CallHierarchyCallsParams, CallHierarchyPrepareParams, CompletionsParams, DefinitionParams,
    DiagnosticsParams, DocumentSymbolsParams, FormatDocumentParams, HoverParams, ReferencesParams,
    RenameParams, WorkspaceSymbolParams,
};
