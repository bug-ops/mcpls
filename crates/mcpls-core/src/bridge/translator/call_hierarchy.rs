//! Call hierarchy prepare/incoming/outgoing handlers.

use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyOutgoingCallsParams,
    CallHierarchyPrepareParams as LspCallHierarchyPrepareParams, PartialResultParams,
    TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
};

use super::Translator;
use super::dto::{
    CallHierarchyItemResult, CallHierarchyPrepareResult, IncomingCall, IncomingCallsResult,
    OutgoingCall, OutgoingCallsResult, Position, lsp_kind_to_u32,
};
use super::encoding_ctx::EncodingCtx;
use super::navigation::ItemBudget;
use super::routing::{Capability, IndexingGate, MAX_POSITION_VALUE};
use crate::config::ToolKind;
use crate::error::{Error, Result};

/// Parsed form of an MCP-facing `CallHierarchyItemResult` JSON value (1-based
/// coordinates), before its ranges are converted back to the routed server's
/// negotiated encoding -- which requires resolving that server first (from
/// [`Self::uri`]), so that step is left to callers via
/// [`call_hierarchy_item_to_lsp`].
struct ParsedCallHierarchyItem {
    uri: lsp_types::Uri,
    mcp: CallHierarchyItemResult,
}

/// Deserialize an MCP-facing `CallHierarchyItemResult` JSON value and parse
/// its URI.
///
/// MCP clients receive `CallHierarchyItemResult` from `prepare_call_hierarchy`
/// and pass it back opaquely to `get_incoming_calls` / `get_outgoing_calls`.
fn parse_mcp_call_hierarchy_item(item: serde_json::Value) -> Result<ParsedCallHierarchyItem> {
    let mcp: CallHierarchyItemResult = serde_json::from_value(item)
        .map_err(|e| Error::InvalidToolParams(format!("Invalid call hierarchy item: {e}")))?;

    // `gen-lsp-types`'s `Uri` is an opaque string wrapper with no validating
    // parse, so constructing it is infallible -- the malformed-URI rejection
    // this call used to provide is gone. Downstream consumers (e.g.
    // `parse_file_uri`) still validate the `file://` scheme and reject what
    // they can't use.
    let uri = lsp_types::Uri::from(mcp.uri.as_str());

    Ok(ParsedCallHierarchyItem { uri, mcp })
}

/// Convert a parsed MCP call hierarchy item (1-based coordinates) into a
/// `lsp_types::CallHierarchyItem` (0-based, in `ctx`'s negotiated encoding).
async fn call_hierarchy_item_to_lsp(
    parsed: ParsedCallHierarchyItem,
    ctx: &EncodingCtx,
) -> CallHierarchyItem {
    let ParsedCallHierarchyItem { uri, mcp } = parsed;

    // `SymbolKind: From<u32>` is infallible (see `lsp_kind_to_u32`'s docs),
    // so this exactly reverses the `lsp_kind_to_u32` call in
    // `convert_call_hierarchy_item` with no fallback needed.
    let kind = lsp_types::SymbolKind::from(mcp.kind);
    let range = ctx.denormalize_range(&uri, &mcp.range).await;
    let selection_range = ctx.denormalize_range(&uri, &mcp.selection_range).await;

    CallHierarchyItem {
        name: mcp.name,
        kind,
        tags: None,
        detail: mcp.detail,
        uri,
        range,
        selection_range,
        data: mcp.data,
    }
}

/// Convert LSP call hierarchy item to MCP call hierarchy item.
async fn convert_call_hierarchy_item(
    item: CallHierarchyItem,
    ctx: &EncodingCtx,
) -> CallHierarchyItemResult {
    let out_of_workspace = ctx.is_out_of_workspace(&item.uri);
    let range = ctx.normalize_range(&item.uri, item.range).await;
    let selection_range = ctx.normalize_range(&item.uri, item.selection_range).await;

    CallHierarchyItemResult {
        name: item.name,
        kind: lsp_kind_to_u32(item.kind),
        detail: item.detail,
        uri: item.uri.to_string(),
        range,
        selection_range,
        data: item.data,
        out_of_workspace,
    }
}

impl Translator {
    /// Handle call hierarchy prepare request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `callHierarchyProvider` support.
    pub async fn handle_call_hierarchy_prepare(
        &self,
        file_path: String,
        position: Position,
    ) -> Result<CallHierarchyPrepareResult> {
        let Position { line, character } = position;
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

        let doc = self
            .prepare_gated_document(
                &file_path,
                ToolKind::CallHierarchy,
                Capability::CallHierarchy,
                IndexingGate::NotRequired,
            )
            .await?;
        let (server_id, client, uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let lsp_position = ctx.to_lsp(uri, position).await;

        let params = LspCallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::CallHierarchyPrepareRequest>(
                params,
                client.request_timeout(),
            )
            .await?;

        // Pre-allocate and build result. Not filtered to workspace roots --
        // see `lsp_locations_to_mcp`'s doc comment in `navigation.rs` for why
        // a read-only location outside the workspace (stdlib, a dependency)
        // is normal, expected navigation rather than something to drop.
        let lsp_items = response.unwrap_or_default();
        let mut items = Vec::with_capacity(lsp_items.len());
        for item in lsp_items {
            items.push(convert_call_hierarchy_item(item, &ctx).await);
        }

        Ok(CallHierarchyPrepareResult {
            items,
            positions_degraded: ctx.positions_degraded(),
        })
    }

    /// Handle incoming calls request.
    ///
    /// Routing through `prepare_gated_document_for_path` means this now
    /// stats, reads, and `didOpen`s the item's own file as a side effect,
    /// even though a call-hierarchy item is opaque per the LSP spec and
    /// needs no open document -- accepted for chokepoint/gating
    /// consistency with `handle_references` and the other whole-workspace
    /// tools; it does mean a replayed item whose file has since been
    /// deleted now fails on that stat instead of just proceeding.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the item is invalid, the
    /// routed server does not advertise `callHierarchyProvider` support, or
    /// the server is still indexing the workspace after
    /// `INDEXING_READY_TIMEOUT`.
    pub async fn handle_incoming_calls(
        &self,
        item: serde_json::Value,
    ) -> Result<IncomingCallsResult> {
        // Deserialize as our own type (1-based coords).
        let parsed = parse_mcp_call_hierarchy_item(item)?;

        // Same ToolKind/route as `handle_call_hierarchy_prepare`.
        let path = self.parse_file_uri(&parsed.uri)?;
        let doc = self
            .prepare_gated_document_for_path(
                &path,
                ToolKind::CallHierarchy,
                Capability::CallHierarchy,
                IndexingGate::Required,
            )
            .await?;
        let (server_id, client, _uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let lsp_item = call_hierarchy_item_to_lsp(parsed, &ctx).await;

        let params = CallHierarchyIncomingCallsParams {
            item: lsp_item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::CallHierarchyIncomingCallsRequest>(
                params,
                client.request_timeout(),
            )
            .await?;

        // Pre-allocate and build result. Not filtered to workspace roots --
        // see `handle_call_hierarchy_prepare`'s comment above.
        let mut budget = ItemBudget::new();
        let mut calls = Vec::new();

        for call in response.unwrap_or_default() {
            // A call is kept together with all its ranges or not at all.
            if !budget.spend_whole(1 + call.from_ranges.len()) {
                continue;
            }
            // Per the LSP spec, `fromRanges` are ranges within the *caller's*
            // document (`call.from.uri`), not the queried item's document.
            let from_uri = call.from.uri.clone();
            let mut from_ranges = Vec::with_capacity(call.from_ranges.len());
            for range in call.from_ranges {
                from_ranges.push(ctx.normalize_range(&from_uri, range).await);
            }

            calls.push(IncomingCall {
                from: convert_call_hierarchy_item(call.from, &ctx).await,
                from_ranges,
            });
        }

        Ok(IncomingCallsResult {
            calls,
            truncated: budget.truncated(),
            positions_degraded: ctx.positions_degraded(),
        })
    }

    /// Handle outgoing calls request.
    ///
    /// Same `didOpen`-as-side-effect trade-off as `handle_incoming_calls` --
    /// see that method's doc.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the item is invalid, the
    /// routed server does not advertise `callHierarchyProvider` support, or
    /// the server is still indexing the workspace after
    /// `INDEXING_READY_TIMEOUT`.
    pub async fn handle_outgoing_calls(
        &self,
        item: serde_json::Value,
    ) -> Result<OutgoingCallsResult> {
        // Deserialize as our own type (1-based coords).
        let parsed = parse_mcp_call_hierarchy_item(item)?;

        // Parse the URI and gate through the same chokepoint as
        // `handle_incoming_calls` -- see that function's comment (#423).
        // Same ToolKind/route as `prepare`.
        let path = self.parse_file_uri(&parsed.uri)?;
        let doc = self
            .prepare_gated_document_for_path(
                &path,
                ToolKind::CallHierarchy,
                Capability::CallHierarchy,
                IndexingGate::Required,
            )
            .await?;
        let (server_id, client, _uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        // Per the LSP spec, an outgoing call's `fromRanges` are ranges within
        // the *queried* item's own document, not the callee's (`call.to.uri`).
        let source_uri = parsed.uri.clone();
        let lsp_item = call_hierarchy_item_to_lsp(parsed, &ctx).await;

        let params = CallHierarchyOutgoingCallsParams {
            item: lsp_item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::CallHierarchyOutgoingCallsRequest>(
                params,
                client.request_timeout(),
            )
            .await?;

        // Pre-allocate and build result. Not filtered to workspace roots --
        // see `handle_call_hierarchy_prepare`'s comment above.
        let mut budget = ItemBudget::new();
        let mut calls = Vec::new();

        for call in response.unwrap_or_default() {
            if !budget.spend_whole(1 + call.from_ranges.len()) {
                continue;
            }
            let mut from_ranges = Vec::with_capacity(call.from_ranges.len());
            for range in call.from_ranges {
                from_ranges.push(ctx.normalize_range(&source_uri, range).await);
            }

            calls.push(OutgoingCall {
                to: convert_call_hierarchy_item(call.to, &ctx).await,
                from_ranges,
            });
        }

        Ok(OutgoingCallsResult {
            calls,
            truncated: budget.truncated(),
            positions_degraded: ctx.positions_degraded(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
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
    use crate::bridge::translator::dto::{Position, Position2D, Range};
    use crate::bridge::translator::testing::*;
    use crate::config::ServerId;

    #[tokio::test]
    async fn test_handle_call_hierarchy_prepare_invalid_position_zero() {
        let translator = Translator::new();
        let result = translator
            .handle_call_hierarchy_prepare(
                "/tmp/test.rs".to_string(),
                Position {
                    line: 0,
                    character: 1,
                },
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));

        let result = translator
            .handle_call_hierarchy_prepare(
                "/tmp/test.rs".to_string(),
                Position {
                    line: 1,
                    character: 0,
                },
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_call_hierarchy_prepare_invalid_position_too_large() {
        let translator = Translator::new();
        let result = translator
            .handle_call_hierarchy_prepare(
                "/tmp/test.rs".to_string(),
                Position {
                    line: 1_000_001,
                    character: 1,
                },
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));

        let result = translator
            .handle_call_hierarchy_prepare(
                "/tmp/test.rs".to_string(),
                Position {
                    line: 1,
                    character: 1_000_001,
                },
            )
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

    /// Builds a `CallHierarchyItemResult` JSON value pointing at `path`, for
    /// driving `handle_incoming_calls`/`handle_outgoing_calls` directly
    /// without a preceding `prepare_call_hierarchy` round trip.
    fn call_hierarchy_item_json(uri: &str) -> serde_json::Value {
        serde_json::to_value(CallHierarchyItemResult {
            name: "queried_fn".to_string(),
            kind: 12,
            detail: None,
            uri: uri.to_string(),
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 4,
                },
            },
            selection_range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 4,
                },
            },
            data: None,
            out_of_workspace: false,
        })
        .unwrap()
    }

    /// #423 regression: `handle_incoming_calls` is a whole-workspace query of
    /// the same class as `references` and must be gated on indexing
    /// readiness the same way, instead of bypassing `prepare_gated_document`
    /// entirely.
    #[tokio::test(start_paused = true)]
    async fn test_handle_incoming_calls_returns_workspace_indexing_error_when_loading() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyProvider::Bool(true)),
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

        let path = dir.path().join("queried.rs");
        fs::write(&path, "fn queried() {}").unwrap();
        let uri = Url::from_file_path(&path).unwrap().to_string();

        let err = translator
            .handle_incoming_calls(call_hierarchy_item_json(&uri))
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        ));
    }

    /// #423 regression: companion for `handle_outgoing_calls` -- see
    /// `test_handle_incoming_calls_returns_workspace_indexing_error_when_loading`.
    #[tokio::test(start_paused = true)]
    async fn test_handle_outgoing_calls_returns_workspace_indexing_error_when_loading() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyProvider::Bool(true)),
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

        let path = dir.path().join("queried.rs");
        fs::write(&path, "fn queried() {}").unwrap();
        let uri = Url::from_file_path(&path).unwrap().to_string();

        let err = translator
            .handle_outgoing_calls(call_hierarchy_item_json(&uri))
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        ));
    }

    /// S4 lock-in for the documented `Uri`-validation-loss behavior change
    /// (see the CHANGELOG entry for #297): `parse_mcp_call_hierarchy_item`
    /// can no longer reject a malformed `uri` field at construction time
    /// (`gen-lsp-types`'s `Uri` has no validating parse). This drives a
    /// structurally-valid item whose `uri` field is `file://`-prefixed (so
    /// `parse_file_uri`'s scheme check still passes, same as before the
    /// migration) but points at a path that does not exist on disk, through
    /// the real `handle_incoming_calls`/`handle_outgoing_calls` handlers, and
    /// pins the actual resulting error: `Error::FileIo` from
    /// `validate_path`'s `canonicalize()` call, not the old construction-time
    /// `Error::InvalidToolParams`.
    #[tokio::test]
    async fn test_handle_incoming_calls_with_nonexistent_file_uri_returns_file_io_not_invalid_uri()
    {
        let mut translator = Translator::new();
        // A workspace root is required so `validate_path` reaches
        // `canonicalize()` instead of failing closed on `NoWorkspaceRoots`.
        #[cfg(windows)]
        translator.set_workspace_roots(vec![std::path::PathBuf::from(r"C:\")]);
        #[cfg(not(windows))]
        translator.set_workspace_roots(vec![std::path::PathBuf::from("/")]);
        // `Url::to_file_path` on Windows requires a drive-letter first path
        // segment; a Unix-style path with none fails to convert at all
        // (`uri_to_path` returns `None`, i.e. `Error::InvalidToolParams`)
        // before ever reaching `canonicalize()`, trivially failing this
        // assertion for the wrong reason. Use a drive-letter path so the
        // test exercises the intended `FileIo` path on every platform.
        #[cfg(windows)]
        let uri = "file:///C:/this/path/does/not/exist/anywhere.rs";
        #[cfg(not(windows))]
        let uri = "file:///this/path/does/not/exist/anywhere.rs";
        let item = serde_json::json!({
            "name": "foo",
            "kind": 12,
            "uri": uri,
            "range": {
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 1}
            },
            "selectionRange": {
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 1}
            }
        });

        let result = translator.handle_incoming_calls(item).await;

        assert!(
            matches!(result, Err(Error::FileIo { .. })),
            "expected Error::FileIo from the canonicalize() failure now that Uri construction \
             cannot itself reject a malformed uri, got {result:?}"
        );
    }

    /// As above, through `handle_outgoing_calls` -- same
    /// `parse_mcp_call_hierarchy_item` code path, different caller.
    #[tokio::test]
    async fn test_handle_outgoing_calls_with_nonexistent_file_uri_returns_file_io_not_invalid_uri()
    {
        let mut translator = Translator::new();
        // A workspace root is required so `validate_path` reaches
        // `canonicalize()` instead of failing closed on `NoWorkspaceRoots`.
        #[cfg(windows)]
        translator.set_workspace_roots(vec![std::path::PathBuf::from(r"C:\")]);
        #[cfg(not(windows))]
        translator.set_workspace_roots(vec![std::path::PathBuf::from("/")]);
        #[cfg(windows)]
        let uri = "file:///C:/this/path/does/not/exist/anywhere.rs";
        #[cfg(not(windows))]
        let uri = "file:///this/path/does/not/exist/anywhere.rs";
        let item = serde_json::json!({
            "name": "foo",
            "kind": 12,
            "uri": uri,
            "range": {
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 1}
            },
            "selectionRange": {
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 1}
            }
        });

        let result = translator.handle_outgoing_calls(item).await;

        assert!(
            matches!(result, Err(Error::FileIo { .. })),
            "expected Error::FileIo from the canonicalize() failure now that Uri construction \
             cannot itself reject a malformed uri, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_convert_call_hierarchy_item_kind_is_numeric() {
        let item = lsp_types::CallHierarchyItem {
            name: "my_fn".to_string(),
            kind: lsp_types::SymbolKind::Function,
            tags: None,
            detail: None,
            uri: lsp_types::Uri::from("file:///tmp/test.rs"),
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
        let result = convert_call_hierarchy_item(item, &test_ctx()).await;
        // SymbolKind::Function is LSP integer 12
        assert_eq!(result.kind, 12u32);
        assert_eq!(result.name, "my_fn");
    }

    /// #467 M2/tester gap 2: pins that `call_hierarchy_item_to_lsp`'s reverse
    /// `u32 -> SymbolKind` conversion round-trips a non-`Function` kind
    /// through to the outbound wire request unchanged -- not the
    /// `unwrap_or(Function)` fallback it used to fabricate on a conversion
    /// failure that could not actually occur.
    #[tokio::test]
    async fn test_handle_incoming_calls_round_trips_non_function_kind() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let queried_path = dir.path().join("queried.rs");
        fs::write(&queried_path, "fn queried() {}").unwrap();
        let queried_uri = Url::from_file_path(&queried_path).unwrap().to_string();

        let item = CallHierarchyItemResult {
            name: "queried_method".to_string(),
            kind: 6, // SymbolKind::Method
            detail: None,
            uri: queried_uri,
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 4,
                },
            },
            selection_range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 4,
                },
            },
            data: None,
            out_of_workspace: false,
        };

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let item = serde_json::to_value(item).unwrap();
            tokio::spawn(async move { translator.handle_incoming_calls(item).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");

        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "callHierarchy/incomingCalls");
        assert_eq!(
            request["params"]["item"]["kind"], 6,
            "the reverse u32 -> SymbolKind conversion must round-trip a non-Function kind \
             exactly, not fall back to Function (12)"
        );

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();
        assert!(result.calls.is_empty());
    }

    /// Per the LSP spec, an incoming call's `fromRanges` are ranges within
    /// the *caller's* document (`call.from.uri`), not the queried item's
    /// document -- `handle_incoming_calls` must convert them against
    /// `caller.rs`'s own content, not `queried.rs`'s. Uses a UTF-8-negotiated
    /// server and two files with different multibyte content, so converting
    /// against the wrong file's line text produces a different, wrong
    /// answer: `"aöb"` (caller) puts LSP byte offset 3 at UTF-16 column 3
    /// (`ö` is 2 UTF-8 bytes / 1 UTF-16 unit), while the ASCII `"abc"`
    /// (queried item) would put the same byte offset at column 4.
    #[tokio::test]
    async fn test_handle_incoming_calls_from_ranges_convert_against_callers_own_uri() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities_and_encoding(
            &dir,
            &server_id,
            caps,
            lsp_types::PositionEncodingKind::UTF8,
        );

        let queried_path = dir.path().join("queried.rs");
        fs::write(&queried_path, "abc").unwrap();
        let queried_uri = Url::from_file_path(&queried_path).unwrap().to_string();

        let caller_path = dir.path().join("caller.rs");
        fs::write(&caller_path, "aöb").unwrap();
        let caller_uri = Url::from_file_path(&caller_path).unwrap().to_string();

        let item = CallHierarchyItemResult {
            name: "queried_fn".to_string(),
            kind: 12,
            detail: None,
            uri: queried_uri,
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 4,
                },
            },
            selection_range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 4,
                },
            },
            data: None,
            out_of_workspace: false,
        };

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let item = serde_json::to_value(item).unwrap();
            tokio::spawn(async move { translator.handle_incoming_calls(item).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");

        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "callHierarchy/incomingCalls");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "from": {
                    "name": "caller_fn",
                    "kind": 12,
                    "uri": caller_uri,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    },
                    "selectionRange": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    }
                },
                "fromRanges": [{
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 3}
                }]
            }]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(result.calls.len(), 1);
        let from_range = &result.calls[0].from_ranges[0];
        assert_eq!(
            from_range.end.character, 3,
            "fromRanges must convert against the caller's own file (\"aöb\"), not the queried \
             item's (\"abc\") -- a byte offset of 3 is UTF-16 column 3 in the former, 4 in the \
             latter"
        );
        assert!(
            !result.calls[0].from.out_of_workspace,
            "an in-workspace caller must not be marked out_of_workspace"
        );
    }

    /// Per the LSP spec, an outgoing call's `fromRanges` are ranges within
    /// the *queried* item's own document, not the callee's (`call.to.uri`) --
    /// the inverse directional convention from incoming calls, tested above.
    #[tokio::test]
    async fn test_handle_outgoing_calls_from_ranges_convert_against_queried_uri() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities_and_encoding(
            &dir,
            &server_id,
            caps,
            lsp_types::PositionEncodingKind::UTF8,
        );

        let queried_path = dir.path().join("queried.rs");
        fs::write(&queried_path, "aöb").unwrap();
        let queried_uri = Url::from_file_path(&queried_path).unwrap().to_string();

        let callee_path = dir.path().join("callee.rs");
        fs::write(&callee_path, "abc").unwrap();
        let callee_uri = Url::from_file_path(&callee_path).unwrap().to_string();

        let item = CallHierarchyItemResult {
            name: "queried_fn".to_string(),
            kind: 12,
            detail: None,
            uri: queried_uri,
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 4,
                },
            },
            selection_range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 4,
                },
            },
            data: None,
            out_of_workspace: false,
        };

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let item = serde_json::to_value(item).unwrap();
            tokio::spawn(async move { translator.handle_outgoing_calls(item).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");

        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "callHierarchy/outgoingCalls");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "to": {
                    "name": "callee_fn",
                    "kind": 12,
                    "uri": callee_uri,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    },
                    "selectionRange": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    }
                },
                "fromRanges": [{
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 3}
                }]
            }]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(result.calls.len(), 1);
        let from_range = &result.calls[0].from_ranges[0];
        assert_eq!(
            from_range.end.character, 3,
            "fromRanges must convert against the queried item's own file (\"aöb\"), not the \
             callee's (\"abc\") -- a byte offset of 3 is UTF-16 column 3 in the former, 4 in \
             the latter"
        );
    }

    /// #415 (revised per critic C1): an incoming call whose caller
    /// (`call.from.uri`) lies outside every configured workspace root must
    /// still be returned -- a caller in the standard library or a
    /// crates.io dependency is normal, expected call-hierarchy navigation,
    /// not an attack. See `navigation.rs`'s
    /// `test_handle_definition_does_not_filter_out_of_workspace_location`
    /// for the same policy on goto-X locations.
    #[tokio::test]
    async fn test_handle_incoming_calls_does_not_filter_out_of_workspace_caller() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let queried_path = dir.path().join("queried.rs");
        fs::write(&queried_path, "fn queried() {}").unwrap();
        let queried_uri = Url::from_file_path(&queried_path).unwrap().to_string();
        let outside_uri = "file:///outside/workspace/stdlib.rs";

        let item = CallHierarchyItemResult {
            name: "queried_fn".to_string(),
            kind: 12,
            detail: None,
            uri: queried_uri,
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 4,
                },
            },
            selection_range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 4,
                },
            },
            data: None,
            out_of_workspace: false,
        };

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let item = serde_json::to_value(item).unwrap();
            tokio::spawn(async move { translator.handle_incoming_calls(item).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");

        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "callHierarchy/incomingCalls");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "from": {
                    "name": "caller_fn",
                    "kind": 12,
                    "uri": outside_uri,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    },
                    "selectionRange": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    }
                },
                "fromRanges": [{
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1}
                }]
            }]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(
            result.calls.len(),
            1,
            "an out-of-workspace caller must be returned, not dropped"
        );
        assert_eq!(result.calls[0].from.uri, outside_uri);
        assert!(
            result.calls[0].from.out_of_workspace,
            "an out-of-workspace caller must be marked out_of_workspace"
        );
    }

    /// #415 (revised per critic C1) companion for outgoing calls: a callee
    /// (`call.to.uri`) outside every configured workspace root must still be
    /// returned -- see the incoming-calls test above for the rationale.
    #[tokio::test]
    async fn test_handle_outgoing_calls_does_not_filter_out_of_workspace_callee() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let queried_path = dir.path().join("queried.rs");
        fs::write(&queried_path, "fn queried() {}").unwrap();
        let queried_uri = Url::from_file_path(&queried_path).unwrap().to_string();
        let outside_uri = "file:///outside/workspace/stdlib.rs";

        let item = CallHierarchyItemResult {
            name: "queried_fn".to_string(),
            kind: 12,
            detail: None,
            uri: queried_uri,
            range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 4,
                },
            },
            selection_range: Range {
                start: Position2D {
                    line: 1,
                    character: 1,
                },
                end: Position2D {
                    line: 1,
                    character: 4,
                },
            },
            data: None,
            out_of_workspace: false,
        };

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let item = serde_json::to_value(item).unwrap();
            tokio::spawn(async move { translator.handle_outgoing_calls(item).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");

        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "callHierarchy/outgoingCalls");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "to": {
                    "name": "callee_fn",
                    "kind": 12,
                    "uri": outside_uri,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    },
                    "selectionRange": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    }
                },
                "fromRanges": [{
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1}
                }]
            }]),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(
            result.calls.len(),
            1,
            "an out-of-workspace callee must be returned, not dropped"
        );
        assert_eq!(result.calls[0].to.uri, outside_uri);
        assert!(
            result.calls[0].to.out_of_workspace,
            "an out-of-workspace callee must be marked out_of_workspace"
        );
    }

    /// #411 regression: `prepare_call_hierarchy` -> `get_incoming_calls`
    /// round trip must resolve a percent-encoded URI (space, non-ASCII)
    /// back to the file it names. Before the fix, `parse_file_uri`
    /// raw-sliced the URI instead of decoding it, so `canonicalize()`
    /// failed with `ENOENT` even though the file exists.
    #[tokio::test]
    async fn test_prepare_then_incoming_calls_round_trip_percent_encoded_path() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities_and_encoding(
            &dir,
            &server_id,
            caps,
            lsp_types::PositionEncodingKind::UTF8,
        );

        let file_path = dir.path().join("my file café.rs");
        fs::write(&file_path, "fn foo() {}").unwrap();
        let file_uri = Url::from_file_path(&file_path).unwrap().to_string();
        assert!(
            file_uri.contains("%20"),
            "test fixture must exercise percent-encoding"
        );

        let translator = Arc::new(translator);
        let mut wire = BufReader::new(&mut server.write_stdout);

        let prepare_handle = {
            let translator = Arc::clone(&translator);
            let path = file_path.to_string_lossy().into_owned();
            tokio::spawn(async move {
                translator
                    .handle_call_hierarchy_prepare(path, pos(1, 1))
                    .await
            })
        };

        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/prepareCallHierarchy");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "name": "foo",
                "kind": 12,
                "uri": file_uri,
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 11}
                },
                "selectionRange": {
                    "start": {"line": 0, "character": 3},
                    "end": {"line": 0, "character": 6}
                }
            }]),
        )
        .await;

        let prepare_result = timeout(Duration::from_secs(2), prepare_handle)
            .await
            .expect("prepare should not hang")
            .unwrap()
            .unwrap();
        assert_eq!(prepare_result.items.len(), 1);
        let item = prepare_result.items[0].clone();
        assert!(item.uri.contains("%20"));

        let incoming_handle = {
            let translator = Arc::clone(&translator);
            let item = serde_json::to_value(item).unwrap();
            tokio::spawn(async move { translator.handle_incoming_calls(item).await })
        };

        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "callHierarchy/incomingCalls");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([]),
        )
        .await;

        let incoming_result = timeout(Duration::from_secs(2), incoming_handle)
            .await
            .expect("incoming calls should not hang")
            .unwrap();

        assert!(
            incoming_result.is_ok(),
            "expected success resolving the percent-encoded path, got {incoming_result:?}"
        );
    }

    #[derive(Clone, Copy)]
    enum Direction {
        Incoming,
        Outgoing,
    }

    impl Direction {
        const fn lsp_method(self) -> &'static str {
            match self {
                Self::Incoming => "callHierarchy/incomingCalls",
                Self::Outgoing => "callHierarchy/outgoingCalls",
            }
        }

        /// Name of the call's item field in the LSP response (`from`/`to`).
        const fn item_field(self) -> &'static str {
            match self {
                Self::Incoming => "from",
                Self::Outgoing => "to",
            }
        }
    }

    /// Drives `handle_incoming_calls`/`handle_outgoing_calls` against a
    /// server answering with `response`, returning the JSON-serialized result.
    async fn calls_with_response(
        direction: Direction,
        response: impl FnOnce(&str) -> serde_json::Value,
    ) -> serde_json::Value {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let queried_path = dir.path().join("queried.rs");
        fs::write(&queried_path, "fn queried() {}").unwrap();
        let queried_uri = Url::from_file_path(&queried_path).unwrap().to_string();
        let item = call_hierarchy_item_json(&queried_uri);

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                match direction {
                    Direction::Incoming => {
                        serde_json::to_value(translator.handle_incoming_calls(item).await.unwrap())
                    }
                    Direction::Outgoing => {
                        serde_json::to_value(translator.handle_outgoing_calls(item).await.unwrap())
                    }
                }
                .unwrap()
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], direction.lsp_method());

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            response(&queried_uri),
        )
        .await;

        timeout(Duration::from_secs(30), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
    }

    fn hierarchy_call_json(uri: &str, direction: Direction, ranges: usize) -> serde_json::Value {
        let range = serde_json::json!({
            "start": {"line": 0, "character": 0},
            "end": {"line": 0, "character": 1}
        });
        serde_json::json!({
            direction.item_field(): {
                "name": "f", "kind": 12, "uri": uri,
                "range": range, "selectionRange": range
            },
            "fromRanges": vec![range; ranges]
        })
    }

    fn range_count(call: &serde_json::Value) -> usize {
        call["from_ranges"].as_array().unwrap().len()
    }

    /// #487: a call and its `fromRanges` draw on one budget; a call that does
    /// not fit with all its ranges is dropped whole, never returned with
    /// fewer (or no) ranges.
    #[tokio::test]
    async fn test_handle_incoming_calls_drops_call_whole_when_ranges_exceed_budget() {
        use crate::bridge::translator::navigation::MAX_NORMALIZED_LOCATIONS;

        let result = calls_with_response(Direction::Incoming, |uri| {
            serde_json::json!([hierarchy_call_json(
                uri,
                Direction::Incoming,
                MAX_NORMALIZED_LOCATIONS
            )])
        })
        .await;

        assert_eq!(result["truncated"], true);
        assert_eq!(result["calls"].as_array().unwrap().len(), 0);
    }

    /// #487 I1: when several calls jointly exceed the cap, every returned call
    /// still carries all its ranges and the rest are dropped.
    #[tokio::test]
    async fn test_handle_incoming_calls_never_returns_call_with_ranges_cut() {
        use crate::bridge::translator::navigation::MAX_NORMALIZED_LOCATIONS;

        let ranges_per_call = MAX_NORMALIZED_LOCATIONS / 2;
        let result = calls_with_response(Direction::Incoming, |uri| {
            let call = hierarchy_call_json(uri, Direction::Incoming, ranges_per_call);
            serde_json::Value::Array(vec![call; 3])
        })
        .await;

        assert_eq!(result["truncated"], true);
        let calls = result["calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(range_count(&calls[0]), ranges_per_call);
    }

    /// #487: one call plus `MAX_NORMALIZED_LOCATIONS - 1` ranges is exactly
    /// the cap and must not be flagged.
    #[tokio::test]
    async fn test_handle_incoming_calls_exactly_at_cap_is_not_truncated() {
        use crate::bridge::translator::navigation::MAX_NORMALIZED_LOCATIONS;

        let result = calls_with_response(Direction::Incoming, |uri| {
            serde_json::json!([hierarchy_call_json(
                uri,
                Direction::Incoming,
                MAX_NORMALIZED_LOCATIONS - 1
            )])
        })
        .await;

        assert!(result.get("truncated").is_none());
        assert_eq!(
            range_count(&result["calls"][0]),
            MAX_NORMALIZED_LOCATIONS - 1
        );
    }

    /// #487: outgoing calls beyond the cap are dropped and reported.
    #[tokio::test]
    async fn test_handle_outgoing_calls_caps_item_count_and_reports_truncation() {
        use crate::bridge::translator::navigation::MAX_NORMALIZED_LOCATIONS;

        let result = calls_with_response(Direction::Outgoing, |uri| {
            let call = hierarchy_call_json(uri, Direction::Outgoing, 0);
            serde_json::Value::Array(vec![call; MAX_NORMALIZED_LOCATIONS + 500])
        })
        .await;

        assert_eq!(result["truncated"], true);
        assert_eq!(
            result["calls"].as_array().unwrap().len(),
            MAX_NORMALIZED_LOCATIONS
        );
    }

    /// #487: a response within the cap is not flagged.
    #[tokio::test]
    async fn test_handle_incoming_calls_within_cap_is_not_truncated() {
        let result = calls_with_response(Direction::Incoming, |uri| {
            serde_json::json!([hierarchy_call_json(uri, Direction::Incoming, 0)])
        })
        .await;

        assert!(result.get("truncated").is_none());
    }
}
