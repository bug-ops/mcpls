//! Translation layer between MCP and LSP protocols.
//!
//! This module handles the bidirectional conversion between
//! MCP tool calls and LSP requests/responses.

mod client_path;
mod encoding;
mod indexing;
mod notifications;
mod published_uri;
pub mod resources;
mod signals;
mod state;
mod translator;
mod workspace_roots;

pub use client_path::{ClientPath, InvalidClientPath};
pub use encoding::{InvalidPositionEncoding, PositionEncoding};
// Not part of the crate's public API surface (unlike `IndexingPolicy`/`IndexingState`
// above, both referenced from public signatures) -- these three exist only
// for `config`'s default-value/validation wiring, so `pub(crate)` avoids
// widening the public surface and keeps `indexing.rs`'s own intra-doc links
// to private items (`IndexingTracker::state`, `PROGRESS_LATCH_IDLE`) valid.
pub(crate) use indexing::{
    DEFAULT_INDEXING_READY_TIMEOUT_SECS, INDEXING_STALENESS_BOUND, IndexingReset, PROGRESS_SETTLE,
};
pub use indexing::{IndexingPolicy, IndexingState};
pub(crate) use notifications::{
    BoundedDiagnostics, DiagnosticsKey, MAX_DIAGNOSTIC_ENTRIES, PushWrite, diagnostics_cache_key,
    on_lifecycle,
};
pub use notifications::{
    DiagnosticInfo, DiagnosticSources, LogEntry, LogLevel, MessageType, NotificationCache,
    ReportedSeverity, ServerMessage, apply_lifecycle_notification,
};
#[cfg(test)]
pub(crate) use published_uri::resolve_one;
pub(crate) use published_uri::{Publication, PublicationKind, PublishedPathResolver};
pub use signals::{Indexed, IndexingSignal, RouteSignals};
pub use state::{
    Access, DocumentOpenState, DocumentTracker, DocumentVersion, OpenRequest, ResourceLimits,
    path_to_uri, uri_to_path,
};
pub(crate) use state::{InFlightGuard, LinePresence, Opening, try_path_to_uri};
#[cfg(test)]
pub(crate) use translator::MAX_FAILED_PULLS_PER_STATE;
pub use translator::{
    AddressableTool, Addressed, BoundedRange, Capability, CheckedHierarchyItem,
    CodeActionKindFilter, Completion, CompletionTrigger, CompletionsResult, Contextual,
    ContextualDiagnostic, ContextualLocation, Diagnostic, DiagnosticsAnswer,
    DiagnosticsAvailability, DiagnosticsOrigin, DiagnosticsResult, DocumentChanges,
    DocumentDiagnosticsResult, DocumentHighlightEntry, DocumentHighlightKind,
    DocumentHighlightsResult, DocumentSymbolsResult, DroppedEdits, EnclosingSymbol,
    EnclosingSymbolOutcome, EnrichmentSummary, FoldingKind, FoldingKindFilter, FoldingRangesResult,
    FoldingRegion, FormatDocumentResult, HierarchyItem, HoverResult, InvalidHierarchyItem,
    InvalidPosition, InvalidRange, InvalidTabSize, InvalidTextField, KindFilter, KindFilterField,
    KindFilterInput, Location, LocationsResult, MAX_COLLAPSED_TEXT_BYTES, MAX_NEW_NAME_LENGTH,
    MAX_POSITION_VALUE, MAX_RANGE_LINES, MAX_RESTART_SERVER_IDS, MAX_SELECTION_CHAIN,
    MAX_SERVER_ID_BYTES, MAX_SYMBOL_NAME_BYTES, MAX_TAB_SIZE, MAX_TRIGGER_CHARACTER_BYTES, NewName,
    NotComputedReason, Position, Position2D, PositionDegradation, PositionRange, PositionSource,
    PrepareRenameOutcome, PrepareRenameResult, Range, RejectedKindFilter, Rejection, RenameResult,
    ResolvedSymbol, ResolvedTarget, RestartFailure, RestartOutcome, RestartServerResult,
    RestartTarget, ResultContext, RouteSupport, SelectionRangesResult, ServerIds, ServerIdsError,
    ServerRestartEntry, Symbol, SymbolFidelity, SymbolKindFilter, SymbolName, SymbolQuery,
    SymbolTarget, TabSize, TextEdit, TextField, Translator, TypeHierarchyResult, UnavailableReason,
};
pub(crate) use translator::{
    CallHierarchyPrepareResult, CodeActionsResult, DiagnosticsRole, IncomingCallsResult,
    InlayHintsResult, Interest, MAX_COMPANION_FILES, MAX_SWEPT_FILES_PER_TICK,
    NotificationReceivers, NotificationWiring, OutgoingCallsResult, SWEEP_INTERVAL,
    ServerLogsResult, ServerMessagesResult, SignatureHelpResult, SweepContext, SweepMemo,
    ToolSupportSnapshot, WorkspaceSymbolResult,
};
#[cfg(test)]
pub(crate) use workspace_roots::{CanonicalizeFn, ProcessCwd};
pub use workspace_roots::{WorkspacePath, WorkspaceRoots};
pub(crate) use workspace_roots::{join_relative_root, lexically_normalize, probe_root};
