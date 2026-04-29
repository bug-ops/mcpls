//! Translation layer between MCP and LSP protocols.
//!
//! This module handles the bidirectional conversion between
//! MCP tool calls and LSP requests/responses.

mod encoding;
mod notifications;
pub mod resources;
mod state;
mod translator;

pub use encoding::{PositionEncoding, lsp_to_mcp_position, mcp_to_lsp_position};
pub use notifications::{
    DiagnosticInfo, LogEntry, LogLevel, MessageType, NotificationCache, ServerMessage,
};
pub use resources::ResourceSubscriptions;
pub use state::{DocumentState, DocumentTracker, path_to_uri, uri_to_path};
pub use translator::{
    Completion, CompletionsResult, DefinitionResult, Diagnostic, DiagnosticSeverity,
    DiagnosticsResult, DocumentChanges, DocumentSymbolsResult, FormatDocumentResult, HoverResult,
    Location, Position2D, Range, ReferencesResult, RenameResult, Symbol, TextEdit, Translator,
};
