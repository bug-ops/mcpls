//! Selection range handler.

use lsp_types::{
    PartialResultParams, SelectionRangeParams, TextDocumentIdentifier, WorkDoneProgressParams,
};

use super::Translator;
use super::dto::{MAX_SELECTION_CHAIN, Position, SelectionRangesResult};
use super::routing::{Capability, IndexingGate};
use crate::bridge::ClientPath;
use crate::error::Result;

/// The ranges of `first` and its ancestors, innermost first, and whether the
/// chain was longer than [`MAX_SELECTION_CHAIN`].
///
/// Walks the `parent` links iteratively, so the cost is bounded by the cap
/// whatever the nesting.
fn chain(first: lsp_types::SelectionRange) -> Chain {
    let mut ranges = Vec::with_capacity(MAX_SELECTION_CHAIN);
    let mut current = Some(first);
    while let Some(node) = current {
        if ranges.len() == MAX_SELECTION_CHAIN {
            return Chain {
                ranges,
                truncated: true,
            };
        }
        ranges.push(node.range);
        current = node.parent.map(|parent| *parent);
    }
    Chain {
        ranges,
        truncated: false,
    }
}

/// A selection chain cut to [`MAX_SELECTION_CHAIN`] ranges.
#[derive(Debug, Default)]
struct Chain {
    ranges: Vec<lsp_types::Range>,
    truncated: bool,
}

impl Translator {
    /// Handle a selection range request: the ranges enclosing `position`,
    /// innermost first.
    ///
    /// One position is sent per request. The chain is returned as the server
    /// reported it, cut to [`MAX_SELECTION_CHAIN`] ranges (the outermost are
    /// dropped and `truncated` is set); an empty or `null` answer is an empty
    /// chain. Both answers are syntactic, so the request does not wait for
    /// indexing.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `selectionRangeProvider`
    /// support.
    pub async fn handle_selection_range(
        &self,
        file_path: ClientPath,
        position: Position,
    ) -> Result<SelectionRangesResult> {
        let doc = self
            .prepare_positioned_document(
                &file_path,
                Capability::SelectionRange,
                IndexingGate::NotRequired,
                &[position],
            )
            .await?;
        let (server_id, client, uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let response_uri = uri.clone();
        let lsp_position = ctx.to_lsp(uri, position).await;

        let params = SelectionRangeParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            positions: vec![lsp_position],
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::SelectionRangeRequest>(params, client.request_timeout())
            .await?;

        let Chain {
            ranges: lsp_ranges,
            truncated,
        } = response
            .and_then(|chains| chains.into_iter().next())
            .map_or_default(chain);
        let mut ranges = Vec::with_capacity(lsp_ranges.len());
        for range in lsp_ranges {
            ranges.push(ctx.normalize_range(&response_uri, range).await);
        }

        Ok(SelectionRangesResult {
            ranges,
            truncated,
            positions_degraded: ctx.positions_degraded(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;
    use std::{assert_matches, fs};

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::time::timeout;

    use super::*;
    use crate::bridge::translator::testing::*;
    use crate::config::ServerId;
    use crate::error::Error;
    use crate::test_lsp::client_path;

    fn caps() -> lsp_types::ServerCapabilities {
        lsp_types::ServerCapabilities {
            selection_range_provider: Some(lsp_types::SelectionRangeProvider::Bool(true)),
            ..Default::default()
        }
    }

    fn range_json(start: u32, end: u32) -> serde_json::Value {
        serde_json::json!({
            "start": {"line": 0, "character": start},
            "end": {"line": 0, "character": end}
        })
    }

    fn nested(widths: &[(u32, u32)]) -> serde_json::Value {
        widths
            .iter()
            .rev()
            .fold(serde_json::Value::Null, |parent, (s, e)| {
                let mut node = serde_json::json!({ "range": range_json(*s, *e) });
                if !parent.is_null() {
                    node["parent"] = parent;
                }
                node
            })
    }

    async fn selection_with_response(
        source: &str,
        response: serde_json::Value,
        encoding: Option<lsp_types::PositionEncodingKind>,
    ) -> (serde_json::Value, SelectionRangesResult) {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let (translator, mut server) = encoding.map_or_else(
            || translator_with_capabilities(&dir, &server_id, caps()),
            |encoding| {
                translator_with_capabilities_and_encoding(&dir, &server_id, caps(), encoding)
            },
        );
        let path = dir.path().join("a.rs");
        fs::write(&path, source).unwrap();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().into_owned();
            tokio::spawn(async move {
                translator
                    .handle_selection_range(client_path(path), pos(1, 5))
                    .await
            })
        };
        let mut wire = BufReader::new(&mut server.write_stdout);
        assert_eq!(
            read_framed_message(&mut wire).await["method"],
            "textDocument/didOpen"
        );
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/selectionRange");
        write_response(&mut server.read_half_stdin, &request["id"], response).await;

        let result = timeout(Duration::from_secs(30), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();
        (request, result)
    }

    #[tokio::test]
    async fn returns_the_chain_innermost_first_and_sends_one_position() {
        let (request, result) = selection_with_response(
            "f(a + b);",
            serde_json::json!([nested(&[(2, 3), (2, 7), (0, 8), (0, 9)])]),
            None,
        )
        .await;

        assert_eq!(
            request["params"]["positions"],
            serde_json::json!([{"line": 0, "character": 4}])
        );
        let spans: Vec<_> = result
            .ranges
            .iter()
            .map(|r| (r.start.character, r.end.character))
            .collect();
        assert_eq!(spans, [(3, 4), (3, 8), (1, 9), (1, 10)]);
        assert!(!result.truncated);
        assert_eq!(result.positions_degraded, None);
        let wire = serde_json::to_value(&result).unwrap();
        assert!(wire.get("truncated").is_none());
    }

    #[tokio::test]
    async fn keeps_equal_consecutive_ranges_as_reported() {
        let (_, result) = selection_with_response(
            "f(a);",
            serde_json::json!([nested(&[(2, 3), (2, 3), (0, 4)])]),
            None,
        )
        .await;
        assert_eq!(result.ranges.len(), 3);
    }

    #[tokio::test]
    async fn null_and_empty_answers_are_an_empty_chain() {
        for response in [serde_json::Value::Null, serde_json::json!([])] {
            let (_, result) = selection_with_response("f(a);", response, None).await;
            assert!(result.ranges.is_empty() && !result.truncated);
        }
    }

    #[tokio::test]
    async fn a_chain_longer_than_the_cap_keeps_the_innermost_ranges() {
        let widths: Vec<(u32, u32)> = (0..100).map(|i| (50 - i / 2, 60 + i)).collect();
        let (_, result) =
            selection_with_response("x", serde_json::json!([nested(&widths)]), None).await;

        assert_eq!(result.ranges.len(), MAX_SELECTION_CHAIN);
        assert!(result.truncated);
        assert_eq!(result.ranges[0].end.character, 61);
        assert_eq!(
            result.ranges[MAX_SELECTION_CHAIN - 1].end.character,
            60 + u32::try_from(MAX_SELECTION_CHAIN).unwrap()
        );
        assert_eq!(serde_json::to_value(&result).unwrap()["truncated"], true);
    }

    #[tokio::test]
    async fn a_chain_of_exactly_the_cap_is_not_truncated_and_one_more_is() {
        let widths = |len: u32| -> Vec<(u32, u32)> { (0..len).map(|i| (50 - i, 60 + i)).collect() };
        let cap = u32::try_from(MAX_SELECTION_CHAIN).unwrap();

        let (_, exact) =
            selection_with_response("x", serde_json::json!([nested(&widths(cap))]), None).await;
        assert_eq!(exact.ranges.len(), MAX_SELECTION_CHAIN);
        assert!(!exact.truncated);

        let (_, over) =
            selection_with_response("x", serde_json::json!([nested(&widths(cap + 1))]), None).await;
        assert_eq!(over.ranges.len(), MAX_SELECTION_CHAIN);
        assert!(over.truncated);
    }

    #[tokio::test]
    async fn only_the_first_chain_of_the_response_is_used() {
        let (_, result) = selection_with_response(
            "f(a);",
            serde_json::json!([nested(&[(2, 3)]), nested(&[(0, 4), (0, 5)])]),
            None,
        )
        .await;
        assert_eq!(result.ranges.len(), 1);
    }

    #[tokio::test]
    async fn utf8_server_ranges_and_position_are_converted() {
        // "é" is 2 UTF-8 bytes: byte offset 5 is character 4 (1-based 5).
        let (request, result) = selection_with_response(
            "é = a;",
            serde_json::json!([nested(&[(5, 6), (0, 7)])]),
            Some(lsp_types::PositionEncodingKind::UTF8),
        )
        .await;
        assert_eq!(request["params"]["positions"][0]["character"], 5);
        assert_eq!(result.ranges[0].start.character, 5);
        assert_eq!(result.ranges[0].end.character, 6);
        assert_eq!(result.positions_degraded, None);
    }

    #[tokio::test]
    async fn rejects_missing_capability() {
        let dir = TempDir::new().unwrap();
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &ServerId::from_static("rust"),
            lsp_types::ServerCapabilities::default(),
        );
        let path = dir.path().join("a.rs");
        fs::write(&path, "f(a);").unwrap();
        let result = translator
            .handle_selection_range(client_path(path.to_string_lossy().into_owned()), pos(1, 1))
            .await;
        assert_matches!(result, Err(Error::CapabilityNotSupported { .. }));
    }

    #[test]
    fn chain_is_bounded_whatever_the_nesting() {
        let mut node = lsp_types::SelectionRange {
            range: lsp_types::Range::default(),
            parent: None,
        };
        for _ in 0..120 {
            node = lsp_types::SelectionRange {
                range: lsp_types::Range::default(),
                parent: Some(Box::new(node)),
            };
        }
        let chain = chain(node);
        assert_eq!(chain.ranges.len(), MAX_SELECTION_CHAIN);
        assert!(chain.truncated);
    }
}
