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

/// Normalize a `textDocument/definition` response into a flat list of MCP
/// `Location` values.
async fn definition_response_to_locations(
    response: Option<lsp_types::DefinitionResponse>,
    ctx: &EncodingCtx,
) -> Vec<Location> {
    let lsp_locs = match response {
        Some(lsp_types::DefinitionResponse::Definition(def)) => definition_to_locations(def),
        Some(lsp_types::DefinitionResponse::DefinitionLinkList(links)) => {
            links.into_iter().map(definition_link_to_location).collect()
        }
        None => vec![],
    };
    lsp_locations_to_mcp(lsp_locs, ctx).await
}

/// Normalize a `textDocument/implementation` response into a flat list of
/// MCP `Location` values.
async fn implementation_response_to_locations(
    response: Option<lsp_types::ImplementationResponse>,
    ctx: &EncodingCtx,
) -> Vec<Location> {
    let lsp_locs = match response {
        Some(lsp_types::ImplementationResponse::Definition(def)) => definition_to_locations(def),
        Some(lsp_types::ImplementationResponse::DefinitionLinkList(links)) => {
            links.into_iter().map(definition_link_to_location).collect()
        }
        None => vec![],
    };
    lsp_locations_to_mcp(lsp_locs, ctx).await
}

/// Normalize a `textDocument/typeDefinition` response into a flat list of
/// MCP `Location` values.
async fn type_definition_response_to_locations(
    response: Option<lsp_types::TypeDefinitionResponse>,
    ctx: &EncodingCtx,
) -> Vec<Location> {
    let lsp_locs = match response {
        Some(lsp_types::TypeDefinitionResponse::Definition(def)) => definition_to_locations(def),
        Some(lsp_types::TypeDefinitionResponse::DefinitionLinkList(links)) => {
            links.into_iter().map(definition_link_to_location).collect()
        }
        None => vec![],
    };
    lsp_locations_to_mcp(lsp_locs, ctx).await
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
        let Position { line, character } = position;
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
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
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = lsp_types::DefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::DefinitionRequest>(params, client.request_timeout())
            .await?;

        let result = DefinitionResult {
            locations: definition_response_to_locations(response, &ctx).await,
        };

        Ok(result)
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
        let Position { line, character } = position;
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
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
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = lsp_types::ImplementationParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::ImplementationRequest>(params, client.request_timeout())
            .await?;

        Ok(LocationsResult {
            locations: implementation_response_to_locations(response, &ctx).await,
        })
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
        let Position { line, character } = position;
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
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
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = lsp_types::TypeDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::TypeDefinitionRequest>(params, client.request_timeout())
            .await?;

        Ok(LocationsResult {
            locations: type_definition_response_to_locations(response, &ctx).await,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, deprecated)]
mod tests {
    use super::*;

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
}
