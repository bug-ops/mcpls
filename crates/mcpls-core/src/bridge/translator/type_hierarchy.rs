//! Type hierarchy prepare/supertypes/subtypes handlers.

use lsp_types::{
    PartialResultParams, TextDocumentIdentifier, TextDocumentPositionParams, TypeHierarchyItem,
    TypeHierarchyPrepareParams, TypeHierarchySubtypesParams, TypeHierarchySupertypesParams,
    WorkDoneProgressParams,
};

use super::Translator;
use super::dto::{CheckedHierarchyItem, Position, TypeHierarchyResult};
use super::encoding_ctx::EncodingCtx;
use super::hierarchy::{hierarchy_item_to_lsp, hierarchy_item_to_mcp};
use super::navigation::ItemBudget;
use super::routing::{Capability, IndexingGate};
use crate::bridge::ClientPath;
use crate::error::Result;

/// Which way a type hierarchy walk goes from the queried item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalkDirection {
    Supertypes,
    Subtypes,
}

/// Convert the admitted LSP items into the MCP result, in 1-based coordinates.
async fn convert_items(
    response: Option<Vec<TypeHierarchyItem>>,
    ctx: &EncodingCtx,
) -> TypeHierarchyResult {
    let mut budget = ItemBudget::new();
    let lsp_items = budget.admit(response.unwrap_or_default());
    let mut items = Vec::with_capacity(lsp_items.len());
    for item in lsp_items {
        items.push(hierarchy_item_to_mcp(item, ctx).await);
    }
    TypeHierarchyResult {
        items,
        truncated: budget.truncated(),
        positions_degraded: ctx.positions_degraded(),
    }
}

impl Translator {
    /// Handle a type hierarchy prepare request.
    ///
    /// # Errors
    ///
    /// Returns an error if the position is invalid, the LSP request fails,
    /// the file cannot be opened, or the routed server does not advertise
    /// `typeHierarchyProvider` support.
    pub async fn handle_type_hierarchy_prepare(
        &self,
        file_path: ClientPath,
        position: Position,
    ) -> Result<TypeHierarchyResult> {
        let doc = self
            .prepare_positioned_document(
                &file_path,
                Capability::TypeHierarchy,
                IndexingGate::NotRequired,
                &[position],
            )
            .await?;
        let (server_id, client, uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let lsp_position = ctx.to_lsp(uri, position).await;

        let params = TypeHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::TypeHierarchyPrepareRequest>(
                params,
                client.request_timeout(),
            )
            .await?;

        Ok(convert_items(response, &ctx).await)
    }

    /// Handle a supertypes request for an item from a previous prepare or walk.
    ///
    /// Opens the item's own file on the routed server as a side effect, like
    /// `handle_incoming_calls`.
    ///
    /// # Errors
    ///
    /// Returns an error if the item's URI is not a workspace file, the LSP
    /// request fails, the routed server does not advertise
    /// `typeHierarchyProvider` support, or the server is still indexing the
    /// workspace after `INDEXING_READY_TIMEOUT`.
    pub async fn handle_supertypes(
        &self,
        item: CheckedHierarchyItem,
    ) -> Result<TypeHierarchyResult> {
        self.walk_type_hierarchy(item, WalkDirection::Supertypes)
            .await
    }

    /// Handle a subtypes request for an item from a previous prepare or walk.
    ///
    /// # Errors
    ///
    /// See [`Self::handle_supertypes`].
    pub async fn handle_subtypes(&self, item: CheckedHierarchyItem) -> Result<TypeHierarchyResult> {
        self.walk_type_hierarchy(item, WalkDirection::Subtypes)
            .await
    }

    async fn walk_type_hierarchy(
        &self,
        item: CheckedHierarchyItem,
        direction: WalkDirection,
    ) -> Result<TypeHierarchyResult> {
        let uri = lsp_types::Uri::from(item.uri());
        let path = self.parse_file_uri(&uri).await?;
        let doc = self
            .prepare_gated_document_for_path(
                &path,
                Capability::TypeHierarchy,
                IndexingGate::Required,
            )
            .await?;
        let (server_id, client) = (doc.server_id(), doc.client());
        let ctx = self.encoding_ctx(server_id);
        let item: TypeHierarchyItem = hierarchy_item_to_lsp(item, doc.uri().clone(), &ctx).await;

        let response = match direction {
            WalkDirection::Supertypes => {
                client
                    .request_typed::<lsp_types::TypeHierarchySupertypesRequest>(
                        TypeHierarchySupertypesParams {
                            item,
                            work_done_progress_params: WorkDoneProgressParams::default(),
                            partial_result_params: PartialResultParams::default(),
                        },
                        client.request_timeout(),
                    )
                    .await?
            }
            WalkDirection::Subtypes => {
                client
                    .request_typed::<lsp_types::TypeHierarchySubtypesRequest>(
                        TypeHierarchySubtypesParams {
                            item,
                            work_done_progress_params: WorkDoneProgressParams::default(),
                            partial_result_params: PartialResultParams::default(),
                        },
                        client.request_timeout(),
                    )
                    .await?
            }
        };

        Ok(convert_items(response, &ctx).await)
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
    use url::Url;

    use super::*;
    use crate::bridge::translator::dto::{HierarchyItem, Position2D, Range};
    use crate::bridge::translator::navigation::MAX_NORMALIZED_LOCATIONS;
    use crate::bridge::translator::testing::*;
    use crate::config::ServerId;
    use crate::error::Error;
    use crate::test_lsp::client_path;

    fn caps() -> lsp_types::ServerCapabilities {
        lsp_types::ServerCapabilities {
            type_hierarchy_provider: Some(lsp_types::TypeHierarchyProvider::Bool(true)),
            ..Default::default()
        }
    }

    fn lsp_item_json(uri: &str, name: &str) -> serde_json::Value {
        let range = serde_json::json!({
            "start": {"line": 0, "character": 0},
            "end": {"line": 0, "character": 4}
        });
        serde_json::json!({
            "name": name, "kind": 5, "uri": uri,
            "range": range, "selectionRange": range,
            "data": {"id": name}
        })
    }

    fn checked_item(uri: &str) -> CheckedHierarchyItem {
        CheckedHierarchyItem::from_client(item_dto(uri)).unwrap()
    }

    fn item_dto(uri: &str) -> HierarchyItem {
        let at = |line, character| Position2D { line, character };
        HierarchyItem {
            name: "Derived".to_string(),
            kind: 5,
            detail: None,
            uri: uri.to_string(),
            range: Range {
                start: at(1, 1),
                end: at(1, 5),
            },
            selection_range: Range {
                start: at(1, 1),
                end: at(1, 5),
            },
            data: Some(serde_json::json!({"id": "Derived"})),
            out_of_workspace: false,
        }
    }

    #[tokio::test]
    async fn prepare_returns_one_based_items() {
        let dir = TempDir::new().unwrap();
        let (translator, mut server) =
            translator_with_capabilities(&dir, &ServerId::from("rust"), caps());
        let path = dir.path().join("a.rs");
        fs::write(&path, "struct Base;").unwrap();
        let uri = Url::from_file_path(&path).unwrap().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().into_owned();
            tokio::spawn(async move {
                translator
                    .handle_type_hierarchy_prepare(client_path(path), pos(1, 8))
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        assert_eq!(
            read_framed_message(&mut wire).await["method"],
            "textDocument/didOpen"
        );
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/prepareTypeHierarchy");
        assert_eq!(request["params"]["position"]["line"], 0);
        assert_eq!(request["params"]["position"]["character"], 7);
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([lsp_item_json(&uri, "Base")]),
        )
        .await;

        let result = timeout(Duration::from_secs(30), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].name, "Base");
        assert_eq!(result.items[0].kind, 5);
        assert_eq!(result.items[0].range.end.character, 5);
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn prepare_null_response_is_empty_not_an_error() {
        let dir = TempDir::new().unwrap();
        let (translator, mut server) =
            translator_with_capabilities(&dir, &ServerId::from("rust"), caps());
        let path = dir.path().join("a.rs");
        fs::write(&path, "let x = 1;").unwrap();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().into_owned();
            tokio::spawn(async move {
                translator
                    .handle_type_hierarchy_prepare(client_path(path), pos(1, 1))
                    .await
            })
        };
        let mut wire = BufReader::new(&mut server.write_stdout);
        read_framed_message(&mut wire).await;
        let request = read_framed_message(&mut wire).await;
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::Value::Null,
        )
        .await;

        let result = timeout(Duration::from_secs(30), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();
        assert!(result.items.is_empty());
    }

    #[tokio::test]
    async fn prepare_without_capability_is_rejected() {
        let dir = TempDir::new().unwrap();
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &ServerId::from("rust"),
            lsp_types::ServerCapabilities::default(),
        );
        let path = dir.path().join("a.rs");
        fs::write(&path, "struct Base;").unwrap();

        let result = translator
            .handle_type_hierarchy_prepare(
                client_path(path.to_string_lossy().into_owned()),
                pos(1, 1),
            )
            .await;
        assert_matches!(result, Err(Error::CapabilityNotSupported { .. }));
    }

    async fn walk_request_method(direction: WalkDirection) -> (String, serde_json::Value) {
        let dir = TempDir::new().unwrap();
        let (translator, mut server) =
            translator_with_capabilities(&dir, &ServerId::from("rust"), caps());
        let path = dir.path().join("a.rs");
        fs::write(&path, "struct Derived;").unwrap();
        let uri = Url::from_file_path(&path).unwrap().to_string();
        let item = checked_item(&uri);

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                match direction {
                    WalkDirection::Supertypes => translator.handle_supertypes(item).await,
                    WalkDirection::Subtypes => translator.handle_subtypes(item).await,
                }
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        assert_eq!(
            read_framed_message(&mut wire).await["method"],
            "textDocument/didOpen"
        );
        let request = read_framed_message(&mut wire).await;
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([lsp_item_json(&uri, "Base")]),
        )
        .await;

        let result = timeout(Duration::from_secs(30), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();
        assert_eq!(result.items[0].name, "Base");
        (
            request["method"].as_str().unwrap().to_string(),
            request["params"]["item"].clone(),
        )
    }

    #[tokio::test]
    async fn supertypes_and_subtypes_send_their_own_method_with_the_opaque_item() {
        for (direction, method) in [
            (WalkDirection::Supertypes, "typeHierarchy/supertypes"),
            (WalkDirection::Subtypes, "typeHierarchy/subtypes"),
        ] {
            let (sent_method, item) = walk_request_method(direction).await;
            assert_eq!(sent_method, method);
            assert_eq!(item["name"], "Derived");
            assert_eq!(item["data"], serde_json::json!({"id": "Derived"}));
            assert_eq!(item["selectionRange"]["start"]["line"], 0);
        }
    }

    #[tokio::test]
    async fn walk_rejects_item_outside_the_workspace_without_a_request() {
        let dir = TempDir::new().unwrap();
        let (translator, _server) =
            translator_with_capabilities(&dir, &ServerId::from("rust"), caps());
        let outside = TempDir::new().unwrap();
        let path = outside.path().join("a.rs");
        fs::write(&path, "struct Derived;").unwrap();
        let uri = Url::from_file_path(&path).unwrap().to_string();

        let result = translator.handle_supertypes(checked_item(&uri)).await;
        assert_matches!(result, Err(Error::PathOutsideWorkspace(_)));
    }

    /// The server must receive the canonical URI of the validated document,
    /// not the client's `sub/../a.rs` spelling, which a server resolving it
    /// physically could read from outside the workspace.
    #[tokio::test]
    async fn walk_forwards_the_canonical_document_uri_not_the_raw_item_uri() {
        let dir = TempDir::new().unwrap();
        let (translator, mut server) =
            translator_with_capabilities(&dir, &ServerId::from("rust"), caps());
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("a.rs"), "struct Derived;").unwrap();
        let root = canonical_dir(&dir);
        let canonical_uri = Url::from_file_path(root.join("a.rs")).unwrap().to_string();
        let root_uri = Url::from_file_path(&root).unwrap().to_string();
        let raw_uri = format!("{}/sub/../a.rs", root_uri.trim_end_matches('/'));
        assert_ne!(raw_uri, canonical_uri);

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let item = checked_item(&raw_uri);
            tokio::spawn(async move { translator.handle_supertypes(item).await })
        };
        let mut wire = BufReader::new(&mut server.write_stdout);
        read_framed_message(&mut wire).await;
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["params"]["item"]["uri"], canonical_uri);
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::Value::Null,
        )
        .await;
        timeout(Duration::from_secs(30), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn walk_rejects_non_file_uri() {
        let result = Translator::new()
            .handle_subtypes(checked_item("https://example.com/a.rs"))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn walk_caps_items_and_reports_truncation() {
        let dir = TempDir::new().unwrap();
        let (translator, mut server) =
            translator_with_capabilities(&dir, &ServerId::from("rust"), caps());
        let path = dir.path().join("a.rs");
        fs::write(&path, "struct Derived;").unwrap();
        let uri = Url::from_file_path(&path).unwrap().to_string();
        let item = checked_item(&uri);

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move { translator.handle_subtypes(item).await })
        };
        let mut wire = BufReader::new(&mut server.write_stdout);
        read_framed_message(&mut wire).await;
        let request = read_framed_message(&mut wire).await;
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::Value::Array(vec![
                lsp_item_json(&uri, "Sub");
                MAX_NORMALIZED_LOCATIONS + 5
            ]),
        )
        .await;

        let result = timeout(Duration::from_secs(30), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();
        assert_eq!(result.items.len(), MAX_NORMALIZED_LOCATIONS);
        assert!(result.truncated);
    }

    #[test]
    fn item_json_round_trips_through_the_typed_dto() {
        let dto = item_dto("file:///a.rs");
        let wire = serde_json::to_value(&dto).unwrap();
        assert!(wire.get("selectionRange").is_some());
        let back: HierarchyItem = serde_json::from_value(wire).unwrap();
        assert_eq!(back.data, dto.data);
    }

    #[test]
    fn malformed_item_is_rejected_at_the_type_boundary() {
        let parsed = serde_json::from_value::<HierarchyItem>(serde_json::json!({"name": "x"}));
        assert!(parsed.is_err());
    }
}
