//! MCP server implementation using rmcp.
//!
//! This module provides the MCP server that exposes LSP capabilities
//! as MCP tools using the rmcp SDK.

use std::borrow::Cow;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::FutureExt as _;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::{Json, Parameters};
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
use super::session::{ListenPermit, ListenUris, SubscriptionRegistry, Target};
use super::tools::{
    CachedDiagnosticsParams, CallHierarchyCallsParams, CodeActionsParams, CompletionsParams,
    DiagnosticsParams, DocumentSymbolsParams, FormatDocumentParams, InlayHintsParams,
    PositionParams, RangeParams, ReferencesParams, RenameParams, ServerLogsParams,
    ServerMessagesParams, WorkspaceSymbolParams,
};
use crate::bridge::resources::{
    DiagnosticsResourceUri, MAX_SUBSCRIPTIONS, ResolvedResource, make_uri, parse_uri,
};
use crate::bridge::{
    DefinitionResult, DiagnosticInfo, DiagnosticsResult, DocumentSymbolsResult, IndexingState,
    NotificationCache, Position, PositionEncoding, ReferencesResult, Translator,
    validate_path_against_roots,
};
use crate::config::{McpConfig, ToolPrefix};

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

/// Byte length of the longest tool name currently registered
/// (`workspace_symbol_search`), used only to keep
/// [`crate::config::MAX_MCP_TOOL_PREFIX_BYTES`] safe (see the compile-time
/// assertion below). Bumping this when a longer tool name is added is
/// always safe on its own; the assertion is what catches the case where
/// that growth would no longer leave enough room for the configured prefix.
const MAX_TOOL_NAME_BYTES: usize = 23;

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

macro_rules! positions_note_response {
    () => {
        "`positions_degraded: \"response\"` (non-UTF-16 servers only) means some returned `character` offsets may be inexact."
    };
}

/// Route-level health signals shared by every diagnostics reader
/// (`get_diagnostics`, `get_cached_diagnostics`, and the diagnostics
/// resource), so the three can never report different keys or semantics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, JsonSchema)]
struct DiagnosticsRouteSignals {
    /// `true` if the language server publishing this file's diagnostics crashed and was
    /// restarted during this mcpls session. Diagnostics it delivers only by push (e.g.
    /// rust-analyzer's flycheck/clippy) are no longer received, so the returned
    /// diagnostics may be incomplete until mcpls restarts.
    push_notifications_degraded: bool,
    /// `true` if the routed language server reported its initial workspace indexing as
    /// in progress during this read; the diagnostics may reflect a partial index.
    indexing_in_progress: bool,
}

impl DiagnosticsRouteSignals {
    // Keyed on the routing identity, not cache ownership, which a respawn clears (#359).
    fn sample(cache: &NotificationCache, route_id: Option<&crate::config::ServerId>) -> Self {
        Self {
            push_notifications_degraded: route_id.is_some_and(|id| cache.is_push_degraded(id)),
            indexing_in_progress: route_id
                .is_some_and(|id| cache.indexing_state(id) == IndexingState::Loading),
        }
    }

    /// Combines two samples taken at different times: a signal is set if it was set in
    /// either.
    const fn union(self, later: Self) -> Self {
        Self {
            push_notifications_degraded: self.push_notifications_degraded
                || later.push_notifications_degraded,
            indexing_in_progress: self.indexing_in_progress || later.indexing_in_progress,
        }
    }
}

/// Response shape for the `get_cached_diagnostics` tool: the shared diagnostics result plus
/// the file's [`DiagnosticsRouteSignals`].
#[derive(serde::Serialize)]
struct CachedDiagnosticsResponse {
    #[serde(flatten)]
    result: DiagnosticsResult,
    #[serde(flatten)]
    signals: DiagnosticsRouteSignals,
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
// By-value `e` matches `Result::map_err`'s `FnOnce(E) -> F`, letting this be
// passed directly as `.map_err(map_bridge_error)` at every call site.
#[allow(clippy::needless_pass_by_value)]
fn map_bridge_error(e: crate::error::Error) -> McpError {
    let message = e.to_string();
    match e.mcp_error_kind() {
        crate::error::McpErrorKind::InvalidParams => McpError::invalid_params(message, None),
        crate::error::McpErrorKind::Internal => McpError::internal_error(message, None),
        crate::error::McpErrorKind::Retryable(data) => match serde_json::to_value(&data) {
            Ok(value) => McpError::new(ErrorCode(data.code()), message, Some(value)),
            Err(e) => {
                tracing::error!(error = %e, "failed to serialize retryable error data");
                McpError::new(ErrorCode(data.code()), message, None)
            }
        },
    }
}

/// Map a bridge-layer result to the MCP tool response shape shared by every `#[tool]` handler.
fn to_tool_result<T: serde::Serialize>(
    result: crate::error::Result<T>,
) -> Result<String, McpError> {
    match result {
        Ok(value) => serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
        Err(e) => Err(map_bridge_error(e)),
    }
}

/// Map a bridge-layer result to a structured MCP tool response (`structuredContent` plus the
/// legacy `content` text block, per the MCP spec's backwards-compat shape).
///
/// The handler's own return type -- not this helper's -- is what the `#[tool]` macro reads to
/// derive `outputSchema`; it must spell `Result<Json<T>, McpError>` literally (no alias) for the
/// macro to detect it. See `Json<T>`'s `IntoCallToolResult` impl, which this helper relies on.
fn to_structured_tool_result<T: Serialize + JsonSchema>(
    result: crate::error::Result<T>,
) -> Result<Json<T>, McpError> {
    match result {
        Ok(value) => Ok(Json(value)),
        Err(e) => Err(map_bridge_error(e)),
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
    let page = &rest[..rest.len().min(page_size)];
    // `start` is client-controlled (parsed straight from the cursor), so the
    // addition must not panic (debug) or silently wrap (release) for a
    // cursor near `usize::MAX`.
    let next_start = start.saturating_add(page_size);
    let next_cursor = (next_start < paths.len()).then(|| next_start.to_string());

    Ok((page, next_cursor))
}

/// `get_diagnostics`'s response shape.
///
/// Wraps `DiagnosticsResult` with the route's [`DiagnosticsRouteSignals`]:
/// `handle_diagnostics` deliberately stays ungated on workspace-indexing readiness (#445 -- it
/// reads from the notification-cache poll path, not a live whole-workspace LSP request, so
/// blocking it the way `IndexingGate::Required` blocks hover/definition/etc. would be the
/// wrong fix shape for this one call site; see `routing::IndexingGate`'s doc). Instead this
/// flags the result rather than silently returning what can read as "no errors" while the
/// routed server is still loading.
#[derive(serde::Serialize, JsonSchema)]
struct DiagnosticsResponse {
    #[serde(flatten)]
    result: DiagnosticsResult,
    #[serde(flatten)]
    signals: DiagnosticsRouteSignals,
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
/// The shared [`DiagnosticsRouteSignals`] mirror `get_cached_diagnostics`
/// (#359): a push-degraded route means `subscribe`'s replay and the pump's
/// `notify_resource_updated` calls for it have gone dark until the whole
/// mcpls process restarts, same as this cache-only read.
#[derive(serde::Serialize)]
struct ResourceDiagnosticsResponse {
    tracked: bool,
    version: Option<i32>,
    diagnostics: Vec<lsp_types::Diagnostic>,
    #[serde(flatten)]
    signals: DiagnosticsRouteSignals,
}

impl ResourceDiagnosticsResponse {
    fn new(
        tracked: bool,
        entry: Option<&DiagnosticInfo>,
        signals: DiagnosticsRouteSignals,
    ) -> Self {
        Self {
            tracked,
            version: entry.and_then(|e| e.version),
            diagnostics: entry.map(|e| e.diagnostics.clone()).unwrap_or_default(),
            signals,
        }
    }
}

/// Build `read_resource`'s response for a file. `tracked` is true when the
/// file is currently open via `DocumentTracker` (`document_open`) *or* the
/// diagnostics cache already holds an entry for it (`entry.is_some()`) --
/// not `document_open` alone: an LSP server publishes
/// `textDocument/publishDiagnostics` for whatever it analyzes, including
/// files mcpls never explicitly opened (e.g. one rust-analyzer pulls in
/// transitively), so `document_open` alone could report `tracked: false`
/// while `diagnostics` is still non-empty, contradicting the documented
/// "untracked implies empty diagnostics" contract.
fn build_resource_diagnostics_response(
    document_open: bool,
    entry: Option<&DiagnosticInfo>,
    signals: DiagnosticsRouteSignals,
) -> ResourceDiagnosticsResponse {
    ResourceDiagnosticsResponse::new(document_open || entry.is_some(), entry, signals)
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
    /// `project_config_ignored` reports whether a CWD-discovered
    /// `./mcpls.toml` was skipped as untrusted when the active config was
    /// loaded (see [`ServerConfig::project_config_ignored`](crate::config::ServerConfig::project_config_ignored));
    /// `get_info` surfaces it in [`RmcpServerConfig::instructions`]. `mcp` carries
    /// the configured `[mcp]` presentation overrides (see
    /// [`crate::config::McpConfig`]), also read by `get_info`.
    #[must_use]
    pub fn new(
        translator: Arc<Translator>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        workspace_roots: Arc<[PathBuf]>,
        subscription_registry: SubscriptionRegistry,
        project_config_ignored: bool,
        mcp: McpConfig,
    ) -> Self {
        let tool_router = Arc::new(Self::build_tool_router(mcp.tool_prefix.as_ref()));
        let context = Arc::new(BridgeContext::new(
            translator,
            notification_cache,
            workspace_roots,
            subscription_registry,
            project_config_ignored,
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
    /// use mcpls_core::bridge::{NotificationCache, Translator};
    /// use mcpls_core::config::McpConfig;
    /// use mcpls_core::mcp::{McplsServer, SubscriptionRegistry};
    /// use tokio::sync::Mutex;
    ///
    /// let server = McplsServer::new(
    ///     Arc::new(Translator::new()),
    ///     Arc::new(Mutex::new(NotificationCache::new())),
    ///     Arc::from(Vec::new()),
    ///     SubscriptionRegistry::new(),
    ///     false,
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
            workspace_roots: Arc::clone(&self.context.workspace_roots),
            session: self.context.session.sibling(),
            project_config_ignored: self.context.project_config_ignored,
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

    /// Router for every MCP tool, with the read-only classification applied
    /// and, when `prefix` is configured, every tool name rewritten to
    /// `{prefix}_{name}`.
    ///
    /// Every mcpls tool is a read-only LSP query: `rename_symbol`,
    /// `format_document` and `get_code_actions` return a *proposed*
    /// `WorkspaceEdit` and never write to disk. Applying that once here
    /// replaces an identical `annotations(...)` block on all 20 `#[tool]`
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
    fn build_tool_router(prefix: Option<&ToolPrefix>) -> ToolRouter<Self> {
        let mut router = Self::declared_tool_router();
        for route in router.map.values_mut() {
            let title = route.attr.title.clone();
            route.attr.annotations.get_or_insert_with(|| {
                ToolAnnotations::from_raw(title, Some(true), Some(false), Some(true), None)
            });
        }
        if let Some(prefix) = prefix {
            debug_assert!(router.map.keys().all(|name| router.has_route(name)));
            let unprefixed = std::mem::take(&mut router.map);
            let entry_count = unprefixed.len();
            for (_, mut route) in unprefixed {
                route.attr.name = Cow::Owned(format!("{prefix}_{}", route.attr.name));
                router.add_route(route);
            }
            debug_assert_eq!(router.map.len(), entry_count);
        }
        router
    }

    /// Get hover information at a position in a file.
    #[tool(
        description = concat!("Type and documentation info at position. Returns signatures, docs, and inferred types for symbols. ", positions_note_request!()),
        title = "Hover"
    )]
    async fn get_hover(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_hover(file_path, Position { line, character })
                .await,
        )
    }

    /// Get the definition location of a symbol.
    #[tool(
        description = concat!("Definition location of symbol at position. Returns file path, line, and character where declared. Capped at a fixed maximum for a pathological case; `truncated: true` on the result means more locations exist than are returned. ", positions_note_request!()),
        title = "Go to Definition"
    )]
    async fn get_definition(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<Json<DefinitionResult>, McpError> {
        to_structured_tool_result(
            self.context
                .translator
                .handle_definition(file_path, Position { line, character })
                .await,
        )
    }

    /// Find all references to a symbol.
    #[tool(
        description = concat!("References to symbol at position, across workspace. Capped at a fixed maximum for an extremely common symbol; `truncated: true` on the result means more references exist than are returned. ", positions_note_request!()),
        title = "Find References"
    )]
    async fn get_references(
        &self,
        Parameters(ReferencesParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
            include_declaration,
        }): Parameters<ReferencesParams>,
    ) -> Result<Json<ReferencesResult>, McpError> {
        to_structured_tool_result(
            self.context
                .translator
                .handle_references(file_path, Position { line, character }, include_declaration)
                .await,
        )
    }

    /// Get diagnostics for a file.
    #[tool(
        description = concat!("Diagnostics for a file. Returns errors, warnings, and hints with severity and location. `indexing_in_progress: true` means the routed server was indexing at some point during this read, so results may be incomplete. `push_notifications_degraded: true` means the routed server crashed and was restarted, so push-only diagnostics (e.g. flycheck) are missing. ", positions_note_response!()),
        title = "Diagnostics"
    )]
    async fn get_diagnostics(
        &self,
        Parameters(DiagnosticsParams { file_path }): Parameters<DiagnosticsParams>,
    ) -> Result<Json<DiagnosticsResponse>, McpError> {
        // Resolved from the validated/canonicalized path (mirrors
        // `read_resource`), not the raw client-supplied path: a symlink
        // whose extension differs from its target must route to the same
        // language `handle_diagnostics`'s own validation resolves, or this
        // could observe the wrong server's (or no server's) indexing state.
        // Best-effort (`.ok()`): an invalid/out-of-workspace path just reads
        // `false` here and fails properly inside `handle_diagnostics` below.
        let route_id =
            validate_path_against_roots(Path::new(&file_path), &self.context.workspace_roots)
                .ok()
                .and_then(|validated_path| {
                    self.context
                        .translator
                        .diagnostics_route_id_for_path(&validated_path)
                });

        // Sampled before and after the pull: indexing may finish, or a respawn may mark push-degraded, mid-pull.
        let before = {
            let cache = self.context.notification_cache.lock().await;
            DiagnosticsRouteSignals::sample(&cache, route_id.as_ref())
        };

        // Merging push-model (flycheck/clippy) diagnostics into the pull
        // result, including the pull-error-but-cache-has-data fallback, is
        // handled inside handle_diagnostics itself -- see its doc comment.
        let result = self
            .context
            .translator
            .handle_diagnostics(file_path, &self.context.notification_cache)
            .await;

        let after = {
            let cache = self.context.notification_cache.lock().await;
            DiagnosticsRouteSignals::sample(&cache, route_id.as_ref())
        };
        let signals = before.union(after);

        to_structured_tool_result(result.map(|result| DiagnosticsResponse { result, signals }))
    }

    /// Rename a symbol across the workspace.
    // read-only: returns a proposed WorkspaceEdit, does not apply it -- mcpls
    // has no write-back path today; revisit if that changes.
    #[tool(
        description = concat!("Rename symbol across workspace. Returns text edits for all files where symbol is used. A non-empty `dropped` field means some edits were withheld (e.g. out-of-workspace files, or `exceeds_item_cap` when a file's edits exceed the fixed maximum) -- the rename is then incomplete even though `changes` is non-empty. ", positions_note_request!()),
        title = "Rename Symbol"
    )]
    async fn rename_symbol(
        &self,
        Parameters(RenameParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
            new_name,
        }): Parameters<RenameParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_rename(file_path, Position { line, character }, new_name)
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
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_completions(file_path, Position { line, character }, trigger)
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
        to_structured_tool_result(
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
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_format_document(file_path, tab_size, insert_spaces)
                .await,
        )
    }

    /// Search for symbols across the workspace.
    #[tool(
        description = concat!("Search workspace symbols by name. Supports partial matching and fuzzy search. `limit` is capped at a fixed server-side maximum regardless of the value requested; `truncated: true` on the result means more matches exist than are returned. ", positions_note_response!()),
        title = "Workspace Symbol Search"
    )]
    async fn workspace_symbol_search(
        &self,
        Parameters(WorkspaceSymbolParams {
            query,
            kind_filter,
            limit,
        }): Parameters<WorkspaceSymbolParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
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
        description = concat!("Code actions for range. Returns quick fixes, refactorings, and source actions with edits. Capped at a fixed maximum; `truncated: true` on the result means some actions, diagnostics, or edits were left out. An action's `edit.dropped` field, when non-empty, means some of that edit's changes were withheld (e.g. out-of-workspace files). Keep the range end inside the file. ", positions_note_request!()),
        title = "Code Actions"
    )]
    async fn get_code_actions(
        &self,
        Parameters(CodeActionsParams {
            file_path,
            range:
                RangeParams {
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                },
            kind_filter,
        }): Parameters<CodeActionsParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_code_actions(
                    file_path,
                    Position {
                        line: start_line,
                        character: start_character,
                    },
                    Position {
                        line: end_line,
                        character: end_character,
                    },
                    kind_filter,
                )
                .await,
        )
    }

    /// Prepare call hierarchy at a position.
    #[tool(
        description = concat!("Prepare call hierarchy at position. Returns callable items for incoming/outgoing call analysis, capped at a fixed maximum; `truncated: true` on the result means more exist than are returned. ", positions_note_request!()),
        title = "Prepare Call Hierarchy"
    )]
    async fn prepare_call_hierarchy(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_call_hierarchy_prepare(file_path, Position { line, character })
                .await,
        )
    }

    /// Get incoming calls (callers).
    #[tool(
        description = concat!("Functions calling the specified item. Takes call hierarchy item, returns callers, capped at a fixed maximum; `truncated: true` on the result means more exist than are returned. ", positions_note_request!()),
        title = "Incoming Calls"
    )]
    async fn get_incoming_calls(
        &self,
        Parameters(CallHierarchyCallsParams { item }): Parameters<CallHierarchyCallsParams>,
    ) -> Result<String, McpError> {
        to_tool_result(self.context.translator.handle_incoming_calls(item).await)
    }

    /// Get outgoing calls (callees).
    #[tool(
        description = concat!("Functions called by the specified item. Takes call hierarchy item, returns callees, capped at a fixed maximum; `truncated: true` on the result means more exist than are returned. ", positions_note_request!()),
        title = "Outgoing Calls"
    )]
    async fn get_outgoing_calls(
        &self,
        Parameters(CallHierarchyCallsParams { item }): Parameters<CallHierarchyCallsParams>,
    ) -> Result<String, McpError> {
        to_tool_result(self.context.translator.handle_outgoing_calls(item).await)
    }

    /// Get cached diagnostics for a file.
    #[tool(
        description = concat!("Cached diagnostics from server notifications. Faster than the pull-model diagnostics tool, no new analysis. `indexing_in_progress: true` means the routed server was indexing at some point during this read, so results may be incomplete. `push_notifications_degraded: true` means the routed server crashed and was restarted, so push-only diagnostics (e.g. flycheck) are missing. ", positions_note_response!()),
        title = "Cached Diagnostics"
    )]
    async fn get_cached_diagnostics(
        &self,
        Parameters(CachedDiagnosticsParams { file_path }): Parameters<CachedDiagnosticsParams>,
    ) -> Result<String, McpError> {
        let result = match Translator::cached_diagnostics_path_and_uri(
            &self.context.workspace_roots,
            &file_path,
        ) {
            Ok((validated_path, uri)) => {
                // Resolved independently of the cache lookup below: a
                // respawn clears `diagnostics_owner` for this server's
                // entries along with its stale diagnostics (#359), so
                // the degraded flag can't be keyed on ownership -- the
                // routing identity is what stays stable across a
                // respawn.
                let route_id = self
                    .context
                    .translator
                    .diagnostics_route_id_for_path(&validated_path);

                // Lock only long enough for the map lookup + clone: no
                // canonicalize() or Vec mapping while `notification_cache`
                // is held, since `diagnostics_pump` needs the same lock.
                let (diag_info, owner, signals) = {
                    let cache = self.context.notification_cache.lock().await;
                    let owner = cache.diagnostics_owner(&uri).cloned();
                    let signals = DiagnosticsRouteSignals::sample(&cache, route_id.as_ref());
                    (cache.diagnostics(&uri).cloned(), owner, signals)
                };
                let encoding = owner.map_or(PositionEncoding::Utf16, |server_id| {
                    self.context.translator.position_encoding_for(&server_id)
                });
                let result = Translator::diagnostics_from_cache_entry(
                    diag_info.as_ref(),
                    encoding,
                    self.context.translator.document_tracker(),
                )
                .await;
                Ok(CachedDiagnosticsResponse { result, signals })
            }
            Err(e) => Err(e),
        };

        to_tool_result(result)
    }

    /// Get recent LSP server log messages.
    #[tool(
        description = "Recent server log messages. Filter by level (error, warning, info, debug) for debugging.",
        title = "Server Logs"
    )]
    async fn get_server_logs(
        &self,
        Parameters(ServerLogsParams { limit, min_level }): Parameters<ServerLogsParams>,
    ) -> Result<String, McpError> {
        to_tool_result({
            let cache = self.context.notification_cache.lock().await;
            Translator::handle_server_logs(&cache, limit, min_level)
        })
    }

    /// Get recent LSP server messages.
    #[tool(
        description = "Recent server messages (showMessage notifications). User-facing prompts and status updates.",
        title = "Server Messages"
    )]
    async fn get_server_messages(
        &self,
        Parameters(ServerMessagesParams { limit }): Parameters<ServerMessagesParams>,
    ) -> Result<String, McpError> {
        to_tool_result({
            let cache = self.context.notification_cache.lock().await;
            Translator::handle_server_messages(&cache, limit)
        })
    }

    /// Get signature help at a position.
    #[tool(
        description = "Signature help at position. Returns parameter info, active signature/parameter, and documentation while typing a call. `positions_degraded: \"request\"` (non-UTF-16 servers only) means the queried position was sent unconverted, so the result may not match the position asked about and should not be trusted.",
        title = "Signature Help"
    )]
    async fn get_signature_help(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_signature_help(file_path, Position { line, character })
                .await,
        )
    }

    /// Go to implementation locations.
    #[tool(
        description = concat!("Implementation locations of trait method or interface member at position. Capped at a fixed maximum for an extremely common trait/interface; `truncated: true` on the result means more implementations exist than are returned. ", positions_note_request!()),
        title = "Go to Implementation"
    )]
    async fn go_to_implementation(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_implementation(file_path, Position { line, character })
                .await,
        )
    }

    /// Go to type definition location.
    #[tool(
        description = concat!("Type definition location of expression at position. Distinct from go-to-definition for variable bindings. Capped at a fixed maximum for a pathological case; `truncated: true` on the result means more locations exist than are returned. ", positions_note_request!()),
        title = "Go to Type Definition"
    )]
    async fn go_to_type_definition(
        &self,
        Parameters(PositionParams {
            file_path,
            line,
            character,
        }): Parameters<PositionParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_type_definition(file_path, Position { line, character })
                .await,
        )
    }

    /// Get inlay hints for a range.
    #[tool(
        description = concat!("Inlay hints in range. Returns inferred type/parameter annotations the editor would render inline. Capped at a fixed maximum; `truncated: true` on the result means more hints exist than are returned. Keep the range end inside the file. ", positions_note_request!()),
        title = "Inlay Hints"
    )]
    async fn get_inlay_hints(
        &self,
        Parameters(InlayHintsParams {
            file_path,
            range:
                RangeParams {
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                },
        }): Parameters<InlayHintsParams>,
    ) -> Result<String, McpError> {
        to_tool_result(
            self.context
                .translator
                .handle_inlay_hints(
                    file_path,
                    Position {
                        line: start_line,
                        character: start_character,
                    },
                    Position {
                        line: end_line,
                        character: end_character,
                    },
                )
                .await,
        )
    }
}

impl McplsServer {
    /// Builds the diagnostics resource payload for `path`; split out of
    /// `read_resource` so tests can drive the real wiring without a
    /// `RequestContext`.
    async fn resource_diagnostics_response(
        &self,
        path: &Path,
    ) -> Result<ResourceDiagnosticsResponse, McpError> {
        // Enforce workspace-root containment — mirrors the guard in every LSP tool.
        // Validated against a lock-free snapshot of workspace_roots (fixed at
        // startup) so this cache-only read never needs to touch `translator` at all.
        let validated_path = validate_path_against_roots(path, &self.context.workspace_roots)
            .map_err(map_bridge_error)?;

        // Build the URI from the canonicalized path (not the raw input path):
        // it must match what `diagnostics_pump` stores from LSP notifications,
        // which are always keyed by the canonical form.
        let lsp_uri = crate::bridge::path_to_uri(&validated_path).map_err(map_bridge_error)?;

        let route_id = self
            .context
            .translator
            .diagnostics_route_id_for_path(&validated_path);

        // Built from a borrow of the cache entry rather than `.cloned()`-ing the
        // whole `DiagnosticInfo` first: `build_resource_diagnostics_response`
        // only ever needs `version` (Copy) and its own clone of `diagnostics`,
        // so cloning the entry up front would clone `diagnostics` twice.
        let cache = self.context.notification_cache.lock().await;
        Ok(build_resource_diagnostics_response(
            self.context.translator.is_document_open(&validated_path),
            cache.diagnostics(lsp_uri.as_ref()),
            DiagnosticsRouteSignals::sample(&cache, route_id.as_ref()),
        ))
    }

    /// Body of `read_resource`, kept separate so it can run under
    /// [`contain_panic`].
    async fn read_resource_inner(
        &self,
        request: ReadResourceRequestParams,
    ) -> Result<ReadResourceResponse, McpError> {
        let path =
            parse_uri(&request.uri).map_err(|e| McpError::invalid_params(e.to_string(), None))?;
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
    match AssertUnwindSafe(handler).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => {
            tracing::error!(
                "{operation} handler panicked: {}",
                crate::panic_message(payload.as_ref())
            );
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
            return Err(no_resolvable_listen_uris());
        }
        let permit = self.context.session.registry().try_reserve_listen()?;
        let roots = Arc::clone(&self.context.workspace_roots);
        let accepted = accepted.to_vec();
        let uris = tokio::task::spawn_blocking(move || ListenUris::resolve(&accepted, &roots))
            .await
            .map_err(|e| listen_join_error(&e))?;
        if uris.is_empty() {
            return Err(no_resolvable_listen_uris());
        }
        Ok(Some((permit, uris)))
    }
}

fn no_resolvable_listen_uris() -> crate::error::Error {
    crate::error::Error::InvalidUri(
        "none of the requested resource URIs resolve inside the workspace".to_owned(),
    )
}

fn listen_join_error(e: &tokio::task::JoinError) -> crate::error::Error {
    crate::error::Error::McpServer(format!("subscriptions/listen resolution: {e}"))
}

// `list_resources` is synchronous (no `.await`), but `ServerHandler::list_resources`
// requires `async fn`; `#[tool_handler]` also expands other trait methods without
// `.await`, so the lint is suppressed for the whole impl block.
#[allow(clippy::unused_async_trait_impl)]
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
                } = DiagnosticsResourceUri::resolve(&request.uri, &self.context.workspace_roots)
                    .map_err(map_bridge_error)?;

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
                    .map_err(|e| map_bridge_error(e.into()))?;
                if !newly_subscribed {
                    tracing::debug!(
                        "client re-subscribed to already-subscribed resource {canonical_uri}"
                    );
                }

                // Build the URI from the canonicalized path, matching `read_resource` and
                // what `diagnostics_pump` stores from LSP notifications.
                let lsp_uri =
                    crate::bridge::path_to_uri(&validated_path).map_err(map_bridge_error)?;
                let has_cached_diagnostics = {
                    let cache = self.context.notification_cache.lock().await;
                    cache.diagnostics(lsp_uri.as_ref()).is_some()
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
                let canonical = match DiagnosticsResourceUri::resolve(
                    &request.uri,
                    &self.context.workspace_roots,
                ) {
                    Ok(resolved) => Some(resolved.uri),
                    Err(e @ crate::error::Error::InvalidUri(_)) => return Err(map_bridge_error(e)),
                    Err(_) => None,
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
            .map_err(map_bridge_error)?
        else {
            return Ok(());
        };

        let uris = Arc::new(uris);
        let sink = context.sink().clone();
        let registration = permit.register(Arc::clone(&uris), |uris| Target::Sink { sink, uris });

        // Registered above, before the cache read, so no publish is lost in between.
        let cached: Vec<&DiagnosticsResourceUri> = {
            let cache = self.context.notification_cache.lock().await;
            uris.canonical()
                .filter(|(_, lsp_uri)| cache.diagnostics(lsp_uri.as_ref()).is_some())
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
                .clone()
                .unwrap_or_else(|| DEFAULT_SERVER_TITLE.to_string()),
        );
        implementation.description = Some(
            self.context
                .mcp
                .description
                .clone()
                .unwrap_or_else(|| DEFAULT_SERVER_DESCRIPTION.to_string()),
        );
        implementation.website_url = Some("https://github.com/bug-ops/mcpls".to_string());

        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_resources_subscribe()
            .build();
        let mut server_info = RmcpServerConfig::new(capabilities);
        server_info.server_info = implementation;
        let mut instructions = self
            .context
            .mcp
            .instructions
            .clone()
            .unwrap_or_else(|| DEFAULT_INSTRUCTIONS.to_string());

        if self.context.project_config_ignored {
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::bridge::resources::ResourceSubscriptions;

    fn create_test_server() -> McplsServer {
        create_test_server_with_ignored_flag(false)
    }

    fn create_test_server_with_ignored_flag(project_config_ignored: bool) -> McplsServer {
        create_test_server_with_mcp_config(project_config_ignored, McpConfig::default())
    }

    fn create_test_server_with_mcp_config(
        project_config_ignored: bool,
        mcp: McpConfig,
    ) -> McplsServer {
        let workspace_roots: Arc<[PathBuf]> = Arc::from(Vec::new());
        create_test_server_with_workspace_roots(project_config_ignored, mcp, workspace_roots)
    }

    /// Like [`create_test_server_with_mcp_config`], for tests that exercise a
    /// path-taking tool (e.g. `get_cached_diagnostics`) and so need a real
    /// workspace root -- an empty one now makes `validate_path_against_roots`
    /// fail closed with `Error::NoWorkspaceRoots`.
    fn create_test_server_with_workspace_roots(
        project_config_ignored: bool,
        mcp: McpConfig,
        workspace_roots: Arc<[PathBuf]>,
    ) -> McplsServer {
        let translator = Arc::new(Translator::new());
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        McplsServer::new(
            translator,
            notification_cache,
            workspace_roots,
            SubscriptionRegistry::new(),
            project_config_ignored,
            mcp,
        )
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
            false,
            McpConfig::default(),
            Arc::from([temp_dir.path().to_path_buf()]),
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
            server_id: crate::config::ServerId::from("rust"),
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
            server_id: crate::config::ServerId::from("python"),
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
        let err = crate::error::Error::NoServerForLanguage("python".to_string());
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
            crate::error::Error::InvalidUri("not a uri".to_string()),
            crate::error::Error::DocumentNotFound(PathBuf::from("/missing.rs")),
            crate::error::Error::FileSizeLimitExceeded { size: 100, max: 10 },
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

    /// #527: a startup failure reaches the client as an internal error that
    /// still carries the spawn failure's install guidance.
    #[test]
    fn test_map_bridge_error_startup_failure_carries_guidance() {
        let error =
            crate::error::Error::ServerFailedToStart(Box::new(crate::error::ServerSpawnFailure {
                server_id: crate::config::ServerId::from("rust"),
                language_id: "rust".to_string(),
                command: "rust-analyzer".to_string(),
                reason: crate::error::StartupFailure::Spawn(Arc::new(
                    crate::error::Error::ServerNotFound {
                        command: "rust-analyzer".to_string(),
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
        let server = create_test_server_with_ignored_flag(false);
        let info = server.get_info();

        assert!(!info.instructions.unwrap().contains("ignored as untrusted"));
    }

    #[tokio::test]
    async fn test_server_info_surfaces_ignored_project_config() {
        let server = create_test_server_with_ignored_flag(true);
        let info = server.get_info();

        let instructions = info.instructions.unwrap();
        assert!(instructions.contains("ignored as untrusted"));
        assert!(instructions.contains("--trust-project-config"));
    }

    #[tokio::test]
    async fn test_get_info_default_mcp_config_uses_built_in_text() {
        let server = create_test_server_with_mcp_config(false, McpConfig::default());
        let info = server.get_info();

        assert_eq!(
            info.server_info.title.as_deref(),
            Some(DEFAULT_SERVER_TITLE)
        );
        assert_eq!(
            info.server_info.description.as_deref(),
            Some(DEFAULT_SERVER_DESCRIPTION)
        );
        assert_eq!(info.instructions.as_deref(), Some(DEFAULT_INSTRUCTIONS));
    }

    #[tokio::test]
    async fn test_get_info_reflects_configured_mcp_fields() {
        let mcp = McpConfig {
            title: Some("Custom Title".to_string()),
            description: Some("Custom description".to_string()),
            instructions: Some("Custom instructions.".to_string()),
            tool_prefix: None,
        };
        let server = create_test_server_with_mcp_config(false, mcp);
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
            instructions: Some(instructions.clone()),
            tool_prefix: None,
        };
        let server = create_test_server_with_mcp_config(true, mcp);
        let info = server.get_info();

        let returned_instructions = info.instructions.unwrap();
        assert!(returned_instructions.starts_with(&instructions));
        assert!(returned_instructions.contains("ignored as untrusted"));
    }

    #[tokio::test]
    async fn test_hover_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(PositionParams {
            file_path: test_file.to_str().unwrap().to_string(),
            line: 1,
            character: 1,
        });

        // No LSP server is registered for any language on this test server,
        // so this fails downstream of the workspace-roots gate with
        // `Error::NoServerForLanguage`/`NoServerConfigured`.
        let result = server.get_hover(params).await;
        assert!(result.is_err());
    }

    /// #417: the fail-closed `Error::NoWorkspaceRoots` path must propagate
    /// correctly through a `#[tool]` handler's full error-mapping chain
    /// (`to_tool_result`/`McpError::internal_error`), not just through the
    /// lower-level `Translator::validate_path`/`validate_path_against_roots`
    /// unit tests -- `create_test_server()` here deliberately keeps the
    /// empty roots that `create_test_server_with_real_file()` (used by the
    /// rest of this test group) sets up a real root to avoid.
    #[tokio::test]
    async fn test_hover_tool_with_params_no_workspace_roots() {
        let server = create_test_server();
        let params = Parameters(PositionParams {
            file_path: "/test/file.rs".to_string(),
            line: 1,
            character: 1,
        });

        let result = server.get_hover(params).await;
        let err = result.unwrap_err();
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
            file_path: test_file.to_str().unwrap().to_string(),
            line: 10,
            character: 5,
        });

        let result = server.get_definition(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_references_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(ReferencesParams {
            position: PositionParams {
                file_path: test_file.to_str().unwrap().to_string(),
                line: 10,
                character: 5,
            },
            include_declaration: false,
        });

        let result = server.get_references(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_diagnostics_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(DiagnosticsParams {
            file_path: test_file.to_str().unwrap().to_string(),
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

        let server_id = ServerId::from("rust");
        let dir = TempDir::new().unwrap();
        let mut translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                "rust".to_string(),
            )]))
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);
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
            Arc::from(vec![dir.path().to_path_buf()]),
            SubscriptionRegistry::new(),
            false,
            McpConfig::default(),
        );

        let call = {
            let params = Parameters(DiagnosticsParams {
                file_path: path_str,
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
        assert!(result.0.signals.indexing_in_progress);
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

        let server_id = ServerId::from("rust");
        let dir = TempDir::new().unwrap();
        let mut translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                "rust".to_string(),
            )]))
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);
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
            Arc::from(vec![dir.path().to_path_buf()]),
            SubscriptionRegistry::new(),
            false,
            McpConfig::default(),
        );

        let call = {
            let params = Parameters(DiagnosticsParams {
                file_path: path_str,
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
        assert!(!result.0.signals.indexing_in_progress);
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

        let server_id = ServerId::from("rust");
        let dir = TempDir::new().unwrap();
        let mut translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                "rust".to_string(),
            )]))
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);
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
            Arc::from(vec![dir.path().to_path_buf()]),
            SubscriptionRegistry::new(),
            false,
            McpConfig::default(),
        );

        let call = {
            let params = Parameters(DiagnosticsParams {
                file_path: path_str,
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
            result.0.signals.indexing_in_progress,
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

        let server_id = ServerId::from("rust");
        let dir = tempfile::TempDir::new().unwrap();
        let mut translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                "rust".to_string(),
            )]))
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);
        let translator = Arc::new(translator);
        let (client, fake) = fake_lsp_client();
        translator.register_client(server_id.clone(), client);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let path = dir.path().join("lib.rs");
        std::fs::write(&path, "fn main() {}").unwrap();

        let server = Arc::new(McplsServer::new(
            translator,
            Arc::clone(&cache),
            Arc::from(vec![dir.path().to_path_buf()]),
            SubscriptionRegistry::new(),
            false,
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
                file_path: fx.path.to_string_lossy().to_string(),
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
        let file_path = fx.path.to_string_lossy().to_string();
        let call = {
            let server = Arc::clone(&fx.server);
            let params = Parameters(DiagnosticsParams {
                file_path: file_path.clone(),
            });
            tokio::spawn(async move { server.get_diagnostics(params).await })
        };
        serve_empty_pull(&mut fx.fake, async {}).await;
        let pulled = serde_json::to_value(call.await.unwrap().unwrap().0).unwrap();

        let cached: serde_json::Value = serde_json::from_str(
            &fx.server
                .get_cached_diagnostics(Parameters(CachedDiagnosticsParams { file_path }))
                .await
                .unwrap(),
        )
        .unwrap();
        let resource = serde_json::to_value(
            fx.server
                .resource_diagnostics_response(&fx.path)
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

    /// Fails on any `properties` name containing an ASCII uppercase letter,
    /// recursing through every other schema keyword (`$defs`, `items`,
    /// `anyOf`, `oneOf`, ...) without checking their names, since those are
    /// type names and JSON Schema keywords rather than wire keys.
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
                                !name.chars().any(|c| c.is_ascii_uppercase()),
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

    /// #504 guard. Tools without an `outputSchema` (returning `String`) are not
    /// covered by value-level checks here; their DTOs are `snake_case` by serde
    /// default and any `rename_all` on them would need a new check. Covers: every tool `outputSchema` (property names only),
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

        let signals = DiagnosticsRouteSignals {
            push_notifications_degraded: true,
            indexing_in_progress: true,
        };
        let result = || DiagnosticsResult {
            diagnostics: Vec::new(),
            positions_degraded: Some(crate::bridge::PositionDegradation::Request),
        };
        let pulled = serde_json::to_value(DiagnosticsResponse {
            result: result(),
            signals,
        })
        .unwrap();
        let cached = serde_json::to_value(CachedDiagnosticsResponse {
            result: result(),
            signals,
        })
        .unwrap();
        let resource =
            serde_json::to_value(build_resource_diagnostics_response(true, None, signals)).unwrap();
        assert_value_keys_snake_case(&pulled, &[]);
        assert_value_keys_snake_case(&cached, &[]);
        assert_value_keys_snake_case(&resource, &["diagnostics"]);

        for data in [
            RetryableErrorData::WorkspaceIndexing {
                server_id: ServerId::from("rust"),
                elapsed_secs: 1,
            },
            RetryableErrorData::ServerInitializing {
                server_id: ServerId::from("rust"),
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

        let server_id = ServerId::from("rust");
        let dir = TempDir::new().unwrap();
        let mut translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                "rust".to_string(),
            )]))
            .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())]));
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);
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
            Arc::from(vec![dir.path().to_path_buf()]),
            SubscriptionRegistry::new(),
            false,
            McpConfig::default(),
        );

        let call = {
            let params = Parameters(DiagnosticsParams {
                file_path: path_str,
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
            result.0.signals.indexing_in_progress,
            "route resolution must follow the symlink to its .rs target, not \
             stop at the .txt extension of the raw client path"
        );
    }

    #[tokio::test]
    async fn test_rename_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(RenameParams {
            position: PositionParams {
                file_path: test_file.to_str().unwrap().to_string(),
                line: 10,
                character: 5,
            },
            new_name: "new_name".to_string(),
        });

        let result = server.rename_symbol(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_completions_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(CompletionsParams {
            position: PositionParams {
                file_path: test_file.to_str().unwrap().to_string(),
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
            file_path: test_file.to_str().unwrap().to_string(),
        });

        let result = server.get_document_symbols(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_format_document_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(FormatDocumentParams {
            file_path: test_file.to_str().unwrap().to_string(),
            tab_size: 4,
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
            file_path: test_file.to_str().unwrap().to_string(),
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
            file_path: test_file.to_str().unwrap().to_string(),
            line: 10,
            character: 5,
        });
        let result = server.prepare_call_hierarchy(params).await;
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
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            },
            "selectionRange": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            }
        });
        let params = Parameters(CallHierarchyCallsParams { item });
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
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            },
            "selectionRange": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            }
        });
        let params = Parameters(CallHierarchyCallsParams { item });
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
            false,
            McpConfig::default(),
            Arc::from([temp_dir.path().to_path_buf()]),
        );

        let params = Parameters(CachedDiagnosticsParams {
            file_path: test_file.to_str().unwrap().to_string(),
        });

        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
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
            false,
            McpConfig::default(),
            Arc::from([temp_dir.path().to_path_buf()]),
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
                &crate::config::ServerId::from("rust"),
                &uri,
                Some(1),
                vec![diagnostic],
            );
        }

        // Textually distinct from `test_file`, but canonicalizes to the same path.
        let noncanonical = subdir.join("..").join("sub").join("test.rs");
        let params = Parameters(CachedDiagnosticsParams {
            file_path: noncanonical.to_str().unwrap().to_string(),
        });

        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let diagnostics = parsed.get("diagnostics").unwrap().as_array().unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].get("message").unwrap(), "cached error");
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
            false,
            McpConfig::default(),
            Arc::from([temp_dir.path().to_path_buf()]),
        );
        let owner = crate::config::ServerId::from("rust");
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
            file_path: test_file.to_str().unwrap().to_string(),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let diagnostics = parsed.get("diagnostics").unwrap().as_array().unwrap();
        assert_eq!(
            diagnostics[0]["range"]["end"]["character"], 3,
            "byte offset 3 on \"héllo\" is UTF-16 column 3 when converted against the \
             registered UTF-8 owner"
        );
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
            false,
            McpConfig::default(),
            Arc::from([temp_dir.path().to_path_buf()]),
        );
        // Deliberately not registered with `translator.register_server`.
        let owner = crate::config::ServerId::from("rust");

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
            file_path: test_file.to_str().unwrap().to_string(),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
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
        // on `Translator::diagnostics_route_id_for_path` (the file's
        // *routed* server, resolved from its detected language), not on
        // `NotificationCache::diagnostics_owner` -- see #359's C1 fix. This
        // is a fast unit test of that wiring alone; the slower
        // `..._after_real_respawn` test below covers the full path through
        // an actual crash + respawn.
        let owner = ServerId::from("rust");
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let translator = Arc::new(
            Translator::new()
                .with_router(ToolRouter::catch_all([(owner.clone(), "rust".to_string())]))
                .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())])),
        );
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let server = McplsServer::new(
            translator,
            Arc::clone(&notification_cache),
            Arc::from([temp_dir.path().to_path_buf()]),
            SubscriptionRegistry::new(),
            false,
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
            file_path: test_file.to_str().unwrap().to_string(),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
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

        let owner = ServerId::from("rust");
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        notification_cache.lock().await.observe_indexing_signal(
            &owner,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = Arc::new(
            Translator::new()
                .with_router(ToolRouter::catch_all([(owner.clone(), "rust".to_string())]))
                .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())])),
        );
        let temp_dir = TempDir::new().unwrap();
        let server = McplsServer::new(
            translator,
            Arc::clone(&notification_cache),
            Arc::from(vec![temp_dir.path().to_path_buf()]),
            SubscriptionRegistry::new(),
            false,
            McpConfig::default(),
        );

        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let params = Parameters(CachedDiagnosticsParams {
            file_path: test_file.to_str().unwrap().to_string(),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
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

        let owner = ServerId::from("rust");
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        notification_cache.lock().await.observe_indexing_signal(
            &owner,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = Arc::new(
            Translator::new()
                .with_router(ToolRouter::catch_all([(owner.clone(), "rust".to_string())]))
                .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())])),
        );
        let temp_dir = TempDir::new().unwrap();
        let server = McplsServer::new(
            translator,
            Arc::clone(&notification_cache),
            Arc::from(vec![temp_dir.path().to_path_buf()]),
            SubscriptionRegistry::new(),
            false,
            McpConfig::default(),
        );

        let target = temp_dir.path().join("target.rs");
        fs::write(&target, "fn main() {}").unwrap();
        let link = temp_dir.path().join("link.txt");
        symlink(&target, &link).unwrap();

        let params = Parameters(CachedDiagnosticsParams {
            file_path: link.to_str().unwrap().to_string(),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
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

        let id = ServerId::from("rust");
        let config = ServerInitConfig {
            server_config: LspServerConfig {
                language_id: "rust".to_string(),
                command: "sh".to_string(),
                args: vec![script_path.to_string_lossy().to_string()],
                env: HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                timeout_seconds: 5,
                request_timeout_seconds: 5,
                heuristics: None,
                name: Some("rust".to_string()),
                handles: None,
                indexing: crate::bridge::IndexingPolicy::Auto,
            },
            workspace_roots: vec![],
            initialization_options: None,
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
            notification_tx: None,
        };

        let seed = LspServer::spawn(config).await.unwrap();

        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let translator = Arc::new(
            Translator::new()
                .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]))
                .with_notification_cache(Arc::clone(&notification_cache))
                .with_extensions(HashMap::from([("rs".to_string(), "rust".to_string())])),
        );
        translator.register_server_complete(seed);

        let server = McplsServer::new(
            Arc::clone(&translator),
            Arc::clone(&notification_cache),
            Arc::from([dir.path().to_path_buf()]),
            SubscriptionRegistry::new(),
            false,
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
            file_path: test_file.to_str().unwrap().to_string(),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
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
            false,
            McpConfig::default(),
            Arc::from([temp_dir.path().to_path_buf()]),
        );

        let params = Parameters(CachedDiagnosticsParams {
            file_path: test_file.to_str().unwrap().to_string(),
        });
        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed.get("push_notifications_degraded").unwrap(), false);
    }

    #[tokio::test]
    async fn test_cached_diagnostics_tool_nonexistent_file() {
        #[cfg(windows)]
        let root = PathBuf::from(r"C:\");
        #[cfg(not(windows))]
        let root = PathBuf::from("/");
        let server =
            create_test_server_with_workspace_roots(false, McpConfig::default(), Arc::from([root]));
        let params = Parameters(CachedDiagnosticsParams {
            file_path: "/nonexistent/file.rs".to_string(),
        });

        let result = server.get_cached_diagnostics(params).await;
        let err = result.unwrap_err();
        assert!(
            err.message.contains("file I/O error"),
            "expected a file I/O error for a nonexistent path, got: {}",
            err.message
        );
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

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.get("logs").is_some());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_error_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 10,
            min_level: Some("error".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let logs = parsed.get("logs").unwrap().as_array().unwrap();
        assert_eq!(logs.len(), 0);
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_warning_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 100,
            min_level: Some("warning".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_info_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 50,
            min_level: Some("info".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_debug_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 20,
            min_level: Some("debug".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_invalid_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 10,
            min_level: Some("invalid_level".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_err());
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

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let logs = parsed.get("logs").unwrap().as_array().unwrap();
        assert_eq!(logs.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_default_params() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 20 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.get("messages").is_some());
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_custom_limit() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 5 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let messages = parsed.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_zero_limit() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 0 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
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
            file_path: test_file.to_str().unwrap().to_string(),
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
            file_path: test_file.to_str().unwrap().to_string(),
            line: 10,
            character: 5,
        });

        let result = server.go_to_implementation(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_go_to_type_definition_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(PositionParams {
            file_path: test_file.to_str().unwrap().to_string(),
            line: 10,
            character: 5,
        });

        let result = server.go_to_type_definition(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_inlay_hints_tool_with_params() {
        let (server, _temp_dir, test_file) = create_test_server_with_real_file();
        let params = Parameters(InlayHintsParams {
            file_path: test_file.to_str().unwrap().to_string(),
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
            ("get_cached_diagnostics", true, false, true),
            ("get_server_logs", true, false, true),
            ("get_server_messages", true, false, true),
            ("get_signature_help", true, false, true),
            ("go_to_implementation", true, false, true),
            ("go_to_type_definition", true, false, true),
            ("get_inlay_hints", true, false, true),
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
            version: Some(1),
            diagnostics,
        }
    }

    #[test]
    fn test_resource_diagnostics_response_untracked_is_not_tracked_and_empty() {
        let response =
            ResourceDiagnosticsResponse::new(false, None, DiagnosticsRouteSignals::default());
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
        let response =
            ResourceDiagnosticsResponse::new(true, None, DiagnosticsRouteSignals::default());
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
            true,
            Some(&entry),
            DiagnosticsRouteSignals::default(),
        );
        assert!(response.tracked);
        assert_eq!(response.version, Some(1));
        assert_eq!(response.diagnostics.len(), 1);
        assert_eq!(response.diagnostics[0].message, "boom".into());

        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["tracked"], true);
        assert_eq!(json["version"], 1);
        assert_eq!(json["diagnostics"][0]["message"], "boom");
    }

    /// A path `read_resource` never opened reports `is_document_open() == false`
    /// -- one of the two inputs `build_resource_diagnostics_response` ORs together.
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
        let response =
            build_resource_diagnostics_response(false, None, DiagnosticsRouteSignals::default());
        assert!(!response.tracked);
        assert_eq!(response.diagnostics.len(), 0);
    }

    #[test]
    fn test_build_resource_diagnostics_response_open_but_uncached_is_tracked() {
        let response =
            build_resource_diagnostics_response(true, None, DiagnosticsRouteSignals::default());
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

        let response = build_resource_diagnostics_response(
            false,
            Some(&entry),
            DiagnosticsRouteSignals::default(),
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
        let response = build_resource_diagnostics_response(
            false,
            None,
            DiagnosticsRouteSignals {
                push_notifications_degraded: true,
                indexing_in_progress: false,
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
        let response = build_resource_diagnostics_response(
            false,
            None,
            DiagnosticsRouteSignals {
                push_notifications_degraded: false,
                indexing_in_progress: true,
            },
        );
        assert!(response.signals.indexing_in_progress);

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
    /// through a symlink must resolve, via `validate_path_against_roots`, to the
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

        let validated = validate_path_against_roots(&noncanonical, &[base]).unwrap();
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
        use std::path::Path;

        use crate::error::Error;

        let mut translator = Translator::new();
        #[cfg(windows)]
        translator.set_workspace_roots(vec![PathBuf::from(r"C:\")]);
        #[cfg(not(windows))]
        translator.set_workspace_roots(vec![PathBuf::from("/")]);
        let result = translator.validate_path(Path::new("/this/path/does/not/exist/at/all.rs"));
        assert!(matches!(result, Err(Error::FileIo { .. })));
    }

    /// #479 regression: `read_resource`/`subscribe` must still return
    /// `INVALID_PARAMS` (`-32602`), not `INTERNAL_ERROR` (`-32603`), for a
    /// client-supplied path that doesn't exist. Exercised at the same
    /// logic level as the rest of this test group (constructing a live
    /// `rmcp::service::RequestContext` isn't possible in a unit test, see
    /// the note above "Resource handler tests"): `validate_path_against_roots`
    /// is the exact call both handlers make, and `map_bridge_error` is the
    /// exact function both now pipe its `Err` through.
    #[test]
    fn test_read_resource_nonexistent_path_maps_to_invalid_params() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roots = [temp_dir.path().to_path_buf()];
        let missing = temp_dir.path().join("does-not-exist.rs");

        let result = validate_path_against_roots(&missing, &roots);
        assert!(matches!(result, Err(crate::error::Error::FileIo { .. })));

        let mcp_err = map_bridge_error(result.unwrap_err());
        assert_eq!(mcp_err.code, ErrorCode::INVALID_PARAMS);
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

        let roots = std::slice::from_ref(&base);
        let validated = validate_path_against_roots(&noncanonical, roots).unwrap();
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
        assert!(validate_path_against_roots(&noncanonical, roots).is_err());

        // Mirrors `unsubscribe`'s handler: no canonical URI once
        // canonicalization fails, only the raw one.
        assert!(DiagnosticsResourceUri::resolve(&raw_uri, roots).is_err());
        assert!(subscriptions.unsubscribe(None, &raw_uri).await.is_some());
        assert!(!subscriptions.contains(&canonical_uri).await);
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
            false,
            McpConfig::default(),
            Arc::from(vec![root]),
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
        assert!(matches!(
            err,
            crate::error::Error::ListenFilterTooLarge { .. }
        ));
        assert_eq!(map_bridge_error(err).code, ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn test_prepare_listen_rejects_oversized_total_bytes() {
        let (server, _dir, uri) = listen_test_server();
        let long = format!("lsp-diagnostics:///{}", "a".repeat(4096));
        let requested = vec![long; 100];
        let err = server.prepare_listen(&requested, &[uri]).await.unwrap_err();
        assert!(matches!(
            err,
            crate::error::Error::ListenFilterTooLarge { .. }
        ));
    }

    #[tokio::test]
    async fn test_listen_join_error_maps_to_internal_error() {
        let handle = tokio::spawn(std::future::pending::<()>());
        handle.abort();
        let err = listen_join_error(&handle.await.unwrap_err());
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
        assert!(matches!(
            err,
            crate::error::Error::ListenStreamsExhausted { .. }
        ));
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

    /// Structured output (`outputSchema`) is advertised for exactly the tools migrated to
    /// `Result<Json<T>, McpError>` handler signatures, and only those. Driven off a literal
    /// expected-name set (not derived from the router) so both a future migration and an
    /// accidental scope change fail loudly here instead of only showing up as an opaque diff in
    /// `test_tool_surface_matches_golden_snapshot`.
    #[test]
    fn test_output_schema_present_only_for_structured_tools() {
        const STRUCTURED_TOOLS: &[&str] = &[
            "get_diagnostics",
            "get_definition",
            "get_references",
            "get_document_symbols",
        ];

        let tools = McplsServer::build_tool_router(None).list_all();
        assert!(!tools.is_empty(), "no tools registered");

        for tool in &tools {
            let expects_schema = STRUCTURED_TOOLS.contains(&tool.name.as_ref());
            assert_eq!(
                tool.output_schema.is_some(),
                expects_schema,
                "tool `{}`: expected output_schema.is_some() == {expects_schema}",
                tool.name
            );
        }
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
        let server = create_test_server_with_mcp_config(false, mcp);

        assert!(server.get_tool("p_get_hover").is_some());
        assert!(server.get_tool("get_hover").is_none());
    }
}
