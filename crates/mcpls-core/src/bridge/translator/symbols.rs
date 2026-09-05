//! Document symbols and workspace symbol search handlers.

use lsp_types::{
    DocumentSymbol, DocumentSymbolParams, PartialResultParams, TextDocumentIdentifier,
    WorkDoneProgressParams, WorkspaceSymbolParams as LspWorkspaceSymbolParams,
};

use super::Translator;
use super::dto::{DocumentSymbolsResult, Location, Symbol, WorkspaceSymbol, WorkspaceSymbolResult};
use super::encoding_ctx::EncodingCtx;
use crate::bridge::lock_std;
use crate::config::{NoServerReason, ToolKind};
use crate::error::{Error, Result};

/// Validate parameters for `handle_workspace_symbol`.
fn validate_workspace_symbol_params(query: &str, kind_filter: Option<&str>) -> Result<()> {
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

    if query.len() > MAX_QUERY_LENGTH {
        return Err(Error::InvalidToolParams(format!(
            "Query too long: {} bytes (max {MAX_QUERY_LENGTH})",
            query.len()
        )));
    }

    if let Some(kind) = kind_filter
        && !VALID_SYMBOL_KINDS
            .iter()
            .any(|k| k.eq_ignore_ascii_case(kind))
    {
        return Err(Error::InvalidToolParams(format!(
            "Invalid kind_filter: '{kind}'. Valid values: {VALID_SYMBOL_KINDS:?}"
        )));
    }

    Ok(())
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
            kind: format!("{:?}", symbol.kind),
            range,
            selection_range,
            children,
        }
    })
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
        file_path: String,
    ) -> Result<DocumentSymbolsResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::DocumentSymbols,
                "documentSymbolProvider",
                |caps| {
                    matches!(
                        caps.document_symbol_provider,
                        Some(
                            lsp_types::DocumentSymbolProvider::Bool(true)
                                | lsp_types::DocumentSymbolProvider::DocumentSymbolOptions(_)
                        )
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let response_uri = uri.clone();

        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::DocumentSymbolRequest>(params, client.request_timeout())
            .await?;

        let symbols = match response {
            Some(lsp_types::DocumentSymbolResponse::SymbolInformationList(symbols)) => {
                let mut result = Vec::with_capacity(symbols.len());
                for sym in symbols {
                    let range = ctx
                        .normalize_range(&sym.location.uri, sym.location.range)
                        .await;
                    let selection_range = range.clone();
                    result.push(Symbol {
                        name: sym.base_symbol_information.name,
                        kind: format!("{:?}", sym.base_symbol_information.kind),
                        range,
                        selection_range,
                        children: None,
                    });
                }
                result
            }
            Some(lsp_types::DocumentSymbolResponse::DocumentSymbolList(symbols)) => {
                let mut result = Vec::with_capacity(symbols.len());
                for sym in symbols {
                    result.push(convert_document_symbol(sym, &ctx, &response_uri).await);
                }
                result
            }
            None => vec![],
        };

        Ok(DocumentSymbolsResult { symbols })
    }

    /// Handle workspace symbol search.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, no server is configured, or
    /// the routed server does not advertise `workspaceSymbolProvider` support.
    #[allow(clippy::too_many_lines)]
    pub async fn handle_workspace_symbol(
        &self,
        query: String,
        kind_filter: Option<String>,
        limit: u32,
    ) -> Result<WorkspaceSymbolResult> {
        validate_workspace_symbol_params(&query, kind_filter.as_deref())?;

        // Workspace search has no document, so it resolves via `resolve_any`
        // rather than a per-language route. If the resolved server is not
        // registered yet but is expected, tell the caller to wait and retry
        // rather than implying nothing is configured.
        let server_id = lock_std(&self.router)
            .resolve_any(ToolKind::WorkspaceSymbols)
            .cloned()
            .map_err(|reason| match reason {
                // `resolve_any` reports "nothing registered", which also
                // covers a server that is configured but has not finished
                // spawning yet -- check `expected_servers` (unavailable to
                // `ToolRouter` itself) to tell the two apart, mirroring
                // `get_client_for_file`'s `ServerInitializing` check below.
                NoServerReason::NothingRegistered => {
                    if lock_std(&self.expected_servers).is_empty() {
                        Error::NoServerConfigured
                    } else {
                        Error::WorkspaceServersInitializing
                    }
                }
                NoServerReason::NoClaimant => Error::NoServerForWorkspaceTool {
                    tool: ToolKind::WorkspaceSymbols,
                },
            })?;
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
                Some(
                    lsp_types::WorkspaceSymbolProvider::Bool(true)
                        | lsp_types::WorkspaceSymbolProvider::WorkspaceSymbolOptions(_)
                )
            )
        })?;

        let params = LspWorkspaceSymbolParams {
            query,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::WorkspaceSymbolRequest>(params, client.request_timeout())
            .await?;

        let ctx = self.encoding_ctx(&server_id);
        let mut symbols: Vec<WorkspaceSymbol> = Vec::new();
        match response {
            Some(lsp_types::WorkspaceSymbolResponse::SymbolInformationList(list)) => {
                for sym in list {
                    let range = ctx
                        .normalize_range(&sym.location.uri, sym.location.range)
                        .await;
                    symbols.push(WorkspaceSymbol {
                        name: sym.base_symbol_information.name,
                        kind: format!("{:?}", sym.base_symbol_information.kind),
                        location: Location {
                            uri: sym.location.uri.to_string(),
                            range,
                        },
                        container_name: sym.base_symbol_information.container_name,
                    });
                }
            }
            Some(lsp_types::WorkspaceSymbolResponse::WorkspaceSymbolList(list)) => {
                for sym in list {
                    let (uri, range) = match sym.location {
                        lsp_types::WorkspaceSymbolLocation::Location(loc) => {
                            let range = ctx.normalize_range(&loc.uri, loc.range).await;
                            (loc.uri.to_string(), range)
                        }
                        // `LocationUriOnly` carries no range -- the server
                        // deliberately withheld it (e.g. to avoid computing it
                        // eagerly for every workspace-search result). The MCP
                        // `Location` DTO has no way to represent "no range", and
                        // a fabricated range (e.g. line 1) would be
                        // indistinguishable from a real symbol there, so the
                        // symbol is dropped rather than inventing coordinates.
                        lsp_types::WorkspaceSymbolLocation::LocationUriOnly(_) => continue,
                    };
                    symbols.push(WorkspaceSymbol {
                        name: sym.base_symbol_information.name,
                        kind: format!("{:?}", sym.base_symbol_information.kind),
                        location: Location { uri, range },
                        container_name: sym.base_symbol_information.container_name,
                    });
                }
            }
            None => {}
        }

        // Apply kind filter if specified
        if let Some(kind) = kind_filter {
            symbols.retain(|s| s.kind.eq_ignore_ascii_case(&kind));
        }

        // Limit results
        symbols.truncate(limit as usize);

        Ok(WorkspaceSymbolResult { symbols })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::time::timeout;

    use super::*;
    use crate::bridge::translator::testing::*;
    use crate::config::{ServerId, ToolRouter};

    #[tokio::test]
    async fn test_handle_workspace_symbol_no_server() {
        let translator = Translator::new();
        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;
        assert!(matches!(result, Err(Error::NoServerConfigured)));
    }

    /// #242/S4 regression: a server is configured and still spawning (large
    /// project load) rather than never having existed -- the router alone
    /// cannot tell these apart (both look like "nothing registered"), so
    /// `handle_workspace_symbol` must consult `expected_servers` to report
    /// "still initializing" instead of the misleading "no server configured".
    #[tokio::test]
    async fn test_handle_workspace_symbol_reports_initializing_when_expected_but_not_registered() {
        let translator = Translator::new();
        translator.set_expected_servers(HashSet::from([ServerId::from("pyright")]));

        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;
        assert!(matches!(result, Err(Error::WorkspaceServersInitializing)));
    }

    /// #242 regression: a server *is* configured and running, it just
    /// doesn't claim `workspace_symbols` and there is no catch-all -- the
    /// error must name the tool rather than collapse into the generic
    /// "no LSP server configured" message a client would also see if
    /// nothing were running at all.
    #[tokio::test]
    async fn test_handle_workspace_symbol_no_claimant_names_tool() {
        let configs = vec![crate::config::LspServerConfig {
            language_id: "python".to_string(),
            command: "pyright-langserver".to_string(),
            args: vec![],
            env: HashMap::new(),
            file_patterns: vec![],
            initialization_options: None,
            timeout_seconds: 30,
            request_timeout_seconds: 30,
            heuristics: None,
            name: Some("pyright".to_string()),
            handles: Some(vec![ToolKind::Hover]),
        }];
        let router = ToolRouter::from_configs(&configs).unwrap();
        let translator = Translator::new().with_router(router);

        let result = translator
            .handle_workspace_symbol("test".to_string(), None, 100)
            .await;
        assert!(matches!(
            result,
            Err(Error::NoServerForWorkspaceTool {
                tool: ToolKind::WorkspaceSymbols
            })
        ));
    }

    /// #361 regression: a `Flat` (`SymbolInformation`) document-symbol
    /// response has only one range in the wire format, so `selection_range`
    /// must equal `range` exactly -- not merely be numerically close, which
    /// a bug re-deriving it via a second `normalize_range` call could still
    /// produce if line-text resolution raced with a concurrent edit.
    #[tokio::test]
    async fn test_handle_document_symbols_flat_response_selection_range_matches_range() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
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

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move { translator.handle_document_symbols(path_str).await })
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
                    "uri": "file:///main.rs",
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

    /// S1/S4 regression: a `workspace/symbol` response in the newer
    /// `WorkspaceSymbol[]` shape can mix `Location` (has a range) and
    /// `LocationUriOnly` (no range) entries in the same response. The
    /// range-less entry must be dropped, not given a fabricated coordinate
    /// that would be indistinguishable from a real symbol at that position.
    #[tokio::test]
    async fn test_handle_workspace_symbol_drops_location_uri_only_entries() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
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
                        "uri": "file:///a.rs",
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 5}
                        }
                    }
                },
                {
                    "name": "without_range",
                    "kind": 12,
                    "location": { "uri": "file:///b.rs" }
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
}
