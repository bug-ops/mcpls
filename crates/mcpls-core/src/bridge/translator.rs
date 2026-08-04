//! MCP to LSP translation layer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams,
    CallHierarchyPrepareParams as LspCallHierarchyPrepareParams, CompletionParams,
    CompletionTriggerKind, DocumentFormattingParams, DocumentSymbol, DocumentSymbolParams,
    FormattingOptions, GotoDefinitionParams, Hover, HoverContents, HoverParams as LspHoverParams,
    InlayHintLabel, InlayHintParams, MarkedString, PartialResultParams, ReferenceContext,
    ReferenceParams, RenameParams as LspRenameParams,
    SignatureHelpParams as LspSignatureHelpParams, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams, WorkspaceEdit,
    WorkspaceSymbolParams as LspWorkspaceSymbolParams,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::Duration;

use super::state::{ResourceLimits, detect_language, path_to_uri};
use super::{DiagnosticInfo, DocumentTracker, NotificationCache, lock_std};
use crate::bridge::encoding::mcp_to_lsp_position;
use crate::config::{ServerId, ToolKind, ToolRouter, base_language_id};
use crate::error::{Error, Result};
use crate::lsp::{LspClient, LspServer, ServerInitConfig};

/// Translator handles MCP tool calls by converting them to LSP requests.
///
/// All fields use interior mutability so `Translator` can be shared via a
/// plain `Arc<Translator>` with no outer lock: every LSP tool call would
/// otherwise serialize behind a single mutex for its entire round trip
/// (including the LSP request timeout), which is the root cause fixed here.
/// Each field is locked independently and only for the short, synchronous
/// section that touches it. In particular, the actual LSP request/response
/// round trip (`client.request(...)`) always runs with no lock held.
///
/// `document_tracker` is no exception: `DocumentTracker` locks its own state
/// per-path internally (see its docs), so `prepare_document`'s call into
/// `ensure_open` never holds a lock shared across unrelated paths or
/// languages while it does that document's disk I/O and
/// `textDocument/didOpen`/`didChange` notify.
#[derive(Debug)]
pub struct Translator {
    /// LSP clients indexed by routing identity. Locked only for the map
    /// lookup/insert itself, never across an LSP request.
    lsp_clients: Arc<StdMutex<HashMap<ServerId, LspClient>>>,
    /// LSP servers indexed by routing identity (held for lifetime management).
    lsp_servers: Arc<StdMutex<HashMap<ServerId, LspServer>>>,
    /// Document state tracker. Locks its own state internally, per path.
    document_tracker: Arc<DocumentTracker>,
    /// Allowed workspace roots for path validation. Read-only after `serve()`
    /// setup, so no lock is needed.
    workspace_roots: Arc<Vec<PathBuf>>,
    /// Custom file extension to language ID mappings. Read-only after
    /// `serve()` setup, so no lock is needed.
    extension_map: Arc<HashMap<String, String>>,
    /// Servers that are configured + applicable but may not have finished
    /// initializing yet (background init). Used to return a clear "still
    /// initializing" error instead of "no server configured".
    expected_servers: Arc<StdMutex<HashSet<ServerId>>>,
    /// Per-tool routing table: resolves `(language, tool)` to a `ServerId`.
    /// Locked independently so `rebind_router` (called from a background
    /// task once registration completes) never contends with an in-flight
    /// LSP round trip.
    router: Arc<StdMutex<ToolRouter>>,
    /// Configs needed to respawn a server if its process dies later, keyed
    /// by routing identity. Populated once per server right after a
    /// successful spawn (see [`Self::register_server_config`]); the respawn
    /// path ([`Self::respawn_if_dead`]) is the only reader.
    server_configs: Arc<StdMutex<HashMap<ServerId, ServerInitConfig>>>,
    /// Per-server single-flight lock so concurrent callers that both observe
    /// a dead process don't race to respawn it independently -- the loser
    /// waits for the winner's attempt to finish (success or failure) and
    /// then re-reads whatever ended up registered. See
    /// [`Self::respawn_if_dead`].
    respawn_locks: Arc<StdMutex<HashMap<ServerId, Arc<Mutex<()>>>>>,
    /// Consecutive respawn failures and last-attempt time per server, so a
    /// crash-looping server backs off instead of eating a fresh
    /// `timeout_seconds` on every tool call that arrives while it is down.
    /// See [`Self::respawn_if_dead`].
    respawn_backoffs: Arc<StdMutex<HashMap<ServerId, RespawnBackoff>>>,
    /// Diagnostics cache, shared with `serve_with`'s notification pump.
    ///
    /// `None` for a `Translator` built without [`Self::with_notification_cache`]
    /// (e.g. most unit tests). When present, [`Self::respawn_if_dead`] uses
    /// it to invalidate a respawned server's stale cached diagnostics --
    /// see that method's docs for why that matters.
    notification_cache: Option<Arc<Mutex<NotificationCache>>>,
}

/// Tracks respawn attempts for one server, so [`Translator::respawn_if_dead`]
/// can back off a crash-looping process instead of retrying it on every
/// single tool call.
#[derive(Debug, Clone, Copy)]
struct RespawnBackoff {
    /// Number of consecutive attempts that have not produced a server which
    /// stayed alive for at least [`RESPAWN_BACKOFF_BASE`]. A spawn failure
    /// counts immediately; a spawn that succeeds but is found dead again
    /// within that window counts too, once that is discovered -- see
    /// [`Translator::reconcile_respawn_stability`]. Without this, a server
    /// that starts, completes `initialize`, and then crashes a second later
    /// (a common real crash-loop shape) would bypass backoff entirely: each
    /// "success" would otherwise look like a fresh, unbacked-off start.
    consecutive_failures: u32,
    /// When the most recent attempt was made, or (if `last_attempt_succeeded`)
    /// when that success was last found to have not held up.
    last_attempt: Instant,
    /// Whether the most recent attempt completed `initialize` successfully.
    /// `false` for an outright spawn failure. Also reset to `false` once a
    /// "successful" respawn is found to have died again within the
    /// stability window, so that discovery is applied only once.
    last_attempt_succeeded: bool,
}

/// Base delay before the first backed-off retry after a respawn failure.
const RESPAWN_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Upper bound on the exponential backoff delay between respawn attempts.
const RESPAWN_BACKOFF_MAX: Duration = Duration::from_secs(30);

impl Translator {
    /// Create a new translator.
    ///
    /// Starts with an empty router: nothing is routable until [`Self::with_router`]
    /// installs one, which matches having no servers registered.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lsp_clients: Arc::new(StdMutex::new(HashMap::new())),
            lsp_servers: Arc::new(StdMutex::new(HashMap::new())),
            document_tracker: Arc::new(DocumentTracker::new(
                ResourceLimits::default(),
                HashMap::new(),
            )),
            workspace_roots: Arc::new(Vec::new()),
            extension_map: Arc::new(HashMap::new()),
            expected_servers: Arc::new(StdMutex::new(HashSet::new())),
            router: Arc::new(StdMutex::new(ToolRouter::default())),
            server_configs: Arc::new(StdMutex::new(HashMap::new())),
            respawn_locks: Arc::new(StdMutex::new(HashMap::new())),
            respawn_backoffs: Arc::new(StdMutex::new(HashMap::new())),
            notification_cache: None,
        }
    }

    /// Set the workspace roots for path validation.
    ///
    /// Only called during single-owner setup, before the translator is
    /// shared, so this replaces the `Arc` wholesale rather than locking.
    pub fn set_workspace_roots(&mut self, roots: Vec<PathBuf>) {
        self.workspace_roots = Arc::new(roots);
    }

    /// Give the translator a handle to the shared diagnostics cache, so the
    /// respawn path can invalidate a respawned server's stale entries.
    ///
    /// Only called during single-owner setup (mirrors [`Self::with_router`]),
    /// before the translator is shared -- `serve_with` passes the same
    /// `Arc<Mutex<NotificationCache>>` used by the notification pump tasks.
    #[must_use]
    pub fn with_notification_cache(mut self, cache: Arc<Mutex<NotificationCache>>) -> Self {
        self.notification_cache = Some(cache);
        self
    }

    /// Mark the set of servers that are expected (configured + applicable)
    /// but may still be initializing in the background.
    pub fn set_expected_servers(&self, servers: HashSet<ServerId>) {
        *lock_std(&self.expected_servers) = servers;
    }

    /// Clear the expected-servers set (e.g. after background init failed).
    pub fn clear_expected_servers(&self) {
        lock_std(&self.expected_servers).clear();
    }

    /// Install the per-tool routing table built from the applicable configs.
    ///
    /// Only called during single-owner setup, before the translator is
    /// shared, so this replaces the `Arc`-wrapped router wholesale.
    #[must_use]
    pub fn with_router(mut self, router: ToolRouter) -> Self {
        self.router = Arc::new(StdMutex::new(router));
        self
    }

    /// Rebind the routing table to the set of servers that actually
    /// registered, dropping or redirecting routes to servers that failed to
    /// spawn. See `ToolRouter::rebind_to_registered` for the full semantics.
    pub fn rebind_router(&self, registered: &HashSet<ServerId>) {
        lock_std(&self.router).rebind_to_registered(registered);
    }

    /// Whether `id` is the server the router currently resolves
    /// `ToolKind::Diagnostics` to for `language_id`.
    ///
    /// Purpose-built for `register_servers`, which needs this to compute the
    /// diagnostics-cache filter passed into each pump task, without exposing
    /// the router's lock guard outside this module.
    #[must_use]
    pub fn is_diagnostics_route(&self, language_id: &str, id: &ServerId) -> bool {
        lock_std(&self.router).resolve(language_id, ToolKind::Diagnostics) == Some(id)
    }

    /// Configure custom file extension mappings.
    ///
    /// This method sets the extension map and updates the document tracker
    /// to use the same mappings for language detection.
    ///
    /// Only called during single-owner setup, before the translator is
    /// shared, so this replaces the `Arc`-wrapped fields wholesale.
    #[must_use]
    pub fn with_extensions(mut self, extension_map: HashMap<String, String>) -> Self {
        self.document_tracker = Arc::new(DocumentTracker::new(
            ResourceLimits::default(),
            extension_map.clone(),
        ));
        self.extension_map = Arc::new(extension_map);
        self
    }

    /// Register an LSP client under its routing identity.
    ///
    /// Only called once per server, from `register_servers` during initial
    /// background init. The respawn path does not reuse this method: it
    /// needs the previous client back (to fail its pending requests) and
    /// must also reset `document_tracker` for the swapped-in server, neither
    /// of which this method does.
    pub fn register_client(&self, id: impl Into<ServerId>, client: LspClient) {
        lock_std(&self.lsp_clients).insert(id.into(), client);
    }

    /// Register an LSP server under its routing identity.
    pub fn register_server(&self, id: impl Into<ServerId>, server: LspServer) {
        lock_std(&self.lsp_servers).insert(id.into(), server);
    }

    /// Store the config needed to respawn `id` if its process dies later.
    ///
    /// Called once per server, right after a successful spawn (see the
    /// crate-root `register_servers`); [`Self::respawn_if_dead`] is the only
    /// reader.
    pub(crate) fn register_server_config(&self, id: impl Into<ServerId>, config: ServerInitConfig) {
        lock_std(&self.server_configs).insert(id.into(), config);
    }

    /// Snapshot of currently open document paths, used for MCP resource listing.
    #[must_use]
    pub fn open_document_paths(&self) -> Vec<PathBuf> {
        self.document_tracker.open_paths()
    }

    /// Whether a document is currently tracked as open.
    #[must_use]
    pub fn is_document_open(&self, path: &Path) -> bool {
        self.document_tracker.is_open(path)
    }

    // TODO: These methods will be implemented in Phase 3-5
    // Initialize and shutdown are now handled by LspServer in lifecycle.rs

    // Future implementation will use LspServer instead of LspClient directly
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticRequestParams {
    text_document: TextDocumentIdentifier,
    #[serde(skip_serializing_if = "Option::is_none")]
    identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_result_id: Option<String>,
    #[serde(flatten)]
    work_done_progress_params: WorkDoneProgressParams,
    #[serde(flatten)]
    partial_result_params: PartialResultParams,
}

fn diagnostic_request_params(text_document: TextDocumentIdentifier) -> DiagnosticRequestParams {
    DiagnosticRequestParams {
        text_document,
        identifier: None,
        previous_result_id: None,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

/// Position in a document (1-based for MCP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position2D {
    /// Line number (1-based).
    pub line: u32,
    /// Character offset (1-based).
    pub character: u32,
}

/// Range in a document (1-based for MCP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    /// Start position.
    pub start: Position2D,
    /// End position.
    pub end: Position2D,
}

/// Location in a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    /// URI of the document.
    pub uri: String,
    /// Range within the document.
    pub range: Range,
}

/// Result of a hover request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoverResult {
    /// Hover contents as markdown string.
    pub contents: String,
    /// Optional range the hover applies to.
    pub range: Option<Range>,
}

/// Result of a definition request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefinitionResult {
    /// Locations of the definition.
    pub locations: Vec<Location>,
}

/// Result of a references request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencesResult {
    /// Locations of all references.
    pub locations: Vec<Location>,
}

/// Diagnostic severity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    /// Error diagnostic.
    Error,
    /// Warning diagnostic.
    Warning,
    /// Informational diagnostic.
    Information,
    /// Hint diagnostic.
    Hint,
}

/// A single diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Range where the diagnostic applies.
    pub range: Range,
    /// Severity of the diagnostic.
    pub severity: DiagnosticSeverity,
    /// Diagnostic message.
    pub message: String,
    /// Optional diagnostic code.
    pub code: Option<String>,
}

/// Result of a diagnostics request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsResult {
    /// List of diagnostics for the document.
    pub diagnostics: Vec<Diagnostic>,
}

/// A text edit operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEdit {
    /// Range to replace.
    pub range: Range,
    /// New text.
    pub new_text: String,
}

/// Changes to a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChanges {
    /// URI of the document.
    pub uri: String,
    /// List of edits to apply.
    pub edits: Vec<TextEdit>,
}

/// Result of a rename request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameResult {
    /// Changes to apply across documents.
    pub changes: Vec<DocumentChanges>,
}

/// A completion item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completion {
    /// Label of the completion.
    pub label: String,
    /// Kind of completion.
    pub kind: Option<String>,
    /// Detail information.
    pub detail: Option<String>,
    /// Documentation.
    pub documentation: Option<String>,
}

/// Result of a completions request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionsResult {
    /// List of completion items.
    pub items: Vec<Completion>,
}

/// A document symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    /// Name of the symbol.
    pub name: String,
    /// Kind of symbol.
    pub kind: String,
    /// Range of the symbol.
    pub range: Range,
    /// Selection range (identifier location).
    pub selection_range: Range,
    /// Child symbols.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<Self>>,
}

/// Result of a document symbols request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSymbolsResult {
    /// List of symbols in the document.
    pub symbols: Vec<Symbol>,
}

/// Result of a format document request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatDocumentResult {
    /// List of edits to format the document.
    pub edits: Vec<TextEdit>,
}

/// A workspace symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSymbol {
    /// Name of the symbol.
    pub name: String,
    /// Kind of symbol.
    pub kind: String,
    /// Location of the symbol.
    pub location: Location,
    /// Optional container name (parent scope).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
}

/// Result of workspace symbol search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSymbolResult {
    /// List of symbols found.
    pub symbols: Vec<WorkspaceSymbol>,
}

/// A single code action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeAction {
    /// Title of the code action.
    pub title: String,
    /// Kind of code action (quickfix, refactor, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Diagnostics that this action resolves.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<Diagnostic>,
    /// Workspace edit to apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit: Option<WorkspaceEditDescription>,
    /// Command to execute.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandDescription>,
    /// Whether this is the preferred action.
    #[serde(default)]
    pub is_preferred: bool,
}

/// Description of a workspace edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEditDescription {
    /// Changes to apply to documents.
    pub changes: Vec<DocumentChanges>,
}

/// Description of a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDescription {
    /// Title of the command.
    pub title: String,
    /// Command identifier.
    pub command: String,
    /// Command arguments.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub arguments: Vec<serde_json::Value>,
}

/// Result of code actions request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeActionsResult {
    /// Available code actions.
    pub actions: Vec<CodeAction>,
}

/// A call hierarchy item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallHierarchyItemResult {
    /// Name of the symbol.
    pub name: String,
    /// LSP numeric symbol kind (e.g. 12 for Function).
    pub kind: u32,
    /// More detail for this item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// URI of the document.
    pub uri: String,
    /// Range of the symbol.
    pub range: Range,
    /// Selection range (identifier location).
    ///
    /// Serialized as `selectionRange` (camelCase) so that the value returned by
    /// `prepare_call_hierarchy` round-trips correctly when the MCP client passes
    /// it back to `get_incoming_calls` / `get_outgoing_calls`, which deserialize
    /// it as `lsp_types::CallHierarchyItem` (camelCase).
    #[serde(rename = "selectionRange")]
    pub selection_range: Range,
    /// Opaque data to pass to incoming/outgoing calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Result of call hierarchy prepare request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallHierarchyPrepareResult {
    /// List of callable items at the position.
    pub items: Vec<CallHierarchyItemResult>,
}

/// An incoming call (caller of the current item).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingCall {
    /// The item that calls the current item.
    pub from: CallHierarchyItemResult,
    /// Ranges where the call occurs.
    pub from_ranges: Vec<Range>,
}

/// Result of incoming calls request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingCallsResult {
    /// List of incoming calls.
    pub calls: Vec<IncomingCall>,
}

/// An outgoing call (callee from the current item).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingCall {
    /// The item being called.
    pub to: CallHierarchyItemResult,
    /// Ranges where the call occurs.
    pub from_ranges: Vec<Range>,
}

/// Result of outgoing calls request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingCallsResult {
    /// List of outgoing calls.
    pub calls: Vec<OutgoingCall>,
}

/// Result of server logs request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerLogsResult {
    /// List of log entries.
    pub logs: Vec<crate::bridge::notifications::LogEntry>,
}

/// Result of server messages request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMessagesResult {
    /// List of server messages.
    pub messages: Vec<crate::bridge::notifications::ServerMessage>,
}

/// A single parameter in a signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureParameter {
    /// Label of the parameter.
    pub label: String,
    /// Optional documentation for the parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
}

/// A single signature overload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureInfo {
    /// Full label of the signature.
    pub label: String,
    /// Optional documentation for the signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    /// Parameters of the signature.
    pub parameters: Vec<SignatureParameter>,
}

/// Result of a signature help request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureHelpResult {
    /// Available signatures.
    pub signatures: Vec<SignatureInfo>,
    /// Index of the active signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_signature: Option<u32>,
    /// Index of the active parameter within the active signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_parameter: Option<u32>,
}

/// Result of a go-to-implementation or go-to-type-definition request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationsResult {
    /// Locations found.
    pub locations: Vec<Location>,
}

/// A single inlay hint entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlayHintEntry {
    /// Position of the hint (1-based MCP).
    pub position: Position2D,
    /// Label text for the hint.
    pub label: String,
    /// Hint kind (1 = Type, 2 = Parameter).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<u8>,
    /// Whether to add a space before the hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding_left: Option<bool>,
    /// Whether to add a space after the hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding_right: Option<bool>,
    /// Tooltip text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
}

/// Result of an inlay hints request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlayHintsResult {
    /// List of inlay hints.
    pub hints: Vec<InlayHintEntry>,
}

/// Maximum allowed position value for validation.
const MAX_POSITION_VALUE: u32 = 1_000_000;
/// Maximum allowed range size in lines.
const MAX_RANGE_LINES: u32 = 10_000;

/// Validate that `path` is within one of `workspace_roots`.
///
/// Free function (rather than a `Translator` method) so callers that only need
/// path validation — e.g. cache-only MCP handlers — can validate against a
/// cloned, lock-free snapshot of the workspace roots instead of locking the
/// full `Arc<Mutex<Translator>>`, which may be held elsewhere across a slow
/// in-flight LSP round-trip.
///
/// # Errors
///
/// Returns `Error::PathOutsideWorkspace` if the path is outside all workspace roots.
pub fn validate_path_against_roots(path: &Path, workspace_roots: &[PathBuf]) -> Result<PathBuf> {
    let canonical = path.canonicalize().map_err(|e| Error::FileIo {
        path: path.to_path_buf(),
        source: e,
    })?;

    // If no workspace roots configured, allow any path (backward compatibility)
    if workspace_roots.is_empty() {
        return Ok(canonical);
    }

    // Check if path is within any workspace root
    for root in workspace_roots {
        if let Ok(canonical_root) = root.canonicalize()
            && canonical.starts_with(&canonical_root)
        {
            return Ok(canonical);
        }
    }

    Err(Error::PathOutsideWorkspace(path.to_path_buf()))
}

impl Translator {
    /// Validate that a path is within allowed workspace boundaries.
    ///
    /// # Errors
    ///
    /// Returns `Error::PathOutsideWorkspace` if the path is outside all workspace roots.
    pub(crate) fn validate_path(&self, path: &Path) -> Result<PathBuf> {
        validate_path_against_roots(path, &self.workspace_roots)
    }

    /// Whether the server tracked under `id` is registered and has exited.
    ///
    /// Returns `false` ("not dead") for an `id` that isn't registered at
    /// all -- that's the separate `ServerInitializing`/`NoServerForTool`
    /// concern callers already handle, not something the respawn path
    /// should react to -- and for any `try_wait` error, on the conservative
    /// assumption that a health check that itself failed should not trigger
    /// a respawn.
    fn is_server_dead(&self, id: &ServerId) -> bool {
        lock_std(&self.lsp_servers)
            .get_mut(id)
            .and_then(|server| server.has_exited().ok())
            .unwrap_or(false)
    }

    /// Return the shared single-flight lock for `id`, creating it on first
    /// use.
    ///
    /// Two concurrent callers racing to respawn the same server both get a
    /// clone of the *same* underlying `Mutex`, so awaiting it actually
    /// serializes them instead of letting both proceed independently.
    fn respawn_lock(&self, id: &ServerId) -> Arc<Mutex<()>> {
        Arc::clone(
            lock_std(&self.respawn_locks)
                .entry(id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Remaining backoff delay before `id` may be respawned again, or
    /// `None` if it may be attempted right now.
    ///
    /// Only consults recorded *failures* -- a server with no recorded
    /// attempt is never backed off. A server whose last attempt "succeeded"
    /// is reconciled by [`Self::reconcile_respawn_stability`] (called by
    /// [`Self::respawn_if_dead`] before this) into either a failure (died
    /// again too soon) or removed entirely (proven stable), so by the time
    /// this runs, a lingering "succeeded" entry never reaches here.
    fn respawn_backoff_remaining(&self, id: &ServerId) -> Option<Duration> {
        let (consecutive_failures, last_attempt) = {
            let entry = lock_std(&self.respawn_backoffs).get(id).copied()?;
            (entry.consecutive_failures, entry.last_attempt)
        };
        if consecutive_failures == 0 {
            return None;
        }
        let shift = consecutive_failures.saturating_sub(1).min(5);
        let delay = RESPAWN_BACKOFF_BASE
            .saturating_mul(1 << shift)
            .min(RESPAWN_BACKOFF_MAX);
        let elapsed = last_attempt.elapsed();
        (elapsed < delay).then(|| delay.saturating_sub(elapsed))
    }

    /// Records a failed respawn attempt for `id`, extending its backoff.
    fn record_respawn_failure(&self, id: &ServerId) {
        let mut backoffs = lock_std(&self.respawn_backoffs);
        let entry = backoffs
            .entry(id.clone())
            .or_insert_with(|| RespawnBackoff {
                consecutive_failures: 0,
                last_attempt: Instant::now(),
                last_attempt_succeeded: false,
            });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.last_attempt = Instant::now();
        entry.last_attempt_succeeded = false;
        drop(backoffs);
    }

    /// Records that a respawn attempt for `id` completed `initialize`
    /// successfully.
    ///
    /// Does *not* clear `consecutive_failures`: whether this attempt
    /// actually broke the crash loop is only known once the server either
    /// stays alive for a while or is found dead again -- see
    /// [`Self::reconcile_respawn_stability`], which is what acts on this
    /// entry.
    fn record_respawn_success(&self, id: &ServerId) {
        let mut backoffs = lock_std(&self.respawn_backoffs);
        let entry = backoffs
            .entry(id.clone())
            .or_insert_with(|| RespawnBackoff {
                consecutive_failures: 0,
                last_attempt: Instant::now(),
                last_attempt_succeeded: true,
            });
        entry.last_attempt = Instant::now();
        entry.last_attempt_succeeded = true;
        drop(backoffs);
    }

    /// Reconciles `id`'s backoff state against a *newly observed* death,
    /// before deciding whether to back off this respawn attempt.
    ///
    /// A no-op unless the last recorded attempt "succeeded" ([`Self::record_respawn_success`]):
    /// - If it has since survived at least [`RESPAWN_BACKOFF_BASE`], it is
    ///   treated as proven stable and its backoff state is cleared -- a
    ///   later, unrelated crash starts a fresh backoff sequence rather than
    ///   inheriting history from a long-resolved incident.
    /// - Otherwise, the server died again before proving itself: this
    ///   counts as a failure (extending `consecutive_failures`) instead of
    ///   being silently forgotten. Without this, a server that starts,
    ///   completes `initialize`, and crashes again a moment later would
    ///   bypass backoff entirely -- every such cycle would look like a
    ///   fresh, unbacked-off start, spawning one child process per tool
    ///   call forever.
    fn reconcile_respawn_stability(&self, id: &ServerId) {
        let Some(entry) = lock_std(&self.respawn_backoffs).get(id).copied() else {
            return;
        };
        if !entry.last_attempt_succeeded {
            return;
        }
        if entry.last_attempt.elapsed() >= RESPAWN_BACKOFF_BASE {
            lock_std(&self.respawn_backoffs).remove(id);
        } else {
            let mut backoffs = lock_std(&self.respawn_backoffs);
            if let Some(current) = backoffs.get_mut(id) {
                current.consecutive_failures = current.consecutive_failures.saturating_add(1);
                current.last_attempt = Instant::now();
                current.last_attempt_succeeded = false;
            }
        }
    }

    /// Detect whether the server routed to `id` has crashed and, if so,
    /// eagerly respawn and re-initialize it before returning.
    ///
    /// A no-op if `id` names a server that was never registered (routing
    /// resolved to it, but it hasn't started yet or never will) or is still
    /// alive.
    ///
    /// # Concurrency
    ///
    /// Multiple callers can race in here for the same `id` -- e.g. two tool
    /// calls landing back-to-back right after the process dies. They
    /// single-flight on [`Self::respawn_lock`]: the first to acquire it
    /// performs the actual respawn; everyone else waits for that attempt to
    /// finish (or fail), rechecks, and finds nothing left to do.
    ///
    /// Requests still parked in the dead client's `pending_requests` are
    /// failed immediately via [`LspClient::fail_pending_requests`] instead
    /// of being left to time out on their own.
    ///
    /// The respawned process has no memory of any document the old one had
    /// open, so this also clears `document_tracker`'s per-server sync
    /// history for `id` -- otherwise `ensure_open` would send `didChange`
    /// instead of `didOpen` for a document the new process never saw. Any
    /// diagnostics cached from the old connection are invalidated (see
    /// [`Self::with_notification_cache`]) rather than left to be merged into
    /// fresh pulls as if still current.
    ///
    /// Diagnostics and other push notifications from the new process itself
    /// are drained and discarded rather than wired into the existing pump
    /// task: the pump's remaining dependencies (resource subscriptions, peer
    /// handle) live in `serve_with`'s scope, not the translator's, so
    /// reconnecting live push for a respawned server is out of scope for
    /// this fix -- it does not resume until the whole mcpls process
    /// restarts, but stale data is no longer served as current.
    ///
    /// A crash-looping server (repeated respawn failures) backs off
    /// exponentially (`RESPAWN_BACKOFF_BASE` up to `RESPAWN_BACKOFF_MAX`)
    /// instead of retrying on every single tool call, each of which would
    /// otherwise cost up to a full `timeout_seconds` inside `initialize`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ServerUnavailable`] if no respawn config was ever
    /// registered for `id`, or if it is currently within its backoff
    /// window. Returns whatever error `LspServer::spawn` produced (e.g. its
    /// command is no longer on `PATH`, or `initialize` fails again) if an
    /// actual respawn attempt failed.
    async fn respawn_if_dead(&self, id: &ServerId) -> Result<()> {
        if !self.is_server_dead(id) {
            return Ok(());
        }

        let lock = self.respawn_lock(id);
        let _guard = lock.lock().await;

        // Another caller may have already respawned it while we waited.
        if !self.is_server_dead(id) {
            return Ok(());
        }

        self.reconcile_respawn_stability(id);

        if let Some(remaining) = self.respawn_backoff_remaining(id) {
            tracing::warn!(
                "LSP server '{id}' is crash-looping, backing off for {remaining:?} \
                 before the next respawn attempt"
            );
            return Err(Error::ServerUnavailable {
                server_id: id.clone(),
                reason: format!("crash-looping, retry in {remaining:?}"),
            });
        }

        let Some(config) = lock_std(&self.server_configs).get(id).cloned() else {
            return Err(Error::ServerUnavailable {
                server_id: id.clone(),
                reason: "no respawn config registered for this server".to_string(),
            });
        };
        let language_id = config.server_config.language_id.clone();

        tracing::warn!("LSP server '{id}' has crashed, respawning");
        let mut new_server = match LspServer::spawn(config).await {
            Ok(server) => {
                self.record_respawn_success(id);
                server
            }
            Err(err) => {
                self.record_respawn_failure(id);
                return Err(err);
            }
        };
        let new_client = new_server.client().clone();
        let mut notification_rx = new_server.take_notification_rx();
        tokio::spawn(async move { while notification_rx.recv().await.is_some() {} });

        let old_client = lock_std(&self.lsp_clients).insert(id.clone(), new_client);
        let old_server = lock_std(&self.lsp_servers).insert(id.clone(), new_server);
        drop(old_server); // dropped after the `lsp_servers` guard, not under it

        self.document_tracker.forget_server(id);

        // Only the diagnostics-route server for this language ever writes
        // to the cache (see `diagnostics_pump`'s `caches_diagnostics` gate
        // in the crate root) -- clearing a non-route server's synced URIs
        // would delete the *healthy* diagnostics server's valid entries for
        // those same files instead. And the route server's own cache
        // entries are not limited to documents mcpls ever opened (it
        // publishes workspace-wide, e.g. `cargo check` diagnostics), so a
        // per-URI clear scoped to synced documents would miss most of what
        // needs invalidating.
        //
        // `clear_all_diagnostics` is workspace-wide, not scoped to this
        // server's language: in a multi-language workspace, a crashed
        // rust-analyzer also wipes a healthy pyright's cached diagnostics
        // for Python files. Accepted as bounded collateral rather than
        // fixed here -- `handle_diagnostics`'s authoritative pull path is
        // unaffected by this, only `get_cached_diagnostics` degrades for
        // the unrelated language until that server republishes.
        // `NotificationCache` has no per-language clear to scope this to
        // yet; a per-language key iterator would be the proper fix.
        //
        // This clear is not atomic with the swap above: a caller that reads
        // `lsp_clients` between the swap and this point sees the new client
        // and could read a not-yet-cleared cache entry. In practice
        // `handle_diagnostics` only reads the cache after a full LSP pull
        // round-trip, so this window is negligible.
        if self.is_diagnostics_route(&language_id, id)
            && let Some(cache) = &self.notification_cache
        {
            cache.lock().await.clear_all_diagnostics();
        }

        if let Some(old_client) = old_client {
            old_client.fail_pending_requests().await;
        }

        tracing::info!("LSP server '{id}' respawned successfully");
        Ok(())
    }

    /// Resolve the client and routing identity for `path`/`tool`, giving the
    /// resolved server a chance to be respawned first if its process has
    /// died.
    ///
    /// Thin async wrapper around [`Self::get_client_for_file`] (kept
    /// synchronous so its existing unit tests don't need a runtime): this is
    /// the entry point async handlers call instead, so a dead server is
    /// transparently replaced before its stale client is handed back.
    async fn resolve_client_for_file(
        &self,
        path: &Path,
        tool: ToolKind,
    ) -> Result<(ServerId, LspClient)> {
        let (id, client) = self.get_client_for_file(path, tool)?;
        self.respawn_if_dead(&id).await?;
        let client = lock_std(&self.lsp_clients)
            .get(&id)
            .cloned()
            .unwrap_or(client);
        Ok((id, client))
    }

    /// Resolve the server that should handle `tool` for the file at `path`,
    /// returning both its routing identity and a cloned client.
    ///
    /// Tries the file's detected language first, then (if that has no route)
    /// its React base language (`.tsx` falling back from `typescriptreact` to
    /// `typescript`, and similarly for `.jsx`) -- in that order, so an
    /// explicit `typescriptreact` server still wins over the `typescript`
    /// fallback when both are configured.
    ///
    /// Locks `router`, `lsp_clients`, and (on the not-yet-registered path)
    /// `expected_servers` only for their respective lookups — every guard is
    /// dropped before this method returns.
    fn get_client_for_file(&self, path: &Path, tool: ToolKind) -> Result<(ServerId, LspClient)> {
        let language = detect_language(path, &self.extension_map);
        let mut candidates: Vec<&str> = vec![language.as_str()];
        if let Some(base) = base_language_id(&language) {
            candidates.push(base);
        }

        for lang in &candidates {
            let resolved = lock_std(&self.router).resolve(lang, tool).cloned();
            let Some(id) = resolved else { continue };

            let found = lock_std(&self.lsp_clients).get(&id).cloned();
            if let Some(client) = found {
                return Ok((id, client));
            }
            // A route naming a server that is still initializing (e.g. a
            // large Unity solution loading via OmniSharp) -- tell the caller
            // to wait and retry rather than implying no server is configured.
            if lock_std(&self.expected_servers).contains(&id) {
                return Err(Error::ServerInitializing { server_id: id });
            }
            // Unreachable once registration has rebound the router
            // (`Translator::rebind_router`) -- a route can only name a
            // registered server after that point. Logged rather than
            // `debug_assert!`-panicked: this method is reachable by any
            // library consumer calling `with_router` without registering
            // matching clients, not just internal misuse.
            tracing::error!(
                "router route names server '{id}' for tool '{tool}' that is neither \
                 registered nor expected"
            );
            return Err(Error::NoServerForTool {
                language_id: (*lang).to_string(),
                tool,
            });
        }

        let has_language = {
            let router = lock_std(&self.router);
            candidates.iter().any(|lang| router.has_language(lang))
        };
        if has_language {
            Err(Error::NoServerForTool {
                language_id: language,
                tool,
            })
        } else {
            Err(Error::NoServerForLanguage(language))
        }
    }

    /// Validate `file_path`, then resolve its routed client via
    /// [`Self::resolve_client_for_file`] (respawn-aware), without opening
    /// the document.
    ///
    /// Split out from [`Self::prepare_document`] so [`Self::prepare_gated_document`]
    /// can check the routed server's capabilities *before* `ensure_open` sends
    /// `textDocument/didOpen` -- a server rejected by the gate should never
    /// observe an open notification for a request it can't service. Also
    /// used directly by handlers that already have a resolved `PathBuf`
    /// (from `parse_file_uri`) but still need capability gating, e.g.
    /// `handle_incoming_calls`/`handle_outgoing_calls`.
    async fn resolve_validated_client_for_file(
        &self,
        file_path: &str,
        tool: ToolKind,
    ) -> Result<(ServerId, LspClient, PathBuf)> {
        let path = PathBuf::from(file_path);
        let validated_path = self.validate_path(&path)?;
        let (server_id, client) = self.resolve_client_for_file(&validated_path, tool).await?;
        Ok((server_id, client, validated_path))
    }

    /// Resolve the LSP client and ensure the document is open.
    ///
    /// This is the "prepare" phase shared by every LSP-round-trip handler:
    /// it validates the path, selects the client via
    /// [`Self::resolve_validated_client_for_file`] (respawn-aware), and
    /// calls `ensure_open`, which locks the document tracker's state only
    /// for the given path. The returned client and URI are owned values, so
    /// the caller can issue the actual LSP request (the "execute" phase)
    /// without holding any lock across the network round trip.
    ///
    /// `ensure_open`'s own awaits (a `stat`, optionally a re-read of the
    /// file, and the `textDocument/didOpen`/`didChange` notify) run under a
    /// lock scoped to `validated_path` alone — see [`DocumentTracker::ensure_open`]
    /// — so a slow or wedged language server cannot stall `prepare_document`
    /// calls for unrelated files. (Per-tool routing, #228, means the same
    /// file can be routed to more than one server; a wedged server-A notify
    /// still holds this path's lock and can therefore delay a healthy
    /// server-B call for that *same* file.)
    async fn prepare_document(
        &self,
        file_path: &str,
        tool: ToolKind,
    ) -> Result<(ServerId, LspClient, lsp_types::Uri)> {
        let (server_id, client, validated_path) = self
            .resolve_validated_client_for_file(file_path, tool)
            .await?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &server_id, &client)
            .await?;
        Ok((server_id, client, uri))
    }

    /// Like [`Self::prepare_document`], but checks `capability` against the
    /// routed server's `ServerCapabilities` *before* opening the document --
    /// see [`Self::resolve_client_for_file`]'s doc comment for why the
    /// ordering matters.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CapabilityNotSupported`] if the routed server's
    /// `ServerCapabilities` explicitly does not advertise `capability`.
    async fn prepare_gated_document(
        &self,
        file_path: &str,
        tool: ToolKind,
        capability: &'static str,
        supported: impl FnOnce(&lsp_types::ServerCapabilities) -> bool,
    ) -> Result<(ServerId, LspClient, lsp_types::Uri)> {
        let (server_id, client, validated_path) = self
            .resolve_validated_client_for_file(file_path, tool)
            .await?;
        self.require_capability(&server_id, capability, supported)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &server_id, &client)
            .await?;
        Ok((server_id, client, uri))
    }

    /// Verify the routed server advertises support for a capability before
    /// dispatching a capability-gated LSP request.
    ///
    /// Production always registers an [`LspServer`] alongside its
    /// [`LspClient`] in the same `register_servers` step (see `lib.rs`), so in
    /// practice a registered client always has known capabilities. If no
    /// `LspServer` is registered for `server_id` regardless -- a client
    /// registered without its server, which only happens in tests, or a
    /// narrow window during registration where the two maps are inserted
    /// under separate locks -- the capability is assumed supported rather
    /// than blocking the request: this mirrors the graceful-degradation
    /// stance used elsewhere in `Translator` when capability information is
    /// unavailable rather than known-absent.
    ///
    /// Note: this checks the `ServerCapabilities` snapshot captured at
    /// `initialize` time. A server that advertises a capability later via
    /// `client/registerCapability` (dynamic registration) is not reflected
    /// here and will be incorrectly rejected; mcpls does not currently apply
    /// dynamic registrations back onto the stored capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CapabilityNotSupported`] if the registered server's
    /// `ServerCapabilities` explicitly does not advertise `capability`.
    fn require_capability(
        &self,
        server_id: &ServerId,
        capability: &'static str,
        supported: impl FnOnce(&lsp_types::ServerCapabilities) -> bool,
    ) -> Result<()> {
        let servers = lock_std(&self.lsp_servers);
        match servers.get(server_id) {
            Some(server) if !supported(server.capabilities()) => {
                Err(Error::CapabilityNotSupported {
                    server_id: server_id.clone(),
                    capability,
                })
            }
            _ => Ok(()),
        }
    }

    /// Parse and validate a file URI, returning the validated path.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The URI doesn't have a file:// scheme
    /// - The path is outside workspace boundaries
    fn parse_file_uri(&self, uri: &lsp_types::Uri) -> Result<PathBuf> {
        let uri_str = uri.as_str();

        // Validate file:// scheme
        if !uri_str.starts_with("file://") {
            return Err(Error::InvalidToolParams(format!(
                "Invalid URI scheme, expected file:// but got: {uri_str}"
            )));
        }

        // Extract path after file://
        let path_str = &uri_str["file://".len()..];

        // Handle Windows paths: file:///C:/path -> /C:/path -> C:/path
        // On Windows, URIs have format file:///C:/path, so we need to strip the leading /
        #[cfg(windows)]
        let path_str = if path_str.len() >= 3
            && path_str.starts_with('/')
            && path_str.chars().nth(2) == Some(':')
        {
            &path_str[1..]
        } else {
            path_str
        };

        let path = PathBuf::from(path_str);

        // Validate path is within workspace
        self.validate_path(&path)
    }

    /// Handle hover request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `hoverProvider` support.
    pub async fn handle_hover(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<HoverResult> {
        let (_server_id, client, uri) = self
            .prepare_gated_document(&file_path, ToolKind::Hover, "hoverProvider", |caps| {
                matches!(
                    caps.hover_provider,
                    Some(
                        lsp_types::HoverProviderCapability::Simple(true)
                            | lsp_types::HoverProviderCapability::Options(_)
                    )
                )
            })
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = LspHoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Hover> = client
            .request("textDocument/hover", params, timeout_duration)
            .await?;

        let result = match response {
            Some(hover) => {
                let contents = extract_hover_contents(hover.contents);
                let range = hover.range.map(normalize_range);
                HoverResult { contents, range }
            }
            None => HoverResult {
                contents: "No hover information available".to_string(),
                range: None,
            },
        };

        Ok(result)
    }

    /// Handle definition request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `definitionProvider` support.
    pub async fn handle_definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<DefinitionResult> {
        let (_server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::Definition,
                "definitionProvider",
                |caps| {
                    matches!(
                        caps.definition_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::GotoDefinitionResponse> = client
            .request("textDocument/definition", params, timeout_duration)
            .await?;

        let locations = match response {
            Some(lsp_types::GotoDefinitionResponse::Scalar(loc)) => vec![loc],
            Some(lsp_types::GotoDefinitionResponse::Array(locs)) => locs,
            Some(lsp_types::GotoDefinitionResponse::Link(links)) => links
                .into_iter()
                .map(|link| lsp_types::Location {
                    uri: link.target_uri,
                    range: link.target_selection_range,
                })
                .collect(),
            None => vec![],
        };

        let result = DefinitionResult {
            locations: locations
                .into_iter()
                .map(|loc| Location {
                    uri: loc.uri.to_string(),
                    range: normalize_range(loc.range),
                })
                .collect(),
        };

        Ok(result)
    }

    /// Handle references request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `referencesProvider` support.
    pub async fn handle_references(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<ReferencesResult> {
        let (_server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::References,
                "referencesProvider",
                |caps| {
                    matches!(
                        caps.references_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration,
            },
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<lsp_types::Location>> = client
            .request("textDocument/references", params, timeout_duration)
            .await?;

        let locations = response.unwrap_or_default();

        let result = ReferencesResult {
            locations: locations
                .into_iter()
                .map(|loc| Location {
                    uri: loc.uri.to_string(),
                    range: normalize_range(loc.range),
                })
                .collect(),
        };

        Ok(result)
    }

    /// Handle diagnostics request.
    ///
    /// Merges the LSP pull-model response (`textDocument/diagnostic`) with
    /// whatever is already cached from `textDocument/publishDiagnostics` push
    /// notifications for the same file, so this returns the same diagnostics
    /// `get_cached_diagnostics` would for the file at the same point in time
    /// (see #244 — rust-analyzer's pull endpoint omits flycheck/clippy-sourced
    /// diagnostics, and empirically also some native ones, that are only ever
    /// delivered via the push path). If the pull request itself fails (e.g. a
    /// push-only server answering `-32601`, or a timeout), a non-empty cache
    /// entry is returned as a cache-only result instead of propagating the
    /// error, since the cache is not required to be fresher than the pull
    /// response to be useful here.
    ///
    /// The cache is read only after the pull request settles (success or
    /// failure) and held only for the lookup itself — never across the LSP
    /// round-trip — matching the lock-ordering discipline documented on
    /// `cached_diagnostics_uri`. Like `get_cached_diagnostics`, the cache is
    /// treated as eventually consistent: a cached entry may reflect a
    /// slightly older document version than the fresh pull result if an edit
    /// landed inside the server's flycheck debounce window.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP pull request fails and the cache holds no
    /// diagnostics for the file either, or if the file cannot be opened.
    pub async fn handle_diagnostics(
        &self,
        file_path: String,
        notification_cache: &Mutex<NotificationCache>,
    ) -> Result<DiagnosticsResult> {
        let (_server_id, client, uri) = self
            .prepare_document(&file_path, ToolKind::Diagnostics)
            .await?;

        let params = diagnostic_request_params(TextDocumentIdentifier { uri: uri.clone() });

        let timeout_duration = Duration::from_secs(30);
        let pull_response: Result<lsp_types::DocumentDiagnosticReportResult> = client
            .request("textDocument/diagnostic", params, timeout_duration)
            .await;

        let diag_info = {
            let cache = notification_cache.lock().await;
            cache.get_diagnostics(uri.as_str()).cloned()
        };

        match pull_response {
            Ok(response) => {
                let items = match response {
                    lsp_types::DocumentDiagnosticReportResult::Report(report) => match report {
                        lsp_types::DocumentDiagnosticReport::Full(full) => {
                            full.full_document_diagnostic_report.items
                        }
                        lsp_types::DocumentDiagnosticReport::Unchanged(_) => vec![],
                    },
                    lsp_types::DocumentDiagnosticReportResult::Partial(_) => vec![],
                };
                let pull = DiagnosticsResult {
                    diagnostics: items.iter().map(diagnostic_to_mcp).collect(),
                };
                Ok(Self::merge_diagnostics(pull, diag_info.as_ref()))
            }
            Err(e) => {
                let cache_only = Self::diagnostics_from_cache_entry(diag_info.as_ref());
                if cache_only.diagnostics.is_empty() {
                    Err(e)
                } else {
                    Ok(cache_only)
                }
            }
        }
    }

    /// Handle rename request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `renameProvider` support.
    pub async fn handle_rename(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        new_name: String,
    ) -> Result<RenameResult> {
        let (_server_id, client, uri) = self
            .prepare_gated_document(&file_path, ToolKind::Rename, "renameProvider", |caps| {
                matches!(
                    caps.rename_provider,
                    Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                )
            })
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = LspRenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            new_name,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<WorkspaceEdit> = client
            .request("textDocument/rename", params, timeout_duration)
            .await?;

        let changes = if let Some(edit) = response {
            let mut result_changes = Vec::new();

            // Prefer the legacy `changes` map (HashMap<Uri, Vec<TextEdit>>).
            if let Some(changes_map) = edit.changes {
                for (uri, edits) in changes_map {
                    result_changes.push(DocumentChanges {
                        uri: uri.to_string(),
                        edits: edits
                            .into_iter()
                            .map(|e| TextEdit {
                                range: normalize_range(e.range),
                                new_text: e.new_text,
                            })
                            .collect(),
                    });
                }
            }

            // Also handle `documentChanges` (array format returned by rust-analyzer).
            if result_changes.is_empty() {
                let text_doc_edits = match edit.document_changes {
                    Some(lsp_types::DocumentChanges::Edits(edits)) => edits,
                    Some(lsp_types::DocumentChanges::Operations(ops)) => ops
                        .into_iter()
                        .filter_map(|op| match op {
                            lsp_types::DocumentChangeOperation::Edit(e) => Some(e),
                            lsp_types::DocumentChangeOperation::Op(_) => None,
                        })
                        .collect(),
                    None => vec![],
                };
                for tde in text_doc_edits {
                    result_changes.push(DocumentChanges {
                        uri: tde.text_document.uri.to_string(),
                        edits: tde
                            .edits
                            .into_iter()
                            .map(|one_of| match one_of {
                                lsp_types::OneOf::Left(te) => TextEdit {
                                    range: normalize_range(te.range),
                                    new_text: te.new_text,
                                },
                                lsp_types::OneOf::Right(ate) => TextEdit {
                                    range: normalize_range(ate.text_edit.range),
                                    new_text: ate.text_edit.new_text,
                                },
                            })
                            .collect(),
                    });
                }
            }

            result_changes
        } else {
            vec![]
        };

        Ok(RenameResult { changes })
    }

    /// Handle completions request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `completionProvider` support.
    pub async fn handle_completions(
        &self,
        file_path: String,
        line: u32,
        character: u32,
        trigger: Option<String>,
    ) -> Result<CompletionsResult> {
        let (_server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::Completions,
                "completionProvider",
                |caps| caps.completion_provider.is_some(),
            )
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let context = trigger.map(|trigger_char| lsp_types::CompletionContext {
            trigger_kind: CompletionTriggerKind::TRIGGER_CHARACTER,
            trigger_character: Some(trigger_char),
        });

        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context,
        };

        let timeout_duration = Duration::from_secs(10);
        let response: Option<lsp_types::CompletionResponse> = client
            .request("textDocument/completion", params, timeout_duration)
            .await?;

        let items = match response {
            Some(lsp_types::CompletionResponse::Array(items)) => items,
            Some(lsp_types::CompletionResponse::List(list)) => list.items,
            None => vec![],
        };

        let result = CompletionsResult {
            items: items
                .into_iter()
                .map(|item| Completion {
                    label: item.label,
                    kind: item.kind.map(|k| format!("{k:?}")),
                    detail: item.detail,
                    documentation: item.documentation.map(|doc| match doc {
                        lsp_types::Documentation::String(s) => s,
                        lsp_types::Documentation::MarkupContent(m) => m.value,
                    }),
                })
                .collect(),
        };

        Ok(result)
    }

    /// Handle document symbols request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `documentSymbolProvider` support.
    pub async fn handle_document_symbols(
        &self,
        file_path: String,
    ) -> Result<DocumentSymbolsResult> {
        let (_server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::DocumentSymbols,
                "documentSymbolProvider",
                |caps| {
                    matches!(
                        caps.document_symbol_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;

        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::DocumentSymbolResponse> = client
            .request("textDocument/documentSymbol", params, timeout_duration)
            .await?;

        let symbols = match response {
            Some(lsp_types::DocumentSymbolResponse::Flat(symbols)) => symbols
                .into_iter()
                .map(|sym| Symbol {
                    name: sym.name,
                    kind: format!("{:?}", sym.kind),
                    range: normalize_range(sym.location.range),
                    selection_range: normalize_range(sym.location.range),
                    children: None,
                })
                .collect(),
            Some(lsp_types::DocumentSymbolResponse::Nested(symbols)) => {
                symbols.into_iter().map(convert_document_symbol).collect()
            }
            None => vec![],
        };

        Ok(DocumentSymbolsResult { symbols })
    }

    /// Handle format document request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `documentFormattingProvider` support.
    pub async fn handle_format_document(
        &self,
        file_path: String,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<FormatDocumentResult> {
        let (_server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::FormatDocument,
                "documentFormattingProvider",
                |caps| {
                    matches!(
                        caps.document_formatting_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;

        let params = DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri },
            options: FormattingOptions {
                tab_size,
                insert_spaces,
                ..Default::default()
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<lsp_types::TextEdit>> = client
            .request("textDocument/formatting", params, timeout_duration)
            .await?;

        let edits = response.unwrap_or_default();

        let result = FormatDocumentResult {
            edits: edits
                .into_iter()
                .map(|edit| TextEdit {
                    range: normalize_range(edit.range),
                    new_text: edit.new_text,
                })
                .collect(),
        };

        Ok(result)
    }

    /// Handle workspace symbol search.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, no server is configured, or
    /// the routed server does not advertise `workspaceSymbolProvider` support.
    pub async fn handle_workspace_symbol(
        &self,
        query: String,
        kind_filter: Option<String>,
        limit: u32,
    ) -> Result<WorkspaceSymbolResult> {
        const MAX_QUERY_LENGTH: usize = 1000;
        const VALID_SYMBOL_KINDS: &[&str] = &[
            "File",
            "Module",
            "Namespace",
            "Package",
            "Class",
            "Method",
            "Property",
            "Field",
            "Constructor",
            "Enum",
            "Interface",
            "Function",
            "Variable",
            "Constant",
            "String",
            "Number",
            "Boolean",
            "Array",
            "Object",
            "Key",
            "Null",
            "EnumMember",
            "Struct",
            "Event",
            "Operator",
            "TypeParameter",
        ];

        // Validate query length
        if query.len() > MAX_QUERY_LENGTH {
            return Err(Error::InvalidToolParams(format!(
                "Query too long: {} chars (max {MAX_QUERY_LENGTH})",
                query.len()
            )));
        }

        // Validate kind filter
        if let Some(ref kind) = kind_filter
            && !VALID_SYMBOL_KINDS
                .iter()
                .any(|k| k.eq_ignore_ascii_case(kind))
        {
            return Err(Error::InvalidToolParams(format!(
                "Invalid kind_filter: '{kind}'. Valid values: {VALID_SYMBOL_KINDS:?}"
            )));
        }

        // Workspace search has no document, so it resolves via `resolve_any`
        // rather than a per-language route. If the resolved server is not
        // registered yet but is expected, tell the caller to wait and retry
        // rather than implying nothing is configured.
        let server_id = lock_std(&self.router)
            .resolve_any(ToolKind::WorkspaceSymbols)
            .cloned()
            .ok_or(Error::NoServerConfigured)?;
        self.respawn_if_dead(&server_id).await?;
        let client = lock_std(&self.lsp_clients).get(&server_id).cloned();
        let client = client.ok_or_else(|| {
            if lock_std(&self.expected_servers).contains(&server_id) {
                Error::ServerInitializing {
                    server_id: server_id.clone(),
                }
            } else {
                Error::NoServerConfigured
            }
        })?;
        self.require_capability(&server_id, "workspaceSymbolProvider", |caps| {
            matches!(
                caps.workspace_symbol_provider,
                Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
            )
        })?;

        let params = LspWorkspaceSymbolParams {
            query,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<lsp_types::SymbolInformation>> = client
            .request("workspace/symbol", params, timeout_duration)
            .await?;

        let mut symbols: Vec<WorkspaceSymbol> = response
            .unwrap_or_default()
            .into_iter()
            .map(|sym| WorkspaceSymbol {
                name: sym.name,
                kind: format!("{:?}", sym.kind),
                location: Location {
                    uri: sym.location.uri.to_string(),
                    range: normalize_range(sym.location.range),
                },
                container_name: sym.container_name,
            })
            .collect();

        // Apply kind filter if specified
        if let Some(kind) = kind_filter {
            symbols.retain(|s| s.kind.eq_ignore_ascii_case(&kind));
        }

        // Limit results
        symbols.truncate(limit as usize);

        Ok(WorkspaceSymbolResult { symbols })
    }

    /// Handle code actions request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `codeActionProvider` support.
    pub async fn handle_code_actions(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
        kind_filter: Option<String>,
    ) -> Result<CodeActionsResult> {
        validate_code_action_params(
            start_line,
            start_character,
            end_line,
            end_character,
            kind_filter.as_deref(),
        )?;

        let (_server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::CodeActions,
                "codeActionProvider",
                |caps| {
                    matches!(
                        caps.code_action_provider,
                        Some(
                            lsp_types::CodeActionProviderCapability::Simple(true)
                                | lsp_types::CodeActionProviderCapability::Options(_)
                        )
                    )
                },
            )
            .await?;

        let range = lsp_types::Range {
            start: mcp_to_lsp_position(start_line, start_character),
            end: mcp_to_lsp_position(end_line, end_character),
        };

        // Build context with optional kind filter
        let only = kind_filter.map(|k| vec![lsp_types::CodeActionKind::from(k)]);

        // Pass empty diagnostics context — rust-analyzer generates code actions
        // based on cursor position and its internal analysis state, not on the
        // passed diagnostics.  Passing stale cached diagnostics (which may lack
        // the internal `data` field ra uses for fix mapping) suppresses results.
        let context_diagnostics: Vec<lsp_types::Diagnostic> = vec![];

        let params = lsp_types::CodeActionParams {
            text_document: TextDocumentIdentifier { uri },
            range,
            context: lsp_types::CodeActionContext {
                diagnostics: context_diagnostics,
                only,
                trigger_kind: Some(lsp_types::CodeActionTriggerKind::INVOKED),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::CodeActionResponse> = client
            .request("textDocument/codeAction", params, timeout_duration)
            .await?;
        let response_vec = response.unwrap_or_default();
        let mut actions = Vec::with_capacity(response_vec.len());

        for action_or_command in response_vec {
            let action = match action_or_command {
                lsp_types::CodeActionOrCommand::CodeAction(action) => convert_code_action(action),
                lsp_types::CodeActionOrCommand::Command(cmd) => {
                    let arguments = cmd.arguments.unwrap_or_else(Vec::new);
                    CodeAction {
                        title: cmd.title.clone(),
                        kind: None,
                        diagnostics: Vec::new(),
                        edit: None,
                        command: Some(CommandDescription {
                            title: cmd.title,
                            command: cmd.command,
                            arguments,
                        }),
                        is_preferred: false,
                    }
                }
            };
            actions.push(action);
        }

        Ok(CodeActionsResult { actions })
    }

    /// Handle call hierarchy prepare request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `callHierarchyProvider` support.
    pub async fn handle_call_hierarchy_prepare(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<CallHierarchyPrepareResult> {
        // Validate position bounds
        if line < 1 || character < 1 {
            return Err(Error::InvalidToolParams(
                "Line and character positions must be >= 1".to_string(),
            ));
        }

        if line > MAX_POSITION_VALUE || character > MAX_POSITION_VALUE {
            return Err(Error::InvalidToolParams(format!(
                "Position values must be <= {MAX_POSITION_VALUE}"
            )));
        }

        let (_server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::CallHierarchy,
                "callHierarchyProvider",
                call_hierarchy_provider_supported,
            )
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = LspCallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<CallHierarchyItem>> = client
            .request(
                "textDocument/prepareCallHierarchy",
                params,
                timeout_duration,
            )
            .await?;

        // Pre-allocate and build result
        let lsp_items = response.unwrap_or_default();
        let mut items = Vec::with_capacity(lsp_items.len());
        for item in lsp_items {
            items.push(convert_call_hierarchy_item(item));
        }

        Ok(CallHierarchyPrepareResult { items })
    }

    /// Handle incoming calls request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the item is invalid, or the
    /// routed server does not advertise `callHierarchyProvider` support.
    pub async fn handle_incoming_calls(
        &self,
        item: serde_json::Value,
    ) -> Result<IncomingCallsResult> {
        // Deserialize as our own type (1-based coords) then convert to LSP (0-based).
        let lsp_item = mcp_item_to_lsp(item)?;

        // Parse and validate the URI. Resolved with the same ToolKind as
        // `handle_call_hierarchy_prepare` -- the opaque item this call
        // receives is only meaningful to the server that produced it, and
        // that server is guaranteed to be the same one `prepare` synced the
        // document to since both resolve via the same (language, tool) route.
        let path = self.parse_file_uri(&lsp_item.uri)?;
        let (server_id, client) = self
            .resolve_client_for_file(&path, ToolKind::CallHierarchy)
            .await?;
        self.require_capability(
            &server_id,
            "callHierarchyProvider",
            call_hierarchy_provider_supported,
        )?;

        let params = CallHierarchyIncomingCallsParams {
            item: lsp_item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<CallHierarchyIncomingCall>> = client
            .request("callHierarchy/incomingCalls", params, timeout_duration)
            .await?;

        // Pre-allocate and build result
        let lsp_calls = response.unwrap_or_default();
        let mut calls = Vec::with_capacity(lsp_calls.len());

        for call in lsp_calls {
            let from_ranges = {
                let mut ranges = Vec::with_capacity(call.from_ranges.len());
                for range in call.from_ranges {
                    ranges.push(normalize_range(range));
                }
                ranges
            };

            calls.push(IncomingCall {
                from: convert_call_hierarchy_item(call.from),
                from_ranges,
            });
        }

        Ok(IncomingCallsResult { calls })
    }

    /// Handle outgoing calls request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the item is invalid, or the
    /// routed server does not advertise `callHierarchyProvider` support.
    pub async fn handle_outgoing_calls(
        &self,
        item: serde_json::Value,
    ) -> Result<OutgoingCallsResult> {
        // Deserialize as our own type (1-based coords) then convert to LSP (0-based).
        let lsp_item = mcp_item_to_lsp(item)?;

        // Parse and validate the URI. Same ToolKind/route as `prepare` and
        // `handle_incoming_calls` -- see that function's comment.
        let path = self.parse_file_uri(&lsp_item.uri)?;
        let (server_id, client) = self
            .resolve_client_for_file(&path, ToolKind::CallHierarchy)
            .await?;
        self.require_capability(
            &server_id,
            "callHierarchyProvider",
            call_hierarchy_provider_supported,
        )?;

        let params = CallHierarchyOutgoingCallsParams {
            item: lsp_item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<CallHierarchyOutgoingCall>> = client
            .request("callHierarchy/outgoingCalls", params, timeout_duration)
            .await?;

        // Pre-allocate and build result
        let lsp_calls = response.unwrap_or_default();
        let mut calls = Vec::with_capacity(lsp_calls.len());

        for call in lsp_calls {
            let from_ranges = {
                let mut ranges = Vec::with_capacity(call.from_ranges.len());
                for range in call.from_ranges {
                    ranges.push(normalize_range(range));
                }
                ranges
            };

            calls.push(OutgoingCall {
                to: convert_call_hierarchy_item(call.to),
                from_ranges,
            });
        }

        Ok(OutgoingCallsResult { calls })
    }

    /// Resolve the LSP-side cache key (URI string) for a cached-diagnostics lookup.
    ///
    /// Split out from the cache read itself so callers (e.g. the
    /// `get_cached_diagnostics` MCP tool) can do the path `canonicalize()` and
    /// workspace-boundary check *before* taking the `NotificationCache` lock —
    /// that lock is also needed by `diagnostics_pump` to store incoming
    /// notifications, so nothing that isn't a plain map lookup should run
    /// while it's held.
    ///
    /// # Errors
    ///
    /// Returns an error if the path is invalid or outside workspace boundaries.
    pub fn cached_diagnostics_uri(workspace_roots: &[PathBuf], file_path: &str) -> Result<String> {
        let path = PathBuf::from(file_path);
        let validated_path = validate_path_against_roots(&path, workspace_roots)?;

        // Use path_to_uri (strips \\?\ on Windows) so the key matches what
        // rust-analyzer stores in publishDiagnostics notifications.
        Ok(path_to_uri(&validated_path).to_string())
    }

    /// Convert a cached diagnostics entry into the MCP-facing result shape.
    ///
    /// Takes an already-cloned `Option<&DiagnosticInfo>` (out of the
    /// `NotificationCache` lock) rather than the cache itself, so this
    /// mapping — which is not a bounded operation for a large diagnostics set
    /// — never runs while the cache is locked.
    #[must_use]
    pub fn diagnostics_from_cache_entry(diag_info: Option<&DiagnosticInfo>) -> DiagnosticsResult {
        let diagnostics = diag_info.map_or_else(Vec::new, |diag_info| {
            diag_info
                .diagnostics
                .iter()
                .map(diagnostic_to_mcp)
                .collect()
        });

        DiagnosticsResult { diagnostics }
    }

    /// Merge push-model diagnostics from the notification cache into a
    /// pull-model (`textDocument/diagnostic`) result.
    ///
    /// rust-analyzer's pull endpoint omits diagnostics that are only ever
    /// delivered via `textDocument/publishDiagnostics` push notifications —
    /// not just flycheck/clippy lints, but empirically (verified against a
    /// live rust-analyzer 1.97.1 session, see #244) some native diagnostics
    /// too. Those are cached separately in `NotificationCache`.
    ///
    /// Where the *same* logical problem is reported through both paths, the
    /// two representations were observed to differ in both `range` and
    /// rendered `message`. Captured example, a "not all trait items
    /// implemented" (E0046) error for one `impl` block: pull reported range
    /// `(96,7)-(96,12)` (the trait name) with message "not all trait items
    /// implemented, missing: `fn hello`"; the push notification for the same
    /// error reported range `(95,1)-(95,32)` (the impl block) with message
    /// "not all trait items implemented, missing: `hello`\nmissing `hello`
    /// in implementation" — same `code`/`severity`, adjacent but distinct
    /// ranges, different message text. Exact field equality never dedups
    /// cases like that.
    ///
    /// Given that, a cache entry is treated as a duplicate of a pull entry
    /// when both carry a `code`, the `(severity, code)` pair matches, *and*
    /// the two ranges are either overlapping or start within
    /// `DUPLICATE_RANGE_PROXIMITY_LINES` lines of each other — close
    /// enough to be the same underlying model divergence, not two distinct
    /// occurrences of the same error class (e.g. two unrelated `E0308`
    /// mismatches at different call sites in one file, one caught only
    /// natively and one only by flycheck). Diagnostics with no `code` fall
    /// back to full-field equality, since there is no cheaper stable
    /// identity available for them.
    ///
    /// Output is sorted by `(start.line, start.character)` so merged
    /// cache-only entries don't land out of document order after the
    /// pull-model ones.
    #[must_use]
    pub fn merge_diagnostics(
        mut pull: DiagnosticsResult,
        diag_info: Option<&DiagnosticInfo>,
    ) -> DiagnosticsResult {
        /// Start-line distance within which same-code, same-severity
        /// diagnostics from the two models are still considered the same
        /// underlying problem. Derived from the captured E0046 case above
        /// (1 line apart); wide enough to absorb span drift between
        /// rust-analyzer's own spans and rustc's, narrow enough that two
        /// genuinely distinct same-code errors elsewhere in a file are not
        /// collapsed into one.
        const DUPLICATE_RANGE_PROXIMITY_LINES: u32 = 3;

        fn position_le(a: &Position2D, b: &Position2D) -> bool {
            (a.line, a.character) <= (b.line, b.character)
        }

        fn ranges_close(a: &Range, b: &Range) -> bool {
            let overlaps = position_le(&a.start, &b.end) && position_le(&b.start, &a.end);
            overlaps || a.start.line.abs_diff(b.start.line) <= DUPLICATE_RANGE_PROXIMITY_LINES
        }

        fn is_duplicate(pull: &[Diagnostic], candidate: &Diagnostic) -> bool {
            pull.iter().any(|p| match (&candidate.code, &p.code) {
                (Some(c), Some(pc)) if c == pc && p.severity == candidate.severity => {
                    ranges_close(&p.range, &candidate.range)
                }
                _ => p == candidate,
            })
        }

        let cached = Self::diagnostics_from_cache_entry(diag_info).diagnostics;
        let new_diagnostics: Vec<_> = cached
            .into_iter()
            .filter(|c| !is_duplicate(&pull.diagnostics, c))
            .collect();
        pull.diagnostics.extend(new_diagnostics);
        pull.diagnostics
            .sort_by_key(|d| (d.range.start.line, d.range.start.character));
        pull
    }

    /// Handle server logs request.
    ///
    /// # Errors
    ///
    /// Returns an error if the `min_level` parameter is invalid.
    pub fn handle_server_logs(
        cache: &NotificationCache,
        limit: usize,
        min_level: Option<String>,
    ) -> Result<ServerLogsResult> {
        use crate::bridge::notifications::LogLevel;

        let min_level_filter = if let Some(level_str) = min_level {
            let level = match level_str.to_lowercase().as_str() {
                "error" => LogLevel::Error,
                "warning" => LogLevel::Warning,
                "info" => LogLevel::Info,
                "debug" => LogLevel::Debug,
                _ => {
                    return Err(Error::InvalidToolParams(format!(
                        "Invalid min_level: '{level_str}'. Valid values: error, warning, info, debug"
                    )));
                }
            };
            Some(level)
        } else {
            None
        };

        let all_logs = cache.get_logs();

        let logs: Vec<_> = all_logs
            .iter()
            .filter(|log| {
                min_level_filter.is_none_or(|min| match min {
                    LogLevel::Error => matches!(log.level, LogLevel::Error),
                    LogLevel::Warning => matches!(log.level, LogLevel::Error | LogLevel::Warning),
                    LogLevel::Info => !matches!(log.level, LogLevel::Debug),
                    LogLevel::Debug => true,
                })
            })
            .take(limit)
            .cloned()
            .collect();

        Ok(ServerLogsResult { logs })
    }

    /// Handle server messages request.
    ///
    /// # Errors
    ///
    /// This method does not return errors.
    pub fn handle_server_messages(
        cache: &NotificationCache,
        limit: usize,
    ) -> Result<ServerMessagesResult> {
        let all_messages = cache.get_messages();
        let messages: Vec<_> = all_messages.iter().take(limit).cloned().collect();
        Ok(ServerMessagesResult { messages })
    }

    /// Handle signature help request (`textDocument/signatureHelp`).
    ///
    /// Returns parameter signatures and documentation while typing a function call.
    /// `context` is omitted (None) — the server infers trigger state from position.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `signatureHelpProvider` support.
    pub async fn handle_signature_help(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<SignatureHelpResult> {
        let (_server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::SignatureHelp,
                "signatureHelpProvider",
                |caps| caps.signature_help_provider.is_some(),
            )
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = LspSignatureHelpParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            context: None,
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::SignatureHelp> = client
            .request("textDocument/signatureHelp", params, timeout_duration)
            .await?;

        let result = match response {
            Some(sig_help) => SignatureHelpResult {
                signatures: sig_help
                    .signatures
                    .into_iter()
                    .map(|sig| SignatureInfo {
                        label: sig.label,
                        documentation: sig.documentation.map(extract_documentation),
                        parameters: sig
                            .parameters
                            .unwrap_or_default()
                            .into_iter()
                            .map(|p| SignatureParameter {
                                label: match p.label {
                                    lsp_types::ParameterLabel::Simple(s) => s,
                                    lsp_types::ParameterLabel::LabelOffsets([start, end]) => {
                                        format!("[{start},{end}]")
                                    }
                                },
                                documentation: p.documentation.map(extract_documentation),
                            })
                            .collect(),
                    })
                    .collect(),
                active_signature: sig_help.active_signature,
                active_parameter: sig_help.active_parameter,
            },
            None => SignatureHelpResult {
                signatures: vec![],
                active_signature: None,
                active_parameter: None,
            },
        };

        Ok(result)
    }

    /// Handle go-to-implementation request (`textDocument/implementation`).
    ///
    /// Returns the locations of trait method or interface member implementations.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `implementationProvider` support.
    pub async fn handle_implementation(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult> {
        let (_server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::Implementation,
                "implementationProvider",
                |caps| {
                    matches!(
                        caps.implementation_provider,
                        Some(
                            lsp_types::ImplementationProviderCapability::Simple(true)
                                | lsp_types::ImplementationProviderCapability::Options(_)
                        )
                    )
                },
            )
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::GotoDefinitionResponse> = client
            .request("textDocument/implementation", params, timeout_duration)
            .await?;

        Ok(LocationsResult {
            locations: goto_response_to_locations(response),
        })
    }

    /// Handle go-to-type-definition request (`textDocument/typeDefinition`).
    ///
    /// Returns the type definition location of the expression at position. Distinct
    /// from go-to-definition for variable bindings where definition and type differ.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `typeDefinitionProvider` support.
    pub async fn handle_type_definition(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<LocationsResult> {
        let (_server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::TypeDefinition,
                "typeDefinitionProvider",
                |caps| {
                    matches!(
                        caps.type_definition_provider,
                        Some(
                            lsp_types::TypeDefinitionProviderCapability::Simple(true)
                                | lsp_types::TypeDefinitionProviderCapability::Options(_)
                        )
                    )
                },
            )
            .await?;
        let lsp_position = mcp_to_lsp_position(line, character);

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<lsp_types::GotoDefinitionResponse> = client
            .request("textDocument/typeDefinition", params, timeout_duration)
            .await?;

        Ok(LocationsResult {
            locations: goto_response_to_locations(response),
        })
    }

    /// Handle inlay hints request (`textDocument/inlayHint`).
    ///
    /// Returns inferred type and parameter annotations the editor would render inline.
    /// Output positions are in MCP 1-based form.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `inlayHintProvider` support.
    pub async fn handle_inlay_hints(
        &self,
        file_path: String,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
    ) -> Result<InlayHintsResult> {
        use crate::bridge::encoding::lsp_to_mcp_position;

        let (_server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::InlayHints,
                "inlayHintProvider",
                |caps| {
                    matches!(
                        caps.inlay_hint_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;

        let lsp_start = mcp_to_lsp_position(start_line, start_character);
        let lsp_end = mcp_to_lsp_position(end_line, end_character);

        let params = InlayHintParams {
            text_document: TextDocumentIdentifier { uri },
            range: lsp_types::Range {
                start: lsp_start,
                end: lsp_end,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let timeout_duration = Duration::from_secs(30);
        let response: Option<Vec<lsp_types::InlayHint>> = client
            .request("textDocument/inlayHint", params, timeout_duration)
            .await?;

        let hints = response
            .unwrap_or_default()
            .into_iter()
            .map(|hint| {
                let (mcp_line, mcp_character) = lsp_to_mcp_position(hint.position);
                let label = match hint.label {
                    InlayHintLabel::String(s) => s,
                    InlayHintLabel::LabelParts(parts) => parts
                        .into_iter()
                        .map(|p| p.value)
                        .collect::<Vec<_>>()
                        .concat(),
                };
                let tooltip = hint.tooltip.map(|t| match t {
                    lsp_types::InlayHintTooltip::String(s) => s,
                    lsp_types::InlayHintTooltip::MarkupContent(m) => m.value,
                });
                InlayHintEntry {
                    position: Position2D {
                        line: mcp_line,
                        character: mcp_character,
                    },
                    label,
                    kind: hint.kind.and_then(|k| {
                        serde_json::to_value(k)
                            .ok()
                            .and_then(|v| v.as_i64())
                            .and_then(|n| u8::try_from(n).ok())
                    }),
                    padding_left: hint.padding_left,
                    padding_right: hint.padding_right,
                    tooltip,
                }
            })
            .collect();

        Ok(InlayHintsResult { hints })
    }
}

/// Extract hover contents as markdown string.
/// Convert LSP `Documentation` to a plain string.
fn extract_documentation(doc: lsp_types::Documentation) -> String {
    match doc {
        lsp_types::Documentation::String(s) => s,
        lsp_types::Documentation::MarkupContent(m) => m.value,
    }
}

/// Normalize a `GotoDefinitionResponse` into a flat list of MCP `Location` values.
fn goto_response_to_locations(
    response: Option<lsp_types::GotoDefinitionResponse>,
) -> Vec<Location> {
    let lsp_locs: Vec<lsp_types::Location> = match response {
        Some(lsp_types::GotoDefinitionResponse::Scalar(loc)) => vec![loc],
        Some(lsp_types::GotoDefinitionResponse::Array(locs)) => locs,
        Some(lsp_types::GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|link| lsp_types::Location {
                uri: link.target_uri,
                range: link.target_selection_range,
            })
            .collect(),
        None => vec![],
    };

    lsp_locs
        .into_iter()
        .map(|loc| Location {
            uri: loc.uri.to_string(),
            range: normalize_range(loc.range),
        })
        .collect()
}

fn extract_hover_contents(contents: HoverContents) -> String {
    match contents {
        HoverContents::Scalar(marked_string) => marked_string_to_string(marked_string),
        HoverContents::Array(marked_strings) => marked_strings
            .into_iter()
            .map(marked_string_to_string)
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(markup) => markup.value,
    }
}

/// Convert a marked string to a plain string.
fn marked_string_to_string(marked: MarkedString) -> String {
    match marked {
        MarkedString::String(s) => s,
        MarkedString::LanguageString(ls) => format!("```{}\n{}\n```", ls.language, ls.value),
    }
}

/// Convert LSP range to MCP range (0-based to 1-based).
/// Validate parameters for `handle_code_actions`.
fn validate_code_action_params(
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
    kind_filter: Option<&str>,
) -> Result<()> {
    const VALID_ACTION_KINDS: &[&str] = &[
        "quickfix",
        "refactor",
        "refactor.extract",
        "refactor.inline",
        "refactor.rewrite",
        "source",
        "source.organizeImports",
    ];

    if let Some(kind) = kind_filter
        && !VALID_ACTION_KINDS
            .iter()
            .any(|k| k.eq_ignore_ascii_case(kind))
    {
        return Err(Error::InvalidToolParams(format!(
            "Invalid kind_filter: '{kind}'. Valid values: {VALID_ACTION_KINDS:?}"
        )));
    }

    if start_line < 1 || start_character < 1 || end_line < 1 || end_character < 1 {
        return Err(Error::InvalidToolParams(
            "Line and character positions must be >= 1".to_string(),
        ));
    }

    if start_line > MAX_POSITION_VALUE
        || start_character > MAX_POSITION_VALUE
        || end_line > MAX_POSITION_VALUE
        || end_character > MAX_POSITION_VALUE
    {
        return Err(Error::InvalidToolParams(format!(
            "Position values must be <= {MAX_POSITION_VALUE}"
        )));
    }

    if end_line.saturating_sub(start_line) > MAX_RANGE_LINES {
        return Err(Error::InvalidToolParams(format!(
            "Range size must be <= {MAX_RANGE_LINES} lines"
        )));
    }

    if start_line > end_line || (start_line == end_line && start_character > end_character) {
        return Err(Error::InvalidToolParams(
            "Start position must be before or equal to end position".to_string(),
        ));
    }

    Ok(())
}

/// Whether a server's capabilities advertise `callHierarchyProvider` support.
///
/// Shared by `handle_call_hierarchy_prepare`, `handle_incoming_calls`, and
/// `handle_outgoing_calls`, which all gate on the same capability field.
const fn call_hierarchy_provider_supported(caps: &lsp_types::ServerCapabilities) -> bool {
    matches!(
        caps.call_hierarchy_provider,
        Some(
            lsp_types::CallHierarchyServerCapability::Simple(true)
                | lsp_types::CallHierarchyServerCapability::Options(_)
        )
    )
}

/// Convert a `CallHierarchyItemResult` JSON (1-based MCP coordinates) into
/// a `lsp_types::CallHierarchyItem` (0-based LSP coordinates).
///
/// MCP clients receive `CallHierarchyItemResult` from `prepare_call_hierarchy`
/// and pass it back opaquely to `get_incoming_calls` / `get_outgoing_calls`.
/// The bridge serialises ranges as 1-based; this function inverts that mapping
/// before forwarding the item to the LSP server.
fn mcp_item_to_lsp(item: serde_json::Value) -> Result<CallHierarchyItem> {
    let mcp: CallHierarchyItemResult = serde_json::from_value(item)
        .map_err(|e| Error::InvalidToolParams(format!("Invalid call hierarchy item: {e}")))?;

    let uri = mcp.uri.parse::<lsp_types::Uri>().map_err(|e| {
        Error::InvalidToolParams(format!("Invalid URI in call hierarchy item: {e}"))
    })?;

    let detail = mcp.detail;
    let data = mcp.data;

    // Round-trip via serde: `convert_call_hierarchy_item` stored the kind as a u32
    // by serialising `SymbolKind`; we reverse this to reconstruct the same value.
    let kind: lsp_types::SymbolKind = serde_json::from_value(serde_json::json!(mcp.kind))
        .unwrap_or(lsp_types::SymbolKind::FUNCTION);

    Ok(CallHierarchyItem {
        name: mcp.name,
        kind,
        tags: None,
        detail,
        uri,
        range: denormalize_range(&mcp.range),
        selection_range: denormalize_range(&mcp.selection_range),
        data,
    })
}

/// Convert a 1-based MCP range back to a 0-based LSP range.
///
/// Used when MCP clients pass back a `CallHierarchyItemResult` that was
/// previously returned by `prepare_call_hierarchy` (which stores 1-based coords).
const fn denormalize_range(range: &Range) -> lsp_types::Range {
    lsp_types::Range {
        start: lsp_types::Position {
            line: range.start.line.saturating_sub(1),
            character: range.start.character.saturating_sub(1),
        },
        end: lsp_types::Position {
            line: range.end.line.saturating_sub(1),
            character: range.end.character.saturating_sub(1),
        },
    }
}

const fn normalize_range(range: lsp_types::Range) -> Range {
    Range {
        start: Position2D {
            line: range.start.line + 1,
            character: range.start.character + 1,
        },
        end: Position2D {
            line: range.end.line + 1,
            character: range.end.character + 1,
        },
    }
}

/// Convert an LSP diagnostic into the MCP-facing `Diagnostic` shape.
///
/// Shared by both the pull-model (`handle_diagnostics`) and cache-derived
/// (`diagnostics_from_cache_entry`) diagnostic paths, so their output never
/// diverges in formatting — `merge_diagnostics`'s dedup logic depends on
/// both sides mapping severity/code identically.
fn diagnostic_to_mcp(diag: &lsp_types::Diagnostic) -> Diagnostic {
    Diagnostic {
        range: normalize_range(diag.range),
        severity: match diag.severity {
            Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
            Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
            Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
            // INFORMATION and None (no severity reported) both fall here.
            _ => DiagnosticSeverity::Information,
        },
        message: diag.message.clone(),
        code: diag.code.as_ref().map(|c| match c {
            lsp_types::NumberOrString::Number(n) => n.to_string(),
            lsp_types::NumberOrString::String(s) => s.clone(),
        }),
    }
}

/// Convert LSP document symbol to MCP symbol.
fn convert_document_symbol(symbol: DocumentSymbol) -> Symbol {
    Symbol {
        name: symbol.name,
        kind: format!("{:?}", symbol.kind),
        range: normalize_range(symbol.range),
        selection_range: normalize_range(symbol.selection_range),
        children: symbol
            .children
            .map(|children| children.into_iter().map(convert_document_symbol).collect()),
    }
}

/// Convert LSP call hierarchy item to MCP call hierarchy item.
fn convert_call_hierarchy_item(item: CallHierarchyItem) -> CallHierarchyItemResult {
    CallHierarchyItemResult {
        name: item.name,
        kind: serde_json::to_value(item.kind)
            .ok()
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
        detail: item.detail,
        uri: item.uri.to_string(),
        range: normalize_range(item.range),
        selection_range: normalize_range(item.selection_range),
        data: item.data,
    }
}

/// Convert LSP code action to MCP code action.
fn convert_code_action(action: lsp_types::CodeAction) -> CodeAction {
    let diagnostics = action.diagnostics.map_or_else(Vec::new, |diags| {
        let mut result = Vec::with_capacity(diags.len());
        for d in diags {
            result.push(Diagnostic {
                range: normalize_range(d.range),
                severity: match d.severity {
                    Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
                    Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
                    Some(lsp_types::DiagnosticSeverity::INFORMATION) => {
                        DiagnosticSeverity::Information
                    }
                    Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
                    _ => DiagnosticSeverity::Information,
                },
                message: d.message,
                code: d.code.map(|c| match c {
                    lsp_types::NumberOrString::Number(n) => n.to_string(),
                    lsp_types::NumberOrString::String(s) => s,
                }),
            });
        }
        result
    });

    let edit = action.edit.map(|edit| {
        let changes = edit.changes.map_or_else(Vec::new, |changes_map| {
            let mut result = Vec::with_capacity(changes_map.len());
            for (uri, edits) in changes_map {
                let mut text_edits = Vec::with_capacity(edits.len());
                for e in edits {
                    text_edits.push(TextEdit {
                        range: normalize_range(e.range),
                        new_text: e.new_text,
                    });
                }
                result.push(DocumentChanges {
                    uri: uri.to_string(),
                    edits: text_edits,
                });
            }
            result
        });
        WorkspaceEditDescription { changes }
    });

    let command = action.command.map(|cmd| {
        let arguments = cmd.arguments.unwrap_or_else(Vec::new);
        CommandDescription {
            title: cmd.title,
            command: cmd.command,
            arguments,
        }
    });

    CodeAction {
        title: action.title,
        kind: action.kind.map(|k| k.as_str().to_string()),
        diagnostics,
        edit,
        command,
        is_preferred: action.is_preferred.unwrap_or(false),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use url::Url;

    use super::*;

    #[test]
    fn test_translator_new() {
        let translator = Translator::new();
        assert_eq!(translator.workspace_roots.len(), 0);
        assert_eq!(lock_std(&translator.lsp_clients).len(), 0);
        assert_eq!(lock_std(&translator.lsp_servers).len(), 0);
    }

    #[test]
    fn test_set_workspace_roots() {
        let mut translator = Translator::new();
        let roots = vec![PathBuf::from("/test/root1"), PathBuf::from("/test/root2")];
        translator.set_workspace_roots(roots.clone());
        assert_eq!(*translator.workspace_roots, roots);
    }

    #[test]
    fn test_register_server() {
        let translator = Translator::new();

        // Initial state: no servers registered
        assert_eq!(lock_std(&translator.lsp_servers).len(), 0);

        // The register_server method exists and is callable
        // Full integration testing with real LspServer is done in integration tests
        // This unit test verifies the method signature and basic functionality

        // Note: We can't easily construct an LspServer in a unit test without async
        // and a real LSP server process. The actual registration functionality is
        // tested in integration tests (see rust_analyzer_tests.rs).
        // This test verifies the data structure is properly initialized.
    }

    #[test]
    fn test_get_client_for_file_server_initializing_when_expected() {
        // A configured/applicable language whose LSP client has not registered
        // yet (large solution still loading via OmniSharp) must surface
        // ServerInitializing — "wait and retry" — not NoServerForLanguage.
        let path = PathBuf::from("/ws/Assets/Scripts/Player.cs");
        let lang = detect_language(&path, &HashMap::new());
        let id = ServerId::from(lang.clone());

        let translator = Translator::new().with_router(ToolRouter::catch_all([(id.clone(), lang)]));
        let mut expected = HashSet::new();
        expected.insert(id.clone());
        translator.set_expected_servers(expected);

        let err = translator
            .get_client_for_file(&path, ToolKind::Hover)
            .unwrap_err();
        assert!(matches!(err, Error::ServerInitializing { server_id } if server_id == id));
    }

    #[test]
    fn test_get_client_for_file_no_server_when_not_expected() {
        // When no route is configured for the language at all, the error
        // stays NoServerForLanguage.
        let translator = Translator::new();
        let path = PathBuf::from("/ws/Assets/Scripts/Player.cs");
        let lang = detect_language(&path, &translator.extension_map);

        let err = translator
            .get_client_for_file(&path, ToolKind::Hover)
            .unwrap_err();
        assert!(matches!(err, Error::NoServerForLanguage(ref l) if *l == lang));
    }

    #[test]
    fn test_clear_expected_servers_reverts_to_no_server_after_all_routes_dropped() {
        // Mirrors the real `serve_with` flow: `rebind_router` (called from
        // `register_servers`/the all-failed path) drops routes to servers
        // that never registered, then `clear_expected_servers` runs under
        // the same lock. Subsequent lookups must fall back to
        // NoServerForLanguage rather than keep implying the server is still
        // on its way.
        let path = PathBuf::from("/ws/Assets/Scripts/Player.cs");
        let lang = detect_language(&path, &HashMap::new());
        let id = ServerId::from(lang.clone());

        let translator = Translator::new().with_router(ToolRouter::catch_all([(id.clone(), lang)]));
        let mut expected = HashSet::new();
        expected.insert(id);
        translator.set_expected_servers(expected);

        translator.rebind_router(&HashSet::new());
        translator.clear_expected_servers();

        let err = translator
            .get_client_for_file(&path, ToolKind::Hover)
            .unwrap_err();
        assert!(matches!(err, Error::NoServerForLanguage(_)));
    }

    // ------------------------------------------------------------------
    // #249: dead-server detection and respawn
    // ------------------------------------------------------------------

    // These three are pure logic (no process spawning), so they run on
    // every platform rather than being swept under `respawn_tests`'s
    // `#[cfg(unix)]` gate below -- otherwise Windows CI would have zero
    // #249 coverage at all.
    #[test]
    fn test_respawn_lock_is_shared_across_lookups_for_same_id() {
        let translator = Translator::new();
        let id = ServerId::from("rust");

        let first = translator.respawn_lock(&id);
        let second = translator.respawn_lock(&id);

        assert!(
            Arc::ptr_eq(&first, &second),
            "two lookups for the same id must return the same underlying lock, \
             otherwise concurrent respawns would not actually be serialized"
        );
    }

    #[test]
    fn test_respawn_lock_differs_across_ids() {
        let translator = Translator::new();

        let rust_lock = translator.respawn_lock(&ServerId::from("rust"));
        let python_lock = translator.respawn_lock(&ServerId::from("python"));

        assert!(!Arc::ptr_eq(&rust_lock, &python_lock));
    }

    #[test]
    fn test_is_server_dead_false_when_not_registered() {
        let translator = Translator::new();
        assert!(!translator.is_server_dead(&ServerId::from("rust")));
    }

    // Gated `#[cfg(unix)]`: this module's fake-LSP-server test double is a
    // hand-written `sh` script (POSIX parameter expansion, `printf`-framed
    // LSP responses, file-based invocation counters), which has no
    // equivalent on Windows. CI's "Test (unit)" job matrix includes
    // `windows-latest`.
    #[cfg(unix)]
    mod respawn_tests {
        use std::path::Path;

        use tokio::time::Duration;

        use super::*;
        use crate::config::LspServerConfig;

        /// Writes a `sh` script that answers the LSP `initialize` handshake
        /// with a canned response -- request id `1`, since a freshly spawned
        /// `LspClient`'s request counter always starts there -- and then
        /// exits shortly after, so `LspServer::spawn` succeeds but the
        /// process is already dead moments later. Stands in for "the server
        /// was alive, then crashed" without needing a real language server
        /// binary.
        ///
        /// The brief sleep before exiting matters: `LspServer::spawn` sends
        /// the `initialized` notification right after the `initialize`
        /// response arrives, and without it the process can (racily) have
        /// already exited by the time that notification is written to its
        /// stdin, failing the spawn itself instead of the respawn this is
        /// meant to seed.
        fn write_crash_after_init_script(dir: &Path) -> PathBuf {
            let script_path = dir.join("crash_after_init.sh");
            let body = r#"body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
sleep 0.3
"#;
            fs::write(&script_path, body).unwrap();
            script_path
        }

        /// Like [`write_crash_after_init_script`], but stays alive for
        /// `sleep_secs` after responding instead of exiting immediately.
        fn write_responder_script(dir: &Path, sleep_secs: u64) -> PathBuf {
            let script_path = dir.join("responder.sh");
            let template = r#"body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
sleep __SLEEP__
"#;
            fs::write(
                &script_path,
                template.replace("__SLEEP__", &sleep_secs.to_string()),
            )
            .unwrap();
            script_path
        }

        fn stub_server_config(id: &str, script: &Path) -> ServerInitConfig {
            ServerInitConfig {
                server_config: LspServerConfig {
                    language_id: id.to_string(),
                    command: "sh".to_string(),
                    args: vec![script.to_string_lossy().to_string()],
                    env: HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 5,
                    heuristics: None,
                    name: Some(id.to_string()),
                    handles: None,
                },
                workspace_roots: vec![],
                initialization_options: None,
                notification_tx: None,
            }
        }

        /// Polls `is_server_dead` until it reports `true`, bounding the wait
        /// so a broken script fails the test instead of hanging it.
        async fn wait_until_dead(translator: &Translator, id: &ServerId) {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if translator.is_server_dead(id) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("seed server never reported as exited");
        }

        #[tokio::test]
        async fn test_respawn_if_dead_noop_when_server_alive() {
            let dir = TempDir::new().unwrap();
            let script = write_responder_script(dir.path(), 1);
            let id = ServerId::from("rust");
            let config = stub_server_config("rust", &script);

            let server = LspServer::spawn(config).await.unwrap();
            let translator = Translator::new();
            translator.register_client(id.clone(), server.client().clone());
            translator.register_server(id.clone(), server);
            // Deliberately no `register_server_config`: if a respawn were
            // (wrongly) attempted despite the server being alive, the
            // missing config would surface as `Error::ServerUnavailable`
            // instead of quietly succeeding -- so `Ok(())` here is proof
            // the alive fast path skipped respawning entirely.

            assert!(translator.respawn_if_dead(&id).await.is_ok());
        }

        #[tokio::test]
        async fn test_respawn_if_dead_errors_when_no_config_registered() {
            let dir = TempDir::new().unwrap();
            let script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let config = stub_server_config("rust", &script);

            let server = LspServer::spawn(config).await.unwrap();
            let translator = Translator::new();
            translator.register_client(id.clone(), server.client().clone());
            translator.register_server(id.clone(), server);
            wait_until_dead(&translator, &id).await;

            let err = translator.respawn_if_dead(&id).await.unwrap_err();
            assert!(
                matches!(err, Error::ServerUnavailable { .. }),
                "got {err:?}"
            );
        }

        #[tokio::test]
        async fn test_respawn_if_dead_propagates_spawn_failure() {
            let dir = TempDir::new().unwrap();
            let script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &script);

            let server = LspServer::spawn(seed_config).await.unwrap();
            let translator = Translator::new();
            translator.register_client(id.clone(), server.client().clone());
            translator.register_server(id.clone(), server);
            wait_until_dead(&translator, &id).await;

            let mut broken = stub_server_config("rust", &script);
            broken.server_config.command = "nonexistent-lsp-cmd-xyz".to_string();
            translator.register_server_config(id.clone(), broken);

            let err = translator.respawn_if_dead(&id).await.unwrap_err();
            assert!(
                matches!(err, Error::ServerSpawnFailed { .. }),
                "got {err:?}"
            );
        }

        /// #249: two concurrent tool calls that both observe the same dead
        /// server must not each perform their own respawn -- only one
        /// replacement process should ever be spawned, and both callers
        /// must still resolve successfully.
        ///
        /// The fake server script counts every invocation and, on its
        /// first run only, exits right after answering `initialize`
        /// (simulating "was alive, then crashed"); every later invocation
        /// answers and then sleeps, standing in for a healthy replacement.
        /// If single-flighting were broken, both concurrent callers would
        /// spawn their own replacement and the invocation count would be
        /// 3 (seed + two independent respawns) instead of 2 (seed + one
        /// shared respawn).
        #[tokio::test]
        async fn test_respawn_if_dead_single_flights_concurrent_callers() {
            let dir = TempDir::new().unwrap();
            let marker = dir.path().join("marker");
            let counter = dir.path().join("invocations");
            let script_path = dir.path().join("flaky.sh");
            let template = r#"echo x >> "__COUNTER__"
if [ -f "__MARKER__" ]; then
  body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
  printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
  sleep 1
else
  touch "__MARKER__"
  body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
  printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
  sleep 0.3
fi
"#;
            let script_body = template
                .replace("__COUNTER__", &counter.display().to_string())
                .replace("__MARKER__", &marker.display().to_string());
            fs::write(&script_path, script_body).unwrap();

            let id = ServerId::from("rust");
            let config = stub_server_config("rust", &script_path);

            let seed = LspServer::spawn(config.clone()).await.unwrap();
            let translator = Arc::new(Translator::new());
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            translator.register_server_config(id.clone(), config);
            wait_until_dead(&translator, &id).await;

            let (t1, id1) = (Arc::clone(&translator), id.clone());
            let (t2, id2) = (Arc::clone(&translator), id.clone());
            let (r1, r2) = tokio::join!(
                tokio::spawn(async move { t1.respawn_if_dead(&id1).await }),
                tokio::spawn(async move { t2.respawn_if_dead(&id2).await }),
            );
            assert!(r1.unwrap().is_ok());
            assert!(r2.unwrap().is_ok());

            let invocations = fs::read_to_string(&counter).unwrap();
            assert_eq!(
                invocations.lines().count(),
                2,
                "expected exactly one seed spawn + one single-flighted \
                 respawn, got:\n{invocations}"
            );
        }

        /// #249 S2 regression: a second `respawn_if_dead` call within the
        /// backoff window must fail fast via `Error::ServerUnavailable`
        /// instead of repeating a real spawn attempt -- proven by the
        /// *kind* of error changing between the two calls, not by timing:
        /// the first call's failure is the genuine `LspServer::spawn` error
        /// (`Error::ServerSpawnFailed`, from a command that does not
        /// exist), and the second, immediately following, is the distinct
        /// backoff error.
        #[tokio::test]
        async fn test_respawn_if_dead_backs_off_after_repeated_failure() {
            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();
            let translator = Translator::new();
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            wait_until_dead(&translator, &id).await;

            let mut broken = stub_server_config("rust", &seed_script);
            broken.server_config.command = "nonexistent-lsp-cmd-xyz".to_string();
            translator.register_server_config(id.clone(), broken);

            let err1 = translator.respawn_if_dead(&id).await.unwrap_err();
            assert!(
                matches!(err1, Error::ServerSpawnFailed { .. }),
                "first attempt should be a real (failed) spawn, got {err1:?}"
            );

            let err2 = translator.respawn_if_dead(&id).await.unwrap_err();
            assert!(
                matches!(err2, Error::ServerUnavailable { .. }),
                "second call within the backoff window must fail fast \
                 without attempting another real spawn, got {err2:?}"
            );
        }

        /// #249 R3 regression: a respawn that *succeeds* (completes
        /// `initialize`) but dies again almost immediately must still
        /// engage backoff -- this is the more realistic crash-loop shape
        /// (start, initialize, then OOM-die a second later) than an
        /// outright spawn failure, and without this fix every such cycle
        /// looked like a fresh, unbacked-off start, spawning one child
        /// process per tool call forever.
        #[tokio::test]
        async fn test_respawn_if_dead_backs_off_after_quick_recrash_following_success() {
            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();
            let translator = Translator::new();
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            wait_until_dead(&translator, &id).await;

            // Reuse the same crash-after-init script as the respawn target:
            // every attempt completes `initialize` successfully, then dies
            // ~0.3s later -- a post-init crash loop, not a spawn failure.
            translator.register_server_config(id.clone(), stub_server_config("rust", &seed_script));

            translator
                .respawn_if_dead(&id)
                .await
                .expect("the replacement completes initialize, so this attempt succeeds");
            wait_until_dead(&translator, &id).await;

            let err = translator.respawn_if_dead(&id).await.unwrap_err();
            assert!(
                matches!(err, Error::ServerUnavailable { .. }),
                "a respawn that dies again within the stability window must \
                 back off instead of being treated as a fresh attempt, got {err:?}"
            );
        }

        /// #249 C1 regression: respawning the *diagnostics-route* server
        /// for a language must invalidate the whole diagnostics cache,
        /// rather than leaving stale entries to be merged into fresh pull
        /// results as if still current -- the crashed process's pump is
        /// gone and will never update or clear them itself.
        ///
        /// Covers the "under-clear" failure mode a scoped-to-synced-URIs
        /// clear has: a real diagnostics-route server (e.g. rust-analyzer)
        /// publishes workspace-wide (`cargo check` results for files never
        /// opened through mcpls), so `never_opened_uri` below stands in for
        /// an entry that must still be cleared despite never having gone
        /// through `ensure_open`.
        #[tokio::test]
        async fn test_respawn_if_dead_clears_diagnostics_cache_when_diagnostics_route() {
            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();

            let cache = Arc::new(Mutex::new(crate::bridge::NotificationCache::new()));
            let translator = Translator::new()
                .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]))
                .with_notification_cache(Arc::clone(&cache));
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);

            let synced_uri: lsp_types::Uri = "file:///workspace/opened.rs".parse().unwrap();
            let never_opened_uri: lsp_types::Uri =
                "file:///workspace/never_opened.rs".parse().unwrap();
            cache
                .lock()
                .await
                .store_diagnostics(&synced_uri, None, vec![]);
            cache
                .lock()
                .await
                .store_diagnostics(&never_opened_uri, None, vec![]);

            wait_until_dead(&translator, &id).await;

            let respawn_script = write_responder_script(dir.path(), 1);
            translator
                .register_server_config(id.clone(), stub_server_config("rust", &respawn_script));

            translator.respawn_if_dead(&id).await.unwrap();

            let guard = cache.lock().await;
            assert!(
                guard.get_diagnostics(synced_uri.as_str()).is_none(),
                "diagnostics attributed to the crashed connection must be \
                 invalidated on respawn, not served as current"
            );
            assert!(
                guard.get_diagnostics(never_opened_uri.as_str()).is_none(),
                "workspace-wide diagnostics for a file mcpls never opened \
                 must also be invalidated, not just synced documents"
            );
            drop(guard);
        }

        /// #249 C1 regression (over-clear direction): respawning a server
        /// that is *not* the diagnostics route for its language must not
        /// touch the cache at all -- otherwise a crashed hover-only server
        /// would wipe out a healthy, still-running diagnostics server's
        /// valid entries for the same files.
        #[tokio::test]
        async fn test_respawn_if_dead_does_not_clear_cache_when_not_diagnostics_route() {
            use crate::config::LspServerConfig;

            let dir = TempDir::new().unwrap();
            let seed_script = write_crash_after_init_script(dir.path());
            let hover_id = ServerId::from("hover-only");
            let hover_seed_config = stub_server_config("hover-only", &seed_script);

            let seed = LspServer::spawn(hover_seed_config).await.unwrap();

            // `hover_id` handles only Hover; a separate (never-registered
            // here, purely routing-table) server is the catch-all and thus
            // the diagnostics route.
            let configs = [
                LspServerConfig {
                    language_id: "rust".to_string(),
                    command: "sh".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 5,
                    heuristics: None,
                    name: Some("hover-only".to_string()),
                    handles: Some(vec![ToolKind::Hover]),
                },
                LspServerConfig {
                    language_id: "rust".to_string(),
                    command: "sh".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                    file_patterns: vec![],
                    initialization_options: None,
                    timeout_seconds: 5,
                    heuristics: None,
                    name: Some("diag-catchall".to_string()),
                    handles: None,
                },
            ];
            let router = ToolRouter::from_configs(configs.iter()).unwrap();

            let cache = Arc::new(Mutex::new(crate::bridge::NotificationCache::new()));
            let translator = Translator::new()
                .with_router(router)
                .with_notification_cache(Arc::clone(&cache));
            translator.register_client(hover_id.clone(), seed.client().clone());
            translator.register_server(hover_id.clone(), seed);

            let owned_by_healthy_server: lsp_types::Uri =
                "file:///workspace/still_healthy.rs".parse().unwrap();
            cache
                .lock()
                .await
                .store_diagnostics(&owned_by_healthy_server, None, vec![]);

            wait_until_dead(&translator, &hover_id).await;

            let respawn_script = write_responder_script(dir.path(), 1);
            // `language_id` must match the router's ("rust"), not the
            // routing identity ("hover-only"): otherwise `is_diagnostics_route`
            // returns `false` because of a language mismatch rather than
            // because of the `handles: Some([Hover])` restriction this test
            // means to exercise, which would pass for the wrong reason.
            let mut respawn_config = stub_server_config("hover-only", &respawn_script);
            respawn_config.server_config.language_id = "rust".to_string();
            translator.register_server_config(hover_id.clone(), respawn_config);

            translator.respawn_if_dead(&hover_id).await.unwrap();

            assert!(
                cache
                    .lock()
                    .await
                    .get_diagnostics(owned_by_healthy_server.as_str())
                    .is_some(),
                "respawning a non-diagnostics-route server must not clear \
                 the diagnostics-route server's cache entries"
            );
        }

        /// #249 test-gap closure: proves `resolve_client_for_file`'s
        /// dead-server branch is actually reached through the shared
        /// entry point every public tool handler (`handle_hover`,
        /// `handle_definition`, ...) funnels through -- not just through
        /// the private `respawn_if_dead`/`is_server_dead` calls the other
        /// tests in this module make directly.
        #[tokio::test]
        async fn test_prepare_document_respawns_dead_server_through_shared_entry_point() {
            let dir = TempDir::new().unwrap();
            let workspace = dir.path();
            let file_path = workspace.join("main.rs");
            fs::write(&file_path, "fn main() {}").unwrap();

            let seed_script = write_crash_after_init_script(dir.path());
            let id = ServerId::from("rust");
            let seed_config = stub_server_config("rust", &seed_script);

            let seed = LspServer::spawn(seed_config).await.unwrap();
            let mut translator = Translator::new()
                .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]))
                .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
            translator.set_workspace_roots(vec![workspace.to_path_buf()]);
            translator.register_client(id.clone(), seed.client().clone());
            translator.register_server(id.clone(), seed);
            wait_until_dead(&translator, &id).await;

            let respawn_script = write_responder_script(dir.path(), 1);
            translator
                .register_server_config(id.clone(), stub_server_config("rust", &respawn_script));

            let result = translator
                .prepare_document(&file_path.to_string_lossy(), ToolKind::Hover)
                .await;
            assert!(result.is_ok(), "got {result:?}");

            assert!(
                !translator.is_server_dead(&id),
                "the respawned replacement should be alive"
            );
        }
    }

    #[test]
    fn test_diagnostic_request_params_omit_optional_null_fields() {
        let uri = "file:///test.ts".parse().unwrap();
        let params = diagnostic_request_params(TextDocumentIdentifier { uri });
        let value = serde_json::to_value(params).unwrap();

        assert_eq!(value["textDocument"]["uri"], "file:///test.ts");
        assert!(value.get("identifier").is_none());
        assert!(value.get("previousResultId").is_none());
    }

    #[test]
    fn test_validate_path_no_workspace_roots() {
        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // With no workspace roots, any valid path should be accepted
        let result = translator.validate_path(&test_file);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_within_workspace() {
        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().to_path_buf();
        translator.set_workspace_roots(vec![workspace_root]);

        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator.validate_path(&test_file);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_outside_workspace() {
        let mut translator = Translator::new();
        let temp_dir1 = TempDir::new().unwrap();
        let temp_dir2 = TempDir::new().unwrap();

        // Set workspace root to temp_dir1
        translator.set_workspace_roots(vec![temp_dir1.path().to_path_buf()]);

        // Create file in temp_dir2 (outside workspace)
        let test_file = temp_dir2.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator.validate_path(&test_file);
        assert!(matches!(result, Err(Error::PathOutsideWorkspace(_))));
    }

    #[test]
    fn test_normalize_range() {
        let lsp_range = lsp_types::Range {
            start: lsp_types::Position {
                line: 0,
                character: 0,
            },
            end: lsp_types::Position {
                line: 2,
                character: 5,
            },
        };

        let mcp_range = normalize_range(lsp_range);
        assert_eq!(mcp_range.start.line, 1);
        assert_eq!(mcp_range.start.character, 1);
        assert_eq!(mcp_range.end.line, 3);
        assert_eq!(mcp_range.end.character, 6);
    }

    #[test]
    fn test_extract_hover_contents_string() {
        let marked_string = lsp_types::MarkedString::String("Test hover".to_string());
        let contents = lsp_types::HoverContents::Scalar(marked_string);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "Test hover");
    }

    #[test]
    fn test_extract_hover_contents_language_string() {
        let marked_string = lsp_types::MarkedString::LanguageString(lsp_types::LanguageString {
            language: "rust".to_string(),
            value: "fn main() {}".to_string(),
        });
        let contents = lsp_types::HoverContents::Scalar(marked_string);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "```rust\nfn main() {}\n```");
    }

    #[test]
    fn test_extract_hover_contents_markup() {
        let markup = lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::Markdown,
            value: "# Documentation".to_string(),
        };
        let contents = lsp_types::HoverContents::Markup(markup);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "# Documentation");
    }

    #[tokio::test]
    async fn test_handle_workspace_symbol_no_server() {
        let translator = Translator::new();
        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;
        assert!(matches!(result, Err(Error::NoServerConfigured)));
    }

    #[tokio::test]
    async fn test_handle_code_actions_invalid_kind() {
        let translator = Translator::new();
        let result = translator
            .handle_code_actions(
                "/tmp/test.rs".to_string(),
                1,
                1,
                1,
                10,
                Some("invalid_kind".to_string()),
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_quickfix() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("quickfix".to_string()),
            )
            .await;
        // Will fail due to no LSP server, but validates kind is accepted
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_refactor() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("refactor".to_string()),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_refactor_extract() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("refactor.extract".to_string()),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_source() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                test_file.to_str().unwrap().to_string(),
                1,
                1,
                1,
                10,
                Some("source.organizeImports".to_string()),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_invalid_range_zero() {
        let translator = Translator::new();
        let result = translator
            .handle_code_actions("/tmp/test.rs".to_string(), 0, 1, 1, 10, None)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_invalid_range_order() {
        let translator = Translator::new();
        let result = translator
            .handle_code_actions("/tmp/test.rs".to_string(), 10, 5, 5, 1, None)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_empty_range() {
        use tempfile::TempDir;

        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // Empty range (same position) should be valid
        let result = translator
            .handle_code_actions(test_file.to_str().unwrap().to_string(), 1, 5, 1, 5, None)
            .await;
        // Will fail due to no LSP server, but validates range is accepted
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[test]
    fn test_convert_code_action_minimal() {
        let lsp_action = lsp_types::CodeAction {
            title: "Fix issue".to_string(),
            kind: None,
            diagnostics: None,
            edit: None,
            command: None,
            is_preferred: None,
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action);
        assert_eq!(result.title, "Fix issue");
        assert!(result.kind.is_none());
        assert!(result.diagnostics.is_empty());
        assert!(result.edit.is_none());
        assert!(result.command.is_none());
        assert!(!result.is_preferred);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_convert_code_action_with_diagnostics_all_severities() {
        let lsp_diagnostics = vec![
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: "Error message".to_string(),
                code: Some(lsp_types::NumberOrString::Number(1)),
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 1,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 1,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                message: "Warning message".to_string(),
                code: Some(lsp_types::NumberOrString::String("W001".to_string())),
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 2,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 2,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::INFORMATION),
                message: "Info message".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 3,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 3,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::HINT),
                message: "Hint message".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
        ];

        let lsp_action = lsp_types::CodeAction {
            title: "Fix all issues".to_string(),
            kind: Some(lsp_types::CodeActionKind::QUICKFIX),
            diagnostics: Some(lsp_diagnostics),
            edit: None,
            command: None,
            is_preferred: None,
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action);
        assert_eq!(result.diagnostics.len(), 4);
        assert!(matches!(
            result.diagnostics[0].severity,
            DiagnosticSeverity::Error
        ));
        assert!(matches!(
            result.diagnostics[1].severity,
            DiagnosticSeverity::Warning
        ));
        assert!(matches!(
            result.diagnostics[2].severity,
            DiagnosticSeverity::Information
        ));
        assert!(matches!(
            result.diagnostics[3].severity,
            DiagnosticSeverity::Hint
        ));
        assert_eq!(result.diagnostics[0].code, Some("1".to_string()));
        assert_eq!(result.diagnostics[1].code, Some("W001".to_string()));
    }

    #[test]
    #[allow(clippy::mutable_key_type)]
    fn test_convert_code_action_with_workspace_edit() {
        use std::collections::HashMap;
        use std::str::FromStr;

        let uri = lsp_types::Uri::from_str("file:///test.rs").unwrap();
        let mut changes_map = HashMap::new();
        changes_map.insert(
            uri,
            vec![lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 5,
                    },
                },
                new_text: "fixed".to_string(),
            }],
        );

        let lsp_action = lsp_types::CodeAction {
            title: "Apply fix".to_string(),
            kind: Some(lsp_types::CodeActionKind::QUICKFIX),
            diagnostics: None,
            edit: Some(lsp_types::WorkspaceEdit {
                changes: Some(changes_map),
                document_changes: None,
                change_annotations: None,
            }),
            command: None,
            is_preferred: Some(true),
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action);
        assert!(result.edit.is_some());
        let edit = result.edit.unwrap();
        assert_eq!(edit.changes.len(), 1);
        assert_eq!(edit.changes[0].uri, "file:///test.rs");
        assert_eq!(edit.changes[0].edits.len(), 1);
        assert_eq!(edit.changes[0].edits[0].new_text, "fixed");
        assert!(result.is_preferred);
    }

    #[test]
    fn test_convert_code_action_with_command() {
        let lsp_action = lsp_types::CodeAction {
            title: "Run command".to_string(),
            kind: Some(lsp_types::CodeActionKind::REFACTOR),
            diagnostics: None,
            edit: None,
            command: Some(lsp_types::Command {
                title: "Execute refactor".to_string(),
                command: "refactor.extract".to_string(),
                arguments: Some(vec![serde_json::json!("arg1"), serde_json::json!(42)]),
            }),
            is_preferred: None,
            disabled: None,
            data: None,
        };

        let result = convert_code_action(lsp_action);
        assert!(result.command.is_some());
        let cmd = result.command.unwrap();
        assert_eq!(cmd.title, "Execute refactor");
        assert_eq!(cmd.command, "refactor.extract");
        assert_eq!(cmd.arguments.len(), 2);
    }

    #[tokio::test]
    async fn test_handle_call_hierarchy_prepare_invalid_position_zero() {
        let translator = Translator::new();
        let result = translator
            .handle_call_hierarchy_prepare("/tmp/test.rs".to_string(), 0, 1)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));

        let result = translator
            .handle_call_hierarchy_prepare("/tmp/test.rs".to_string(), 1, 0)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_call_hierarchy_prepare_invalid_position_too_large() {
        let translator = Translator::new();
        let result = translator
            .handle_call_hierarchy_prepare("/tmp/test.rs".to_string(), 1_000_001, 1)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));

        let result = translator
            .handle_call_hierarchy_prepare("/tmp/test.rs".to_string(), 1, 1_000_001)
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_incoming_calls_invalid_json() {
        let translator = Translator::new();
        let invalid_item = serde_json::json!({"invalid": "structure"});
        let result = translator.handle_incoming_calls(invalid_item).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_outgoing_calls_invalid_json() {
        let translator = Translator::new();
        let invalid_item = serde_json::json!({"invalid": "structure"});
        let result = translator.handle_outgoing_calls(invalid_item).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_parse_file_uri_invalid_scheme() {
        let translator = Translator::new();
        let uri: lsp_types::Uri = "http://example.com/file.rs".parse().unwrap();
        let result = translator.parse_file_uri(&uri);
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_parse_file_uri_valid_scheme() {
        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // Use url crate for cross-platform file URI creation
        let file_url = Url::from_file_path(&test_file).unwrap();
        let uri: lsp_types::Uri = file_url.as_str().parse().unwrap();
        let result = translator.parse_file_uri(&uri);
        assert!(result.is_ok());
    }

    #[test]
    fn test_handle_cached_diagnostics_empty() {
        let cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let cache_key =
            Translator::cached_diagnostics_uri(&[], test_file.to_str().unwrap()).unwrap();
        let diag_info = cache.get_diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(diag_info.as_ref());
        assert_eq!(diags.diagnostics.len(), 0);
    }

    #[test]
    fn test_handle_server_logs_with_filter() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        // Add some logs
        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        // Test with error filter
        let result = Translator::handle_server_logs(&cache, 10, Some("error".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 1);
        assert_eq!(logs.logs[0].message, "error msg");

        // Test with warning filter (includes error and warning)
        let result = Translator::handle_server_logs(&cache, 10, Some("warning".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 2);

        // Test with info filter (excludes debug)
        let result = Translator::handle_server_logs(&cache, 10, Some("info".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 3);

        // Test with debug filter (includes all)
        let result = Translator::handle_server_logs(&cache, 10, Some("debug".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 4);

        // Test with invalid filter
        let result = Translator::handle_server_logs(&cache, 10, Some("invalid".to_string()));
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[test]
    fn test_handle_server_messages_limit() {
        use crate::bridge::notifications::MessageType;

        let mut cache = NotificationCache::new();

        // Add some messages
        for i in 0..10 {
            cache.store_message(MessageType::Info, format!("message {i}"));
        }

        // Test limit
        let result = Translator::handle_server_messages(&cache, 5);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 5);
        assert_eq!(messages.messages[0].message, "message 0");
        assert_eq!(messages.messages[4].message, "message 4");

        // Test limit larger than available
        let result = Translator::handle_server_messages(&cache, 100);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 10);
    }

    #[test]
    fn test_handle_cached_diagnostics_with_data() {
        let mut cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: Some(lsp_types::NumberOrString::String("E001".to_string())),
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&uri, Some(1), vec![diagnostic]);

        let cache_key =
            Translator::cached_diagnostics_uri(&[], test_file.to_str().unwrap()).unwrap();
        let diag_info = cache.get_diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(diag_info.as_ref());
        assert_eq!(diags.diagnostics.len(), 1);
        assert_eq!(diags.diagnostics[0].message, "test error");
        assert_eq!(diags.diagnostics[0].code, Some("E001".to_string()));
        assert!(matches!(
            diags.diagnostics[0].severity,
            DiagnosticSeverity::Error
        ));
        assert_eq!(diags.diagnostics[0].range.start.line, 1);
        assert_eq!(diags.diagnostics[0].range.start.character, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_handle_cached_diagnostics_multiple_severities() {
        let mut cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostics = vec![
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                message: "error".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 1,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 1,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::WARNING),
                message: "warning".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 2,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 2,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::INFORMATION),
                message: "info".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 3,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 3,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::HINT),
                message: "hint".to_string(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
        ];

        cache.store_diagnostics(&uri, Some(1), diagnostics);

        let cache_key =
            Translator::cached_diagnostics_uri(&[], test_file.to_str().unwrap()).unwrap();
        let diag_info = cache.get_diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(diag_info.as_ref());
        assert_eq!(diags.diagnostics.len(), 4);
        assert!(matches!(
            diags.diagnostics[0].severity,
            DiagnosticSeverity::Error
        ));
        assert!(matches!(
            diags.diagnostics[1].severity,
            DiagnosticSeverity::Warning
        ));
        assert!(matches!(
            diags.diagnostics[2].severity,
            DiagnosticSeverity::Information
        ));
        assert!(matches!(
            diags.diagnostics[3].severity,
            DiagnosticSeverity::Hint
        ));
    }

    #[test]
    fn test_handle_cached_diagnostics_with_numeric_code() {
        let mut cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: Some(lsp_types::NumberOrString::Number(42)),
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&uri, Some(1), vec![diagnostic]);

        let cache_key =
            Translator::cached_diagnostics_uri(&[], test_file.to_str().unwrap()).unwrap();
        let diag_info = cache.get_diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(diag_info.as_ref());
        assert_eq!(diags.diagnostics.len(), 1);
        assert_eq!(diags.diagnostics[0].code, Some("42".to_string()));
    }

    #[test]
    fn test_handle_cached_diagnostics_invalid_path() {
        let result = Translator::cached_diagnostics_uri(&[], "/nonexistent/path/file.rs");
        assert!(matches!(result, Err(Error::FileIo { .. })));
    }

    /// Builds an LSP-side diagnostic for `merge_diagnostics` cache fixtures.
    fn lsp_diag(
        line: u32,
        end_character: u32,
        severity: lsp_types::DiagnosticSeverity,
        message: &str,
        code: Option<&str>,
    ) -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position { line, character: 0 },
                end: lsp_types::Position {
                    line,
                    character: end_character,
                },
            },
            severity: Some(severity),
            message: message.to_string(),
            code: code.map(|c| lsp_types::NumberOrString::String(c.to_string())),
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        }
    }

    fn diag_info(diagnostics: Vec<lsp_types::Diagnostic>) -> DiagnosticInfo {
        DiagnosticInfo {
            uri: "file:///test.rs".parse().unwrap(),
            version: Some(1),
            diagnostics,
        }
    }

    #[test]
    fn test_merge_diagnostics_cache_only_appends_to_empty_pull() {
        let pull = DiagnosticsResult {
            diagnostics: vec![],
        };
        let cache = diag_info(vec![lsp_diag(
            0,
            10,
            lsp_types::DiagnosticSeverity::WARNING,
            "unused import: `std::fmt`",
            None,
        )]);

        let merged = Translator::merge_diagnostics(pull, Some(&cache));

        assert_eq!(merged.diagnostics.len(), 1);
        assert_eq!(merged.diagnostics[0].message, "unused import: `std::fmt`");
        assert!(matches!(
            merged.diagnostics[0].severity,
            DiagnosticSeverity::Warning
        ));
    }

    #[test]
    fn test_merge_diagnostics_exact_duplicate_not_repeated() {
        // Same range/severity/message/code as the cache entry below, expressed
        // in the 1-based MCP shape `diagnostics_from_cache_entry` would produce.
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 11,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "mismatched types".to_string(),
            code: Some("E0308".to_string()),
        };
        let pull = DiagnosticsResult {
            diagnostics: vec![pull_diag.clone()],
        };
        let cache = diag_info(vec![lsp_diag(
            0,
            10,
            lsp_types::DiagnosticSeverity::ERROR,
            "mismatched types",
            Some("E0308"),
        )]);

        let merged = Translator::merge_diagnostics(pull, Some(&cache));

        assert_eq!(merged.diagnostics.len(), 1);
        assert_eq!(merged.diagnostics[0], pull_diag);
    }

    #[test]
    fn test_merge_diagnostics_no_cache_entry_returns_pull_unchanged() {
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 5,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "syntax error".to_string(),
            code: None,
        };
        let pull = DiagnosticsResult {
            diagnostics: vec![pull_diag.clone()],
        };

        let merged = Translator::merge_diagnostics(pull, None);

        assert_eq!(merged.diagnostics, vec![pull_diag]);
    }

    #[test]
    fn test_merge_diagnostics_multiple_distinct_cache_entries_all_appear() {
        let pull = DiagnosticsResult {
            diagnostics: vec![],
        };
        let cache = diag_info(vec![
            lsp_diag(
                0,
                10,
                lsp_types::DiagnosticSeverity::WARNING,
                "unused import: `std::fmt`",
                None,
            ),
            lsp_diag(
                5,
                8,
                lsp_types::DiagnosticSeverity::WARNING,
                "function `helper` is never used",
                None,
            ),
        ]);

        let merged = Translator::merge_diagnostics(pull, Some(&cache));

        assert_eq!(merged.diagnostics.len(), 2);
        assert!(
            merged
                .diagnostics
                .iter()
                .any(|d| d.message == "unused import: `std::fmt`")
        );
        assert!(
            merged
                .diagnostics
                .iter()
                .any(|d| d.message == "function `helper` is never used")
        );
    }

    #[test]
    fn test_merge_diagnostics_same_range_different_message_not_deduped() {
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 11,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "mismatched types".to_string(),
            code: None,
        };
        let pull = DiagnosticsResult {
            diagnostics: vec![pull_diag],
        };
        // Same range and severity as the pull diagnostic, but a different
        // message — must be treated as a distinct diagnostic, not a duplicate.
        let cache = diag_info(vec![lsp_diag(
            0,
            10,
            lsp_types::DiagnosticSeverity::ERROR,
            "expected `i32`, found `&str`",
            None,
        )]);

        let merged = Translator::merge_diagnostics(pull, Some(&cache));

        assert_eq!(merged.diagnostics.len(), 2);
    }

    /// Pins a cross-model duplicate shape verified empirically against a live
    /// rust-analyzer 1.97.1 session (#244): the pull and push diagnostics for
    /// the *same* "not all trait items implemented" (E0046) error had
    /// different ranges (trait name vs. impl block) and different messages
    /// (terse vs. rustc's full rendering), but shared `code` and `severity`.
    /// Exact-field dedup would report this twice; the `(severity, code)`
    /// fingerprint must collapse it to one entry.
    #[test]
    fn test_merge_diagnostics_same_code_different_range_and_message_deduped() {
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 96,
                    character: 7,
                },
                end: Position2D {
                    line: 96,
                    character: 12,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "not all trait items implemented, missing: `fn hello`".to_string(),
            code: Some("E0046".to_string()),
        };
        let pull = DiagnosticsResult {
            diagnostics: vec![pull_diag.clone()],
        };
        // Same code and severity, but a different range and a longer,
        // differently-worded message -- the rustc-rendered push side of the
        // same underlying error.
        let cache = diag_info(vec![lsp_diag(
            94,
            31,
            lsp_types::DiagnosticSeverity::ERROR,
            "not all trait items implemented, missing: `hello`\nmissing `hello` in implementation",
            Some("E0046"),
        )]);

        let merged = Translator::merge_diagnostics(pull, Some(&cache));

        assert_eq!(merged.diagnostics.len(), 1);
        assert_eq!(merged.diagnostics[0], pull_diag);
    }

    /// Regression: `merge_diagnostics`'s `(severity, code)` fingerprint alone
    /// is coarser than full-field equality and cannot tell apart two
    /// genuinely distinct diagnostics that happen to share `code` and
    /// `severity` -- e.g. two separate `E0308` mismatched-type errors at
    /// different locations in the same file, one caught only by native
    /// (pull) analysis and a second, unrelated one caught only by
    /// flycheck/cargo check (cache), such as an error inside macro-expanded
    /// code the native pass did not evaluate. This previously caused the
    /// cache-only entry to be silently dropped -- reproducing #244's exact
    /// failure mode, just relocated from "no merge" to "over-eager dedup".
    ///
    /// The range-proximity check on `is_duplicate` (see `merge_diagnostics`)
    /// closes this: these two diagnostics are 45 lines apart, far outside
    /// `DUPLICATE_RANGE_PROXIMITY_LINES`, so both must survive the merge.
    #[test]
    fn test_merge_diagnostics_same_code_distinct_diagnostics_at_different_locations_both_kept() {
        let pull_diag = Diagnostic {
            range: Range {
                start: Position2D {
                    line: 5,
                    character: 9,
                },
                end: Position2D {
                    line: 5,
                    character: 20,
                },
            },
            severity: DiagnosticSeverity::Error,
            message: "mismatched types: expected `i32`, found `&str`".to_string(),
            code: Some("E0308".to_string()),
        };
        let pull = DiagnosticsResult {
            diagnostics: vec![pull_diag.clone()],
        };
        // A second, unrelated E0308 at a completely different location with
        // a completely different message -- a real, distinct diagnostic,
        // not a duplicate of pull_diag.
        let cache = diag_info(vec![lsp_diag(
            49,
            22,
            lsp_types::DiagnosticSeverity::ERROR,
            "mismatched types: expected `String`, found `Vec<u8>`",
            Some("E0308"),
        )]);

        let merged = Translator::merge_diagnostics(pull, Some(&cache));

        assert_eq!(merged.diagnostics.len(), 2);
        assert_eq!(merged.diagnostics[0], pull_diag);
        assert_eq!(
            merged.diagnostics[1].message,
            "mismatched types: expected `String`, found `Vec<u8>`"
        );
    }

    #[test]
    fn test_handle_server_logs_no_filter() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, None);
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 4);
    }

    #[test]
    fn test_handle_server_logs_error_filter_strict() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("error".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 1);
        assert_eq!(logs.logs[0].message, "error msg");
    }

    #[test]
    fn test_handle_server_logs_warning_filter_includes_errors() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("warning".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 2);
    }

    #[test]
    fn test_handle_server_logs_info_filter_excludes_debug() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("info".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 2);
    }

    #[test]
    fn test_handle_server_logs_debug_filter_includes_all() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("debug".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 4);
    }

    #[test]
    fn test_handle_server_logs_limit_applies_after_filter() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        for i in 0..10 {
            cache.store_log(LogLevel::Error, format!("error {i}"));
        }

        let result = Translator::handle_server_logs(&cache, 5, Some("error".to_string()));
        assert!(result.is_ok());
        let logs = result.unwrap();
        assert_eq!(logs.logs.len(), 5);
        assert_eq!(logs.logs[0].message, "error 0");
        assert_eq!(logs.logs[4].message, "error 4");
    }

    #[test]
    fn test_handle_server_logs_case_insensitive_level() {
        use crate::bridge::notifications::LogLevel;

        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());

        let result = Translator::handle_server_logs(&cache, 10, Some("ERROR".to_string()));
        assert!(result.is_ok());

        let result = Translator::handle_server_logs(&cache, 10, Some("Error".to_string()));
        assert!(result.is_ok());

        let result = Translator::handle_server_logs(&cache, 10, Some("eRrOr".to_string()));
        assert!(result.is_ok());
    }

    #[test]
    fn test_handle_server_messages_empty() {
        let cache = NotificationCache::new();

        let result = Translator::handle_server_messages(&cache, 10);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 0);
    }

    #[test]
    fn test_handle_server_messages_with_different_types() {
        use crate::bridge::notifications::MessageType;

        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Error, "error".to_string());
        cache.store_message(MessageType::Warning, "warning".to_string());
        cache.store_message(MessageType::Info, "info".to_string());
        cache.store_message(MessageType::Log, "log".to_string());

        let result = Translator::handle_server_messages(&cache, 10);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 4);
        assert_eq!(messages.messages[0].message, "error");
        assert_eq!(messages.messages[1].message, "warning");
        assert_eq!(messages.messages[2].message, "info");
        assert_eq!(messages.messages[3].message, "log");
    }

    #[test]
    fn test_handle_server_messages_zero_limit() {
        use crate::bridge::notifications::MessageType;

        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Info, "test".to_string());

        let result = Translator::handle_server_messages(&cache, 0);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 0);
    }

    #[test]
    fn test_handle_cached_diagnostics_path_outside_workspace() {
        let temp_dir1 = TempDir::new().unwrap();
        let temp_dir2 = TempDir::new().unwrap();

        let workspace_roots = vec![temp_dir1.path().to_path_buf()];

        let test_file = temp_dir2.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result =
            Translator::cached_diagnostics_uri(&workspace_roots, test_file.to_str().unwrap());
        assert!(matches!(result, Err(Error::PathOutsideWorkspace(_))));
    }

    #[test]
    fn test_translator_with_custom_extensions() {
        let mut extension_map = HashMap::new();
        extension_map.insert("nu".to_string(), "nushell".to_string());
        extension_map.insert("customext".to_string(), "customlang".to_string());

        let translator = Translator::new().with_extensions(extension_map.clone());

        assert_eq!(translator.extension_map.len(), 2);
        assert_eq!(
            translator.extension_map.get("nu"),
            Some(&"nushell".to_string())
        );
        assert_eq!(
            translator.extension_map.get("customext"),
            Some(&"customlang".to_string())
        );
    }

    #[test]
    fn test_get_client_for_file_uses_custom_extension() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("script.nu");
        fs::write(&test_file, "echo hello").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("nu".to_string(), "nushell".to_string());

        let translator = Translator::new().with_extensions(extension_map);

        let result = translator.get_client_for_file(&test_file, ToolKind::Hover);

        assert!(result.is_err());
        if let Err(Error::NoServerForLanguage(lang)) = result {
            assert_eq!(lang, "nushell");
        } else {
            panic!("Expected NoServerForLanguage(nushell) error");
        }
    }

    #[test]
    fn test_get_client_for_file_falls_back_to_default() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("unknown.xyz");
        fs::write(&test_file, "content").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("rs".to_string(), "rust".to_string());

        let translator = Translator::new().with_extensions(extension_map);

        let result = translator.get_client_for_file(&test_file, ToolKind::Hover);

        assert!(result.is_err());
        if let Err(Error::NoServerForLanguage(lang)) = result {
            assert_eq!(lang, "plaintext");
        } else {
            panic!("Expected NoServerForLanguage(plaintext) error");
        }
    }

    #[test]
    fn test_get_client_for_file_routes_tsx_to_typescript_server() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("component.tsx");
        fs::write(&test_file, "export const Component = () => <div />").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("tsx".to_string(), "typescriptreact".to_string());

        let translator = Translator::new()
            .with_extensions(extension_map)
            .with_router(ToolRouter::catch_all([(
                ServerId::from("typescript"),
                "typescript".to_string(),
            )]));
        translator.register_client(
            "typescript".to_string(),
            LspClient::new(crate::config::LspServerConfig::typescript()),
        );

        let (_id, client) = translator
            .get_client_for_file(&test_file, ToolKind::Hover)
            .unwrap();
        assert_eq!(client.language_id(), "typescript");
    }

    #[test]
    fn test_get_client_for_file_prefers_exact_react_server() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("component.tsx");
        fs::write(&test_file, "export const Component = () => <div />").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("tsx".to_string(), "typescriptreact".to_string());

        let typescript_react_config = crate::config::LspServerConfig {
            language_id: "typescriptreact".to_string(),
            command: "typescript-language-server".to_string(),
            args: vec!["--stdio".to_string()],
            env: HashMap::new(),
            file_patterns: vec!["**/*.tsx".to_string()],
            initialization_options: None,
            timeout_seconds: 30,
            heuristics: None,
            name: None,
            handles: None,
        };

        let translator = Translator::new()
            .with_extensions(extension_map)
            .with_router(ToolRouter::catch_all([
                (ServerId::from("typescript"), "typescript".to_string()),
                (
                    ServerId::from("typescriptreact"),
                    "typescriptreact".to_string(),
                ),
            ]));
        translator.register_client(
            "typescript".to_string(),
            LspClient::new(crate::config::LspServerConfig::typescript()),
        );
        translator.register_client(
            "typescriptreact".to_string(),
            LspClient::new(typescript_react_config),
        );

        let (_id, client) = translator
            .get_client_for_file(&test_file, ToolKind::Hover)
            .unwrap();
        assert_eq!(client.language_id(), "typescriptreact");
    }

    #[test]
    fn test_get_client_for_file_routes_jsx_to_javascript_server() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("component.jsx");
        fs::write(&test_file, "export const Component = () => <div />").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("jsx".to_string(), "javascriptreact".to_string());

        let javascript_config = crate::config::LspServerConfig {
            language_id: "javascript".to_string(),
            command: "typescript-language-server".to_string(),
            args: vec!["--stdio".to_string()],
            env: HashMap::new(),
            file_patterns: vec!["**/*.js".to_string(), "**/*.jsx".to_string()],
            initialization_options: None,
            timeout_seconds: 30,
            heuristics: None,
            name: None,
            handles: None,
        };
        let translator = Translator::new()
            .with_extensions(extension_map)
            .with_router(ToolRouter::catch_all([(
                ServerId::from("javascript"),
                "javascript".to_string(),
            )]));
        translator.register_client("javascript".to_string(), LspClient::new(javascript_config));

        let (_id, client) = translator
            .get_client_for_file(&test_file, ToolKind::Hover)
            .unwrap();
        assert_eq!(client.language_id(), "javascript");
    }

    #[tokio::test]
    async fn test_serve_initializes_translator_with_extensions() {
        use crate::config::{LanguageExtensionMapping, WorkspaceConfig};

        let language_extensions = vec![
            LanguageExtensionMapping {
                extensions: vec!["nu".to_string()],
                language_id: "nushell".to_string(),
            },
            LanguageExtensionMapping {
                extensions: vec!["rs".to_string()],
                language_id: "rust".to_string(),
            },
        ];

        let config = crate::config::ServerConfig {
            workspace: WorkspaceConfig {
                roots: vec![PathBuf::from("/tmp/test-workspace")],
                position_encodings: vec!["utf-8".to_string()],
                language_extensions: language_extensions.clone(),
                heuristics_max_depth: 10,
            },
            lsp_servers: vec![],
            project_config_ignored: false,
        };

        let extension_map = config.build_effective_extension_map();
        assert_eq!(extension_map.get("nu"), Some(&"nushell".to_string()));
        assert_eq!(extension_map.get("rs"), Some(&"rust".to_string()));

        // serve() starts in protocol-only mode when no LSP servers are configured;
        // it may return a transport error but must not return NoServersAvailable.
        let result = crate::serve(config).await;
        if let Err(ref err) = result {
            assert!(
                !matches!(err, crate::error::Error::NoServersAvailable(_)),
                "serve() must not return NoServersAvailable for empty lsp_servers config"
            );
        }
    }

    #[test]
    fn test_convert_call_hierarchy_item_kind_is_numeric() {
        let item = lsp_types::CallHierarchyItem {
            name: "my_fn".to_string(),
            kind: lsp_types::SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: "file:///tmp/test.rs".parse().unwrap(),
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 5,
                },
            },
            selection_range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 5,
                },
            },
            data: None,
        };
        let result = convert_call_hierarchy_item(item);
        // SymbolKind::FUNCTION is LSP integer 12
        assert_eq!(result.kind, 12u32);
        assert_eq!(result.name, "my_fn");
    }

    // ------------------------------------------------------------------
    // Lock-latency regression tests (#108, #159)
    // ------------------------------------------------------------------
    //
    // These use two `cat` child processes as a fake LSP transport, the same
    // technique as `bridge::state::tests::fake_lsp_client` (duplicated here
    // since that helper is private to its own test module): `cat` on the
    // "write" half echoes back whatever mcpls sends it, letting a test read
    // outbound requests/notifications off `write_stdout`; `cat` on the "read"
    // half relays whatever a test writes to `read_half_stdin` back to the
    // client as if it came from a real server, letting a test fabricate
    // responses with controlled timing.

    use std::process::Stdio;

    use serde_json::Value as JsonValue;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::process::{Child, ChildStdin, ChildStdout, Command};
    use tokio::time::timeout;

    use crate::config::LspServerConfig;
    use crate::lsp::LspTransport;

    struct FakeServer {
        _write_half: Child,
        _read_half: Child,
        read_half_stdin: ChildStdin,
        write_stdout: ChildStdout,
    }

    fn fake_lsp_client() -> (LspClient, FakeServer) {
        let mut write_half = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let write_stdin = write_half.stdin.take().unwrap();
        let write_stdout = write_half.stdout.take().unwrap();

        let mut read_half = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let read_stdout = read_half.stdout.take().unwrap();
        let read_stdin = read_half.stdin.take().unwrap();

        let transport = LspTransport::new(write_stdin, read_stdout);
        let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);

        (
            client,
            FakeServer {
                _write_half: write_half,
                _read_half: read_half,
                read_half_stdin: read_stdin,
                write_stdout,
            },
        )
    }

    /// Reads one `Content-Length`-framed JSON-RPC message off `reader`.
    ///
    /// `reader` must be reused across calls, not recreated per message: a
    /// fresh `BufReader` would silently drop any bytes of a later message it
    /// over-read into its internal buffer while parsing an earlier one.
    async fn read_framed_message(reader: &mut BufReader<&mut ChildStdout>) -> JsonValue {
        let mut content_length = None;
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some((key, value)) = line.trim_end().split_once(':')
                && key.trim().eq_ignore_ascii_case("content-length")
            {
                content_length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let mut buf = vec![0u8; content_length.unwrap()];
        reader.read_exact(&mut buf).await.unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    /// Writes a framed JSON-RPC success response, as a real LSP server would.
    async fn write_response(stdin: &mut ChildStdin, id: &JsonValue, result: JsonValue) {
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        let content = serde_json::to_string(&message).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", content.len());
        stdin.write_all(header.as_bytes()).await.unwrap();
        stdin.write_all(content.as_bytes()).await.unwrap();
        stdin.flush().await.unwrap();
    }

    /// Writes a framed JSON-RPC error response, e.g. to simulate a push-only
    /// server answering `textDocument/diagnostic` with method-not-found.
    async fn write_error_response(
        stdin: &mut ChildStdin,
        id: &JsonValue,
        code: i64,
        message: &str,
    ) {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message,
            },
        });
        let content = serde_json::to_string(&response).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", content.len());
        stdin.write_all(header.as_bytes()).await.unwrap();
        stdin.write_all(content.as_bytes()).await.unwrap();
        stdin.flush().await.unwrap();
    }

    #[tokio::test]
    async fn test_concurrent_handlers_on_different_files_do_not_serialize() {
        // Before the fix, Translator was shared as Arc<Mutex<Translator>>, so
        // handling one LSP request held that lock across the `.await` on the
        // response -- blocking every other tool call, even for a completely
        // different file and language server, until the first request
        // completed or timed out (up to 30s). With interior mutability, a
        // concurrent call for a different file must complete without waiting
        // on an unrelated in-flight request.
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("aa".to_string(), "lang_a".to_string());
        extensions.insert("bb".to_string(), "lang_b".to_string());

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([
                    (ServerId::from("lang_a"), "lang_a".to_string()),
                    (ServerId::from("lang_b"), "lang_b".to_string()),
                ]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let (client_a, mut server_a) = fake_lsp_client();
        let (client_b, mut server_b) = fake_lsp_client();
        translator.register_client("lang_a".to_string(), client_a);
        translator.register_client("lang_b".to_string(), client_b);

        let path_a = dir.path().join("file.aa");
        let path_b = dir.path().join("file.bb");
        fs::write(&path_a, "content a").unwrap();
        fs::write(&path_b, "content b").unwrap();

        let translator = Arc::new(translator);

        // `server_a` is never given a response, simulating a slow server. If
        // any translator-held lock still spanned the LSP round trip, this
        // task blocking forever would also block the "fast" call below.
        let slow = {
            let translator = Arc::clone(&translator);
            let path = path_a.to_string_lossy().to_string();
            tokio::spawn(async move { translator.handle_hover(path, 1, 1).await })
        };

        // Wait for the slow task to actually reach its LSP request (i.e. the
        // request bytes were written to the wire) before treating it as
        // "in-flight", so the test doesn't race the spawned task's startup.
        let mut wire_a = BufReader::new(&mut server_a.write_stdout);
        let opened_a = read_framed_message(&mut wire_a).await;
        assert_eq!(opened_a["method"], "textDocument/didOpen");
        let hover_request_a = read_framed_message(&mut wire_a).await;
        assert_eq!(hover_request_a["method"], "textDocument/hover");

        // The fast path: a concurrent call for a different file/server.
        let fast = {
            let translator = Arc::clone(&translator);
            let path = path_b.to_string_lossy().to_string();
            tokio::spawn(async move { translator.handle_hover(path, 1, 1).await })
        };

        let mut wire_b = BufReader::new(&mut server_b.write_stdout);
        let opened_b = read_framed_message(&mut wire_b).await;
        assert_eq!(opened_b["method"], "textDocument/didOpen");
        let hover_request_b = read_framed_message(&mut wire_b).await;
        assert_eq!(hover_request_b["method"], "textDocument/hover");
        write_response(
            &mut server_b.read_half_stdin,
            &hover_request_b["id"],
            JsonValue::Null,
        )
        .await;

        let fast_result = timeout(Duration::from_secs(2), fast)
            .await
            .expect("fast call must not be blocked by the slow in-flight request")
            .unwrap();
        assert!(fast_result.is_ok());

        assert!(
            !slow.is_finished(),
            "slow call should still be waiting on its (never-sent) response"
        );
        slow.abort();
    }

    #[tokio::test]
    async fn test_concurrent_ensure_open_same_path_sends_single_did_open() {
        // Regression test: concurrent handler calls for the SAME path must
        // serialize on that path's `ensure_open` lock (see `DocumentTracker::lock_path`)
        // so they can't both observe "not open yet" and both send didOpen.
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("aa".to_string(), "lang_a".to_string());

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([(
                    ServerId::from("lang_a"),
                    "lang_a".to_string(),
                )]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let (client, mut server) = fake_lsp_client();
        translator.register_client("lang_a".to_string(), client);

        let path = dir.path().join("file.aa");
        fs::write(&path, "content").unwrap();

        let concurrent_calls = 4;

        let translator = Arc::new(translator);
        let path_str = path.to_string_lossy().to_string();

        let handles: Vec<_> = (0..concurrent_calls)
            .map(|_| {
                let translator = Arc::clone(&translator);
                let path_str = path_str.clone();
                tokio::spawn(async move { translator.handle_hover(path_str, 1, 1).await })
            })
            .collect();

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");

        for _ in 0..concurrent_calls {
            let request = read_framed_message(&mut wire).await;
            assert_eq!(
                request["method"], "textDocument/hover",
                "no second didOpen must appear ahead of the hover requests"
            );
            write_response(&mut server.read_half_stdin, &request["id"], JsonValue::Null).await;
        }

        for handle in handles {
            let result = timeout(Duration::from_secs(2), handle)
                .await
                .expect("handler call should not hang")
                .unwrap();
            assert!(result.is_ok());
        }
    }

    /// #174 §12's own headline dispatch scenario: "pyright/pylsp fixture --
    /// hover -> pyright, diagnostics -> pylsp, rename (unclaimed) ->
    /// `NoServerForTool`", exercised through `Translator`'s public handlers
    /// end to end rather than through `ToolRouter`'s unit tests alone.
    #[tokio::test]
    async fn test_dispatch_routes_hover_and_diagnostics_to_different_servers() {
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("py".to_string(), "python".to_string());

        let pyright_id = ServerId::from("pyright");
        let pylsp_id = ServerId::from("pylsp");
        let configs = vec![
            LspServerConfig {
                language_id: "python".to_string(),
                command: "pyright-langserver".to_string(),
                args: vec![],
                env: HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                timeout_seconds: 30,
                heuristics: None,
                name: Some("pyright".to_string()),
                handles: Some(vec![ToolKind::Hover]),
            },
            LspServerConfig {
                language_id: "python".to_string(),
                command: "pylsp".to_string(),
                args: vec![],
                env: HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                timeout_seconds: 30,
                heuristics: None,
                name: Some("pylsp".to_string()),
                handles: Some(vec![ToolKind::Diagnostics]),
            },
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();

        let mut translator = Translator::new()
            .with_extensions(extensions)
            .with_router(router);
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let (client_pyright, mut server_pyright) = fake_lsp_client();
        let (client_pylsp, mut server_pylsp) = fake_lsp_client();
        translator.register_client(pyright_id, client_pyright);
        translator.register_client(pylsp_id, client_pylsp);

        let path = dir.path().join("main.py");
        fs::write(&path, "x = 1").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let translator = Arc::new(translator);

        // rename is claimed by neither server -> NoServerForTool, checked
        // first so it can't be masked by either server's wire state.
        let rename_result = translator
            .handle_rename(path_str.clone(), 1, 1, "renamed".to_string())
            .await;
        assert!(
            matches!(
                rename_result,
                Err(Error::NoServerForTool {
                    tool: ToolKind::Rename,
                    ..
                })
            ),
            "expected NoServerForTool for rename, got {rename_result:?}"
        );

        // hover must route to pyright: didOpen + hover request on its wire.
        let hover = {
            let translator = Arc::clone(&translator);
            let path_str = path_str.clone();
            tokio::spawn(async move { translator.handle_hover(path_str, 1, 1).await })
        };
        let mut wire_pyright = BufReader::new(&mut server_pyright.write_stdout);
        let opened = read_framed_message(&mut wire_pyright).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let hover_request = read_framed_message(&mut wire_pyright).await;
        assert_eq!(hover_request["method"], "textDocument/hover");
        write_response(
            &mut server_pyright.read_half_stdin,
            &hover_request["id"],
            JsonValue::Null,
        )
        .await;
        hover
            .await
            .unwrap()
            .expect("hover routed to pyright must succeed");

        // diagnostics must route to pylsp, independently of pyright: its own
        // didOpen (a second server's first sync of the same path) followed
        // by the diagnostic request on pylsp's wire, never pyright's.
        let diagnostics = {
            let translator = Arc::clone(&translator);
            let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
            tokio::spawn(async move {
                translator
                    .handle_diagnostics(path_str, &notification_cache)
                    .await
            })
        };
        let mut wire_pylsp = BufReader::new(&mut server_pylsp.write_stdout);
        let opened = read_framed_message(&mut wire_pylsp).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire_pylsp).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        // Routing is proven by the request landing on pylsp's wire; abort
        // rather than crafting a well-formed DocumentDiagnosticReportResult.
        diagnostics.abort();
    }

    /// S1 regression (#244): a push-only server (or one that times out)
    /// answering `textDocument/diagnostic` with an LSP error must not
    /// discard diagnostics `handle_diagnostics` already knows about from the
    /// cache -- it should return the cache-only result instead of `Err`.
    #[tokio::test]
    async fn test_handle_diagnostics_pull_error_falls_back_to_nonempty_cache() {
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("rs".to_string(), "rust".to_string());

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([(
                    ServerId::from("rust"),
                    "rust".to_string(),
                )]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let (client, mut server) = fake_lsp_client();
        translator.register_client("rust".to_string(), client);

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        // Prime the cache under the exact URI handle_diagnostics will look
        // up (path_to_uri over the canonicalized path, same as
        // document_tracker uses to open the document).
        let canonical = path.canonicalize().unwrap();
        let uri = path_to_uri(&canonical);
        let notification_cache = Mutex::new(NotificationCache::new());
        {
            let mut cache = notification_cache.lock().await;
            cache.store_diagnostics(
                &uri,
                Some(1),
                vec![lsp_diag(
                    0,
                    4,
                    lsp_types::DiagnosticSeverity::WARNING,
                    "unused import: `std::fmt`",
                    None,
                )],
            );
        }

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_diagnostics(path_str, &notification_cache)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        write_error_response(
            &mut server.read_half_stdin,
            &diag_request["id"],
            -32601,
            "method not found",
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap();

        let diagnostics = result.expect("cache-only fallback should succeed despite pull error");
        assert_eq!(diagnostics.diagnostics.len(), 1);
        assert_eq!(
            diagnostics.diagnostics[0].message,
            "unused import: `std::fmt`"
        );
    }

    /// S1 counterpart: when the cache is also empty, the pull error must
    /// still propagate -- there is nothing to fall back to.
    #[tokio::test]
    async fn test_handle_diagnostics_pull_error_and_empty_cache_propagates_error() {
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("rs".to_string(), "rust".to_string());

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([(
                    ServerId::from("rust"),
                    "rust".to_string(),
                )]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let (client, mut server) = fake_lsp_client();
        translator.register_client("rust".to_string(), client);

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let notification_cache = Mutex::new(NotificationCache::new());

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_diagnostics(path_str, &notification_cache)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        write_error_response(
            &mut server.read_half_stdin,
            &diag_request["id"],
            -32601,
            "method not found",
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap();

        assert!(
            result.is_err(),
            "pull error with no cache data must propagate, got {result:?}"
        );
    }

    // ------------------------------------------------------------------
    // Capability gate tests (#240)
    // ------------------------------------------------------------------

    /// No `LspServer` registered for `server_id` (only a raw `LspClient`, as
    /// most tests in this module do) -- capability is unknown, so the gate
    /// must not block the request.
    #[test]
    fn test_require_capability_ok_when_server_not_registered() {
        let translator = Translator::new();
        let result =
            translator.require_capability(&ServerId::from("rust"), "renameProvider", |_| false);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_require_capability_ok_when_capability_present() {
        let translator = Translator::new();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::OneOf::Left(true)),
            ..Default::default()
        };
        translator.register_server(server_id.clone(), LspServer::new_for_test(caps));

        let result = translator.require_capability(&server_id, "renameProvider", |c| {
            matches!(
                c.rename_provider,
                Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
            )
        });
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_require_capability_err_when_capability_absent() {
        let translator = Translator::new();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities::default();
        translator.register_server(server_id.clone(), LspServer::new_for_test(caps));

        let result = translator.require_capability(&server_id, "renameProvider", |c| {
            matches!(
                c.rename_provider,
                Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
            )
        });
        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "renameProvider",
                ..
            })
        ));
    }

    /// Builds a single-server translator routed to `server_id` for every tool,
    /// with a registered `LspServer` fixture carrying `capabilities` (default
    /// capabilities advertise nothing).
    fn translator_with_capabilities(
        dir: &TempDir,
        server_id: &ServerId,
        capabilities: lsp_types::ServerCapabilities,
    ) -> (Translator, FakeServer) {
        let mut extensions = HashMap::new();
        extensions.insert("rs".to_string(), "rust".to_string());

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([(
                    server_id.clone(),
                    "rust".to_string(),
                )]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

        let (client, server) = fake_lsp_client();
        translator.register_client(server_id.clone(), client);
        translator.register_server(server_id.clone(), LspServer::new_for_test(capabilities));

        (translator, server)
    }

    #[tokio::test]
    async fn test_handle_rename_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_rename(
                path.to_string_lossy().to_string(),
                1,
                1,
                "renamed".to_string(),
            )
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "renameProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_code_actions_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(path.to_string_lossy().to_string(), 1, 1, 1, 5, None)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "codeActionProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_signature_help_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_signature_help(path.to_string_lossy().to_string(), 1, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "signatureHelpProvider",
                ..
            })
        ));
    }

    /// `handle_incoming_calls` resolves its server via `get_client_for_file`
    /// directly (not `prepare_document`), a separate code path from the other
    /// gated handlers -- exercise it explicitly.
    #[tokio::test]
    async fn test_handle_incoming_calls_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let uri = Url::from_file_path(&path).unwrap().to_string();

        let item = serde_json::json!({
            "name": "test_function",
            "kind": 12,
            "uri": uri,
            "range": {
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 10}
            },
            "selectionRange": {
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 10}
            }
        });

        let result = translator.handle_incoming_calls(item).await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "callHierarchyProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_outgoing_calls_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let uri = Url::from_file_path(&path).unwrap().to_string();

        let item = serde_json::json!({
            "name": "test_function",
            "kind": 12,
            "uri": uri,
            "range": {
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 10}
            },
            "selectionRange": {
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 10}
            }
        });

        let result = translator.handle_outgoing_calls(item).await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "callHierarchyProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_format_document_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_format_document(path.to_string_lossy().to_string(), 4, true)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "documentFormattingProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_call_hierarchy_prepare_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_call_hierarchy_prepare(path.to_string_lossy().to_string(), 1, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "callHierarchyProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_inlay_hints_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_inlay_hints(path.to_string_lossy().to_string(), 1, 1, 10, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "inlayHintProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_hover_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_hover(path.to_string_lossy().to_string(), 1, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "hoverProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_definition_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_definition(path.to_string_lossy().to_string(), 1, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "definitionProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_references_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_references(path.to_string_lossy().to_string(), 1, 1, false)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "referencesProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_completions_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_completions(path.to_string_lossy().to_string(), 1, 1, None)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "completionProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_document_symbols_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_document_symbols(path.to_string_lossy().to_string())
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "documentSymbolProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_workspace_symbol_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let result = translator
            .handle_workspace_symbol("main".to_string(), None, 100)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "workspaceSymbolProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_implementation_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_implementation(path.to_string_lossy().to_string(), 1, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "implementationProvider",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn test_handle_type_definition_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_type_definition(path.to_string_lossy().to_string(), 1, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "typeDefinitionProvider",
                ..
            })
        ));
    }

    /// Explicit `Some(OneOf::Left(false))` -- as distinct from an absent
    /// (`None`) field -- must also be rejected: some servers advertise a
    /// provider field with an explicit `false` rather than omitting it.
    #[tokio::test]
    async fn test_require_capability_err_when_capability_explicitly_false() {
        let translator = Translator::new();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::OneOf::Left(false)),
            ..Default::default()
        };
        translator.register_server(server_id.clone(), LspServer::new_for_test(caps));

        let result = translator.require_capability(&server_id, "renameProvider", |c| {
            matches!(
                c.rename_provider,
                Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
            )
        });
        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "renameProvider",
                ..
            })
        ));
    }

    /// Positive path: when the routed server *does* advertise the gated
    /// capability, the gate must let the request proceed into dispatch rather
    /// than short-circuiting with `CapabilityNotSupported`. Drives the fake
    /// wire to answer the request so the call completes quickly instead of
    /// idling out its internal 30s request timeout.
    #[tokio::test]
    async fn test_handle_rename_proceeds_when_capability_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::OneOf::Left(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_rename(path_str, 1, 1, "renamed".to_string())
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let rename_request = read_framed_message(&mut wire).await;
        assert_eq!(rename_request["method"], "textDocument/rename");
        write_response(
            &mut server.read_half_stdin,
            &rename_request["id"],
            JsonValue::Null,
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap();

        assert!(
            !matches!(result, Err(Error::CapabilityNotSupported { .. })),
            "capability is supported, gate must not block dispatch, got {result:?}"
        );
        assert!(
            result.is_ok(),
            "fake server answered, expected Ok: {result:?}"
        );
    }
}
