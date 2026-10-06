//! Document highlights handler.

use lsp_types::{
    DocumentHighlightParams, PartialResultParams, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams,
};

use super::Translator;
use super::dto::{
    DocumentHighlightEntry, DocumentHighlightKind, DocumentHighlightsResult, Position,
};
use super::navigation::ItemBudget;
use super::routing::{Capability, IndexingGate};
use crate::bridge::ClientPath;
use crate::error::Result;

/// The MCP kind of an LSP highlight kind. A server that omits the kind, or
/// sends a custom value outside the three defined ones, means the LSP
/// default: a textual occurrence.
const fn highlight_kind(kind: Option<lsp_types::DocumentHighlightKind>) -> DocumentHighlightKind {
    match kind {
        Some(lsp_types::DocumentHighlightKind::Read) => DocumentHighlightKind::Read,
        Some(lsp_types::DocumentHighlightKind::Write) => DocumentHighlightKind::Write,
        Some(
            lsp_types::DocumentHighlightKind::Text | lsp_types::DocumentHighlightKind::Custom(_),
        )
        | None => DocumentHighlightKind::Text,
    }
}

impl Translator {
    /// Handle a document highlights request.
    ///
    /// # Errors
    ///
    /// Returns an error if the position is invalid, the LSP request fails,
    /// the file cannot be opened, or the routed server does not advertise
    /// `documentHighlightProvider` support.
    pub async fn handle_document_highlights(
        &self,
        file_path: ClientPath,
        position: Position,
    ) -> Result<DocumentHighlightsResult> {
        let doc = self
            .prepare_positioned_document(
                &file_path,
                Capability::DocumentHighlights,
                IndexingGate::NotRequired,
                &[position],
            )
            .await?;
        let (server_id, client, uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let response_uri = uri.clone();
        let lsp_position = ctx.to_lsp(uri, position).await;

        let params = DocumentHighlightParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::DocumentHighlightRequest>(params, client.request_timeout())
            .await?;

        let mut budget = ItemBudget::new();
        let lsp_highlights = budget.admit(response.unwrap_or_default());
        let mut highlights = Vec::with_capacity(lsp_highlights.len());
        for highlight in lsp_highlights {
            highlights.push(DocumentHighlightEntry {
                range: ctx.normalize_range(&response_uri, highlight.range).await,
                kind: highlight_kind(highlight.kind),
            });
        }

        Ok(DocumentHighlightsResult {
            highlights,
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
            document_highlight_provider: Some(lsp_types::DocumentHighlightProvider::Bool(true)),
            ..Default::default()
        }
    }

    fn highlight_json(line: u32, start: u32, end: u32, kind: Option<u32>) -> serde_json::Value {
        let mut value = serde_json::json!({
            "range": {
                "start": {"line": line, "character": start},
                "end": {"line": line, "character": end}
            }
        });
        if let Some(kind) = kind {
            value["kind"] = kind.into();
        }
        value
    }

    async fn highlights_with_response(
        source: &str,
        response: serde_json::Value,
        encoding: Option<lsp_types::PositionEncodingKind>,
    ) -> (serde_json::Value, DocumentHighlightsResult) {
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
                    .handle_document_highlights(client_path(path), pos(1, 4))
                    .await
            })
        };
        let mut wire = BufReader::new(&mut server.write_stdout);
        assert_eq!(
            read_framed_message(&mut wire).await["method"],
            "textDocument/didOpen"
        );
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/documentHighlight");
        write_response(&mut server.read_half_stdin, &request["id"], response).await;

        let result = timeout(Duration::from_secs(30), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();
        (request, result)
    }

    #[tokio::test]
    async fn maps_kinds_and_defaults_an_omitted_kind_to_text() {
        let (request, result) = highlights_with_response(
            "let a = 1; a; a",
            serde_json::json!([
                highlight_json(0, 4, 5, Some(3)),
                highlight_json(0, 11, 12, Some(2)),
                highlight_json(0, 14, 15, None),
                highlight_json(0, 0, 1, Some(42)),
            ]),
            None,
        )
        .await;
        assert_eq!(request["params"]["position"]["character"], 3);
        let kinds: Vec<_> = result.highlights.iter().map(|h| h.kind).collect();
        assert_eq!(
            kinds,
            [
                DocumentHighlightKind::Write,
                DocumentHighlightKind::Read,
                DocumentHighlightKind::Text,
                DocumentHighlightKind::Text,
            ]
        );
        assert_eq!(result.highlights[0].range.start.character, 5);
        let wire = serde_json::to_value(&result).unwrap();
        assert_eq!(wire["highlights"][0]["kind"], "write");
        assert!(wire.get("truncated").is_none());
    }

    #[tokio::test]
    async fn null_response_is_an_empty_result() {
        let (_, result) =
            highlights_with_response("let a = 1;", serde_json::Value::Null, None).await;
        assert!(result.highlights.is_empty());
    }

    #[tokio::test]
    async fn utf8_server_ranges_are_converted_to_mcp_columns() {
        // "é" is 2 UTF-8 bytes but 1 character: byte offset 5 is character 4.
        let (_, result) = highlights_with_response(
            "let é = 1;",
            serde_json::json!([highlight_json(0, 4, 6, Some(1))]),
            Some(lsp_types::PositionEncodingKind::UTF8),
        )
        .await;
        assert_eq!(result.highlights[0].range.start.character, 5);
        assert_eq!(result.highlights[0].range.end.character, 6);
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
        fs::write(&path, "let a = 1;").unwrap();
        let result = translator
            .handle_document_highlights(client_path(path.to_string_lossy().into_owned()), pos(1, 1))
            .await;
        assert_matches!(result, Err(Error::CapabilityNotSupported { .. }));
    }

    #[test]
    fn highlight_kind_defaults_to_text() {
        assert_eq!(highlight_kind(None), DocumentHighlightKind::Text);
        assert_eq!(
            highlight_kind(Some(lsp_types::DocumentHighlightKind::Custom(9))),
            DocumentHighlightKind::Text
        );
    }
}
