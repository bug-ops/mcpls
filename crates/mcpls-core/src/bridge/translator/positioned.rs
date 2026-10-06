//! The request shape shared by every position-taking tool: gate and open the
//! document, convert the MCP position into the server's encoding, build the
//! LSP params, send, and keep the document alive for response conversion.

use std::time::Duration;

use lsp_types::{
    CallHierarchyPrepareParams, CompletionContext, CompletionParams, DeclarationParams,
    DefinitionParams, DocumentHighlightParams, HoverParams, ImplementationParams,
    PartialResultParams, PrepareRenameParams, ReferenceContext, ReferenceParams, RenameParams,
    SignatureHelpParams, TextDocumentIdentifier, TextDocumentPositionParams, TypeDefinitionParams,
    TypeHierarchyPrepareParams, WorkDoneProgressParams,
};

use super::Translator;
use super::dto::Position;
use super::encoding_ctx::EncodingCtx;
use super::routing::{Capability, IndexingGate, PreparedDocument};
use crate::bridge::{ClientPath, Indexed, IndexingSignal};
use crate::config::ServerId;
use crate::error::Result;
use crate::lsp::{LspClient, UnclassifiedError};

/// LSP request params built from a resolved document position.
///
/// Implemented once per position-taking request so [`Translator::position_request`]
/// builds every one of them through the same path.
pub(super) trait FromPosition: Sized {
    /// Request-specific input beyond the position (`()` when there is none).
    type Extra;

    /// Builds the params from the converted position, defaulting the
    /// work-done and partial-result progress fields.
    fn from_position(position: TextDocumentPositionParams, extra: Self::Extra) -> Self;

    /// The timeout applied to this request.
    fn timeout(client: &LspClient) -> Duration {
        client.request_timeout()
    }
}

macro_rules! from_position_work_done {
    ($($params:ty),+ $(,)?) => {$(
        impl FromPosition for $params {
            type Extra = ();

            fn from_position(text_document_position_params: TextDocumentPositionParams, (): ()) -> Self {
                Self {
                    text_document_position_params,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                }
            }
        }
    )+};
}

macro_rules! from_position_with_partial {
    ($($params:ty),+ $(,)?) => {$(
        impl FromPosition for $params {
            type Extra = ();

            fn from_position(text_document_position_params: TextDocumentPositionParams, (): ()) -> Self {
                Self {
                    text_document_position_params,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                }
            }
        }
    )+};
}

from_position_work_done!(
    HoverParams,
    CallHierarchyPrepareParams,
    TypeHierarchyPrepareParams,
    PrepareRenameParams,
);

from_position_with_partial!(
    DefinitionParams,
    ImplementationParams,
    TypeDefinitionParams,
    DeclarationParams,
    DocumentHighlightParams,
);

impl FromPosition for ReferenceParams {
    type Extra = ReferenceContext;

    fn from_position(
        text_document_position_params: TextDocumentPositionParams,
        context: ReferenceContext,
    ) -> Self {
        Self {
            text_document_position_params,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context,
        }
    }
}

impl FromPosition for CompletionParams {
    type Extra = Option<CompletionContext>;

    fn from_position(
        text_document_position_params: TextDocumentPositionParams,
        context: Option<CompletionContext>,
    ) -> Self {
        Self {
            text_document_position_params,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context,
        }
    }

    fn timeout(client: &LspClient) -> Duration {
        client.completion_timeout()
    }
}

impl FromPosition for SignatureHelpParams {
    type Extra = ();

    fn from_position(text_document_position_params: TextDocumentPositionParams, (): ()) -> Self {
        Self {
            text_document_position_params,
            work_done_progress_params: WorkDoneProgressParams::default(),
            context: None,
        }
    }
}

impl FromPosition for RenameParams {
    type Extra = String;

    fn from_position(
        text_document_position_params: TextDocumentPositionParams,
        new_name: String,
    ) -> Self {
        Self {
            text_document_position_params,
            new_name,
            work_done_progress_params: WorkDoneProgressParams::default(),
        }
    }
}

/// A sent positioned request's result, with the encoding context and the
/// document it was made against.
///
/// Holds the document so its in-flight guard outlives response conversion;
/// bind the whole value until the response is converted.
#[derive(Debug)]
#[must_use = "dropping the document makes it evictable while the response is converted"]
pub(super) struct Positioned<T, D = PreparedDocument> {
    pub(super) result: T,
    pub(super) ctx: EncodingCtx,
    pub(super) doc: D,
}

/// A positioned request ready to send.
#[must_use = "a positioned call does nothing until sent"]
pub(super) struct PositionedCall<R: lsp_types::Request> {
    doc: PreparedDocument,
    ctx: EncodingCtx,
    params: R::Params,
}

impl<R> PositionedCall<R>
where
    R: lsp_types::Request,
    R::Params: FromPosition,
{
    /// Sends the request and returns its result.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails.
    pub(super) async fn send(self) -> Result<Positioned<R::Result>> {
        let client = self.doc.client();
        let result = client
            .request_typed::<R>(self.params, <R::Params as FromPosition>::timeout(client))
            .await?;
        Ok(Positioned {
            result,
            ctx: self.ctx,
            doc: self.doc,
        })
    }

    /// As [`Self::send`], but keeps the failure's classification for a caller
    /// that tells a server's rejection from an operational error.
    pub(super) async fn send_classified(
        self,
    ) -> Positioned<std::result::Result<R::Result, UnclassifiedError>> {
        let client = self.doc.client();
        let result = client
            .request_typed_classified::<R>(
                self.params,
                <R::Params as FromPosition>::timeout(client),
            )
            .await;
        Positioned {
            result,
            ctx: self.ctx,
            doc: self.doc,
        }
    }
}

/// A document opened for a name-resolving tool that does not gate on indexing
/// readiness but must report it (#668).
///
/// Exposes no client: the only way to query the server is [`Self::request`],
/// which samples the indexing state around the call and returns an
/// [`Indexed`] answer, so the `indexing_in_progress` flag cannot be omitted.
#[must_use = "dropping the document makes it evictable mid-request"]
pub(super) struct DisclosedDocument<'a> {
    translator: &'a Translator,
    doc: PreparedDocument,
}

impl DisclosedDocument<'_> {
    pub(super) const fn server_id(&self) -> &ServerId {
        self.doc.server_id()
    }

    pub(super) const fn uri(&self) -> &lsp_types::Uri {
        self.doc.uri()
    }

    /// Sends `params` and pairs the response with the indexing state sampled
    /// before and after, so a read that overlapped indexing is flagged even
    /// when indexing ends mid-request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails.
    pub(super) async fn request<R: lsp_types::Request>(
        &self,
        params: R::Params,
    ) -> Result<Indexed<R::Result>> {
        let client = self.doc.client();
        let before = self.translator.sample_indexing(self.server_id()).await;
        let result = client
            .request_typed::<R>(params, client.request_timeout())
            .await?;
        let after = self.translator.sample_indexing(self.server_id()).await;
        Ok(Indexed::new(result, before.union(after)))
    }
}

impl std::fmt::Debug for DisclosedDocument<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisclosedDocument")
            .field("doc", &self.doc)
            .finish_non_exhaustive()
    }
}

impl Translator {
    /// Never waits: ungated tools disclose the indexing state rather than gate
    /// on it. Samples nothing for a translator without a notification cache.
    async fn sample_indexing(&self, server_id: &ServerId) -> IndexingSignal {
        match &self.notification_cache {
            Some(cache) => IndexingSignal::sample(&*cache.lock().await, Some(server_id)),
            None => IndexingSignal::default(),
        }
    }

    /// Opens `file_path` for a name-resolving tool that discloses indexing
    /// instead of waiting for it, after the capability gate and the line check
    /// of every one of `positions`.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::prepare_positioned_document`], without the
    /// indexing wait.
    pub(super) async fn prepare_disclosed_document(
        &self,
        file_path: &ClientPath,
        capability: Capability,
        positions: &[Position],
    ) -> Result<DisclosedDocument<'_>> {
        let doc = self
            .prepare_positioned_document(file_path, capability, IndexingGate::FileLocal, positions)
            .await?;
        Ok(DisclosedDocument {
            translator: self,
            doc,
        })
    }

    async fn positioned_params<P: FromPosition>(
        &self,
        server_id: &ServerId,
        uri: &lsp_types::Uri,
        position: Position,
        extra: P::Extra,
    ) -> (EncodingCtx, P) {
        let ctx = self.encoding_ctx(server_id);
        let params = P::from_position(
            TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: ctx.to_lsp(uri, position).await,
            },
            extra,
        );
        (ctx, params)
    }

    /// Gates and opens `file_path`, converts `position`, and builds the
    /// request, without sending it.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::prepare_positioned_document`].
    pub(super) async fn position_call<R>(
        &self,
        file_path: &ClientPath,
        position: Position,
        capability: Capability,
        indexing_gate: IndexingGate,
        extra: <R::Params as FromPosition>::Extra,
    ) -> Result<PositionedCall<R>>
    where
        R: lsp_types::Request,
        R::Params: FromPosition,
    {
        let doc = self
            .prepare_positioned_document(file_path, capability, indexing_gate, &[position])
            .await?;
        let (ctx, params) = self
            .positioned_params::<R::Params>(doc.server_id(), doc.uri(), position, extra)
            .await;
        Ok(PositionedCall { doc, ctx, params })
    }

    /// [`Self::position_call`], then send.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::position_call`] and [`PositionedCall::send`].
    pub(super) async fn position_request<R>(
        &self,
        file_path: &ClientPath,
        position: Position,
        capability: Capability,
        indexing_gate: IndexingGate,
        extra: <R::Params as FromPosition>::Extra,
    ) -> Result<Positioned<R::Result>>
    where
        R: lsp_types::Request,
        R::Params: FromPosition,
    {
        self.position_call::<R>(file_path, position, capability, indexing_gate, extra)
            .await?
            .send()
            .await
    }

    /// As [`Self::position_request`] for a name-resolving tool that discloses
    /// indexing instead of gating on it; the result is [`Indexed`].
    ///
    /// # Errors
    ///
    /// The errors of [`Self::prepare_disclosed_document`] and
    /// [`DisclosedDocument::request`].
    pub(super) async fn disclosed_position_request<R>(
        &self,
        file_path: &ClientPath,
        position: Position,
        capability: Capability,
        extra: <R::Params as FromPosition>::Extra,
    ) -> Result<Positioned<Indexed<R::Result>, DisclosedDocument<'_>>>
    where
        R: lsp_types::Request,
        R::Params: FromPosition,
    {
        let doc = self
            .prepare_disclosed_document(file_path, capability, &[position])
            .await?;
        let (ctx, params) = self
            .positioned_params::<R::Params>(doc.server_id(), doc.uri(), position, extra)
            .await;
        let result = doc.request::<R>(params).await?;
        Ok(Positioned { result, ctx, doc })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position_params() -> TextDocumentPositionParams {
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: lsp_types::Uri::from("file:///ws/a.rs"),
            },
            position: lsp_types::Position {
                line: 3,
                character: 7,
            },
        }
    }

    #[test]
    fn test_from_position_keeps_position_for_progress_only_params() {
        let params = HoverParams::from_position(position_params(), ());
        assert_eq!(params.text_document_position_params, position_params());
    }

    #[test]
    fn test_from_position_carries_reference_context() {
        let params = ReferenceParams::from_position(
            position_params(),
            ReferenceContext {
                include_declaration: true,
            },
        );
        assert!(params.context.include_declaration);
        assert_eq!(params.text_document_position_params, position_params());
    }

    #[test]
    fn test_from_position_carries_rename_new_name() {
        let params = RenameParams::from_position(position_params(), "renamed".to_owned());
        assert_eq!(params.new_name, "renamed");
    }

    #[test]
    fn test_from_position_signature_help_has_no_context() {
        let params = SignatureHelpParams::from_position(position_params(), ());
        assert!(params.context.is_none());
    }

    #[test]
    fn test_from_position_carries_completion_context() {
        let context = CompletionContext {
            trigger_kind: lsp_types::CompletionTriggerKind::TriggerCharacter,
            trigger_character: Some(".".to_owned()),
        };
        let params = CompletionParams::from_position(position_params(), Some(context));
        assert_eq!(
            params.context.and_then(|c| c.trigger_character).as_deref(),
            Some(".")
        );
    }
}
