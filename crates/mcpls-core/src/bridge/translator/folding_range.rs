//! Folding range handler.

use lsp_types::{
    FoldingRangeParams, PartialResultParams, TextDocumentIdentifier, WorkDoneProgressParams,
};
use tracing::debug;

use super::Translator;
use super::dto::{FoldingKind, FoldingKindFilter, FoldingRangesResult, FoldingRegion};
use super::encoding_ctx::EncodingCtx;
use super::navigation::ItemBudget;
use super::routing::{Capability, IndexingGate};
use crate::bridge::ClientPath;
use crate::error::Result;
use crate::util::escape_control_owned;

/// The closed kind of an LSP folding range kind; a missing or custom kind is
/// [`FoldingKind::Unspecified`].
const fn folding_kind(kind: Option<&lsp_types::FoldingRangeKind>) -> FoldingKind {
    match kind {
        Some(lsp_types::FoldingRangeKind::Comment) => FoldingKind::Comment,
        Some(lsp_types::FoldingRangeKind::Imports) => FoldingKind::Imports,
        Some(lsp_types::FoldingRangeKind::Region) => FoldingKind::Region,
        Some(lsp_types::FoldingRangeKind::Custom(_)) | None => FoldingKind::Unspecified,
    }
}

/// The regions of `ranges` that are well formed and pass `filter`, ordered by
/// ascending start line then descending end line.
fn select(
    ranges: Vec<lsp_types::FoldingRange>,
    filter: FoldingKindFilter,
) -> Vec<(FoldingKind, lsp_types::FoldingRange)> {
    let mut selected: Vec<_> = ranges
        .into_iter()
        .filter(|range| {
            let well_formed = range.end_line >= range.start_line;
            if !well_formed {
                debug!(
                    start_line = range.start_line,
                    end_line = range.end_line,
                    "dropping a folding range that ends before it starts"
                );
            }
            well_formed
        })
        .map(|range| (folding_kind(range.kind.as_ref()), range))
        .filter(|(kind, _)| filter.admits(*kind))
        .collect();
    selected.sort_by_key(|(_, range)| (range.start_line, std::cmp::Reverse(range.end_line)));
    selected
}

async fn character(
    ctx: &EncodingCtx,
    uri: &lsp_types::Uri,
    line: u32,
    character: Option<u32>,
) -> Option<u32> {
    let character = character?;
    Some(
        ctx.to_mcp(uri, lsp_types::Position { line, character })
            .await
            .character,
    )
}

async fn region(
    ctx: &EncodingCtx,
    uri: &lsp_types::Uri,
    kind: FoldingKind,
    range: lsp_types::FoldingRange,
) -> FoldingRegion {
    FoldingRegion {
        start_line: range.start_line.saturating_add(1),
        end_line: range.end_line.saturating_add(1),
        start_character: character(ctx, uri, range.start_line, range.start_character).await,
        end_character: character(ctx, uri, range.end_line, range.end_character).await,
        kind,
        collapsed_text: range.collapsed_text.map(escape_control_owned),
    }
}

impl Translator {
    /// Handle a folding range request: the foldable regions of a file.
    ///
    /// Regions that end before they start are dropped, the rest are filtered
    /// by `filter`, ordered by ascending start line then descending end line,
    /// and capped (`truncated` is set when more remain). A kind the server
    /// does not name, or names with a custom value, is
    /// [`FoldingKind::Unspecified`]. Both answers are syntactic, so the
    /// request does not wait for indexing.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `foldingRangeProvider`
    /// support.
    pub async fn handle_folding_range(
        &self,
        file_path: ClientPath,
        filter: FoldingKindFilter,
    ) -> Result<FoldingRangesResult> {
        let doc = self
            .prepare_gated_document(
                &file_path,
                Capability::FoldingRange,
                IndexingGate::NotRequired,
            )
            .await?;
        let (server_id, client, uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let response_uri = uri.clone();

        let params = FoldingRangeParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::FoldingRangeRequest>(params, client.request_timeout())
            .await?;

        let mut budget = ItemBudget::new();
        let admitted = budget.admit(select(response.unwrap_or_default(), filter));
        let mut regions = Vec::with_capacity(admitted.len());
        for (kind, range) in admitted {
            regions.push(region(&ctx, &response_uri, kind, range).await);
        }

        Ok(FoldingRangesResult {
            regions,
            truncated: budget.truncated(),
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
            folding_range_provider: Some(lsp_types::FoldingRangeProvider::Bool(true)),
            ..Default::default()
        }
    }

    fn fold(start: u32, end: u32, kind: Option<&str>) -> serde_json::Value {
        let mut value = serde_json::json!({"startLine": start, "endLine": end});
        if let Some(kind) = kind {
            value["kind"] = kind.into();
        }
        value
    }

    async fn folding_with_response(
        source: &str,
        filter: FoldingKindFilter,
        response: serde_json::Value,
        encoding: Option<lsp_types::PositionEncodingKind>,
    ) -> FoldingRangesResult {
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
                    .handle_folding_range(client_path(path), filter)
                    .await
            })
        };
        let mut wire = BufReader::new(&mut server.write_stdout);
        assert_eq!(
            read_framed_message(&mut wire).await["method"],
            "textDocument/didOpen"
        );
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/foldingRange");
        write_response(&mut server.read_half_stdin, &request["id"], response).await;

        timeout(Duration::from_secs(30), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap()
    }

    #[test]
    fn maps_kinds_to_a_closed_set() {
        use lsp_types::FoldingRangeKind as Lsp;

        assert_eq!(folding_kind(Some(&Lsp::Comment)), FoldingKind::Comment);
        assert_eq!(folding_kind(Some(&Lsp::Imports)), FoldingKind::Imports);
        assert_eq!(folding_kind(Some(&Lsp::Region)), FoldingKind::Region);
        assert_eq!(
            folding_kind(Some(&Lsp::Custom("fn".into()))),
            FoldingKind::Unspecified
        );
        assert_eq!(folding_kind(None), FoldingKind::Unspecified);
    }

    #[tokio::test]
    async fn orders_regions_by_start_then_longest_and_converts_lines() {
        let result = folding_with_response(
            "a\nb\nc\nd\ne\nf\n",
            FoldingKindFilter::All,
            serde_json::json!([
                fold(3, 4, None),
                fold(0, 2, Some("imports")),
                fold(0, 5, Some("region")),
                fold(3, 3, Some("comment")),
            ]),
            None,
        )
        .await;

        let spans: Vec<_> = result
            .regions
            .iter()
            .map(|r| (r.start_line, r.end_line, r.kind))
            .collect();
        assert_eq!(
            spans,
            [
                (1, 6, FoldingKind::Region),
                (1, 3, FoldingKind::Imports),
                (4, 5, FoldingKind::Unspecified),
                (4, 4, FoldingKind::Comment),
            ]
        );
        let wire = serde_json::to_value(&result).unwrap();
        assert_eq!(wire["regions"][0]["kind"], "region");
        assert!(wire["regions"][0].get("start_character").is_none());
        assert!(wire.get("truncated").is_none());
    }

    #[tokio::test]
    async fn the_kind_filter_selects_only_that_kind_and_never_unspecified() {
        let response = serde_json::json!([
            fold(0, 1, Some("imports")),
            fold(2, 3, None),
            fold(4, 5, Some("custom-kind")),
            fold(6, 7, Some("comment")),
        ]);
        for (filter, expected) in [
            (FoldingKindFilter::Imports, 1),
            (FoldingKindFilter::Comment, 1),
            (FoldingKindFilter::Region, 0),
            (FoldingKindFilter::All, 4),
        ] {
            let result =
                folding_with_response("x\n".repeat(8).as_str(), filter, response.clone(), None)
                    .await;
            assert_eq!(result.regions.len(), expected, "{filter:?}");
        }
    }

    #[tokio::test]
    async fn drops_regions_that_end_before_they_start() {
        let result = folding_with_response(
            "a\nb\nc\n",
            FoldingKindFilter::All,
            serde_json::json!([fold(2, 1, None), fold(0, 1, None)]),
            None,
        )
        .await;
        assert_eq!(result.regions.len(), 1);
        assert_eq!(result.regions[0].start_line, 1);
    }

    #[tokio::test]
    async fn the_filter_applies_before_the_bound() {
        let mut regions: Vec<_> = (0..10_050).map(|i| fold(i, i + 1, None)).collect();
        regions.push(fold(20_000, 20_001, Some("imports")));
        let result = folding_with_response(
            "x",
            FoldingKindFilter::Imports,
            serde_json::Value::Array(regions.clone()),
            None,
        )
        .await;
        assert_eq!(result.regions.len(), 1);
        assert!(!result.truncated);

        let all = folding_with_response(
            "x",
            FoldingKindFilter::All,
            serde_json::Value::Array(regions),
            None,
        )
        .await;
        assert!(all.truncated);
        assert_eq!(all.regions.len(), 10_000);
        assert_eq!(all.regions[0].start_line, 1);
    }

    #[tokio::test]
    async fn null_and_empty_answers_are_an_empty_result() {
        for response in [serde_json::Value::Null, serde_json::json!([])] {
            let result = folding_with_response("x", FoldingKindFilter::All, response, None).await;
            assert!(result.regions.is_empty() && !result.truncated);
        }
    }

    #[tokio::test]
    async fn characters_are_independent_converted_and_never_invented() {
        // "é" is 2 UTF-8 bytes: byte offset 5 is character 4 (1-based 5).
        let result = folding_with_response(
            "é = {\n  x\n}\n",
            FoldingKindFilter::All,
            serde_json::json!([{
                "startLine": 0, "startCharacter": 5,
                "endLine": 2, "kind": "region", "collapsedText": "{...}"
            }]),
            Some(lsp_types::PositionEncodingKind::UTF8),
        )
        .await;

        let region = &result.regions[0];
        assert_eq!(region.start_character, Some(5));
        assert_eq!(region.end_character, None);
        assert_eq!(region.collapsed_text.as_deref(), Some("{...}"));
        assert_eq!(result.positions_degraded, None);
    }

    #[tokio::test]
    async fn collapsed_text_is_escaped() {
        let result = folding_with_response(
            "a\nb\n",
            FoldingKindFilter::All,
            serde_json::json!([{
                "startLine": 0, "endLine": 1, "collapsedText": "x\u{1b}[31my"
            }]),
            None,
        )
        .await;
        assert_eq!(
            result.regions[0].collapsed_text.as_deref(),
            Some("x\\u{1b}[31my")
        );
        assert_ne!(
            result.regions[0].collapsed_text.as_deref(),
            Some("x\u{1b}[31my")
        );
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
        fs::write(&path, "x").unwrap();
        let result = translator
            .handle_folding_range(
                client_path(path.to_string_lossy().into_owned()),
                FoldingKindFilter::All,
            )
            .await;
        assert_matches!(result, Err(Error::CapabilityNotSupported { .. }));
    }
}
