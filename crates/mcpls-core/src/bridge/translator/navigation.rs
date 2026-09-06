//! Hover, go-to-definition/implementation/type-definition, and references
//! handlers.

use lsp_types::{
    HoverParams as LspHoverParams, PartialResultParams, ReferenceContext, ReferenceParams,
    TextDocumentIdentifier, TextDocumentPositionParams, WorkDoneProgressParams,
};

use super::Translator;
use super::dto::{
    DefinitionResult, HoverResult, Location, LocationsResult, Position, ReferencesResult,
};
use super::encoding_ctx::EncodingCtx;
use crate::config::ToolKind;
use crate::error::Result;

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

/// Converts raw LSP locations into MCP-facing `Location` values, normalizing
/// each range into the caller's 1-based coordinate space.
async fn lsp_locations_to_mcp(locs: Vec<lsp_types::Location>, ctx: &EncodingCtx) -> Vec<Location> {
    let mut locations = Vec::with_capacity(locs.len());
    for loc in locs {
        locations.push(Location {
            uri: loc.uri.to_string(),
            range: ctx.normalize_range(&loc.uri, loc.range).await,
        });
    }
    locations
}

/// The two response shapes shared by `textDocument/definition`,
/// `textDocument/implementation`, and `textDocument/typeDefinition`: either a
/// single `Definition` (`Location` or `Location[]`), or a `DefinitionLink[]`
/// from clients that opted into `LinkSupport`.
enum GotoKind {
    /// A plain `Definition`, as returned to clients without `LinkSupport`.
    Definition(lsp_types::Definition),
    /// A `DefinitionLink[]`, as returned to clients with `LinkSupport`.
    DefinitionLinkList(Vec<lsp_types::DefinitionLink>),
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
            Self::Definition(def) => GotoKind::Definition(def),
            Self::DefinitionLinkList(links) => GotoKind::DefinitionLinkList(links),
        }
    }
}

impl GotoResponse for lsp_types::ImplementationResponse {
    fn into_kind(self) -> GotoKind {
        match self {
            Self::Definition(def) => GotoKind::Definition(def),
            Self::DefinitionLinkList(links) => GotoKind::DefinitionLinkList(links),
        }
    }
}

impl GotoResponse for lsp_types::TypeDefinitionResponse {
    fn into_kind(self) -> GotoKind {
        match self {
            Self::Definition(def) => GotoKind::Definition(def),
            Self::DefinitionLinkList(links) => GotoKind::DefinitionLinkList(links),
        }
    }
}

/// Normalize a go-to-X response (`textDocument/definition`,
/// `textDocument/implementation`, or `textDocument/typeDefinition`) into a
/// flat list of MCP `Location` values.
async fn goto_response_to_locations<R: GotoResponse>(
    response: Option<R>,
    ctx: &EncodingCtx,
) -> Vec<Location> {
    let lsp_locs = match response.map(GotoResponse::into_kind) {
        Some(GotoKind::Definition(def)) => definition_to_locations(def),
        Some(GotoKind::DefinitionLinkList(links)) => {
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

/// Extracts hover contents as a plain string.
///
/// `MarkedString` is `#[deprecated]` in favor of `MarkupContent`, but LSP
/// 3.17 servers may still send it inside `Hover.contents` -- dropping
/// support would silently discard hover text from those servers, so this
/// (and `marked_string_to_string`) carry a narrow, scoped allow rather than
/// rewriting to `MarkupContent`-only.
#[allow(deprecated)]
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
#[allow(deprecated)]
fn marked_string_to_string(marked: lsp_types::MarkedString) -> String {
    match marked {
        lsp_types::MarkedString::String(s) => s,
        lsp_types::MarkedString::MarkedStringWithLanguage(ls) => {
            format!("```{}\n{}\n```", ls.language, ls.value)
        }
    }
}

impl Translator {
    /// Handle hover request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `hoverProvider` support.
    pub async fn handle_hover(&self, file_path: String, position: Position) -> Result<HoverResult> {
        let Position { line, character } = position;
        let (server_id, client, uri) = self
            .prepare_gated_document(&file_path, ToolKind::Hover, "hoverProvider", |caps| {
                matches!(
                    caps.hover_provider,
                    Some(
                        lsp_types::HoverProvider::Bool(true)
                            | lsp_types::HoverProvider::HoverOptions(_)
                    )
                )
            })
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;
        let response_uri = uri.clone();

        let params = LspHoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
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
                HoverResult { contents, range }
            }
            None => HoverResult {
                contents: "No hover information available".to_string(),
                range: None,
            },
        };

        Ok(result)
    }

    /// Shared implementation of the go-to-X handlers (`textDocument/definition`,
    /// `textDocument/implementation`, `textDocument/typeDefinition`): gate on
    /// the request's capability, translate the MCP position into LSP
    /// coordinates, dispatch the LSP request, and flatten the response into
    /// MCP locations. Each public handler supplies its request type via `R`
    /// plus the capability key/predicate specific to it.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `capability` support.
    async fn handle_goto<R, T>(
        &self,
        file_path: &str,
        position: Position,
        tool: ToolKind,
        capability: &'static str,
        supported: impl FnOnce(&lsp_types::ServerCapabilities) -> bool,
    ) -> Result<Vec<Location>>
    where
        R: lsp_types::Request<Result = Option<T>>,
        R::Params: GotoParams,
        T: GotoResponse,
    {
        let Position { line, character } = position;
        let (server_id, client, uri) = self
            .prepare_gated_document(file_path, tool, capability, supported)
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = R::Params::from_position(TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
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
    /// or the routed server does not advertise `definitionProvider` support.
    pub async fn handle_definition(
        &self,
        file_path: String,
        position: Position,
    ) -> Result<DefinitionResult> {
        let locations = self
            .handle_goto::<lsp_types::DefinitionRequest, _>(
                &file_path,
                position,
                ToolKind::Definition,
                "definitionProvider",
                |caps| {
                    matches!(
                        caps.definition_provider,
                        Some(
                            lsp_types::DefinitionProvider::Bool(true)
                                | lsp_types::DefinitionProvider::DefinitionOptions(_)
                        )
                    )
                },
            )
            .await?;

        Ok(DefinitionResult { locations })
    }

    /// Handle references request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `referencesProvider` support.
    pub async fn handle_references(
        &self,
        file_path: String,
        position: Position,
        include_declaration: bool,
    ) -> Result<ReferencesResult> {
        let Position { line, character } = position;
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::References,
                "referencesProvider",
                |caps| {
                    matches!(
                        caps.references_provider,
                        Some(
                            lsp_types::ReferencesProvider::Bool(true)
                                | lsp_types::ReferencesProvider::ReferenceOptions(_)
                        )
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = ReferenceParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
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

        let locations = response.unwrap_or_default();

        let mut result_locations = Vec::with_capacity(locations.len());
        for loc in locations {
            result_locations.push(Location {
                uri: loc.uri.to_string(),
                range: ctx.normalize_range(&loc.uri, loc.range).await,
            });
        }
        let result = ReferencesResult {
            locations: result_locations,
        };

        Ok(result)
    }

    /// Handle go-to-implementation request (`textDocument/implementation`).
    ///
    /// Returns the locations of trait method or interface member implementations.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `implementationProvider` support.
    pub async fn handle_implementation(
        &self,
        file_path: String,
        position: Position,
    ) -> Result<LocationsResult> {
        let locations = self
            .handle_goto::<lsp_types::ImplementationRequest, _>(
                &file_path,
                position,
                ToolKind::Implementation,
                "implementationProvider",
                |caps| {
                    matches!(
                        caps.implementation_provider,
                        Some(
                            lsp_types::ImplementationProvider::Bool(true)
                                | lsp_types::ImplementationProvider::ImplementationOptions(_)
                                | lsp_types::ImplementationProvider::ImplementationRegistrationOptions(_)
                        )
                    )
                },
            )
            .await?;

        Ok(LocationsResult { locations })
    }

    /// Handle go-to-type-definition request (`textDocument/typeDefinition`).
    ///
    /// Returns the type definition location of the expression at position. Distinct
    /// from go-to-definition for variable bindings where definition and type differ.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `typeDefinitionProvider` support.
    pub async fn handle_type_definition(
        &self,
        file_path: String,
        position: Position,
    ) -> Result<LocationsResult> {
        let locations = self
            .handle_goto::<lsp_types::TypeDefinitionRequest, _>(
                &file_path,
                position,
                ToolKind::TypeDefinition,
                "typeDefinitionProvider",
                |caps| {
                    matches!(
                        caps.type_definition_provider,
                        Some(
                            lsp_types::TypeDefinitionProvider::Bool(true)
                                | lsp_types::TypeDefinitionProvider::TypeDefinitionOptions(_)
                                | lsp_types::TypeDefinitionProvider::TypeDefinitionRegistrationOptions(_)
                        )
                    )
                },
            )
            .await?;

        Ok(LocationsResult { locations })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, deprecated)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::time::timeout;
    use url::Url;

    use super::*;
    use crate::bridge::translator::testing::*;
    use crate::config::ServerId;

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
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            definition_provider: Some(lsp_types::DefinitionProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let target_path = dir.path().join("target.rs");
        fs::write(&target_path, "fn target() {}").unwrap();
        let target_uri = Url::from_file_path(&target_path).unwrap().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_definition(
                        path,
                        Position {
                            line: 1,
                            character: 1,
                        },
                    )
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
    }

    /// Success-path coverage for `handle_implementation` through the
    /// `Definition::LocationList` -> `GotoKind::Definition` arm, pinning the
    /// `GotoResponse` impl for `ImplementationResponse`.
    #[tokio::test]
    async fn test_handle_implementation_flattens_location_list() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
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
                        path,
                        Position {
                            line: 1,
                            character: 1,
                        },
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

    /// Success-path coverage for `handle_type_definition` through the
    /// `DefinitionLinkList` -> `GotoKind::DefinitionLinkList` arm, pinning
    /// the `GotoResponse` impl for `TypeDefinitionResponse` and the
    /// `definition_link_to_location` mapping (`target_selection_range`, not
    /// `target_range`).
    #[tokio::test]
    async fn test_handle_type_definition_flattens_definition_link_list() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
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
                        path,
                        Position {
                            line: 1,
                            character: 1,
                        },
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
}
