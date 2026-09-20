//! Translation layer between MCP and LSP protocols.
//!
//! This module handles the bidirectional conversion between
//! MCP tool calls and LSP requests/responses.

use std::path::{Component, PathBuf};
use std::sync::{Mutex as StdMutex, MutexGuard, PoisonError};

use lsp_types::Uri;

mod encoding;
mod indexing;
mod notifications;
pub mod resources;
mod state;
mod translator;

pub use encoding::{PositionEncoding, lsp_to_mcp_position, mcp_to_lsp_position};
pub use indexing::{IndexingPolicy, IndexingState};
pub(crate) use notifications::apply_lifecycle_notification;
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

/// Whether `uri` resolves to a path within one of `workspace_roots`.
///
/// The single containment check shared by every path where mcpls forwards a
/// server-supplied URI it is about to *write through* or otherwise treat as
/// authoritative on the MCP client's behalf: the diagnostics pump
/// (`crate::diagnostic_path_in_workspace`) and rename/code-action
/// `WorkspaceEdit` results (`bridge::translator::edits`). A compromised or
/// misbehaving LSP server is treated as untrusted input on both, so they
/// must reject the same out-of-workspace URIs the same way rather than risk
/// the two checks drifting apart.
///
/// Deliberately **not** applied to read-only navigation results
/// (`get_definition`/`get_references`/`get_implementation`/
/// `get_type_definition`/call hierarchy, and `search_workspace_symbols`):
/// a legitimate result routinely points outside the workspace (the standard
/// library, a crates.io dependency), so dropping those would break ordinary
/// navigation. Any subsequent attempt to open or read the path such a
/// location names still goes through the inbound
/// `validate_path_against_roots` gate (`mcp/server.rs`), which fails closed,
/// so the untrusted-URI concern is already covered downstream for that case.
/// `get_document_symbols`' legacy flat (`SymbolInformation`) response shape
/// takes a different approach again: rather than filter, it normalizes every
/// entry against the already-resolved, trusted queried document instead of
/// trusting the server-reported per-entry URI (see
/// `bridge::translator::symbols::handle_document_symbols`), since
/// `document_symbols` is a single-document request by construction.
///
/// Deliberately does not canonicalize -- this runs on every response
/// location and notification, and LSP servers report already-resolved
/// canonical paths, so a prefix check is enough to reject a URI a legitimate
/// server would never publish, without a filesystem syscall per call.
///
/// # Preconditions
///
/// `workspace_roots` must itself already be canonical, or every URI silently
/// fails to match and is dropped. [`Translator::workspace_roots`] and
/// `serve_with`'s `workspace_roots_snapshot` both satisfy this via
/// `resolve_workspace_roots`.
///
/// An empty `workspace_roots` (no workspace configured) rejects every URI,
/// matching [`validate_path_against_roots`]'s fail-closed
/// `Error::NoWorkspaceRoots` behavior: without a configured root there is
/// nothing to treat as authoritative, so no server-supplied URI is trusted.
pub(crate) fn uri_in_workspace_roots(uri: &Uri, workspace_roots: &[PathBuf]) -> bool {
    if workspace_roots.is_empty() {
        return false;
    }
    let Some(path) = uri_to_path(uri) else {
        return false;
    };
    // `Path::starts_with` compares components lexically and does not resolve
    // `.`/`..`, so `/workspace/../etc/passwd` would otherwise pass the
    // `/workspace` prefix check despite pointing outside it. A legitimate LSP
    // server never publishes such a path (canonical paths never contain
    // `.`/`..` components), so rejecting them outright costs nothing and
    // closes the bypass for a server that deliberately crafts one.
    if path
        .components()
        .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return false;
    }
    workspace_roots.iter().any(|root| path.starts_with(root))
}

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
