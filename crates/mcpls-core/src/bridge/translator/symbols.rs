//! Document symbols and workspace symbol search handlers.

use lsp_types::{
    DocumentSymbol, DocumentSymbolParams, PartialResultParams, TextDocumentIdentifier,
    WorkDoneProgressParams, WorkspaceSymbolParams as LspWorkspaceSymbolParams,
};

use super::Translator;
use super::dto::{
    DocumentSymbolsResult, Location, Symbol, WorkspaceSymbol, WorkspaceSymbolResult,
    lsp_kind_to_u32,
};
use super::encoding_ctx::EncodingCtx;
use super::kind_filter::SymbolKindFilter;
use super::navigation::MAX_NORMALIZED_LOCATIONS;
use super::routing::{
    Capability, IndexingGate, PreparedDocument, WorkspaceRouteLookup, lookup_workspace_route,
};
use crate::bridge::ClientPath;
use crate::config::ToolKind;
use crate::error::{Error, Result};
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
                Capability::DocumentSymbols,
                IndexingGate::FileLocal,
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
        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier {
                uri: doc.uri().clone(),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let client = doc.client();
        let response = client
            .request_typed::<lsp_types::DocumentSymbolRequest>(params, client.request_timeout())
            .await?;
        Ok(FetchedSymbols { doc, ctx, response })
    }

    /// Handle workspace symbol search.
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
    #[expect(
        clippy::too_many_lines,
        reason = "linear request/response flow; splitting would scatter the steps"
    )]
    pub async fn handle_workspace_symbol(
        &self,
        query: String,
        kind_filter: Option<SymbolKindFilter>,
        limit: u32,
    ) -> Result<WorkspaceSymbolResult> {
        validate_query_length(&query)?;
        let kind_filter = kind_filter.map(|filter| u32::from(filter.kind()));

        // Workspace search has no document, so it resolves via `resolve_any`
        // rather than a per-language route. If the resolved server is not
        // registered yet but is expected, tell the caller to wait and retry
        // rather than implying nothing is configured.
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

        let params = LspWorkspaceSymbolParams {
            query,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::WorkspaceSymbolRequest>(params, client.request_timeout())
            .await?;

        let ctx = self.encoding_ctx(&server_id);

        // Collected without normalizing each symbol's range yet, so
        // `kind_filter`/`limit` below can drop entries before paying for
        // `EncodingCtx::normalize_range` (a disk read on a cache miss) on
        // each one -- bounds the *normalization* loop's cost to `limit`;
        // this collection loop itself still scans the full response (#474).
        let mut raw_symbols: Vec<RawWorkspaceSymbol> = Vec::new();
        match response {
            // Not filtered to workspace roots -- like other read-only
            // navigation results (see `WorkspaceRoots::admits_uri`'s doc
            // comment), a legitimate workspace-symbol result routinely
            // points outside the workspace (stdlib, a dependency), and any
            // subsequent open/read of it still hits the inbound
            // `WorkspaceRoots::validate` gate.
            Some(lsp_types::WorkspaceSymbolResponse::SymbolInformationList(list)) => {
                for sym in list {
                    raw_symbols.push(RawWorkspaceSymbol {
                        name: sym.base_symbol_information.name,
                        kind: lsp_kind_to_u32(sym.base_symbol_information.kind),
                        container_name: sym.base_symbol_information.container_name,
                        out_of_workspace: ctx.is_out_of_workspace(&sym.location.uri),
                        uri: sym.location.uri,
                        range: sym.location.range,
                    });
                }
            }
            Some(lsp_types::WorkspaceSymbolResponse::WorkspaceSymbolList(list)) => {
                for sym in list {
                    let (uri, range) = match sym.location {
                        lsp_types::WorkspaceSymbolLocation::Location(loc) => (loc.uri, loc.range),
                        // `LocationUriOnly` carries no range -- the server
                        // deliberately withheld it (e.g. to avoid computing it
                        // eagerly for every workspace-search result). The MCP
                        // `Location` DTO has no way to represent "no range", and
                        // a fabricated range (e.g. line 1) would be
                        // indistinguishable from a real symbol there, so the
                        // symbol is dropped rather than inventing coordinates.
                        lsp_types::WorkspaceSymbolLocation::LocationUriOnly(_) => continue,
                    };
                    raw_symbols.push(RawWorkspaceSymbol {
                        name: sym.base_symbol_information.name,
                        kind: lsp_kind_to_u32(sym.base_symbol_information.kind),
                        container_name: sym.base_symbol_information.container_name,
                        out_of_workspace: ctx.is_out_of_workspace(&uri),
                        uri,
                        range,
                    });
                }
            }
            None => {}
        }

        if let Some(target) = kind_filter {
            raw_symbols.retain(|s| s.kind == target);
        }
        // `limit` is clamped to MAX_NORMALIZED_LOCATIONS (else u32::MAX
        // would reopen the unbounded normalization loop, see #474).
        let effective_limit = (limit as usize).min(MAX_NORMALIZED_LOCATIONS);
        let truncated = raw_symbols.len() > effective_limit;
        raw_symbols.truncate(effective_limit);

        let mut symbols = Vec::with_capacity(raw_symbols.len());
        for raw in raw_symbols {
            let range = ctx.normalize_range(&raw.uri, raw.range).await;
            symbols.push(WorkspaceSymbol {
                name: raw.name,
                kind: raw.kind,
                location: Location {
                    uri: raw.uri.to_string(),
                    range,
                    out_of_workspace: raw.out_of_workspace,
                },
                container_name: raw.container_name,
            });
        }

        Ok(WorkspaceSymbolResult {
            symbols,
            truncated,
            positions_degraded: ctx.positions_degraded(),
        })
    }
}

/// A workspace symbol not yet normalized into MCP coordinates -- lets
/// [`Translator::handle_workspace_symbol`] apply `kind_filter`/`limit` before
/// paying for [`EncodingCtx::normalize_range`] on each surviving entry (see
/// #474).
struct RawWorkspaceSymbol {
    name: String,
    kind: u32,
    container_name: Option<String>,
    out_of_workspace: bool,
    uri: lsp_types::Uri,
    range: lsp_types::Range,
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
    use crate::bridge::translator::dto::PositionDegradation;
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
            #[allow(
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
            args: vec![],
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
            result.symbols[0].range.end.character, 3,
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
