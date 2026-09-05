//! Call hierarchy prepare/incoming/outgoing handlers.

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams,
    CallHierarchyPrepareParams as LspCallHierarchyPrepareParams, PartialResultParams,
    TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
};

use super::Translator;
use super::dto::{
    CallHierarchyItemResult, CallHierarchyPrepareResult, IncomingCall, IncomingCallsResult,
    OutgoingCall, OutgoingCallsResult, Position,
};
use super::encoding_ctx::EncodingCtx;
use super::routing::MAX_POSITION_VALUE;
use crate::config::ToolKind;
use crate::error::{Error, Result};

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

    let uri = mcp.uri.parse::<lsp_types::Uri>().map_err(|e| {
        Error::InvalidToolParams(format!("Invalid URI in call hierarchy item: {e}"))
    })?;

    Ok(ParsedCallHierarchyItem { uri, mcp })
}

/// Convert a parsed MCP call hierarchy item (1-based coordinates) into a
/// `lsp_types::CallHierarchyItem` (0-based, in `ctx`'s negotiated encoding).
async fn call_hierarchy_item_to_lsp(
    parsed: ParsedCallHierarchyItem,
    ctx: &EncodingCtx,
) -> CallHierarchyItem {
    let ParsedCallHierarchyItem { uri, mcp } = parsed;

    // Round-trip via serde: `convert_call_hierarchy_item` stored the kind as a u32
    // by serialising `SymbolKind`; we reverse this to reconstruct the same value.
    let kind: lsp_types::SymbolKind = serde_json::from_value(serde_json::json!(mcp.kind))
        .unwrap_or(lsp_types::SymbolKind::FUNCTION);
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
    let range = ctx.normalize_range(&item.uri, item.range).await;
    let selection_range = ctx.normalize_range(&item.uri, item.selection_range).await;

    CallHierarchyItemResult {
        name: item.name,
        kind: serde_json::to_value(item.kind)
            .ok()
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
        detail: item.detail,
        uri: item.uri.to_string(),
        range,
        selection_range,
        data: item.data,
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

        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::CallHierarchy,
                "callHierarchyProvider",
                call_hierarchy_provider_supported,
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = LspCallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response: Option<Vec<CallHierarchyItem>> = client
            .request(
                "textDocument/prepareCallHierarchy",
                params,
                client.request_timeout(),
            )
            .await?;

        // Pre-allocate and build result
        let lsp_items = response.unwrap_or_default();
        let mut items = Vec::with_capacity(lsp_items.len());
        for item in lsp_items {
            items.push(convert_call_hierarchy_item(item, &ctx).await);
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
        // Deserialize as our own type (1-based coords).
        let parsed = parse_mcp_call_hierarchy_item(item)?;

        // Parse and validate the URI. Resolved with the same ToolKind as
        // `handle_call_hierarchy_prepare` -- the opaque item this call
        // receives is only meaningful to the server that produced it, and
        // that server is guaranteed to be the same one `prepare` synced the
        // document to since both resolve via the same (language, tool) route.
        let path = self.parse_file_uri(&parsed.uri)?;
        let (server_id, client) = self
            .resolve_client_for_file(&path, ToolKind::CallHierarchy)
            .await?;
        self.require_capability(
            &server_id,
            "callHierarchyProvider",
            call_hierarchy_provider_supported,
        )?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_item = call_hierarchy_item_to_lsp(parsed, &ctx).await;

        let params = CallHierarchyIncomingCallsParams {
            item: lsp_item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<Vec<CallHierarchyIncomingCall>> = client
            .request(
                "callHierarchy/incomingCalls",
                params,
                client.request_timeout(),
            )
            .await?;

        // Pre-allocate and build result
        let lsp_calls = response.unwrap_or_default();
        let mut calls = Vec::with_capacity(lsp_calls.len());

        for call in lsp_calls {
            // Per the LSP spec, `fromRanges` are ranges within the *caller's*
            // document (`call.from.uri`), not the queried item's document.
            let from_uri = call.from.uri.clone();
            let from_ranges = {
                let mut ranges = Vec::with_capacity(call.from_ranges.len());
                for range in call.from_ranges {
                    ranges.push(ctx.normalize_range(&from_uri, range).await);
                }
                ranges
            };

            calls.push(IncomingCall {
                from: convert_call_hierarchy_item(call.from, &ctx).await,
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
        // Deserialize as our own type (1-based coords).
        let parsed = parse_mcp_call_hierarchy_item(item)?;

        // Parse and validate the URI. Same ToolKind/route as `prepare` and
        // `handle_incoming_calls` -- see that function's comment.
        let path = self.parse_file_uri(&parsed.uri)?;
        let (server_id, client) = self
            .resolve_client_for_file(&path, ToolKind::CallHierarchy)
            .await?;
        self.require_capability(
            &server_id,
            "callHierarchyProvider",
            call_hierarchy_provider_supported,
        )?;
        let ctx = self.encoding_ctx(&server_id);
        // Per the LSP spec, an outgoing call's `fromRanges` are ranges within
        // the *queried* item's own document, not the callee's (`call.to.uri`).
        let source_uri = parsed.uri.clone();
        let lsp_item = call_hierarchy_item_to_lsp(parsed, &ctx).await;

        let params = CallHierarchyOutgoingCallsParams {
            item: lsp_item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<Vec<CallHierarchyOutgoingCall>> = client
            .request(
                "callHierarchy/outgoingCalls",
                params,
                client.request_timeout(),
            )
            .await?;

        // Pre-allocate and build result
        let lsp_calls = response.unwrap_or_default();
        let mut calls = Vec::with_capacity(lsp_calls.len());

        for call in lsp_calls {
            let from_ranges = {
                let mut ranges = Vec::with_capacity(call.from_ranges.len());
                for range in call.from_ranges {
                    ranges.push(ctx.normalize_range(&source_uri, range).await);
                }
                ranges
            };

            calls.push(OutgoingCall {
                to: convert_call_hierarchy_item(call.to, &ctx).await,
                from_ranges,
            });
        }

        Ok(OutgoingCallsResult { calls })
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
    use tokio::time::timeout;
    use url::Url;

    use super::*;
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

    #[tokio::test]
    async fn test_convert_call_hierarchy_item_kind_is_numeric() {
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
        let result = convert_call_hierarchy_item(item, &test_ctx()).await;
        // SymbolKind::FUNCTION is LSP integer 12
        assert_eq!(result.kind, 12u32);
        assert_eq!(result.name, "my_fn");
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
            call_hierarchy_provider: Some(lsp_types::CallHierarchyServerCapability::Simple(true)),
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
        };

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let item = serde_json::to_value(item).unwrap();
            tokio::spawn(async move { translator.handle_incoming_calls(item).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
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
    }

    /// Per the LSP spec, an outgoing call's `fromRanges` are ranges within
    /// the *queried* item's own document, not the callee's (`call.to.uri`) --
    /// the inverse directional convention from incoming calls, tested above.
    #[tokio::test]
    async fn test_handle_outgoing_calls_from_ranges_convert_against_queried_uri() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            call_hierarchy_provider: Some(lsp_types::CallHierarchyServerCapability::Simple(true)),
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
        };

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let item = serde_json::to_value(item).unwrap();
            tokio::spawn(async move { translator.handle_outgoing_calls(item).await })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
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
}
