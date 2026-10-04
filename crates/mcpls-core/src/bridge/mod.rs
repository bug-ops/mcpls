//! Translation layer between MCP and LSP protocols.
//!
//! This module handles the bidirectional conversion between
//! MCP tool calls and LSP requests/responses.

use std::sync::{Mutex as StdMutex, MutexGuard, PoisonError};

mod client_path;
mod encoding;
mod indexing;
mod notifications;
pub mod resources;
mod state;
mod translator;
mod workspace_roots;

pub use client_path::{ClientPath, InvalidClientPath};
pub use encoding::PositionEncoding;
// Not part of the crate's public API surface (unlike `IndexingPolicy`/`IndexingState`
// above, both referenced from public signatures) -- these three exist only
// for `config`'s default-value/validation wiring, so `pub(crate)` avoids
// widening the public surface and keeps `indexing.rs`'s own intra-doc links
// to private items (`IndexingTracker::state`, `PROGRESS_LATCH_IDLE`) valid.
pub(crate) use indexing::{
    DEFAULT_INDEXING_READY_TIMEOUT_SECS, INDEXING_STALENESS_BOUND, PROGRESS_SETTLE,
};
pub use indexing::{IndexingPolicy, IndexingState};
pub use notifications::{
    DiagnosticInfo, DiagnosticSources, LogEntry, LogLevel, MessageType, NotificationCache,
    ServerMessage, apply_lifecycle_notification,
};
pub(crate) use notifications::{DiagnosticsKey, diagnostics_cache_key};
pub use state::{
    DEFAULT_MAX_DOCUMENTS, DEFAULT_MAX_FILE_SIZE, DocumentTracker, ResourceLimits, path_to_uri,
    uri_to_path,
};
pub(crate) use state::{InFlightGuard, try_path_to_uri};
#[cfg(test)]
pub(crate) use translator::Capability;
pub use translator::{
    AddressableTool, Addressed, Completion, CompletionsResult, DefinitionResult, Diagnostic,
    DiagnosticSeverity, DiagnosticsResult, DocumentChanges, DocumentSymbolsResult, DroppedEdits,
    FormatDocumentResult, HoverResult, Location, MAX_RESTART_SERVER_IDS, MAX_SERVER_ID_BYTES,
    MAX_SYMBOL_NAME_BYTES, Position, Position2D, PositionDegradation, PositionSource, Range,
    ReferencesResult, RenameResult, ResolvedSymbol, ResolvedTarget, RestartFailure, RestartOutcome,
    RestartServerResult, RestartTarget, ServerIds, ServerIdsError, ServerRestartEntry, Symbol,
    SymbolName, SymbolNameError, SymbolQuery, SymbolTarget, TextEdit, Translator,
    parse_symbol_kind,
};
pub(crate) use translator::{
    CallHierarchyPrepareResult, CodeActionsResult, IncomingCallsResult, InlayHintsResult,
    LocationsResult, NotificationReceivers, NotificationWiring, OutgoingCallsResult, RouteSupport,
    ServerLogsResult, ServerMessagesResult, SignatureHelpResult, ToolSupportSnapshot,
    WorkspaceSymbolResult, validate_path_against_roots,
};
#[cfg(test)]
pub(crate) use workspace_roots::ProcessCwd;
pub use workspace_roots::WorkspaceRoots;
pub(crate) use workspace_roots::{
    canonicalize_existing_prefix, join_relative_root, lexically_normalize, probe_root,
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
