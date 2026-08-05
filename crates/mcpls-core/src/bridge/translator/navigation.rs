//! Hover, go-to-definition/implementation/type-definition, and references
//! handlers.

use lsp_types::{
    GotoDefinitionParams, Hover, HoverContents, HoverParams as LspHoverParams, MarkedString,
    PartialResultParams, ReferenceContext, ReferenceParams, TextDocumentIdentifier,
    TextDocumentPositionParams, WorkDoneProgressParams,
};

use super::Translator;
use super::dto::{DefinitionResult, HoverResult, Location, LocationsResult, ReferencesResult};
use super::encoding_ctx::EncodingCtx;
use crate::config::ToolKind;
use crate::error::Result;

/// Normalize a `GotoDefinitionResponse` into a flat list of MCP `Location` values.
async fn goto_response_to_locations(
    response: Option<lsp_types::GotoDefinitionResponse>,
    ctx: &EncodingCtx,
) -> Vec<Location> {
    let lsp_locs: Vec<lsp_types::Location> = match response {
        Some(lsp_types::GotoDefinitionResponse::Scalar(loc)) => vec![loc],
        Some(lsp_types::GotoDefinitionResponse::Array(locs)) => locs,
        Some(lsp_types::GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|link| lsp_types::Location {
                uri: link.target_uri,
                range: link.target_selection_range,
            })
            .collect(),
        None => vec![],
    };

    let mut locations = Vec::with_capacity(lsp_locs.len());
    for loc in lsp_locs {
        locations.push(Location {
            uri: loc.uri.to_string(),
            range: ctx.normalize_range(&loc.uri, loc.range).await,
        });
    }
    locations
}

fn extract_hover_contents(contents: HoverContents) -> String {
    match contents {
        HoverContents::Scalar(marked_string) => marked_string_to_string(marked_string),
        HoverContents::Array(marked_strings) => marked_strings
            .into_iter()
            .map(marked_string_to_string)
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(markup) => markup.value,
    }
}

/// Convert a marked string to a plain string.
fn marked_string_to_string(marked: MarkedString) -> String {
    match marked {
        MarkedString::String(s) => s,
        MarkedString::LanguageString(ls) => format!("```{}\n{}\n```", ls.language, ls.value),
    }
}

impl Translator {
    /// Handle hover request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `hoverProvider` support.
    pub async fn handle_hover(
        &self,
        file_path: String,
        line: u32,
        character: u32,
    ) -> Result<HoverResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(&file_path, ToolKind::Hover, "hoverProvider", |caps| {
                matches!(
                    caps.hover_provider,
                    Some(
                        lsp_types::HoverProviderCapability::Simple(true)
                            | lsp_types::HoverProviderCapability::Options(_)
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

        let response: Option<Hover> = client
            .request("textDocument/hover", params, client.request_timeout())
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
        line: u32,
        character: u32,
    ) -> Result<DefinitionResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::Definition,
                "definitionProvider",
                |caps| {
                    matches!(
                        caps.definition_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<lsp_types::GotoDefinitionResponse> = client
            .request("textDocument/definition", params, client.request_timeout())
            .await?;

        let result = DefinitionResult {
            locations: goto_response_to_locations(response, &ctx).await,
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
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<ReferencesResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::References,
                "referencesProvider",
                |caps| {
                    matches!(
                        caps.references_provider,
                        Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration,
            },
        };

        let response: Option<Vec<lsp_types::Location>> = client
            .request("textDocument/references", params, client.request_timeout())
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
        line: u32,
        character: u32,
    ) -> Result<LocationsResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::Implementation,
                "implementationProvider",
                |caps| {
                    matches!(
                        caps.implementation_provider,
                        Some(
                            lsp_types::ImplementationProviderCapability::Simple(true)
                                | lsp_types::ImplementationProviderCapability::Options(_)
                        )
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<lsp_types::GotoDefinitionResponse> = client
            .request(
                "textDocument/implementation",
                params,
                client.request_timeout(),
            )
            .await?;

        Ok(LocationsResult {
            locations: goto_response_to_locations(response, &ctx).await,
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
        line: u32,
        character: u32,
    ) -> Result<LocationsResult> {
        let (server_id, client, uri) = self
            .prepare_gated_document(
                &file_path,
                ToolKind::TypeDefinition,
                "typeDefinitionProvider",
                |caps| {
                    matches!(
                        caps.type_definition_provider,
                        Some(
                            lsp_types::TypeDefinitionProviderCapability::Simple(true)
                                | lsp_types::TypeDefinitionProviderCapability::Options(_)
                        )
                    )
                },
            )
            .await?;
        let ctx = self.encoding_ctx(&server_id);
        let lsp_position = ctx.to_lsp(&uri, line, character).await;

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: lsp_position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let response: Option<lsp_types::GotoDefinitionResponse> = client
            .request(
                "textDocument/typeDefinition",
                params,
                client.request_timeout(),
            )
            .await?;

        Ok(LocationsResult {
            locations: goto_response_to_locations(response, &ctx).await,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_hover_contents_string() {
        let marked_string = lsp_types::MarkedString::String("Test hover".to_string());
        let contents = lsp_types::HoverContents::Scalar(marked_string);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "Test hover");
    }

    #[test]
    fn test_extract_hover_contents_language_string() {
        let marked_string = lsp_types::MarkedString::LanguageString(lsp_types::LanguageString {
            language: "rust".to_string(),
            value: "fn main() {}".to_string(),
        });
        let contents = lsp_types::HoverContents::Scalar(marked_string);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "```rust\nfn main() {}\n```");
    }

    #[test]
    fn test_extract_hover_contents_markup() {
        let markup = lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::Markdown,
            value: "# Documentation".to_string(),
        };
        let contents = lsp_types::HoverContents::Markup(markup);
        let result = extract_hover_contents(contents);
        assert_eq!(result, "# Documentation");
    }
}
