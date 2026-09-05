//! Translation layer between MCP and LSP protocols.
//!
//! This module handles the bidirectional conversion between
//! MCP tool calls and LSP requests/responses.

use std::sync::{Mutex as StdMutex, MutexGuard, PoisonError};

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
pub(crate) use state::try_path_to_uri;
pub use state::{
    DEFAULT_MAX_DOCUMENTS, DEFAULT_MAX_FILE_SIZE, DocumentState, DocumentTracker, ResourceLimits,
    path_to_uri, uri_to_path,
};
pub(crate) use translator::validate_path_against_roots;
pub use translator::{
    Completion, CompletionsResult, DefinitionResult, Diagnostic, DiagnosticSeverity,
    DiagnosticsResult, DocumentChanges, DocumentSymbolsResult, FormatDocumentResult, HoverResult,
    Location, Position, Position2D, Range, ReferencesResult, RenameResult, Symbol, TextEdit,
    Translator,
};

/// Lock a `std::sync::Mutex`, recovering the guard if a previous holder
/// panicked while holding it.
///
/// Every lock guarded this way protects a short, synchronous, panic-free
/// critical section (a `HashMap`/`HashSet` lookup or insert), so poisoning
/// can only happen if an unrelated bug already panicked; refusing to unwind
/// the whole process a second time over stale poisoning is preferable to
/// deadlocking future calls. Shared by `translator` and `state` so both
/// modules lock their interior `HashMap`/`HashSet` fields the same way.
pub(crate) fn lock_std<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
