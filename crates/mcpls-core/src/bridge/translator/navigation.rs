//! Hover, go-to-definition/implementation/type-definition, and references
//! handlers.

use std::time::Duration;

use lsp_types::{
    HoverParams as LspHoverParams, PartialResultParams, ReferenceContext, ReferenceParams,
    TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
};
use tokio::time::Instant;

use super::Translator;
use super::dto::{
    DefinitionResult, HoverResult, Location, LocationsResult, Position, ReferencesResult,
};
use super::encoding_ctx::EncodingCtx;
use super::routing::{Capability, IndexingGate};
use crate::bridge::IndexingState;
use crate::bridge::indexing::{
    DEFAULT_INDEXING_READY_TIMEOUT_SECS, INDEXING_STALENESS_BOUND, PROGRESS_LATCH_IDLE,
    PROGRESS_SETTLE,
};
use crate::config::{ServerId, ToolKind};
use crate::error::{Error, Result};

/// Default maximum time [`Translator::wait_for_indexing_ready`] waits for a
/// routed LSP server to report it has finished its initial workspace load,
/// once a readiness signal has shown indexing is actually in progress.
/// Matches the timeout already used throughout the rust-analyzer integration
/// test suite's own (test-only) indexing-readiness helper.
///
/// This is only the built-in default (used by [`Translator::new`]) --
/// overridable per `Translator` via [`Translator::with_indexing_ready_timeout`],
/// wired from `workspace.indexing_ready_timeout_seconds` in `mcpls.toml`
/// (#424). The compile-time invariants below are checked against this
/// default; the same invariants are re-checked against a configured override
/// at `ServerConfig::validate` time, since a runtime value can't be asserted
/// at compile time.
pub(super) const INDEXING_READY_TIMEOUT: Duration =
    Duration::from_secs(DEFAULT_INDEXING_READY_TIMEOUT_SECS);

/// Poll interval used while waiting out [`INDEXING_READY_TIMEOUT`]. A single
/// mutex lock plus map lookup, not a network round trip, so a short
/// interval adds no meaningful overhead relative to the LSP request that
/// follows once the wait resolves.
const INDEXING_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// `INDEXING_STALENESS_BOUND` must stay larger than `INDEXING_READY_TIMEOUT`,
/// or the read-time staleness self-heal could fire within a single caller's
/// own wait -- reintroducing the cross-caller self-heal race this bound
/// exists to prevent.
const _: () = assert!(
    INDEXING_STALENESS_BOUND.as_nanos() > INDEXING_READY_TIMEOUT.as_nanos(),
    "INDEXING_STALENESS_BOUND must be greater than INDEXING_READY_TIMEOUT"
);

/// `PROGRESS_SETTLE` (the read-path settle window) must stay shorter than
/// `PROGRESS_LATCH_IDLE` (the write-path latch threshold) -- see
/// `bridge::indexing::PROGRESS_LATCH_IDLE`'s doc for why collapsing the two
/// into a single threshold reintroduces a real regression (N1).
const _: () = assert!(
    PROGRESS_SETTLE.as_nanos() < PROGRESS_LATCH_IDLE.as_nanos(),
    "PROGRESS_SETTLE must be less than PROGRESS_LATCH_IDLE"
);

/// A settle window longer than the gate's own wait timeout would let
/// `wait_for_indexing_ready` time out while the entry is merely mid-settle,
/// not actually still loading.
const _: () = assert!(
    PROGRESS_SETTLE.as_nanos() < INDEXING_READY_TIMEOUT.as_nanos(),
    "PROGRESS_SETTLE must be less than INDEXING_READY_TIMEOUT"
);

/// Flattens a `Definition` (`Location` or `Location[]`) into an owned `Vec`.
fn definition_to_locations(definition: lsp_types::Definition) -> Vec<lsp_types::Location> {
    match definition {
        lsp_types::Definition::Location(loc) => vec![loc],
        lsp_types::Definition::LocationList(locs) => locs,
    }
}

/// Converts a `DefinitionLink` into a plain `Location` pointing at its target.
fn definition_link_to_location(link: lsp_types::DefinitionLink) -> lsp_types::Location {
    lsp_types::Location {
        uri: link.target_uri,
        range: link.target_selection_range,
    }
}

/// Converts raw LSP locations into MCP-facing `Location` values, normalizing
/// each range into the caller's 1-based coordinate space.
///
/// Deliberately not filtered to workspace roots: unlike a write-bearing
/// `WorkspaceEdit` (see `edits.rs`), a goto-X/references location is
/// read-only, and legitimate results routinely point outside the workspace
/// (e.g. the standard library or a crates.io dependency) -- dropping those
/// would break ordinary navigation. Any subsequent attempt to open or read
/// the path this location names still goes through the inbound
/// `validate_path_against_roots` gate (`mcp/server.rs`), which fails closed,
/// so the untrusted-URI concern is already covered downstream.
async fn lsp_locations_to_mcp(locs: Vec<lsp_types::Location>, ctx: &EncodingCtx) -> Vec<Location> {
    let mut locations = Vec::with_capacity(locs.len());
    for loc in locs {
        locations.push(Location {
            uri: loc.uri.to_string(),
            range: ctx.normalize_range(&loc.uri, loc.range).await,
            out_of_workspace: ctx.is_out_of_workspace(&loc.uri),
        });
    }
    locations
}

/// The two response shapes shared by `textDocument/definition`,
/// `textDocument/implementation`, and `textDocument/typeDefinition`: either a
/// single `Definition` (`Location` or `Location[]`), or a `DefinitionLink[]`
/// from clients that opted into `LinkSupport`.
enum GotoKind {
    /// A plain `Definition`, as returned to clients without `LinkSupport`.
    Definition(lsp_types::Definition),
    /// A `DefinitionLink[]`, as returned to clients with `LinkSupport`.
    DefinitionLinkList(Vec<lsp_types::DefinitionLink>),
}

/// Implemented once per go-to-X response enum so [`goto_response_to_locations`]
/// can normalize all three through one code path instead of three near-identical
/// match arms.
trait GotoResponse {
    /// Reduce the response enum down to the two variants shared by every
    /// go-to-X LSP response.
    fn into_kind(self) -> GotoKind;
}

impl GotoResponse for lsp_types::DefinitionResponse {
    fn into_kind(self) -> GotoKind {
        match self {
            Self::Definition(def) => GotoKind::Definition(def),
            Self::DefinitionLinkList(links) => GotoKind::DefinitionLinkList(links),
        }
    }
}

impl GotoResponse for lsp_types::ImplementationResponse {
    fn into_kind(self) -> GotoKind {
        match self {
            Self::Definition(def) => GotoKind::Definition(def),
            Self::DefinitionLinkList(links) => GotoKind::DefinitionLinkList(links),
        }
    }
}

impl GotoResponse for lsp_types::TypeDefinitionResponse {
    fn into_kind(self) -> GotoKind {
        match self {
            Self::Definition(def) => GotoKind::Definition(def),
            Self::DefinitionLinkList(links) => GotoKind::DefinitionLinkList(links),
        }
    }
}

/// Normalize a go-to-X response (`textDocument/definition`,
/// `textDocument/implementation`, or `textDocument/typeDefinition`) into a
/// flat list of MCP `Location` values.
async fn goto_response_to_locations<R: GotoResponse>(
    response: Option<R>,
    ctx: &EncodingCtx,
) -> Vec<Location> {
    let lsp_locs = match response.map(GotoResponse::into_kind) {
        Some(GotoKind::Definition(def)) => definition_to_locations(def),
        Some(GotoKind::DefinitionLinkList(links)) => {
            links.into_iter().map(definition_link_to_location).collect()
        }
        None => vec![],
    };
    lsp_locations_to_mcp(lsp_locs, ctx).await
}

/// Implemented once per go-to-X request params type so [`Translator::handle_goto`]
/// can build request params generically instead of duplicating the
/// `TextDocumentPositionParams` wiring per handler.
trait GotoParams: Sized {
    /// Build the request params from the resolved document position, filling
    /// the remaining fields (work-done/partial-result progress) with defaults.
    fn from_position(text_document_position_params: TextDocumentPositionParams) -> Self;
}

impl GotoParams for lsp_types::DefinitionParams {
    fn from_position(text_document_position_params: TextDocumentPositionParams) -> Self {
        Self {
            text_document_position_params,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }
    }
}

impl GotoParams for lsp_types::ImplementationParams {
    fn from_position(text_document_position_params: TextDocumentPositionParams) -> Self {
        Self {
            text_document_position_params,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }
    }
}

impl GotoParams for lsp_types::TypeDefinitionParams {
    fn from_position(text_document_position_params: TextDocumentPositionParams) -> Self {
        Self {
            text_document_position_params,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }
    }
}

/// Extracts hover contents as a plain string.
///
/// `MarkedString` is `#[deprecated]` in favor of `MarkupContent`, but LSP
/// 3.17 servers may still send it inside `Hover.contents` -- dropping
/// support would silently discard hover text from those servers, so this
/// (and `marked_string_to_string`) carry a narrow, scoped allow rather than
/// rewriting to `MarkupContent`-only.
#[allow(deprecated)]
fn extract_hover_contents(contents: lsp_types::Contents) -> String {
    match contents {
        lsp_types::Contents::MarkedString(marked_string) => marked_string_to_string(marked_string),
        lsp_types::Contents::MarkedStringList(marked_strings) => marked_strings
            .into_iter()
            .map(marked_string_to_string)
            .collect::<Vec<_>>()
            .join("\n\n"),
        lsp_types::Contents::MarkupContent(markup) => markup.value,
    }
}

/// Convert a marked string to a plain string.
#[allow(deprecated)]
fn marked_string_to_string(marked: lsp_types::MarkedString) -> String {
    match marked {
        lsp_types::MarkedString::String(s) => s,
        lsp_types::MarkedString::MarkedStringWithLanguage(ls) => {
            format!("```{}\n{}\n```", ls.language, ls.value)
        }
    }
}

impl Translator {
    /// Wait for the routed server `server_id` to finish its initial
    /// workspace-load/indexing phase before a whole-workspace query (hover,
    /// definition, implementation, type definition, references, rename,
    /// completions, code actions) reaches it. Called from
    /// [`Translator::prepare_gated_document`] for every call site declared
    /// [`IndexingGate::Required`].
    ///
    /// Returns immediately, without waiting, unless
    /// [`crate::bridge::NotificationCache::indexing_state`] currently
    /// reports [`IndexingState::Loading`] for `server_id` -- i.e. a
    /// recognized signal has positively indicated indexing is in progress.
    /// A server that has never reported any readiness signal
    /// ([`IndexingState::Unknown`]) is treated the same as
    /// [`IndexingState::Ready`]: without evidence indexing is happening,
    /// waiting would only add latency for servers and workspaces that have
    /// no indexing phase at all.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WorkspaceIndexing`] if the server is still
    /// [`IndexingState::Loading`] after the translator's configured
    /// indexing-ready timeout (default [`INDEXING_READY_TIMEOUT`],
    /// overridable via [`Self::with_indexing_ready_timeout`]) elapses.
    pub(super) async fn wait_for_indexing_ready(&self, server_id: &ServerId) -> Result<()> {
        self.wait_for_indexing_ready_with(
            server_id,
            self.indexing_ready_timeout,
            INDEXING_POLL_INTERVAL,
        )
        .await
    }

    /// [`Self::wait_for_indexing_ready`] with an injectable timeout and poll
    /// interval, so tests can exercise the timeout path without waiting out
    /// the real default.
    ///
    /// On timeout this returns [`Error::WorkspaceIndexing`] to the caller
    /// *without* mutating any shared state -- self-healing for a stuck
    /// `Loading` signal (a dropped `quiescent: true` notification, or a
    /// server that stalls mid-index) is handled entirely by
    /// [`crate::bridge::NotificationCache::indexing_state`]'s own
    /// read-time staleness check, keyed to the signal's age rather than
    /// this call's. Earlier revisions reset the shared entry here on
    /// timeout, which let one caller's short timeout silently un-gate
    /// every other concurrent or later caller before its own deadline;
    /// never reintroduce a write here.
    async fn wait_for_indexing_ready_with(
        &self,
        server_id: &ServerId,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<()> {
        let Some(cache) = self.notification_cache.as_ref() else {
            return Ok(());
        };

        let start = Instant::now();
        let deadline = start + timeout;
        loop {
            let state = cache.lock().await.indexing_state(server_id);
            if state != IndexingState::Loading {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::WorkspaceIndexing {
                    server_id: server_id.clone(),
                    elapsed_secs: start.elapsed().as_secs(),
                });
            }
            tokio::time::sleep(poll_interval.min(remaining)).await;
        }
    }

    /// Handle hover request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// the routed server does not advertise `hoverProvider` support, or the
    /// server is still indexing the workspace after
    /// `INDEXING_READY_TIMEOUT`.
    pub async fn handle_hover(&self, file_path: String, position: Position) -> Result<HoverResult> {
        let Position { line, character } = position;
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::Hover,
                Capability::Hover,
                IndexingGate::Required,
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;
        let response_uri = uri.clone();

        let params = LspHoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::HoverRequest>(params, client.request_timeout())
            .await?;

        let result = match response {
            Some(hover) => {
                let contents = extract_hover_contents(hover.contents);
                let range = match hover.range {
                    Some(r) => Some(ctx.normalize_range(&response_uri, r).await),
                    None => None,
                };
                HoverResult { contents, range }
            }
            None => HoverResult {
                contents: "No hover information available".to_string(),
                range: None,
            },
        };

        Ok(result)
    }

    /// Shared implementation of the go-to-X handlers (`textDocument/definition`,
    /// `textDocument/implementation`, `textDocument/typeDefinition`): gate on
    /// the request's capability and on indexing readiness, translate the MCP
    /// position into LSP coordinates, dispatch the LSP request, and flatten
    /// the response into MCP locations. Each public handler supplies its
    /// request type via `R` plus the capability key/predicate specific to
    /// it.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// the routed server does not advertise `capability` support, or the
    /// server is still indexing the workspace after `INDEXING_READY_TIMEOUT`.
    async fn handle_goto<R, T>(
        &self,
        file_path: &str,
        position: Position,
        tool: ToolKind,
        capability: Capability,
    ) -> Result<Vec<Location>>
    where
        R: lsp_types::Request<Result = Option<T>>,
        R::Params: GotoParams,
        T: GotoResponse,
    {
        let Position { line, character } = position;
        let (server_id, client, uri) = self
            .prepare_gated_document(file_path, tool, capability, IndexingGate::Required)
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = R::Params::from_position(TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position: lsp_position,
        });

        let response = client
            .request_typed::<R>(params, client.request_timeout())
            .await?;

        Ok(goto_response_to_locations(response, &ctx).await)
    }

    /// Handle definition request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// the routed server does not advertise `definitionProvider` support, or
    /// the server is still indexing the workspace after
    /// `INDEXING_READY_TIMEOUT`.
    pub async fn handle_definition(
        &self,
        file_path: String,
        position: Position,
    ) -> Result<DefinitionResult> {
        let locations = self
            .handle_goto::<lsp_types::DefinitionRequest, _>(
                &file_path,
                position,
                ToolKind::Definition,
                Capability::Definition,
            )
            .await?;

        Ok(DefinitionResult { locations })
    }

    /// Handle references request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// the routed server does not advertise `referencesProvider` support, or
    /// the server is still indexing the workspace after
    /// `INDEXING_READY_TIMEOUT`.
    pub async fn handle_references(
        &self,
        file_path: String,
        position: Position,
        include_declaration: bool,
    ) -> Result<ReferencesResult> {
        let Position { line, character } = position;
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::References,
                Capability::References,
                IndexingGate::Required,
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = ReferenceParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration,
            },
        };

        let response = client
            .request_typed::<lsp_types::ReferencesRequest>(params, client.request_timeout())
            .await?;

        let locations = response.unwrap_or_default();
        let result_locations = lsp_locations_to_mcp(locations, &ctx).await;
        let result = ReferencesResult {
            locations: result_locations,
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
    /// the routed server does not advertise `implementationProvider`
    /// support, or the server is still indexing the workspace after
    /// `INDEXING_READY_TIMEOUT`.
    pub async fn handle_implementation(
        &self,
        file_path: String,
        position: Position,
    ) -> Result<LocationsResult> {
        let locations = self
            .handle_goto::<lsp_types::ImplementationRequest, _>(
                &file_path,
                position,
                ToolKind::Implementation,
                Capability::Implementation,
            )
            .await?;

        Ok(LocationsResult { locations })
    }

    /// Handle go-to-type-definition request (`textDocument/typeDefinition`).
    ///
    /// Returns the type definition location of the expression at position. Distinct
    /// from go-to-definition for variable bindings where definition and type differ.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// the routed server does not advertise `typeDefinitionProvider`
    /// support, or the server is still indexing the workspace after
    /// `INDEXING_READY_TIMEOUT`.
    pub async fn handle_type_definition(
        &self,
        file_path: String,
        position: Position,
    ) -> Result<LocationsResult> {
        let locations = self
            .handle_goto::<lsp_types::TypeDefinitionRequest, _>(
                &file_path,
                position,
                ToolKind::TypeDefinition,
                Capability::TypeDefinition,
            )
            .await?;

        Ok(LocationsResult { locations })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, deprecated)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::sync::Mutex;
    use tokio::time::timeout;
    use url::Url;

    use super::*;
    use crate::bridge::NotificationCache;
    use crate::bridge::translator::testing::*;
    use crate::config::ServerId;

    // -----------------------------------------------------------------
    // Indexing readiness gate (`Translator::wait_for_indexing_ready`)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn test_wait_for_indexing_ready_without_cache_is_noop() {
        // No wired cache (most fixtures) must never block -- see `Translator::notification_cache`'s field doc.
        let translator = Translator::new();
        let server_id = ServerId::from("rust");

        translator
            .wait_for_indexing_ready(&server_id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_indexing_ready_unknown_state_is_noop() {
        let translator = Translator::new()
            .with_notification_cache(Arc::new(Mutex::new(NotificationCache::new())));
        let server_id = ServerId::from("rust");

        translator
            .wait_for_indexing_ready(&server_id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_indexing_ready_ready_state_is_noop() {
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let server_id = ServerId::from("rust");
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        let translator = Translator::new().with_notification_cache(cache);

        translator
            .wait_for_indexing_ready(&server_id)
            .await
            .unwrap();
    }

    /// #424: `with_indexing_ready_timeout` must actually change the bound
    /// `wait_for_indexing_ready` (the public entry point, not the
    /// timeout-injectable `_with` test helper) waits before giving up --
    /// pins the config wiring end-to-end rather than only the constructor
    /// storing the value.
    #[tokio::test(start_paused = true)]
    async fn test_wait_for_indexing_ready_uses_configured_timeout_override() {
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let server_id = ServerId::from("rust");
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = Translator::new()
            .with_notification_cache(cache)
            .with_indexing_ready_timeout(Duration::from_secs(5));

        let start = Instant::now();
        let err = translator
            .wait_for_indexing_ready(&server_id)
            .await
            .unwrap_err();

        assert!(matches!(err, Error::WorkspaceIndexing { elapsed_secs, .. } if elapsed_secs == 5));
        assert_eq!(start.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn test_wait_for_indexing_ready_loading_times_out() {
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let server_id = ServerId::from("rust");
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = Translator::new().with_notification_cache(cache);

        let err = translator
            .wait_for_indexing_ready_with(
                &server_id,
                Duration::from_millis(50),
                Duration::from_millis(10),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == ServerId::from("rust")
        ));
    }

    #[tokio::test]
    async fn test_wait_for_indexing_ready_returns_ok_once_signaled_ready() {
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let server_id = ServerId::from("rust");
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = Translator::new().with_notification_cache(Arc::clone(&cache));

        let waiter = {
            let server_id = server_id.clone();
            tokio::spawn(async move {
                translator
                    .wait_for_indexing_ready_with(
                        &server_id,
                        Duration::from_secs(5),
                        Duration::from_millis(10),
                    )
                    .await
            })
        };

        tokio::time::sleep(Duration::from_millis(30)).await;
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );

        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter task timed out")
            .expect("waiter task panicked")
            .expect("expected Ok once quiescent");
    }

    /// End-to-end: a real handler (`handle_hover`) must surface
    /// `Error::WorkspaceIndexing` -- not an empty/`null` result -- when the
    /// routed server is still `Loading`, without ever reaching the fake LSP
    /// server. Runs under paused virtual time so it does not actually wait
    /// out the real `INDEXING_READY_TIMEOUT`.
    #[tokio::test(start_paused = true)]
    async fn test_handle_hover_returns_workspace_indexing_error_when_loading() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            hover_provider: Some(lsp_types::HoverProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, _server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = translator.with_notification_cache(cache);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let err = translator
            .handle_hover(path.to_string_lossy().to_string(), pos(1, 1))
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, elapsed_secs: 30 } if id == server_id
        ));
    }

    /// Companion to the timeout test above: when the cache reports `Ready`,
    /// `handle_hover` must dispatch normally with no added delay.
    #[tokio::test]
    async fn test_handle_hover_dispatches_when_indexing_ready() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            hover_provider: Some(lsp_types::HoverProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        let translator = Arc::new(translator.with_notification_cache(cache));

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move { translator.handle_hover(path, pos(1, 1)).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/hover");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!({
                "contents": {"kind": "markdown", "value": "hover text"}
            }),
        )
        .await;

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result.contents, "hover text");
    }

    /// End-to-end: `handle_definition` must surface `Error::WorkspaceIndexing`
    /// while the routed server is still `Loading`, without reaching the fake
    /// LSP server.
    #[tokio::test(start_paused = true)]
    async fn test_handle_definition_returns_workspace_indexing_error_when_loading() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            definition_provider: Some(lsp_types::DefinitionProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, _server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = translator.with_notification_cache(cache);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let err = translator
            .handle_definition(path.to_string_lossy().to_string(), pos(1, 1))
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        ));
    }

    /// Companion: when the cache reports `Ready`, `handle_definition` must
    /// dispatch normally.
    #[tokio::test]
    async fn test_handle_definition_dispatches_when_indexing_ready() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            definition_provider: Some(lsp_types::DefinitionProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        let translator = Arc::new(translator.with_notification_cache(cache));

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move { translator.handle_definition(path, pos(1, 1)).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/definition");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::Value::Null,
        )
        .await;

        let result = handle.await.unwrap().unwrap();
        assert!(result.locations.is_empty());
    }

    /// End-to-end: `handle_references` must surface `Error::WorkspaceIndexing`
    /// while the routed server is still `Loading`, without reaching the fake
    /// LSP server.
    #[tokio::test(start_paused = true)]
    async fn test_handle_references_returns_workspace_indexing_error_when_loading() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            references_provider: Some(lsp_types::ReferencesProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, _server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = translator.with_notification_cache(cache);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let err = translator
            .handle_references(path.to_string_lossy().to_string(), pos(1, 1), true)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        ));
    }

    /// Companion: when the cache reports `Ready`, `handle_references` must
    /// dispatch normally.
    #[tokio::test]
    async fn test_handle_references_dispatches_when_indexing_ready() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            references_provider: Some(lsp_types::ReferencesProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        let translator = Arc::new(translator.with_notification_cache(cache));

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move { translator.handle_references(path, pos(1, 1), true).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/references");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::Value::Null,
        )
        .await;

        let result = handle.await.unwrap().unwrap();
        assert!(result.locations.is_empty());
    }

    /// S3 fix: `handle_implementation` shares `handle_goto` with
    /// `handle_definition` and must now be gated the same way --
    /// `textDocument/implementation` needs the whole-crate trait-impl index,
    /// which is at least as index-dependent as `definition`.
    #[tokio::test(start_paused = true)]
    async fn test_handle_implementation_returns_workspace_indexing_error_when_loading() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            implementation_provider: Some(lsp_types::ImplementationProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, _server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = translator.with_notification_cache(cache);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let err = translator
            .handle_implementation(path.to_string_lossy().to_string(), pos(1, 1))
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        ));
    }

    /// S3 fix, companion for `handle_type_definition`.
    #[tokio::test(start_paused = true)]
    async fn test_handle_type_definition_returns_workspace_indexing_error_when_loading() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            type_definition_provider: Some(lsp_types::TypeDefinitionProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, _server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = translator.with_notification_cache(cache);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let err = translator
            .handle_type_definition(path.to_string_lossy().to_string(), pos(1, 1))
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        ));
    }

    /// A timed-out wait must return `Error::WorkspaceIndexing` to its own
    /// caller without mutating the shared cache entry -- a fixed-in-review
    /// regression had the timeout handler reset the entry to `Unknown`,
    /// which released every other concurrent/later caller early (see
    /// `test_wait_for_indexing_ready_one_callers_timeout_does_not_release_another`
    /// for the direct reproduction). Self-healing for a genuinely stuck
    /// signal now lives entirely in
    /// `NotificationCache::indexing_state`'s own staleness check.
    #[tokio::test]
    async fn test_wait_for_indexing_ready_timeout_does_not_mutate_shared_state() {
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let server_id = ServerId::from("rust");
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = Translator::new().with_notification_cache(Arc::clone(&cache));

        translator
            .wait_for_indexing_ready_with(
                &server_id,
                Duration::from_millis(30),
                Duration::from_millis(10),
            )
            .await
            .unwrap_err();

        assert_eq!(
            cache.lock().await.indexing_state(&server_id),
            IndexingState::Loading,
            "a timed-out wait must not touch the shared entry -- it is still fresh, so it must \
             still read as Loading for any other caller"
        );
    }

    /// Direct reproduction of the self-heal race: a short-timeout waiter's
    /// own timeout must never resolve a concurrent long-timeout waiter's
    /// independent wait early. Before the fix, both waiters observed the
    /// same shared `IndexingState`, and the short waiter's timeout handler
    /// reset it to `Unknown` as a side effect -- silently un-gating the
    /// long waiter tens of seconds before its own deadline.
    #[tokio::test]
    async fn test_wait_for_indexing_ready_one_callers_timeout_does_not_release_another() {
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let server_id = ServerId::from("rust");
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = Arc::new(Translator::new().with_notification_cache(Arc::clone(&cache)));

        let short = {
            let translator = Arc::clone(&translator);
            let server_id = server_id.clone();
            tokio::spawn(async move {
                translator
                    .wait_for_indexing_ready_with(
                        &server_id,
                        Duration::from_millis(80),
                        Duration::from_millis(10),
                    )
                    .await
            })
        };
        let long = {
            let translator = Arc::clone(&translator);
            let server_id = server_id.clone();
            tokio::spawn(async move {
                translator
                    .wait_for_indexing_ready_with(
                        &server_id,
                        Duration::from_secs(30),
                        Duration::from_millis(10),
                    )
                    .await
            })
        };

        let short_result = short.await.unwrap();
        assert!(
            matches!(short_result, Err(Error::WorkspaceIndexing { .. })),
            "the short-timeout waiter must time out on its own schedule, got {short_result:?}"
        );

        // Well past the short waiter's 80ms deadline, nowhere near the long
        // waiter's 30s one.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !long.is_finished(),
            "a concurrent caller's short timeout must never resolve another caller's \
             independent wait early"
        );
        long.abort();
    }

    #[test]
    fn test_extract_hover_contents_string() {
        let marked_string = lsp_types::MarkedString::String("Test hover".to_string());
        let contents = lsp_types::Contents::MarkedString(marked_string);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "Test hover");
    }

    #[test]
    fn test_extract_hover_contents_language_string() {
        let marked_string = lsp_types::MarkedString::MarkedStringWithLanguage(
            lsp_types::MarkedStringWithLanguage {
                language: "rust".to_string(),
                value: "fn main() {}".to_string(),
            },
        );
        let contents = lsp_types::Contents::MarkedString(marked_string);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "```rust\nfn main() {}\n```");
    }

    #[test]
    fn test_extract_hover_contents_markup() {
        let markup = lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::Markdown,
            value: "# Documentation".to_string(),
        };
        let contents = lsp_types::Contents::MarkupContent(markup);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "# Documentation");
    }

    /// Success-path coverage for `handle_definition` through the
    /// `Definition::Location` -> `GotoKind::Definition` arm, pinning the
    /// `GotoResponse` impl for `DefinitionResponse`.
    #[tokio::test]
    async fn test_handle_definition_flattens_single_location() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            definition_provider: Some(lsp_types::DefinitionProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let target_path = dir.path().join("target.rs");
        fs::write(&target_path, "fn target() {}").unwrap();
        let target_uri = Url::from_file_path(&target_path).unwrap().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_definition(
                        path,
                        Position {
                            line: 1,
                            character: 1,
                        },
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/definition");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!({
                "uri": target_uri,
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 6}
                }
            }),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_definition should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(result.locations.len(), 1);
        assert_eq!(result.locations[0].uri, target_uri);
        assert!(
            !result.locations[0].out_of_workspace,
            "a definition location inside the workspace root must not be marked out_of_workspace"
        );
    }

    /// #415 (revised per critic C1): a definition location whose URI falls
    /// outside every configured workspace root must still be returned --
    /// goto-definition into the standard library or a crates.io dependency
    /// is normal, expected navigation, not an attack. The untrusted-URI
    /// concern is instead covered downstream, by the inbound
    /// `validate_path_against_roots` gate any subsequent open/read of the
    /// path would hit.
    #[tokio::test]
    async fn test_handle_definition_does_not_filter_out_of_workspace_location() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            definition_provider: Some(lsp_types::DefinitionProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let outside_uri = "file:///outside/workspace/stdlib.rs";

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_definition(
                        path,
                        Position {
                            line: 1,
                            character: 1,
                        },
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/definition");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!({
                "uri": outside_uri,
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 6}
                }
            }),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_definition should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(
            result.locations.len(),
            1,
            "an out-of-workspace definition location (e.g. stdlib/a dependency) must be \
             returned, not dropped"
        );
        assert_eq!(result.locations[0].uri, outside_uri);
        assert!(
            result.locations[0].out_of_workspace,
            "a definition location outside every workspace root must be marked out_of_workspace"
        );
    }

    /// #415 (revised per critic C1) companion for `handle_references`: an
    /// out-of-workspace location must pass through unfiltered, same as an
    /// in-workspace one -- see `test_handle_definition_does_not_filter_out_of_workspace_location`.
    #[tokio::test]
    async fn test_handle_references_does_not_filter_out_of_workspace_location() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            references_provider: Some(lsp_types::ReferencesProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let inside_path = dir.path().join("inside.rs");
        fs::write(&inside_path, "fn used() {}").unwrap();
        let inside_uri = Url::from_file_path(&inside_path).unwrap().to_string();
        let outside_uri = "file:///outside/workspace/stdlib.rs";

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move { translator.handle_references(path, pos(1, 1), true).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/references");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([
                {
                    "uri": inside_uri,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 4}
                    }
                },
                {
                    "uri": outside_uri,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 4}
                    }
                }
            ]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_references should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(
            result.locations.len(),
            2,
            "both the in-workspace and out-of-workspace locations must survive"
        );
        assert!(result.locations.iter().any(|l| l.uri == inside_uri));
        assert!(result.locations.iter().any(|l| l.uri == outside_uri));
        assert!(
            !result
                .locations
                .iter()
                .find(|l| l.uri == inside_uri)
                .unwrap()
                .out_of_workspace,
            "an in-workspace reference location must not be marked out_of_workspace"
        );
        assert!(
            result
                .locations
                .iter()
                .find(|l| l.uri == outside_uri)
                .unwrap()
                .out_of_workspace,
            "an out-of-workspace reference location must be marked out_of_workspace"
        );
    }

    /// Success-path coverage for `handle_implementation` through the
    /// `Definition::LocationList` -> `GotoKind::Definition` arm, pinning the
    /// `GotoResponse` impl for `ImplementationResponse`.
    #[tokio::test]
    async fn test_handle_implementation_flattens_location_list() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            implementation_provider: Some(lsp_types::ImplementationProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let first_impl_path = dir.path().join("impl_a.rs");
        fs::write(&first_impl_path, "struct A;").unwrap();
        let first_impl_uri = Url::from_file_path(&first_impl_path).unwrap().to_string();
        let second_impl_path = dir.path().join("impl_b.rs");
        fs::write(&second_impl_path, "struct B;").unwrap();
        let second_impl_uri = Url::from_file_path(&second_impl_path).unwrap().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_implementation(
                        path,
                        Position {
                            line: 1,
                            character: 1,
                        },
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/implementation");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([
                {
                    "uri": first_impl_uri,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 9}
                    }
                },
                {
                    "uri": second_impl_uri,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 9}
                    }
                }
            ]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_implementation should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(result.locations.len(), 2);
        assert_eq!(result.locations[0].uri, first_impl_uri);
        assert_eq!(result.locations[1].uri, second_impl_uri);
    }

    /// Success-path coverage for `handle_type_definition` through the
    /// `DefinitionLinkList` -> `GotoKind::DefinitionLinkList` arm, pinning
    /// the `GotoResponse` impl for `TypeDefinitionResponse` and the
    /// `definition_link_to_location` mapping (`target_selection_range`, not
    /// `target_range`).
    #[tokio::test]
    async fn test_handle_type_definition_flattens_definition_link_list() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            type_definition_provider: Some(lsp_types::TypeDefinitionProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let target_path = dir.path().join("target_type.rs");
        fs::write(&target_path, "struct TargetType;").unwrap();
        let target_uri = Url::from_file_path(&target_path).unwrap().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_type_definition(
                        path,
                        Position {
                            line: 1,
                            character: 1,
                        },
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/typeDefinition");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "targetUri": target_uri,
                "targetRange": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 18}
                },
                "targetSelectionRange": {
                    "start": {"line": 0, "character": 7},
                    "end": {"line": 0, "character": 17}
                }
            }]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_type_definition should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(result.locations.len(), 1);
        assert_eq!(result.locations[0].uri, target_uri);
        assert_eq!(result.locations[0].range.start.character, 8);
    }
}
