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
    DefinitionResult, HoverResult, Location, LocationsResult, Position, PositionDegradation,
    ReferencesResult,
};
use super::enclosing::{Contextualized, ResultContext};
use super::encoding_ctx::EncodingCtx;
use super::routing::{Capability, IndexingGate};
use crate::bridge::indexing::{
    DEFAULT_INDEXING_READY_TIMEOUT_SECS, INDEXING_STALENESS_BOUND, PROGRESS_LATCH_IDLE,
    PROGRESS_SETTLE,
};
use crate::bridge::{ClientPath, IndexingState};
use crate::config::ServerId;
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

/// Hard cap on the number of items a single call normalizes (`goto`,
/// `references`, `workspace_symbol_search`, call hierarchy, inlay hints,
/// workspace-edit entries). Without a limit, a response naming an unbounded
/// number of items turns one MCP tool call into an unbounded number of range
/// conversions -- each one a potential disk read on a cache miss -- letting a
/// hostile or misbehaving LSP server amplify one request into massive I/O
/// (see #474, #487). Applied before normalization, not after, so it bounds
/// the work actually done rather than just the size of the returned list.
/// Also used by `Translator::handle_workspace_symbol` to clamp its
/// caller-supplied `limit`, which otherwise has no upper bound of its own.
pub(super) const MAX_NORMALIZED_LOCATIONS: usize = 10_000;

/// Per-response allowance of items, [`MAX_NORMALIZED_LOCATIONS`] unless built
/// with [`ItemBudget::with_cap`].
///
/// Every item a handler normalizes must first pass through [`Self::admit`],
/// so nested loops (e.g. call hierarchy `fromRanges`, workspace-edit
/// per-file edit lists) share one budget and total work stays within the cap
/// rather than multiplying per level (#487).
#[derive(Debug)]
pub(super) struct ItemBudget {
    remaining: usize,
    cap: usize,
    truncated: bool,
}

impl ItemBudget {
    /// A fresh budget holding the full [`MAX_NORMALIZED_LOCATIONS`].
    pub(super) const fn new() -> Self {
        Self::with_cap(MAX_NORMALIZED_LOCATIONS)
    }

    /// A fresh budget holding `cap` items.
    pub(super) const fn with_cap(cap: usize) -> Self {
        Self {
            remaining: cap,
            cap,
            truncated: false,
        }
    }

    /// Keeps at most the remaining allowance of `items`, spending it, and
    /// records whether anything was dropped. Logs one `warn!` on the first
    /// drop of the response.
    pub(super) fn admit<T>(&mut self, mut items: Vec<T>) -> Vec<T> {
        if items.len() > self.remaining {
            self.record_drop(items.len());
            items.truncate(self.remaining);
        }
        self.remaining = self.remaining.saturating_sub(items.len());
        items
    }

    /// Like [`Self::admit`] for a lazy sequence: pulls at most the remaining
    /// allowance plus one item, so the cost stays bounded by the cap however
    /// long the sequence is.
    pub(super) fn admit_iter<T>(&mut self, items: impl IntoIterator<Item = T>) -> Vec<T> {
        let mut items = items.into_iter();
        let kept: Vec<T> = items.by_ref().take(self.remaining).collect();
        if items.next().is_some() {
            self.record_drop(kept.len().saturating_add(1));
        }
        self.remaining = self.remaining.saturating_sub(kept.len());
        kept
    }

    /// Admits `items` only if all of them fit the remaining allowance;
    /// otherwise admits none and records the drop. For lists that must not be
    /// cut midway, such as one file's workspace edits.
    pub(super) fn admit_whole<T>(&mut self, items: Vec<T>) -> Option<Vec<T>> {
        self.spend_whole(items.len()).then_some(items)
    }

    /// Spends `count` items of the allowance if all of them fit, otherwise
    /// spends nothing and records the drop. For a group, such as a call and
    /// its ranges, that must be kept or dropped together.
    pub(super) fn spend_whole(&mut self, count: usize) -> bool {
        if count > self.remaining {
            self.record_drop(count);
            return false;
        }
        self.remaining = self.remaining.saturating_sub(count);
        true
    }

    fn record_drop(&mut self, reported: usize) {
        if !self.truncated {
            tracing::warn!(
                reported,
                cap = self.cap,
                "LSP response item count exceeds the item cap; truncating"
            );
        }
        self.truncated = true;
    }

    /// Whether any admission ([`Self::admit`], [`Self::admit_whole`], or
    /// [`Self::spend_whole`]) dropped items.
    pub(super) const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Converts raw LSP locations into MCP-facing `Location` values, normalizing
/// each range into the caller's 1-based coordinate space.
///
/// Truncates to [`MAX_NORMALIZED_LOCATIONS`] first -- see its doc. Logs a
/// single `warn!` when that truncation actually drops locations, so a
/// response silently capped below what the LSP server reported is at least
/// visible in logs (see #474).
///
/// Deliberately not filtered to workspace roots: unlike a write-bearing
/// `WorkspaceEdit` (see `edits.rs`), a goto-X/references location is
/// read-only, and legitimate results routinely point outside the workspace
/// (e.g. the standard library or a crates.io dependency) -- dropping those
/// would break ordinary navigation. Any subsequent attempt to open or read
/// the path this location names still goes through the inbound
/// `WorkspaceRoots::validate` gate (`mcp/server.rs`), which fails closed,
/// so the untrusted-URI concern is already covered downstream.
async fn lsp_locations_to_mcp(
    locs: Vec<lsp_types::Location>,
    ctx: &EncodingCtx,
) -> NormalizedLocations {
    let mut budget = ItemBudget::new();
    let locs = budget.admit(locs);
    let truncated = budget.truncated();
    let mut locations = Vec::with_capacity(locs.len());
    for loc in locs {
        locations.push(Location {
            uri: loc.uri.to_string(),
            range: ctx.normalize_range(&loc.uri, loc.range).await,
            out_of_workspace: ctx.is_out_of_workspace(&loc.uri),
        });
    }
    NormalizedLocations {
        locations,
        truncated,
        positions_degraded: ctx.positions_degraded(),
    }
}

/// [`lsp_locations_to_mcp`]'s result: the normalized locations plus whether
/// [`MAX_NORMALIZED_LOCATIONS`] actually dropped any of the LSP server's
/// reported locations -- surfaced to the MCP caller via each result DTO's
/// `truncated` field, since `references`'/goto-X's tool descriptions
/// otherwise imply a complete result (see #474) -- and whether any position
/// among them could not be resolved for encoding conversion while
/// normalizing (disk-read budget exhaustion, an unresolvable path, a line
/// past EOF, or invalid UTF-8 -- see `ctx.positions_degraded()`), surfaced
/// via each result DTO's `positions_degraded` field (#497).
struct NormalizedLocations {
    locations: Vec<Location>,
    truncated: bool,
    positions_degraded: Option<PositionDegradation>,
}

/// The two response shapes shared by `textDocument/definition`,
/// `textDocument/implementation`, and `textDocument/typeDefinition`: either a
/// single `Definition` (`Location` or `Location[]`), or a `DefinitionLink[]`
/// from clients that opted into `LinkSupport`.
enum GotoKind {
    /// Plain locations, as returned to clients without `LinkSupport`.
    Locations(Vec<lsp_types::Location>),
    /// A `LocationLink[]`, as returned to clients with `LinkSupport`.
    Links(Vec<lsp_types::LocationLink>),
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
            Self::Definition(def) => GotoKind::Locations(definition_to_locations(def)),
            Self::DefinitionLinkList(links) => GotoKind::Links(links),
        }
    }
}

impl GotoResponse for lsp_types::ImplementationResponse {
    fn into_kind(self) -> GotoKind {
        match self {
            Self::Definition(def) => GotoKind::Locations(definition_to_locations(def)),
            Self::DefinitionLinkList(links) => GotoKind::Links(links),
        }
    }
}

impl GotoResponse for lsp_types::TypeDefinitionResponse {
    fn into_kind(self) -> GotoKind {
        match self {
            Self::Definition(def) => GotoKind::Locations(definition_to_locations(def)),
            Self::DefinitionLinkList(links) => GotoKind::Links(links),
        }
    }
}

impl GotoResponse for lsp_types::DeclarationResponse {
    fn into_kind(self) -> GotoKind {
        match self {
            Self::Declaration(lsp_types::Declaration::Location(loc)) => {
                GotoKind::Locations(vec![loc])
            }
            Self::Declaration(lsp_types::Declaration::LocationList(locs)) => {
                GotoKind::Locations(locs)
            }
            Self::DeclarationLinkList(links) => GotoKind::Links(links),
        }
    }
}

/// Normalize a go-to-X response (`textDocument/definition`,
/// `textDocument/implementation`, `textDocument/typeDefinition`, or
/// `textDocument/declaration`) into a
/// flat list of MCP `Location` values.
async fn goto_response_to_locations<R: GotoResponse>(
    response: Option<R>,
    ctx: &EncodingCtx,
) -> NormalizedLocations {
    let lsp_locs = match response.map(GotoResponse::into_kind) {
        Some(GotoKind::Locations(locs)) => locs,
        Some(GotoKind::Links(links)) => {
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

impl GotoParams for lsp_types::DeclarationParams {
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
#[allow(deprecated, reason = "LSP servers still send this deprecated field")]
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
#[allow(deprecated, reason = "LSP servers still send this deprecated field")]
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
        #[allow(
            clippy::arithmetic_side_effects,
            reason = "production passes the fixed INDEXING_READY_TIMEOUT"
        )]
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
    pub async fn handle_hover(
        &self,
        file_path: ClientPath,
        position: Position,
    ) -> Result<HoverResult> {
        let doc = self
            .prepare_positioned_document(
                &file_path,
                Capability::Hover,
                IndexingGate::Required,
                &[position],
            )
            .await?;
        let (server_id, client, uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let lsp_position = ctx.to_lsp(uri, position).await;
        let response_uri = uri.clone();

        let params = LspHoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
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
                HoverResult {
                    contents,
                    range,
                    positions_degraded: ctx.positions_degraded(),
                }
            }
            None => HoverResult {
                contents: "No hover information available".to_string(),
                range: None,
                positions_degraded: ctx.positions_degraded(),
            },
        };

        Ok(result)
    }

    async fn contextualize(
        &self,
        normalized: NormalizedLocations,
        context: ResultContext,
    ) -> Contextualized<Location> {
        self.contextualize_locations(normalized.locations, context, normalized.positions_degraded)
            .await
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
        file_path: &ClientPath,
        position: Position,
        capability: Capability,
    ) -> Result<NormalizedLocations>
    where
        R: lsp_types::Request<Result = Option<T>>,
        R::Params: GotoParams,
        T: GotoResponse,
    {
        let doc = self
            .prepare_positioned_document(file_path, capability, IndexingGate::Required, &[position])
            .await?;
        let (server_id, client, uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let lsp_position = ctx.to_lsp(uri, position).await;

        let params = R::Params::from_position(TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
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
        file_path: ClientPath,
        position: Position,
        context: ResultContext,
    ) -> Result<DefinitionResult> {
        let normalized = self
            .handle_goto::<lsp_types::DefinitionRequest, _>(
                &file_path,
                position,
                Capability::Definition,
            )
            .await?;
        let truncated = normalized.truncated;
        let Contextualized {
            items: locations,
            enrichment,
            positions_degraded,
        } = self.contextualize(normalized, context).await;

        Ok(DefinitionResult {
            locations,
            truncated,
            positions_degraded,
            enrichment,
        })
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
        file_path: ClientPath,
        position: Position,
        include_declaration: bool,
        context: ResultContext,
    ) -> Result<ReferencesResult> {
        let doc = self
            .prepare_positioned_document(
                &file_path,
                Capability::References,
                IndexingGate::Required,
                &[position],
            )
            .await?;
        let (server_id, client, uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let lsp_position = ctx.to_lsp(uri, position).await;

        let params = ReferenceParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
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

        let normalized = lsp_locations_to_mcp(response.unwrap_or_default(), &ctx).await;
        let truncated = normalized.truncated;
        let Contextualized {
            items: locations,
            enrichment,
            positions_degraded,
        } = self.contextualize(normalized, context).await;

        Ok(ReferencesResult {
            locations,
            truncated,
            positions_degraded,
            enrichment,
        })
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
        file_path: ClientPath,
        position: Position,
        context: ResultContext,
    ) -> Result<LocationsResult> {
        let normalized = self
            .handle_goto::<lsp_types::ImplementationRequest, _>(
                &file_path,
                position,
                Capability::Implementation,
            )
            .await?;
        let truncated = normalized.truncated;
        let Contextualized {
            items: locations,
            enrichment,
            positions_degraded,
        } = self.contextualize(normalized, context).await;

        Ok(LocationsResult {
            locations,
            truncated,
            positions_degraded,
            enrichment,
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
    /// the routed server does not advertise `typeDefinitionProvider`
    /// support, or the server is still indexing the workspace after
    /// `INDEXING_READY_TIMEOUT`.
    pub async fn handle_type_definition(
        &self,
        file_path: ClientPath,
        position: Position,
        context: ResultContext,
    ) -> Result<LocationsResult> {
        let normalized = self
            .handle_goto::<lsp_types::TypeDefinitionRequest, _>(
                &file_path,
                position,
                Capability::TypeDefinition,
            )
            .await?;
        let truncated = normalized.truncated;
        let Contextualized {
            items: locations,
            enrichment,
            positions_degraded,
        } = self.contextualize(normalized, context).await;

        Ok(LocationsResult {
            locations,
            truncated,
            positions_degraded,
            enrichment,
        })
    }

    /// Handle go-to-declaration request (`textDocument/declaration`).
    ///
    /// Returns the declaration location of the symbol at position. Differs
    /// from go-to-definition for languages that separate declaration from
    /// definition (C/C++ headers, interface members).
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// the routed server does not advertise `declarationProvider` support, or
    /// the server is still indexing the workspace after
    /// `INDEXING_READY_TIMEOUT`.
    pub async fn handle_declaration(
        &self,
        file_path: ClientPath,
        position: Position,
        context: ResultContext,
    ) -> Result<LocationsResult> {
        let normalized = self
            .handle_goto::<lsp_types::DeclarationRequest, _>(
                &file_path,
                position,
                Capability::Declaration,
            )
            .await?;
        let truncated = normalized.truncated;
        let Contextualized {
            items: locations,
            enrichment,
            positions_degraded,
        } = self.contextualize(normalized, context).await;

        Ok(LocationsResult {
            locations,
            truncated,
            positions_degraded,
            enrichment,
        })
    }
}

#[cfg(test)]
#[allow(deprecated, reason = "LSP servers still send this deprecated field")]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;
    use std::{assert_matches, fs};

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::sync::Mutex;
    use tokio::time::timeout;
    use url::Url;

    use super::*;
    use crate::bridge::encoding::PositionEncoding;
    use crate::bridge::translator::testing::*;
    use crate::bridge::{NotificationCache, path_to_uri};
    use crate::config::{IndexingReadyTimeoutSecs, ServerId};
    use crate::test_lsp::client_path;
    use crate::util::lock_std;

    // -----------------------------------------------------------------
    // Indexing readiness gate (`Translator::wait_for_indexing_ready`)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn test_wait_for_indexing_ready_without_cache_is_noop() {
        // No wired cache (most fixtures) must never block -- see `Translator::notification_cache`'s field doc.
        let translator = Translator::new();
        let server_id = ServerId::from_static("rust");

        translator
            .wait_for_indexing_ready(&server_id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_indexing_ready_unknown_state_is_noop() {
        let translator = Translator::new()
            .with_notification_cache(Arc::new(Mutex::new(NotificationCache::new())));
        let server_id = ServerId::from_static("rust");

        translator
            .wait_for_indexing_ready(&server_id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_indexing_ready_ready_state_is_noop() {
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let server_id = ServerId::from_static("rust");
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
        let server_id = ServerId::from_static("rust");
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = Translator::new()
            .with_notification_cache(cache)
            .with_indexing_ready_timeout(IndexingReadyTimeoutSecs::new(5).unwrap());

        let start = Instant::now();
        let err = translator
            .wait_for_indexing_ready(&server_id)
            .await
            .unwrap_err();

        assert_matches!(err, Error::WorkspaceIndexing { elapsed_secs, .. } if elapsed_secs == 5);
        assert_eq!(start.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn test_wait_for_indexing_ready_loading_times_out() {
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let server_id = ServerId::from_static("rust");
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

        assert_matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == ServerId::from_static("rust")
        );
    }

    #[tokio::test]
    async fn test_wait_for_indexing_ready_returns_ok_once_signaled_ready() {
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let server_id = ServerId::from_static("rust");
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
        let server_id = ServerId::from_static("rust");
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
            .handle_hover(client_path(path.to_string_lossy().into_owned()), pos(1, 1))
            .await
            .unwrap_err();

        assert_matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, elapsed_secs: 30 } if id == server_id
        );
    }

    /// Companion to the timeout test above: when the cache reports `Ready`,
    /// `handle_hover` must dispatch normally with no added delay.
    #[tokio::test]
    async fn test_handle_hover_dispatches_when_indexing_ready() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
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
            tokio::spawn(async move { translator.handle_hover(client_path(path), pos(1, 1)).await })
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

    /// #518: a hover that times out while awaiting the server's reply must
    /// leave the bridge usable -- the document stays tracked, so the next
    /// hover goes straight to the request without a second `didOpen`.
    #[tokio::test(start_paused = true)]
    async fn test_handle_hover_timeout_keeps_document_open_for_next_request() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
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
        let path = path.to_string_lossy().to_string();

        let spawn_hover = || {
            let translator = Arc::clone(&translator);
            let path = path.clone();
            tokio::spawn(async move { translator.handle_hover(client_path(path), pos(1, 1)).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);

        let first = spawn_hover();
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let first_request = read_framed_message(&mut wire).await;
        assert_eq!(first_request["method"], "textDocument/hover");

        let timeout_secs = crate::config::LspServerConfig::rust_analyzer().request_timeout_seconds;
        tokio::time::advance(Duration::from_secs(timeout_secs.get() + 1)).await;
        assert_matches!(first.await.unwrap().unwrap_err(), Error::Timeout(_));

        let second = spawn_hover();
        let second_request = read_framed_message(&mut wire).await;
        assert_eq!(
            second_request["method"], "textDocument/hover",
            "a timed-out hover must not cause the document to be re-opened"
        );
        write_response(
            &mut server.read_half_stdin,
            &second_request["id"],
            serde_json::json!({"contents": {"kind": "markdown", "value": "after timeout"}}),
        )
        .await;

        assert_eq!(second.await.unwrap().unwrap().contents, "after timeout");
    }

    /// End-to-end: `handle_definition` must surface `Error::WorkspaceIndexing`
    /// while the routed server is still `Loading`, without reaching the fake
    /// LSP server.
    #[tokio::test(start_paused = true)]
    async fn test_handle_definition_returns_workspace_indexing_error_when_loading() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
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
            .handle_definition(
                client_path(path.to_string_lossy().into_owned()),
                pos(1, 1),
                ResultContext::None,
            )
            .await
            .unwrap_err();

        assert_matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        );
    }

    /// Companion: when the cache reports `Ready`, `handle_definition` must
    /// dispatch normally.
    #[tokio::test]
    async fn test_handle_definition_dispatches_when_indexing_ready() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
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
            tokio::spawn(async move {
                translator
                    .handle_definition(client_path(path), pos(1, 1), ResultContext::None)
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
        let server_id = ServerId::from_static("rust");
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
            .handle_references(
                client_path(path.to_string_lossy().into_owned()),
                pos(1, 1),
                true,
                ResultContext::None,
            )
            .await
            .unwrap_err();

        assert_matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        );
    }

    /// Companion: when the cache reports `Ready`, `handle_references` must
    /// dispatch normally.
    #[tokio::test]
    async fn test_handle_references_dispatches_when_indexing_ready() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
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
            tokio::spawn(async move {
                translator
                    .handle_references(client_path(path), pos(1, 1), true, ResultContext::None)
                    .await
            })
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
        let server_id = ServerId::from_static("rust");
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
            .handle_implementation(
                client_path(path.to_string_lossy().into_owned()),
                pos(1, 1),
                ResultContext::None,
            )
            .await
            .unwrap_err();

        assert_matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        );
    }

    /// S3 fix, companion for `handle_type_definition`.
    #[tokio::test(start_paused = true)]
    async fn test_handle_type_definition_returns_workspace_indexing_error_when_loading() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
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
            .handle_type_definition(
                client_path(path.to_string_lossy().into_owned()),
                pos(1, 1),
                ResultContext::None,
            )
            .await
            .unwrap_err();

        assert_matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        );
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
        let server_id = ServerId::from_static("rust");
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
        let server_id = ServerId::from_static("rust");
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
        assert_matches!(
            short_result,
            Err(Error::WorkspaceIndexing { .. }),
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
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            definition_provider: Some(lsp_types::DefinitionProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = canonical_dir(&dir).join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let target_path = canonical_dir(&dir).join("target.rs");
        fs::write(&target_path, "fn target() {}").unwrap();
        let target_uri = Url::from_file_path(&target_path).unwrap().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_definition(client_path(path), Position::at(1, 1), ResultContext::None)
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
    /// `WorkspaceRoots::validate` gate any subsequent open/read of the
    /// path would hit.
    #[tokio::test]
    async fn test_handle_definition_does_not_filter_out_of_workspace_location() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
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
                    .handle_definition(client_path(path), Position::at(1, 1), ResultContext::None)
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
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            references_provider: Some(lsp_types::ReferencesProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = canonical_dir(&dir).join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let inside_path = canonical_dir(&dir).join("inside.rs");
        fs::write(&inside_path, "fn used() {}").unwrap();
        let inside_uri = Url::from_file_path(&inside_path).unwrap().to_string();
        let outside_uri = "file:///outside/workspace/stdlib.rs";

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_references(client_path(path), pos(1, 1), true, ResultContext::None)
                    .await
            })
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

    /// Regression for #474/M4: `get_references`' tool description no longer
    /// promises "all" references, since a response past
    /// `MAX_NORMALIZED_LOCATIONS` is capped -- the client must be able to
    /// detect that via `ReferencesResult::truncated` rather than silently
    /// receiving a partial result that looks complete.
    #[tokio::test]
    async fn test_handle_references_sets_truncated_flag_past_cap() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            references_provider: Some(lsp_types::ReferencesProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let uri = Url::from_file_path(&path).unwrap().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_references(client_path(path), pos(1, 1), true, ResultContext::None)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/references");

        let locations: Vec<serde_json::Value> = (0..MAX_NORMALIZED_LOCATIONS + 500)
            .map(|_| {
                serde_json::json!({
                    "uri": uri,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 4}
                    }
                })
            })
            .collect();
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!(locations),
        )
        .await;

        let result = timeout(Duration::from_secs(5), handle)
            .await
            .expect("handle_references should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(result.locations.len(), MAX_NORMALIZED_LOCATIONS);
        assert!(
            result.truncated,
            "a references response past MAX_NORMALIZED_LOCATIONS must set truncated: true"
        );
    }

    /// Success-path coverage for `handle_implementation` through the
    /// `Definition::LocationList` -> `GotoKind::Definition` arm, pinning the
    /// `GotoResponse` impl for `ImplementationResponse`.
    #[tokio::test]
    async fn test_handle_implementation_flattens_location_list() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
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
                        client_path(path),
                        Position::at(1, 1),
                        ResultContext::None,
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

    /// `handle_declaration` flattens a plain `Declaration::LocationList` and
    /// sends `textDocument/declaration`.
    #[tokio::test]
    async fn test_handle_declaration_flattens_location_list() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            declaration_provider: Some(lsp_types::DeclarationProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let decl_path = dir.path().join("decl.rs");
        fs::write(&decl_path, "fn decl();").unwrap();
        let decl_uri = Url::from_file_path(&decl_path).unwrap().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = client_path(&path);
            tokio::spawn(async move {
                translator
                    .handle_declaration(path, pos(1, 1), ResultContext::None)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/declaration");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "uri": decl_uri,
                "range": {
                    "start": {"line": 0, "character": 3},
                    "end": {"line": 0, "character": 7}
                }
            }]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_declaration should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(result.locations.len(), 1);
        assert_eq!(result.locations[0].uri, decl_uri);
    }

    /// `handle_declaration` maps `DeclarationLink[]` through `targetSelectionRange`.
    #[tokio::test]
    async fn test_handle_declaration_flattens_link_list() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            declaration_provider: Some(lsp_types::DeclarationProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let target_path = dir.path().join("target.h");
        fs::write(&target_path, "struct TargetType;").unwrap();
        let target_uri = Url::from_file_path(&target_path).unwrap().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = client_path(&path);
            tokio::spawn(async move {
                translator
                    .handle_declaration(path, pos(1, 1), ResultContext::None)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let _opened = read_framed_message(&mut wire).await;
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/declaration");

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
            .expect("handle_declaration should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(result.locations.len(), 1);
        assert_eq!(result.locations[0].range.start.character, 8);
    }

    /// An empty declaration answer is an empty result, not an error (FR-003).
    #[tokio::test]
    async fn test_handle_declaration_null_response_is_empty() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            declaration_provider: Some(lsp_types::DeclarationProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = client_path(&path);
            tokio::spawn(async move {
                translator
                    .handle_declaration(path, pos(1, 1), ResultContext::None)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let _opened = read_framed_message(&mut wire).await;
        let request = read_framed_message(&mut wire).await;
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::Value::Null,
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle_declaration should not hang")
            .unwrap()
            .unwrap();
        assert!(result.locations.is_empty());
    }

    /// `handle_declaration` is gated by the indexing readiness check like definition.
    #[tokio::test(start_paused = true)]
    async fn test_handle_declaration_returns_workspace_indexing_error_when_loading() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            declaration_provider: Some(lsp_types::DeclarationProvider::Bool(true)),
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
            .handle_declaration(client_path(&path), pos(1, 1), ResultContext::None)
            .await
            .unwrap_err();

        assert_matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        );
    }

    /// Success-path coverage for `handle_type_definition` through the
    /// `DefinitionLinkList` -> `GotoKind::DefinitionLinkList` arm, pinning
    /// the `GotoResponse` impl for `TypeDefinitionResponse` and the
    /// `definition_link_to_location` mapping (`target_selection_range`, not
    /// `target_range`).
    #[tokio::test]
    async fn test_handle_type_definition_flattens_definition_link_list() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
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
                        client_path(path),
                        Position::at(1, 1),
                        ResultContext::None,
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

    // -----------------------------------------------------------------
    // Resource-amplification defenses (#474)
    // -----------------------------------------------------------------

    /// Regression for #474's exact attack scenario: many `Location`s
    /// clustered onto a handful of distinct `(file, line)` pairs must cost
    /// one disk read per distinct pair, not one per location. Proven through
    /// the real `lsp_locations_to_mcp` entry point shared by `handle_goto`
    /// and `handle_references` -- not by calling the cache-backed helper
    /// directly -- and via the cache's own size, which is a direct count of
    /// how many times the disk-read fallback actually ran.
    #[tokio::test]
    async fn test_lsp_locations_to_mcp_reads_disk_once_per_distinct_file_line() {
        let dir = TempDir::new().unwrap();
        let mut uris = Vec::new();
        for i in 0..3 {
            let path = dir.path().join(format!("file{i}.rs"));
            fs::write(&path, "hello").unwrap();
            uris.push(path_to_uri(&path).unwrap());
        }

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        // 300 locations, but only 3 distinct (file, line) pairs.
        let locs: Vec<lsp_types::Location> = (0..300)
            .map(|i| lsp_types::Location {
                uri: uris[i % 3].clone(),
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
            })
            .collect();

        let result = lsp_locations_to_mcp(locs, &ctx).await;

        assert_eq!(result.locations.len(), 300);
        assert!(!result.truncated);
        assert!(
            result.locations.iter().all(|l| l.range.end.character == 4),
            "MCP columns are 1-based, so LSP byte offset 3 in all-ASCII \"hello\" must convert \
             to 4"
        );
        assert_eq!(
            lock_std(&ctx.line_cache).entries.len(),
            3,
            "300 locations across 3 distinct files must populate the cache with exactly 3 \
             entries (one disk read per distinct file/line), not one per location"
        );
    }

    /// Regression for #474: without a cap, a response naming an unbounded
    /// number of locations would drive an unbounded number of range
    /// conversions. `Utf16` needs no disk read at all (see `test_ctx`), so
    /// this isolates the truncation itself from I/O cost -- a response well
    /// past `MAX_NORMALIZED_LOCATIONS` must be truncated to it, not hang,
    /// OOM, or panic.
    #[tokio::test]
    async fn test_lsp_locations_to_mcp_truncates_to_max_normalized_locations() {
        let ctx = test_ctx();
        let uri = test_uri();
        let locs: Vec<lsp_types::Location> = (0..MAX_NORMALIZED_LOCATIONS + 500)
            .map(|_| lsp_types::Location {
                uri: uri.clone(),
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
            })
            .collect();

        let result = lsp_locations_to_mcp(locs, &ctx).await;

        assert_eq!(result.locations.len(), MAX_NORMALIZED_LOCATIONS);
        assert!(
            result.truncated,
            "a response naming more than MAX_NORMALIZED_LOCATIONS must report truncated: true"
        );
    }

    #[test]
    fn test_item_budget_admit_spends_shared_allowance() {
        let mut budget = ItemBudget::new();
        let first = budget.admit(vec![0u8; MAX_NORMALIZED_LOCATIONS - 1]);
        assert_eq!(first.len(), MAX_NORMALIZED_LOCATIONS - 1);
        assert!(!budget.truncated());

        let second = budget.admit(vec![0u8; 5]);
        assert_eq!(second.len(), 1);
        assert!(budget.truncated());

        assert_eq!(budget.admit(vec![0u8; 3]).len(), 0);
    }

    #[test]
    fn test_item_budget_admit_whole_never_splits_a_list() {
        let mut budget = ItemBudget::new();
        assert!(
            budget
                .admit_whole(vec![0u8; MAX_NORMALIZED_LOCATIONS - 2])
                .is_some()
        );
        assert!(budget.admit_whole(vec![0u8; 3]).is_none());
        assert!(budget.truncated());
        assert!(budget.admit_whole(vec![0u8; 2]).is_some());
    }

    #[test]
    fn test_item_budget_with_cap_admits_a_lazy_sequence_up_to_the_cap() {
        let mut budget = ItemBudget::with_cap(3);
        assert_eq!(budget.admit_iter(0..3), [0, 1, 2]);
        assert!(!budget.truncated());

        let mut budget = ItemBudget::with_cap(3);
        assert_eq!(budget.admit_iter(0..), [0, 1, 2]);
        assert!(budget.truncated());
        assert!(budget.admit_iter(0..2).is_empty());
    }

    #[test]
    fn test_item_budget_exactly_at_cap_is_not_truncated() {
        let mut budget = ItemBudget::new();
        let items = budget.admit(vec![0u8; MAX_NORMALIZED_LOCATIONS]);
        assert_eq!(items.len(), MAX_NORMALIZED_LOCATIONS);
        assert!(!budget.truncated());
    }
}
