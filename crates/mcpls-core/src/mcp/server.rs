//! MCP server implementation using rmcp.
//!
//! This module provides the MCP server that exposes LSP capabilities
//! as MCP tools using the rmcp SDK.

use std::borrow::Cow;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Json;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ErrorCode, Implementation, ListResourcesResult,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ResourceUpdatedNotificationParam, ServerCapabilities,
    ServerConfig as RmcpServerConfig, SubscribeRequestParams, SubscriptionFilter, ToolAnnotations,
    UnsubscribeRequestParams,
};
use rmcp::service::SubscriptionContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::Mutex;

use super::handlers::BridgeContext;
use super::parameters::Parameters;
use super::schema_shape::shape_tool_schemas;
use super::session::{ListenPermit, ListenRegistration, ListenUris, SubscriptionRegistry, Target};
use super::tool_support::{McpTool, ToolSupportReport, prefixed_tool_name};
use super::tools::{
    CachedDiagnosticsParams, CallHierarchyCallsParams, CodeActionsParams, CompletionsParams,
    DeclarationParams, DiagnosticsParams, DocumentSymbolsParams, FoldingRangesParams,
    FormatDocumentParams, FormatRangeParams, InlayHintsParams, NavigationParams, PositionParams,
    RangeParams, ReferencesParams, RenameParams, RestartServerParams, ServerLogsParams,
    ServerMessagesParams, SymbolTargetInput, SymbolTargetParams, ToolSupportParams,
    TypeHierarchyWalkParams, WorkspaceSymbolParams,
};
use crate::bridge::resources::{
    DiagnosticsResourceUri, MAX_SUBSCRIPTIONS, ResolvedResource, make_uri, parse_uri,
};
use crate::bridge::{
    AddressableTool, Addressed, BoundedRange, CallHierarchyPrepareResult, CheckedHierarchyItem,
    ClientPath, CodeActionsResult, CompletionsResult, DefinitionResult, DiagnosticInfo,
    DiagnosticsAvailability, DiagnosticsOrigin, DiagnosticsResult, DocumentDiagnosticsResult,
    DocumentHighlightsResult, DocumentSymbolsResult, DocumentVersion, FoldingRangesResult,
    FormatDocumentResult, HierarchyItem, HoverResult, IncomingCallsResult, Indexed,
    InlayHintsResult, KindFilter, KindFilterField, KindFilterInput, LocationsResult,
    NotificationCache, OutgoingCallsResult, Position, PositionEncoding, PositionRange,
    PrepareRenameResult, ReferencesResult, RenameResult, RestartServerResult, RouteSignals,
    SelectionRangesResult, ServerLogsResult, ServerMessagesResult, SignatureHelpResult,
    SymbolTarget, Translator, TypeHierarchyResult, WorkspaceRoots, WorkspaceSymbolResult,
};
use crate::config::{McpConfig, ProjectConfigStatus, ToolPrefix};
use crate::redaction::{Redactions, ServerText};

/// Built-in `serverInfo.title`, used when `[mcp].title` is not configured.
const DEFAULT_SERVER_TITLE: &str = "MCPLS - MCP to LSP Bridge";

/// Built-in `serverInfo.description`, used when `[mcp].description` is not
/// configured. Sourced from the crate's own `Cargo.toml` `description` field.
const DEFAULT_SERVER_DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");

/// Built-in `RmcpServerConfig.instructions` capability blurb, used when
/// `[mcp].instructions` is not configured. A configured value replaces this
/// text entirely rather than appending to it -- see [`McpConfig::instructions`].
const DEFAULT_INSTRUCTIONS: &str = concat!(
    "Universal MCP to LSP bridge. Exposes Language Server Protocol ",
    "capabilities as MCP tools for semantic code intelligence. ",
    "Supports hover, definition, references, diagnostics, rename, ",
    "completions, symbols, and formatting."
);

/// Byte length of the longest tool name currently registered, used only to
/// keep [`crate::config::MAX_MCP_TOOL_PREFIX_BYTES`] safe (see the
/// compile-time assertion below). Derived from [`McpTool::ALL`], so a longer
/// tool name is picked up automatically; the assertion is what catches the
/// case where that growth would no longer leave enough room for the
/// configured prefix.
const MAX_TOOL_NAME_BYTES: usize = McpTool::MAX_NAME_BYTES;

/// The stricter of the two ceilings a joined `{prefix}_{tool_name}` must fit
/// under: not rmcp's own 128-byte `SHOULD`-level limit (see the second
/// assertion below), but the tighter pattern some LLM client APIs have
/// historically enforced for tool names (`^[a-zA-Z0-9_-]{1,64}$`). This is
/// the actual binding constraint [`crate::config::MAX_MCP_TOOL_PREFIX_BYTES`]'s
/// doc comment promises to stay under -- asserting against it, not just
/// rmcp's 128, is what makes that promise machine-checked.
const CLIENT_TOOL_NAME_BYTE_LIMIT: usize = 64;

/// Ties [`crate::config::MAX_MCP_TOOL_PREFIX_BYTES`] to the longest
/// currently-registered tool name and the stricter client-side tool name
/// length ceiling ([`CLIENT_TOOL_NAME_BYTE_LIMIT`]). If a future tool name
/// grows past what the current prefix budget allows, this fails to
/// *compile* against `MAX_TOOL_NAME_BYTES`, not silently or at runtime
/// against the user-facing, config-breaking `MAX_MCP_TOOL_PREFIX_BYTES`.
const _: () = assert!(
    crate::config::MAX_MCP_TOOL_PREFIX_BYTES + 1 + MAX_TOOL_NAME_BYTES
        <= CLIENT_TOOL_NAME_BYTE_LIMIT,
    "MAX_TOOL_NAME_BYTES has grown too large for the configured MAX_MCP_TOOL_PREFIX_BYTES \
     budget under the stricter client-side tool name length limit"
);

/// Secondary check against rmcp's own tool name length ceiling (128 bytes,
/// `SHOULD`-level per the MCP spec; see `tool_name_validation.rs` in the
/// pinned rmcp version). Kept alongside the stricter assertion above so a
/// future change to [`CLIENT_TOOL_NAME_BYTE_LIMIT`] can't accidentally drop
/// below what rmcp itself requires.
const _: () = assert!(
    crate::config::MAX_MCP_TOOL_PREFIX_BYTES + 1 + MAX_TOOL_NAME_BYTES <= 128,
    "MAX_TOOL_NAME_BYTES has grown too large for the configured MAX_MCP_TOOL_PREFIX_BYTES \
     budget under rmcp's 128-byte tool name limit"
);

/// Tool-description sentences shared by every tool carrying `positions_degraded`.
macro_rules! positions_note_request {
    () => {
        "`positions_degraded` (non-UTF-16 servers only): `\"request\"` means the queried position was sent unconverted, so the result may describe a different symbol and should not be trusted; `\"response\"` means only returned `character` offsets may be inexact."
    };
}

/// Tool-description sentence for the tools that accept a symbol name.
macro_rules! name_addressing_note {
    () => {
        "Aim it with `line` + `character`, or with `symbol_name` (optionally narrowed by `symbol_kind` and `container`) for a symbol defined in the file: the result then carries `resolved_symbol` with the position that was queried. A name matching several symbols, none, or an unverifiable position fails with the candidates or the reason instead of guessing."
    };
}

/// Tool-description sentence for tools accepting `context: \"enclosing_symbol\"`.
macro_rules! enclosing_symbol_note {
    () => {
        "Pass `context: \"enclosing_symbol\"` to attach `enclosing_symbol` to each item: `status` `resolved` (with `name_path`, `kind`, `range`, `fidelity`), `top_level` (no symbol contains it), `not_computed` or `unavailable` (with a `reason`; nothing is known, never read as top level). Costs one documentSymbol request per distinct file, capped per call; `enrichment` reports files enriched or skipped and `cut_short`."
    };
}

/// Tool-description sentence for the tools that report `indexing_in_progress`.
macro_rules! indexing_note {
    () => {
        "`indexing_in_progress: true`: the routed server was still indexing, so an empty or partial result may be incomplete."
    };
}

/// Tool-description sentences for the diagnostics tools that report `availability`.
macro_rules! availability_note {
    () => {
        "`availability`: `published` (an empty list means clean), `pending` (nothing published yet) or `evicted` (a publish was dropped); an empty list next to `pending` or `evicted` is not clean."
    };
}

/// Tool-description sentence for the tools whose results flag `out_of_workspace`.
macro_rules! out_of_workspace_note {
    () => {
        "`out_of_workspace: true` means not provably inside a configured workspace root (lexical, advisory)."
    };
}

macro_rules! positions_note_response {
    () => {
        "`positions_degraded: \"response\"` (non-UTF-16 servers only) means some returned `character` offsets may be inexact."
    };
}

/// Response shape for the `get_cached_diagnostics` tool: the shared diagnostics result plus
/// the file's [`RouteSignals`].
#[derive(serde::Serialize, JsonSchema)]
struct CachedDiagnosticsResponse {
    #[serde(flatten)]
    result: DiagnosticsResult,
    /// Whether the cache has an answer for the file; an empty list next to
    /// `pending` or `evicted` is not a clean file.
    availability: DiagnosticsAvailability,
    #[serde(flatten)]
    signals: RouteSignals,
}

/// MCP server that exposes LSP capabilities as tools.
///
/// Deliberately not `Clone` (#478): each HTTP session must get its own
/// subscription state via [`Self::for_new_session`], not a shared instance a
/// stray `.clone()` could hand to two sessions at once. `for_new_session`
/// builds a new value field by field instead (each field an `Arc` bump except
/// `session`), so this costs nothing at the one production call site
/// (`transport::run_http`'s per-session factory closure).
pub struct McplsServer {
    context: Arc<BridgeContext>,

    /// `Arc`-wrapped so building a new session in `for_new_session` is a
    /// cheap `Arc` bump rather than a deep clone of every registered
    /// `ToolRoute`. `#[tool_handler(router = self.tool_router)]`'s generated
    /// `self.tool_router.call(..)` auto-derefs through the `Arc`, so this is
    /// transparent to the macro-generated code.
    tool_router: Arc<ToolRouter<Self>>,
}

/// Maps an [`crate::error::Error`] onto the wire-level MCP error, via
/// [`crate::error::Error::mcp_error_kind`]'s classification: caller-fault
/// variants become `INVALID_PARAMS`, retryable variants (e.g.
/// `WorkspaceIndexing`, `ServerInitializing`) get their own bespoke code plus
/// a structured `data` payload, and everything else falls back to
/// `INTERNAL_ERROR`.
#[expect(
    clippy::needless_pass_by_value,
    reason = "by-value `e` lets this be passed straight to `Result::map_err`"
)]
fn map_bridge_error(e: crate::error::Error) -> McpError {
    let message = e.to_string();
    match e.mcp_error_kind() {
        crate::error::McpErrorKind::InvalidParams => McpError::invalid_params(message, None),
        crate::error::McpErrorKind::InvalidPosition(raw) => {
            error_with_data(ErrorCode::INVALID_PARAMS, message, &raw)
        }
        crate::error::McpErrorKind::SymbolResolution(data) => {
            error_with_data(ErrorCode::INVALID_PARAMS, message, &data)
        }
        crate::error::McpErrorKind::Internal => McpError::internal_error(message, None),
        crate::error::McpErrorKind::Retryable(data) => {
            error_with_data(ErrorCode(data.code()), message, &data)
        }
    }
}

/// [`map_bridge_error`], with every configured secret hidden in the message and
/// in `data` (a rewritten server error carries the server's raw message).
///
/// The one funnel for errors that can embed server text: a tool error, a
/// resource error carrying a spawn failure's stderr, a listen failure. An
/// error built from client input only keeps the plain [`map_bridge_error`].
fn render_error(error: crate::error::Error, redactions: &Redactions) -> McpError {
    let mut mapped = map_bridge_error(error);
    if redactions.is_empty() {
        return mapped;
    }
    if let Cow::Owned(message) = redactions.apply(&mapped.message) {
        mapped.message = Cow::Owned(message);
    }
    if let Some(data) = &mut mapped.data {
        redactions.redact_json(data);
    }
    mapped
}

/// Maps a typed client-input error to its JSON-RPC error, so every parse helper
/// below reports a bad value the same way.
fn client_input_error(error: impl Into<crate::error::Error>) -> McpError {
    map_bridge_error(error.into())
}

/// Parses a client-supplied `file_path` at the tool boundary. Done in the
/// tool method rather than while deserializing the parameters, because the MCP
/// layer reports a deserialization failure as a tool-result error instead of
/// a JSON-RPC `-32602`.
fn parse_client_path(path: PathBuf) -> Result<ClientPath, McpError> {
    ClientPath::try_from(path).map_err(client_input_error)
}

/// Parses the file path and target of an addressed tool; a bad path or
/// position is `-32602`.
fn parse_target(
    file_path: PathBuf,
    target: SymbolTargetInput,
) -> Result<(ClientPath, SymbolTarget), McpError> {
    let target = target.into_target().map_err(client_input_error)?;
    Ok((parse_client_path(file_path)?, target))
}

/// Parses a client-supplied 1-based position, so a bad value is `-32602`.
fn parse_position(line: u32, character: u32) -> Result<Position, McpError> {
    Position::from_client(line, character).map_err(client_input_error)
}

/// Resolves a kind input to its typed kind, so an unknown kind is `-32602`.
fn parse_kind<K: KindFilter>(
    input: KindFilterInput<K>,
    field: KindFilterField,
) -> Result<K, McpError> {
    input.into_known(field).map_err(client_input_error)
}

/// Resolves an optional `kind_filter` input; absent stays absent.
fn parse_kind_filter<K: KindFilter>(
    input: Option<KindFilterInput<K>>,
    field: KindFilterField,
) -> Result<Option<K>, McpError> {
    input.map(|input| parse_kind(input, field)).transpose()
}

/// Parses a client-supplied hierarchy item, so a bad range is `-32602`.
fn parse_hierarchy_item(item: HierarchyItem) -> Result<CheckedHierarchyItem, McpError> {
    CheckedHierarchyItem::from_client(item).map_err(client_input_error)
}

/// Parses a client-supplied ordered range.
fn parse_range(range: &RangeParams) -> Result<PositionRange, McpError> {
    PositionRange::from_client(
        (range.start_line, range.start_character),
        (range.end_line, range.end_character),
    )
    .map_err(client_input_error)
}

/// Parses a client-supplied ordered range of at most `MAX_RANGE_LINES` lines.
fn parse_bounded_range(range: &RangeParams) -> Result<BoundedRange, McpError> {
    BoundedRange::try_from(parse_range(range)?).map_err(client_input_error)
}

/// Builds an error with `data` as its payload; a failed serialization is
/// logged and the error is sent without `data` rather than lost.
fn error_with_data(code: ErrorCode, message: String, data: &impl Serialize) -> McpError {
    match serde_json::to_value(data) {
        Ok(value) => McpError::new(code, message, Some(value)),
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize error data");
            McpError::new(code, message, None)
        }
    }
}

/// Map a bridge-layer result to a structured MCP tool response (`structuredContent` plus the
/// legacy `content` text block, per the MCP spec's backwards-compat shape).
///
/// The handler's own return type -- not this helper's -- is what the `#[tool]` macro reads to
/// derive `outputSchema`; it must spell `Result<Json<T>, McpError>` literally (no alias) for the
/// macro to detect it. See `Json<T>`'s `IntoCallToolResult` impl, which this helper relies on.
///
/// Server-supplied display prose in the value is redacted with `redactions` first; see
/// [`ServerText`] for what counts as prose.
fn to_structured_tool_result<T: Serialize + JsonSchema + ServerText>(
    result: crate::error::Result<T>,
    redactions: &Redactions,
) -> Result<Json<T>, McpError> {
    match result {
        Ok(mut value) => {
            value.redact_server_text(redactions);
            Ok(Json(value))
        }
        Err(e) => Err(render_error(e, redactions)),
    }
}

/// Fixed page size for `list_resources` pagination.
///
/// `DocumentTracker`'s configured `max_documents` (0 = unlimited) isn't
/// reachable from here -- it's private to the tracker, and `0` means the
/// document count itself is unbounded anyway -- so this is an independent
/// page-size ceiling, large enough to rarely trigger for typical workspaces
/// but small enough to stay well under stdio transport buffer limits.
const RESOURCE_PAGE_SIZE: usize = 100;

/// Slice `paths` into the page starting at the position `cursor` resumes
/// from, returning the page and the cursor for the next page (`None` once
/// the last page is reached).
///
/// `paths` must already be sorted: the caller's source
/// (`open_document_paths()`) is backed by a `HashMap` with no ordering
/// guarantee, and a stable order is required for a cursor to resume at a
/// reproducible position across calls. The cursor is an index into that
/// order, not a document identity: if a document closes at an index below
/// the cursor between two calls, every later entry shifts down one and the
/// next page silently skips the entry that moved into the cursor's old
/// slot. Low-impact for this use case (a stdio single-session server), but
/// callers pairing pagination with concurrent document open/close should be
/// aware a page can miss an entry rather than duplicate one.
///
/// # Errors
///
/// Returns an error only if `cursor` fails to parse as a `usize`. Any
/// parseable value is accepted as a page-start index, including one that
/// isn't page-aligned (not a value this function itself ever returns via
/// `next_cursor`) or is out of range (e.g. documents were closed between
/// calls) -- an out-of-range cursor is not an error, it yields an empty
/// final page.
fn paginate_resource_paths<'a>(
    paths: &'a [PathBuf],
    cursor: Option<&str>,
    page_size: usize,
) -> Result<(&'a [PathBuf], Option<String>), McpError> {
    debug_assert!(
        page_size > 0,
        "page_size must be non-zero, or next_cursor never advances"
    );

    let start = match cursor {
        Some(c) => c.parse::<usize>().map_err(|_| {
            McpError::invalid_params(format!("invalid pagination cursor: {c}"), None)
        })?,
        None => 0,
    };

    let rest = paths.get(start..).unwrap_or_default();
    let page = rest.get(..rest.len().min(page_size)).unwrap_or_default();
    // `start` is client-controlled (parsed straight from the cursor), so the
    // addition must not panic (debug) or silently wrap (release) for a
    // cursor near `usize::MAX`.
    let next_start = start.saturating_add(page_size);
    let next_cursor = (next_start < paths.len()).then(|| next_start.to_string());

    Ok((page, next_cursor))
}

/// Moves the indexing signal a handler sampled outside the `resolved_symbol`
/// that name addressing wraps around it.
fn hoist_indexing<T>(addressed: Addressed<Indexed<T>>) -> Indexed<Addressed<T>> {
    let Addressed {
        result,
        resolved_symbol,
    } = addressed;
    Indexed::new(
        Addressed {
            result: result.result,
            resolved_symbol,
        },
        result.indexing,
    )
}

/// `get_diagnostics`'s response shape.
///
/// Wraps `DiagnosticsResult` with the route's [`RouteSignals`]:
/// `handle_diagnostics` deliberately stays ungated on workspace-indexing readiness (#445 -- it
/// reads from the notification-cache poll path, not a live whole-workspace LSP request, so
/// blocking it the way `IndexingGate::Required` blocks hover/definition/etc. would be the
/// wrong fix shape for this one call site; see `routing::IndexingGate`'s doc). Instead this
/// flags the result rather than silently returning what can read as "no errors" while the
/// routed server is still loading.
#[derive(serde::Serialize, JsonSchema)]
struct DiagnosticsResponse {
    #[serde(flatten)]
    result: DocumentDiagnosticsResult,
    /// Whether the cache has an answer for the file; an empty list next to
    /// `pending` or `evicted` is not a clean file.
    availability: DiagnosticsAvailability,
    /// `pull` when a `textDocument/diagnostic` request answered (merged with
    /// the push cache), `push_cache` when the server has no pull provider and
    /// the push cache alone answered, `cache_after_failed_pull` when the pull
    /// failed and the cache answered.
    origin: DiagnosticsOrigin,
    #[serde(flatten)]
    signals: RouteSignals,
}

/// `read_resource`'s diagnostics payload, distinguishing a file mcpls has no
/// information about (`tracked: false`, always paired with empty
/// `diagnostics`) from one it does -- whether because the file is currently
/// open via `DocumentTracker`, or an LSP server has published diagnostics
/// for it regardless of open state (`tracked: true`; `diagnostics: []` if
/// clean or not yet analyzed).
///
/// `version` is the document version the diagnostics were computed against
/// (the client's staleness signal, mirroring `DiagnosticInfo::version`) --
/// `None` both when untracked and when tracked but nothing has been
/// published yet. `uri` is deliberately omitted: the caller already knows it
/// (it's the resource they requested).
///
/// The shared [`RouteSignals`] mirror `get_cached_diagnostics`
/// (#359): a push-degraded route means `subscribe`'s replay and the pump's
/// `notify_resource_updated` calls for it have gone dark until the whole
/// mcpls process restarts, same as this cache-only read.
#[derive(serde::Serialize)]
struct ResourceDiagnosticsResponse {
    tracked: bool,
    version: Option<DocumentVersion>,
    diagnostics: Vec<lsp_types::Diagnostic>,
    availability: DiagnosticsAvailability,
    #[serde(flatten)]
    signals: RouteSignals,
}

/// Whether a file is currently open through `DocumentTracker`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocumentState {
    Open,
    NotOpen,
}

impl DocumentState {
    const fn of(open: bool) -> Self {
        if open { Self::Open } else { Self::NotOpen }
    }
}

impl ResourceDiagnosticsResponse {
    /// Builds `read_resource`'s response for a file. `tracked` is true when the
    /// file is open (`document`) *or* the diagnostics cache already holds an
    /// entry for it (`entry.is_some()`) -- not `document` alone: an LSP server
    /// publishes `textDocument/publishDiagnostics` for whatever it analyzes,
    /// including files mcpls never explicitly opened (e.g. one rust-analyzer
    /// pulls in transitively), so `document` alone could report
    /// `tracked: false` while `diagnostics` is still non-empty, contradicting
    /// the documented "untracked implies empty diagnostics" contract.
    fn new(
        document: DocumentState,
        entry: Option<&DiagnosticInfo>,
        availability: DiagnosticsAvailability,
        signals: RouteSignals,
    ) -> Self {
        Self {
            tracked: document == DocumentState::Open || entry.is_some(),
            version: entry.and_then(|e| e.version),
            diagnostics: entry.map_or_default(|e| e.diagnostics.clone()),
            availability,
            signals,
        }
    }
}

/// One read of the diagnostics cache for a file, taken under a single lock so
/// the tool and the resource report the same state of the same file.
struct DiagnosticsSnapshot {
    sources: crate::bridge::DiagnosticSources,
    availability: DiagnosticsAvailability,
    signals: RouteSignals,
    /// The server that published the cached diagnostics, if any.
    owner: Option<crate::config::ServerId>,
}

// Diagnostics were redacted when they entered the cache or the pull path.
impl ServerText for CachedDiagnosticsResponse {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            result,
            availability: _,
            signals: _,
        } = self;
        result.redact_server_text(redactions);
    }
}

impl ServerText for DiagnosticsResponse {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            result,
            availability: _,
            origin: _,
            signals: _,
        } = self;
        result.redact_server_text(redactions);
    }
}

#[tool_router(router = declared_tool_router)]
impl McplsServer {
    /// Create a new MCP server with the given translator, notification cache,
    /// workspace roots, and subscription registry.
    ///
    /// This instance starts with an empty subscription set and joins
    /// `subscription_registry` on its first subscribe -- see
    /// [`Self::for_new_session`] for how per-HTTP-session isolation builds on
    /// top of that.
    ///
    /// `project_config_status` reports whether a CWD-discovered
    /// `./mcpls.toml` was skipped as untrusted when the active config was
    /// loaded (see [`ServerConfig::project_config_status`](crate::config::ServerConfig::project_config_status));
    /// `get_info` surfaces it in [`RmcpServerConfig::instructions`]. `mcp` carries
    /// the configured `[mcp]` presentation overrides (see
    /// [`crate::config::McpConfig`]), also read by `get_info`.
    #[must_use]
    pub fn new(
        translator: Arc<Translator>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        workspace_roots: WorkspaceRoots,
        subscription_registry: SubscriptionRegistry,
        project_config_status: ProjectConfigStatus,
        mcp: McpConfig,
    ) -> Self {
        let tool_router = Arc::new(Self::build_tool_router(mcp.tool_prefix.as_ref()));
        let context = Arc::new(BridgeContext::new(
            translator,
            notification_cache,
            workspace_roots,
            subscription_registry,
            project_config_status,
            mcp,
        ));
        Self {
            context,
            tool_router,
        }
    }

    /// Build a new server instance for a new HTTP session, giving it its own
    /// isolated subscription state that joins the same [`SubscriptionRegistry`]
    /// on its first subscribe.
    ///
    /// Every other piece of shared state (translator, notification cache,
    /// workspace roots, config) is shared with the original via a cheap `Arc`
    /// clone -- only the subscription state is fresh. A new `BridgeContext`
    /// field must be listed here; see that type's docs for the state rule.
    /// Called from the HTTP transport's service factory (see
    /// `transport::run_http`); stdio never calls this, since it only ever
    /// serves the one session `McplsServer::new` already built.
    ///
    /// "Per session" narrows to "per request" on rmcp's stateless HTTP path;
    /// such an instance is rejected by `subscribe`/`unsubscribe` and so never
    /// registers.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use mcpls_core::bridge::{NotificationCache, Translator, WorkspaceRoots};
    /// use mcpls_core::config::{McpConfig, ProjectConfigStatus};
    /// use mcpls_core::mcp::{McplsServer, SubscriptionRegistry};
    /// use tokio::sync::Mutex;
    ///
    /// let server = McplsServer::new(
    ///     Arc::new(Translator::new()),
    ///     Arc::new(Mutex::new(NotificationCache::new())),
    ///     WorkspaceRoots::default(),
    ///     SubscriptionRegistry::new(),
    ///     ProjectConfigStatus::NotIgnored,
    ///     McpConfig::default(),
    /// );
    /// // One `McplsServer` clone per HTTP session; each gets isolated resource
    /// // subscriptions while still sharing the same LSP-facing state.
    /// let _session = server.for_new_session();
    /// ```
    #[must_use]
    pub fn for_new_session(&self) -> Self {
        let context = Arc::new(BridgeContext {
            translator: Arc::clone(&self.context.translator),
            notification_cache: Arc::clone(&self.context.notification_cache),
            workspace_roots: self.context.workspace_roots.clone(),
            session: self.context.session.sibling(),
            project_config_status: self.context.project_config_status,
            mcp: self.context.mcp.clone(),
        });
        Self {
            context,
            tool_router: Arc::clone(&self.tool_router),
        }
    }

    /// This instance's [`SubscriptionRegistry`], so a test exercising the
    /// real HTTP factory/session-close path (`transport.rs`'s integration
    /// tests) can assert on registry state without reaching into private
    /// `BridgeContext` fields cross-module.
    #[cfg(test)]
    pub(crate) fn subscription_registry(&self) -> SubscriptionRegistry {
        self.context.session.registry()
    }

    /// The declared tools with the read-only annotation default applied and
    /// their schemas exactly as the generator produced them.
    ///
    /// [`Self::build_tool_router`] shapes these schemas; this stays separate so
    /// the schema-shaping tests can compare the two.
    pub(super) fn unshaped_tool_router() -> ToolRouter<Self> {
        let mut router = Self::declared_tool_router();
        for route in router.map.values_mut() {
            let title = route.attr.title.clone();
            route.attr.annotations.get_or_insert_with(|| {
                ToolAnnotations::from_raw(title, Some(true), Some(false), Some(true), None)
            });
        }
        router
    }

    /// Router for every MCP tool, with the read-only classification applied
    /// and, when `prefix` is configured, every tool name rewritten to
    /// `{prefix}_{name}`.
    ///
    /// Every mcpls tool is a read-only LSP query: `rename_symbol`,
    /// `format_document` and `get_code_actions` return a *proposed*
    /// `WorkspaceEdit` and never write to disk. Applying that once here
    /// replaces an identical `annotations(...)` block on every `#[tool]`
    /// attributes. A tool declaring its own annotations keeps them;
    /// `test_tool_annotation_classifications_match_intent` forces a future
    /// mutating tool to write down an explicit classification rather than
    /// inherit this default silently.
    ///
    /// With `prefix: None` this returns byte-for-byte what it always has --
    /// `test_tool_surface_matches_golden_snapshot` pins that as the
    /// backward-compatibility guarantee for the default, unprefixed surface.
    ///
    /// The rename re-keys `router.map` via [`ToolRouter::add_route`] rather
    /// than inserting directly, so it goes through rmcp's own
    /// `validate_and_warn_tool_name` for defence-in-depth. It does **not**
    /// re-key `ToolRouter`'s private `disabled` set -- inert today (mcpls
    /// never calls `disable_route`/`with_disabled`, pinned by
    /// `test_no_route_is_ever_disabled` below), but a latent bug the moment
    /// that changes: any future `disable_route` call must name the
    /// already-prefixed tool name and must run strictly after this rename.
    pub(super) fn build_tool_router(prefix: Option<&ToolPrefix>) -> ToolRouter<Self> {
        let mut router = Self::unshaped_tool_router();
        shape_tool_schemas(router.map.values_mut().map(|route| &mut route.attr));
        if let Some(prefix) = prefix {
            debug_assert!(router.map.keys().all(|name| router.has_route(name)));
            let unprefixed = std::mem::take(&mut router.map);
            let entry_count = unprefixed.len();
            for (_, mut route) in unprefixed {
                route.attr.name = Cow::Owned(prefixed_tool_name(Some(prefix), &route.attr.name));
                router.add_route(route);
            }
            debug_assert_eq!(router.map.len(), entry_count);
        }
        router
    }

    /// Get hover information at a position in a file.
    #[tool(
        description = concat!("Type and documentation info for a symbol. Returns signatures, docs, and inferred types. ", name_addressing_note!(), " ", positions_note_request!()),
        title = "Hover"
    )]
    async fn get_hover(
        &self,
        Parameters(SymbolTargetParams { file_path, target }): Parameters<SymbolTargetParams>,
    ) -> Result<Json<Addressed<HoverResult>>, McpError> {
        let (file_path, target) = parse_target(file_path, target)?;
        let translator = &self.context.translator;
        self.structured_result(
            translator
                .with_resolved_target(
                    file_path,
                    target,
                    AddressableTool::Hover,
                    |file_path, position| translator.handle_hover(file_path, position),
                )
                .await,
        )
    }

    /// Get the definition location of a symbol.
    #[tool(
        description = concat!("Definition location of a symbol. Returns file path, line, and character where declared. Capped at a fixed maximum for a pathological case; `truncated: true` on the result means more locations exist than are returned. ", name_addressing_note!(), " ", positions_note_request!(), " ", enclosing_symbol_note!(), " ", out_of_workspace_note!()),
        title = "Go to Definition"
    )]
    async fn get_definition(
        &self,
        Parameters(NavigationParams {
            target: SymbolTargetParams { file_path, target },
            context,
        }): Parameters<NavigationParams>,
    ) -> Result<Json<Addressed<DefinitionResult>>, McpError> {
        let (file_path, target) = parse_target(file_path, target)?;
        let translator = &self.context.translator;
        self.structured_result(
            translator
                .with_resolved_target(
                    file_path,
                    target,
                    AddressableTool::Definition,
                    |file_path, position| {
                        translator.handle_definition(file_path, position, context)
                    },
                )
                .await,
        )
    }

    /// Find all references to a symbol.
    #[tool(
        description = concat!("References to a symbol, across workspace. Capped at a fixed maximum for an extremely common symbol; `truncated: true` on the result means more references exist than are returned. ", name_addressing_note!(), " ", positions_note_request!(), " ", enclosing_symbol_note!(), " ", out_of_workspace_note!()),
        title = "Find References"
    )]
    async fn get_references(
        &self,
        Parameters(ReferencesParams {
            target: SymbolTargetParams { file_path, target },
            include_declaration,
            context,
        }): Parameters<ReferencesParams>,
    ) -> Result<Json<Addressed<ReferencesResult>>, McpError> {
        let (file_path, target) = parse_target(file_path, target)?;
        let translator = &self.context.translator;
        self.structured_result(
            translator
                .with_resolved_target(
                    file_path,
                    target,
                    AddressableTool::References,
                    |file_path, position| {
                        translator.handle_references(
                            file_path,
                            position,
                            include_declaration,
                            context,
                        )
                    },
                )
                .await,
        )
    }

    /// Get diagnostics for a file.
    #[tool(
        description = concat!("Diagnostics for a file. Returns errors, warnings, and hints with severity and location. `origin`: `pull`, `push_cache` when the server has no pull provider, or `cache_after_failed_pull` when the pull failed and the cache answered. ", availability_note!(), " `indexing_in_progress: true` means the routed server was indexing at some point during this read, so results may be incomplete. `push_notifications_degraded: true` means the routed server crashed and was restarted, so push-only diagnostics (e.g. flycheck) are missing. ", positions_note_response!(), " ", enclosing_symbol_note!(), " ", out_of_workspace_note!()),
        title = "Diagnostics"
    )]
    async fn get_diagnostics(
        &self,
        Parameters(DiagnosticsParams { file_path, context }): Parameters<DiagnosticsParams>,
    ) -> Result<Json<DiagnosticsResponse>, McpError> {
        let file_path = parse_client_path(file_path)?;
        // Resolved from the validated/canonicalized path (mirrors
        // `read_resource`), not the raw client-supplied path: a symlink
        // whose extension differs from its target must route to the same
        // language `handle_diagnostics`'s own validation resolves, or this
        // could observe the wrong server's (or no server's) indexing state.
        // Best-effort (`.ok()`): an invalid/out-of-workspace path just reads
        // `false` here and fails properly inside `handle_diagnostics` below,
        // and a failed-to-start server's `ServerFailedToStart` is likewise
        // reported there by the pull request itself.
        let validated = self.context.translator.validate_path(&file_path).await;
        let route_id = validated.as_ref().ok().and_then(|validated_path| {
            self.context
                .translator
                .diagnostics_route_for_path(validated_path.as_path())
                .server_id()
                .cloned()
        });

        // Sampled before and after the pull: indexing may finish, or a respawn may mark push-degraded, mid-pull.
        let before = {
            let cache = self.context.notification_cache.lock().await;
            RouteSignals::sample(&cache, route_id.as_ref())
        };

        // Merging push-model (flycheck/clippy) diagnostics into the pull
        // result, including the pull-error-but-cache-has-data fallback, is
        // handled inside handle_diagnostics itself -- see its doc comment.
        let result = async {
            let path = validated?;
            self.context
                .translator
                .handle_validated_diagnostics(&path, context, &self.context.notification_cache)
                .await
        }
        .await;

        let after = {
            let cache = self.context.notification_cache.lock().await;
            RouteSignals::sample(&cache, route_id.as_ref())
        };
        let signals = before.union(after);

        self.structured_result(result.map(|answer| DiagnosticsResponse {
            result: answer.result,
            availability: answer.availability,
            origin: answer.origin,
            signals,
        }))
    }

    /// Rename a symbol across the workspace.
    // read-only: returns a proposed WorkspaceEdit, does not apply it -- mcpls
    // has no write-back path today; revisit if that changes.
    #[tool(
        description = concat!("Rename symbol across workspace. Returns text edits for all files where symbol is used. A non-empty `dropped` field means some edits were withheld (e.g. out-of-workspace files, or `exceeds_item_cap` when a file's edits exceed the fixed maximum) -- the rename is then incomplete even though `changes` is non-empty. ", name_addressing_note!(), " An ambiguous name never produces an edit. ", positions_note_request!(), " ", out_of_workspace_note!()),
        title = "Rename Symbol"
    )]
    async fn rename_symbol(
        &self,
        Parameters(RenameParams {
            target: SymbolTargetParams { file_path, target },
            new_name,
        }): Parameters<RenameParams>,
    ) -> Result<Json<Addressed<RenameResult>>, McpError> {
        let (file_path, target) = parse_target(file_path, target)?;
        let translator = &self.context.translator;
        self.structured_result(
            translator
                .with_resolved_target(
                    file_path,
                    target,
                    AddressableTool::Rename,
                    |file_path, position| translator.handle_rename(file_path, position, new_name),
                )
                .await,
        )
    }

    /// Get code completion suggestions.
    #[tool(
        description = "Completion suggestions at position. Returns methods, functions, variables, types, and snippets. `positions_degraded: \"request\"` (non-UTF-16 servers only) means the queried position was sent unconverted, so the result may not match the position asked about and should not be trusted.",
        title = "Completions"
    )]
    async fn get_completions(
        &self,
        Parameters(CompletionsParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
            trigger,
        }): Parameters<CompletionsParams>,
    ) -> Result<Json<CompletionsResult>, McpError> {
        let file_path = parse_client_path(file_path)?;
        self.structured_result(
            self.context
                .translator
                .handle_completions(file_path, parse_position(line, character)?, trigger)
                .await,
        )
    }

    /// Get all symbols in a document.
    #[tool(
        description = concat!("Symbols in a file. Returns hierarchical outline with functions, classes, structs, and locations. ", positions_note_response!()),
        title = "Document Symbols"
    )]
    async fn get_document_symbols(
        &self,
        Parameters(DocumentSymbolsParams { file_path }): Parameters<DocumentSymbolsParams>,
    ) -> Result<Json<DocumentSymbolsResult>, McpError> {
        let file_path = parse_client_path(file_path)?;
        self.structured_result(
            self.context
                .translator
                .handle_document_symbols(file_path)
                .await,
        )
    }

    /// Format a document according to language server rules.
    // read-only: returns proposed text edits, does not apply them -- mcpls
    // has no write-back path today; revisit if that changes.
    #[tool(
        description = concat!("Format document with language-specific rules. Returns text edits for indentation, spacing, and style. ", positions_note_response!()),
        title = "Format Document"
    )]
    async fn format_document(
        &self,
        Parameters(FormatDocumentParams {
            file_path,
            tab_size,
            insert_spaces,
        }): Parameters<FormatDocumentParams>,
    ) -> Result<Json<FormatDocumentResult>, McpError> {
        let file_path = parse_client_path(file_path)?;
        self.structured_result(
            self.context
                .translator
                .handle_format_document(file_path, tab_size, insert_spaces)
                .await,
        )
    }

    /// Search for symbols across the workspace.
    #[tool(
        description = concat!("Search workspace symbols by name. Supports partial matching and fuzzy search. `limit` is capped at a fixed server-side maximum regardless of the value requested; `truncated: true` on the result means more matches exist than are returned. ", positions_note_response!(), " ", out_of_workspace_note!()),
        title = "Workspace Symbol Search"
    )]
    async fn workspace_symbol_search(
        &self,
        Parameters(WorkspaceSymbolParams {
            query,
            kind_filter,
            limit,
        }): Parameters<WorkspaceSymbolParams>,
    ) -> Result<Json<WorkspaceSymbolResult>, McpError> {
        let kind_filter = parse_kind_filter(kind_filter, KindFilterField::KindFilter)?;
        self.structured_result(
            self.context
                .translator
                .handle_workspace_symbol(query, kind_filter, limit)
                .await,
        )
    }

    /// Get code actions for a range.
    // read-only: returns proposed CodeAction edits, does not apply them --
    // mcpls has no write-back path today; revisit if that changes.
    #[tool(
        description = concat!("Code actions for range. Returns quick fixes, refactorings, and source actions with edits. Capped at a fixed maximum; `truncated: true` on the result means some actions, diagnostics, or edits were left out. An action's `edit.dropped` field, when non-empty, means some of that edit's changes were withheld (e.g. out-of-workspace files). The end line must exist in the file; a line past the end is rejected as invalid params. ", positions_note_request!(), " ", out_of_workspace_note!()),
        title = "Code Actions"
    )]
    async fn get_code_actions(
        &self,
        Parameters(CodeActionsParams {
            file_path,
            range,
            kind_filter,
        }): Parameters<CodeActionsParams>,
    ) -> Result<Json<CodeActionsResult>, McpError> {
        let file_path = parse_client_path(file_path)?;
        let kind_filter = parse_kind_filter(kind_filter, KindFilterField::KindFilter)?;
        self.structured_result(
            self.context
                .translator
                .handle_code_actions(file_path, parse_bounded_range(&range)?, kind_filter)
                .await,
        )
    }

    /// Prepare call hierarchy at a position.
    #[tool(
        description = concat!("Prepare call hierarchy for a symbol. Returns callable items for incoming/outgoing call analysis, capped at a fixed maximum; `truncated: true` on the result means more exist than are returned. ", name_addressing_note!(), " ", indexing_note!(), " ", positions_note_request!(), " ", out_of_workspace_note!()),
        title = "Prepare Call Hierarchy"
    )]
    async fn prepare_call_hierarchy(
        &self,
        Parameters(SymbolTargetParams { file_path, target }): Parameters<SymbolTargetParams>,
    ) -> Result<Json<Indexed<Addressed<CallHierarchyPrepareResult>>>, McpError> {
        let (file_path, target) = parse_target(file_path, target)?;
        let translator = &self.context.translator;
        self.structured_result(
            translator
                .with_resolved_target(
                    file_path,
                    target,
                    AddressableTool::PrepareCallHierarchy,
                    |file_path, position| {
                        translator.handle_call_hierarchy_prepare(file_path, position)
                    },
                )
                .await
                .map(hoist_indexing),
        )
    }

    /// Get incoming calls (callers).
    #[tool(
        description = concat!("Functions calling the specified item. Takes call hierarchy item, returns callers, capped at a fixed maximum; `truncated: true` on the result means more exist than are returned. ", positions_note_request!(), " ", out_of_workspace_note!()),
        title = "Incoming Calls"
    )]
    async fn get_incoming_calls(
        &self,
        Parameters(CallHierarchyCallsParams { item }): Parameters<CallHierarchyCallsParams>,
    ) -> Result<Json<IncomingCallsResult>, McpError> {
        self.structured_result(
            self.context
                .translator
                .handle_incoming_calls(parse_hierarchy_item(item)?)
                .await,
        )
    }

    /// Get outgoing calls (callees).
    #[tool(
        description = concat!("Functions called by the specified item. Takes call hierarchy item, returns callees, capped at a fixed maximum; `truncated: true` on the result means more exist than are returned. ", positions_note_request!(), " ", out_of_workspace_note!()),
        title = "Outgoing Calls"
    )]
    async fn get_outgoing_calls(
        &self,
        Parameters(CallHierarchyCallsParams { item }): Parameters<CallHierarchyCallsParams>,
    ) -> Result<Json<OutgoingCallsResult>, McpError> {
        self.structured_result(
            self.context
                .translator
                .handle_outgoing_calls(parse_hierarchy_item(item)?)
                .await,
        )
    }

    /// Prepare type hierarchy at a position.
    #[tool(
        description = concat!("Prepare type hierarchy at position. Returns type items (classes, interfaces, structs) to pass to get_supertypes / get_subtypes, capped at a fixed maximum; `truncated: true` on the result means more exist than are returned. ", indexing_note!(), " ", positions_note_request!(), " ", out_of_workspace_note!()),
        title = "Prepare Type Hierarchy"
    )]
    async fn prepare_type_hierarchy(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<Json<Indexed<TypeHierarchyResult>>, McpError> {
        let file_path = parse_client_path(file_path)?;
        self.structured_result(
            self.context
                .translator
                .handle_type_hierarchy_prepare(file_path, parse_position(line, character)?)
                .await,
        )
    }

    /// Get the supertypes (bases) of a type hierarchy item.
    #[tool(
        description = concat!("Supertypes (base classes, implemented interfaces) of a type hierarchy item. Takes an item exactly as returned by prepare_type_hierarchy, get_supertypes or get_subtypes; returns one level, capped at a fixed maximum; `truncated: true` on the result means more exist than are returned. ", positions_note_request!(), " ", out_of_workspace_note!()),
        title = "Supertypes"
    )]
    async fn get_supertypes(
        &self,
        Parameters(TypeHierarchyWalkParams { item }): Parameters<TypeHierarchyWalkParams>,
    ) -> Result<Json<TypeHierarchyResult>, McpError> {
        self.structured_result(
            self.context
                .translator
                .handle_supertypes(parse_hierarchy_item(item)?)
                .await,
        )
    }

    /// Get the subtypes (derived types) of a type hierarchy item.
    #[tool(
        description = concat!("Subtypes (derived classes, implementors) of a type hierarchy item. Takes an item exactly as returned by prepare_type_hierarchy, get_supertypes or get_subtypes; returns one level, capped at a fixed maximum; `truncated: true` on the result means more exist than are returned. ", positions_note_request!(), " ", out_of_workspace_note!()),
        title = "Subtypes"
    )]
    async fn get_subtypes(
        &self,
        Parameters(TypeHierarchyWalkParams { item }): Parameters<TypeHierarchyWalkParams>,
    ) -> Result<Json<TypeHierarchyResult>, McpError> {
        self.structured_result(
            self.context
                .translator
                .handle_subtypes(parse_hierarchy_item(item)?)
                .await,
        )
    }

    /// Check whether the symbol at a position can be renamed.
    #[tool(
        description = concat!("Whether the symbol at position can be renamed, before proposing a rename. `status` is `renameable` (with the identifier `range` and, when the server gives it, `placeholder`), `default_behavior` (rename is accepted but the server gives no range) or `not_renameable` (optionally with the server's `server_message`). Unsupported servers are refused with a capability error instead. ", positions_note_request!()),
        title = "Prepare Rename"
    )]
    async fn prepare_rename(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<Json<PrepareRenameResult>, McpError> {
        let file_path = parse_client_path(file_path)?;
        self.structured_result(
            self.context
                .translator
                .handle_prepare_rename(file_path, parse_position(line, character)?)
                .await,
        )
    }

    /// Get all occurrences of the symbol at a position within one file.
    #[tool(
        description = concat!("Occurrences of the symbol at position within the same file, each marked `read`, `write` or `text`. A file-local subset of get_references. Capped at a fixed maximum; `truncated: true` on the result means more exist than are returned. ", positions_note_request!()),
        title = "Document Highlights"
    )]
    async fn get_document_highlights(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<Json<DocumentHighlightsResult>, McpError> {
        let file_path = parse_client_path(file_path)?;
        self.structured_result(
            self.context
                .translator
                .handle_document_highlights(file_path, parse_position(line, character)?)
                .await,
        )
    }

    /// Get the chain of ranges enclosing a position.
    #[tool(
        description = "Ranges enclosing a position, innermost first, as the server reports them; their values can be passed to get_code_actions or format_range. Capped at a fixed maximum; `truncated: true` means the outermost were dropped. `positions_degraded: \"request\"` (non-UTF-16 servers only) means the queried position was inexact: do not trust the result.",
        title = "Selection Ranges"
    )]
    async fn get_selection_ranges(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<Json<SelectionRangesResult>, McpError> {
        let file_path = parse_client_path(file_path)?;
        self.structured_result(
            self.context
                .translator
                .handle_selection_range(file_path, parse_position(line, character)?)
                .await,
        )
    }

    /// Get the foldable regions of a file.
    #[tool(
        description = "Foldable regions of a file (blocks, imports, comments, marked regions): 1-based lines, optional columns, `kind`, collapsed text. Sorted by start line, longest first. Capped at a fixed maximum; `truncated: true` means more exist.",
        title = "Folding Ranges"
    )]
    async fn get_folding_ranges(
        &self,
        Parameters(FoldingRangesParams { file_path, kind }): Parameters<FoldingRangesParams>,
    ) -> Result<Json<FoldingRangesResult>, McpError> {
        let file_path = parse_client_path(file_path)?;
        let kind = parse_kind(kind, KindFilterField::Kind)?;
        self.structured_result(
            self.context
                .translator
                .handle_folding_range(file_path, kind)
                .await,
        )
    }

    /// Format only a range of a document according to language server rules.
    // read-only: returns proposed text edits, does not apply them -- mcpls
    // has no write-back path today; revisit if that changes.
    #[tool(
        description = concat!("Format only a range with language-specific rules. Returns text edits the server reports for that range; edits are not filtered or applied. A line past the end of the file is rejected. ", positions_note_request!()),
        title = "Format Range"
    )]
    async fn format_range(
        &self,
        Parameters(FormatRangeParams {
            file_path,
            range,
            tab_size,
            insert_spaces,
        }): Parameters<FormatRangeParams>,
    ) -> Result<Json<FormatDocumentResult>, McpError> {
        let file_path = parse_client_path(file_path)?;
        self.structured_result(
            self.context
                .translator
                .handle_format_range(
                    file_path,
                    parse_bounded_range(&range)?,
                    tab_size,
                    insert_spaces,
                )
                .await,
        )
    }

    /// Get cached diagnostics for a file.
    #[tool(
        description = concat!("Cached diagnostics from server notifications. Faster than the pull-model diagnostics tool, no new analysis. Errors with a retryable `ServerInitializing` while the file's server is still starting, and with `ServerFailedToStart` if it failed to start, instead of returning an empty list. ", availability_note!(), " `indexing_in_progress: true` means the routed server was indexing at some point during this read, so results may be incomplete. `push_notifications_degraded: true` means the routed server crashed and was restarted, so push-only diagnostics (e.g. flycheck) are missing. ", positions_note_response!()),
        title = "Cached Diagnostics"
    )]
    async fn get_cached_diagnostics(
        &self,
        Parameters(CachedDiagnosticsParams { file_path }): Parameters<CachedDiagnosticsParams>,
    ) -> Result<Json<CachedDiagnosticsResponse>, McpError> {
        let file_path = parse_client_path(file_path)?;
        let result = async {
            let (
                _,
                DiagnosticsSnapshot {
                    sources,
                    availability,
                    signals,
                    owner,
                },
            ) = self.diagnostics_snapshot(&file_path).await?;
            let diag_info = sources.merge();
            let encoding = owner.map_or(PositionEncoding::Utf16, |server_id| {
                self.context.translator.position_encoding_for(&server_id)
            });
            let result = Translator::diagnostics_from_cache_entry(
                diag_info.as_ref(),
                encoding,
                self.context.translator.document_tracker(),
            )
            .await;
            Ok::<_, crate::error::Error>(CachedDiagnosticsResponse {
                result,
                availability,
                signals,
            })
        }
        .await;

        self.structured_result(result)
    }

    /// Get recent LSP server log messages.
    #[tool(
        description = "Recent server log messages. Filter by level (error, warning, info, debug) for debugging.",
        title = "Server Logs"
    )]
    async fn get_server_logs(
        &self,
        Parameters(ServerLogsParams { limit, min_level }): Parameters<ServerLogsParams>,
    ) -> Result<Json<ServerLogsResult>, McpError> {
        // Logs were redacted at ingestion (`ServerText` for `ServerLogsResult` is a no-op).
        let cache = self.context.notification_cache.lock().await;
        Ok(Json(Translator::handle_server_logs(
            &cache, limit, min_level,
        )))
    }

    /// Get recent LSP server messages.
    #[tool(
        description = "Recent server messages (showMessage notifications). User-facing prompts and status updates.",
        title = "Server Messages"
    )]
    async fn get_server_messages(
        &self,
        Parameters(ServerMessagesParams { limit }): Parameters<ServerMessagesParams>,
    ) -> Result<Json<ServerMessagesResult>, McpError> {
        self.structured_result({
            let cache = self.context.notification_cache.lock().await;
            Translator::handle_server_messages(&cache, limit)
        })
    }

    /// Get signature help at a position.
    #[tool(
        description = concat!("Signature help at position. Returns parameter info, active signature/parameter, and documentation while typing a call. ", indexing_note!(), " `positions_degraded: \"request\"` (non-UTF-16 servers only) means the queried position was sent unconverted, so the result may not match the position asked about and should not be trusted."),
        title = "Signature Help"
    )]
    async fn get_signature_help(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<Json<Indexed<SignatureHelpResult>>, McpError> {
        let file_path = parse_client_path(file_path)?;
        self.structured_result(
            self.context
                .translator
                .handle_signature_help(file_path, parse_position(line, character)?)
                .await,
        )
    }

    /// Go to implementation locations.
    #[tool(
        description = concat!("Implementation locations of a trait method or interface member. Capped at a fixed maximum for an extremely common trait/interface; `truncated: true` on the result means more implementations exist than are returned. ", name_addressing_note!(), " ", positions_note_request!(), " ", enclosing_symbol_note!(), " ", out_of_workspace_note!()),
        title = "Go to Implementation"
    )]
    async fn go_to_implementation(
        &self,
        Parameters(NavigationParams {
            target: SymbolTargetParams { file_path, target },
            context,
        }): Parameters<NavigationParams>,
    ) -> Result<Json<Addressed<LocationsResult>>, McpError> {
        let (file_path, target) = parse_target(file_path, target)?;
        let translator = &self.context.translator;
        self.structured_result(
            translator
                .with_resolved_target(
                    file_path,
                    target,
                    AddressableTool::Implementation,
                    |file_path, position| {
                        translator.handle_implementation(file_path, position, context)
                    },
                )
                .await,
        )
    }

    /// Go to type definition location.
    #[tool(
        description = concat!("Type definition location of an expression or symbol. Distinct from go-to-definition for variable bindings. Capped at a fixed maximum for a pathological case; `truncated: true` on the result means more locations exist than are returned. ", name_addressing_note!(), " ", positions_note_request!(), " ", enclosing_symbol_note!(), " ", out_of_workspace_note!()),
        title = "Go to Type Definition"
    )]
    async fn go_to_type_definition(
        &self,
        Parameters(NavigationParams {
            target: SymbolTargetParams { file_path, target },
            context,
        }): Parameters<NavigationParams>,
    ) -> Result<Json<Addressed<LocationsResult>>, McpError> {
        let (file_path, target) = parse_target(file_path, target)?;
        let translator = &self.context.translator;
        self.structured_result(
            translator
                .with_resolved_target(
                    file_path,
                    target,
                    AddressableTool::TypeDefinition,
                    |file_path, position| {
                        translator.handle_type_definition(file_path, position, context)
                    },
                )
                .await,
        )
    }

    /// Go to declaration location.
    #[tool(
        description = concat!("Declaration location of the symbol at position. Differs from go-to-definition for languages that separate declaration from definition (C/C++ headers, interface members); servers without a declaration concept may return the definition. An empty result is valid. Capped at a fixed maximum; `truncated: true` on the result means more locations exist than are returned. ", positions_note_request!(), " ", enclosing_symbol_note!(), " ", out_of_workspace_note!()),
        title = "Go to Declaration"
    )]
    async fn go_to_declaration(
        &self,
        Parameters(DeclarationParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
            context,
        }): Parameters<DeclarationParams>,
    ) -> Result<Json<LocationsResult>, McpError> {
        let file_path = parse_client_path(file_path)?;
        self.structured_result(
            self.context
                .translator
                .handle_declaration(file_path, parse_position(line, character)?, context)
                .await,
        )
    }

    /// Restart LSP servers.
    #[tool(
        description = "Restart LSP servers: stop the old process (graceful shutdown, then a kill of its whole process group) and start a fresh one, discarding its in-memory state. Use it when a server is wedged or serves a stale index (e.g. after editing `Cargo.toml` or `package.json`). Give `servers` (ids) or `all: true`. Per server, `status` is `restarted` (with `indexing_state`; `coalesced: true` if another restart of the same server just finished; `push_notifications_degraded: true` means call it again), `failed` (with a typed `reason`; the server stays registered and the next tool call retries), `throttled` (retry after `retry_in_ms`), `initializing` or `not_running`. Requests in flight on the old process fail with a retryable error. Processes that detach into their own session may survive; shared daemons the server started (e.g. Gradle) are killed. Destructive (it kills processes the server started, which other clients may share), not read-only and not idempotent.",
        title = "Restart Server",
        annotations(
            title = "Restart Server",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn restart_server(
        &self,
        Parameters(RestartServerParams { target }): Parameters<RestartServerParams>,
    ) -> Result<Json<RestartServerResult>, McpError> {
        self.structured_result(self.context.translator.restart_servers(target).await)
    }

    /// Report which tools are usable for which languages.
    #[tool(
        description = "Which tools are usable for which languages in this session, without making a failing call. Call it before using a tool on a new language. Per tool, `coverage` is `all`, `some`, `none`, `unknown` (a server is still initializing) or `always` (needs no language server); `routes` groups the languages (`languages`) that share a `status`: `supported`, `push_only` (no pull provider: `get_diagnostics` answers from the push cache), `capability_not_advertised`, `initializing` or `no_server`. `file_path` restricts the report to that file's language. `supported` means the call will be dispatched to a server advertising the capability, not that it will succeed: indexing, push-only diagnostics and respawn backoff can still fail it.",
        title = "Tool Support"
    )]
    async fn get_tool_support(
        &self,
        Parameters(ToolSupportParams { file_path }): Parameters<ToolSupportParams>,
    ) -> Result<Json<ToolSupportReport>, McpError> {
        let file_path = file_path.map(parse_client_path).transpose()?;
        let translator = &self.context.translator;
        let file_language = match file_path.as_ref() {
            Some(path) => translator.language_for_path(path).await.map(Some),
            None => Ok(None),
        };
        let snapshot = translator.tool_support_snapshot();
        self.structured_result(file_language.map(|file_language| {
            let languages =
                file_language.map_or_else(|| snapshot.languages(), |language| vec![language]);
            ToolSupportReport::build(&snapshot, &languages, self.context.mcp.tool_prefix.as_ref())
        }))
    }

    /// Get inlay hints for a range.
    #[tool(
        description = concat!("Inlay hints in range. Returns inferred type/parameter annotations the editor would render inline. Capped at a fixed maximum; `truncated: true` on the result means more hints exist than are returned. The end line must exist in the file; a line past the end is rejected as invalid params. ", indexing_note!(), " ", positions_note_request!()),
        title = "Inlay Hints"
    )]
    async fn get_inlay_hints(
        &self,
        Parameters(InlayHintsParams { file_path, range }): Parameters<InlayHintsParams>,
    ) -> Result<Json<Indexed<InlayHintsResult>>, McpError> {
        let file_path = parse_client_path(file_path)?;
        self.structured_result(
            self.context
                .translator
                .handle_inlay_hints(file_path, parse_range(&range)?)
                .await,
        )
    }
}

impl McplsServer {
    /// [`DiagnosticsResourceUri::resolve`] on the blocking pool, so a slow
    /// filesystem cannot stall a runtime worker.
    async fn resolve_resource(&self, raw: &str) -> crate::error::Result<ResolvedResource> {
        let raw = raw.to_owned();
        let roots = self.context.workspace_roots.clone();
        tokio::task::spawn_blocking(move || DiagnosticsResourceUri::resolve(&raw, &roots))
            .await
            .map_err(|source| crate::error::Error::TaskFailed {
                task: crate::error::BackgroundTask::PathValidation,
                source,
            })?
    }

    /// [`render_error`] with the secrets of every live server.
    fn render_error(&self, error: crate::error::Error) -> McpError {
        render_error(error, &self.context.translator.server_text_redactions())
    }

    /// [`to_structured_tool_result`] with the secrets of every live server
    /// hidden from the result's display prose.
    fn structured_result<T: Serialize + JsonSchema + ServerText>(
        &self,
        result: crate::error::Result<T>,
    ) -> Result<Json<T>, McpError> {
        to_structured_tool_result(result, &self.context.translator.server_text_redactions())
    }

    /// Builds the diagnostics resource payload for `path`; split out of
    /// `read_resource` so tests can drive the real wiring without a
    /// `RequestContext`.
    async fn resource_diagnostics_response(
        &self,
        path: &ClientPath,
    ) -> Result<ResourceDiagnosticsResponse, McpError> {
        let (validated_path, snapshot) = self
            .diagnostics_snapshot(path)
            .await
            .map_err(|e| self.render_error(e))?;
        // Merging the sources (dedupe, sort, size cap) runs after the cache
        // lock is released, since `diagnostics_pump` needs the same lock.
        let diag_info = snapshot.sources.merge();
        Ok(ResourceDiagnosticsResponse::new(
            DocumentState::of(
                self.context
                    .translator
                    .is_document_open(validated_path.as_path()),
            ),
            diag_info.as_ref(),
            snapshot.availability,
            snapshot.signals,
        ))
    }

    /// Validates `file_path` against the workspace roots and reads the cache
    /// for it, for the cache-only reads (`get_cached_diagnostics` and the
    /// diagnostics resource).
    ///
    /// The validation is lock-free (the roots are fixed at startup) and the
    /// URI is built from the canonicalized path, since that is how
    /// `diagnostics_pump` keys what it stores. The route is resolved
    /// independently of the cache lookup: a respawn clears
    /// `diagnostics_owner` for the server's entries along with its stale
    /// diagnostics (#359), so the degraded flag can't be keyed on ownership --
    /// the routing identity is what stays stable across a respawn. A server
    /// that failed to start is an error, not an empty list (#535).
    async fn diagnostics_snapshot(
        &self,
        file_path: &ClientPath,
    ) -> crate::error::Result<(crate::bridge::WorkspacePath, DiagnosticsSnapshot)> {
        let (validated_path, uri) =
            Translator::cached_diagnostics_path_and_uri(&self.context.workspace_roots, file_path)
                .await?;
        let route_id = self
            .context
            .translator
            .diagnostics_route_for_path(validated_path.as_path())
            .into_read_result()?;
        let cache = self.context.notification_cache.lock().await;
        let snapshot = DiagnosticsSnapshot {
            owner: cache.diagnostics_owner(&uri).cloned(),
            availability: cache.availability(&uri, route_id.as_ref()),
            signals: RouteSignals::sample(&cache, route_id.as_ref()),
            sources: cache.diagnostic_sources(&uri),
        };
        drop(cache);
        Ok((validated_path, snapshot))
    }

    /// Body of `read_resource`, kept separate so it can run under
    /// [`contain_panic`].
    async fn read_resource_inner(
        &self,
        request: ReadResourceRequestParams,
    ) -> Result<ReadResourceResponse, McpError> {
        let path = parse_uri(&request.uri).map_err(client_input_error)?;
        let response = self.resource_diagnostics_response(&path).await?;

        let json = serde_json::to_string(&response)
            .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None))?;

        Ok(ReadResourceResult::new(vec![ResourceContents::text(json, request.uri)]).into())
    }
}

/// Runs `handler`, turning a panic in it into an internal MCP error.
///
/// Without this, a panicking request handler sends no response and the client
/// waits for its own timeout (#528).
async fn contain_panic<T>(
    handler: impl Future<Output = Result<T, McpError>>,
    operation: &'static str,
) -> Result<T, McpError> {
    match crate::util::catch_panic(handler).await {
        Ok(result) => result,
        Err(panicked) => {
            tracing::error!("{operation} handler panicked: {}", panicked.message());
            Err(McpError::internal_error(
                format!("{operation} handler panicked"),
                None,
            ))
        }
    }
}

impl McplsServer {
    /// Admits a `subscriptions/listen` request and resolves its URIs.
    ///
    /// Gate order matters: the size check and the empty fast path touch
    /// nothing, the listen slot is taken before any filesystem access, and
    /// the canonicalizing resolution runs off the runtime threads. Returns
    /// `None` when no resource URIs were requested at all; requesting URIs
    /// of which none resolves is an error, not a silent empty stream.
    async fn prepare_listen(
        &self,
        requested: &[String],
        accepted: &[String],
    ) -> crate::error::Result<Option<(ListenPermit, ListenUris)>> {
        if ListenUris::exceeds_budget(requested) {
            return Err(crate::error::Error::ListenFilterTooLarge {
                max: MAX_SUBSCRIPTIONS,
            });
        }
        if requested.is_empty() {
            return Ok(None);
        }
        if accepted.is_empty() {
            return Err(crate::error::Error::NoResolvableListenUris);
        }
        let permit = self.context.session.registry().try_reserve_listen()?;
        let roots = self.context.workspace_roots.clone();
        let accepted = accepted.to_vec();
        let uris = tokio::task::spawn_blocking(move || ListenUris::resolve(&accepted, &roots))
            .await
            .map_err(listen_join_error)?;
        if uris.is_empty() {
            return Err(crate::error::Error::NoResolvableListenUris);
        }
        Ok(Some((permit, uris)))
    }

    /// Reports startup failures of the servers behind a listen's URIs.
    ///
    /// Resolved after registering, like `subscribe`, so a failure settling
    /// concurrently is seen by one side or the other. A failed URI is
    /// published once so the client re-reads and gets the error. If every URI
    /// failed there is nothing to stream, so the registration is dropped and
    /// the first failure is returned (the same rule as "none resolves").
    async fn settle_listen_startup_failures(
        &self,
        registration: ListenRegistration,
        uris: &ListenUris,
    ) -> Result<ListenRegistration, McpError> {
        let failed: Vec<_> = uris
            .canonical()
            .filter_map(|(uri, _)| {
                self.context
                    .translator
                    .diagnostics_route_for_uri(uri)
                    .into_startup_failure()
                    .map(|failure| (uri, failure))
            })
            .collect();
        if let Some(((_, first), _)) = failed.split_first()
            && failed.len() == uris.canonical().count()
        {
            return Err(self.render_error(crate::error::Error::ServerFailedToStart(first.clone())));
        }
        for (uri, _) in &failed {
            registration.publish(uri).await;
        }
        Ok(registration)
    }
}

/// Starts the HTTP listen lease of this request, if its transport attached
/// one; over stdio, or with the lease off, there is no slot and nothing
/// happens.
#[cfg(feature = "transport-http")]
fn start_listen_lease(context: &SubscriptionContext) {
    if let Some(slot) = context
        .request_context()
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| {
            parts
                .extensions
                .get::<Arc<crate::transport::ListenLeaseSlot>>()
        })
    {
        slot.start();
    }
}

const fn listen_join_error(source: tokio::task::JoinError) -> crate::error::Error {
    crate::error::Error::TaskFailed {
        task: crate::error::BackgroundTask::ListenResolution,
        source,
    }
}

#[expect(
    clippy::unused_async_trait_impl,
    reason = "`#[tool_handler]` expands trait methods without `.await`"
)]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for McplsServer {
    async fn list_resources(
        &self,
        request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        contain_panic(
            async {
                let mut open_paths = self.context.translator.open_document_paths();
                // `open_document_paths()` is backed by a `HashMap`; sort so pagination
                // cursors resume at a stable, deterministic position across calls.
                open_paths.sort();

                let cursor = request.and_then(|r| r.cursor);
                let (page, next_cursor) =
                    paginate_resource_paths(&open_paths, cursor.as_deref(), RESOURCE_PAGE_SIZE)?;

                let resources: Vec<_> = page
                    .iter()
                    .filter_map(|path| {
                        let uri = make_uri(path)
                            .inspect_err(|e| {
                                tracing::warn!(
                                    "Skipping path in list_resources (make_uri failed): {}: {e}",
                                    path.display()
                                );
                            })
                            .ok()?;
                        let name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        Some(
                            Resource::new(uri, name)
                                .with_mime_type("application/json")
                                .with_description("LSP diagnostics for this file"),
                        )
                    })
                    .collect();

                Ok(ListResourcesResult {
                    next_cursor,
                    ..ListResourcesResult::with_all_items(resources)
                })
            },
            "list_resources",
        )
        .await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        contain_panic(self.read_resource_inner(request), "read_resource").await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let tcc = ToolCallContext::new(self, request, context);
        contain_panic(self.tool_router.call(tcc), "tool call").await
    }

    /// When cached diagnostics exist, the replay notification is sent to the client
    /// before this call returns its own response; this is legal per JSON-RPC/MCP, which
    /// permits notifications to interleave with in-flight requests, so a conformant
    /// client must demultiplex by request `id` rather than assume response-before-notification ordering.
    /// Over stdio the replay is flushed on the same stream as the response; over HTTP it goes
    /// to the session's standalone GET stream, not the POST response.
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        contain_panic(
            async {
                let session = self.context.session.require_stateful(&context)?;

                // Keyed by the canonical URI: the pump publishes canonical paths only.
                let ResolvedResource {
                    path: validated_path,
                    uri: canonical_uri,
                } = self
                    .resolve_resource(&request.uri)
                    .await
                    .map_err(|e| self.render_error(e))?;

                // Record the subscription *before* checking the cache. This closes the race where
                // a PublishDiagnostics notification lands between the cache check and the
                // subscription being recorded: if diagnostics arrive before this point, the check
                // below catches them; if they arrive after, `diagnostics_pump`'s own
                // `subs.contains` check already sees this URI as subscribed and delivers the
                // update through the normal push path.
                //
                // The raw request URI is recorded as an alias of the canonical one so a later
                // `unsubscribe` for the same raw URI still resolves even if canonicalizing it then
                // fails, e.g. because the file was deleted since subscribing (#499).
                let newly_subscribed = session
                    .subscribe(canonical_uri.clone(), request.uri.clone())
                    .await
                    .map_err(|e| self.render_error(e.into()))?;
                if !newly_subscribed {
                    tracing::debug!(
                        "client re-subscribed to already-subscribed resource {canonical_uri}"
                    );
                }

                // Resolved after recording, mirroring the settle publish in
                // `lib.rs` (which mutates the translator, then snapshots the
                // subscriptions): one side always observes the other, so the
                // client sees either this error or exactly one update. A stray
                // update for the rolled-back URI is harmless. A re-subscribe to
                // an already-subscribed failed route errors too but keeps its
                // earlier subscription; the settle publish already covered it.
                if let Some(failure) = self
                    .context
                    .translator
                    .diagnostics_route_for_path(&validated_path)
                    .into_startup_failure()
                {
                    if newly_subscribed {
                        session
                            .unsubscribe(Some(&canonical_uri), &request.uri)
                            .await;
                    }
                    return Err(
                        self.render_error(crate::error::Error::ServerFailedToStart(failure))
                    );
                }

                // Build the URI from the canonicalized path, matching `read_resource` and
                // what `diagnostics_pump` stores from LSP notifications.
                let lsp_uri = crate::bridge::path_to_uri(&validated_path)
                    .map_err(|e| self.render_error(e))?;
                let has_cached_diagnostics = {
                    let cache = self.context.notification_cache.lock().await;
                    cache.has_diagnostics(&lsp_uri)
                };

                if has_cached_diagnostics
                    && let Err(e) = context
                        .peer
                        .notify_resource_updated(ResourceUpdatedNotificationParam::new(
                            canonical_uri.as_str(),
                        ))
                        .await
                {
                    tracing::warn!("Failed to replay cached diagnostics for {canonical_uri}: {e}");
                }

                Ok(())
            },
            "subscribe",
        )
        .await
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        contain_panic(
            async {
                let session = self.context.session.require_stateful(&context)?;

                // Only a malformed URI errors; a deleted file resolves via its alias (#499).
                let canonical = match self.resolve_resource(&request.uri).await {
                    Ok(resolved) => Some(resolved.uri),
                    Err(e) if e.is_unresolvable_resource() => None,
                    Err(e) => return Err(self.render_error(e)),
                };

                if !session.unsubscribe(canonical.as_ref(), &request.uri).await {
                    tracing::debug!(
                        "client unsubscribed from resource with no matching subscription: {}",
                        request.uri
                    );
                }
                Ok(())
            },
            "unsubscribe",
        )
        .await
    }

    /// Syntax-only: no filesystem access and no capacity check, so an
    /// unauthenticated caller cannot make this cheap synchronous hook expensive.
    /// The acknowledgment is advisory -- `listen` delivers only for URIs that
    /// still resolve inside the workspace and may be refused for capacity after
    /// the acknowledgment was sent. Always `Some`: `None` would answer
    /// `subscriptions/listen` with method-not-found.
    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        let mut filter = SubscriptionFilter::new();
        filter.resource_subscriptions = requested
            .resource_subscriptions
            .as_deref()
            .and_then(ListenUris::syntactic_filter);
        Some(filter)
    }

    /// Serves one `subscriptions/listen` stream (2026-07-28): request-scoped
    /// delivery of `resources/updated`, echoing each raw URI the client asked
    /// for, until the request is cancelled or its connection closes. Works on
    /// every transport and, unlike `resources/subscribe`, needs no session.
    ///
    /// Over HTTP a stream also ends abruptly after its lease
    /// (`ListenLease`); a live client listens
    /// again and the replay below covers the gap.
    ///
    /// Replay and live delivery are paced (a burst of 32, then 320
    /// notifications per second) on a best-effort basis: rmcp's client has no
    /// end-to-end backpressure, so a stalled transport can compress the
    /// spacing back into a burst. The supported
    /// guarantee is client-side: a client using `Peer::listen_with_capacity`
    /// with a capacity of at least `MAX_SUBSCRIPTIONS` never lags on the
    /// replay alone. Live updates arriving during a replay share that buffer,
    /// so a capacity of at least 2000 (twice `MAX_SUBSCRIPTIONS`) is
    /// recommended.
    ///
    /// Streams are capped at `MAX_LISTEN_STREAMS` (shared by stdio and HTTP,
    /// independent of `max_concurrent_sessions`); beyond it the request fails
    /// with a retryable error after the acknowledgment. Over stdio, closing
    /// the input with a stream open makes rmcp wait out its drain timeout
    /// (a few seconds) because it does not cancel request tokens at EOF.
    async fn listen(&self, context: SubscriptionContext) -> Result<(), McpError> {
        let requested = context
            .requested()
            .resource_subscriptions
            .as_deref()
            .unwrap_or_default();
        let accepted = context
            .accepted()
            .resource_subscriptions
            .as_deref()
            .unwrap_or_default();
        let Some((permit, uris)) = self
            .prepare_listen(requested, accepted)
            .await
            .map_err(|e| self.render_error(e))?
        else {
            return Ok(());
        };

        #[cfg(feature = "transport-http")]
        start_listen_lease(&context);

        let uris = Arc::new(uris);
        let sink = context.sink().clone();
        let registration = permit.register(Arc::clone(&uris), |uris| Target::Sink { sink, uris });

        let registration = self
            .settle_listen_startup_failures(registration, &uris)
            .await?;

        // Registered above, before the cache read, so no publish is lost in between.
        let cached: Vec<&DiagnosticsResourceUri> = {
            let cache = self.context.notification_cache.lock().await;
            uris.canonical()
                .filter(|(_, lsp_uri)| cache.is_listen_replayable(lsp_uri))
                .map(|(uri, _)| uri)
                .collect()
        };
        for uri in cached {
            registration.publish(uri).await;
        }

        context.cancelled().await;
        drop(registration);
        Ok(())
    }

    fn get_info(&self) -> RmcpServerConfig {
        let mut implementation = Implementation::new("mcpls", env!("CARGO_PKG_VERSION"));
        implementation.title = Some(
            self.context
                .mcp
                .title
                .as_ref()
                .map_or_else(|| DEFAULT_SERVER_TITLE.to_string(), ToString::to_string),
        );
        implementation.description = Some(self.context.mcp.description.as_ref().map_or_else(
            || DEFAULT_SERVER_DESCRIPTION.to_string(),
            ToString::to_string,
        ));
        implementation.website_url = Some("https://github.com/bug-ops/mcpls".to_string());

        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_resources_subscribe()
            .build();
        let mut server_info = RmcpServerConfig::new(capabilities);
        server_info.server_info = implementation;
        let mut instructions = self.context.mcp.instructions.as_ref().map_or_else(
            || {
                format!(
                    "{DEFAULT_INSTRUCTIONS} Call {} to see which tools work for which languages.",
                    prefixed_tool_name(
                        self.context.mcp.tool_prefix.as_ref(),
                        McpTool::GetToolSupport.name()
                    )
                )
            },
            ToString::to_string,
        );

        if self.context.project_config_status == ProjectConfigStatus::IgnoredUntrusted {
            instructions.push_str(
                " NOTE: a project-local mcpls.toml was found in the current directory but \
                 ignored as untrusted; the server is running on built-in defaults or a global \
                 config instead. If this repository is trusted, restart mcpls with \
                 --trust-project-config (or MCPLS_TRUST_PROJECT_CONFIG=true) to load it.",
            );
        }
        server_info.instructions = Some(instructions);

        server_info
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::path::Path;

    use super::*;
    use crate::bridge::resources::ResourceSubscriptions;
    use crate::bridge::{
        Capability, IndexingSignal, LogLevel, NewName, ResultContext, RouteSignals,
    };
    use crate::config::{
        FileExtension, LanguageId, McpDescription, McpInstructions, McpTitle, ServerCommand,
    };
    #[cfg(unix)]
    use crate::config::{PositionEncodings, TimeoutSecs};
    use crate::mcp::tool_support::ToolBackend;
    use crate::test_lsp::client_path;

    #[test]
    fn test_symbol_resolution_error_maps_to_invalid_params_with_the_outcome_as_data() {
        let err = map_bridge_error(crate::error::Error::SymbolResolution(Box::new(
            crate::error::SymbolResolutionData::NotDefinedInFile {
                name: "Config".to_string(),
            },
        )));

        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(
            err.data,
            Some(serde_json::json!({"resolution": "not_defined_in_file", "name": "Config"}))
        );
        assert!(err.message.contains("Config"));
    }

    #[test]
    fn test_server_restarted_error_is_retryable_with_its_own_code() {
        let err = map_bridge_error(crate::error::Error::ServerRestarted {
            server_id: crate::config::ServerId::from_static("rust"),
        });

        assert_eq!(
            err.code,
            ErrorCode(crate::error::SERVER_RESTARTED_ERROR_CODE)
        );
        assert_eq!(err.data, Some(serde_json::json!({"server_id": "rust"})));
    }

    #[test]
    fn test_unknown_server_error_is_invalid_params_listing_configured_ids() {
        let err = map_bridge_error(crate::error::Error::UnknownServers {
            unknown: vec![crate::config::ServerId::from_static("nope")],
            configured: vec![crate::config::ServerId::from_static("rust")],
        });

        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        assert!(err.message.contains("'nope'") && err.message.contains("'rust'"));
    }

    /// A position-addressed tool call, from the plain position parameters.
    fn at(params: Parameters<PositionParams>) -> Parameters<SymbolTargetParams> {
        Parameters(params.0.into())
    }

    /// As [`at`], for the tools that also take a `context`.
    fn nav(params: Parameters<PositionParams>) -> Parameters<NavigationParams> {
        Parameters(params.0.into())
    }

    fn create_test_server() -> McplsServer {
        create_test_server_with_status(ProjectConfigStatus::NotIgnored)
    }

    fn create_test_server_with_status(project_config_status: ProjectConfigStatus) -> McplsServer {
        create_test_server_with_mcp_config(project_config_status, McpConfig::default())
    }

    fn create_test_server_with_mcp_config(
        project_config_status: ProjectConfigStatus,
        mcp: McpConfig,
    ) -> McplsServer {
        let workspace_roots = WorkspaceRoots::default();
        create_test_server_with_workspace_roots(project_config_status, mcp, workspace_roots)
    }

    /// Like [`create_test_server_with_mcp_config`], for tests that exercise a
    /// path-taking tool (e.g. `get_cached_diagnostics`) and so need a real
    /// workspace root -- an empty one now makes `WorkspaceRoots::validate`
    /// fail closed with `Error::NoWorkspaceRoots`.
    fn create_test_server_with_workspace_roots(
        project_config_status: ProjectConfigStatus,
        mcp: McpConfig,
        workspace_roots: WorkspaceRoots,
    ) -> McplsServer {
        let translator = Arc::new(Translator::new());
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        McplsServer::new(
            translator,
            notification_cache,
            workspace_roots,
            SubscriptionRegistry::new(),
            project_config_status,
            mcp,
        )
    }

    /// Registers a fake client for `id`; keep the returned server alive for
    /// the test. A route to an unregistered, unexpected server reads as
    /// unrouted, so route-dependent tests need a registered one.
    fn register_fake_client(
        translator: &Translator,
        id: &crate::config::ServerId,
    ) -> crate::test_lsp::FakeServer {
        let (client, fake) = crate::test_lsp::fake_lsp_client();
        translator.register_client(id.clone(), client);
        fake
    }

    fn expect_err<T>(result: Result<T, McpError>) -> McpError {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        }
    }

    /// A server with no LSP servers configured, backed by a real file under
    /// a real workspace root -- so a path-taking tool call clears the
    /// workspace-roots gate and exercises the "no server configured for this
    /// language" handler error downstream of it, rather than stopping at the
    /// gate itself with an unrelated `NoWorkspaceRoots`/`FileIo` error.
    fn create_test_server_with_real_file() -> (McplsServer, tempfile::TempDir, PathBuf) {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let test_file = temp_dir.path().join("file.rs");
        std::fs::write(&test_file, "fn main() {}").unwrap();
        let server = create_test_server_with_workspace_roots(
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
        );
        (server, temp_dir, test_file)
    }

    /// #424: `Error::WorkspaceIndexing` must map onto the dedicated
    /// `WORKSPACE_INDEXING_ERROR_CODE`, not the generic `INTERNAL_ERROR`
    /// every other variant gets, and must carry `server_id`/`elapsed_secs`
    /// in `data` so a client can act on them mechanically.
    #[test]
    fn test_map_bridge_error_workspace_indexing_uses_dedicated_error_code() {
        let err = crate::error::Error::WorkspaceIndexing {
            server_id: crate::config::ServerId::from_static("rust"),
            elapsed_secs: 30,
        };
        let mcp_err = map_bridge_error(err);

        assert_eq!(
            mcp_err.code,
            ErrorCode(crate::error::WORKSPACE_INDEXING_ERROR_CODE)
        );
        let data = mcp_err.data.unwrap();
        assert_eq!(data["server_id"], "rust");
        assert_eq!(data["elapsed_secs"], 30);
    }

    /// #479: `Error::ServerInitializing` must be distinguishable on the wire
    /// from a crash (`INTERNAL_ERROR`) and from `WorkspaceIndexing`, since
    /// both are retryable but for different reasons.
    #[test]
    fn test_map_bridge_error_server_initializing_uses_dedicated_error_code() {
        let err = crate::error::Error::ServerInitializing {
            server_id: crate::config::ServerId::from_static("python"),
        };
        let mcp_err = map_bridge_error(err);

        assert_eq!(
            mcp_err.code,
            ErrorCode(crate::error::SERVER_INITIALIZING_ERROR_CODE)
        );
        assert_ne!(
            mcp_err.code,
            ErrorCode(crate::error::WORKSPACE_INDEXING_ERROR_CODE)
        );
        let data = mcp_err.data.unwrap();
        assert_eq!(data["server_id"], "python");
    }

    /// Counterpart to the above: a variant with no explicit classification
    /// (neither caller-fault nor retryable) must still map onto the generic
    /// `INTERNAL_ERROR` code, unchanged.
    #[test]
    fn test_map_bridge_error_other_variant_uses_internal_error_code() {
        let err = crate::error::Error::NoServerForLanguage {
            language: LanguageId::from_static("python"),
            file: crate::config::FileKey::Unmappable,
            patterns: std::sync::Arc::default(),
        };
        let mcp_err = map_bridge_error(err);

        assert_eq!(mcp_err.code, ErrorCode::INTERNAL_ERROR);
        assert!(mcp_err.data.is_none());
    }

    /// #479: caller-fault variants that used to fall through to
    /// `INTERNAL_ERROR` (`-32603`) must now map onto `INVALID_PARAMS`
    /// (`-32602`), matching how the resource handlers already classify a
    /// `PathOutsideWorkspace` rejection.
    #[test]
    fn test_map_bridge_error_caller_fault_variants_use_invalid_params() {
        let caller_fault_errors = vec![
            crate::error::Error::InvalidToolParams("bad params".to_string()),
            crate::error::Error::PathOutsideWorkspace(PathBuf::from("/etc/passwd")),
            crate::error::Error::NotARegularFile(PathBuf::from("/dev/null")),
            crate::error::Error::NoResolvableListenUris,
            crate::error::Error::DocumentNotFound(PathBuf::from("/missing.rs")),
            crate::error::Error::FileSizeLimitExceeded(crate::util::SizeExceeded {
                size: 100,
                max: std::num::NonZeroU64::new(10).unwrap(),
            }),
            crate::error::Error::InvalidClientPath(crate::bridge::InvalidClientPath::Empty),
        ];

        for err in caller_fault_errors {
            let debug = format!("{err:?}");
            let mcp_err = map_bridge_error(err);
            assert_eq!(
                mcp_err.code,
                ErrorCode::INVALID_PARAMS,
                "expected {debug} to map onto INVALID_PARAMS"
            );
        }
    }

    fn two_server_redactions(secret: &str) -> Redactions {
        Redactions::new([
            ("A_TOKEN".to_owned(), "alpha-secret-111".to_owned()),
            ("B_TOKEN".to_owned(), secret.to_owned()),
        ])
    }

    /// #612: server B's secret inside server A's error message, split by the
    /// client's one 4 KiB cut, leaves no fragment in the tool error or in the
    /// rewritten error's `data`, because it is redacted before the cut.
    #[tokio::test]
    async fn test_error_with_another_servers_secret_across_the_cut_leaves_no_fragment() {
        use tokio::io::BufReader;

        let secret = "bravo-secret-222-padded-to-straddle";
        let union = two_server_redactions(secret);
        let (client, mut fake, _lanes) =
            crate::test_lsp::fake_lsp_client_with_redactions(union.clone());
        let request = tokio::spawn(async move {
            client
                .request::<_, serde_json::Value>(
                    "textDocument/hover",
                    serde_json::json!({}),
                    std::time::Duration::from_secs(30),
                )
                .await
        });
        let mut reader = BufReader::new(&mut fake.write_stdout);
        let wire = crate::test_lsp::read_framed_message(&mut reader).await;
        let pad = "x".repeat(crate::lsp::MAX_ERROR_MESSAGE_CALLER_BYTES - 20);
        let message = format!("Invalid offset {pad}{secret}");
        crate::test_lsp::write_error_response(
            &mut fake.read_half_stdin,
            &wire["id"],
            -32602,
            &message,
        )
        .await;

        let error = request.await.unwrap().unwrap_err();
        let rendered = render_error(error, &union);

        let data = rendered.data.as_ref().map_or_default(ToString::to_string);
        for text in [rendered.message.to_string(), data] {
            for len in 4..=secret.len() {
                assert!(!text.contains(&secret[..len]), "{len}: {text}");
            }
        }
    }

    /// #612: an embedder's client whose own set lacks another server's
    /// secret still has it hidden by the funnel, in the message and in `data`.
    #[test]
    fn test_render_error_hides_configured_secrets_in_message_and_data() {
        let redactions = two_server_redactions("bravo-secret-222");
        let rendered = render_error(
            crate::error::Error::LspServerError {
                code: -32602,
                message: "Invalid offset LineCol { line: 9 } for bravo-secret-222".to_owned(),
                data: None,
            },
            &redactions,
        );
        let data = rendered.data.as_ref().map(ToString::to_string).unwrap();
        assert!(
            !rendered.message.contains("bravo-secret-222"),
            "{}",
            rendered.message
        );
        assert!(data.contains("[redacted:B_TOKEN]"), "{data}");
        assert!(!data.contains("bravo-secret-222"), "{data}");

        let plain = render_error(
            crate::error::Error::InvalidToolParams("nothing secret".into()),
            &redactions,
        );
        assert!(
            plain.message.contains("nothing secret"),
            "{}",
            plain.message
        );
    }

    /// #612: with no live server at all, the translator still knows the
    /// startup-wide secrets, so the funnel redacts.
    #[tokio::test]
    async fn test_render_error_redacts_with_no_live_client() {
        let startup = std::sync::Arc::new(two_server_redactions("bravo-secret-222"));
        let translator = Translator::new().with_startup_redactions(startup);
        let server = McplsServer::new(
            Arc::new(translator),
            Arc::new(Mutex::new(NotificationCache::new())),
            WorkspaceRoots::default(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );
        let rendered = server.render_error(crate::error::Error::InvalidToolParams(
            "token bravo-secret-222".to_owned(),
        ));
        assert!(
            !rendered.message.contains("bravo-secret-222"),
            "{}",
            rendered.message
        );
    }

    /// #612: a resource error built from a spawn failure whose text carries
    /// another server's secret is hidden through the server's live set.
    #[tokio::test]
    async fn test_resource_error_from_a_startup_failure_hides_another_servers_secret() {
        let server = create_test_server();
        let (client, _fake, _lanes) = crate::test_lsp::fake_lsp_client_with_redactions(
            two_server_redactions("bravo-secret-222"),
        );
        server
            .context
            .translator
            .register_client(crate::config::ServerId::from_static("a"), client);
        let failure =
            crate::error::Error::ServerFailedToStart(Box::new(crate::error::ServerSpawnFailure {
                server_id: crate::config::ServerId::from_static("b"),
                language_id: crate::config::LanguageId::from_static("python"),
                command: ServerCommand::from_static("pyright"),
                reason: crate::error::StartupFailure::Spawn(Arc::new(
                    crate::error::Error::LspInitFailed {
                        phase: crate::error::InitPhase::Initialize,
                        cause: Box::new(crate::error::Error::LspProtocolError(
                            crate::error::RedactedText::fixed("exited with token bravo-secret-222"),
                        )),
                        hint: None,
                        stderr: None,
                    },
                )),
            }));

        let rendered = server.render_error(failure);

        assert!(
            !rendered.message.contains("bravo-secret-222"),
            "{}",
            rendered.message
        );
        assert!(
            rendered.message.contains("[redacted:B_TOKEN]"),
            "{}",
            rendered.message
        );
    }

    /// #617: the remaining position tools reject out-of-range input too, and a
    /// valid position is not mistaken for invalid parameters.
    #[tokio::test]
    async fn test_remaining_position_tools_reject_bad_input_and_accept_valid_input() {
        let server = create_test_server();
        let position = |line: u32, character: u32| PositionParams {
            file_path: PathBuf::from("/ws/a.rs"),
            line,
            character,
        };
        let range = |start: (u32, u32), end: (u32, u32)| RangeParams {
            start_line: start.0,
            start_character: start.1,
            end_line: end.0,
            end_character: end.1,
        };
        for (line, character) in [(0, 1), (1, 0), (1_000_001, 1)] {
            let results = [
                server
                    .get_signature_help(Parameters(position(line, character)))
                    .await
                    .map(|_| ()),
                server
                    .go_to_implementation(nav(Parameters(position(line, character))))
                    .await
                    .map(|_| ()),
                server
                    .go_to_type_definition(nav(Parameters(position(line, character))))
                    .await
                    .map(|_| ()),
                server
                    .go_to_declaration(Parameters(position(line, character).into()))
                    .await
                    .map(|_| ()),
                server
                    .prepare_call_hierarchy(at(Parameters(position(line, character))))
                    .await
                    .map(|_| ()),
                server
                    .prepare_type_hierarchy(Parameters(position(line, character)))
                    .await
                    .map(|_| ()),
                server
                    .prepare_rename(Parameters(position(line, character)))
                    .await
                    .map(|_| ()),
                server
                    .get_document_highlights(Parameters(position(line, character)))
                    .await
                    .map(|_| ()),
                server
                    .get_code_actions(Parameters(CodeActionsParams {
                        file_path: PathBuf::from("/ws/a.rs"),
                        range: range((line, character), (line, character)),
                        kind_filter: None,
                    }))
                    .await
                    .map(|_| ()),
                server
                    .format_range(Parameters(FormatRangeParams {
                        file_path: PathBuf::from("/ws/a.rs"),
                        range: range((line, character), (line, character)),
                        tab_size: crate::bridge::TabSize::default(),
                        insert_spaces: true,
                    }))
                    .await
                    .map(|_| ()),
            ];
            for result in results {
                assert_eq!(
                    result.unwrap_err().code,
                    ErrorCode::INVALID_PARAMS,
                    "({line}, {character})"
                );
            }
        }

        let control = server
            .get_hover(at(Parameters(position(1, 1))))
            .await
            .map(|_| ());
        assert_ne!(control.unwrap_err().code, ErrorCode::INVALID_PARAMS);
    }

    /// #622: a malformed resource URI is not an unresolvable one, so the
    /// unsubscribe path reports it as invalid params, off the async worker.
    #[tokio::test]
    async fn test_resolve_resource_reports_a_malformed_uri_as_invalid_params() {
        let server = create_test_server();
        let error = server.resolve_resource("file:///a.rs").await.unwrap_err();
        assert!(!error.is_unresolvable_resource(), "{error:?}");
        assert_eq!(map_bridge_error(error).code, ErrorCode::INVALID_PARAMS);
    }

    /// #618: a malformed call hierarchy `item` fails at the parameter
    /// boundary, before any handler runs.
    #[test]
    fn test_malformed_call_hierarchy_item_is_rejected_when_deserializing() {
        for item in [
            serde_json::json!({"invalid": "structure"}),
            serde_json::json!({"name": "f", "kind": "function", "uri": "file:///a.rs"}),
        ] {
            let parsed = serde_json::from_value::<CallHierarchyCallsParams>(
                serde_json::json!({ "item": item }),
            );
            assert!(parsed.is_err());
        }
    }

    /// #636: a hierarchy item with a zero, oversized or reversed range is
    /// `-32602` for every walking tool, before any server is asked.
    #[tokio::test]
    async fn test_hierarchy_tools_reject_out_of_range_item_as_invalid_params() {
        let server = create_test_server();
        let item_json = |range: (u32, u32), selection: (u32, u32)| {
            let at = |(line, character): (u32, u32)| serde_json::json!({"line": line, "character": character});
            serde_json::from_value::<crate::bridge::HierarchyItem>(serde_json::json!({
                "name": "x", "kind": 5, "uri": "file:///ws/a.rs",
                "range": {"start": at(range), "end": at((1_000, 1))},
                "selectionRange": {"start": at(selection), "end": at(selection)},
            }))
            .unwrap()
        };
        for (range, selection) in [
            ((0, 1), (1, 1)),
            ((1, 0), (1, 1)),
            ((1, 1_000_001), (1, 1)),
            ((1, 1), (0, 1)),
            ((1, 1), (1, 0)),
            ((1, 1), (1, 1_000_001)),
            ((1, 1), (4_294_967_295, 1)),
            ((1, 1), (1, 4_294_967_295)),
        ] {
            let results = [
                server
                    .get_incoming_calls(Parameters(CallHierarchyCallsParams {
                        item: item_json(range, selection),
                    }))
                    .await
                    .map(|_| ()),
                server
                    .get_outgoing_calls(Parameters(CallHierarchyCallsParams {
                        item: item_json(range, selection),
                    }))
                    .await
                    .map(|_| ()),
                server
                    .get_supertypes(Parameters(TypeHierarchyWalkParams {
                        item: item_json(range, selection),
                    }))
                    .await
                    .map(|_| ()),
                server
                    .get_subtypes(Parameters(TypeHierarchyWalkParams {
                        item: item_json(range, selection),
                    }))
                    .await
                    .map(|_| ()),
            ];
            for result in results {
                let err = result.unwrap_err();
                assert_eq!(
                    err.code,
                    ErrorCode::INVALID_PARAMS,
                    "{range:?} {selection:?}"
                );
            }
        }
    }

    /// An unknown `kind_filter` keeps its `-32602` class, in any case a known
    /// one passes the boundary, and a non-string value is a parameter error.
    #[tokio::test]
    async fn test_kind_filters_reject_unknown_kinds_as_invalid_params() {
        let server = create_test_server();
        let file = std::env::temp_dir().join("a.rs");
        let code_actions = |kind: serde_json::Value| {
            serde_json::from_value::<CodeActionsParams>(serde_json::json!({
                "file_path": file,
                "start_line": 1, "start_character": 1, "end_line": 1, "end_character": 2,
                "kind_filter": kind,
            }))
        };
        let symbols = |kind: serde_json::Value| {
            serde_json::from_value::<WorkspaceSymbolParams>(
                serde_json::json!({"query": "x", "kind_filter": kind}),
            )
        };

        for (kind, rejected) in [
            ("QuickFix", false),
            ("SOURCE.organizeimports", false),
            ("bogus", true),
        ] {
            let params = code_actions(kind.into()).unwrap();
            let code = server
                .get_code_actions(Parameters(params))
                .await
                .map(|_| ())
                .unwrap_err()
                .code;
            assert_eq!(code == ErrorCode::INVALID_PARAMS, rejected, "{kind}");
        }
        for (kind, rejected) in [
            ("Function", false),
            ("function", false),
            ("22", false),
            ("NotAKind", true),
        ] {
            let params = symbols(kind.into()).unwrap();
            let code = server
                .workspace_symbol_search(Parameters(params))
                .await
                .map(|_| ())
                .unwrap_err()
                .code;
            assert_eq!(code == ErrorCode::INVALID_PARAMS, rejected, "{kind}");
        }
        assert!(code_actions(serde_json::json!(3)).is_err());
        assert!(symbols(serde_json::json!(true)).is_err());
    }

    /// #700: `symbol_kind`, `kind` of `get_folding_ranges` and an over-long
    /// `kind_filter` fail as `-32602` like every other kind filter, in any
    /// case, with a bounded message.
    #[tokio::test]
    async fn test_every_kind_filter_input_fails_the_same_way() {
        let server = create_test_server();
        let file = std::env::temp_dir().join("a.rs");
        let references = |kind: &str| {
            serde_json::from_value::<ReferencesParams>(serde_json::json!({
                "file_path": file, "symbol_name": "f", "symbol_kind": kind,
            }))
            .unwrap()
        };
        let folding = |kind: &str| {
            serde_json::from_value::<FoldingRangesParams>(
                serde_json::json!({"file_path": file, "kind": kind}),
            )
            .unwrap()
        };
        let long = "x".repeat(10_000);

        let mut errors = Vec::new();
        for kind in ["NotAKind", long.as_str()] {
            errors.push(
                server
                    .get_references(Parameters(references(kind)))
                    .await
                    .map(|_| ()),
            );
            errors.push(
                server
                    .get_folding_ranges(Parameters(folding(kind)))
                    .await
                    .map(|_| ()),
            );
        }
        let long_symbols = serde_json::from_value::<WorkspaceSymbolParams>(
            serde_json::json!({"query": "x", "kind_filter": long}),
        )
        .unwrap();
        errors.push(
            server
                .workspace_symbol_search(Parameters(long_symbols))
                .await
                .map(|_| ()),
        );
        for error in errors {
            let error = error.unwrap_err();
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS, "{error:?}");
            assert!(error.message.len() < 1_000, "{} bytes", error.message.len());
        }

        for accepted in ["Imports", "IMPORTS", "all"] {
            let outcome = server
                .get_folding_ranges(Parameters(folding(accepted)))
                .await
                .map(|_| ());
            assert_ne!(
                outcome.unwrap_err().code,
                ErrorCode::INVALID_PARAMS,
                "{accepted}"
            );
        }
    }

    /// The diagnostics responses carry `availability`, and `get_diagnostics`
    /// also `origin`, beside the route signals.
    #[test]
    fn test_diagnostics_responses_carry_availability_and_origin() {
        let pulled = serde_json::to_value(DiagnosticsResponse {
            result: DocumentDiagnosticsResult {
                diagnostics: Vec::new(),
                positions_degraded: None,
                enrichment: None,
            },
            availability: DiagnosticsAvailability::Pending,
            origin: DiagnosticsOrigin::PushCache,
            signals: RouteSignals::default(),
        })
        .unwrap();
        assert_eq!(pulled["availability"], "pending");
        assert_eq!(pulled["origin"], "push_cache");
        assert_eq!(pulled["indexing_in_progress"], false);

        let resource = serde_json::to_value(ResourceDiagnosticsResponse::new(
            DocumentState::Open,
            None,
            DiagnosticsAvailability::Pending,
            RouteSignals::default(),
        ))
        .unwrap();
        assert_eq!(resource["availability"], "pending");
    }

    /// #636: an inverted `range` or `selectionRange` is `-32602`, while a
    /// valid item gets past input validation (no `-32602`).
    #[tokio::test]
    async fn test_hierarchy_tools_reject_inverted_range_and_accept_valid_item() {
        let server = create_test_server();
        let uri = url::Url::from_file_path(std::env::temp_dir().join("a.rs")).unwrap();
        let item = |range_end: (u32, u32), selection_end: (u32, u32)| {
            let at = |(line, character): (u32, u32)| serde_json::json!({"line": line, "character": character});
            serde_json::from_value::<crate::bridge::HierarchyItem>(serde_json::json!({
                "name": "x", "kind": 5, "uri": uri.as_str(),
                "range": {"start": at((5, 1)), "end": at(range_end)},
                "selectionRange": {"start": at((5, 1)), "end": at(selection_end)},
            }))
            .unwrap()
        };
        let call = |item| async {
            server
                .get_incoming_calls(Parameters(CallHierarchyCallsParams { item }))
                .await
                .map(|_| ())
        };
        for (range_end, selection_end) in [((4, 1), (5, 1)), ((9, 1), (4, 1))] {
            let err = call(item(range_end, selection_end)).await.unwrap_err();
            assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{range_end:?}");
        }
        let err = call(item((9, 1), (5, 4))).await.unwrap_err();
        assert_ne!(err.code, ErrorCode::INVALID_PARAMS, "{err:?}");
    }

    /// #617: a zero or oversized line, or a malformed range, is `-32602` for
    /// every position- and range-taking tool, before any server is asked.
    #[tokio::test]
    async fn test_position_tools_reject_out_of_range_input_as_invalid_params() {
        let server = create_test_server();
        let position = |line: u32, character: u32| PositionParams {
            file_path: PathBuf::from("/ws/a.rs"),
            line,
            character,
        };
        let range = |start: (u32, u32), end: (u32, u32)| RangeParams {
            start_line: start.0,
            start_character: start.1,
            end_line: end.0,
            end_character: end.1,
        };
        for (line, character) in [(0, 1), (1, 0), (1_000_001, 1), (1, 1_000_001)] {
            let results = [
                server
                    .get_hover(at(Parameters(position(line, character))))
                    .await
                    .map(|_| ()),
                server
                    .get_definition(nav(Parameters(position(line, character))))
                    .await
                    .map(|_| ()),
                server
                    .get_references(Parameters(ReferencesParams {
                        target: position(line, character).into(),
                        include_declaration: false,
                        context: crate::bridge::ResultContext::None,
                    }))
                    .await
                    .map(|_| ()),
                server
                    .rename_symbol(Parameters(RenameParams {
                        target: position(line, character).into(),
                        new_name: NewName::try_new("x").unwrap(),
                    }))
                    .await
                    .map(|_| ()),
                server
                    .get_completions(Parameters(CompletionsParams {
                        position: position(line, character),
                        trigger: None,
                    }))
                    .await
                    .map(|_| ()),
                server
                    .get_inlay_hints(Parameters(InlayHintsParams {
                        file_path: PathBuf::from("/ws/a.rs"),
                        range: range((line, character), (line, character)),
                    }))
                    .await
                    .map(|_| ()),
            ];
            for result in results {
                let err = result.unwrap_err();
                assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "({line}, {character})");
            }
        }

        let reversed = server
            .get_inlay_hints(Parameters(InlayHintsParams {
                file_path: PathBuf::from("/ws/a.rs"),
                range: range((3, 1), (2, 1)),
            }))
            .await;
        assert_eq!(
            reversed.map(|_| ()).unwrap_err().code,
            ErrorCode::INVALID_PARAMS
        );

        let oversized = server
            .format_range(Parameters(FormatRangeParams {
                file_path: PathBuf::from("/ws/a.rs"),
                range: range((1, 1), (10_002, 1)),
                tab_size: crate::bridge::TabSize::default(),
                insert_spaces: true,
            }))
            .await;
        assert_eq!(
            oversized.map(|_| ()).unwrap_err().code,
            ErrorCode::INVALID_PARAMS
        );
    }

    /// #527: a startup failure reaches the client as an internal error that
    /// still carries the spawn failure's install guidance.
    #[test]
    fn test_map_bridge_error_startup_failure_carries_guidance() {
        let error =
            crate::error::Error::ServerFailedToStart(Box::new(crate::error::ServerSpawnFailure {
                server_id: crate::config::ServerId::from_static("rust"),
                language_id: LanguageId::from_static("rust"),
                command: ServerCommand::from_static("rust-analyzer"),
                reason: crate::error::StartupFailure::Spawn(Arc::new(
                    crate::error::Error::ServerNotFound {
                        command: crate::config::ServerCommand::from_static("rust-analyzer"),
                        source: std::io::Error::from(std::io::ErrorKind::NotFound),
                    },
                )),
            }));

        let mapped = map_bridge_error(error);

        assert_eq!(mapped.code, ErrorCode::INTERNAL_ERROR);
        assert!(
            mapped
                .message
                .contains("rustup component add rust-analyzer"),
            "{}",
            mapped.message
        );
    }

    #[tokio::test]
    async fn test_contain_panic_turns_panic_into_internal_error() {
        let result: Result<(), McpError> = contain_panic(
            async {
                panic!("handler boom");
            },
            "tool call",
        )
        .await;

        let error = result.unwrap_err();
        assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
        assert!(error.message.contains("tool call handler panicked"));
    }

    #[tokio::test]
    async fn test_contain_panic_passes_through_handler_result() {
        let ok = contain_panic(async { Ok(7) }, "read_resource").await;
        assert_eq!(ok.unwrap(), 7);

        let err: Result<(), McpError> = contain_panic(
            async { Err(McpError::invalid_params("bad", None)) },
            "read_resource",
        )
        .await;
        assert_eq!(err.unwrap_err().code, ErrorCode::INVALID_PARAMS);
    }

    /// #465: the rewritten "position out of range" error is `INVALID_PARAMS` and
    /// still exposes the server's raw error as `data`.
    #[test]
    fn test_map_bridge_error_invalid_position_carries_raw_error_as_data() {
        let mcp_err = map_bridge_error(crate::error::Error::LspServerError {
            code: -32603,
            message: "Invalid offset LineCol { line: 9, col: 0 }".to_string(),
            data: None,
        });

        assert_eq!(mcp_err.code, ErrorCode::INVALID_PARAMS);
        assert!(mcp_err.message.contains("position out of range"));
        assert_eq!(
            mcp_err.data,
            Some(serde_json::json!({
                "code": -32603,
                "raw_message": "Invalid offset LineCol { line: 9, col: 0 }"
            }))
        );
    }

    /// #479 follow-up: `WorkspaceServersInitializing` (the no-single-server
    /// counterpart of `ServerInitializing`, see its doc comment) must be
    /// retryable too, sharing `SERVER_INITIALIZING_ERROR_CODE` since it's the
    /// same underlying condition.
    #[test]
    fn test_map_bridge_error_workspace_servers_initializing_is_retryable() {
        let err = crate::error::Error::WorkspaceServersInitializing;
        let mcp_err = map_bridge_error(err);

        assert_eq!(
            mcp_err.code,
            ErrorCode(crate::error::SERVER_INITIALIZING_ERROR_CODE)
        );
    }

    #[tokio::test]
    async fn test_server_info() {
        let server = create_test_server();
        let info = server.get_info();

        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, "mcpls");
        assert!(info.instructions.is_some());
    }

    #[tokio::test]
    async fn test_server_info_omits_ignore_notice_when_not_ignored() {
        let server = create_test_server_with_status(ProjectConfigStatus::NotIgnored);
        let info = server.get_info();

        assert!(!info.instructions.unwrap().contains("ignored as untrusted"));
    }

    #[tokio::test]
    async fn test_server_info_surfaces_ignored_project_config() {
        let server = create_test_server_with_status(ProjectConfigStatus::IgnoredUntrusted);
        let info = server.get_info();

        let instructions = info.instructions.unwrap();
        assert!(instructions.contains("ignored as untrusted"));
        assert!(instructions.contains("--trust-project-config"));
    }

    #[tokio::test]
    async fn test_get_info_default_mcp_config_uses_built_in_text() {
        let server = create_test_server_with_mcp_config(
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );
        let info = server.get_info();

        assert_eq!(
            info.server_info.title.as_deref(),
            Some(DEFAULT_SERVER_TITLE)
        );
        assert_eq!(
            info.server_info.description.as_deref(),
            Some(DEFAULT_SERVER_DESCRIPTION)
        );
        let instructions = info.instructions.unwrap();
        assert!(instructions.starts_with(DEFAULT_INSTRUCTIONS));
        assert!(instructions.contains("get_tool_support"));
    }

    #[tokio::test]
    async fn test_get_info_default_instructions_name_the_prefixed_tool_support_tool() {
        let mcp = McpConfig {
            tool_prefix: Some("p".parse().unwrap()),
            ..McpConfig::default()
        };
        let info =
            create_test_server_with_mcp_config(ProjectConfigStatus::NotIgnored, mcp).get_info();

        assert!(info.instructions.unwrap().contains("p_get_tool_support"));
    }

    #[tokio::test]
    async fn test_get_info_reflects_configured_mcp_fields() {
        let mcp = McpConfig {
            title: Some(McpTitle::new("Custom Title").unwrap()),
            description: Some(McpDescription::new("Custom description").unwrap()),
            instructions: Some(McpInstructions::new("Custom instructions.").unwrap()),
            tool_prefix: None,
        };
        let server = create_test_server_with_mcp_config(ProjectConfigStatus::NotIgnored, mcp);
        let info = server.get_info();

        assert_eq!(info.server_info.title.as_deref(), Some("Custom Title"));
        assert_eq!(
            info.server_info.description.as_deref(),
            Some("Custom description")
        );
        assert_eq!(info.instructions.as_deref(), Some("Custom instructions."));
    }

    /// Configured `instructions` replace the built-in blurb, but the
    /// untrusted-project-config NOTE must still be appended afterward --
    /// including when `instructions` sits exactly at
    /// `MAX_MCP_INSTRUCTIONS_BYTES`, proving the NOTE is outside the user's
    /// budget rather than truncated to make room for it.
    #[tokio::test]
    async fn test_get_info_appends_ignore_notice_after_configured_instructions_at_cap() {
        let instructions = "a".repeat(crate::config::MAX_MCP_INSTRUCTIONS_BYTES);
        let mcp = McpConfig {
            title: None,
            description: None,
            instructions: Some(McpInstructions::new(instructions.clone()).unwrap()),
            tool_prefix: None,
        };
        let server = create_test_server_with_mcp_config(ProjectConfigStatus::IgnoredUntrusted, mcp);
        let info = server.get_info();

        let returned_instructions = info.instructions.unwrap();
        assert!(returned_instructions.starts_with(&instructions));
        assert!(returned_instructions.contains("ignored as untrusted"));
    }

    #[tokio::test]
    async fn test_hover_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(PositionParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
            line: 1,
            character: 1,
        });

        // No LSP server is registered for any language on this test server,
        // so this fails downstream of the workspace-roots gate with
        // `Error::NoServerForLanguage`/`NoServerConfigured`.
        let result = server.get_hover(at(params)).await;
        assert!(result.is_err());
    }

    /// #417: the fail-closed `Error::NoWorkspaceRoots` path must propagate
    /// correctly through a `#[tool]` handler's full error-mapping chain
    /// (`to_structured_tool_result`/`McpError::internal_error`), not just through the
    /// lower-level `Translator::validate_path`/`WorkspaceRoots::validate`
    /// unit tests -- `create_test_server()` here deliberately keeps the
    /// empty roots that `create_test_server_with_real_file()` (used by the
    /// rest of this test group) sets up a real root to avoid.
    #[tokio::test]
    async fn test_hover_tool_with_params_no_workspace_roots() {
        let server = create_test_server();
        let params = Parameters(PositionParams {
            file_path: PathBuf::from("/test/file.rs"),
            line: 1,
            character: 1,
        });

        let result = server.get_hover(at(params)).await;
        let err = result.err().unwrap();
        assert!(
            err.message.contains("no workspace roots configured"),
            "expected the NoWorkspaceRoots error to propagate through the tool handler, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn test_definition_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(PositionParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
            line: 10,
            character: 5,
        });

        let result = server.get_definition(nav(params)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_references_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(ReferencesParams {
            target: PositionParams {
                file_path: PathBuf::from(test_file.to_str().unwrap()),
                line: 10,
                character: 5,
            }
            .into(),
            include_declaration: false,
            context: ResultContext::None,
        });

        let result = server.get_references(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_diagnostics_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(DiagnosticsParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
            context: ResultContext::None,
        });

        let result = server.get_diagnostics(params).await;
        assert!(result.is_err());
    }

    /// #445: `get_diagnostics` must flag `indexing_in_progress: true` when the
    /// file's diagnostics-route server has an active `Loading` signal as of
    /// this read, since `handle_diagnostics` itself deliberately stays
    /// ungated (see `routing::IndexingGate`'s doc) -- this is the only place
    /// that signal reaches the caller.
    #[tokio::test]
    async fn test_get_diagnostics_flags_indexing_in_progress() {
        use std::collections::HashMap;
        use std::fs;

        use tempfile::TempDir;
        use tokio::io::BufReader;

        use crate::config::{ServerId, ToolRouter};
        use crate::test_lsp::{fake_lsp_client, read_framed_message, write_response};

        let server_id = ServerId::from_static("rust");
        let dir = TempDir::new().unwrap();
        let mut translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                LanguageId::from_static("rust"),
            )]))
            .with_extensions(HashMap::from([(
                FileExtension::from_static("rs"),
                LanguageId::from_static("rust"),
            )]));
        translator
            .set_workspace_roots(WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap());
        let translator = Arc::new(translator);
        let (client, mut fake_server) = fake_lsp_client();
        translator.register_client(server_id.clone(), client);

        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        notification_cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let mcp_server = McplsServer::new(
            Arc::clone(&translator),
            Arc::clone(&notification_cache),
            WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );

        let call = {
            let params = Parameters(DiagnosticsParams {
                file_path: PathBuf::from(path_str),
                context: ResultContext::None,
            });
            tokio::spawn(async move { mcp_server.get_diagnostics(params).await })
        };

        let mut wire = BufReader::new(&mut fake_server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        write_response(
            &mut fake_server.read_half_stdin,
            &diag_request["id"],
            serde_json::json!({"kind": "full", "items": []}),
        )
        .await;

        let result = call.await.unwrap().unwrap();
        assert!(result.0.signals.indexing.indexing_in_progress);
        assert_eq!(result.0.result.diagnostics.len(), 0);
    }

    /// Counterpart to the above: once the server has no active `Loading`
    /// signal, `indexing_in_progress` must read back `false`.
    #[tokio::test]
    async fn test_get_diagnostics_indexing_in_progress_false_when_ready() {
        use std::collections::HashMap;
        use std::fs;

        use tempfile::TempDir;
        use tokio::io::BufReader;

        use crate::config::{ServerId, ToolRouter};
        use crate::test_lsp::{fake_lsp_client, read_framed_message, write_response};

        let server_id = ServerId::from_static("rust");
        let dir = TempDir::new().unwrap();
        let mut translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                LanguageId::from_static("rust"),
            )]))
            .with_extensions(HashMap::from([(
                FileExtension::from_static("rs"),
                LanguageId::from_static("rust"),
            )]));
        translator
            .set_workspace_roots(WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap());
        let translator = Arc::new(translator);
        let (client, mut fake_server) = fake_lsp_client();
        translator.register_client(server_id.clone(), client);

        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let mcp_server = McplsServer::new(
            Arc::clone(&translator),
            Arc::clone(&notification_cache),
            WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );

        let call = {
            let params = Parameters(DiagnosticsParams {
                file_path: PathBuf::from(path_str),
                context: ResultContext::None,
            });
            tokio::spawn(async move { mcp_server.get_diagnostics(params).await })
        };

        let mut wire = BufReader::new(&mut fake_server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        write_response(
            &mut fake_server.read_half_stdin,
            &diag_request["id"],
            serde_json::json!({"kind": "full", "items": []}),
        )
        .await;

        let result = call.await.unwrap().unwrap();
        assert!(!result.0.signals.indexing.indexing_in_progress);
    }

    /// S2 regression: the server is `Loading` when the pull *starts* but
    /// transitions to `Ready` before the pull *settles* -- sampling only
    /// after the pull (the original, buggy shape) would read `false` here,
    /// the exact false negative #445 exists to close. `indexing_in_progress`
    /// must still read `true`, proving the "sample before" half of the OR
    /// actually does its job.
    #[tokio::test]
    async fn test_get_diagnostics_flags_indexing_in_progress_even_if_finished_mid_pull() {
        use std::collections::HashMap;
        use std::fs;

        use tempfile::TempDir;
        use tokio::io::BufReader;

        use crate::config::{ServerId, ToolRouter};
        use crate::test_lsp::{fake_lsp_client, read_framed_message, write_response};

        let server_id = ServerId::from_static("rust");
        let dir = TempDir::new().unwrap();
        let mut translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                LanguageId::from_static("rust"),
            )]))
            .with_extensions(HashMap::from([(
                FileExtension::from_static("rs"),
                LanguageId::from_static("rust"),
            )]));
        translator
            .set_workspace_roots(WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap());
        let translator = Arc::new(translator);
        let (client, mut fake_server) = fake_lsp_client();
        translator.register_client(server_id.clone(), client);

        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        notification_cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let mcp_server = McplsServer::new(
            Arc::clone(&translator),
            Arc::clone(&notification_cache),
            WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );

        let call = {
            let params = Parameters(DiagnosticsParams {
                file_path: PathBuf::from(path_str),
                context: ResultContext::None,
            });
            tokio::spawn(async move { mcp_server.get_diagnostics(params).await })
        };

        let mut wire = BufReader::new(&mut fake_server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");

        // Indexing finishes while the pull request is in flight, before the
        // response is written -- the "before" sample already ran, so this
        // must not erase the signal.
        notification_cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );

        write_response(
            &mut fake_server.read_half_stdin,
            &diag_request["id"],
            serde_json::json!({"kind": "full", "items": []}),
        )
        .await;

        let result = call.await.unwrap().unwrap();
        assert!(
            result.0.signals.indexing.indexing_in_progress,
            "the pre-pull sample must still catch a server that finished indexing mid-pull"
        );
    }

    struct DiagnosticsFixture {
        server: Arc<McplsServer>,
        cache: Arc<Mutex<NotificationCache>>,
        fake: crate::test_lsp::FakeServer,
        server_id: crate::config::ServerId,
        path: PathBuf,
        _dir: tempfile::TempDir,
    }

    fn diagnostics_fixture() -> DiagnosticsFixture {
        use std::collections::HashMap;

        use crate::config::{ServerId, ToolRouter};
        use crate::test_lsp::fake_lsp_client;

        let server_id = ServerId::from_static("rust");
        let dir = tempfile::TempDir::new().unwrap();
        let mut translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                LanguageId::from_static("rust"),
            )]))
            .with_extensions(HashMap::from([(
                FileExtension::from_static("rs"),
                LanguageId::from_static("rust"),
            )]));
        translator
            .set_workspace_roots(WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap());
        let translator = Arc::new(translator);
        let (client, fake) = fake_lsp_client();
        translator.register_client(server_id.clone(), client);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let path = dir.path().join("lib.rs");
        std::fs::write(&path, "fn main() {}").unwrap();

        let server = Arc::new(McplsServer::new(
            translator,
            Arc::clone(&cache),
            WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        ));
        DiagnosticsFixture {
            server,
            cache,
            fake,
            server_id,
            path,
            _dir: dir,
        }
    }

    /// Answers the `didOpen` + `textDocument/diagnostic` pair a `get_diagnostics`
    /// call sends, running `mid_pull` after the pull request is read and
    /// before it is answered.
    async fn serve_empty_pull<F: std::future::Future<Output = ()>>(
        fake: &mut crate::test_lsp::FakeServer,
        mid_pull: F,
    ) {
        use tokio::io::BufReader;

        use crate::test_lsp::{read_framed_message, write_response};

        let mut wire = BufReader::new(&mut fake.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        mid_pull.await;
        write_response(
            &mut fake.read_half_stdin,
            &diag_request["id"],
            serde_json::json!({"kind": "full", "items": []}),
        )
        .await;
    }

    /// #480: the route is marked push-degraded *after* the pull request was
    /// read (a respawn triggered by the pull itself) -- only the post-pull
    /// sample can see it.
    #[tokio::test]
    async fn test_get_diagnostics_flags_push_degraded_marked_mid_pull() {
        let mut fx = diagnostics_fixture();
        let call = {
            let server = Arc::clone(&fx.server);
            let params = Parameters(DiagnosticsParams {
                file_path: PathBuf::from(fx.path.to_string_lossy().into_owned()),
                context: ResultContext::None,
            });
            tokio::spawn(async move { server.get_diagnostics(params).await })
        };

        let cache = Arc::clone(&fx.cache);
        let server_id = fx.server_id.clone();
        serve_empty_pull(&mut fx.fake, async move {
            cache.lock().await.mark_push_degraded(&server_id);
        })
        .await;

        let result = call.await.unwrap().unwrap();
        assert!(
            result.0.signals.push_notifications_degraded,
            "the post-pull sample must catch a push degradation marked mid-pull"
        );
    }

    /// Serialized output of the three diagnostics readers for the fixture's file.
    async fn read_all_diagnostics_readers(
        fx: &mut DiagnosticsFixture,
    ) -> [(&'static str, serde_json::Value); 3] {
        let file_path = fx.path.clone();
        let call = {
            let server = Arc::clone(&fx.server);
            let params = Parameters(DiagnosticsParams {
                file_path: file_path.clone(),
                context: ResultContext::None,
            });
            tokio::spawn(async move { server.get_diagnostics(params).await })
        };
        serve_empty_pull(&mut fx.fake, async {}).await;
        let pulled = serde_json::to_value(call.await.unwrap().unwrap().0).unwrap();

        let cached = serde_json::to_value(
            fx.server
                .get_cached_diagnostics(Parameters(CachedDiagnosticsParams { file_path }))
                .await
                .unwrap()
                .0,
        )
        .unwrap();
        let resource = serde_json::to_value(
            fx.server
                .resource_diagnostics_response(&client_path(&fx.path))
                .await
                .unwrap(),
        )
        .unwrap();
        [
            ("get_diagnostics", pulled),
            ("get_cached_diagnostics", cached),
            ("resource", resource),
        ]
    }

    /// #480 + #504: `get_diagnostics`, `get_cached_diagnostics` and the
    /// diagnostics resource must report the same route signals under the same
    /// `snake_case` keys, checked on the serialized JSON each client sees --
    /// both when nothing is wrong and when the route is degraded.
    #[tokio::test]
    async fn test_diagnostics_readers_agree_on_route_signals() {
        let mut healthy = diagnostics_fixture();
        for (reader, json) in read_all_diagnostics_readers(&mut healthy).await {
            assert_eq!(json["push_notifications_degraded"], false, "{reader}");
            assert_eq!(json["indexing_in_progress"], false, "{reader}");
        }

        let mut fx = diagnostics_fixture();
        fx.cache.lock().await.mark_push_degraded(&fx.server_id);
        fx.cache.lock().await.observe_indexing_signal(
            &fx.server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        for (reader, json) in read_all_diagnostics_readers(&mut fx).await {
            assert_eq!(json["push_notifications_degraded"], true, "{reader}");
            assert_eq!(json["indexing_in_progress"], true, "{reader}");
            let skip: &[&str] = if reader == "resource" {
                &["diagnostics"]
            } else {
                &[]
            };
            assert_value_keys_snake_case(&json, skip);
        }
    }

    /// Fails on any object key containing an ASCII uppercase letter, except
    /// inside the subtrees named in `skip_subtrees` (verbatim LSP objects).
    fn assert_value_keys_snake_case(value: &serde_json::Value, skip_subtrees: &[&str]) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    assert!(
                        !key.chars().any(|c| c.is_ascii_uppercase()),
                        "non-snake_case key `{key}`"
                    );
                    if !skip_subtrees.contains(&key.as_str()) {
                        assert_value_keys_snake_case(child, skip_subtrees);
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    assert_value_keys_snake_case(item, skip_subtrees);
                }
            }
            _ => {}
        }
    }

    const CAMEL_CASE_ROUND_TRIP_KEY: &str = "selectionRange";

    /// Fails on any `properties` name containing an ASCII uppercase letter,
    /// recursing through every other schema keyword (`$defs`, `items`,
    /// `anyOf`, `oneOf`, ...) without checking their names, since those are
    /// type names and JSON Schema keywords rather than wire keys.
    ///
    /// `selectionRange` is the one deliberate exception: call hierarchy items are
    /// passed back verbatim as LSP `CallHierarchyItem`s.
    fn assert_schema_property_names_snake_case(schema: &serde_json::Value) {
        match schema {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    if key == "properties" {
                        let serde_json::Value::Object(properties) = child else {
                            continue;
                        };
                        for (name, property) in properties {
                            assert!(
                                name == CAMEL_CASE_ROUND_TRIP_KEY
                                    || !name.chars().any(|c| c.is_ascii_uppercase()),
                                "non-snake_case schema property `{name}`"
                            );
                            assert_schema_property_names_snake_case(property);
                        }
                    } else {
                        assert_schema_property_names_snake_case(child);
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    assert_schema_property_names_snake_case(item);
                }
            }
            _ => {}
        }
    }

    /// #504 guard. Covers: every tool `outputSchema` (property names only),
    /// the serialized `DiagnosticsResponse`/`CachedDiagnosticsResponse`/
    /// `ResourceDiagnosticsResponse` with both signals set (the resource's raw
    /// LSP `diagnostics` subtree excepted), and the `data` of every
    /// `RetryableErrorData` variant.
    #[test]
    fn test_mcpls_owned_keys_are_snake_case() {
        use crate::config::ServerId;
        use crate::error::RetryableErrorData;

        let tools = serde_json::to_value(McplsServer::build_tool_router(None).list_all()).unwrap();
        let mut visited = 0;
        for tool in tools.as_array().unwrap() {
            if let Some(schema) = tool.get("outputSchema") {
                assert_schema_property_names_snake_case(schema);
                visited += 1;
            }
        }
        assert!(visited > 0, "no outputSchema visited; the guard is a no-op");

        let signals = RouteSignals {
            push_notifications_degraded: true,
            indexing: IndexingSignal {
                indexing_in_progress: true,
            },
        };
        let degraded = Some(crate::bridge::PositionDegradation::Request);
        let pulled = serde_json::to_value(DiagnosticsResponse {
            result: DocumentDiagnosticsResult {
                diagnostics: Vec::new(),
                positions_degraded: degraded,
                enrichment: None,
            },
            availability: DiagnosticsAvailability::Published,
            origin: DiagnosticsOrigin::Pull,
            signals,
        })
        .unwrap();
        let cached = serde_json::to_value(CachedDiagnosticsResponse {
            result: DiagnosticsResult {
                diagnostics: Vec::new(),
                positions_degraded: degraded,
            },
            availability: DiagnosticsAvailability::Published,
            signals,
        })
        .unwrap();
        let resource = serde_json::to_value(ResourceDiagnosticsResponse::new(
            DocumentState::Open,
            None,
            DiagnosticsAvailability::Pending,
            signals,
        ))
        .unwrap();
        assert_value_keys_snake_case(&pulled, &[]);
        assert_value_keys_snake_case(&cached, &[]);
        assert_value_keys_snake_case(&resource, &["diagnostics"]);

        for data in [
            RetryableErrorData::WorkspaceIndexing {
                server_id: ServerId::from_static("rust"),
                elapsed_secs: 1,
            },
            RetryableErrorData::ServerInitializing {
                server_id: ServerId::from_static("rust"),
            },
            RetryableErrorData::WorkspaceServersInitializing {},
        ] {
            assert_value_keys_snake_case(&serde_json::to_value(&data).unwrap(), &[]);
        }
    }

    /// M2 regression: `get_diagnostics` must resolve the indexing-signal
    /// route from the *canonicalized* path, not the raw client-supplied one
    /// -- a symlink whose extension differs from its target (here `.txt` ->
    /// `.rs`) must still route to the same server the actual pull request
    /// canonicalizes and routes to internally, or the raw-path lookup would
    /// silently resolve no route at all (`plaintext` has none configured)
    /// and always read `indexing_in_progress: false` regardless of the real
    /// server's state.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_get_diagnostics_resolves_indexing_route_through_symlink() {
        use std::collections::HashMap;
        use std::fs;
        use std::os::unix::fs::symlink;

        use tempfile::TempDir;
        use tokio::io::BufReader;

        use crate::config::{ServerId, ToolRouter};
        use crate::test_lsp::{fake_lsp_client, read_framed_message, write_response};

        let server_id = ServerId::from_static("rust");
        let dir = TempDir::new().unwrap();
        let mut translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                LanguageId::from_static("rust"),
            )]))
            .with_extensions(HashMap::from([(
                FileExtension::from_static("rs"),
                LanguageId::from_static("rust"),
            )]));
        translator
            .set_workspace_roots(WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap());
        let translator = Arc::new(translator);
        let (client, mut fake_server) = fake_lsp_client();
        translator.register_client(server_id.clone(), client);

        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        notification_cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );

        let target = dir.path().join("target.rs");
        fs::write(&target, "fn main() {}").unwrap();
        let link = dir.path().join("link.txt");
        symlink(&target, &link).unwrap();
        let path_str = link.to_string_lossy().to_string();

        let mcp_server = McplsServer::new(
            Arc::clone(&translator),
            Arc::clone(&notification_cache),
            WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );

        let call = {
            let params = Parameters(DiagnosticsParams {
                file_path: PathBuf::from(path_str),
                context: ResultContext::None,
            });
            tokio::spawn(async move { mcp_server.get_diagnostics(params).await })
        };

        let mut wire = BufReader::new(&mut fake_server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        write_response(
            &mut fake_server.read_half_stdin,
            &diag_request["id"],
            serde_json::json!({"kind": "full", "items": []}),
        )
        .await;

        let result = call.await.unwrap().unwrap();
        assert!(
            result.0.signals.indexing.indexing_in_progress,
            "route resolution must follow the symlink to its .rs target, not \
             stop at the .txt extension of the raw client path"
        );
    }

    #[tokio::test]
    async fn test_rename_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(RenameParams {
            target: PositionParams {
                file_path: PathBuf::from(test_file.to_str().unwrap()),
                line: 10,
                character: 5,
            }
            .into(),
            new_name: NewName::try_new("new_name").unwrap(),
        });

        let result = server.rename_symbol(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_completions_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(CompletionsParams {
            position: PositionParams {
                file_path: PathBuf::from(test_file.to_str().unwrap()),
                line: 10,
                character: 5,
            },
            trigger: None,
        });

        let result = server.get_completions(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_document_symbols_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(DocumentSymbolsParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
        });

        let result = server.get_document_symbols(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_format_document_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(FormatDocumentParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
            tab_size: crate::bridge::TabSize::default(),
            insert_spaces: true,
        });

        let result = server.format_document(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_workspace_symbol_search_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(WorkspaceSymbolParams {
            query: "User".to_string(),
            kind_filter: None,
            limit: 100,
        });
        let result = server.workspace_symbol_search(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_code_actions_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(CodeActionsParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
            range: RangeParams {
                start_line: 10,
                start_character: 5,
                end_line: 10,
                end_character: 15,
            },
            kind_filter: None,
        });
        let result = server.get_code_actions(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_prepare_call_hierarchy_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(PositionParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
            line: 10,
            character: 5,
        });
        let result = server.prepare_call_hierarchy(at(params)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_incoming_calls_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let uri = url::Url::from_file_path(&test_file).unwrap().to_string();
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
        let params = Parameters(CallHierarchyCallsParams {
            item: serde_json::from_value(item).unwrap(),
        });
        let result = server.get_incoming_calls(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_outgoing_calls_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let uri = url::Url::from_file_path(&test_file).unwrap().to_string();
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
        let params = Parameters(CallHierarchyCallsParams {
            item: serde_json::from_value(item).unwrap(),
        });
        let result = server.get_outgoing_calls(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_cached_diagnostics_tool_with_params() {
        use std::fs;

        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let server = create_test_server_with_workspace_roots(
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
        );

        let params = Parameters(CachedDiagnosticsParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
        });

        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        assert!(parsed.get("diagnostics").is_some());
    }

    /// `get_cached_diagnostics` end-to-end: a cache entry stored under the
    /// canonical URI (as `diagnostics_pump` would store it) must be found when
    /// requested via a textually non-canonical path -- proving `cached_diagnostics_uri`
    /// still canonicalizes correctly after the lock-scope split, and that
    /// `diagnostics_from_cache_entry` correctly maps a populated entry through
    /// the actual tool call (not just the unit-level helpers directly).
    #[tokio::test]
    async fn test_cached_diagnostics_tool_finds_entry_via_noncanonical_path() {
        use std::fs;

        use tempfile::TempDir;
        use url::Url;

        let temp_dir = TempDir::new().unwrap();
        let subdir = temp_dir.path().join("sub");
        fs::create_dir(&subdir).unwrap();
        let test_file = subdir.join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let server = create_test_server_with_workspace_roots(
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
        );

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri =
            lsp_types::Uri::from(Url::from_file_path(&canonical_path).unwrap().as_str());
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::Error),
            code: None,
            code_description: None,
            source: None,
            message: "cached error".to_string().into(),
            related_information: None,
            tags: None,
            data: None,
        };
        {
            let mut cache = server.context.notification_cache.lock().await;
            cache.store_diagnostics(
                &crate::config::ServerId::from_static("rust"),
                &uri,
                Some(1),
                vec![diagnostic],
            );
        }

        // Textually distinct from `test_file`, but canonicalizes to the same path.
        let noncanonical = subdir.join("..").join("sub").join("test.rs");
        let params = Parameters(CachedDiagnosticsParams {
            file_path: PathBuf::from(noncanonical.to_str().unwrap()),
        });

        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        let diagnostics = parsed.get("diagnostics").unwrap().as_array().unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].get("message").unwrap(), "cached error");
    }

    /// #695: the cache tool and the resource read one snapshot, so for the same
    /// file they report the same diagnostics, availability and signals, for a
    /// cached file and for one nothing was published for.
    #[tokio::test]
    async fn test_cached_tool_and_resource_share_one_snapshot() {
        use std::fs;

        use tempfile::TempDir;
        use url::Url;

        let temp_dir = TempDir::new().unwrap();
        let cached = temp_dir.path().join("cached.rs");
        let silent = temp_dir.path().join("silent.rs");
        fs::write(&cached, "fn main() {}").unwrap();
        fs::write(&silent, "fn main() {}").unwrap();
        let server = create_test_server_with_workspace_roots(
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
        );
        let uri: lsp_types::Uri = lsp_types::Uri::from(
            Url::from_file_path(cached.canonicalize().unwrap())
                .unwrap()
                .as_str(),
        );
        server
            .context
            .notification_cache
            .lock()
            .await
            .store_diagnostics(
                &crate::config::ServerId::from_static("rust"),
                &uri,
                Some(1),
                vec![lsp_types::Diagnostic {
                    message: "cached error".to_owned().into(),
                    ..lsp_types::Diagnostic::default()
                }],
            );

        for file in [&cached, &silent] {
            let tool = server
                .get_cached_diagnostics(Parameters(CachedDiagnosticsParams {
                    file_path: file.clone(),
                }))
                .await
                .unwrap();
            let tool = serde_json::to_value(&tool.0).unwrap();
            let resource = server
                .resource_diagnostics_response(&client_path(file))
                .await
                .unwrap();
            let resource = serde_json::to_value(&resource).unwrap();

            assert_eq!(tool["availability"], resource["availability"], "{file:?}");
            assert_eq!(
                tool["diagnostics"].as_array().map(Vec::len),
                resource["diagnostics"].as_array().map(Vec::len),
                "{file:?}"
            );
            assert_eq!(
                tool["push_notifications_degraded"],
                resource["push_notifications_degraded"]
            );
        }
    }

    /// #571 RT-010: a root loaded from a config file stays addressable by its
    /// configured symlinked spelling through the tool handler.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_cached_diagnostics_tool_admits_config_loaded_symlinked_root() {
        use std::fs;

        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let base = dunce::canonicalize(temp_dir.path()).unwrap();
        let real = base.join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("main.rs"), "fn main() {}").unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let config_path = base.join("mcpls.toml");
        fs::write(
            &config_path,
            format!(
                "[workspace]\nroots = [{}]\n",
                crate::test_lsp::toml_path_literal(&link)
            ),
        )
        .unwrap();

        let config = crate::config::ServerConfig::load_from(&config_path).unwrap();
        let server = create_test_server_with_workspace_roots(
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
            WorkspaceRoots::from_configured(&config.workspace.roots).unwrap(),
        );

        let result = server
            .get_cached_diagnostics(Parameters(CachedDiagnosticsParams {
                file_path: link.join("main.rs"),
            }))
            .await;

        assert!(result.is_ok(), "rejected: {:?}", result.err());
    }

    /// #290 gap: a cache-only read must resolve the *owner* server's
    /// negotiated encoding, not silently assume UTF-16. Registers the
    /// publishing server as UTF-8 and stores a diagnostic over a real
    /// multibyte line ("héllo") so a UTF-16 assumption would produce a
    /// visibly different (wrong) column: LSP byte offset 3 is MCP column 3
    /// under the registered server's UTF-8 encoding, but would read as raw
    /// column 4 (unconverted passthrough) under the UTF-16 default tested in
    /// `test_cached_diagnostics_tool_no_owner_falls_back_to_utf16` below.
    #[tokio::test]
    async fn test_cached_diagnostics_tool_uses_registered_owner_encoding() {
        use std::fs;

        use tempfile::TempDir;
        use url::Url;

        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "héllo").unwrap();

        let server = create_test_server_with_workspace_roots(
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
        );
        let owner = crate::config::ServerId::from_static("rust");
        server.context.translator.register_server(
            owner.clone(),
            crate::lsp::LspServer::new_for_test_with_encoding(
                lsp_types::ServerCapabilities::default(),
                lsp_types::PositionEncodingKind::UTF8,
            ),
        );

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri =
            lsp_types::Uri::from(Url::from_file_path(&canonical_path).unwrap().as_str());
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 3,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::Error),
            code: None,
            code_description: None,
            source: None,
            message: "multibyte range".to_string().into(),
            related_information: None,
            tags: None,
            data: None,
        };
        {
            let mut cache = server.context.notification_cache.lock().await;
            cache.store_diagnostics(&owner, &uri, Some(1), vec![diagnostic]);
        }

        let params = Parameters(CachedDiagnosticsParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        let diagnostics = parsed.get("diagnostics").unwrap().as_array().unwrap();
        assert_eq!(
            diagnostics[0]["range"]["end"]["character"], 3,
            "byte offset 3 on \"héllo\" is UTF-16 column 3 when converted against the \
             registered UTF-8 owner"
        );
    }

    /// #599: display prose in a tool result is redacted with the secrets of
    /// the live servers, while an edit's text passes through unmodified.
    #[tokio::test]
    async fn test_structured_result_redacts_prose_with_live_server_secrets() {
        use crate::bridge::{FormatDocumentResult, HoverResult, Position2D, Range, TextEdit};
        use crate::redaction::Redactions;

        let server = create_test_server();
        let (client, _fake, _lanes) = crate::test_lsp::fake_lsp_client_with_redactions(
            Redactions::new([("API_TOKEN".to_owned(), "SuperSecretValue123".to_owned())]),
        );
        server
            .context
            .translator
            .register_client(crate::config::ServerId::from_static("rust"), client);

        let hover = server
            .structured_result(Ok(HoverResult {
                contents: "token SuperSecretValue123".to_owned(),
                range: None,
                positions_degraded: None,
            }))
            .unwrap();
        assert_eq!(hover.0.contents, "token [redacted:API_TOKEN]");

        let edits = server
            .structured_result(Ok(FormatDocumentResult {
                edits: vec![TextEdit {
                    range: Range {
                        start: Position2D {
                            line: 1,
                            character: 1,
                        },
                        end: Position2D {
                            line: 1,
                            character: 2,
                        },
                    },
                    new_text: "SuperSecretValue123".to_owned(),
                }],
                positions_degraded: None,
            }))
            .unwrap();
        assert_eq!(edits.0.edits[0].new_text, "SuperSecretValue123");
    }

    /// #583: a configured secret echoed in a pushed diagnostic never reaches
    /// the cached-diagnostics tool or the diagnostics resource, while the
    /// URIs stay intact.
    #[tokio::test]
    async fn test_diagnostics_redacted_before_tool_and_resource_reads() {
        use tempfile::TempDir;
        use url::Url;

        use crate::lsp::{LspClient, LspNotification};
        use crate::redaction::Redactions;

        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        std::fs::write(&test_file, "fn main() {}").unwrap();
        let server = create_test_server_with_workspace_roots(
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
        );
        let uri = Url::from_file_path(test_file.canonicalize().unwrap())
            .unwrap()
            .to_string();
        let range = serde_json::json!({
            "start": {"line": 0, "character": 0},
            "end": {"line": 0, "character": 1}
        });
        let mut notification = LspNotification::parse(
            "textDocument/publishDiagnostics",
            Some(serde_json::json!({
                "uri": uri,
                "diagnostics": [
                    {
                        "range": range,
                        "message": "plain SuperSecretValue123",
                        "data": {"hint": ["SuperSecretValue123"]}
                    },
                    {
                        "range": range,
                        "message": {"kind": "plaintext", "value": "markup SuperSecretValue123"},
                        "relatedInformation": [{
                            "location": {"uri": "file:///SuperSecretValue123/a.rs", "range": range},
                            "message": "related SuperSecretValue123"
                        }]
                    }
                ]
            })),
        );
        LspClient::redact_notification(
            &mut notification,
            &Redactions::new([("API_TOKEN".to_owned(), "SuperSecretValue123".to_owned())]),
        );
        let LspNotification::PublishDiagnostics(params) = notification else {
            panic!("expected publishDiagnostics");
        };
        {
            let mut cache = server.context.notification_cache.lock().await;
            cache.store_diagnostics(
                &crate::config::ServerId::from_static("rust"),
                &params.uri,
                None,
                params.diagnostics,
            );
        }

        let tool = server
            .get_cached_diagnostics(Parameters(CachedDiagnosticsParams {
                file_path: test_file.clone(),
            }))
            .await
            .unwrap();
        let tool = serde_json::to_string(&tool.0).unwrap();
        let resource = server
            .resource_diagnostics_response(&client_path(&test_file))
            .await
            .unwrap();
        let resource = serde_json::to_string(&resource).unwrap();

        let kept_uri = "file:///SuperSecretValue123/a.rs";
        assert!(resource.contains(kept_uri), "{resource}");
        for output in [tool, resource.replace(kept_uri, "")] {
            assert!(output.contains("[redacted:API_TOKEN]"), "{output}");
            assert!(!output.contains("SuperSecretValue123"), "{output}");
        }
    }

    /// Companion to the test above: when no server is registered under the
    /// cached entry's owner id (or no owner is tracked at all),
    /// `get_cached_diagnostics` must fall back to UTF-16 -- a raw,
    /// unconverted passthrough -- rather than panicking or guessing.
    #[tokio::test]
    async fn test_cached_diagnostics_tool_no_owner_falls_back_to_utf16() {
        use std::fs;

        use tempfile::TempDir;
        use url::Url;

        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "héllo").unwrap();

        let server = create_test_server_with_workspace_roots(
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
        );
        // Deliberately not registered with `translator.register_server`.
        let owner = crate::config::ServerId::from_static("rust");

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri =
            lsp_types::Uri::from(Url::from_file_path(&canonical_path).unwrap().as_str());
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 3,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::Error),
            code: None,
            code_description: None,
            source: None,
            message: "multibyte range".to_string().into(),
            related_information: None,
            tags: None,
            data: None,
        };
        {
            let mut cache = server.context.notification_cache.lock().await;
            cache.store_diagnostics(&owner, &uri, Some(1), vec![diagnostic]);
        }

        let params = Parameters(CachedDiagnosticsParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        let diagnostics = parsed.get("diagnostics").unwrap().as_array().unwrap();
        assert_eq!(
            diagnostics[0]["range"]["end"]["character"], 4,
            "with no registered owner, must fall back to UTF-16 (raw passthrough: \
             character + 1), not the UTF-8-correct column"
        );
    }

    /// #359: `get_cached_diagnostics` must surface `push_notifications_degraded`
    /// when the cached entry's owning server was marked degraded (respawned
    /// with its push notifications discarded), so a caller can tell the
    /// result may be stale rather than treating it as current.
    #[tokio::test]
    async fn test_cached_diagnostics_tool_flags_push_degraded_owner() {
        use std::collections::HashMap;
        use std::fs;

        use tempfile::TempDir;
        use url::Url;

        use crate::config::{ServerId, ToolRouter};

        // A router + extension map is required: the degraded flag is keyed
        // on `Translator::diagnostics_route_for_path` (the file's
        // *routed* server, resolved from its detected language), not on
        // `NotificationCache::diagnostics_owner` -- see #359's C1 fix. This
        // is a fast unit test of that wiring alone; the slower
        // `..._after_real_respawn` test below covers the full path through
        // an actual crash + respawn.
        let owner = ServerId::from_static("rust");
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let translator = Arc::new(
            Translator::new()
                .with_router(ToolRouter::catch_all([(
                    owner.clone(),
                    LanguageId::from_static("rust"),
                )]))
                .with_extensions(HashMap::from([(
                    FileExtension::from_static("rs"),
                    LanguageId::from_static("rust"),
                )])),
        );
        let _fake = register_fake_client(&translator, &owner);
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let server = McplsServer::new(
            translator,
            Arc::clone(&notification_cache),
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri =
            lsp_types::Uri::from(Url::from_file_path(&canonical_path).unwrap().as_str());
        {
            let mut cache = notification_cache.lock().await;
            cache.store_diagnostics(&owner, &uri, Some(1), vec![]);
            cache.mark_push_degraded(&owner);
        }

        let params = Parameters(CachedDiagnosticsParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        assert_eq!(parsed.get("push_notifications_degraded").unwrap(), true);
    }

    /// #445: `get_cached_diagnostics` must surface the same
    /// `indexing_in_progress` signal `get_diagnostics` does -- a cache-only
    /// read is exactly as vulnerable to reflecting a partial index as the
    /// pull-model one.
    #[tokio::test]
    async fn test_cached_diagnostics_tool_flags_indexing_in_progress() {
        use std::collections::HashMap;
        use std::fs;

        use tempfile::TempDir;

        use crate::config::{ServerId, ToolRouter};

        let owner = ServerId::from_static("rust");
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        notification_cache.lock().await.observe_indexing_signal(
            &owner,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = Arc::new(
            Translator::new()
                .with_router(ToolRouter::catch_all([(
                    owner.clone(),
                    LanguageId::from_static("rust"),
                )]))
                .with_extensions(HashMap::from([(
                    FileExtension::from_static("rs"),
                    LanguageId::from_static("rust"),
                )])),
        );
        let _fake = register_fake_client(&translator, &owner);
        let temp_dir = TempDir::new().unwrap();
        let server = McplsServer::new(
            translator,
            Arc::clone(&notification_cache),
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );

        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let params = Parameters(CachedDiagnosticsParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        assert_eq!(parsed.get("indexing_in_progress").unwrap(), true);
    }

    /// M2 counterpart for `get_cached_diagnostics`: route resolution must
    /// follow a symlink to its target's extension (`.txt` -> `.rs`), not
    /// stop at the raw client path's extension, or this would silently
    /// resolve no route (`plaintext` has none configured) and always read
    /// `indexing_in_progress: false` regardless of the real server's state.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_cached_diagnostics_tool_resolves_indexing_route_through_symlink() {
        use std::collections::HashMap;
        use std::fs;
        use std::os::unix::fs::symlink;

        use tempfile::TempDir;

        use crate::config::{ServerId, ToolRouter};

        let owner = ServerId::from_static("rust");
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        notification_cache.lock().await.observe_indexing_signal(
            &owner,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = Arc::new(
            Translator::new()
                .with_router(ToolRouter::catch_all([(
                    owner.clone(),
                    LanguageId::from_static("rust"),
                )]))
                .with_extensions(HashMap::from([(
                    FileExtension::from_static("rs"),
                    LanguageId::from_static("rust"),
                )])),
        );
        let _fake = register_fake_client(&translator, &owner);
        let temp_dir = TempDir::new().unwrap();
        let server = McplsServer::new(
            translator,
            Arc::clone(&notification_cache),
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );

        let target = temp_dir.path().join("target.rs");
        fs::write(&target, "fn main() {}").unwrap();
        let link = temp_dir.path().join("link.txt");
        symlink(&target, &link).unwrap();

        let params = Parameters(CachedDiagnosticsParams {
            file_path: PathBuf::from(link.to_str().unwrap()),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        assert_eq!(
            parsed.get("indexing_in_progress").unwrap(),
            true,
            "route resolution must follow the symlink to its .rs target, not \
             stop at the .txt extension of the raw client path"
        );
    }

    /// #359 regression: drives the *actual* `respawn_if_dead` path (not a
    /// hand-ordered `store_diagnostics`-then-`mark_push_degraded` call
    /// sequence, which a real respawn never produces, since
    /// `clear_server_diagnostics` removes `diagnostics_owner` for the
    /// crashed server before `mark_push_degraded` runs) and asserts through
    /// the real `get_cached_diagnostics` MCP handler that
    /// `push_notifications_degraded` comes back `true` afterward -- proving
    /// the flag is actually reachable in production, keyed on the stable
    /// routing identity rather than the cleared per-URI ownership map.
    ///
    /// `#[cfg(unix)]`: the fake LSP server is a hand-written `sh` script, no
    /// equivalent on Windows -- mirrors the gating on the respawn tests in
    /// `bridge::translator::respawn`.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_cached_diagnostics_tool_flags_push_degraded_after_real_respawn() {
        use std::collections::HashMap;
        use std::fs;

        use tempfile::TempDir;

        use crate::config::{LspServerConfig, ServerId, ToolRouter};
        use crate::lsp::{LspServer, ServerInitConfig};
        use crate::test_lsp::with_read_preamble;

        let dir = TempDir::new().unwrap();
        let script_path = dir.path().join("crash_after_init.sh");
        // Answers the `initialize` handshake, then exits ~0.3s later --
        // stands in for "was alive, then crashed" without a real language
        // server binary (same shape as `respawn::tests::respawn_tests`'
        // `write_crash_after_init_script`, duplicated here since that
        // module's test helpers are private to it).
        fs::write(
            &script_path,
            with_read_preamble(
                r#"body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
sleep 0.3
"#,
            ),
        )
        .unwrap();

        let id = ServerId::from_static("rust");
        let config = ServerInitConfig::new(
            LspServerConfig {
                language_id: LanguageId::from_static("rust"),
                command: ServerCommand::from_static("sh").into(),
                args: vec![script_path.to_string_lossy().to_string()],
                env: crate::config::ServerEnv::default(),
                file_patterns: vec![],
                initialization_options: None,
                settings: None,
                timeout_seconds: TimeoutSecs::new(5).unwrap(),
                request_timeout_seconds: TimeoutSecs::new(5).unwrap(),
                heuristics: None,
                name: Some(ServerId::from_static("rust")),
                handles: None,
                indexing: crate::bridge::IndexingPolicy::Auto,
            },
            WorkspaceRoots::default(),
            PositionEncodings::DEFAULT,
            std::sync::Arc::default(),
        );

        let seed = LspServer::spawn(config).await.unwrap();

        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let translator = Arc::new(
            Translator::new()
                .with_router(ToolRouter::catch_all([(
                    id.clone(),
                    LanguageId::from_static("rust"),
                )]))
                .with_notification_cache(Arc::clone(&notification_cache))
                .with_extensions(HashMap::from([(
                    FileExtension::from_static("rs"),
                    LanguageId::from_static("rust"),
                )])),
        );
        translator.register_server_complete(seed);

        let server = McplsServer::new(
            Arc::clone(&translator),
            Arc::clone(&notification_cache),
            WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );

        // Drive real respawn attempts (a no-op while the seed is still
        // alive) until one actually observes the crash and marks the
        // diagnostics-route server degraded -- bounded so a broken script
        // fails the test instead of hanging it.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        loop {
            let _ = translator.respawn_if_dead(&id).await;
            if notification_cache.lock().await.is_push_degraded(&id) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "server was never observed dead and respawned within the deadline"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        }

        let test_file = dir.path().join("main.rs");
        fs::write(&test_file, "fn main() {}").unwrap();
        let params = Parameters(CachedDiagnosticsParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        assert_eq!(
            parsed.get("push_notifications_degraded").unwrap(),
            true,
            "a real respawn must be observable through the actual MCP handler, got: {parsed}"
        );
    }

    /// Companion to the test above: an entry from a server that was never
    /// marked degraded must report `push_notifications_degraded: false`.
    #[tokio::test]
    async fn test_cached_diagnostics_tool_reports_not_degraded_by_default() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        std::fs::write(&test_file, "fn main() {}").unwrap();

        let server = create_test_server_with_workspace_roots(
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
        );

        let params = Parameters(CachedDiagnosticsParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        assert_eq!(parsed.get("push_notifications_degraded").unwrap(), false);
    }

    #[tokio::test]
    async fn test_cached_diagnostics_tool_nonexistent_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = create_test_server_with_workspace_roots(
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
            WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap(),
        );
        let params = Parameters(CachedDiagnosticsParams {
            file_path: PathBuf::from(
                dir.path()
                    .join("nonexistent/file.rs")
                    .to_string_lossy()
                    .to_string(),
            ),
        });

        let result = server.get_cached_diagnostics(params).await;
        let err = result.err().unwrap();
        assert!(
            err.message.contains("file I/O error"),
            "expected a file I/O error for a nonexistent path, got: {}",
            err.message
        );
    }

    /// Server whose only language server failed to start, with a real file
    /// under its workspace root.
    fn server_with_failed_rust_server() -> (tempfile::TempDir, std::path::PathBuf, McplsServer) {
        use crate::config::{ServerId, ToolRouter};
        use crate::error::{ServerSpawnFailure, StartupFailure};

        let dir = tempfile::TempDir::new().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let file = root.join("main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let id = ServerId::from_static("rust");
        let translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                id.clone(),
                LanguageId::from_static("rust"),
            )]))
            .with_extensions(crate::test_lsp::test_extensions());
        translator.record_startup_failures(&[ServerSpawnFailure {
            server_id: id,
            language_id: LanguageId::from_static("rust"),
            command: ServerCommand::from_static("rust-analyzer"),
            reason: StartupFailure::Spawn(Arc::new(crate::error::Error::ServerNotFound {
                command: crate::config::ServerCommand::from_static("rust-analyzer"),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            })),
        }]);
        translator.rebind_router(&std::collections::HashSet::new());
        translator.clear_expected_servers();

        let server = McplsServer::new(
            Arc::new(translator),
            Arc::new(Mutex::new(NotificationCache::new())),
            WorkspaceRoots::from_paths(&[root]).unwrap(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );
        (dir, file, server)
    }

    /// #535: a cached read for a language whose server failed to start
    /// reports the failure instead of an empty diagnostics list.
    #[tokio::test]
    async fn test_cached_diagnostics_tool_reports_failed_server_start() {
        let (_dir, file, server) = server_with_failed_rust_server();

        let result = server
            .get_cached_diagnostics(Parameters(CachedDiagnosticsParams {
                file_path: PathBuf::from(file.to_string_lossy().into_owned()),
            }))
            .await;

        let Err(err) = result else {
            panic!("expected the startup failure to be reported");
        };
        assert!(err.message.contains("failed to start"), "{}", err.message);
    }

    /// #535: the diagnostics resource read reports the same failure.
    #[tokio::test]
    async fn test_diagnostics_resource_reports_failed_server_start() {
        let (_dir, file, server) = server_with_failed_rust_server();

        let result = server
            .resource_diagnostics_response(&client_path(file))
            .await;

        let Err(err) = result else {
            panic!("expected the startup failure to be reported");
        };
        assert!(err.message.contains("failed to start"), "{}", err.message);
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_default_params() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 50,
            min_level: None,
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        assert!(parsed.get("logs").is_some());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_error_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 10,
            min_level: Some(LogLevel::Error),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        let logs = parsed.get("logs").unwrap().as_array().unwrap();
        assert_eq!(logs.len(), 0);
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_warning_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 100,
            min_level: Some(LogLevel::Warning),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_info_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 50,
            min_level: Some(LogLevel::Info),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_debug_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 20,
            min_level: Some(LogLevel::Debug),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_server_logs_params_reject_levels_outside_the_lowercase_enum() {
        for level in ["verbose", "ERROR", "Error", "invalid_level"] {
            let parsed = serde_json::from_value::<ServerLogsParams>(
                serde_json::json!({"limit": 10, "min_level": level}),
            );
            assert!(parsed.is_err(), "{level} was accepted");
        }
        let parsed: ServerLogsParams =
            serde_json::from_value(serde_json::json!({"min_level": "warning"})).unwrap();
        assert_eq!(parsed.min_level, Some(LogLevel::Warning));
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_zero_limit() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 0,
            min_level: None,
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        let logs = parsed.get("logs").unwrap().as_array().unwrap();
        assert_eq!(logs.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_default_params() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 20 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        assert!(parsed.get("messages").is_some());
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_custom_limit() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 5 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        let messages = parsed.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_zero_limit() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 0 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let output = result.unwrap();
        let parsed: serde_json::Value = serde_json::to_value(&output.0).unwrap();
        let messages = parsed.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_large_limit() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 1000 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_get_signature_help_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(PositionParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
            line: 10,
            character: 5,
        });

        let result = server.get_signature_help(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_go_to_implementation_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(PositionParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
            line: 10,
            character: 5,
        });

        let result = server.go_to_implementation(nav(params)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_go_to_type_definition_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(PositionParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
            line: 10,
            character: 5,
        });

        let result = server.go_to_type_definition(nav(params)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_inlay_hints_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(InlayHintsParams {
            file_path: PathBuf::from(test_file.to_str().unwrap()),
            range: RangeParams {
                start_line: 1,
                start_character: 1,
                end_line: 10,
                end_character: 1,
            },
        });

        let result = server.get_inlay_hints(params).await;
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // Tool annotation tests
    // ------------------------------------------------------------------

    /// Every registered tool must carry `ToolAnnotations` (plus the current-spec
    /// `Tool.title`) so MCP clients can decide when to skip confirmation dialogs
    /// (read-only tools) or must prompt the user (destructive tools) without
    /// invoking the tool first. Sourced from `build_tool_router(None).list_all()`
    /// (not a hand-written list of tool names). This test alone does not catch a
    /// future *mutating* tool that omits `annotations(...)`: `build_tool_router()`'s
    /// central pass (see its doc comment) blanket-labels any such tool
    /// read-only rather than leaving it `None`, so the hint assertions above
    /// always pass. `test_tool_annotation_classifications_match_intent` below
    /// forces a new mutating tool to write down an explicit classification,
    /// though it does not verify that classification is truthful.
    #[test]
    fn test_all_tools_carry_annotations() {
        let tools = McplsServer::build_tool_router(None).list_all();
        assert!(!tools.is_empty(), "no tools registered");

        for tool in &tools {
            assert!(
                tool.title.is_some(),
                "tool `{}` is missing a top-level title",
                tool.name
            );
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("tool `{}` is missing annotations", tool.name));
            assert!(
                annotations.title.is_some(),
                "tool `{}` is missing an annotations title",
                tool.name
            );
            assert!(
                annotations.read_only_hint.is_some(),
                "tool `{}` is missing read_only_hint",
                tool.name
            );
            assert!(
                annotations.destructive_hint.is_some(),
                "tool `{}` is missing destructive_hint",
                tool.name
            );
            assert!(
                annotations.idempotent_hint.is_some(),
                "tool `{}` is missing idempotent_hint",
                tool.name
            );
        }
    }

    /// Value-level regression guard for every tool's `(read_only, destructive,
    /// idempotent)` classification, sourced from the live `tool_router` (not
    /// per-tool `*_tool_attr()` calls) so the expected-tool table itself is
    /// checked against the actual registered count.
    #[test]
    fn test_tool_annotation_classifications_match_intent() {
        let tools = McplsServer::build_tool_router(None).list_all();
        let by_name: std::collections::HashMap<&str, &rmcp::model::ToolAnnotations> = tools
            .iter()
            .map(|tool| {
                (
                    tool.name.as_ref(),
                    tool.annotations
                        .as_ref()
                        .unwrap_or_else(|| panic!("tool `{}` is missing annotations", tool.name)),
                )
            })
            .collect();

        // (tool name, read_only_hint, destructive_hint, idempotent_hint)
        let expected: &[(&str, bool, bool, bool)] = &[
            ("get_hover", true, false, true),
            ("get_definition", true, false, true),
            ("get_references", true, false, true),
            ("get_diagnostics", true, false, true),
            ("rename_symbol", true, false, true),
            ("get_completions", true, false, true),
            ("get_document_symbols", true, false, true),
            ("format_document", true, false, true),
            ("workspace_symbol_search", true, false, true),
            ("get_code_actions", true, false, true),
            ("prepare_call_hierarchy", true, false, true),
            ("get_incoming_calls", true, false, true),
            ("get_outgoing_calls", true, false, true),
            ("prepare_type_hierarchy", true, false, true),
            ("get_supertypes", true, false, true),
            ("get_subtypes", true, false, true),
            ("prepare_rename", true, false, true),
            ("get_document_highlights", true, false, true),
            ("format_range", true, false, true),
            ("get_selection_ranges", true, false, true),
            ("get_folding_ranges", true, false, true),
            ("get_cached_diagnostics", true, false, true),
            ("get_server_logs", true, false, true),
            ("get_server_messages", true, false, true),
            ("get_signature_help", true, false, true),
            ("go_to_implementation", true, false, true),
            ("go_to_type_definition", true, false, true),
            ("get_inlay_hints", true, false, true),
            ("get_tool_support", true, false, true),
            ("go_to_declaration", true, false, true),
            ("restart_server", false, true, false),
        ];

        assert_eq!(
            expected.len(),
            tools.len(),
            "expected-classification table is out of sync with the registered tool count"
        );

        for (name, read_only, destructive, idempotent) in expected {
            let annotations = by_name
                .get(name)
                .unwrap_or_else(|| panic!("tool `{name}` not found in tool_router"));
            assert_eq!(
                annotations.read_only_hint,
                Some(*read_only),
                "tool `{name}` read_only_hint mismatch"
            );
            assert_eq!(
                annotations.destructive_hint,
                Some(*destructive),
                "tool `{name}` destructive_hint mismatch"
            );
            assert_eq!(
                annotations.idempotent_hint,
                Some(*idempotent),
                "tool `{name}` idempotent_hint mismatch"
            );
        }
    }

    // ------------------------------------------------------------------
    // Resource handler tests (logic-level, avoiding rmcp::service::RequestContext
    // which requires a live Peer with private fields)
    // ------------------------------------------------------------------

    /// `list_resources` returns an empty vec for a fresh translator with no open documents.
    #[tokio::test]
    async fn test_list_resources_returns_empty_when_no_open_documents() {
        let server = create_test_server();
        let empty = server.context.translator.open_document_paths().is_empty();
        assert!(empty);
    }

    // ------------------------------------------------------------------
    // `paginate_resource_paths` (pagination logic behind `list_resources`)
    // ------------------------------------------------------------------

    fn paths(n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| PathBuf::from(format!("/f{i:04}.rs")))
            .collect()
    }

    #[test]
    fn test_paginate_first_page_under_page_size_has_no_next_cursor() {
        let p = paths(5);
        let (page, next_cursor) = paginate_resource_paths(&p, None, 100).unwrap();
        assert_eq!(page.len(), 5);
        assert!(next_cursor.is_none());
    }

    #[test]
    fn test_paginate_splits_across_pages_when_over_page_size() {
        let p = paths(250);

        let (page1, cursor1) = paginate_resource_paths(&p, None, 100).unwrap();
        assert_eq!(page1.len(), 100);
        assert_eq!(page1.first(), p.first());
        assert_eq!(cursor1.as_deref(), Some("100"));

        let (page2, cursor2) = paginate_resource_paths(&p, cursor1.as_deref(), 100).unwrap();
        assert_eq!(page2.len(), 100);
        assert_eq!(page2.first(), Some(&p[100]));
        assert_eq!(cursor2.as_deref(), Some("200"));

        let (page3, cursor3) = paginate_resource_paths(&p, cursor2.as_deref(), 100).unwrap();
        assert_eq!(page3.len(), 50);
        assert_eq!(page3.first(), Some(&p[200]));
        assert!(cursor3.is_none());
    }

    #[test]
    fn test_paginate_rejects_malformed_cursor() {
        let p = paths(5);
        let result = paginate_resource_paths(&p, Some("not-a-number"), 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_paginate_out_of_range_cursor_yields_empty_page_not_error() {
        let p = paths(5);
        let (page, next_cursor) = paginate_resource_paths(&p, Some("9999"), 100).unwrap();
        assert_eq!(page.len(), 0);
        assert!(next_cursor.is_none());
    }

    /// Regression for a client-controlled cursor near `usize::MAX`: `start + page_size`
    /// must not panic (debug) or silently wrap to a bogus cursor (release).
    #[test]
    fn test_paginate_cursor_near_usize_max_does_not_overflow() {
        let p = paths(5);
        let cursor = usize::MAX.to_string();
        let (page, next_cursor) = paginate_resource_paths(&p, Some(&cursor), 100).unwrap();
        assert_eq!(page.len(), 0);
        assert!(next_cursor.is_none());
    }

    /// `list_resources` overrides `next_cursor` via struct-update syntax on top of
    /// `ListResourcesResult::with_all_items` (which always sets it to `None`) --
    /// confirm the explicit field wins and survives serialization under its
    /// wire name (`nextCursor`, camelCase per `rmcp`'s `paginated_result!`).
    #[test]
    fn test_list_resources_result_next_cursor_survives_struct_update_override() {
        let result = ListResourcesResult {
            next_cursor: Some("100".to_string()),
            ..ListResourcesResult::with_all_items(Vec::new())
        };
        assert_eq!(result.next_cursor.as_deref(), Some("100"));

        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json.get("nextCursor").unwrap(), "100");
    }

    // ------------------------------------------------------------------
    // `ResourceDiagnosticsResponse` (tracked-vs-untracked shape behind
    // `read_resource`)
    // ------------------------------------------------------------------

    fn sample_diagnostic_info(diagnostics: Vec<lsp_types::Diagnostic>) -> DiagnosticInfo {
        use url::Url;

        let uri: lsp_types::Uri =
            lsp_types::Uri::from(Url::parse("file:///sample.rs").unwrap().as_str());
        DiagnosticInfo {
            uri,
            version: Some(DocumentVersion::FIRST),
            diagnostics,
        }
    }

    #[test]
    fn test_resource_diagnostics_response_untracked_is_not_tracked_and_empty() {
        let response = ResourceDiagnosticsResponse::new(
            DocumentState::NotOpen,
            None,
            DiagnosticsAvailability::Pending,
            RouteSignals::default(),
        );
        assert!(!response.tracked);
        assert!(response.version.is_none());
        assert_eq!(response.diagnostics.len(), 0);

        // #132's contract is the wire shape, not the Rust struct -- assert the JSON directly.
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["tracked"], false);
        assert!(json["version"].is_null());
        assert_eq!(json["diagnostics"], serde_json::json!([]));
    }

    #[test]
    fn test_resource_diagnostics_response_tracked_but_no_cache_entry_is_clean() {
        let response = ResourceDiagnosticsResponse::new(
            DocumentState::Open,
            None,
            DiagnosticsAvailability::Pending,
            RouteSignals::default(),
        );
        assert!(response.tracked);
        assert!(response.version.is_none());
        assert_eq!(response.diagnostics.len(), 0);

        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["tracked"], true);
        assert!(json["version"].is_null());
        assert_eq!(json["diagnostics"], serde_json::json!([]));
    }

    #[test]
    fn test_resource_diagnostics_response_tracked_with_diagnostics() {
        let entry = sample_diagnostic_info(vec![lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::Error),
            code: None,
            code_description: None,
            source: None,
            message: "boom".to_string().into(),
            related_information: None,
            tags: None,
            data: None,
        }]);
        let response = ResourceDiagnosticsResponse::new(
            DocumentState::Open,
            Some(&entry),
            DiagnosticsAvailability::Published,
            RouteSignals::default(),
        );
        assert!(response.tracked);
        assert_eq!(response.version, Some(DocumentVersion::new(1)));
        assert_eq!(response.diagnostics.len(), 1);
        assert_eq!(response.diagnostics[0].message, "boom".into());

        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["tracked"], true);
        assert_eq!(json["version"], 1);
        assert_eq!(json["diagnostics"][0]["message"], "boom");
    }

    /// A path `read_resource` never opened reports `is_document_open() == false`
    /// -- one of the two inputs `ResourceDiagnosticsResponse::new` ORs together.
    #[tokio::test]
    async fn test_read_resource_untracked_path_is_not_open() {
        let server = create_test_server();
        let tracked = server
            .context
            .translator
            .is_document_open(std::path::Path::new("/never/opened.rs"));
        assert!(!tracked);
    }

    #[test]
    fn test_build_resource_diagnostics_response_neither_open_nor_cached_is_untracked() {
        let response = ResourceDiagnosticsResponse::new(
            DocumentState::NotOpen,
            None,
            DiagnosticsAvailability::Pending,
            RouteSignals::default(),
        );
        assert!(!response.tracked);
        assert_eq!(response.diagnostics.len(), 0);
    }

    #[test]
    fn test_build_resource_diagnostics_response_open_but_uncached_is_tracked() {
        let response = ResourceDiagnosticsResponse::new(
            DocumentState::Open,
            None,
            DiagnosticsAvailability::Pending,
            RouteSignals::default(),
        );
        assert!(response.tracked);
        assert_eq!(response.diagnostics.len(), 0);
    }

    /// Regression: an LSP server can publish diagnostics for a file mcpls never
    /// explicitly opened via `DocumentTracker` (e.g. one rust-analyzer analyzes
    /// transitively). `tracked` must still be `true` here -- deriving it from
    /// `document_open` alone would report `tracked: false` while `diagnostics`
    /// is non-empty, contradicting the documented "untracked implies empty
    /// diagnostics" contract.
    #[test]
    fn test_build_resource_diagnostics_response_cached_but_unopened_is_tracked() {
        let entry = sample_diagnostic_info(vec![lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::Warning),
            code: None,
            code_description: None,
            source: None,
            message: "transitively analyzed".to_string().into(),
            related_information: None,
            tags: None,
            data: None,
        }]);

        let response = ResourceDiagnosticsResponse::new(
            DocumentState::NotOpen,
            Some(&entry),
            DiagnosticsAvailability::Published,
            RouteSignals::default(),
        );
        assert!(
            response.tracked,
            "a cached diagnostics entry must make the response tracked, \
             even for a file that was never explicitly opened"
        );
        assert_eq!(response.diagnostics.len(), 1);
    }

    /// #359 S2: `read_resource`'s response carries the same
    /// `push_notifications_degraded` signal as `get_cached_diagnostics`, since
    /// both serve the same cache and go dark the same way after a respawn.
    #[test]
    fn test_build_resource_diagnostics_response_flags_push_degraded() {
        let response = ResourceDiagnosticsResponse::new(
            DocumentState::NotOpen,
            None,
            DiagnosticsAvailability::Pending,
            RouteSignals {
                push_notifications_degraded: true,
                indexing: IndexingSignal::default(),
            },
        );
        assert!(response.signals.push_notifications_degraded);

        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["push_notifications_degraded"], true);
    }

    /// #445 counterpart to the push-degraded test above: `read_resource`'s
    /// response carries the same `indexing_in_progress` signal
    /// `get_diagnostics`/`get_cached_diagnostics` surface.
    #[test]
    fn test_build_resource_diagnostics_response_flags_indexing_in_progress() {
        let response = ResourceDiagnosticsResponse::new(
            DocumentState::NotOpen,
            None,
            DiagnosticsAvailability::Pending,
            RouteSignals {
                push_notifications_degraded: false,
                indexing: IndexingSignal {
                    indexing_in_progress: true,
                },
            },
        );
        assert!(response.signals.indexing.indexing_in_progress);

        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["indexing_in_progress"], true);
    }

    /// `parse_uri` rejects `file://` scheme — ensures `read_resource` would return an error.
    #[test]
    fn test_read_resource_rejects_file_scheme() {
        let result = parse_uri("file:///some/file.rs");
        assert!(result.is_err());
    }

    /// `parse_uri` rejects `https://` scheme.
    #[test]
    fn test_subscribe_rejects_https_scheme() {
        let result = parse_uri("https://evil.com/file.rs");
        assert!(result.is_err());
    }

    /// Regression test for `read_resource`'s canonical-path fix: a path reached
    /// through a symlink must resolve, via `WorkspaceRoots::validate`, to the
    /// same URI as its canonical (symlink-resolved) form -- matching what
    /// `diagnostics_pump` stores from LSP notifications. Building `lsp_uri` from
    /// the raw (symlinked) path (the pre-fix behavior) would produce a
    /// mismatched cache key and always miss.
    ///
    /// Uses a real symlink rather than `..` segments: `path_to_uri` re-parses
    /// the URI string through `url::Url::parse` (for RFC 3986 char encoding),
    /// which normalizes away `..` segments regardless of platform -- so a path
    /// differing only by `..` produces the same URI as its canonical form with
    /// or without the fix. Only an actual symlink resolution (which happens in
    /// `canonicalize()`, not in URI string normalization) creates a real
    /// raw-vs-canonical difference. Unix-only: creating symlinks on Windows CI
    /// runners typically requires elevated privileges / Developer Mode.
    #[test]
    #[cfg(unix)]
    fn test_read_resource_canonical_path_matches_pump_cache_key() {
        use std::fs;
        use std::os::unix::fs::symlink;

        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        // Canonicalize the base up front so any symlink-iness already present
        // in the OS temp directory itself (e.g. macOS's `/tmp` -> `/private/tmp`)
        // doesn't leak into the comparison -- the only symlink under test is
        // `link_dir`.
        let base = temp_dir.path().canonicalize().unwrap();
        let real_dir = base.join("real");
        fs::create_dir(&real_dir).unwrap();
        let test_file = real_dir.join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let link_dir = base.join("link");
        symlink(&real_dir, &link_dir).unwrap();
        let noncanonical = link_dir.join("test.rs");
        assert_ne!(noncanonical, test_file);

        let validated = WorkspaceRoots::from_paths(&[base])
            .unwrap()
            .validate_blocking(&client_path(&noncanonical))
            .map(crate::bridge::WorkspacePath::into_path_buf)
            .unwrap();
        assert_eq!(validated, test_file.canonicalize().unwrap());

        let uri_from_raw_path = crate::bridge::path_to_uri(&noncanonical).unwrap();
        let uri_from_validated_path = crate::bridge::path_to_uri(&validated).unwrap();
        assert_ne!(
            uri_from_raw_path, uri_from_validated_path,
            "raw and canonical paths must differ here, otherwise this test can't \
             detect a regression back to keying off the raw path"
        );
    }

    /// `validate_path` rejects a non-existent path (canonicalize fails).
    #[tokio::test]
    async fn test_validate_path_rejects_nonexistent_path() {
        use crate::error::Error;

        let dir = tempfile::TempDir::new().unwrap();
        let mut translator = Translator::new();
        translator
            .set_workspace_roots(WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap());
        let result = translator
            .validate_path(&client_path(dir.path().join("this/path/does/not/exist.rs")))
            .await;
        assert_matches!(result, Err(Error::FileIo { .. }));
    }

    /// #479 regression: `read_resource`/`subscribe` must still return
    /// `INVALID_PARAMS` (`-32602`), not `INTERNAL_ERROR` (`-32603`), for a
    /// client-supplied path that doesn't exist. Exercised at the same
    /// logic level as the rest of this test group (constructing a live
    /// `rmcp::service::RequestContext` isn't possible in a unit test, see
    /// the note above "Resource handler tests"): `WorkspaceRoots::validate`
    /// is the exact call both handlers make, and `map_bridge_error` is the
    /// exact function both now pipe its `Err` through.
    #[test]
    fn test_read_resource_nonexistent_path_maps_to_invalid_params() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roots = WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap();
        let missing = temp_dir.path().join("does-not-exist.rs");

        let result = roots
            .validate_blocking(&client_path(missing))
            .map(crate::bridge::WorkspacePath::into_path_buf);
        assert_matches!(result, Err(crate::error::Error::FileIo { .. }));

        let mcp_err = map_bridge_error(result.unwrap_err());
        assert_eq!(mcp_err.code, ErrorCode::INVALID_PARAMS);
    }

    /// #575: a path that runs through a regular file (`<file>/x`) is a
    /// malformed caller path, so it is `INVALID_PARAMS`, not an internal error.
    #[test]
    fn test_path_through_a_regular_file_maps_to_invalid_params() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let file = temp_dir.path().join("main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let roots = WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap();

        let err = roots
            .validate_blocking(&client_path(file.join("x")))
            .map(crate::bridge::WorkspacePath::into_path_buf)
            .unwrap_err();

        assert_matches!(
            err,
            crate::error::Error::MalformedPath { .. } | crate::error::Error::FileIo { .. },
            "{err:?}"
        );
        assert_eq!(map_bridge_error(err).code, ErrorCode::INVALID_PARAMS);
    }

    /// [`tools_call_over_the_wire`] for `get_hover` with `arguments`.
    async fn hover_over_the_wire(
        server: McplsServer,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        let params = serde_json::json!({"name": "get_hover", "arguments": arguments});
        tools_call_over_the_wire(server, params).await
    }

    /// Serves `server` over an in-memory duplex pipe, performs the MCP
    /// handshake as a raw JSON-RPC client and returns the response to one
    /// `tools/call` with `params`, so parameter parsing and error mapping run
    /// exactly as in production.
    async fn tools_call_over_the_wire(
        server: McplsServer,
        params: serde_json::Value,
    ) -> serde_json::Value {
        use rmcp::ServiceExt as _;
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let serving = tokio::spawn(async move {
            if let Ok(running) = server.serve(tokio::io::split(server_io)).await {
                running.waiting().await.ok();
            }
        });
        let (client_read, mut client_write) = tokio::io::split(client_io);
        let mut lines = BufReader::new(client_read).lines();
        let send = |message: serde_json::Value| {
            let mut line = message.to_string();
            line.push('\n');
            line
        };

        let initialize = send(serde_json::json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"},
            },
        }));
        client_write.write_all(initialize.as_bytes()).await.unwrap();
        lines.next_line().await.unwrap().unwrap();
        let initialized =
            send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        client_write
            .write_all(initialized.as_bytes())
            .await
            .unwrap();
        let call = send(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": params,
        }));
        client_write.write_all(call.as_bytes()).await.unwrap();

        let response = loop {
            let line = lines.next_line().await.unwrap().unwrap();
            let message: serde_json::Value = serde_json::from_str(&line).unwrap();
            if message["id"] == 1 {
                break message;
            }
        };
        serving.abort();
        response
    }

    /// #575: a malformed `file_path` is reported as `-32602` by the real tool
    /// dispatch (not only by the unit-level parsing and mapping tests): empty,
    /// NUL byte, and a path through a regular file.
    #[tokio::test]
    async fn test_get_hover_malformed_file_path_is_invalid_params_over_the_wire() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let file = temp_dir.path().join("main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let through_file = file.join("x");

        for bad in [
            String::new(),
            format!("{}\u{0}x", file.display()),
            through_file.display().to_string(),
        ] {
            let roots = WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap();
            let mut translator = Translator::new();
            translator.set_workspace_roots(roots.clone());
            let server = McplsServer::new(
                Arc::new(translator),
                Arc::new(Mutex::new(NotificationCache::new())),
                roots,
                SubscriptionRegistry::new(),
                ProjectConfigStatus::NotIgnored,
                McpConfig::default(),
            );
            let arguments = serde_json::json!({"file_path": bad, "line": 1, "character": 1});

            let response = hover_over_the_wire(server, arguments).await;

            assert_eq!(response["error"]["code"], -32602, "{bad:?}: {response}");
        }
    }

    fn server_over(workspace: &Path) -> McplsServer {
        let roots = WorkspaceRoots::from_paths(&[workspace.to_path_buf()]).unwrap();
        let mut translator = Translator::new();
        translator.set_workspace_roots(roots.clone());
        McplsServer::new(
            Arc::new(translator),
            Arc::new(Mutex::new(NotificationCache::new())),
            roots,
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        )
    }

    /// #705: an argument name the tool does not declare is a parameter error
    /// (a tool-result error, like every parameter deserialization failure)
    /// naming the field and the accepted ones, before any handler runs, for
    /// each reproduction of the issue and for a nested item input.
    #[tokio::test]
    async fn test_unknown_tool_arguments_are_rejected_naming_the_field() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let file = temp_dir.path().join("main.rs").display().to_string();
        let item = serde_json::json!({
            "name": "f", "kind": 12, "uri": "file:///a.rs",
            "range": {"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 2}},
            "selectionRange": {"start": {"line": 1, "character": 1}, "end": {"line": 1, "character": 2}},
        });
        let mut nested = item.clone();
        nested["extra"] = true.into();
        let range = |extra: (&str, serde_json::Value)| {
            let mut arguments = serde_json::json!({
                "file_path": file, "start_line": 1, "start_character": 1,
                "end_line": 1, "end_character": 2,
            });
            arguments[extra.0] = extra.1;
            arguments
        };
        let cases = [
            (
                "get_code_actions",
                range(("kinds", serde_json::json!(["quickfix"]))),
                "kinds",
                "kind_filter",
            ),
            (
                "workspace_symbol_search",
                serde_json::json!({"query": "x", "kind": "function"}),
                "kind",
                "kind_filter",
            ),
            (
                "restart_server",
                serde_json::json!({"server_ids": ["rust"]}),
                "server_ids",
                "servers",
            ),
            (
                "get_hover",
                serde_json::json!({"file_path": file, "line": 1, "character": 1, "extra": 1}),
                "extra",
                "character",
            ),
            (
                "get_references",
                serde_json::json!({"file_path": file, "line": 1, "character": 1, "include": true}),
                "include",
                "include_declaration",
            ),
            (
                "get_incoming_calls",
                serde_json::json!({"item": nested}),
                "extra",
                "selectionRange",
            ),
        ];
        for (tool, arguments, unknown, accepted) in cases {
            let server = server_over(temp_dir.path());
            let params = serde_json::json!({"name": tool, "arguments": arguments});

            let response = tools_call_over_the_wire(server, params).await;

            assert_eq!(response["result"]["isError"], true, "{tool}: {response}");
            let message = response["result"]["content"][0]["text"].as_str().unwrap();
            assert!(
                message.contains(&format!("`{unknown}`")),
                "{tool}: {message}"
            );
            assert!(
                message.contains(&format!("`{accepted}`")),
                "{tool}: {message}"
            );
        }
    }

    /// #716: a client key or value of any length is bounded in the rejection.
    #[tokio::test]
    async fn test_over_long_argument_text_is_bounded_in_the_rejection() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let file = temp_dir.path().join("main.rs").display().to_string();
        let long_key = "k".repeat(2048);
        let cases = [
            serde_json::json!({"file_path": file, "line": 1, "character": 1, long_key.clone(): 1}),
            serde_json::json!({"file_path": file, "line": "z".repeat(1 << 20), "character": 1}),
        ];
        for arguments in cases {
            let params = serde_json::json!({"name": "get_hover", "arguments": arguments});

            let response = tools_call_over_the_wire(server_over(temp_dir.path()), params).await;

            assert_eq!(response["result"]["isError"], true, "{response}");
            let message = response["result"]["content"][0]["text"].as_str().unwrap();
            assert!(message.starts_with("failed to deserialize parameters:"));
            assert!(!message.contains(&long_key));
            assert!(message.len() <= crate::lsp::MAX_ERROR_MESSAGE_CALLER_BYTES + 64);
        }
    }

    /// #705: `_meta` and the other request-level fields sit beside `arguments`
    /// in `params`, so the strict argument types never see them.
    #[tokio::test]
    async fn test_request_meta_is_not_an_unknown_argument() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let file = temp_dir.path().join("main.rs").display().to_string();
        let params = serde_json::json!({
            "name": "get_hover",
            "arguments": {"file_path": file, "line": 1, "character": 1},
            "_meta": {"progressToken": "t"},
        });

        let response = tools_call_over_the_wire(server_over(temp_dir.path()), params).await;

        let message = response.to_string();
        assert!(!message.contains("unknown field"), "{message}");
    }

    /// #496 site 2 regression: a `SubscriptionError::LimitReached`, routed
    /// through `map_bridge_error` the same way `subscribe`'s handler now
    /// does, must classify as `INTERNAL_ERROR` (like `DocumentLimitExceeded`),
    /// not `INVALID_PARAMS` -- it fires on aggregate per-session tracker
    /// state, not this request's params, so it can succeed unchanged once
    /// other subscriptions are dropped.
    #[test]
    fn test_subscription_limit_reached_maps_to_internal_error() {
        let err: crate::error::Error =
            crate::bridge::resources::SubscriptionError::LimitReached.into();
        let mcp_err = map_bridge_error(err);
        assert_eq!(mcp_err.code, ErrorCode::INTERNAL_ERROR);
    }

    /// #499 regression: `unsubscribe`'s best-effort fallback to the raw
    /// request URI must still resolve to the entry `subscribe` created, even
    /// when canonicalizing the raw URI now fails because the file was
    /// deleted since subscribing. Without the alias recorded by
    /// `record_alias` at subscribe time, this fallback used to leak a capped
    /// subscription slot forever.
    #[tokio::test]
    #[cfg(unix)]
    async fn test_unsubscribe_resolves_stale_deleted_file_via_alias() {
        use std::fs;
        use std::os::unix::fs::symlink;

        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let base = temp_dir.path().canonicalize().unwrap();
        let real_dir = base.join("real");
        fs::create_dir(&real_dir).unwrap();
        let test_file = real_dir.join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let link_dir = base.join("link");
        symlink(&real_dir, &link_dir).unwrap();
        let noncanonical = link_dir.join("test.rs");

        let roots = &WorkspaceRoots::from_paths(std::slice::from_ref(&base)).unwrap();
        let validated = roots
            .validate_blocking(&client_path(&noncanonical))
            .map(crate::bridge::WorkspacePath::into_path_buf)
            .unwrap();
        let raw_uri = make_uri(&noncanonical).unwrap();
        let canonical_uri = DiagnosticsResourceUri::resolve(&raw_uri, roots)
            .unwrap()
            .uri;
        assert_eq!(canonical_uri.as_str(), make_uri(&validated).unwrap());
        assert_ne!(raw_uri, canonical_uri.as_str());

        let subscriptions = ResourceSubscriptions::new();
        subscriptions
            .subscribe(canonical_uri.clone())
            .await
            .unwrap();
        subscriptions
            .record_alias(raw_uri.clone(), &canonical_uri)
            .await;

        // Delete the file (through the real path, not the symlink) so
        // canonicalizing the symlinked path at unsubscribe time fails.
        fs::remove_file(&test_file).unwrap();
        assert!(
            roots
                .validate_blocking(&client_path(&noncanonical))
                .map(crate::bridge::WorkspacePath::into_path_buf)
                .is_err()
        );

        // Mirrors `unsubscribe`'s handler: no canonical URI once
        // canonicalization fails, only the raw one.
        assert!(DiagnosticsResourceUri::resolve(&raw_uri, roots).is_err());
        assert!(subscriptions.unsubscribe(None, &raw_uri).await.is_some());
        assert!(!subscriptions.contains(&canonical_uri).await);
    }

    #[test]
    fn test_unsubscribe_resolution_failures_split_malformed_from_unresolvable() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let roots = WorkspaceRoots::from_paths(std::slice::from_ref(&base)).unwrap();

        let malformed = DiagnosticsResourceUri::resolve("file:///a.rs", &roots).unwrap_err();
        assert!(!malformed.is_unresolvable_resource(), "{malformed:?}");

        let missing = make_uri(&base.join("gone.rs")).unwrap();
        let deleted = DiagnosticsResourceUri::resolve(&missing, &roots).unwrap_err();
        assert!(deleted.is_unresolvable_resource(), "{deleted:?}");

        let outside_path = if cfg!(windows) {
            r"C:\definitely\outside.rs"
        } else {
            "/definitely/outside.rs"
        };
        let outside = make_uri(std::path::Path::new(outside_path)).unwrap();
        let escaped = DiagnosticsResourceUri::resolve(&outside, &roots).unwrap_err();
        assert!(escaped.is_unresolvable_resource(), "{escaped:?}");

        assert!(
            !crate::error::Error::PathToUri(std::path::PathBuf::from("/x"))
                .is_unresolvable_resource()
        );
    }

    /// subscribe cap enforced: after `MAX_SUBSCRIPTIONS` entries, the next call returns `Err`.
    #[tokio::test]
    async fn test_subscription_cap_enforced_in_handler_context() {
        use crate::bridge::resources::MAX_SUBSCRIPTIONS;

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        for i in 0..MAX_SUBSCRIPTIONS {
            subscriptions
                .subscribe(DiagnosticsResourceUri::for_test(&format!(
                    "lsp-diagnostics:///file{i}.rs"
                )))
                .await
                .unwrap();
        }
        let over = subscriptions
            .subscribe(DiagnosticsResourceUri::for_test(
                "lsp-diagnostics:///overflow.rs",
            ))
            .await;
        assert!(over.is_err());
    }

    /// unsubscribing a URI that was never subscribed is a no-op (returns `false`, not an error).
    #[tokio::test]
    async fn test_unsubscribe_nonexistent_is_noop() {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let missing = DiagnosticsResourceUri::for_test("lsp-diagnostics:///nonexistent.rs");
        let removed = subscriptions
            .unsubscribe(Some(&missing), missing.as_str())
            .await;
        assert!(removed.is_none());
    }

    /// `for_new_session` gives each HTTP session its own subscription state
    /// (#478): subscribing on one instance must not be visible to another.
    #[tokio::test]
    async fn test_for_new_session_isolates_subscriptions() {
        let server = create_test_server();
        let session_a = server.for_new_session();
        let session_b = server.for_new_session();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);

        session_a
            .context
            .session
            .subscribe_for_test(
                &DiagnosticsResourceUri::for_test("lsp-diagnostics:///a.rs"),
                super::super::Target::Channel(tx),
            )
            .await
            .unwrap();

        assert!(!session_a.context.session.state().is_empty().await);
        assert!(session_b.context.session.state().is_empty().await);
        assert!(server.context.session.state().is_empty().await);
    }

    /// A session leaves the shared registry once its `McplsServer` is
    /// dropped, without any explicit close-time bookkeeping (#478).
    #[tokio::test]
    async fn test_dropped_session_subscriptions_are_reclaimed() {
        let server = create_test_server();
        let registry = server.subscription_registry();
        {
            let session = server.for_new_session();
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            session
                .context
                .session
                .subscribe_for_test(
                    &DiagnosticsResourceUri::for_test("lsp-diagnostics:///a.rs"),
                    super::super::Target::Channel(tx),
                )
                .await
                .unwrap();
            assert_eq!(registry.live_sessions().len(), 1);
        }
        assert!(registry.live_sessions().is_empty());
    }

    fn listen_test_server() -> (McplsServer, tempfile::TempDir, String) {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let file = root.join("main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let uri = make_uri(&file).unwrap();
        let server = create_test_server_with_workspace_roots(
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
            WorkspaceRoots::from_paths(&[root]).unwrap(),
        );
        (server, dir, uri)
    }

    #[test]
    fn test_accepted_subscription_filter_is_always_some_and_syntax_only() {
        let server = create_test_server();
        let mut requested = SubscriptionFilter::new();
        let missing = crate::test_lsp::absolute_uri("no/such/file.rs");
        requested.resource_subscriptions = Some(vec![missing.clone(), "file:///bad.rs".to_owned()]);
        let accepted = server.accepted_subscription_filter(&requested).unwrap();
        assert_eq!(accepted.resource_subscriptions, Some(vec![missing]));

        requested.resource_subscriptions = Some(vec![
            "lsp-diagnostics:///a.rs".to_owned();
            MAX_SUBSCRIPTIONS + 1
        ]);
        let accepted = server.accepted_subscription_filter(&requested).unwrap();
        assert_eq!(accepted.resource_subscriptions, None);
    }

    #[tokio::test]
    async fn test_prepare_listen_rejects_oversized_request() {
        let (server, _dir, uri) = listen_test_server();
        let err = server
            .prepare_listen(&vec![uri.clone(); MAX_SUBSCRIPTIONS + 1], &[uri])
            .await
            .unwrap_err();
        assert_matches!(err, crate::error::Error::ListenFilterTooLarge { .. });
        assert_eq!(map_bridge_error(err).code, ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn test_prepare_listen_rejects_oversized_total_bytes() {
        let (server, _dir, uri) = listen_test_server();
        let long = format!("lsp-diagnostics:///{}", "a".repeat(4096));
        let requested = vec![long; 100];
        let err = server.prepare_listen(&requested, &[uri]).await.unwrap_err();
        assert_matches!(err, crate::error::Error::ListenFilterTooLarge { .. });
    }

    #[tokio::test]
    async fn test_listen_join_error_maps_to_internal_error() {
        let handle = tokio::spawn(std::future::pending::<()>());
        handle.abort();
        let err = listen_join_error(handle.await.unwrap_err());
        assert_eq!(map_bridge_error(err).code, ErrorCode::INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn test_prepare_listen_requested_but_none_accepted_fails() {
        let (server, _dir, _uri) = listen_test_server();
        let bad = "file:///bad.rs".to_owned();
        let err = server
            .prepare_listen(std::slice::from_ref(&bad), &[])
            .await
            .unwrap_err();
        assert_eq!(map_bridge_error(err).code, ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn test_prepare_listen_without_requested_uris_takes_no_slot() {
        let (server, _dir, _uri) = listen_test_server();
        let registry = server.subscription_registry();
        let held: Vec<_> = (0..crate::bridge::resources::MAX_LISTEN_STREAMS)
            .map(|_| registry.try_reserve_listen().unwrap())
            .collect();
        assert!(server.prepare_listen(&[], &[]).await.unwrap().is_none());
        drop(held);
    }

    #[tokio::test]
    async fn test_prepare_listen_reports_exhaustion_as_retryable() {
        let (server, _dir, uri) = listen_test_server();
        let registry = server.subscription_registry();
        let _held: Vec<_> = (0..crate::bridge::resources::MAX_LISTEN_STREAMS)
            .map(|_| registry.try_reserve_listen().unwrap())
            .collect();
        let err = server
            .prepare_listen(std::slice::from_ref(&uri), std::slice::from_ref(&uri))
            .await
            .unwrap_err();
        assert_matches!(err, crate::error::Error::ListenStreamsExhausted { .. });
        assert_eq!(
            map_bridge_error(err).code,
            ErrorCode(crate::error::LISTEN_STREAMS_EXHAUSTED_ERROR_CODE)
        );
    }

    #[tokio::test]
    async fn test_prepare_listen_unresolvable_uris_fail_and_release_the_slot() {
        let (server, _dir, _uri) = listen_test_server();
        let missing = crate::test_lsp::absolute_uri("no/such/file.rs");
        let err = server
            .prepare_listen(
                std::slice::from_ref(&missing),
                std::slice::from_ref(&missing),
            )
            .await
            .unwrap_err();
        assert_eq!(map_bridge_error(err).code, ErrorCode::INVALID_PARAMS);
        let registry = server.subscription_registry();
        let held: Vec<_> = (0..crate::bridge::resources::MAX_LISTEN_STREAMS)
            .map(|_| registry.try_reserve_listen())
            .collect();
        assert!(held.iter().all(Result::is_ok), "the slot must be released");
    }

    #[tokio::test]
    async fn test_prepare_listen_resolves_valid_uris() {
        let (server, _dir, uri) = listen_test_server();
        let (_permit, uris) = server
            .prepare_listen(std::slice::from_ref(&uri), std::slice::from_ref(&uri))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uris.canonical().count(), 1);
    }

    /// Where the sole `rust` server stands in a [`startup_fixture`].
    #[derive(Clone, Copy)]
    enum RustServerState {
        Starting,
        FailedToStart,
    }

    struct StartupFixture {
        server: McplsServer,
        rust_file: PathBuf,
        python_file: PathBuf,
        roots: WorkspaceRoots,
        _dir: tempfile::TempDir,
    }

    /// A server whose only configured language server (`rust`) is either
    /// still starting or failed to start; `.py` files have no route.
    fn startup_fixture(state: RustServerState) -> StartupFixture {
        use std::collections::{HashMap, HashSet};

        use crate::config::{ServerId, ToolRouter};
        use crate::error::{ServerSpawnFailure, StartupFailure};

        let dir = tempfile::TempDir::new().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let rust_file = root.join("main.rs");
        let python_file = root.join("script.py");
        std::fs::write(&rust_file, "fn main() {}").unwrap();
        std::fs::write(&python_file, "pass").unwrap();

        let id = ServerId::from_static("rust");
        let mut translator = Translator::new()
            .with_extensions(HashMap::from([
                (
                    FileExtension::from_static("rs"),
                    LanguageId::from_static("rust"),
                ),
                (
                    FileExtension::from_static("py"),
                    LanguageId::from_static("python"),
                ),
            ]))
            .with_router(ToolRouter::catch_all([(
                id.clone(),
                LanguageId::from_static("rust"),
            )]));
        translator
            .set_workspace_roots(WorkspaceRoots::from_paths(std::slice::from_ref(&root)).unwrap());
        match state {
            RustServerState::Starting => translator.set_expected_servers(HashSet::from([id])),
            RustServerState::FailedToStart => {
                translator.record_startup_failures(&[ServerSpawnFailure {
                    server_id: id,
                    language_id: LanguageId::from_static("rust"),
                    command: ServerCommand::from_static("rust-analyzer"),
                    reason: StartupFailure::Spawn(Arc::new(crate::error::Error::ServerNotFound {
                        command: crate::config::ServerCommand::from_static("rust-analyzer"),
                        source: std::io::Error::from(std::io::ErrorKind::NotFound),
                    })),
                }]);
                translator.rebind_router(&HashSet::new());
            }
        }
        let server = McplsServer::new(
            Arc::new(translator),
            Arc::new(Mutex::new(NotificationCache::new())),
            WorkspaceRoots::from_paths(std::slice::from_ref(&root)).unwrap(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );
        StartupFixture {
            server,
            rust_file,
            python_file,
            roots: WorkspaceRoots::from_paths(&[root]).unwrap(),
            _dir: dir,
        }
    }

    async fn read_cached(
        fx: &StartupFixture,
        path: &Path,
    ) -> Result<Json<CachedDiagnosticsResponse>, McpError> {
        fx.server
            .get_cached_diagnostics(Parameters(CachedDiagnosticsParams {
                file_path: PathBuf::from(path.to_string_lossy().into_owned()),
            }))
            .await
    }

    /// The errors the cached-diagnostics tool and the resource read return
    /// for the fixture's Rust file.
    async fn cache_reader_errors(fx: &StartupFixture) -> [McpError; 2] {
        [
            expect_err(read_cached(fx, &fx.rust_file).await),
            expect_err(
                fx.server
                    .resource_diagnostics_response(&client_path(&fx.rust_file))
                    .await,
            ),
        ]
    }

    /// #545: a still-starting server is a retryable error on both cache
    /// readers, not an empty (apparently clean) list.
    #[tokio::test]
    async fn test_cache_readers_report_server_initializing() {
        let fx = startup_fixture(RustServerState::Starting);
        let expected = ErrorCode(crate::error::SERVER_INITIALIZING_ERROR_CODE);

        for err in cache_reader_errors(&fx).await {
            assert_eq!(err.code, expected);
        }
    }

    /// #535: a server that failed to start is reported by both cache readers
    /// with the spawn failure's detail.
    #[tokio::test]
    async fn test_cache_readers_report_server_failed_to_start() {
        let fx = startup_fixture(RustServerState::FailedToStart);

        for err in cache_reader_errors(&fx).await {
            assert!(err.message.contains("rust-analyzer"), "{}", err.message);
            assert_ne!(
                err.code,
                ErrorCode(crate::error::SERVER_INITIALIZING_ERROR_CODE)
            );
        }
    }

    /// A file whose language has no configured server still reads as an empty
    /// cache: there is no server whose failure could be reported.
    #[tokio::test]
    async fn test_cache_readers_keep_empty_result_for_unrouted_language() {
        let fx = startup_fixture(RustServerState::FailedToStart);

        let tool = read_cached(&fx, &fx.python_file).await.unwrap();
        let resource = fx
            .server
            .resource_diagnostics_response(&client_path(fx.python_file))
            .await
            .unwrap();

        assert!(tool.0.result.diagnostics.is_empty());
        assert!(resource.diagnostics.is_empty());
    }

    fn register_listen(
        fx: &StartupFixture,
        files: &[&Path],
    ) -> (
        ListenRegistration,
        Arc<ListenUris>,
        tokio::sync::mpsc::Receiver<String>,
    ) {
        let accepted: Vec<String> = files.iter().map(|f| make_uri(f).unwrap()).collect();
        let uris = Arc::new(ListenUris::resolve(&accepted, &fx.roots));
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let registration = fx
            .server
            .subscription_registry()
            .try_reserve_listen()
            .unwrap()
            .register(Arc::clone(&uris), |_| Target::Channel(tx));
        (registration, uris, rx)
    }

    /// #535 listen: when every URI's server failed to start there is nothing
    /// to stream, so the listen errors and releases its slot.
    #[tokio::test]
    async fn test_listen_settle_errors_when_every_uri_failed_to_start() {
        let fx = startup_fixture(RustServerState::FailedToStart);
        let (registration, uris, _rx) = register_listen(&fx, &[&fx.rust_file]);

        let err = fx
            .server
            .settle_listen_startup_failures(registration, &uris)
            .await
            .unwrap_err();

        assert!(err.message.contains("rust-analyzer"), "{}", err.message);
        let registry = fx.server.subscription_registry();
        let held: Vec<_> = (0..crate::bridge::resources::MAX_LISTEN_STREAMS)
            .map(|_| registry.try_reserve_listen())
            .collect();
        assert!(held.iter().all(Result::is_ok), "the slot must be released");
    }

    /// A listen mixing a failed URI with a healthy one keeps streaming and
    /// publishes exactly the failed URI, so the client re-reads the error.
    #[tokio::test]
    async fn test_listen_settle_publishes_only_failed_uris_in_a_mixed_listen() {
        let fx = startup_fixture(RustServerState::FailedToStart);
        let (registration, uris, mut rx) = register_listen(&fx, &[&fx.rust_file, &fx.python_file]);

        let registration = fx
            .server
            .settle_listen_startup_failures(registration, &uris)
            .await
            .unwrap();

        let published = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(published, make_uri(&fx.rust_file).unwrap());
        assert!(rx.try_recv().is_err(), "the healthy URI must not publish");
        drop(registration);
    }

    /// A listen whose servers are merely starting streams as normal: updates
    /// follow once they register.
    #[tokio::test]
    async fn test_listen_settle_ignores_starting_servers() {
        let fx = startup_fixture(RustServerState::Starting);
        let (registration, uris, mut rx) = register_listen(&fx, &[&fx.rust_file]);

        let registration = fx
            .server
            .settle_listen_startup_failures(registration, &uris)
            .await
            .unwrap();

        assert!(rx.try_recv().is_err());
        drop(registration);
    }

    /// Server capabilities advertise resources support.
    #[tokio::test]
    async fn test_server_capabilities_include_resources() {
        let server = create_test_server();
        let info = server.get_info();
        assert!(info.capabilities.resources.is_some());
    }

    /// Dump the current tool surface to stdout so it can be captured into
    /// `tool_surface.json`. Not part of the regular suite.
    #[test]
    #[ignore = "run manually to (re)generate tool_surface.json"]
    fn dump_tool_surface() {
        let tools = McplsServer::build_tool_router(None).list_all();
        println!("{}", serde_json::to_string_pretty(&tools).unwrap());
    }

    /// Pins the client-visible tool surface (name, description, title,
    /// annotations, input schema) exposed by `build_tool_router(None).list_all()`.
    /// `serde_json::Value` comparison, not string comparison, so key
    /// order/whitespace drift doesn't cause false failures -- only an actual
    /// change to what an MCP client sees does.
    #[test]
    fn test_tool_surface_matches_golden_snapshot() {
        let tools = McplsServer::build_tool_router(None).list_all();
        let actual = serde_json::to_value(&tools).unwrap();
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("tool_surface.json")).unwrap();
        assert_eq!(
            actual, expected,
            "client-visible tool surface changed -- update tool_surface.json only if the \
             change is intentional"
        );
    }

    /// Only the six enrichment tools declare `context`, with exactly the
    /// two closed values; `get_cached_diagnostics` must not.
    #[test]
    fn test_context_param_is_declared_only_on_enrichment_tools() {
        let tools = McplsServer::build_tool_router(None).list_all();
        let with_context: std::collections::BTreeSet<&str> = tools
            .iter()
            .filter(|tool| {
                tool.input_schema
                    .get("properties")
                    .is_some_and(|p| p.get("context").is_some())
            })
            .map(|tool| tool.name.as_ref())
            .collect();
        assert_eq!(
            with_context,
            std::collections::BTreeSet::from([
                "get_definition",
                "get_diagnostics",
                "get_references",
                "go_to_declaration",
                "go_to_implementation",
                "go_to_type_definition",
            ])
        );
        let schema = serde_json::to_string(
            &tools
                .iter()
                .find(|tool| tool.name == "get_references")
                .unwrap()
                .input_schema,
        )
        .unwrap();
        assert!(schema.contains("enclosing_symbol") && schema.contains("\"none\""));
    }

    /// `context` accepts the two closed values, defaults to `none`, and
    /// rejects anything else before a handler runs.
    #[test]
    fn test_context_param_deserialization() {
        let base = |context: Option<&str>| {
            let mut value = serde_json::json!({"file_path": "/a.rs", "line": 1, "character": 1});
            if let Some(context) = context {
                value["context"] = context.into();
            }
            value
        };
        let parse = |value| serde_json::from_value::<NavigationParams>(value);

        assert_eq!(parse(base(None)).unwrap().context, ResultContext::None);
        assert_eq!(
            parse(base(Some("enclosing_symbol"))).unwrap().context,
            ResultContext::EnclosingSymbol
        );
        assert!(parse(base(Some("bogus"))).is_err());
        assert!(serde_json::from_value::<ReferencesParams>(base(Some("bogus"))).is_err());
        assert!(
            serde_json::from_value::<DiagnosticsParams>(serde_json::json!({
                "file_path": "/a.rs",
                "context": "bogus",
            }))
            .is_err()
        );
    }

    /// Every tool advertises an `outputSchema`, i.e. every handler returns
    /// `Result<Json<T>, McpError>`.
    #[test]
    fn test_every_tool_has_output_schema() {
        let tools = McplsServer::build_tool_router(None).list_all();
        assert!(!tools.is_empty(), "no tools registered");

        for tool in &tools {
            assert!(
                tool.output_schema.is_some(),
                "tool `{}` has no output_schema",
                tool.name
            );
        }
    }

    // ------------------------------------------------------------------
    // get_tool_support tests
    // ------------------------------------------------------------------

    const CAPABILITY_REFUSAL: &str = "does not support capability";

    /// `McpTool` must name exactly the tools the macro-generated router
    /// registers, so the report can neither omit nor invent a tool.
    #[test]
    fn test_mcp_tool_catalogue_matches_declared_router() {
        let mut declared: Vec<String> = McplsServer::declared_tool_router()
            .map
            .keys()
            .map(ToString::to_string)
            .collect();
        let mut catalogued: Vec<String> = McpTool::ALL
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        declared.sort_unstable();
        catalogued.sort_unstable();
        assert_eq!(declared, catalogued);
    }

    /// Server advertising exactly `capability` (an `{}` options object where
    /// LSP has no boolean form, `true` otherwise), built from the LSP field
    /// name so it cross-checks `Capability::name` against `is_supported`.
    fn capabilities_advertising(capability: Capability) -> lsp_types::ServerCapabilities {
        let json = match capability {
            Capability::Completions | Capability::SignatureHelp => {
                serde_json::json!({ capability.name(): {} })
            }
            Capability::PrepareRename => {
                serde_json::json!({ "renameProvider": { "prepareProvider": true } })
            }
            _ => serde_json::json!({ capability.name(): true }),
        };
        serde_json::from_value(json).unwrap()
    }

    struct SupportFixture {
        server: McplsServer,
        dir: tempfile::TempDir,
        file: PathBuf,
        _fake_servers: Vec<crate::test_lsp::FakeServer>,
    }

    /// One live (registered, non-dead) catch-all server per `(id, language,
    /// capabilities)` entry, behind a real workspace file `a.rs`.
    fn support_fixture(
        servers: Vec<(&str, &str, lsp_types::ServerCapabilities)>,
        mcp: McpConfig,
    ) -> SupportFixture {
        use crate::config::{LanguageId, ServerId, ToolRouter};
        use crate::lsp::LspServer;
        use crate::test_lsp::fake_lsp_client;

        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let mut translator = Translator::new()
            .with_router(ToolRouter::catch_all(servers.iter().map(
                |(id, language, _)| {
                    (
                        ServerId::new(*id).unwrap(),
                        LanguageId::new(*language).unwrap(),
                    )
                },
            )))
            .with_extensions(
                std::collections::HashMap::from([
                    (
                        FileExtension::from_static("rs"),
                        LanguageId::from_static("rust"),
                    ),
                    (
                        FileExtension::from_static("py"),
                        LanguageId::from_static("python"),
                    ),
                ])
                .into_iter()
                .chain(servers.iter().map(|(_, language, _)| {
                    (
                        FileExtension::new(*language).unwrap(),
                        LanguageId::new(*language).unwrap(),
                    )
                }))
                .collect::<std::collections::HashMap<_, _>>(),
            );
        translator
            .set_workspace_roots(WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap());
        let mut fake_servers = Vec::new();
        for (id, _, caps) in servers {
            let (client, fake) = fake_lsp_client();
            translator.register_client(ServerId::new(id).unwrap(), client);
            translator.register_server(ServerId::new(id).unwrap(), LspServer::new_for_test(caps));
            fake_servers.push(fake);
        }
        let server = McplsServer::new(
            Arc::new(translator),
            Arc::new(Mutex::new(NotificationCache::new())),
            WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            mcp,
        );
        SupportFixture {
            server,
            dir,
            file,
            _fake_servers: fake_servers,
        }
    }

    /// Calls the real handler for `tool` with canned params against `file`,
    /// discarding the payload. The exhaustive match makes a new `McpTool`
    /// variant fail to compile until it is wired into the parity matrix.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per tool; splitting would only scatter the exhaustive match"
    )]
    async fn call_tool(server: &McplsServer, tool: McpTool, file: &Path) -> Result<(), McpError> {
        let file_path = file.to_str().unwrap().to_string();
        let position = || PositionParams {
            file_path: PathBuf::from(file_path.clone()),
            line: 1,
            character: 1,
        };
        let range = || RangeParams {
            start_line: 1,
            start_character: 1,
            end_line: 1,
            end_character: 2,
        };
        let item = || {
            let uri = url::Url::from_file_path(file).unwrap().to_string();
            let range = serde_json::json!({
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 10}
            });
            CallHierarchyCallsParams {
                item: serde_json::from_value(serde_json::json!({
                    "name": "f", "kind": 12, "uri": uri,
                    "range": range, "selectionRange": range
                }))
                .unwrap(),
            }
        };
        let type_item = || {
            let uri = url::Url::from_file_path(file).unwrap().to_string();
            let at = |character| crate::bridge::Position2D { line: 1, character };
            TypeHierarchyWalkParams {
                item: crate::bridge::HierarchyItem {
                    name: "T".to_string(),
                    kind: 5,
                    detail: None,
                    uri,
                    range: crate::bridge::Range {
                        start: at(1),
                        end: at(11),
                    },
                    selection_range: crate::bridge::Range {
                        start: at(1),
                        end: at(2),
                    },
                    data: None,
                    out_of_workspace: false,
                },
            }
        };
        match tool {
            McpTool::GetHover => server
                .get_hover(Parameters(position().into()))
                .await
                .map(|_| ()),
            McpTool::GetDefinition => server
                .get_definition(Parameters(position().into()))
                .await
                .map(|_| ()),
            McpTool::GetReferences => server
                .get_references(Parameters(ReferencesParams {
                    target: position().into(),
                    include_declaration: false,
                    context: ResultContext::None,
                }))
                .await
                .map(|_| ()),
            McpTool::GetDiagnostics => server
                .get_diagnostics(Parameters(DiagnosticsParams {
                    file_path: PathBuf::from(file_path.clone()),
                    context: ResultContext::None,
                }))
                .await
                .map(|_| ()),
            McpTool::RenameSymbol => server
                .rename_symbol(Parameters(RenameParams {
                    target: position().into(),
                    new_name: NewName::try_new("renamed").unwrap(),
                }))
                .await
                .map(|_| ()),
            McpTool::GetCompletions => server
                .get_completions(Parameters(CompletionsParams {
                    position: position(),
                    trigger: None,
                }))
                .await
                .map(|_| ()),
            McpTool::GetDocumentSymbols => server
                .get_document_symbols(Parameters(DocumentSymbolsParams {
                    file_path: PathBuf::from(file_path.clone()),
                }))
                .await
                .map(|_| ()),
            McpTool::FormatDocument => server
                .format_document(Parameters(FormatDocumentParams {
                    file_path: PathBuf::from(file_path.clone()),
                    tab_size: crate::bridge::TabSize::default(),
                    insert_spaces: true,
                }))
                .await
                .map(|_| ()),
            McpTool::WorkspaceSymbolSearch => server
                .workspace_symbol_search(Parameters(WorkspaceSymbolParams {
                    query: "main".to_string(),
                    kind_filter: None,
                    limit: 10,
                }))
                .await
                .map(|_| ()),
            McpTool::GetCodeActions => server
                .get_code_actions(Parameters(CodeActionsParams {
                    file_path: PathBuf::from(file_path.clone()),
                    range: range(),
                    kind_filter: None,
                }))
                .await
                .map(|_| ()),
            McpTool::PrepareCallHierarchy => server
                .prepare_call_hierarchy(Parameters(position().into()))
                .await
                .map(|_| ()),
            McpTool::GetIncomingCalls => server
                .get_incoming_calls(Parameters(item()))
                .await
                .map(|_| ()),
            McpTool::GetOutgoingCalls => server
                .get_outgoing_calls(Parameters(item()))
                .await
                .map(|_| ()),
            McpTool::PrepareTypeHierarchy => server
                .prepare_type_hierarchy(Parameters(position()))
                .await
                .map(|_| ()),
            McpTool::GetSupertypes => server
                .get_supertypes(Parameters(type_item()))
                .await
                .map(|_| ()),
            McpTool::GetSubtypes => server
                .get_subtypes(Parameters(type_item()))
                .await
                .map(|_| ()),
            McpTool::PrepareRename => server
                .prepare_rename(Parameters(position()))
                .await
                .map(|_| ()),
            McpTool::GetDocumentHighlights => server
                .get_document_highlights(Parameters(position()))
                .await
                .map(|_| ()),
            McpTool::GetFoldingRanges => server
                .get_folding_ranges(Parameters(FoldingRangesParams {
                    file_path: PathBuf::from(file_path.clone()),
                    kind: KindFilterInput::default(),
                }))
                .await
                .map(|_| ()),
            McpTool::GetSelectionRanges => server
                .get_selection_ranges(Parameters(position()))
                .await
                .map(|_| ()),
            McpTool::FormatRange => server
                .format_range(Parameters(FormatRangeParams {
                    file_path: PathBuf::from(file_path.clone()),
                    range: range(),
                    tab_size: crate::bridge::TabSize::default(),
                    insert_spaces: true,
                }))
                .await
                .map(|_| ()),
            McpTool::GetCachedDiagnostics => server
                .get_cached_diagnostics(Parameters(CachedDiagnosticsParams {
                    file_path: PathBuf::from(file_path.clone()),
                }))
                .await
                .map(|_| ()),
            McpTool::GetServerLogs => server
                .get_server_logs(Parameters(ServerLogsParams {
                    limit: 1,
                    min_level: None,
                }))
                .await
                .map(|_| ()),
            McpTool::GetServerMessages => server
                .get_server_messages(Parameters(ServerMessagesParams { limit: 1 }))
                .await
                .map(|_| ()),
            McpTool::GetSignatureHelp => server
                .get_signature_help(Parameters(position()))
                .await
                .map(|_| ()),
            McpTool::GoToImplementation => server
                .go_to_implementation(Parameters(position().into()))
                .await
                .map(|_| ()),
            McpTool::GoToDeclaration => server
                .go_to_declaration(Parameters(position().into()))
                .await
                .map(|_| ()),
            McpTool::GoToTypeDefinition => server
                .go_to_type_definition(Parameters(position().into()))
                .await
                .map(|_| ()),
            McpTool::GetInlayHints => server
                .get_inlay_hints(Parameters(InlayHintsParams {
                    file_path: PathBuf::from(file_path.clone()),
                    range: range(),
                }))
                .await
                .map(|_| ()),
            McpTool::GetToolSupport => server
                .get_tool_support(Parameters(ToolSupportParams::default()))
                .await
                .map(|_| ()),
            McpTool::RestartServer => server
                .restart_server(Parameters(
                    serde_json::from_value(serde_json::json!({"servers": ["unconfigured"]}))
                        .unwrap(),
                ))
                .await
                .map(|_| ()),
        }
    }

    /// The report and enforcement must agree for every tool under every
    /// single-capability server: a tool is reported `capability_not_advertised`
    /// iff its real handler is refused with `CapabilityNotSupported`.
    /// Advertising one capability at a time (plus none) is what catches a
    /// tool mapped to the wrong `ToolKind`, which an empty-capability table
    /// cannot. The client is live because `respawn_if_dead` runs before
    /// `require_capability`; tools passing the gate then fail on the silent
    /// fake server, which is irrelevant to the comparison.
    #[tokio::test(start_paused = true)]
    async fn test_tool_support_report_matches_enforcement_for_every_capability() {
        use crate::bridge::RouteSupport;

        let advertised_cases = std::iter::once(None).chain(Capability::ALL.map(Some));
        for advertised in advertised_cases {
            let caps = advertised.map_or_else(
                lsp_types::ServerCapabilities::default,
                capabilities_advertising,
            );
            let fixture =
                support_fixture(vec![("rust", "rust", caps.clone())], McpConfig::default());
            let snapshot = fixture.server.context.translator.tool_support_snapshot();

            for tool in McpTool::ALL {
                let refused = call_tool(&fixture.server, tool, &fixture.file)
                    .await
                    .is_err_and(|e| e.message.contains(CAPABILITY_REFUSAL));
                let (reported, expected_refusal) = match tool.spec().backend {
                    ToolBackend::Local => (None, false),
                    ToolBackend::Document(kind) => (
                        Some(snapshot.document_support_gated(
                            &LanguageId::from_static("rust"),
                            kind,
                            tool.capability(),
                        )),
                        tool.capability()
                            .is_some_and(|cap| !cap.is_supported(&caps)),
                    ),
                    ToolBackend::Workspace(kind) => (
                        Some(snapshot.workspace_support(kind)),
                        tool.capability()
                            .is_some_and(|cap| !cap.is_supported(&caps)),
                    ),
                };
                let report_says_refused =
                    matches!(reported, Some(RouteSupport::CapabilityNotAdvertised { .. }));
                assert_eq!(
                    refused,
                    report_says_refused,
                    "{} with only {advertised:?} advertised: handler refused={refused}, \
                     report={reported:?}",
                    tool.name()
                );
                assert_eq!(
                    refused,
                    expected_refusal,
                    "{} with only {advertised:?} advertised",
                    tool.name()
                );
                assert!(
                    !matches!(
                        reported,
                        Some(RouteSupport::Initializing | RouteSupport::NoServer)
                    ),
                    "{} reported {reported:?} for a live server",
                    tool.name()
                );
            }
        }
    }

    async fn report_json(server: &McplsServer, file_path: Option<PathBuf>) -> serde_json::Value {
        let text = server
            .get_tool_support(Parameters(ToolSupportParams { file_path }))
            .await
            .unwrap();
        serde_json::to_value(&text.0).unwrap()
    }

    fn tool_entry<'a>(report: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
        report["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("tool `{name}` missing from report"))
    }

    #[tokio::test]
    async fn test_get_tool_support_distinguishes_all_some_none_and_always() {
        let hover = lsp_types::ServerCapabilities {
            hover_provider: Some(lsp_types::HoverProvider::Bool(true)),
            ..Default::default()
        };
        let fixture = support_fixture(
            vec![
                ("rust-srv", "rust", hover),
                ("py-srv", "python", lsp_types::ServerCapabilities::default()),
            ],
            McpConfig::default(),
        );
        let report = report_json(&fixture.server, None).await;

        assert_eq!(report["languages"], serde_json::json!(["python", "rust"]));
        assert_eq!(report["tools"].as_array().unwrap().len(), 31);

        let hover = tool_entry(&report, "get_hover");
        assert_eq!(hover["coverage"], "some");
        assert_eq!(
            hover["routes"],
            serde_json::json!([
                {"languages": ["python"], "status": "capability_not_advertised",
                 "server": "py-srv", "capability": "hoverProvider"},
                {"languages": ["rust"], "status": "supported", "server": "rust-srv"},
            ])
        );

        let diagnostics = tool_entry(&report, "get_diagnostics");
        assert_eq!(diagnostics["coverage"], "all");
        assert_eq!(
            diagnostics["routes"],
            serde_json::json!([
                {"languages": ["python"], "status": "push_only", "server": "py-srv"},
                {"languages": ["rust"], "status": "push_only", "server": "rust-srv"},
            ]),
            "servers advertising no diagnosticProvider answer from the push cache"
        );

        let workspace = tool_entry(&report, "workspace_symbol_search");
        assert_eq!(workspace["coverage"], "none");
        assert!(workspace["routes"][0].get("languages").is_none());

        let logs = tool_entry(&report, "get_server_logs");
        assert_eq!(logs["coverage"], "always");
        assert!(logs.get("routes").is_none());
    }

    #[tokio::test]
    async fn test_get_tool_support_file_path_restricts_languages() {
        let fixture = support_fixture(
            vec![
                ("rust-srv", "rust", lsp_types::ServerCapabilities::default()),
                ("py-srv", "python", lsp_types::ServerCapabilities::default()),
            ],
            McpConfig::default(),
        );
        let report = report_json(&fixture.server, Some(PathBuf::from(&fixture.file))).await;
        assert_eq!(report["languages"], serde_json::json!(["rust"]));

        let outside = fixture
            .server
            .get_tool_support(Parameters(ToolSupportParams {
                file_path: Some(PathBuf::from("/definitely/not/in/workspace.rs")),
            }))
            .await;
        assert!(outside.is_err());
        drop(fixture.dir);
    }

    #[tokio::test]
    async fn test_get_tool_support_uses_prefixed_tool_names() {
        let mcp = McpConfig {
            tool_prefix: Some("p".parse().unwrap()),
            ..McpConfig::default()
        };
        let fixture = support_fixture(
            vec![("rust", "rust", lsp_types::ServerCapabilities::default())],
            mcp,
        );
        let report = report_json(&fixture.server, None).await;
        assert_eq!(tool_entry(&report, "p_get_hover")["coverage"], "none");
    }

    /// #676: the languages are those a file can be detected as through the
    /// map built from the config, among them an extensionless name and the
    /// derived `typescriptreact`.
    #[tokio::test]
    async fn test_get_tool_support_lists_languages_through_the_config_built_map() {
        use crate::config::{ServerConfig, ToolRouter};

        let config: ServerConfig = toml::from_str(
            r#"
            [[lsp_servers]]
            language_id = "typescript"
            command = "typescript-language-server"
            file_patterns = ["**/*.ts", "**/*.tsx"]

            [[lsp_servers]]
            language_id = "make"
            command = "make-lsp"
            file_patterns = ["**/Makefile", "**/*.mk"]
            "#,
        )
        .unwrap();
        let translator = Translator::new()
            .with_extensions(config.build_effective_language_map())
            .with_router(ToolRouter::from_configs(config.lsp_servers.iter()).unwrap());
        let dir = tempfile::TempDir::new().unwrap();
        let server = McplsServer::new(
            Arc::new(translator),
            Arc::new(Mutex::new(NotificationCache::new())),
            WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );

        let report = report_json(&server, None).await;

        assert_eq!(
            report["languages"],
            serde_json::json!(["make", "typescript", "typescriptreact"])
        );
    }

    #[tokio::test]
    async fn test_get_tool_support_with_nothing_configured_reports_no_languages() {
        let server = create_test_server();
        let report = report_json(&server, None).await;
        assert_eq!(report["languages"], serde_json::json!([]));
        assert_eq!(tool_entry(&report, "get_hover")["coverage"], "none");
        assert_eq!(tool_entry(&report, "get_server_logs")["coverage"], "always");
    }

    /// A language whose only server failed to spawn is still listed, as
    /// `no_server`, rather than silently dropped from the report.
    #[tokio::test]
    async fn test_get_tool_support_lists_language_whose_server_failed_to_spawn() {
        use std::collections::HashSet;

        use crate::config::ServerId;

        let fixture = support_fixture(
            vec![
                ("rust", "rust", lsp_types::ServerCapabilities::default()),
                ("py-srv", "python", lsp_types::ServerCapabilities::default()),
            ],
            McpConfig::default(),
        );
        fixture
            .server
            .context
            .translator
            .rebind_router(&HashSet::from([ServerId::from_static("rust")]));
        let report = report_json(&fixture.server, None).await;
        assert_eq!(report["languages"], serde_json::json!(["python", "rust"]));
        assert_eq!(
            tool_entry(&report, "get_hover")["routes"][0],
            serde_json::json!({"languages": ["python"], "status": "no_server"})
        );
    }

    /// Languages sharing one outcome collapse into a single route entry.
    #[tokio::test]
    async fn test_get_tool_support_groups_languages_with_identical_status() {
        let none = lsp_types::ServerCapabilities::default;
        let fixture = support_fixture(
            vec![
                ("a-srv", "alpha", none()),
                ("b-srv", "beta", none()),
                ("c-srv", "gamma", none()),
            ],
            McpConfig::default(),
        );
        let report = report_json(&fixture.server, None).await;
        let routes = tool_entry(&report, "get_hover")["routes"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(routes.len(), 3);

        fixture
            .server
            .context
            .translator
            .rebind_router(&std::collections::HashSet::new());
        let report = report_json(&fixture.server, None).await;
        assert_eq!(
            tool_entry(&report, "get_hover")["routes"],
            serde_json::json!([
                {"languages": ["alpha", "beta", "gamma"], "status": "no_server"}
            ])
        );
    }

    /// With every other configured server advertising nothing, each
    /// capability advertised by the one target server (mid config order)
    /// must be reported and enforced for exactly that server's language.
    #[tokio::test(start_paused = true)]
    async fn test_tool_support_report_matches_enforcement_with_one_server_among_many() {
        use crate::bridge::RouteSupport;

        for advertised in Capability::ALL {
            let fixture = support_fixture(
                vec![
                    ("a-srv", "alpha", lsp_types::ServerCapabilities::default()),
                    ("rust", "rust", capabilities_advertising(advertised)),
                    ("z-srv", "zeta", lsp_types::ServerCapabilities::default()),
                ],
                McpConfig::default(),
            );
            let snapshot = fixture.server.context.translator.tool_support_snapshot();

            for tool in McpTool::ALL {
                let refused = call_tool(&fixture.server, tool, &fixture.file)
                    .await
                    .is_err_and(|e| e.message.contains(CAPABILITY_REFUSAL));
                let reported = match tool.spec().backend {
                    ToolBackend::Local => continue,
                    ToolBackend::Document(kind) => snapshot.document_support_gated(
                        &LanguageId::from_static("rust"),
                        kind,
                        tool.capability(),
                    ),
                    ToolBackend::Workspace(kind) => snapshot.workspace_support(kind),
                };
                assert_eq!(
                    refused,
                    matches!(reported, RouteSupport::CapabilityNotAdvertised { .. }),
                    "{} with only {advertised:?} on rust: refused={refused}, report={reported:?}",
                    tool.name()
                );
                match tool.spec().backend {
                    ToolBackend::Document(_) => assert_eq!(
                        refused,
                        tool.capability().is_some_and(
                            |cap| !cap.is_supported(&capabilities_advertising(advertised))
                        ),
                        "{} with only {advertised:?} on rust",
                        tool.name()
                    ),
                    // `resolve_any` picks the first catch-all in config order,
                    // which advertises nothing, whatever "rust" advertises.
                    ToolBackend::Workspace(_) => assert_eq!(
                        reported,
                        RouteSupport::CapabilityNotAdvertised {
                            server: crate::config::ServerId::from_static("a-srv"),
                            capability: Capability::WorkspaceSymbols,
                        }
                    ),
                    ToolBackend::Local => {}
                }
            }
        }
    }

    /// Before registration completes, an expected server reads as
    /// `initializing`/`unknown`, not as unsupported.
    #[tokio::test]
    async fn test_get_tool_support_reports_expected_unregistered_server_as_unknown() {
        use std::collections::HashSet;

        use crate::config::{ServerId, ToolRouter};

        let translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                ServerId::from_static("rust"),
                LanguageId::from_static("rust"),
            )]))
            .with_extensions(std::collections::HashMap::from([(
                FileExtension::from_static("rs"),
                LanguageId::from_static("rust"),
            )]));
        translator.set_expected_servers(HashSet::from([ServerId::from_static("rust")]));
        let server = McplsServer::new(
            Arc::new(translator),
            Arc::new(Mutex::new(NotificationCache::new())),
            WorkspaceRoots::default(),
            SubscriptionRegistry::new(),
            ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );
        let report = report_json(&server, None).await;
        let hover = tool_entry(&report, "get_hover");
        assert_eq!(hover["coverage"], "unknown");
        assert_eq!(hover["routes"][0]["status"], "initializing");
    }

    // ------------------------------------------------------------------
    // tool_prefix tests
    // ------------------------------------------------------------------

    /// Pins that `ToolRouter::disable_route`/`with_disabled` are unreachable
    /// today, which is what makes `build_tool_router`'s rename pass safe to
    /// skip re-keying the private `disabled` set (see its doc comment). If
    /// this ever fails, a `disable_route` call was added somewhere and the
    /// rename pass needs to be updated to preserve disabled state across
    /// the rekey.
    #[test]
    fn test_no_route_is_ever_disabled() {
        let router = McplsServer::build_tool_router(None);
        for name in router.map.keys() {
            assert!(
                !router.is_disabled(name),
                "route {name} is unexpectedly disabled"
            );
        }
    }

    /// Every currently-registered tool name must fit within
    /// `MAX_TOOL_NAME_BYTES`, the constant the `MAX_MCP_TOOL_PREFIX_BYTES`
    /// safety margin is derived from (see the compile-time assertion next
    /// to it). A future tool name that grows past this fails here, with a
    /// clear pointer to `MAX_TOOL_NAME_BYTES`, rather than surfacing as a
    /// confusing prefix-length failure far away in `config`.
    #[test]
    fn test_registered_tool_names_fit_max_tool_name_bytes() {
        let router = McplsServer::build_tool_router(None);
        for tool in router.list_all() {
            assert!(
                tool.name.len() <= MAX_TOOL_NAME_BYTES,
                "tool `{}` is {} bytes, exceeding MAX_TOOL_NAME_BYTES ({MAX_TOOL_NAME_BYTES}); \
                 bump MAX_TOOL_NAME_BYTES and re-check its compile-time assertion against \
                 MAX_MCP_TOOL_PREFIX_BYTES",
                tool.name,
                tool.name.len()
            );
        }
    }

    /// A configured prefix rewrites only `name` -- every other field
    /// (description, title, annotations, input schema, output schema) stays
    /// identical to the unprefixed golden snapshot, and no route is gained
    /// or lost.
    #[test]
    fn test_build_tool_router_with_prefix_renames_only_name() {
        let prefix: ToolPrefix = "optics".parse().unwrap();
        let unprefixed = McplsServer::build_tool_router(None).list_all();
        let prefixed = McplsServer::build_tool_router(Some(&prefix)).list_all();

        assert_eq!(unprefixed.len(), prefixed.len());
        for (before, after) in unprefixed.iter().zip(prefixed.iter()) {
            assert_eq!(after.name, format!("optics_{}", before.name));
            assert_eq!(after.description, before.description);
            assert_eq!(after.title, before.title);
            assert_eq!(after.annotations, before.annotations);
            assert_eq!(after.input_schema, before.input_schema);
            assert_eq!(after.output_schema, before.output_schema);
        }
    }

    /// The map key for every route must equal its own `attr.name` --
    /// `ToolRouter::call` dispatches by looking up `map` with the
    /// requested name, so a mismatch here would silently break routing to
    /// unreachable dead entries under their pre-rename key.
    #[test]
    fn test_build_tool_router_map_keys_match_route_names() {
        let prefix: ToolPrefix = "optics".parse().unwrap();
        let router = McplsServer::build_tool_router(Some(&prefix));
        for (key, route) in &router.map {
            assert_eq!(key.as_ref(), route.attr.name.as_ref());
        }
    }

    /// The single test proving the prefix threads end-to-end from the
    /// public `McplsServer::new` constructor, not just from calling
    /// `build_tool_router` directly -- every other test above calls
    /// `build_tool_router` in isolation, so this is the one that would
    /// catch a wiring mistake in `new` (e.g. forgetting to read
    /// `mcp.tool_prefix` before `mcp` is moved into `BridgeContext::new`).
    #[tokio::test]
    async fn test_new_threads_configured_tool_prefix_end_to_end() {
        let mcp = McpConfig {
            tool_prefix: Some("p".parse().unwrap()),
            ..McpConfig::default()
        };
        let server = create_test_server_with_mcp_config(ProjectConfigStatus::NotIgnored, mcp);

        assert!(server.get_tool("p_get_hover").is_some());
        assert!(server.get_tool("get_hover").is_none());
    }
}
