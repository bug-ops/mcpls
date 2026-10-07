//! The request shape shared by every position-taking tool: gate and open the
//! document, convert the MCP position into the server's encoding, build the
//! LSP params, send, and keep the document alive for response conversion.

use std::time::Duration;

use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    CodeActionContext, CodeActionParams, CompletionContext, CompletionParams, DeclarationParams,
    DefinitionParams, DocumentHighlightParams, DocumentRangeFormattingParams, FormattingOptions,
    HoverParams, ImplementationParams, InlayHintParams, PartialResultParams, PrepareRenameParams,
    ReferenceContext, ReferenceParams, RenameParams, SignatureHelpParams, TextDocumentIdentifier,
    TextDocumentPositionParams, TypeDefinitionParams, TypeHierarchyPrepareParams,
    TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, WorkDoneProgressParams,
};

use super::Translator;
use super::dto::{CheckedHierarchyItem, Position, PositionRange};
use super::encoding_ctx::EncodingCtx;
use super::hierarchy::{LspHierarchyItem, hierarchy_item_to_lsp};
use super::routing::{Capability, IndexingGate, PreparedDocument};
use crate::bridge::{ClientPath, Indexed, IndexingSignal};
use crate::config::ServerId;
use crate::error::Result;
use crate::lsp::{LspClient, UnclassifiedError};

/// The timeout applied to a request carrying these params.
///
/// Every request sent through [`PositionedCall`] or [`DisclosedDocument`]
/// reads its timeout here, so a request kind with its own budget (completion)
/// cannot lose it on one of the two paths.
pub(super) trait RequestTimeout {
    /// The timeout for this request on `client`.
    fn timeout(client: &LspClient) -> Duration {
        client.request_timeout()
    }
}

/// LSP request params built from a resolved document position.
///
/// Implemented once per position-taking request so [`Translator::position_request`]
/// builds every one of them through the same path.
pub(super) trait FromPosition: RequestTimeout + Sized {
    /// Request-specific input beyond the position (`()` when there is none).
    type Extra;

    /// Builds the params from the converted position, defaulting the
    /// work-done and partial-result progress fields.
    fn from_position(position: TextDocumentPositionParams, extra: Self::Extra) -> Self;
}

macro_rules! from_position_work_done {
    ($($params:ty),+ $(,)?) => {$(
        impl RequestTimeout for $params {}

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
        impl RequestTimeout for $params {}

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

impl RequestTimeout for ReferenceParams {}

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

impl RequestTimeout for CompletionParams {
    fn timeout(client: &LspClient) -> Duration {
        client.completion_timeout()
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
}

impl RequestTimeout for SignatureHelpParams {}

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

impl RequestTimeout for RenameParams {}

impl RequestTimeout for InlayHintParams {}

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

/// LSP request params built from a resolved document range.
///
/// The range counterpart of [`FromPosition`], implemented once per
/// range-taking request so [`Translator::range_request`] builds each of them
/// through the same path.
pub(super) trait FromRange: RequestTimeout + Sized {
    /// Request-specific input beyond the range (`()` when there is none).
    type Extra;

    /// Builds the params from the converted range, defaulting the work-done
    /// and partial-result progress fields.
    fn from_range(
        text_document: TextDocumentIdentifier,
        range: lsp_types::Range,
        extra: Self::Extra,
    ) -> Self;
}

impl FromRange for InlayHintParams {
    type Extra = ();

    fn from_range(text_document: TextDocumentIdentifier, range: lsp_types::Range, (): ()) -> Self {
        Self {
            text_document,
            range,
            work_done_progress_params: WorkDoneProgressParams::default(),
        }
    }
}

impl RequestTimeout for DocumentRangeFormattingParams {}

impl FromRange for DocumentRangeFormattingParams {
    type Extra = FormattingOptions;

    fn from_range(
        text_document: TextDocumentIdentifier,
        range: lsp_types::Range,
        options: FormattingOptions,
    ) -> Self {
        Self {
            text_document,
            range,
            options,
            work_done_progress_params: WorkDoneProgressParams::default(),
        }
    }
}

impl RequestTimeout for CodeActionParams {}

impl FromRange for CodeActionParams {
    type Extra = CodeActionContext;

    fn from_range(
        text_document: TextDocumentIdentifier,
        range: lsp_types::Range,
        context: CodeActionContext,
    ) -> Self {
        Self {
            text_document,
            range,
            context,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }
    }
}

/// LSP request params that walk one level around a hierarchy item.
pub(super) trait FromItem: RequestTimeout + Sized {
    /// The LSP item type the params carry.
    type Item: From<LspHierarchyItem>;

    /// Builds the params around `item`, defaulting the progress fields.
    fn from_item(item: Self::Item) -> Self;
}

macro_rules! from_item_with_partial {
    ($($params:ty => $item:ty),+ $(,)?) => {$(
        impl RequestTimeout for $params {}

        impl FromItem for $params {
            type Item = $item;

            fn from_item(item: $item) -> Self {
                Self {
                    item,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                }
            }
        }
    )+};
}

from_item_with_partial!(
    CallHierarchyIncomingCallsParams => lsp_types::CallHierarchyItem,
    CallHierarchyOutgoingCallsParams => lsp_types::CallHierarchyItem,
    TypeHierarchySupertypesParams => lsp_types::TypeHierarchyItem,
    TypeHierarchySubtypesParams => lsp_types::TypeHierarchyItem,
);

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
            .request_typed::<R>(self.params, <R::Params as RequestTimeout>::timeout(client))
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
                <R::Params as RequestTimeout>::timeout(client),
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

impl<'a> DisclosedDocument<'a> {
    pub(super) const fn new(translator: &'a Translator, doc: PreparedDocument) -> Self {
        Self { translator, doc }
    }

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
    pub(super) async fn request<R>(&self, params: R::Params) -> Result<Indexed<R::Result>>
    where
        R: lsp_types::Request,
        R::Params: RequestTimeout,
    {
        let client = self.doc.client();
        let before = self.translator.sample_indexing(self.server_id()).await;
        let result = client
            .request_typed::<R>(params, <R::Params as RequestTimeout>::timeout(client))
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
        indexing_gate: IndexingGate,
        extra: <R::Params as FromPosition>::Extra,
    ) -> Result<PositionedCall<R>>
    where
        R: lsp_types::Request,
        R::Params: FromPosition,
    {
        let doc = self
            .prepare_positioned_document(file_path, indexing_gate, &[position])
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
        indexing_gate: IndexingGate,
        extra: <R::Params as FromPosition>::Extra,
    ) -> Result<Positioned<R::Result>>
    where
        R: lsp_types::Request,
        R::Params: FromPosition,
    {
        self.position_call::<R>(file_path, position, indexing_gate, extra)
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
    async fn ranged_params<P: FromRange>(
        &self,
        server_id: &ServerId,
        uri: &lsp_types::Uri,
        range: PositionRange,
        extra: P::Extra,
    ) -> (EncodingCtx, P) {
        let ctx = self.encoding_ctx(server_id);
        let lsp_range = ctx.denormalize_range(uri, range).await;
        let params = P::from_range(
            TextDocumentIdentifier { uri: uri.clone() },
            lsp_range,
            extra,
        );
        (ctx, params)
    }

    /// Gates and opens `file_path`, converts `range`, and sends the request.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::prepare_positioned_document`] and
    /// [`LspClient::request_typed`].
    pub(super) async fn range_request<R>(
        &self,
        file_path: &ClientPath,
        indexing_gate: IndexingGate,
        range: PositionRange,
        extra: <R::Params as FromRange>::Extra,
    ) -> Result<Positioned<R::Result>>
    where
        R: lsp_types::Request,
        R::Params: FromRange,
    {
        let doc = self
            .prepare_positioned_document(file_path, indexing_gate, &[range.start(), range.end()])
            .await?;
        let (ctx, params) = self
            .ranged_params::<R::Params>(doc.server_id(), doc.uri(), range, extra)
            .await;
        let client = doc.client();
        let result = client
            .request_typed::<R>(params, <R::Params as RequestTimeout>::timeout(client))
            .await?;
        Ok(Positioned { result, ctx, doc })
    }

    /// As [`Self::range_request`] for a name-resolving tool that discloses
    /// indexing instead of gating on it; the result is [`Indexed`].
    ///
    /// # Errors
    ///
    /// The errors of [`Self::prepare_disclosed_document`] and
    /// [`DisclosedDocument::request`].
    pub(super) async fn disclosed_range_request<R>(
        &self,
        file_path: &ClientPath,
        capability: Capability,
        range: PositionRange,
        extra: <R::Params as FromRange>::Extra,
    ) -> Result<Positioned<Indexed<R::Result>, DisclosedDocument<'_>>>
    where
        R: lsp_types::Request,
        R::Params: FromRange,
    {
        let doc = self
            .prepare_disclosed_document(file_path, capability, &[range.start(), range.end()])
            .await?;
        let (ctx, params) = self
            .ranged_params::<R::Params>(doc.server_id(), doc.uri(), range, extra)
            .await;
        let result = doc.request::<R>(params).await?;
        Ok(Positioned { result, ctx, doc })
    }
    /// Resolves, gates and queries one level around a hierarchy `item`.
    ///
    /// The returned document's URI is the canonical one the request carried,
    /// not the client's raw spelling (`sym/../f`) that could resolve elsewhere
    /// on the server's side.
    ///
    /// # Errors
    ///
    /// Returns an error if the item's URI is not a workspace file, the
    /// document cannot be gated or opened, or the LSP request fails.
    pub(super) async fn item_request<R>(
        &self,
        item: CheckedHierarchyItem,
        capability: Capability,
    ) -> Result<Positioned<R::Result>>
    where
        R: lsp_types::Request,
        R::Params: FromItem,
    {
        let item_uri = lsp_types::Uri::from(item.uri());
        let path = self.parse_file_uri(&item_uri).await?;
        let doc = self
            .prepare_gated_document_for_path(&path, IndexingGate::Required(capability))
            .await?;
        let ctx = self.encoding_ctx(doc.server_id());
        let lsp_item = hierarchy_item_to_lsp(item, doc.uri().clone(), &ctx).await;
        let client = doc.client();
        let result = client
            .request_typed::<R>(
                R::Params::from_item(lsp_item),
                <R::Params as RequestTimeout>::timeout(client),
            )
            .await?;
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
    fn test_from_range_builds_params_for_each_range_request() {
        let range = lsp_types::Range {
            start: lsp_types::Position {
                line: 1,
                character: 2,
            },
            end: lsp_types::Position {
                line: 3,
                character: 4,
            },
        };
        let document = position_params().text_document;

        let hints = InlayHintParams::from_range(document.clone(), range, ());
        assert_eq!(hints.range, range);
        assert_eq!(hints.text_document, document);

        let format = DocumentRangeFormattingParams::from_range(
            document.clone(),
            range,
            FormattingOptions {
                tab_size: 2,
                insert_spaces: true,
                ..Default::default()
            },
        );
        assert_eq!(format.range, range);
        assert_eq!(format.options.tab_size, 2);

        let actions = CodeActionParams::from_range(
            document,
            range,
            CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
        );
        assert_eq!(actions.range, range);
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

    #[test]
    fn test_request_timeout_is_per_params_type() {
        let client = LspClient::new(crate::config::LspServerConfig::rust_analyzer());
        assert_eq!(
            <CompletionParams as RequestTimeout>::timeout(&client),
            client.completion_timeout()
        );
        assert!(client.completion_timeout() < client.request_timeout());
        assert_eq!(
            <InlayHintParams as RequestTimeout>::timeout(&client),
            client.request_timeout()
        );
        assert_eq!(
            <SignatureHelpParams as RequestTimeout>::timeout(&client),
            client.request_timeout()
        );
    }
}
