//! Document symbols and workspace symbol search handlers.

use std::collections::HashSet;
use std::sync::Arc;

use lsp_types::{
    BaseSymbolInformation, DocumentSymbol, PartialResultParams, WorkDoneProgressParams,
    WorkspaceSymbolParams as LspWorkspaceSymbolParams,
};

use super::Translator;
use super::dto::{
    DocumentSymbolsResult, Location, ServerOutcome, ServerStatus, Symbol, WorkspaceSymbol,
    WorkspaceSymbolResult, lsp_kind_to_u32,
};
use super::encoding_ctx::EncodingCtx;
use super::kind_filter::SymbolKindFilter;
use super::navigation::MAX_NORMALIZED_LOCATIONS;
use super::positioned::{budgeted, document_request};
use super::routing::{
    Capability, FanOutMembers, FileLocalCapability, IndexingGate, MemberPresence, PreparedDocument,
    WorkspaceRouteLookup, is_untrusted_refusal, lookup_workspace_route, workspace_fan_out_members,
};
use crate::bridge::{ClientPath, Indexed};
use crate::config::{ServerId, ToolKind};
use crate::error::{Error, Result, ServerSpawnFailure};
use crate::util::lock_std;

/// Validate `query`'s length for `handle_workspace_symbol`.
fn validate_query_length(query: &str) -> Result<()> {
    const MAX_QUERY_LENGTH: usize = 1000;

    if query.len() > MAX_QUERY_LENGTH {
        return Err(Error::InvalidToolParams(format!(
            "Query too long: {} bytes (max {MAX_QUERY_LENGTH})",
            query.len()
        )));
    }

    Ok(())
}

/// The `workspace/symbol` request for `query`.
fn workspace_symbol_params(query: String) -> LspWorkspaceSymbolParams {
    LspWorkspaceSymbolParams {
        query,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

/// `limit` clamped to `MAX_NORMALIZED_LOCATIONS`, else `u32::MAX` would reopen
/// the unbounded normalization loop (#474).
fn effective_limit(limit: u32) -> usize {
    (limit as usize).min(MAX_NORMALIZED_LOCATIONS)
}

/// A `textDocument/documentSymbol` answer together with what is needed to
/// interpret it: the prepared document (kept alive so the document stays
/// tracked) and the encoding context of the routed server.
pub(super) struct FetchedSymbols {
    pub(super) doc: PreparedDocument,
    pub(super) ctx: EncodingCtx,
    pub(super) response: Option<lsp_types::DocumentSymbolResponse>,
}

/// Convert LSP document symbol to MCP symbol. `uri` is the queried
/// document's own URI: nested `DocumentSymbol` entries have no URI of their
/// own, since `textDocument/documentSymbol` is always scoped to one file.
///
/// Boxed because it recurses through `children` and an `async fn` cannot
/// call itself directly (its future would have unbounded size).
fn convert_document_symbol<'a>(
    symbol: DocumentSymbol,
    ctx: &'a EncodingCtx,
    uri: &'a lsp_types::Uri,
) -> futures::future::BoxFuture<'a, Symbol> {
    Box::pin(async move {
        let range = ctx.normalize_range(uri, symbol.range).await;
        let selection_range = ctx.normalize_range(uri, symbol.selection_range).await;
        let children = match symbol.children {
            Some(children) => {
                let mut result = Vec::with_capacity(children.len());
                for child in children {
                    result.push(convert_document_symbol(child, ctx, uri).await);
                }
                Some(result)
            }
            None => None,
        };

        Symbol {
            name: symbol.name,
            kind: lsp_kind_to_u32(symbol.kind),
            range,
            selection_range,
            children,
        }
    })
}

/// A `textDocument/documentSymbol` answer normalized into MCP coordinates.
#[derive(Debug)]
pub(super) enum DocumentSymbolTree {
    /// Hierarchical `DocumentSymbol[]`: ancestry is the `children` nesting.
    Hierarchical(Vec<Symbol>),
    /// Legacy flat `SymbolInformation[]`: ancestry is at most `containerName`.
    Flat(Vec<FlatSymbol>),
}

impl DocumentSymbolTree {
    /// The symbols as `get_document_symbols` reports them.
    fn into_symbols(self) -> Vec<Symbol> {
        match self {
            Self::Hierarchical(symbols) => symbols,
            Self::Flat(symbols) => symbols.into_iter().map(|flat| flat.symbol).collect(),
        }
    }
}

/// One entry of a flat `SymbolInformation[]` answer.
#[derive(Debug)]
pub(super) struct FlatSymbol {
    pub(super) symbol: Symbol,
    pub(super) container_name: Option<String>,
}

/// Normalizes a `textDocument/documentSymbol` answer into MCP coordinates,
/// keeping the flat `SymbolInformation` shape distinct so callers can tell
/// how much ancestry the server reported.
///
/// `uri` is the queried document: a flat entry carries its own
/// `location.uri`, which is not trusted (it feeds `normalize_range` into a
/// file read), so every entry is normalized against `uri`, the already
/// resolved document the request was made for.
pub(super) async fn symbol_tree(
    response: Option<lsp_types::DocumentSymbolResponse>,
    ctx: &EncodingCtx,
    uri: &lsp_types::Uri,
) -> DocumentSymbolTree {
    match response {
        Some(lsp_types::DocumentSymbolResponse::SymbolInformationList(symbols)) => {
            let mut result = Vec::with_capacity(symbols.len());
            for sym in symbols {
                let range = ctx.normalize_range(uri, sym.location.range).await;
                let selection_range = range.clone();
                result.push(FlatSymbol {
                    symbol: Symbol {
                        name: sym.base_symbol_information.name,
                        kind: lsp_kind_to_u32(sym.base_symbol_information.kind),
                        range,
                        selection_range,
                        children: None,
                    },
                    container_name: sym.base_symbol_information.container_name,
                });
            }
            DocumentSymbolTree::Flat(result)
        }
        Some(lsp_types::DocumentSymbolResponse::DocumentSymbolList(symbols)) => {
            let mut result = Vec::with_capacity(symbols.len());
            for sym in symbols {
                result.push(convert_document_symbol(sym, ctx, uri).await);
            }
            DocumentSymbolTree::Hierarchical(result)
        }
        None => DocumentSymbolTree::Hierarchical(vec![]),
    }
}

impl Translator {
    /// Handle document symbols request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `documentSymbolProvider` support.
    pub async fn handle_document_symbols(
        &self,
        file_path: ClientPath,
    ) -> Result<DocumentSymbolsResult> {
        let FetchedSymbols { doc, ctx, response } =
            self.request_document_symbols(&file_path).await?;
        let tree = symbol_tree(response, &ctx, doc.uri()).await;

        Ok(DocumentSymbolsResult {
            symbols: tree.into_symbols(),
            positions_degraded: ctx.positions_degraded(),
        })
    }

    /// Request `textDocument/documentSymbol` for `file_path`, gated on the
    /// routed server's `documentSymbolProvider` but not on indexing (the
    /// answer is file-local).
    ///
    /// # Errors
    ///
    /// Routing, capability and document errors, or the LSP request's error.
    pub(super) async fn request_document_symbols(
        &self,
        file_path: &ClientPath,
    ) -> Result<FetchedSymbols> {
        let doc = self
            .prepare_gated_document(
                file_path,
                IndexingGate::FileLocal(FileLocalCapability::DocumentSymbols),
            )
            .await?;
        self.fetch_document_symbols(doc).await
    }

    /// Requests `textDocument/documentSymbol` for an already prepared
    /// document.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails.
    pub(super) async fn fetch_document_symbols(
        &self,
        doc: PreparedDocument,
    ) -> Result<FetchedSymbols> {
        let ctx = self.encoding_ctx(doc.server_id());
        let response = document_request::<lsp_types::DocumentSymbolRequest>(&doc, ()).await?;
        Ok(FetchedSymbols { doc, ctx, response })
    }

    /// Handle workspace symbol search.
    ///
    /// With two or more eligible servers (the explicit claimers and catch-alls
    /// of every language, minus any refused by untrusted-workspace mode)
    /// the query goes to every live capable one concurrently, each bounded by
    /// its own request deadline, and the answers are merged in config order
    /// with a producing-server tag and a per-server outcome list. A dead
    /// server is reported, never respawned. With fewer, the single-server
    /// path below applies unchanged.
    ///
    /// Deliberately not gated on indexing readiness, unlike other
    /// whole-workspace queries (e.g. `references`, call hierarchy
    /// incoming/outgoing calls): it resolves via `resolve_any` rather than a
    /// per-file route, so it never goes through `prepare_gated_document`
    /// (the only chokepoint `IndexingGate` applies to) at all. Whether/how to
    /// gate it was deferred as a separate open question (spec FR-008) and
    /// remains a known limitation (#423).
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, no server is configured, or
    /// the routed server does not advertise `workspaceSymbolProvider` support.
    pub async fn handle_workspace_symbol(
        &self,
        query: String,
        kind_filter: Option<SymbolKindFilter>,
        limit: u32,
    ) -> Result<WorkspaceSymbolResult> {
        validate_query_length(&query)?;

        if let Some(members) = self.fan_out_members() {
            return self
                .workspace_symbol_fan_out(query, kind_filter, limit, members)
                .await;
        }

        let server_id = self.workspace_symbol_server()?;
        self.respawn_if_dead(&server_id).await?;
        let (client, expected) = {
            let servers = lock_std(&self.servers);
            (servers.client(&server_id), servers.is_expected(&server_id))
        };
        let client = client.ok_or_else(|| {
            if expected {
                Error::ServerInitializing {
                    server_id: server_id.clone(),
                }
            } else {
                Error::NoServerConfigured
            }
        })?;
        self.require_capability(&server_id, Capability::WorkspaceSymbols)?;

        let response =
            budgeted::<lsp_types::WorkspaceSymbolRequest>(&client, workspace_symbol_params(query))
                .await?;

        let ctx = self.encoding_ctx(&server_id);

        // Collected without normalizing each symbol's range yet, so
        // `kind_filter`/`limit` below can drop entries before paying for
        // `EncodingCtx::normalize_range` (a disk read on a cache miss) on
        // each one -- bounds the *normalization* loop's cost to `limit`;
        // this collection loop itself still scans the full response (#474).
        let mut raw_symbols = raw_workspace_symbols(response, &ctx);
        let server_answered_empty = raw_symbols.is_empty();

        raw_symbols.retain(|symbol| symbol.kind_matches(kind_filter));
        let effective_limit = effective_limit(limit);
        let truncated = raw_symbols.len() > effective_limit;
        raw_symbols.truncate(effective_limit);

        let mut symbols = Vec::with_capacity(raw_symbols.len());
        for raw in raw_symbols {
            symbols.push(raw.normalize(&ctx, None).await);
        }

        let index_coverage = server_answered_empty.then(|| self.index_coverage(&server_id));
        Ok(WorkspaceSymbolResult {
            symbols,
            truncated,
            positions_degraded: ctx.positions_degraded(),
            index_coverage,
            servers: Vec::new(),
        })
    }

    /// The servers a workspace symbol search fans out over, or `None` when
    /// fewer than two are eligible. One `servers` guard serves every closure.
    fn fan_out_members(&self) -> Option<FanOutMembers> {
        let servers = lock_std(&self.servers);
        workspace_fan_out_members(
            &self.configured_router,
            ToolKind::WorkspaceSymbols,
            |id| servers.failure(id).is_some_and(is_untrusted_refusal),
            |id| servers.client(id).is_some(),
            |id| servers.is_expected(id),
        )
    }

    /// Fans a workspace symbol search out over `members`.
    ///
    /// Every branch is bounded by its own request deadline, so the call takes
    /// as long as the slowest server, not the sum. A server that is not live,
    /// not capable or failed is reported in the outcome list while the others'
    /// symbols are kept; only when no server answered does the call fail.
    async fn workspace_symbol_fan_out(
        &self,
        query: String,
        kind_filter: Option<SymbolKindFilter>,
        limit: u32,
        members: FanOutMembers,
    ) -> Result<WorkspaceSymbolResult> {
        let params = workspace_symbol_params(query);
        let ask = |(id, presence): (ServerId, MemberPresence)| {
            let params = params.clone();
            async move {
                let answer = self.ask_workspace_member(&id, presence, params).await;
                (id, answer)
            }
        };
        let (first, rest) = members.into_parts();
        // Bounded by the configured server count, as `members` is.
        let (first, rest) = futures::join!(
            ask(first),
            futures::future::join_all(rest.into_iter().map(&ask))
        );

        // One extra unique symbol is enough to know the result was cut.
        let effective_limit = effective_limit(limit);
        let mut merged = MergedSymbols::new(effective_limit.saturating_add(1));
        let mut outcomes = Vec::with_capacity(rest.len().saturating_add(1));
        let mut record = |(id, answer): (ServerId, Answer)| match answer {
            Ok(Indexed {
                result: response,
                indexing,
            }) => {
                let producer = Arc::new(Producer {
                    ctx: self.encoding_ctx(&id),
                    id: id.clone(),
                });
                let raw = raw_workspace_symbols(response, &producer.ctx);
                let returned = raw.len();
                merged.extend(
                    &producer,
                    raw.into_iter()
                        .filter(|symbol| symbol.kind_matches(kind_filter)),
                );
                outcomes.push(ServerOutcome {
                    server: id.clone(),
                    status: ServerStatus::Answered {
                        symbols: u32::try_from(returned).unwrap_or(u32::MAX),
                        index_coverage: (returned == 0).then(|| self.index_coverage(&id)),
                        indexing_in_progress: indexing.indexing_in_progress,
                    },
                });
                Recorded::Producer(producer)
            }
            Err(failure) => {
                let status = failure.status();
                tracing::debug!(
                    server = %id,
                    outcome = ?status,
                    error = %failure,
                    "workspace symbol search: server did not answer"
                );
                outcomes.push(ServerOutcome { server: id, status });
                Recorded::Failure(failure)
            }
        };
        let mut tally = Tally::start(record(first));
        for answer in rest {
            tally = tally.absorb(record(answer));
        }

        let producers = match tally {
            Tally::NoneAnswered(failures) => return Err(aggregate_failure(failures)),
            Tally::Answered(producers) => producers,
        };

        let (accepted, truncated) = merged.finish(effective_limit);
        let mut symbols = Vec::with_capacity(accepted.len());
        for (producer, raw) in accepted {
            symbols.push(
                raw.normalize(&producer.ctx, Some(producer.id.clone()))
                    .await,
            );
        }

        Ok(WorkspaceSymbolResult {
            symbols,
            truncated,
            positions_degraded: producers
                .iter()
                .filter_map(|producer| producer.ctx.positions_degraded())
                .max(),
            index_coverage: None,
            servers: outcomes,
        })
    }

    /// Asks one fan-out member for its workspace symbols, never respawning it.
    ///
    /// The indexing state is sampled around the request, so an answer that
    /// overlapped indexing is flagged.
    async fn ask_workspace_member(
        &self,
        id: &ServerId,
        presence: MemberPresence,
        params: LspWorkspaceSymbolParams,
    ) -> Answer {
        match presence {
            MemberPresence::Registered => {}
            MemberPresence::Initializing => {
                return Err(MemberFailure::Initializing {
                    server_id: id.clone(),
                });
            }
            MemberPresence::Absent => {
                return Err(self.startup_failure(id).map_or(
                    MemberFailure::Unavailable(Error::NoServerConfigured),
                    |failure| MemberFailure::FailedToStart(Box::new(failure)),
                ));
            }
        }
        if self.dead_server_config(id).is_some() {
            return Err(MemberFailure::Unavailable(Error::ServerTerminated));
        }
        let client = lock_std(&self.servers).client(id);
        let Some(client) = client else {
            return Err(MemberFailure::Unavailable(Error::NoServerConfigured));
        };
        if let Err(error) = self.require_capability(id, Capability::WorkspaceSymbols) {
            return Err(MemberFailure::CapabilityNotAdvertised(error));
        }
        let before = self.sample_indexing(id).await;
        let response = budgeted::<lsp_types::WorkspaceSymbolRequest>(&client, params)
            .await
            .map_err(MemberFailure::from_request_error)?;
        let after = self.sample_indexing(id).await;
        Ok(Indexed::new(response, before.union(after)))
    }

    /// The server a workspace symbol search goes to.
    ///
    /// Workspace search has no document, so it resolves via `resolve_any`
    /// rather than a per-language route. If the resolved server is not
    /// registered yet but is expected, tell the caller to wait and retry
    /// rather than implying nothing is configured.
    fn workspace_symbol_server(&self) -> Result<ServerId> {
        let router = self.router_snapshot();
        let lookup = {
            let servers = lock_std(&self.servers);
            lookup_workspace_route(
                || router.resolve_any(ToolKind::WorkspaceSymbols).cloned(),
                |id| servers.client(id).is_some(),
                |id| servers.is_expected(id),
                || !servers.any_expected(),
            )
        };
        let server_id = match lookup {
            WorkspaceRouteLookup::Registered(id) => id,
            WorkspaceRouteLookup::Initializing(server_id) => {
                return Err(Error::ServerInitializing { server_id });
            }
            WorkspaceRouteLookup::AllInitializing => {
                return Err(Error::WorkspaceServersInitializing);
            }
            WorkspaceRouteLookup::Dangling(_) => return Err(Error::NoServerConfigured),
            WorkspaceRouteLookup::NothingConfigured => {
                let failures = self.startup_failures();
                return Err(if failures.is_empty() {
                    Error::NoServerConfigured
                } else {
                    Error::AllServersFailedToInit { failures }
                });
            }
            // A claimant that failed to start was rebound away, so the live
            // router sees no claimant for it -- report the failure instead.
            WorkspaceRouteLookup::NoClaimant => {
                return Err(self
                    .configured_router
                    .resolve_any(ToolKind::WorkspaceSymbols)
                    .ok()
                    .and_then(|id| self.startup_failure(id))
                    .map_or(
                        Error::NoServerForWorkspaceTool {
                            tool: ToolKind::WorkspaceSymbols,
                        },
                        |failure| Error::ServerFailedToStart(Box::new(failure)),
                    ));
            }
        };
        Ok(server_id)
    }
}

/// The answer of one fan-out member.
type Answer =
    std::result::Result<Indexed<Option<lsp_types::WorkspaceSymbolResponse>>, MemberFailure>;

/// Why a fan-out member contributed no answer, with the error the
/// single-server path would have returned for it.
#[derive(Debug)]
enum MemberFailure {
    Initializing { server_id: ServerId },
    FailedToStart(Box<ServerSpawnFailure>),
    Unavailable(Error),
    TimedOut(Error),
    Failed(Error),
    CapabilityNotAdvertised(Error),
}

impl MemberFailure {
    /// A process that died mid-request is unavailable, not failed, so the
    /// outcome does not depend on how soon its command channel closed.
    fn from_request_error(error: Error) -> Self {
        match error {
            Error::Timeout(_) => Self::TimedOut(error),
            Error::ServerTerminated => Self::Unavailable(error),
            Error::ServerFailedToStart(failure) => Self::FailedToStart(failure),
            _ => Self::Failed(error),
        }
    }

    const fn status(&self) -> ServerStatus {
        match self {
            Self::Initializing { .. } => ServerStatus::Initializing,
            Self::FailedToStart(_) | Self::Unavailable(_) => ServerStatus::Unavailable,
            Self::TimedOut(_) => ServerStatus::TimedOut,
            Self::Failed(_) => ServerStatus::Failed,
            Self::CapabilityNotAdvertised(_) => ServerStatus::CapabilityNotAdvertised,
        }
    }

    /// Whether the member broke rather than being merely early or incapable.
    const fn is_breakage(&self) -> bool {
        matches!(
            self,
            Self::FailedToStart(_) | Self::Unavailable(_) | Self::TimedOut(_) | Self::Failed(_)
        )
    }

    const fn is_initializing(&self) -> bool {
        matches!(self, Self::Initializing { .. })
    }

    fn spawn_failure(&self) -> Option<&ServerSpawnFailure> {
        match self {
            Self::FailedToStart(failure) => Some(failure),
            _ => None,
        }
    }

    fn into_error(self) -> Error {
        match self {
            Self::Initializing { server_id } => Error::ServerInitializing { server_id },
            Self::FailedToStart(failure) => Error::ServerFailedToStart(failure),
            Self::Unavailable(error)
            | Self::TimedOut(error)
            | Self::Failed(error)
            | Self::CapabilityNotAdvertised(error) => error,
        }
    }
}

impl std::fmt::Display for MemberFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Initializing { server_id } => write!(f, "server {server_id} is initializing"),
            Self::FailedToStart(failure) => write!(f, "{}", failure.failed_to_start()),
            Self::Unavailable(error)
            | Self::TimedOut(error)
            | Self::Failed(error)
            | Self::CapabilityNotAdvertised(error) => write!(f, "{error}"),
        }
    }
}

/// The members of a fan-out that did not answer: at least one, in config order.
struct MemberFailures {
    first: MemberFailure,
    rest: Vec<MemberFailure>,
}

impl MemberFailures {
    fn iter(&self) -> impl Iterator<Item = &MemberFailure> {
        std::iter::once(&self.first).chain(&self.rest)
    }

    /// The first member satisfying `wanted`, else the first member.
    fn into_first_or_head(self, wanted: impl Fn(&MemberFailure) -> bool) -> MemberFailure {
        let Self { first, rest } = self;
        if wanted(&first) {
            return first;
        }
        rest.into_iter()
            .find(|failure| wanted(failure))
            .unwrap_or(first)
    }
}

/// What a member's answer turned into once recorded.
enum Recorded {
    Producer(Arc<Producer>),
    Failure(MemberFailure),
}

/// How the fan-out stands: nobody has answered yet, or somebody has.
enum Tally {
    NoneAnswered(MemberFailures),
    Answered(Vec<Arc<Producer>>),
}

impl Tally {
    fn start(recorded: Recorded) -> Self {
        match recorded {
            Recorded::Producer(producer) => Self::Answered(vec![producer]),
            Recorded::Failure(first) => Self::NoneAnswered(MemberFailures {
                first,
                rest: Vec::new(),
            }),
        }
    }

    fn absorb(self, recorded: Recorded) -> Self {
        match (self, recorded) {
            (Self::NoneAnswered(_), Recorded::Producer(producer)) => Self::Answered(vec![producer]),
            (Self::NoneAnswered(mut failures), Recorded::Failure(failure)) => {
                failures.rest.push(failure);
                Self::NoneAnswered(failures)
            }
            (Self::Answered(mut producers), Recorded::Producer(producer)) => {
                producers.push(producer);
                Self::Answered(producers)
            }
            (Self::Answered(producers), Recorded::Failure(_)) => Self::Answered(producers),
        }
    }
}

/// The error for a fan-out in which no member answered.
///
/// A member still initializing wins, because that is the retryable answer:
/// all of them give the aggregate, a mix gives the first one's error. Failing
/// that, members that all failed to start give the all-failed aggregate (#527),
/// then the first breakage stands, else the first member's own error.
fn aggregate_failure(failures: MemberFailures) -> Error {
    if failures.iter().any(MemberFailure::is_initializing) {
        return if failures.iter().all(MemberFailure::is_initializing) {
            Error::WorkspaceServersInitializing
        } else {
            failures
                .into_first_or_head(MemberFailure::is_initializing)
                .into_error()
        };
    }
    let spawn_failures: Option<Vec<ServerSpawnFailure>> = failures
        .iter()
        .map(|failure| failure.spawn_failure().cloned())
        .collect();
    spawn_failures.map_or_else(
        || {
            failures
                .into_first_or_head(MemberFailure::is_breakage)
                .into_error()
        },
        |failures| Error::AllServersFailedToInit { failures },
    )
}

/// A server that answered a fan-out, with the context its symbols are
/// normalized in.
struct Producer {
    id: ServerId,
    ctx: EncodingCtx,
}

/// A symbol's identity for duplicate detection: everything the server
/// reported, with the range as the raw LSP coordinates (the same symbol
/// reported in two encodings is deliberately kept twice).
#[derive(PartialEq, Eq, Hash)]
struct SymbolIdentity {
    name: String,
    kind: lsp_types::SymbolKind,
    container_name: Option<String>,
    uri: lsp_types::Uri,
    range: lsp_types::Range,
}

/// The unique symbols of a fan-out, concatenated in config order, collection
/// stopping at `cap`.
struct MergedSymbols {
    cap: usize,
    seen: HashSet<SymbolIdentity>,
    accepted: Vec<(Arc<Producer>, RawWorkspaceSymbol)>,
}

impl MergedSymbols {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            seen: HashSet::new(),
            accepted: Vec::new(),
        }
    }

    /// Adds `symbols` of `producer`, dropping any already accepted from this
    /// server or an earlier one before it counts towards the cap.
    fn extend(
        &mut self,
        producer: &Arc<Producer>,
        symbols: impl IntoIterator<Item = RawWorkspaceSymbol>,
    ) {
        for symbol in symbols {
            if self.accepted.len() >= self.cap {
                return;
            }
            if self.seen.insert(symbol.identity()) {
                self.accepted.push((Arc::clone(producer), symbol));
            }
        }
    }

    /// The first `limit` symbols, and whether more were collected.
    fn finish(mut self, limit: usize) -> (Vec<(Arc<Producer>, RawWorkspaceSymbol)>, bool) {
        let truncated = self.accepted.len() > limit;
        self.accepted.truncate(limit);
        (self.accepted, truncated)
    }
}

/// A workspace symbol not yet normalized into MCP coordinates -- lets
/// [`Translator::handle_workspace_symbol`] apply `kind_filter`/`limit` before
/// paying for [`EncodingCtx::normalize_range`] on each surviving entry (see
/// #474).
struct RawWorkspaceSymbol {
    name: String,
    kind: lsp_types::SymbolKind,
    container_name: Option<String>,
    out_of_workspace: bool,
    uri: lsp_types::Uri,
    range: lsp_types::Range,
}

impl RawWorkspaceSymbol {
    fn identity(&self) -> SymbolIdentity {
        SymbolIdentity {
            name: self.name.clone(),
            kind: self.kind,
            container_name: self.container_name.clone(),
            uri: self.uri.clone(),
            range: self.range,
        }
    }

    fn kind_matches(&self, filter: Option<SymbolKindFilter>) -> bool {
        filter.is_none_or(|filter| filter.kind() == self.kind)
    }

    /// Converts into the MCP symbol, tagged with `server` when several
    /// servers were asked.
    async fn normalize(self, ctx: &EncodingCtx, server: Option<ServerId>) -> WorkspaceSymbol {
        let range = ctx.normalize_range(&self.uri, self.range).await;
        WorkspaceSymbol {
            name: self.name,
            kind: lsp_kind_to_u32(self.kind),
            location: Location {
                uri: self.uri.to_string(),
                range,
                out_of_workspace: self.out_of_workspace,
            },
            container_name: self.container_name,
            server,
        }
    }

    fn new(
        base: BaseSymbolInformation,
        uri: lsp_types::Uri,
        range: lsp_types::Range,
        ctx: &EncodingCtx,
    ) -> Self {
        Self {
            name: base.name,
            kind: base.kind,
            container_name: base.container_name,
            out_of_workspace: ctx.is_out_of_workspace(&uri),
            uri,
            range,
        }
    }
}

/// The symbols of a workspace symbol response, in server order.
///
/// Not filtered to workspace roots -- like other read-only navigation results
/// (see `WorkspaceRoots::admits_uri`'s doc comment), a legitimate
/// workspace-symbol result routinely points outside the workspace (stdlib, a
/// dependency), and any subsequent open/read of it still hits the inbound
/// `WorkspaceRoots::validate` gate.
fn raw_workspace_symbols(
    response: Option<lsp_types::WorkspaceSymbolResponse>,
    ctx: &EncodingCtx,
) -> Vec<RawWorkspaceSymbol> {
    match response {
        Some(lsp_types::WorkspaceSymbolResponse::SymbolInformationList(list)) => list
            .into_iter()
            .map(|sym| {
                RawWorkspaceSymbol::new(
                    sym.base_symbol_information,
                    sym.location.uri,
                    sym.location.range,
                    ctx,
                )
            })
            .collect(),
        Some(lsp_types::WorkspaceSymbolResponse::WorkspaceSymbolList(list)) => list
            .into_iter()
            .filter_map(|sym| match sym.location {
                lsp_types::WorkspaceSymbolLocation::Location(loc) => Some(RawWorkspaceSymbol::new(
                    sym.base_symbol_information,
                    loc.uri,
                    loc.range,
                    ctx,
                )),
                // `LocationUriOnly` carries no range -- the server deliberately
                // withheld it (e.g. to avoid computing it eagerly for every
                // workspace-search result). The MCP `Location` DTO has no way
                // to represent "no range", and a fabricated range (e.g. line 1)
                // would be indistinguishable from a real symbol there, so the
                // symbol is dropped rather than inventing coordinates.
                lsp_types::WorkspaceSymbolLocation::LocationUriOnly(_) => None,
            })
            .collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;
    use std::{assert_matches, fs};

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::time::timeout;
    use url::Url;

    use super::*;
    use crate::bridge::translator::dto::{IndexCoverage, PositionDegradation};
    use crate::bridge::translator::testing::*;
    use crate::config::{LanguageId, ServerCommand, ServerId, TimeoutSecs, ToolRouter, ToolSet};
    use crate::test_lsp::client_path;

    /// #355/#467 regression: `SymbolKindFilter`'s name-matching branch
    /// accepts/rejects `kind_filter` values based on `SymbolKind`'s derived
    /// `Debug` output, since `gen-lsp-types` provides no `as_str()`/`Display`.
    /// This pins that assumption directly so a future `gen-lsp-types` bump
    /// that changes the `Debug` rendering (e.g. back to a newtype) fails
    /// loudly here instead of silently diverging from the input-filter
    /// matching logic.
    #[test]
    fn test_symbol_kind_debug_rendering_is_pinned() {
        assert_eq!(
            format!("{:?}", lsp_types::SymbolKind::EnumMember),
            "EnumMember"
        );
    }

    /// #467 regression: the output `kind` field is the raw LSP wire-format
    /// `u32`, not the `SymbolKind`'s `Debug` string -- pins the numeric
    /// behavior that replaced the old, lossy `format!("{:?}", kind)`
    /// rendering.
    #[tokio::test]
    async fn test_convert_document_symbol_kind_is_numeric() {
        let symbol = DocumentSymbol {
            name: "my_enum_member".to_string(),
            detail: None,
            kind: lsp_types::SymbolKind::EnumMember,
            tags: None,
            #[expect(
                deprecated,
                reason = "`deprecated` is a deprecated field of the LSP struct literal"
            )]
            deprecated: None,
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
            children: None,
        };
        let ctx = test_ctx();
        let uri = lsp_types::Uri::from("file:///tmp/test.rs");
        let result = convert_document_symbol(symbol, &ctx, &uri).await;
        // SymbolKind::EnumMember is LSP integer 22.
        assert_eq!(result.kind, 22u32);
    }

    #[tokio::test]
    async fn test_handle_workspace_symbol_no_server() {
        let translator = Translator::new();
        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;
        assert_matches!(result, Err(Error::NoServerConfigured));
    }

    /// #242/S4 regression: a server is configured and still spawning (large
    /// project load) rather than never having existed -- the router alone
    /// cannot tell these apart (both look like "nothing registered"), so
    /// `handle_workspace_symbol` must consult the expected servers to report
    /// "still initializing" instead of the misleading "no server configured".
    #[tokio::test]
    async fn test_handle_workspace_symbol_reports_initializing_when_expected_but_not_registered() {
        let translator = Translator::new();
        translator.set_expected_servers(HashSet::from([ServerId::from_static("pyright")]));

        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;
        assert_matches!(result, Err(Error::WorkspaceServersInitializing));
    }

    /// #527: workspace search with every server failed to start reports the
    /// failures instead of "no server configured".
    #[tokio::test]
    async fn test_handle_workspace_symbol_reports_startup_failures() {
        let translator = Translator::new();
        translator.record_startup_failures(&[crate::error::ServerSpawnFailure {
            server_id: ServerId::from_static("pyright"),
            language_id: LanguageId::from_static("python"),
            command: ServerCommand::from_static("pyright-langserver"),
            reason: crate::error::StartupFailure::InitTaskPanicked,
        }]);

        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;

        let Err(Error::AllServersFailedToInit { failures }) = result else {
            panic!("expected AllServersFailedToInit, got {result:?}");
        };
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].server_id, ServerId::from_static("pyright"));
    }

    /// A failed server that would have claimed workspace search is reported
    /// instead of "no server handles this tool" while another server lives.
    #[tokio::test]
    async fn test_handle_workspace_symbol_reports_failed_claimant() {
        let mut live = crate::config::LspServerConfig::pyright();
        live.name = Some(ServerId::from_static("live"));
        live.handles = Some(ToolSet::new(vec![ToolKind::Hover]).unwrap());
        let mut failing = crate::config::LspServerConfig::rust_analyzer();
        failing.name = Some(ServerId::from_static("failing"));
        failing.handles = Some(ToolSet::new(vec![ToolKind::WorkspaceSymbols]).unwrap());
        let router = ToolRouter::from_configs([&live, &failing]).unwrap();
        let failing_id = ServerId::from_static("failing");

        let translator = Translator::new().with_router(router);
        translator.record_startup_failures(&[crate::error::ServerSpawnFailure {
            server_id: failing_id.clone(),
            language_id: LanguageId::from_static("rust"),
            command: ServerCommand::from_static("rust-analyzer"),
            reason: crate::error::StartupFailure::InitTaskPanicked,
        }]);
        translator.rebind_router(&HashSet::from([ServerId::from_static("live")]));
        translator.clear_expected_servers();

        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;

        assert_matches!(&result, Err(Error::ServerFailedToStart(f)) if f.server_id == failing_id,
            "got {result:?}"
        );
    }

    /// A server still expected to register wins over a recorded failure.
    #[tokio::test]
    async fn test_handle_workspace_symbol_initializing_wins_over_recorded_failure() {
        let translator = Translator::new();
        translator.set_expected_servers(HashSet::from([ServerId::from_static("pyright")]));
        translator.record_startup_failures(&[crate::error::ServerSpawnFailure {
            server_id: ServerId::from_static("other"),
            language_id: LanguageId::from_static("go"),
            command: ServerCommand::from_static("gopls"),
            reason: crate::error::StartupFailure::InitTaskPanicked,
        }]);

        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;

        assert_matches!(result, Err(Error::WorkspaceServersInitializing));
    }

    /// #242 regression: a server *is* configured and running, it just
    /// doesn't claim `workspace_symbols` and there is no catch-all -- the
    /// error must name the tool rather than collapse into the generic
    /// "no LSP server configured" message a client would also see if
    /// nothing were running at all.
    #[tokio::test]
    async fn test_handle_workspace_symbol_no_claimant_names_tool() {
        let configs = vec![crate::config::LspServerConfig {
            language_id: LanguageId::from_static("python"),
            command: ServerCommand::from_static("pyright-langserver").into(),
            args: crate::config::ServerArgs::default(),
            env: crate::config::ServerEnv::default(),
            file_patterns: vec![],
            initialization_options: None,
            settings: None,
            timeout_seconds: TimeoutSecs::new(30).unwrap(),
            request_timeout_seconds: TimeoutSecs::new(30).unwrap(),
            heuristics: None,
            name: Some(ServerId::from_static("pyright")),
            handles: Some(ToolSet::new(vec![ToolKind::Hover]).unwrap()),
            indexing: crate::bridge::IndexingPolicy::Auto,
        }];
        let router = ToolRouter::from_configs(&configs).unwrap();
        let translator = Translator::new().with_router(router);

        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;
        assert_matches!(
            result,
            Err(Error::NoServerForWorkspaceTool {
                tool: ToolKind::WorkspaceSymbols
            })
        );
    }

    /// #361 regression: a `Flat` (`SymbolInformation`) document-symbol
    /// response has only one range in the wire format, so `selection_range`
    /// must equal `range` exactly -- not merely be numerically close, which
    /// a bug re-deriving it via a second `normalize_range` call could still
    /// produce if line-text resolution raced with a concurrent edit.
    #[tokio::test]
    async fn test_handle_document_symbols_flat_response_selection_range_matches_range() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let (translator, mut server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities {
                document_symbol_provider: Some(lsp_types::DocumentSymbolProvider::Bool(true)),
                ..Default::default()
            },
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let path_str = path.to_string_lossy().to_string();
        let uri = Url::from_file_path(&path).unwrap().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_document_symbols(client_path(path_str))
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let symbol_request = read_framed_message(&mut wire).await;
        assert_eq!(symbol_request["method"], "textDocument/documentSymbol");

        write_response(
            &mut server.read_half_stdin,
            &symbol_request["id"],
            serde_json::json!([{
                "name": "main",
                "kind": 12,
                "location": {
                    "uri": uri,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 12},
                    },
                },
            }]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .expect("flat document symbol response should succeed");

        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].range, result.symbols[0].selection_range);
    }

    /// M2: a flat `SymbolInformation` document-symbol entry's `location.uri`
    /// is never trusted for encoding conversion -- `document_symbols` is a
    /// single-document request by construction, so every entry is
    /// normalized against `response_uri` (the already-resolved, trusted
    /// queried document), regardless of what the entry's own `location.uri`
    /// says. Uses a UTF-8-negotiated server and multibyte content to prove
    /// this: the entry names a nonexistent out-of-workspace URI, so if that
    /// URI were used instead, the disk read would fail and the position
    /// would fall back to the raw, unconverted byte offset (4) rather than
    /// the correctly re-derived UTF-16 column (3).
    #[tokio::test]
    async fn test_handle_document_symbols_flat_response_normalizes_against_response_uri() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let (translator, mut server) = translator_with_capabilities_and_encoding(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities {
                document_symbol_provider: Some(lsp_types::DocumentSymbolProvider::Bool(true)),
                ..Default::default()
            },
            lsp_types::PositionEncodingKind::UTF8,
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "aöb").unwrap();
        let path_str = path.to_string_lossy().to_string();
        let outside_uri = "file:///outside/workspace/does-not-exist.rs";

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_document_symbols(client_path(path_str))
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let symbol_request = read_framed_message(&mut wire).await;
        assert_eq!(symbol_request["method"], "textDocument/documentSymbol");

        write_response(
            &mut server.read_half_stdin,
            &symbol_request["id"],
            serde_json::json!([{
                "name": "sym",
                "kind": 12,
                "location": {
                    "uri": outside_uri,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 3},
                    },
                },
            }]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .expect("flat document symbol response should succeed");

        assert_eq!(result.symbols.len(), 1);
        assert_eq!(
            result.symbols[0].range.end.character(),
            3,
            "must convert against the queried document's own content (\"aöb\"), not fail to \
             read the entry's own (nonexistent, out-of-workspace) location.uri and fall back to \
             the raw byte offset"
        );
    }

    /// #497 end-to-end: `DocumentSymbolsResult::positions_degraded` must
    /// become `true` when a returned range's line can't be resolved for
    /// conversion under a non-UTF-16 server (here: a line past the queried
    /// document's own EOF) -- proving the flag actually reaches the
    /// caller-facing DTO, not just `EncodingCtx::positions_degraded()`
    /// itself.
    #[tokio::test]
    async fn test_handle_document_symbols_sets_positions_degraded_for_unresolvable_line() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let (translator, mut server) = translator_with_capabilities_and_encoding(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities {
                document_symbol_provider: Some(lsp_types::DocumentSymbolProvider::Bool(true)),
                ..Default::default()
            },
            lsp_types::PositionEncodingKind::UTF8,
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "aöb").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_document_symbols(client_path(path_str))
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let symbol_request = read_framed_message(&mut wire).await;
        assert_eq!(symbol_request["method"], "textDocument/documentSymbol");

        write_response(
            &mut server.read_half_stdin,
            &symbol_request["id"],
            serde_json::json!([{
                "name": "sym",
                "kind": 12,
                // The file has one line -- line 5 doesn't exist, so its
                // text can't be resolved for UTF-8 conversion.
                "range": {
                    "start": {"line": 5, "character": 0},
                    "end": {"line": 5, "character": 1},
                },
                "selectionRange": {
                    "start": {"line": 5, "character": 0},
                    "end": {"line": 5, "character": 1},
                },
            }]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .expect("document symbol response should still succeed, just degraded");

        assert_eq!(result.symbols.len(), 1);
        assert_eq!(
            result.positions_degraded,
            Some(PositionDegradation::Response),
            "a range whose line can't be resolved must mark the result degraded"
        );
    }

    /// S1/S4 regression: a `workspace/symbol` response in the newer
    /// `WorkspaceSymbol[]` shape can mix `Location` (has a range) and
    /// `LocationUriOnly` (no range) entries in the same response. The
    /// range-less entry must be dropped, not given a fabricated coordinate
    /// that would be indistinguishable from a real symbol at that position.
    #[tokio::test]
    async fn test_handle_workspace_symbol_drops_location_uri_only_entries() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            workspace_symbol_provider: Some(lsp_types::WorkspaceSymbolProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);
        let a_uri = Url::from_file_path(dir.path().join("a.rs"))
            .unwrap()
            .to_string();
        let b_uri = Url::from_file_path(dir.path().join("b.rs"))
            .unwrap()
            .to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_workspace_symbol("foo".to_string(), None, 100)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "workspace/symbol");

        // Untagged `WorkspaceSymbolResponse` deserialization is all-or-nothing
        // over the whole array: since `with_range`'s sibling below has no
        // `location.range`, the array as a whole fails to deserialize as
        // `Vec<SymbolInformation>` and falls through to `Vec<WorkspaceSymbol>`,
        // where `with_range`'s location becomes `Location` and
        // `without_range`'s becomes `LocationUriOnly`.
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([
                {
                    "name": "with_range",
                    "kind": 12,
                    "location": {
                        "uri": a_uri,
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 5}
                        }
                    }
                },
                {
                    "name": "without_range",
                    "kind": 12,
                    "location": { "uri": b_uri }
                }
            ]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(
            result.symbols.len(),
            1,
            "the range-less LocationUriOnly symbol must be dropped, not fabricated"
        );
        assert_eq!(result.symbols[0].name, "with_range");
        assert!(
            result.index_coverage.is_none(),
            "a non-empty result carries no coverage note"
        );
    }

    /// #796: an empty answer carries the coverage note.
    #[tokio::test]
    async fn test_handle_workspace_symbol_empty_result_carries_index_coverage() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            workspace_symbol_provider: Some(lsp_types::WorkspaceSymbolProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);
        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_workspace_symbol("foo".to_string(), None, 100)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let request = read_framed_message(&mut wire).await;
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::Value::Null,
        )
        .await;

        let result = handle.await.unwrap().unwrap();
        assert!(result.symbols.is_empty());
        assert_eq!(
            result.index_coverage,
            Some(IndexCoverage {
                synced_documents: 0,
                server_restarted: false,
            })
        );
    }

    /// #796: an answer emptied by `limit` is not an index-coverage question.
    #[tokio::test]
    async fn test_handle_workspace_symbol_limit_zero_carries_no_index_coverage() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            workspace_symbol_provider: Some(lsp_types::WorkspaceSymbolProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);
        let uri = Url::from_file_path(dir.path().join("a.rs"))
            .unwrap()
            .to_string();
        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_workspace_symbol("foo".to_string(), None, 0)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let request = read_framed_message(&mut wire).await;
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "name": "foo",
                "kind": 12,
                "location": {
                    "uri": uri,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 3}
                    }
                }
            }]),
        )
        .await;

        let result = handle.await.unwrap().unwrap();
        assert!(result.symbols.is_empty());
        assert!(result.truncated);
        assert!(result.index_coverage.is_none());
    }

    /// #415 (revised: `search_workspace_symbols` is read-only navigation,
    /// same policy as `get_definition`/`get_references`/call hierarchy --
    /// see `WorkspaceRoots::admits_uri`'s doc comment): a result whose URI falls
    /// outside every configured workspace root must still be returned, e.g.
    /// a symbol defined in the standard library or a crates.io dependency.
    #[tokio::test]
    async fn test_handle_workspace_symbol_does_not_filter_out_of_workspace_location() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            workspace_symbol_provider: Some(lsp_types::WorkspaceSymbolProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);
        let inside_uri = Url::from_file_path(canonical_dir(&dir).join("inside.rs"))
            .unwrap()
            .to_string();
        let outside_uri = "file:///outside/workspace/evil.rs";

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_workspace_symbol("foo".to_string(), None, 100)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "workspace/symbol");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([
                {
                    "name": "inside",
                    "kind": 12,
                    "location": {
                        "uri": inside_uri,
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 5}
                        }
                    }
                },
                {
                    "name": "outside",
                    "kind": 12,
                    "location": {
                        "uri": outside_uri,
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 4}
                        }
                    }
                }
            ]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(
            result.symbols.len(),
            2,
            "both the in-workspace and out-of-workspace symbols must be returned"
        );
        assert!(result.symbols.iter().any(|s| s.name == "inside"));
        assert!(result.symbols.iter().any(|s| s.name == "outside"));
        assert!(
            !result
                .symbols
                .iter()
                .find(|s| s.name == "inside")
                .unwrap()
                .location
                .out_of_workspace,
            "an in-workspace symbol location must not be marked out_of_workspace"
        );
        assert!(
            result
                .symbols
                .iter()
                .find(|s| s.name == "outside")
                .unwrap()
                .location
                .out_of_workspace,
            "an out-of-workspace symbol location must be marked out_of_workspace"
        );
    }

    /// Regression for #474: `limit` is a caller-supplied `u32` with no
    /// upper bound of its own -- `limit: u32::MAX` must still be clamped to
    /// `MAX_NORMALIZED_LOCATIONS`, not restore the unbounded normalization
    /// loop the cap exists to prevent.
    #[tokio::test]
    async fn test_handle_workspace_symbol_clamps_limit_to_max_normalized_locations() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            workspace_symbol_provider: Some(lsp_types::WorkspaceSymbolProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);
        let uri = Url::from_file_path(dir.path().join("many.rs"))
            .unwrap()
            .to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_workspace_symbol("foo".to_string(), None, u32::MAX)
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "workspace/symbol");

        let symbols: Vec<serde_json::Value> = (0..MAX_NORMALIZED_LOCATIONS + 500)
            .map(|i| {
                serde_json::json!({
                    "name": format!("sym{i}"),
                    "kind": 12,
                    "location": {
                        "uri": uri,
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 3}
                        }
                    }
                })
            })
            .collect();
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!(symbols),
        )
        .await;

        let result = timeout(Duration::from_secs(5), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(
            result.symbols.len(),
            MAX_NORMALIZED_LOCATIONS,
            "limit: u32::MAX must be clamped to MAX_NORMALIZED_LOCATIONS, not left unbounded"
        );
        assert!(
            result.truncated,
            "a limit clamped below what the caller asked for must set truncated: true"
        );
    }

    /// `document_symbols` is single-file analysis, valid even mid-index
    /// (spec FR-008), so `IndexingGate::FileLocal` at its
    /// `prepare_gated_document` call site must mean it dispatches even
    /// while the routed server reports `IndexingState::Loading` -- unlike
    /// the whole-workspace tools, which would error in this state.
    #[tokio::test]
    async fn test_handle_document_symbols_dispatches_while_indexing_loading() {
        use crate::bridge::NotificationCache;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            document_symbol_provider: Some(lsp_types::DocumentSymbolProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(tokio::sync::Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = Arc::new(translator.with_notification_cache(cache));

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move { translator.handle_document_symbols(client_path(path)).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/documentSymbol");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang -- document_symbols must not be gated")
            .unwrap()
            .unwrap();
        assert!(result.symbols.is_empty());
    }
}

#[cfg(test)]
mod fan_out_tests {
    use std::assert_matches;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::time::Duration;

    use futures::FutureExt as _;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt as _, BufReader};
    use tokio::task::JoinHandle;
    use url::Url;

    use super::*;
    use crate::bridge::WorkspaceRoots;
    use crate::bridge::translator::dto::IndexCoverage;
    use crate::bridge::translator::kind_filter::KindFilter as _;
    use crate::bridge::translator::testing::*;
    use crate::config::{LanguageId, ToolKind, ToolRouter, ToolSet};
    use crate::error::{ServerSpawnFailure, StartupFailure, UntrustedRefusal};
    use crate::lsp::LspServer;
    use crate::test_lsp::next_framed_message;

    enum Member {
        Live { capable: bool },
        LiveUtf8,
        Initializing,
        Absent,
    }

    struct Polyglot {
        dir: TempDir,
        translator: Arc<Translator>,
        servers: HashMap<String, FakeServer>,
    }

    impl Polyglot {
        fn new(members: &[(&str, Member)]) -> Self {
            let router = ToolRouter::catch_all(
                members
                    .iter()
                    .map(|(id, _)| (ServerId::new(*id).unwrap(), LanguageId::new(*id).unwrap())),
            );
            Self::with_router(members, router)
        }

        fn with_router(members: &[(&str, Member)], router: ToolRouter) -> Self {
            let dir = TempDir::new().unwrap();
            let mut translator = Translator::new().with_router(router);
            translator.set_workspace_roots(
                WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap(),
            );
            let mut servers = HashMap::new();
            let mut expected = HashSet::new();
            for (id, member) in members {
                let server_id = ServerId::new(*id).unwrap();
                match member {
                    Member::Live { capable } => {
                        let (client, server) = fake_lsp_client();
                        translator.register_client(server_id.clone(), client);
                        translator.register_server(
                            server_id,
                            LspServer::new_for_test(lsp_types::ServerCapabilities {
                                workspace_symbol_provider: capable
                                    .then_some(lsp_types::WorkspaceSymbolProvider::Bool(true)),
                                ..Default::default()
                            }),
                        );
                        servers.insert((*id).to_owned(), server);
                    }
                    Member::LiveUtf8 => {
                        let (client, server) = fake_lsp_client();
                        translator.register_client(server_id.clone(), client);
                        translator.register_server(
                            server_id,
                            LspServer::new_for_test_with_encoding(
                                lsp_types::ServerCapabilities {
                                    workspace_symbol_provider: Some(
                                        lsp_types::WorkspaceSymbolProvider::Bool(true),
                                    ),
                                    ..Default::default()
                                },
                                lsp_types::PositionEncodingKind::UTF8,
                            ),
                        );
                        servers.insert((*id).to_owned(), server);
                    }
                    Member::Initializing => {
                        expected.insert(server_id);
                    }
                    Member::Absent => {}
                }
            }
            translator.set_expected_servers(expected);
            Self {
                dir,
                translator: Arc::new(translator),
                servers,
            }
        }

        fn symbol(&self, name: &str, file: &str, line: u32) -> Value {
            let uri = Url::from_file_path(self.dir.path().join(file))
                .unwrap()
                .to_string();
            json!({
                "name": name,
                "kind": 12,
                "location": {
                    "uri": uri,
                    "range": {
                        "start": {"line": line, "character": 0},
                        "end": {"line": line, "character": 5},
                    },
                },
            })
        }

        fn take(&mut self, id: &str) -> FakeServer {
            self.servers.remove(id).unwrap()
        }

        async fn search(&self, kind: Option<&str>, limit: u32) -> Result<WorkspaceSymbolResult> {
            let filter = kind.map(|name| SymbolKindFilter::parse(name).expect("a known kind"));
            self.translator
                .handle_workspace_symbol("q".to_owned(), filter, limit)
                .await
        }
    }

    fn answer(mut server: FakeServer, delay: Duration, symbols: Value) -> JoinHandle<FakeServer> {
        tokio::spawn(async move {
            let id = {
                let mut wire = BufReader::new(&mut server.write_stdout);
                let request = next_framed_message(&mut wire).await;
                assert_eq!(request["method"], "workspace/symbol");
                request["id"].clone()
            };
            tokio::time::sleep(delay).await;
            write_response(&mut server.read_half_stdin, &id, symbols).await;
            server
        })
    }

    fn assert_no_traffic(server: &mut FakeServer) {
        let mut byte = [0_u8; 1];
        assert!(
            server.write_stdout.read(&mut byte).now_or_never().is_none(),
            "the server must not have been sent anything"
        );
    }

    fn names(result: &WorkspaceSymbolResult) -> Vec<(&str, &str)> {
        result
            .symbols
            .iter()
            .map(|symbol| {
                (
                    symbol.name.as_str(),
                    symbol.server.as_ref().map_or("", ServerId::as_str),
                )
            })
            .collect()
    }

    fn status_of<'a>(result: &'a WorkspaceSymbolResult, id: &str) -> &'a ServerStatus {
        &result
            .servers
            .iter()
            .find(|outcome| outcome.server.as_str() == id)
            .unwrap()
            .status
    }

    #[tokio::test(start_paused = true)]
    async fn merges_in_config_order_with_producer_tags() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::Live { capable: true }),
        ]);
        let a = answer(
            p.take("a"),
            Duration::from_millis(50),
            json!([p.symbol("Alpha", "a.rs", 0)]),
        );
        let b = answer(
            p.take("b"),
            Duration::ZERO,
            json!([p.symbol("Beta", "b.py", 1)]),
        );

        let result = p.search(None, 100).await.unwrap();
        let (_a, _b) = (a.await.unwrap(), b.await.unwrap());

        assert_eq!(names(&result), [("Alpha", "a"), ("Beta", "b")]);
        assert!(!result.truncated);
        assert!(result.index_coverage.is_none());
        assert_eq!(
            result
                .servers
                .iter()
                .map(|o| o.server.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(
            status_of(&result, "a"),
            &ServerStatus::Answered {
                symbols: 1,
                index_coverage: None,
                indexing_in_progress: false,
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reversed_reply_latency_gives_the_same_bytes() {
        let mut bytes = Vec::new();
        for (a_delay, b_delay) in [(10, 200), (200, 10)] {
            let mut p = Polyglot::new(&[
                ("a", Member::Live { capable: true }),
                ("b", Member::Live { capable: true }),
            ]);
            let a = answer(
                p.take("a"),
                Duration::from_millis(a_delay),
                json!([p.symbol("Alpha", "a.rs", 0)]),
            );
            let b = answer(
                p.take("b"),
                Duration::from_millis(b_delay),
                json!([p.symbol("Beta", "b.py", 1)]),
            );
            let result = p.search(None, 100).await.unwrap();
            let (_a, _b) = (a.await.unwrap(), b.await.unwrap());
            let mut value = serde_json::to_value(&result).unwrap();
            if let Some(symbols) = value["symbols"].as_array_mut() {
                for symbol in symbols {
                    symbol["location"]["uri"] = Value::Null;
                }
            }
            bytes.push(value.to_string());
        }
        assert_eq!(bytes[0], bytes[1]);
    }

    #[tokio::test(start_paused = true)]
    async fn total_latency_is_the_slowest_server_not_the_sum() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::Live { capable: true }),
            ("c", Member::Live { capable: true }),
        ]);
        let handles: Vec<_> = ["a", "b", "c"]
            .into_iter()
            .map(|id| answer(p.take(id), Duration::from_millis(200), json!([])))
            .collect();

        let started = tokio::time::Instant::now();
        let result = p.search(None, 100).await.unwrap();
        let elapsed = started.elapsed();
        for handle in handles {
            handle.await.unwrap();
        }

        assert!(elapsed < Duration::from_millis(400), "{elapsed:?}");
        assert_eq!(result.servers.len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn identical_duplicates_are_dropped_before_they_count_towards_the_cut() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::Live { capable: true }),
        ]);
        let x = p.symbol("X", "x.rs", 0);
        let a = answer(
            p.take("a"),
            Duration::ZERO,
            json!([x.clone(), x.clone(), x.clone()]),
        );
        let b = answer(
            p.take("b"),
            Duration::ZERO,
            json!([x, p.symbol("Z", "z.rs", 0)]),
        );

        let result = p.search(None, 2).await.unwrap();
        let (_a, _b) = (a.await.unwrap(), b.await.unwrap());

        assert_eq!(names(&result), [("X", "a"), ("Z", "b")]);
        assert!(!result.truncated, "only two unique symbols exist");
        assert_eq!(
            status_of(&result, "a"),
            &ServerStatus::Answered {
                symbols: 3,
                index_coverage: None,
                indexing_in_progress: false,
            },
            "the count is the answer's, before duplicates are dropped"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn limit_cuts_the_merged_set_and_flags_truncation() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::Live { capable: true }),
        ]);
        let a = answer(
            p.take("a"),
            Duration::ZERO,
            json!([p.symbol("A1", "a.rs", 0), p.symbol("A2", "a.rs", 1)]),
        );
        let b = answer(
            p.take("b"),
            Duration::ZERO,
            json!([p.symbol("B1", "b.py", 0)]),
        );

        let result = p.search(None, 2).await.unwrap();
        let (_a, _b) = (a.await.unwrap(), b.await.unwrap());

        assert_eq!(names(&result), [("A1", "a"), ("A2", "a")]);
        assert!(result.truncated);
        assert_eq!(
            status_of(&result, "b"),
            &ServerStatus::Answered {
                symbols: 1,
                index_coverage: None,
                indexing_in_progress: false,
            },
            "a server past the cut still reports its count"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn kind_filter_applies_to_every_server() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::Live { capable: true }),
        ]);
        let mut class = p.symbol("Klass", "b.py", 0);
        class["kind"] = json!(5);
        let a = answer(
            p.take("a"),
            Duration::ZERO,
            json!([p.symbol("func", "a.rs", 0)]),
        );
        let b = answer(
            p.take("b"),
            Duration::ZERO,
            json!([class, p.symbol("other", "b.py", 1)]),
        );

        let result = p.search(Some("class"), 100).await.unwrap();
        let (_a, _b) = (a.await.unwrap(), b.await.unwrap());

        assert_eq!(names(&result), [("Klass", "b")]);
        assert_eq!(
            status_of(&result, "a"),
            &ServerStatus::Answered {
                symbols: 1,
                index_coverage: None,
                indexing_in_progress: false,
            },
            "filtered-out symbols are not an empty answer"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_initializing_server_is_reported_without_waiting_for_it() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::Initializing),
        ]);
        let a = answer(
            p.take("a"),
            Duration::ZERO,
            json!([p.symbol("Alpha", "a.rs", 0)]),
        );

        let started = tokio::time::Instant::now();
        let result = p.search(None, 100).await.unwrap();
        a.await.unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(names(&result), [("Alpha", "a")]);
        assert_eq!(status_of(&result, "b"), &ServerStatus::Initializing);
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_without_the_capability_is_never_asked() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::Live { capable: false }),
        ]);
        let a = answer(
            p.take("a"),
            Duration::ZERO,
            json!([p.symbol("Alpha", "a.rs", 0)]),
        );
        let mut b = p.take("b");

        let result = p.search(None, 100).await.unwrap();
        a.await.unwrap();

        assert_no_traffic(&mut b);
        assert_eq!(
            status_of(&result, "b"),
            &ServerStatus::CapabilityNotAdvertised
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_server_times_out_at_its_own_deadline_and_others_survive() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::Live { capable: true }),
            ("c", Member::Live { capable: true }),
        ]);
        let a = answer(
            p.take("a"),
            Duration::ZERO,
            json!([p.symbol("Alpha", "a.rs", 0)]),
        );
        let c = answer(
            p.take("c"),
            Duration::ZERO,
            json!([p.symbol("Gamma", "c.go", 0)]),
        );
        let _silent = p.take("b");
        let deadline = crate::config::LspServerConfig::rust_analyzer()
            .request_timeout_seconds
            .as_duration();

        let started = tokio::time::Instant::now();
        let result = p.search(None, 100).await.unwrap();
        let elapsed = started.elapsed();
        let (_a, _c) = (a.await.unwrap(), c.await.unwrap());

        assert!(elapsed >= deadline, "{elapsed:?}");
        assert!(
            elapsed < deadline + Duration::from_millis(100),
            "{elapsed:?}"
        );
        assert_eq!(names(&result), [("Alpha", "a"), ("Gamma", "c")]);
        assert_eq!(status_of(&result, "b"), &ServerStatus::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_failed_to_start_is_listed_as_unavailable() {
        let mut p = Polyglot::new(&[("a", Member::Live { capable: true }), ("b", Member::Absent)]);
        p.translator.record_startup_failures(&[ServerSpawnFailure {
            server_id: ServerId::from_static("b"),
            language_id: LanguageId::from_static("b"),
            command: crate::config::ServerCommand::from_static("b-ls"),
            reason: StartupFailure::InitTaskPanicked,
        }]);
        let a = answer(
            p.take("a"),
            Duration::ZERO,
            json!([p.symbol("Alpha", "a.rs", 0)]),
        );

        let result = p.search(None, 100).await.unwrap();
        a.await.unwrap();

        assert_eq!(names(&result), [("Alpha", "a")]);
        assert_eq!(status_of(&result, "b"), &ServerStatus::Unavailable);
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_answer_carries_its_own_index_coverage() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::Live { capable: true }),
        ]);
        let a = answer(
            p.take("a"),
            Duration::ZERO,
            json!([p.symbol("Alpha", "a.rs", 0)]),
        );
        let b = answer(p.take("b"), Duration::ZERO, Value::Null);

        let result = p.search(None, 100).await.unwrap();
        let (_a, _b) = (a.await.unwrap(), b.await.unwrap());

        assert!(result.index_coverage.is_none());
        assert_eq!(
            status_of(&result, "b"),
            &ServerStatus::Answered {
                symbols: 0,
                index_coverage: Some(IndexCoverage {
                    synced_documents: 0,
                    server_restarted: false,
                }),
                indexing_in_progress: false,
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_still_indexing_flags_its_own_answer_only() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::Live { capable: true }),
        ]);
        let cache = Arc::new(tokio::sync::Mutex::new(
            crate::bridge::NotificationCache::new(),
        ));
        for (id, quiescent) in [("a", false), ("b", true)] {
            cache.lock().await.observe_indexing_signal(
                &ServerId::new(id).unwrap(),
                "experimental/serverStatus",
                Some(&json!({"quiescent": quiescent})),
            );
        }
        p.translator = Arc::new(
            Arc::into_inner(p.translator)
                .expect("no other owner")
                .with_notification_cache(cache),
        );
        let a = answer(p.take("a"), Duration::ZERO, json!([]));
        let b = answer(p.take("b"), Duration::ZERO, json!([]));

        let result = p.search(None, 100).await.unwrap();
        let (_a, _b) = (a.await.unwrap(), b.await.unwrap());

        assert_matches!(
            status_of(&result, "a"),
            ServerStatus::Answered {
                indexing_in_progress: true,
                ..
            }
        );
        assert_matches!(
            status_of(&result, "b"),
            ServerStatus::Answered {
                indexing_in_progress: false,
                ..
            }
        );
        let wire = serde_json::to_value(&result.servers).unwrap();
        assert_eq!(wire[0]["indexing_in_progress"], true);
        assert!(wire[1].get("indexing_in_progress").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn every_server_initializing_is_the_retryable_aggregate() {
        let p = Polyglot::new(&[("a", Member::Initializing), ("b", Member::Initializing)]);
        assert_matches!(
            p.search(None, 100).await,
            Err(Error::WorkspaceServersInitializing)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_capable_server_is_a_capability_error_never_an_empty_success() {
        let p = Polyglot::new(&[
            ("a", Member::Live { capable: false }),
            ("b", Member::Live { capable: false }),
        ]);
        assert_matches!(
            p.search(None, 100).await,
            Err(Error::CapabilityNotSupported { server_id, .. }) if server_id.as_str() == "a"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_server_leaves_the_single_server_shape_and_is_never_asked() {
        let mut p = Polyglot::new(&[("a", Member::Live { capable: true }), ("b", Member::Absent)]);
        p.translator.record_startup_failures(&[ServerSpawnFailure {
            server_id: ServerId::from_static("b"),
            language_id: LanguageId::from_static("b"),
            command: crate::config::ServerCommand::from_static("b-ls"),
            reason: StartupFailure::RefusedUntrustedWorkspace(UntrustedRefusal::NotAllowed {
                builtin: None,
            }),
        }]);
        let a = answer(
            p.take("a"),
            Duration::ZERO,
            json!([p.symbol("Alpha", "a.rs", 0)]),
        );

        let result = p.search(None, 100).await.unwrap();
        a.await.unwrap();

        let value = serde_json::to_value(&result).unwrap();
        assert!(value.get("servers").is_none(), "{value}");
        assert!(value["symbols"][0].get("server").is_none(), "{value}");
        assert_eq!(result.symbols.len(), 1);
    }

    /// A server whose `handles` omit the tool is never asked, while an
    /// explicit claimer no longer hides the catch-alls of other languages.
    #[tokio::test(start_paused = true)]
    async fn handles_declarations_decide_who_is_asked() {
        let configs = [
            crate::config::LspServerConfig {
                language_id: LanguageId::from_static("a"),
                name: Some(ServerId::from_static("a")),
                handles: Some(ToolSet::new(vec![ToolKind::WorkspaceSymbols]).unwrap()),
                ..crate::config::LspServerConfig::rust_analyzer()
            },
            crate::config::LspServerConfig {
                language_id: LanguageId::from_static("narrow"),
                name: Some(ServerId::from_static("narrow")),
                handles: Some(ToolSet::new(vec![ToolKind::Hover]).unwrap()),
                ..crate::config::LspServerConfig::rust_analyzer()
            },
            crate::config::LspServerConfig {
                language_id: LanguageId::from_static("b"),
                name: Some(ServerId::from_static("b")),
                handles: None,
                ..crate::config::LspServerConfig::rust_analyzer()
            },
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();
        let mut p = Polyglot::with_router(
            &[
                ("a", Member::Live { capable: true }),
                ("narrow", Member::Live { capable: true }),
                ("b", Member::Live { capable: true }),
            ],
            router,
        );
        let a = answer(
            p.take("a"),
            Duration::ZERO,
            json!([p.symbol("Alpha", "a.rs", 0)]),
        );
        let b = answer(
            p.take("b"),
            Duration::ZERO,
            json!([p.symbol("Beta", "b.go", 0)]),
        );
        let mut narrow = p.take("narrow");

        let result = p.search(None, 100).await.unwrap();
        let (_a, _b) = (a.await.unwrap(), b.await.unwrap());

        assert_no_traffic(&mut narrow);
        assert_eq!(names(&result), [("Alpha", "a"), ("Beta", "b")]);
        assert_eq!(
            result.servers.len(),
            2,
            "the declining server is not listed"
        );
    }

    fn start_failure(id: &'static str) -> ServerSpawnFailure {
        ServerSpawnFailure {
            server_id: ServerId::from_static(id),
            language_id: LanguageId::from_static(id),
            command: crate::config::ServerCommand::from_static("ls"),
            reason: StartupFailure::InitTaskPanicked,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_missing_server_next_to_an_initializing_one_stays_retryable() {
        let p = Polyglot::new(&[("a", Member::Absent), ("b", Member::Initializing)]);
        p.translator.record_startup_failures(&[start_failure("a")]);
        assert_matches!(
            p.search(None, 100).await,
            Err(Error::ServerInitializing { server_id }) if server_id.as_str() == "b"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn every_server_failing_to_start_reports_all_failures() {
        let p = Polyglot::new(&[("a", Member::Absent), ("b", Member::Absent)]);
        p.translator
            .record_startup_failures(&[start_failure("a"), start_failure("b")]);
        let Err(Error::AllServersFailedToInit { failures }) = p.search(None, 100).await else {
            panic!("expected AllServersFailedToInit");
        };
        assert_eq!(failures.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn single_server_output_is_byte_identical_to_the_pre_fan_out_shape() {
        let mut p = Polyglot::new(&[("a", Member::Live { capable: true })]);
        let symbol = p.symbol("Alpha", "a.rs", 0);
        let uri = symbol["location"]["uri"].clone();
        let a = answer(p.take("a"), Duration::ZERO, json!([symbol]));

        let result = p.search(None, 100).await.unwrap();
        let mut a = a.await.unwrap();

        assert_eq!(
            serde_json::to_string(&result).unwrap(),
            format!(
                r#"{{"symbols":[{{"name":"Alpha","kind":12,"location":{{"uri":{uri},"range":{{"start":{{"line":1,"character":1}},"end":{{"line":1,"character":6}}}}}}}}]}}"#
            )
        );
        assert_no_traffic(&mut a);
    }

    #[tokio::test(start_paused = true)]
    async fn degradation_of_a_contributing_server_is_reported() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::LiveUtf8),
        ]);
        let a = answer(
            p.take("a"),
            Duration::ZERO,
            json!([p.symbol("Alpha", "a.rs", 0)]),
        );
        let b = answer(
            p.take("b"),
            Duration::ZERO,
            json!([p.symbol("Beta", "b.py", 0)]),
        );

        let result = p.search(None, 100).await.unwrap();
        let (_a, _b) = (a.await.unwrap(), b.await.unwrap());

        assert_eq!(
            result.positions_degraded,
            Some(crate::bridge::translator::dto::PositionDegradation::Response)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn degradation_of_a_server_cut_by_the_limit_is_not_reported() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::LiveUtf8),
        ]);
        let a = answer(
            p.take("a"),
            Duration::ZERO,
            json!([p.symbol("Alpha", "a.rs", 0)]),
        );
        let b = answer(
            p.take("b"),
            Duration::ZERO,
            json!([p.symbol("Beta", "b.py", 0)]),
        );

        let result = p.search(None, 1).await.unwrap();
        let (_a, _b) = (a.await.unwrap(), b.await.unwrap());

        assert_eq!(names(&result), [("Alpha", "a")]);
        assert_eq!(result.positions_degraded, None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_cap_bounds_the_merged_result_across_servers() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::Live { capable: true }),
        ]);
        let many = |prefix: &str| {
            Value::Array(
                (0..6_000)
                    .map(|n| p.symbol(&format!("{prefix}{n}"), "x.rs", n))
                    .collect(),
            )
        };
        let (a_symbols, b_symbols) = (many("a"), many("b"));
        let a = answer(p.take("a"), Duration::ZERO, a_symbols);
        let b = answer(p.take("b"), Duration::ZERO, b_symbols);

        let result = p.search(None, u32::MAX).await.unwrap();
        let (_a, _b) = (a.await.unwrap(), b.await.unwrap());

        assert_eq!(result.symbols.len(), MAX_NORMALIZED_LOCATIONS);
        assert!(result.truncated);
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_dies_mid_call_is_unavailable_and_the_rest_survive() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::Live { capable: true }),
        ]);
        let a = answer(
            p.take("a"),
            Duration::from_millis(10),
            json!([p.symbol("Alpha", "a.rs", 0)]),
        );
        let mut b = p.take("b");
        let dying = tokio::spawn(async move {
            {
                let mut wire = BufReader::new(&mut b.write_stdout);
                next_framed_message(&mut wire).await;
            }
            drop(b);
        });

        let result = p.search(None, 100).await.unwrap();
        a.await.unwrap();
        dying.await.unwrap();

        assert_eq!(names(&result), [("Alpha", "a")]);
        assert_eq!(status_of(&result, "b"), &ServerStatus::Unavailable);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_the_call_cancels_every_outstanding_request() {
        let mut p = Polyglot::new(&[
            ("a", Member::Live { capable: true }),
            ("b", Member::Live { capable: true }),
        ]);
        let mut a = p.take("a");
        let mut b = p.take("b");
        let translator = Arc::clone(&p.translator);
        let call = tokio::spawn(async move {
            translator
                .handle_workspace_symbol("q".to_owned(), None, 100)
                .await
        });

        for server in [&mut a, &mut b] {
            let mut wire = BufReader::new(&mut server.write_stdout);
            let request = next_framed_message(&mut wire).await;
            assert_eq!(request["method"], "workspace/symbol");
        }
        call.abort();
        let _ = call.await;

        for server in [&mut a, &mut b] {
            let mut wire = BufReader::new(&mut server.write_stdout);
            let cancel = next_framed_message(&mut wire).await;
            assert_eq!(cancel["method"], "$/cancelRequest");
        }
    }

    fn producer(id: &str) -> Arc<Producer> {
        Arc::new(Producer {
            id: ServerId::new(id).unwrap(),
            ctx: test_ctx(),
        })
    }

    fn raw(name: &str, line: u32) -> RawWorkspaceSymbol {
        RawWorkspaceSymbol {
            name: name.to_owned(),
            kind: lsp_types::SymbolKind::Function,
            container_name: None,
            out_of_workspace: false,
            uri: lsp_types::Uri::from("file:///w/a.rs"),
            range: lsp_types::Range {
                start: lsp_types::Position { line, character: 0 },
                end: lsp_types::Position { line, character: 1 },
            },
        }
    }

    #[test]
    fn merged_symbols_stop_collecting_at_the_cap() {
        let mut merged = MergedSymbols::new(3);
        merged.extend(&producer("a"), (0..10).map(|line| raw("s", line)));
        merged.extend(&producer("b"), (10..20).map(|line| raw("s", line)));
        let (accepted, truncated) = merged.finish(2);
        assert_eq!(accepted.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn merged_symbols_keep_the_same_name_at_a_different_range() {
        let mut merged = MergedSymbols::new(10);
        merged.extend(&producer("a"), [raw("s", 0), raw("s", 0), raw("s", 1)]);
        let (accepted, truncated) = merged.finish(10);
        assert_eq!(accepted.len(), 2);
        assert!(!truncated);
    }

    fn timed_out() -> MemberFailure {
        MemberFailure::TimedOut(Error::Timeout(Duration::from_secs(1)))
    }

    fn initializing(id: &'static str) -> MemberFailure {
        MemberFailure::Initializing {
            server_id: ServerId::from_static(id),
        }
    }

    fn incapable(id: &'static str) -> MemberFailure {
        MemberFailure::CapabilityNotAdvertised(Error::CapabilityNotSupported {
            server_id: ServerId::from_static(id),
            capability: Capability::WorkspaceSymbols,
        })
    }

    fn failures(first: MemberFailure, rest: Vec<MemberFailure>) -> MemberFailures {
        MemberFailures { first, rest }
    }

    #[test]
    fn aggregate_failure_prefers_an_initializing_server_over_a_breakage() {
        let error = aggregate_failure(failures(timed_out(), vec![initializing("b")]));
        assert_matches!(error, Error::ServerInitializing { server_id } if server_id.as_str() == "b");
    }

    #[test]
    fn aggregate_failure_prefers_a_breakage_over_a_capability_gap() {
        let error = aggregate_failure(failures(incapable("a"), vec![timed_out()]));
        assert_matches!(error, Error::Timeout(_));
    }

    #[test]
    fn aggregate_failure_of_initializing_and_incapable_servers_is_initializing() {
        let error = aggregate_failure(failures(incapable("a"), vec![initializing("b")]));
        assert_matches!(error, Error::ServerInitializing { .. });
    }

    #[test]
    fn aggregate_failure_of_only_initializing_servers_is_the_aggregate() {
        let error = aggregate_failure(failures(initializing("a"), vec![initializing("b")]));
        assert_matches!(error, Error::WorkspaceServersInitializing);
    }

    #[test]
    fn aggregate_failure_of_incapable_servers_is_the_first_one() {
        let error = aggregate_failure(failures(incapable("a"), vec![incapable("b")]));
        assert_matches!(error, Error::CapabilityNotSupported { server_id, .. } if server_id.as_str() == "a");
    }

    #[test]
    fn a_process_that_died_mid_request_is_unavailable_not_failed() {
        assert_eq!(
            MemberFailure::from_request_error(Error::ServerTerminated).status(),
            ServerStatus::Unavailable
        );
        assert_eq!(
            MemberFailure::from_request_error(Error::NoServerConfigured).status(),
            ServerStatus::Failed
        );
    }

    #[test]
    fn a_failure_to_start_reported_by_a_request_keeps_the_all_failed_aggregate() {
        let spawn = start_failure("a");
        let from_request =
            MemberFailure::from_request_error(Error::ServerFailedToStart(Box::new(spawn)));
        assert_eq!(from_request.status(), ServerStatus::Unavailable);
        let error = aggregate_failure(failures(
            from_request,
            vec![MemberFailure::FailedToStart(Box::new(start_failure("b")))],
        ));
        assert_matches!(error, Error::AllServersFailedToInit { failures } if failures.len() == 2);
    }
}
