//! Rename, prepare-rename, format-document, format-range, and code-actions handlers.

use std::collections::{HashMap, HashSet};

use lsp_types::{
    DocumentFormattingParams, FormattingOptions, TextDocumentIdentifier, WorkDoneProgressParams,
};
use tokio::task::JoinSet;

use super::Translator;
use super::diagnostics::diagnostic_to_mcp;
use super::dto::{
    BoundedRange, CodeAction, CodeActionsResult, CommandDescription, DocumentChanges, DroppedEdits,
    FormatDocumentResult, NewName, Position, PrepareRenameOutcome, PrepareRenameResult,
    RenameResult, TabSize, TextEdit, WorkspaceEditDescription,
};
use super::encoding_ctx::EncodingCtx;
use super::kind_filter::CodeActionKindFilter;
use super::navigation::ItemBudget;
use super::positioned::Positioned;
use super::routing::{Capability, FileLocalCapability, IndexingGate};
use crate::bridge::{ClientPath, WorkspaceRoots};
use crate::config::ServerId;
use crate::error::{Error, McpErrorKind, Result};
use crate::escape_control;
use crate::lsp::{LspClient, UnclassifiedError};
use crate::redaction::Redactions;

/// Convert a raw LSP `WorkspaceEdit` into MCP `DocumentChanges`.
///
/// Prefers the legacy `changes` map (`HashMap<Uri, Vec<TextEdit>>`) and falls
/// back to `documentChanges` (the array form some servers, e.g.
/// rust-analyzer, use instead) only when `changes` is `None` or an empty
/// map. The choice of source is made once, up front, from the *raw* map's
/// presence and emptiness -- decided before any per-entry filtering -- never
/// from whether filtering happened to leave zero surviving entries. This
/// matters because `changes` and `documentChanges` each get their own
/// [`DroppedEdits`] tally: branching on the post-filter result instead would
/// reintroduce #475 M1, where a `changes` map that exists but whose entries
/// are all filtered out falls through to `documentChanges` and either
/// double-counts or loses the `changes`-side drops. The fallback order is
/// safe because mcpls's advertised client capabilities (`lsp/lifecycle.rs`)
/// do not set `workspace.workspaceEdit.documentChanges`, so per LSP 3.17 a
/// spec-compliant server must always populate `changes`; the
/// `documentChanges`-only fallback exists solely for common non-compliant
/// servers (e.g. rust-analyzer) -- do not "fix" this to prefer
/// `documentChanges` first without first advertising that capability.
///
/// When both fields are present, `changes` wins but `documentChanges` is
/// still tallied so nothing is discarded untraced (#498): file operations
/// count as `unsupported_file_operation`, and a text-document edit whose URI
/// `changes` does not already name counts as `shadowed_by_changes`. A URI
/// present in both is a mirror, not a drop. An entry outside
/// `workspace_roots` is dropped rather than rewritten into the response --
/// see [`WorkspaceRoots::admits_edit_uri`]. Edits beyond
/// `MAX_NORMALIZED_LOCATIONS` are counted in `exceeds_item_cap` (#487): each
/// file's edit list is admitted whole or not at all, files in URI order, from
/// the caller's `budget`, which the caller shares across every edit of one
/// response. `edit_kind` names the caller for the dropped-entry log line
/// (e.g. `"rename edit"`, `"code-action edit"`).
async fn convert_workspace_edit(
    edit: lsp_types::WorkspaceEdit,
    ctx: &EncodingCtx,
    workspace_roots: &WorkspaceRoots,
    edit_kind: &str,
    budget: &mut ItemBudget,
) -> (Vec<DocumentChanges>, DroppedEdits) {
    let mut converter = WorkspaceEditConverter {
        ctx,
        workspace_roots,
        edit_kind,
        budget,
        dropped: DroppedEdits::default(),
        changes: Vec::new(),
    };

    if let Some(changes_map) = edit.changes.filter(|m| !m.is_empty()) {
        let mirrored: HashSet<lsp_types::Uri> = changes_map.keys().cloned().collect();
        converter.convert_changes_map(changes_map).await;
        if let Some(document_changes) = edit.document_changes {
            converter.tally_shadowed(document_changes, &mirrored);
        }
    } else if let Some(document_changes) = edit.document_changes {
        converter.convert_document_changes(document_changes).await;
    }

    (converter.changes, converter.dropped)
}

/// Mutable state threaded through [`convert_workspace_edit`]'s two source
/// branches so they share one item budget and one drop tally.
struct WorkspaceEditConverter<'a> {
    ctx: &'a EncodingCtx,
    workspace_roots: &'a WorkspaceRoots,
    edit_kind: &'a str,
    budget: &'a mut ItemBudget,
    dropped: DroppedEdits,
    changes: Vec<DocumentChanges>,
}

impl WorkspaceEditConverter<'_> {
    /// Whether `uri` may be written through (see
    /// [`WorkspaceRoots::admits_edit_uri`]: exact-case lexical containment plus
    /// canonical containment); an outside URI is logged and tallied in
    /// `dropped.out_of_workspace` as a side effect.
    async fn check_in_workspace(&mut self, uri: &lsp_types::Uri) -> bool {
        let in_workspace = self.workspace_roots.admits_edit_uri(uri).await;
        if !in_workspace {
            tracing::warn!(
                uri = uri.as_ref(),
                "dropping out-of-workspace {}",
                self.edit_kind
            );
            self.dropped.out_of_workspace = self.dropped.out_of_workspace.saturating_add(1);
        }
        in_workspace
    }

    /// Admits one file's whole edit list against the budget, or tallies all
    /// of it as over the cap and returns `None`. Snippet edits are admitted
    /// (and so spend budget) before they are dropped as unsupported.
    fn admit_edits<T>(&mut self, edits: Vec<T>) -> Option<Vec<T>> {
        let total = edits.len();
        let admitted = self.budget.admit_whole(edits);
        if admitted.is_none() {
            self.dropped.exceeds_item_cap = self.dropped.exceeds_item_cap.saturating_add(total);
        }
        admitted
    }

    /// An entry whose edits were all dropped is left out; one that arrived
    /// empty still round-trips.
    fn push_entry(&mut self, uri: String, edits: Vec<TextEdit>, entry_dropped: usize) {
        if edits.is_empty() && entry_dropped > 0 {
            return;
        }
        self.changes.push(DocumentChanges { uri, edits });
    }

    async fn convert_changes_map(
        &mut self,
        changes_map: HashMap<lsp_types::Uri, Vec<lsp_types::TextEdit>>,
    ) {
        let mut entries: Vec<_> = changes_map.into_iter().collect();
        entries.sort_by_cached_key(|(uri, _)| uri.to_string());
        for (uri, edits) in entries {
            if !self.check_in_workspace(&uri).await {
                continue;
            }
            let Some(edits) = self.admit_edits(edits) else {
                continue;
            };
            let mut text_edits = Vec::with_capacity(edits.len());
            for e in edits {
                text_edits.push(TextEdit {
                    range: self.ctx.normalize_range(&uri, e.range).await,
                    new_text: e.new_text,
                });
            }
            self.push_entry(uri.to_string(), text_edits, 0);
        }
    }

    async fn convert_document_changes(&mut self, document_changes: Vec<lsp_types::DocumentChange>) {
        for change in document_changes {
            let tde = match change {
                lsp_types::DocumentChange::TextDocumentEdit(e) => e,
                lsp_types::DocumentChange::CreateFile(_)
                | lsp_types::DocumentChange::RenameFile(_)
                | lsp_types::DocumentChange::DeleteFile(_) => {
                    tracing::debug!("dropping unsupported file-operation document change");
                    self.dropped.unsupported_file_operation =
                        self.dropped.unsupported_file_operation.saturating_add(1);
                    continue;
                }
            };
            let edit_uri = tde.text_document.text_document_identifier.uri;
            if !self.check_in_workspace(&edit_uri).await {
                continue;
            }
            let Some(edits) = self.admit_edits(tde.edits) else {
                continue;
            };
            let mut entry_dropped = 0usize;
            let mut text_edits = Vec::with_capacity(edits.len());
            for one_of in edits {
                let text_edit = match one_of {
                    lsp_types::Edit::TextEdit(te) => TextEdit {
                        range: self.ctx.normalize_range(&edit_uri, te.range).await,
                        new_text: te.new_text,
                    },
                    lsp_types::Edit::AnnotatedTextEdit(ate) => TextEdit {
                        range: self
                            .ctx
                            .normalize_range(&edit_uri, ate.text_edit.range)
                            .await,
                        new_text: ate.text_edit.new_text,
                    },
                    // Snippet edits are an LSP 3.18 addition mcpls does not
                    // advertise support for (`WorkspaceEditClientCapabilities`
                    // carries no `snippetEditSupport`). A server can still send
                    // one; its `new_text` would carry literal snippet
                    // placeholder syntax (e.g. `${1:name}`), which would be
                    // written into the user's file as-is if treated as plain
                    // text -- this is dropped instead, consistent with how
                    // `CreateFile`/`RenameFile`/`DeleteFile` are already
                    // dropped above rather than mistranslated.
                    lsp_types::Edit::SnippetTextEdit(_) => {
                        tracing::debug!("dropping unsupported snippet text edit");
                        self.dropped.unsupported_snippet_edit =
                            self.dropped.unsupported_snippet_edit.saturating_add(1);
                        entry_dropped = entry_dropped.saturating_add(1);
                        continue;
                    }
                };
                text_edits.push(text_edit);
            }
            self.push_entry(edit_uri.to_string(), text_edits, entry_dropped);
        }
    }

    fn tally_shadowed(
        &mut self,
        document_changes: Vec<lsp_types::DocumentChange>,
        mirrored: &HashSet<lsp_types::Uri>,
    ) {
        for change in document_changes {
            match change {
                lsp_types::DocumentChange::TextDocumentEdit(tde) => {
                    let uri = &tde.text_document.text_document_identifier.uri;
                    if !mirrored.contains(uri) {
                        tracing::warn!(
                            uri = uri.as_ref(),
                            "ignoring documentChanges entry shadowed by a non-empty changes map in {}",
                            self.edit_kind
                        );
                        self.dropped.shadowed_by_changes =
                            self.dropped.shadowed_by_changes.saturating_add(1);
                    }
                }
                lsp_types::DocumentChange::CreateFile(_)
                | lsp_types::DocumentChange::RenameFile(_)
                | lsp_types::DocumentChange::DeleteFile(_) => {
                    tracing::debug!("dropping unsupported file-operation document change");
                    self.dropped.unsupported_file_operation =
                        self.dropped.unsupported_file_operation.saturating_add(1);
                }
            }
        }
    }
}

/// Upper bound on the number of `codeAction/resolve` round-trips attempted
/// for a single `handle_code_actions` call.
///
/// mcpls advertises both `data_support` and `resolve_support: ["edit"]`
/// (`lsp/lifecycle.rs`), so a server such as rust-analyzer may defer the
/// edit on every returned action -- realistically 5-20 for a single cursor
/// position. Resolves run concurrently (see `handle_code_actions`) and each
/// is capped by `LspClient::code_action_resolve_timeout`, but an unbounded
/// count would still let one tool call fan out an unbounded number of LSP
/// requests. Actions beyond this limit are returned without an edit, exactly
/// as they were before this round-trip existed (#432).
const MAX_CODE_ACTION_RESOLVES: usize = 20;

/// Resolve a code action's deferred `edit` via `codeAction/resolve`.
///
/// Per LSP 3.16, a server may return a `CodeAction` with `data: Some(_)` and
/// `edit: None` from `textDocument/codeAction`, expecting the client to
/// follow up with `codeAction/resolve` to obtain the actual edit (#432) --
/// mcpls advertises `resolve_support` for `edit` (`lsp/lifecycle.rs`) but
/// previously never issued that follow-up request, so such an action was
/// always reported to the MCP caller with no edit at all.
///
/// Returns the *original* `action` with only its `edit` field replaced by
/// the resolve response's `edit`, rather than the resolve response
/// wholesale: mcpls's `resolve_support` advertises exactly one resolvable
/// property (`edit`), so a server that builds a fresh `CodeAction` in its
/// resolve handler instead of mutating the one it was handed could otherwise
/// silently drop `kind`, `command`, `diagnostics`, `is_preferred`, or
/// `disabled`.
///
/// Falls back to the original, edit-less `action` on any resolve failure
/// (timeout, server error, or a server that does not actually implement
/// resolve despite advertising it) -- a missing edit is strictly better than
/// failing the whole `code_actions` call over one unresolvable action.
async fn resolve_code_action(
    client: &LspClient,
    server_id: &ServerId,
    mut action: lsp_types::CodeAction,
) -> lsp_types::CodeAction {
    let outcome = client
        .request_typed::<lsp_types::CodeActionResolveRequest>(
            action.clone(),
            client.code_action_resolve_timeout(),
        )
        .await;
    match outcome {
        Ok(resolved) => {
            if resolved.edit.is_some() {
                action.edit = resolved.edit;
            } else {
                tracing::debug!(
                    %server_id,
                    title = %action.title,
                    "codeAction/resolve succeeded but returned no edit"
                );
            }
        }
        Err(err) => {
            tracing::warn!(
                %server_id,
                title = %action.title,
                error = %err,
                "codeAction/resolve failed, returning action without edit"
            );
        }
    }
    action
}

/// Resolves up to [`MAX_CODE_ACTION_RESOLVES`] deferred actions in `entries`
/// concurrently -- via [`resolve_code_action`] -- replacing each resolved
/// entry in place. No-op when `resolve_supported` is `false`.
///
/// Runs the round-trips through a [`JoinSet`] rather than sequentially so
/// this call's added latency stays close to one resolve's, not proportional
/// to how many deferred actions the response contains (#432).
async fn resolve_deferred_code_actions(
    entries: &mut [lsp_types::CodeActionResponse],
    client: &LspClient,
    server_id: &ServerId,
    resolve_supported: bool,
) {
    if !resolve_supported {
        return;
    }

    let mut resolve_tasks = JoinSet::new();
    let mut skipped_due_to_cap = 0usize;
    for (index, entry) in entries.iter().enumerate() {
        let lsp_types::CodeActionResponse::CodeAction(action) = entry else {
            continue;
        };
        if action.edit.is_some() || action.data.is_none() {
            continue;
        }
        if resolve_tasks.len() >= MAX_CODE_ACTION_RESOLVES {
            skipped_due_to_cap = skipped_due_to_cap.saturating_add(1);
            continue;
        }
        let client = client.clone();
        let server_id = server_id.clone();
        let action = action.clone();
        resolve_tasks.spawn(async move {
            (
                index,
                resolve_code_action(&client, &server_id, action).await,
            )
        });
    }
    if skipped_due_to_cap > 0 {
        tracing::warn!(
            %server_id,
            skipped = skipped_due_to_cap,
            cap = MAX_CODE_ACTION_RESOLVES,
            "codeAction/resolve cap reached, returning some actions without edit"
        );
    }

    while let Some(result) = resolve_tasks.join_next().await {
        match result {
            Ok((index, resolved_action)) => {
                if let Some(slot) = entries.get_mut(index) {
                    *slot = lsp_types::CodeActionResponse::CodeAction(resolved_action);
                }
            }
            Err(join_err) => {
                // The original, edit-less action already in `entries` is
                // kept as-is -- same graceful-degradation outcome as a
                // resolve request that returns an LSP error.
                tracing::warn!(
                    %server_id,
                    error = %join_err,
                    "codeAction/resolve task panicked, returning action without edit"
                );
            }
        }
    }
}

/// Convert LSP code action to MCP code action. `uri` is the queried
/// document's own URI, used for the action's `diagnostics` (always scoped to
/// the requested document); `edit`'s per-file URIs (from either `changes` or
/// `documentChanges`) are each checked against `workspace_roots` before being
/// trusted -- see [`WorkspaceRoots::admits_edit_uri`].
async fn convert_code_action(
    action: lsp_types::CodeAction,
    ctx: &EncodingCtx,
    uri: &lsp_types::Uri,
    workspace_roots: &WorkspaceRoots,
    budget: &mut ItemBudget,
) -> CodeAction {
    let diagnostics = match action.diagnostics {
        Some(diags) => {
            let diags = budget.admit(diags);
            let mut result = Vec::with_capacity(diags.len());
            for d in &diags {
                result.push(diagnostic_to_mcp(d, ctx, uri).await);
            }
            result
        }
        None => Vec::new(),
    };

    let edit = match action.edit {
        Some(edit) => {
            let (changes, dropped) =
                convert_workspace_edit(edit, ctx, workspace_roots, "code-action edit", budget)
                    .await;
            Some(WorkspaceEditDescription { changes, dropped })
        }
        None => None,
    };

    let command = action.command.map(|cmd| {
        let arguments = cmd.arguments.unwrap_or_else(Vec::new);
        CommandDescription {
            title: cmd.title,
            command: cmd.command,
            arguments,
        }
    });

    CodeAction {
        title: action.title,
        kind: action.kind.map(String::from),
        diagnostics,
        edit,
        command,
        is_preferred: action.is_preferred.unwrap_or(false),
    }
}

/// Convert LSP formatting edits into MCP edits in 1-based coordinates.
async fn convert_text_edits(
    edits: Vec<lsp_types::TextEdit>,
    ctx: &EncodingCtx,
    uri: &lsp_types::Uri,
) -> Vec<TextEdit> {
    let mut converted = Vec::with_capacity(edits.len());
    for edit in edits {
        converted.push(TextEdit {
            range: ctx.normalize_range(uri, edit.range).await,
            new_text: edit.new_text,
        });
    }
    converted
}

/// The code of a `textDocument/prepareRename` failure that means "no renameable
/// symbol here": `InvalidParams` (rust-analyzer) or the catch-all
/// `UnknownErrorCode` (clangd).
fn not_renameable_code(code: i32) -> Option<lsp_types::ErrorCodes> {
    let code = lsp_types::ErrorCodes::from(code);
    matches!(
        code,
        lsp_types::ErrorCodes::InvalidParams | lsp_types::ErrorCodes::UnknownErrorCode
    )
    .then_some(code)
}

/// Classify a failed `prepareRename` request.
///
/// rust-analyzer reports an out-of-range position as `-32602` with its
/// "Invalid offset" text, so `mcp_error_kind` is consulted first: a
/// caller-fault position stays an error and is never read as "not renameable".
fn prepare_rename_rejection(
    err: UnclassifiedError,
    redactions: &Redactions,
) -> Result<PrepareRenameOutcome> {
    let (message, code) = match err.error() {
        Error::LspServerError { code, message, .. }
            if let Some(code) = not_renameable_code(*code)
                && !matches!(
                    err.error().mcp_error_kind(),
                    McpErrorKind::InvalidPosition(_)
                ) =>
        {
            (escape_control(message).into_owned(), code)
        }
        _ => return Err(err.surface()),
    };
    drop(err.handled());
    if code == lsp_types::ErrorCodes::UnknownErrorCode {
        tracing::warn!(
            "prepareRename failed with the catch-all UnknownErrorCode; reported as not_renameable: {}",
            crate::util::truncate_str(
                &redactions.apply(&message),
                crate::util::MAX_LOG_STRING_BYTES
            )
        );
    }
    Ok(PrepareRenameOutcome::NotRenameable {
        server_message: Some(message),
    })
}

impl Translator {
    /// Handle rename request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened, the routed server
    /// does not advertise `renameProvider` support, or the server is still
    /// indexing the workspace (see `Translator::wait_for_indexing_ready`) --
    /// a rename needs the same whole-workspace reference index as
    /// `get_references`.
    pub async fn handle_rename(
        &self,
        file_path: ClientPath,
        position: Position,
        new_name: NewName,
    ) -> Result<RenameResult> {
        let Positioned {
            result: response,
            ctx,
            doc: _doc,
        } = self
            .position_request::<lsp_types::RenameRequest>(
                &file_path,
                position,
                IndexingGate::Required(Capability::Rename),
                new_name.into_string(),
            )
            .await?;

        let (changes, dropped) = if let Some(edit) = response {
            convert_workspace_edit(
                edit,
                &ctx,
                &self.workspace_roots,
                "rename edit",
                &mut ItemBudget::new(),
            )
            .await
        } else {
            (vec![], DroppedEdits::default())
        };

        Ok(RenameResult {
            changes,
            dropped,
            positions_degraded: ctx.positions_degraded(),
        })
    }

    /// Handle a prepare rename request: whether the symbol at `position` can
    /// be renamed, and the range a rename would replace.
    ///
    /// Routes with `rename_symbol` (the `Rename` route), so
    /// the verdict comes from the server that would perform the rename. A
    /// `null` answer, or `defaultBehavior: false`, carries no range and no
    /// permission to rename, so both read as not renameable.
    ///
    /// # Errors
    ///
    /// Returns an error if the position is invalid or its line lies beyond
    /// the end of the document ([`Error::PositionBeyondDocument`]), the LSP
    /// request fails for a reason other than rejecting the position, the
    /// file cannot be opened, the routed server does not advertise rename
    /// preparation support (`renameProvider.prepareProvider`), or the server
    /// is still indexing the workspace.
    pub async fn handle_prepare_rename(
        &self,
        file_path: ClientPath,
        position: Position,
    ) -> Result<PrepareRenameResult> {
        let Positioned {
            result: response,
            ctx,
            doc,
        } = self
            .position_call::<lsp_types::PrepareRenameRequest>(
                &file_path,
                position,
                IndexingGate::Required(Capability::PrepareRename),
                (),
            )
            .await?
            .send_classified()
            .await;
        let response_uri = doc.uri();
        let outcome = match response {
            Ok(Some(lsp_types::PrepareRenameResult::Range(range))) => {
                PrepareRenameOutcome::Renameable {
                    range: ctx.normalize_range(response_uri, range).await,
                    placeholder: None,
                }
            }
            Ok(Some(lsp_types::PrepareRenameResult::PrepareRenamePlaceholder(p))) => {
                PrepareRenameOutcome::Renameable {
                    range: ctx.normalize_range(response_uri, p.range).await,
                    placeholder: Some(p.placeholder),
                }
            }
            Ok(Some(lsp_types::PrepareRenameResult::PrepareRenameDefaultBehavior(d)))
                if d.default_behavior =>
            {
                PrepareRenameOutcome::DefaultBehavior
            }
            Ok(Some(lsp_types::PrepareRenameResult::PrepareRenameDefaultBehavior(_)) | None) => {
                PrepareRenameOutcome::NotRenameable {
                    server_message: None,
                }
            }
            Err(err) => prepare_rename_rejection(err, &self.server_text_redactions())?,
        };

        Ok(PrepareRenameResult {
            outcome,
            positions_degraded: ctx.positions_degraded(),
        })
    }

    /// Handle format document request.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// or the routed server does not advertise `documentFormattingProvider` support.
    pub async fn handle_format_document(
        &self,
        file_path: ClientPath,
        tab_size: TabSize,
        insert_spaces: bool,
    ) -> Result<FormatDocumentResult> {
        let doc = self
            .prepare_gated_document(
                &file_path,
                IndexingGate::FileLocal(FileLocalCapability::FormatDocument),
            )
            .await?;
        let (server_id, client, uri) = (doc.server_id(), doc.client(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let response_uri = uri.clone();

        let params = DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            options: FormattingOptions {
                tab_size: tab_size.get(),
                insert_spaces,
                ..Default::default()
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        let response = client
            .request_typed::<lsp_types::DocumentFormattingRequest>(params, client.request_timeout())
            .await?;

        Ok(FormatDocumentResult {
            edits: convert_text_edits(response.unwrap_or_default(), &ctx, &response_uri).await,
            positions_degraded: ctx.positions_degraded(),
        })
    }

    /// Handle format range request: formatting edits for only `start..end`.
    ///
    /// # Errors
    ///
    /// Returns an error if the range is invalid (zero or oversized
    /// positions, start after end), a line lies beyond the end of the
    /// document ([`Error::PositionBeyondDocument`]), the LSP request fails,
    /// the file cannot be opened, or the routed server does not advertise
    /// `documentRangeFormattingProvider` support.
    pub async fn handle_format_range(
        &self,
        file_path: ClientPath,
        range: BoundedRange,
        tab_size: TabSize,
        insert_spaces: bool,
    ) -> Result<FormatDocumentResult> {
        let options = FormattingOptions {
            tab_size: tab_size.get(),
            insert_spaces,
            ..Default::default()
        };
        let Positioned {
            result: response,
            ctx,
            doc,
        } = self
            .range_request::<lsp_types::DocumentRangeFormattingRequest>(
                &file_path,
                IndexingGate::FileLocal(FileLocalCapability::FormatRange),
                range.range(),
                options,
            )
            .await?;
        let response_uri = doc.uri().clone();

        Ok(FormatDocumentResult {
            edits: convert_text_edits(response.unwrap_or_default(), &ctx, &response_uri).await,
            positions_degraded: ctx.positions_degraded(),
        })
    }

    /// Handle code actions request.
    ///
    /// For an action returned with `data` present but `edit` absent, and
    /// only when the routed server's `codeActionProvider` advertises
    /// `resolveProvider: true`, follows up with a `codeAction/resolve`
    /// request to populate the edit before returning the action (#432).
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP request fails, the file cannot be opened,
    /// the routed server does not advertise `codeActionProvider` support, or
    /// the server is still indexing the workspace (see
    /// `wait_for_indexing_ready`).
    pub async fn handle_code_actions(
        &self,
        file_path: ClientPath,
        range: BoundedRange,
        kind_filter: Option<CodeActionKindFilter>,
    ) -> Result<CodeActionsResult> {
        let only =
            kind_filter.map(|k| vec![lsp_types::CodeActionKind::from(k.as_str().to_owned())]);

        // Pass empty diagnostics context — rust-analyzer generates code actions
        // based on cursor position and its internal analysis state, not on the
        // passed diagnostics.  Passing stale cached diagnostics (which may lack
        // the internal `data` field ra uses for fix mapping) suppresses results.
        let context = lsp_types::CodeActionContext {
            diagnostics: vec![],
            only,
            trigger_kind: Some(lsp_types::CodeActionTriggerKind::Invoked),
        };

        let Positioned {
            result: response,
            ctx,
            doc,
        } = self
            .range_request::<lsp_types::CodeActionRequest>(
                &file_path,
                IndexingGate::Required(Capability::CodeActions),
                range.range(),
                context,
            )
            .await?;
        let (server_id, client) = (doc.server_id(), doc.client());
        let response_uri = doc.uri().clone();
        let mut budget = ItemBudget::new();
        let mut entries = budget.admit(response.unwrap_or_default());
        let resolve_supported = self.code_action_resolve_supported(server_id);
        resolve_deferred_code_actions(&mut entries, client, server_id, resolve_supported).await;

        let mut actions = Vec::with_capacity(entries.len());
        for action_or_command in entries {
            let action = match action_or_command {
                lsp_types::CodeActionResponse::CodeAction(action) => {
                    convert_code_action(
                        action,
                        &ctx,
                        &response_uri,
                        &self.workspace_roots,
                        &mut budget,
                    )
                    .await
                }
                lsp_types::CodeActionResponse::Command(cmd) => {
                    let arguments = cmd.arguments.unwrap_or_else(Vec::new);
                    CodeAction {
                        title: cmd.title.clone(),
                        kind: None,
                        diagnostics: Vec::new(),
                        edit: None,
                        command: Some(CommandDescription {
                            title: cmd.title,
                            command: cmd.command,
                            arguments,
                        }),
                        is_preferred: false,
                    }
                }
            };
            actions.push(action);
        }

        Ok(CodeActionsResult {
            actions,
            truncated: budget.truncated(),
            positions_degraded: ctx.positions_degraded(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, fs};

    use super::*;
    use crate::bridge::WorkspaceRoots;
    use crate::bridge::translator::dto::DiagnosticSeverity;
    use crate::bridge::translator::testing::*;
    use crate::test_lsp::client_path;

    /// S2/S4 regression: a `documentChanges` entry mixing a plain `TextEdit`
    /// with an `Edit::SnippetTextEdit` (LSP 3.18, reachable even though mcpls
    /// advertises no `snippetEditSupport`) must drop the snippet edit rather
    /// than pass its literal placeholder syntax (`${1:...}`) through as
    /// ordinary replacement text -- `handle_rename` is the one tool that
    /// rewrites the user's files.
    #[tokio::test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "LSP snippet placeholders, not format arguments"
    )]
    async fn test_handle_rename_drops_snippet_text_edit_and_keeps_plain_edits() {
        use std::sync::Arc;
        use std::time::Duration;

        use tempfile::TempDir;
        use tokio::io::BufReader;
        use tokio::time::timeout;
        use url::Url;

        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::RenameProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let file_path = canonical_dir(&dir).join("main.rs");
        fs::write(&file_path, "fn old_name() {}").unwrap();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = file_path.to_str().unwrap().to_string();
            tokio::spawn(async move {
                translator
                    .handle_rename(
                        client_path(path),
                        Position::at(1, 4),
                        NewName::try_new("new_name").unwrap(),
                    )
                    .await
            })
        };

        let file_uri = Url::from_file_path(&file_path).unwrap().to_string();
        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/rename");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!({
                "documentChanges": [
                    {
                        "textDocument": { "uri": file_uri, "version": 1 },
                        "edits": [
                            {
                                "range": {
                                    "start": {"line": 0, "character": 3},
                                    "end": {"line": 0, "character": 11}
                                },
                                "newText": "new_name"
                            },
                            {
                                "range": {
                                    "start": {"line": 0, "character": 0},
                                    "end": {"line": 0, "character": 0}
                                },
                                "snippet": { "value": "${1:comment}\n", "kind": "snippet" }
                            }
                        ]
                    }
                ]
            }),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(result.changes.len(), 1);
        assert_eq!(
            result.changes[0].edits.len(),
            1,
            "the snippet edit must be dropped, not converted to literal text"
        );
        assert_eq!(result.changes[0].edits[0].new_text, "new_name");
        assert!(
            !result.changes[0]
                .edits
                .iter()
                .any(|e| e.new_text.contains("${1:comment}")),
            "snippet placeholder syntax must never appear as literal replacement text"
        );
        assert_eq!(
            result.dropped.unsupported_snippet_edit, 1,
            "the dropped snippet edit must be tallied so callers can tell the rename is incomplete"
        );
        assert_eq!(result.dropped.out_of_workspace, 0);
        assert_eq!(result.dropped.unsupported_file_operation, 0);
    }

    /// #415: a `documentChanges` entry whose URI falls outside every
    /// configured workspace root must be dropped -- the routed LSP server is
    /// a trust boundary, and a compromised/misbehaving server could
    /// otherwise smuggle an out-of-workspace path into a `WorkspaceEdit`
    /// alongside legitimate in-workspace entries.
    #[tokio::test]
    async fn test_handle_rename_drops_out_of_workspace_workspace_edit_entries() {
        use std::sync::Arc;
        use std::time::Duration;

        use tempfile::TempDir;
        use tokio::io::BufReader;
        use tokio::time::timeout;
        use url::Url;

        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::RenameProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let file_path = canonical_dir(&dir).join("main.rs");
        fs::write(&file_path, "fn old_name() {}").unwrap();
        let inside_uri = Url::from_file_path(&file_path).unwrap().to_string();
        let outside_uri = "file:///outside/workspace/evil.rs";

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            let path = file_path.to_str().unwrap().to_string();
            tokio::spawn(async move {
                translator
                    .handle_rename(
                        client_path(path),
                        Position::at(1, 4),
                        NewName::try_new("new_name").unwrap(),
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/rename");

        let mut changes_map = serde_json::Map::new();
        changes_map.insert(
            inside_uri.clone(),
            serde_json::json!([
                {
                    "range": {
                        "start": {"line": 0, "character": 3},
                        "end": {"line": 0, "character": 11}
                    },
                    "newText": "new_name"
                }
            ]),
        );
        changes_map.insert(
            outside_uri.to_string(),
            serde_json::json!([
                {
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 3}
                    },
                    "newText": "evil"
                }
            ]),
        );

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!({ "changes": changes_map }),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap()
            .unwrap();

        assert_eq!(
            result.changes.len(),
            1,
            "the out-of-workspace entry must be dropped, not forwarded"
        );
        assert_eq!(result.changes[0].uri, inside_uri);
        assert_eq!(
            result.dropped.out_of_workspace, 1,
            "the dropped out-of-workspace entry must be tallied so callers can tell the rename is incomplete"
        );
        assert_eq!(result.dropped.unsupported_file_operation, 0);
        assert_eq!(result.dropped.unsupported_snippet_edit, 0);
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_quickfix() {
        use tempfile::TempDir;

        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        translator.set_workspace_roots(
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
        );
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                client_path(test_file.to_str().unwrap()),
                bounded(Position::at(1, 1), Position::at(1, 10)),
                Some(CodeActionKindFilter::QuickFix),
            )
            .await;
        // Will fail due to no LSP server, but validates kind is accepted
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_refactor() {
        use tempfile::TempDir;

        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        translator.set_workspace_roots(
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
        );
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                client_path(test_file.to_str().unwrap()),
                bounded(Position::at(1, 1), Position::at(1, 10)),
                Some(CodeActionKindFilter::Refactor),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_refactor_extract() {
        use tempfile::TempDir;

        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        translator.set_workspace_roots(
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
        );
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                client_path(test_file.to_str().unwrap()),
                bounded(Position::at(1, 1), Position::at(1, 10)),
                Some(CodeActionKindFilter::RefactorExtract),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_valid_kind_source() {
        use tempfile::TempDir;

        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        translator.set_workspace_roots(
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
        );
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                client_path(test_file.to_str().unwrap()),
                bounded(Position::at(1, 1), Position::at(1, 10)),
                Some(CodeActionKindFilter::SourceOrganizeImports),
            )
            .await;
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_handle_code_actions_empty_range() {
        use tempfile::TempDir;

        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        translator.set_workspace_roots(
            WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
        );
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // Empty range (same position) should be valid
        let result = translator
            .handle_code_actions(
                client_path(test_file.to_str().unwrap()),
                bounded(Position::at(1, 5), Position::at(1, 5)),
                None,
            )
            .await;
        // Will fail due to no LSP server, but validates range is accepted
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_convert_code_action_minimal() {
        let lsp_action = lsp_types::CodeAction {
            title: "Fix issue".to_string(),
            kind: None,
            diagnostics: None,
            edit: None,
            command: None,
            is_preferred: None,
            disabled: None,
            tags: None,
            data: None,
        };

        let result = convert_code_action(
            lsp_action,
            &test_ctx(),
            &test_uri(),
            &WorkspaceRoots::default(),
            &mut ItemBudget::new(),
        )
        .await;
        assert_eq!(result.title, "Fix issue");
        assert!(result.kind.is_none());
        assert_eq!(result.diagnostics.len(), 0);
        assert!(result.edit.is_none());
        assert!(result.command.is_none());
        assert!(!result.is_preferred);
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one scenario covering every severity"
    )]
    async fn test_convert_code_action_with_diagnostics_all_severities() {
        let lsp_diagnostics = vec![
            lsp_types::Diagnostic {
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
                severity: Some(lsp_types::DiagnosticSeverity::Error),
                message: "Error message".to_string().into(),
                code: Some(lsp_types::Code::Int(1)),
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 1,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 1,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::Warning),
                message: "Warning message".to_string().into(),
                code: Some(lsp_types::Code::String("W001".to_string())),
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 2,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 2,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::Information),
                message: "Info message".to_string().into(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
            lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 3,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 3,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::Hint),
                message: "Hint message".to_string().into(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
        ];

        let lsp_action = lsp_types::CodeAction {
            title: "Fix all issues".to_string(),
            kind: Some(lsp_types::CodeActionKind::QuickFix),
            diagnostics: Some(lsp_diagnostics),
            edit: None,
            command: None,
            is_preferred: None,
            disabled: None,
            tags: None,
            data: None,
        };

        let result = convert_code_action(
            lsp_action,
            &test_ctx(),
            &test_uri(),
            &WorkspaceRoots::default(),
            &mut ItemBudget::new(),
        )
        .await;
        assert_eq!(result.diagnostics.len(), 4);
        assert_matches!(result.diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_matches!(result.diagnostics[1].severity, DiagnosticSeverity::Warning);
        assert_matches!(
            result.diagnostics[2].severity,
            DiagnosticSeverity::Information
        );
        assert_matches!(result.diagnostics[3].severity, DiagnosticSeverity::Hint);
        assert_eq!(result.diagnostics[0].code, Some("1".to_string()));
        assert_eq!(result.diagnostics[1].code, Some("W001".to_string()));
    }

    #[tokio::test]
    #[allow(
        clippy::mutable_key_type,
        reason = "`lsp_types::Uri` keys are never mutated"
    )]
    async fn test_convert_code_action_with_workspace_edit() {
        use std::collections::HashMap;

        use url::Url;

        let dir = tempfile::TempDir::new().unwrap();
        let file_path = dir.path().join("test.rs");
        fs::write(&file_path, "fn main() {}").unwrap();
        let uri_string = Url::from_file_path(&file_path).unwrap().to_string();
        let uri = lsp_types::Uri::from(uri_string.as_str());
        let mut changes_map = HashMap::new();
        changes_map.insert(
            uri,
            vec![lsp_types::TextEdit {
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
                new_text: "fixed".to_string(),
            }],
        );

        let lsp_action = lsp_types::CodeAction {
            title: "Apply fix".to_string(),
            kind: Some(lsp_types::CodeActionKind::QuickFix),
            diagnostics: None,
            edit: Some(lsp_types::WorkspaceEdit {
                changes: Some(changes_map),
                document_changes: None,
                change_annotations: None,
            }),
            command: None,
            is_preferred: Some(true),
            disabled: None,
            tags: None,
            data: None,
        };

        let workspace_roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();
        let result = convert_code_action(
            lsp_action,
            &test_ctx(),
            &test_uri(),
            &workspace_roots,
            &mut ItemBudget::new(),
        )
        .await;
        assert!(result.edit.is_some());
        let edit = result.edit.unwrap();
        assert_eq!(edit.changes.len(), 1);
        assert_eq!(edit.changes[0].uri, uri_string);
        assert_eq!(edit.changes[0].edits.len(), 1);
        assert_eq!(edit.changes[0].edits[0].new_text, "fixed");
        assert!(result.is_preferred);
    }

    /// A canonical temp base with `real/a.rs` and `link -> real`; the root
    /// sets that admit the `link` spelling: the configured symlink itself and
    /// the logical `$PWD` of a relative root.
    #[cfg(unix)]
    fn alias_root_sets() -> (tempfile::TempDir, std::path::PathBuf, Vec<WorkspaceRoots>) {
        use crate::bridge::ProcessCwd;

        let dir = tempfile::TempDir::new().unwrap();
        let base = dunce::canonicalize(dir.path()).unwrap();
        let real = base.join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("a.rs"), "fn main() {}").unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let roots = vec![
            WorkspaceRoots::from_paths(std::slice::from_ref(&link)).unwrap(),
            WorkspaceRoots::from_paths_with(&[std::path::PathBuf::from(".")], || {
                Ok(ProcessCwd::new(
                    real.clone(),
                    Some(link.clone().into_os_string()),
                ))
            })
            .unwrap(),
        ];
        (dir, link, roots)
    }

    /// #558: an edit whose URI uses an alias spelling of a root (configured
    /// symlink, logical `$PWD`) is kept, like `WorkspaceRoots::validate`
    /// would admit it; an unrelated URI is still dropped.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_convert_workspace_edit_keeps_alias_spelled_uri() {
        let (_dir, link, root_sets) = alias_root_sets();
        let alias_uri = crate::bridge::path_to_uri(&link.join("a.rs")).unwrap();
        let outside_uri = lsp_types::Uri::from("file:///definitely/elsewhere/b.rs");
        let text_edit = lsp_types::TextEdit {
            range: lsp_types::Range::default(),
            new_text: "x".to_string(),
        };

        for roots in root_sets {
            let edit = lsp_types::WorkspaceEdit {
                changes: Some(HashMap::from([
                    (alias_uri.clone(), vec![text_edit.clone()]),
                    (outside_uri.clone(), vec![text_edit.clone()]),
                ])),
                document_changes: None,
                change_annotations: None,
            };

            let (changes, dropped) = convert_workspace_edit(
                edit,
                &test_ctx(),
                &roots,
                "rename edit",
                &mut ItemBudget::new(),
            )
            .await;

            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].uri, alias_uri.as_ref());
            assert_eq!(dropped.out_of_workspace, 1);
        }
    }

    /// Edits `path` through `roots` with a single-file `changes` map and
    /// returns how many entries were kept and how many were dropped as
    /// out-of-workspace.
    #[cfg(unix)]
    async fn convert_one_edit(roots: &WorkspaceRoots, path: &std::path::Path) -> (usize, usize) {
        let uri = crate::bridge::path_to_uri(path).unwrap();
        let edit = lsp_types::WorkspaceEdit {
            changes: Some(HashMap::from([(
                uri,
                vec![lsp_types::TextEdit {
                    range: lsp_types::Range::default(),
                    new_text: "x".to_string(),
                }],
            )])),
            document_changes: None,
            change_annotations: None,
        };
        let (changes, dropped) = convert_workspace_edit(
            edit,
            &test_ctx(),
            roots,
            "rename edit",
            &mut ItemBudget::new(),
        )
        .await;
        (changes.len(), dropped.out_of_workspace)
    }

    /// Aliases are only a spelling aid: a symlink inside the workspace that
    /// points out, a retargeted alias, and a case-variant spelling are all
    /// dropped even though a lexical check would admit (or fold) them.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_convert_workspace_edit_drops_escaping_retargeted_and_case_variant_uris() {
        let (_dir, link, root_sets) = alias_root_sets();
        let base = link.parent().unwrap().to_path_buf();
        let real = base.join("real");
        let outside = base.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.rs"), "").unwrap();
        std::os::unix::fs::symlink(&outside, real.join("out")).unwrap();

        for roots in &root_sets {
            for escaping in [
                link.join("out/secret.rs"),
                real.join("out/secret.rs"),
                base.join("REAL/a.rs"),
            ] {
                assert_eq!(
                    convert_one_edit(roots, &escaping).await,
                    (0, 1),
                    "{escaping:?}"
                );
            }
            assert_eq!(convert_one_edit(roots, &link.join("a.rs")).await, (1, 0));
        }

        let roots = WorkspaceRoots::from_paths(std::slice::from_ref(&link)).unwrap();
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert_eq!(
            convert_one_edit(&roots, &link.join("secret.rs")).await,
            (0, 1)
        );
    }

    /// #429: a `codeAction` response carrying only `documentChanges` (the
    /// array form some servers, e.g. rust-analyzer, use instead of the
    /// legacy `changes` map) must still populate the action's edit list
    /// rather than silently dropping it.
    #[tokio::test]
    async fn test_convert_code_action_with_document_changes_only() {
        use url::Url;

        let dir = tempfile::TempDir::new().unwrap();
        let file_path = dir.path().join("test.rs");
        fs::write(&file_path, "fn main() {}").unwrap();
        let uri_string = Url::from_file_path(&file_path).unwrap().to_string();
        let uri = lsp_types::Uri::from(uri_string.as_str());
        let text_document_edit = lsp_types::TextDocumentEdit {
            text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                version: Some(1),
                text_document_identifier: TextDocumentIdentifier { uri },
            },
            edits: vec![lsp_types::Edit::TextEdit(lsp_types::TextEdit {
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
                new_text: "fixed".to_string(),
            })],
        };

        let lsp_action = lsp_types::CodeAction {
            title: "Apply fix via documentChanges".to_string(),
            kind: Some(lsp_types::CodeActionKind::QuickFix),
            diagnostics: None,
            edit: Some(lsp_types::WorkspaceEdit {
                changes: None,
                document_changes: Some(vec![lsp_types::DocumentChange::TextDocumentEdit(
                    text_document_edit,
                )]),
                change_annotations: None,
            }),
            command: None,
            is_preferred: Some(true),
            disabled: None,
            tags: None,
            data: None,
        };

        let workspace_roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();
        let result = convert_code_action(
            lsp_action,
            &test_ctx(),
            &test_uri(),
            &workspace_roots,
            &mut ItemBudget::new(),
        )
        .await;
        assert!(result.edit.is_some());
        let edit = result.edit.unwrap();
        assert_eq!(edit.changes.len(), 1);
        assert_eq!(edit.changes[0].uri, uri_string);
        assert_eq!(edit.changes[0].edits.len(), 1);
        assert_eq!(edit.changes[0].edits[0].new_text, "fixed");
        assert!(result.is_preferred);
    }

    /// #429 companion: when a `WorkspaceEdit` carries both `changes` and
    /// `documentChanges`, `changes` must win and `documentChanges` must be
    /// tallied as shadowed -- matching the precedence `convert_workspace_edit`
    /// already applies for `handle_rename`.
    #[tokio::test]
    #[allow(
        clippy::mutable_key_type,
        reason = "`lsp_types::Uri` keys are never mutated"
    )]
    async fn test_convert_code_action_changes_takes_precedence_over_document_changes() {
        use std::collections::HashMap;

        use url::Url;

        let dir = tempfile::TempDir::new().unwrap();
        let changes_path = dir.path().join("changes.rs");
        fs::write(&changes_path, "fn changes() {}").unwrap();
        let changes_uri_string = Url::from_file_path(&changes_path).unwrap().to_string();
        let changes_uri = lsp_types::Uri::from(changes_uri_string.as_str());
        let mut changes_map = HashMap::new();
        changes_map.insert(
            changes_uri,
            vec![lsp_types::TextEdit {
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
                new_text: "from_changes".to_string(),
            }],
        );

        let document_changes_path = dir.path().join("document_changes.rs");
        fs::write(&document_changes_path, "fn document_changes() {}").unwrap();
        let document_changes_uri = lsp_types::Uri::from(
            Url::from_file_path(&document_changes_path)
                .unwrap()
                .as_str(),
        );
        let text_document_edit = lsp_types::TextDocumentEdit {
            text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                version: Some(1),
                text_document_identifier: TextDocumentIdentifier {
                    uri: document_changes_uri,
                },
            },
            edits: vec![lsp_types::Edit::TextEdit(lsp_types::TextEdit {
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
                new_text: "from_document_changes".to_string(),
            })],
        };

        let lsp_action = lsp_types::CodeAction {
            title: "Apply fix".to_string(),
            kind: Some(lsp_types::CodeActionKind::QuickFix),
            diagnostics: None,
            edit: Some(lsp_types::WorkspaceEdit {
                changes: Some(changes_map),
                document_changes: Some(vec![lsp_types::DocumentChange::TextDocumentEdit(
                    text_document_edit,
                )]),
                change_annotations: None,
            }),
            command: None,
            is_preferred: None,
            disabled: None,
            tags: None,
            data: None,
        };

        let workspace_roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();
        let result = convert_code_action(
            lsp_action,
            &test_ctx(),
            &test_uri(),
            &workspace_roots,
            &mut ItemBudget::new(),
        )
        .await;
        let edit = result.edit.unwrap();
        assert_eq!(
            edit.changes.len(),
            1,
            "only the `changes` entry should be present"
        );
        assert_eq!(edit.changes[0].uri, changes_uri_string);
        assert_eq!(edit.changes[0].edits[0].new_text, "from_changes");
        assert_eq!(
            edit.dropped.shadowed_by_changes, 1,
            "a documentChanges entry shadowed by a non-empty changes map must be tallied (#498)"
        );
        assert_eq!(edit.dropped.out_of_workspace, 0);
    }

    /// #429 companion / #415 parity: a `documentChanges` entry whose URI
    /// falls outside every configured workspace root must be dropped, the
    /// same trust-boundary check `changes` entries already get.
    #[tokio::test]
    async fn test_convert_code_action_document_changes_drops_out_of_workspace_entry() {
        use url::Url;

        let dir = tempfile::TempDir::new().unwrap();
        let inside_path = dir.path().join("inside.rs");
        fs::write(&inside_path, "fn inside() {}").unwrap();
        let inside_uri_string = Url::from_file_path(&inside_path).unwrap().to_string();
        let inside_uri = lsp_types::Uri::from(inside_uri_string.as_str());
        let outside_uri = lsp_types::Uri::from("file:///outside/workspace/evil.rs");

        let make_edit = |uri: lsp_types::Uri, new_text: &str| {
            lsp_types::DocumentChange::TextDocumentEdit(lsp_types::TextDocumentEdit {
                text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                    version: Some(1),
                    text_document_identifier: TextDocumentIdentifier { uri },
                },
                edits: vec![lsp_types::Edit::TextEdit(lsp_types::TextEdit {
                    range: lsp_types::Range {
                        start: lsp_types::Position {
                            line: 0,
                            character: 0,
                        },
                        end: lsp_types::Position {
                            line: 0,
                            character: 3,
                        },
                    },
                    new_text: new_text.to_string(),
                })],
            })
        };

        let lsp_action = lsp_types::CodeAction {
            title: "Apply fix".to_string(),
            kind: Some(lsp_types::CodeActionKind::QuickFix),
            diagnostics: None,
            edit: Some(lsp_types::WorkspaceEdit {
                changes: None,
                document_changes: Some(vec![
                    make_edit(inside_uri, "fixed"),
                    make_edit(outside_uri, "evil"),
                ]),
                change_annotations: None,
            }),
            command: None,
            is_preferred: None,
            disabled: None,
            tags: None,
            data: None,
        };

        let workspace_roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();
        let result = convert_code_action(
            lsp_action,
            &test_ctx(),
            &test_uri(),
            &workspace_roots,
            &mut ItemBudget::new(),
        )
        .await;
        let edit = result.edit.unwrap();
        assert_eq!(
            edit.changes.len(),
            1,
            "the out-of-workspace entry must be dropped, not forwarded"
        );
        assert_eq!(edit.changes[0].uri, inside_uri_string);
        assert_eq!(edit.changes[0].edits[0].new_text, "fixed");
        assert_eq!(
            edit.dropped.out_of_workspace, 1,
            "the dropped out-of-workspace entry must be tallied so callers can tell the code action is incomplete"
        );
    }

    /// #475: `CreateFile`/`RenameFile`/`DeleteFile` document changes -- e.g.
    /// rust-analyzer emitting `RenameFile` for a module rename -- must each be
    /// tallied under `dropped.unsupported_file_operation`, distinct from the
    /// other two drop reasons, while a plain `TextDocumentEdit` in the same
    /// response still survives.
    #[tokio::test]
    async fn test_convert_workspace_edit_tallies_dropped_file_operations() {
        use url::Url;

        let dir = tempfile::TempDir::new().unwrap();
        let file_path = dir.path().join("kept.rs");
        fs::write(&file_path, "fn kept() {}").unwrap();
        let uri_string = Url::from_file_path(&file_path).unwrap().to_string();
        let uri = lsp_types::Uri::from(uri_string.as_str());

        let text_document_edit = lsp_types::TextDocumentEdit {
            text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                version: Some(1),
                text_document_identifier: TextDocumentIdentifier { uri: uri.clone() },
            },
            edits: vec![lsp_types::Edit::TextEdit(lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 2,
                    },
                },
                new_text: "kept".to_string(),
            })],
        };

        let edit = lsp_types::WorkspaceEdit {
            changes: None,
            document_changes: Some(vec![
                lsp_types::DocumentChange::TextDocumentEdit(text_document_edit),
                lsp_types::DocumentChange::CreateFile(lsp_types::CreateFile {
                    uri: lsp_types::Uri::from("file:///workspace/new.rs"),
                    options: None,
                    annotation_id: None,
                }),
                lsp_types::DocumentChange::RenameFile(lsp_types::RenameFile {
                    old_uri: lsp_types::Uri::from("file:///workspace/old_module.rs"),
                    new_uri: lsp_types::Uri::from("file:///workspace/new_module.rs"),
                    options: None,
                    annotation_id: None,
                }),
                lsp_types::DocumentChange::DeleteFile(lsp_types::DeleteFile {
                    uri: lsp_types::Uri::from("file:///workspace/gone.rs"),
                    options: None,
                    annotation_id: None,
                }),
            ]),
            change_annotations: None,
        };

        let workspace_roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();
        let (changes, dropped) = convert_workspace_edit(
            edit,
            &test_ctx(),
            &workspace_roots,
            "rename edit",
            &mut ItemBudget::new(),
        )
        .await;

        assert_eq!(
            changes.len(),
            1,
            "the plain TextDocumentEdit must survive alongside the dropped file operations"
        );
        assert_eq!(changes[0].uri, uri_string);
        assert_eq!(
            dropped.unsupported_file_operation, 3,
            "CreateFile, RenameFile, and DeleteFile must each be tallied"
        );
        assert_eq!(dropped.out_of_workspace, 0);
        assert_eq!(dropped.unsupported_snippet_edit, 0);
    }

    /// #475: when every entry in a `WorkspaceEdit` is filtered out, the
    /// resulting `changes` list is empty just like "nothing to rename" would
    /// be -- `dropped` is what makes the two cases distinguishable.
    #[tokio::test]
    #[allow(
        clippy::mutable_key_type,
        reason = "`lsp_types::Uri` keys are never mutated"
    )]
    async fn test_convert_workspace_edit_everything_dropped_is_distinguishable_from_no_edits() {
        use std::collections::HashMap;

        let outside_uri = lsp_types::Uri::from("file:///outside/workspace/evil.rs");
        let mut changes_map = HashMap::new();
        changes_map.insert(
            outside_uri,
            vec![lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 3,
                    },
                },
                new_text: "evil".to_string(),
            }],
        );
        let all_dropped_edit = lsp_types::WorkspaceEdit {
            changes: Some(changes_map),
            document_changes: None,
            change_annotations: None,
        };

        let dir = tempfile::TempDir::new().unwrap();
        let workspace_roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();
        let (changes, dropped) = convert_workspace_edit(
            all_dropped_edit,
            &test_ctx(),
            &workspace_roots,
            "rename edit",
            &mut ItemBudget::new(),
        )
        .await;
        assert!(changes.is_empty());
        assert!(
            !dropped.is_empty(),
            "an edit where everything was withheld must not look like an edit with nothing to do"
        );
        assert_eq!(dropped.out_of_workspace, 1);

        let no_op_edit = lsp_types::WorkspaceEdit {
            changes: None,
            document_changes: None,
            change_annotations: None,
        };
        let (changes, dropped) = convert_workspace_edit(
            no_op_edit,
            &test_ctx(),
            &workspace_roots,
            "rename edit",
            &mut ItemBudget::new(),
        )
        .await;
        assert!(changes.is_empty());
        assert!(
            dropped.is_empty(),
            "a genuinely empty edit must not be reported as having withheld anything"
        );
    }

    /// #475 M1: a `WorkspaceEdit` populating both `changes` and
    /// `documentChanges` with the same withheld entry must not tally it
    /// twice -- `changes` takes precedence, and a `documentChanges` entry
    /// mirroring one of its URIs is not tallied again.
    #[tokio::test]
    #[allow(
        clippy::mutable_key_type,
        reason = "`lsp_types::Uri` keys are never mutated"
    )]
    async fn test_convert_workspace_edit_changes_precedence_avoids_double_counting_drops() {
        use std::collections::HashMap;

        let outside_uri = lsp_types::Uri::from("file:///outside/workspace/evil.rs");
        let mut changes_map = HashMap::new();
        changes_map.insert(
            outside_uri.clone(),
            vec![lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 3,
                    },
                },
                new_text: "evil".to_string(),
            }],
        );

        let text_document_edit = lsp_types::TextDocumentEdit {
            text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                version: Some(1),
                text_document_identifier: TextDocumentIdentifier { uri: outside_uri },
            },
            edits: vec![lsp_types::Edit::TextEdit(lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 3,
                    },
                },
                new_text: "evil".to_string(),
            })],
        };

        let edit = lsp_types::WorkspaceEdit {
            changes: Some(changes_map),
            document_changes: Some(vec![lsp_types::DocumentChange::TextDocumentEdit(
                text_document_edit,
            )]),
            change_annotations: None,
        };

        let dir = tempfile::TempDir::new().unwrap();
        let workspace_roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();
        let (changes, dropped) = convert_workspace_edit(
            edit,
            &test_ctx(),
            &workspace_roots,
            "rename edit",
            &mut ItemBudget::new(),
        )
        .await;

        assert!(changes.is_empty());
        assert_eq!(
            dropped.out_of_workspace, 1,
            "documentChanges must be ignored entirely once changes is present, not merged in \
             and double-tallied"
        );
    }

    /// #475 M1 asymmetric case: `changes` populates entries that are *all*
    /// dropped while `documentChanges` separately carries a distinct,
    /// in-workspace edit that would fully succeed. A design that falls back
    /// to `documentChanges` whenever `changes` yields no *surviving* entries
    /// (rather than deciding up front from field presence) would process
    /// `documentChanges` here, discarding the real `changes` drops in the
    /// process -- reporting `dropped.is_empty()` even though entries were
    /// genuinely withheld. `changes` being present must keep its own drop
    /// count intact regardless of what `documentChanges` separately contains.
    #[tokio::test]
    #[allow(
        clippy::mutable_key_type,
        reason = "`lsp_types::Uri` keys are never mutated"
    )]
    async fn test_convert_workspace_edit_changes_precedence_keeps_drops_when_document_changes_would_succeed()
     {
        use std::collections::HashMap;

        use url::Url;

        let dir = tempfile::TempDir::new().unwrap();
        let mut changes_map = HashMap::new();
        changes_map.insert(
            lsp_types::Uri::from("file:///outside/workspace/one.rs"),
            vec![lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 3,
                    },
                },
                new_text: "evil".to_string(),
            }],
        );
        changes_map.insert(
            lsp_types::Uri::from("file:///outside/workspace/two.rs"),
            vec![lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 3,
                    },
                },
                new_text: "evil".to_string(),
            }],
        );

        let in_workspace_path = dir.path().join("kept.rs");
        fs::write(&in_workspace_path, "fn kept() {}").unwrap();
        let in_workspace_uri =
            lsp_types::Uri::from(Url::from_file_path(&in_workspace_path).unwrap().as_str());
        let text_document_edit = lsp_types::TextDocumentEdit {
            text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                version: Some(1),
                text_document_identifier: TextDocumentIdentifier {
                    uri: in_workspace_uri,
                },
            },
            edits: vec![lsp_types::Edit::TextEdit(lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 2,
                    },
                },
                new_text: "kept".to_string(),
            })],
        };

        let edit = lsp_types::WorkspaceEdit {
            changes: Some(changes_map),
            document_changes: Some(vec![lsp_types::DocumentChange::TextDocumentEdit(
                text_document_edit,
            )]),
            change_annotations: None,
        };

        let workspace_roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();
        let (changes, dropped) = convert_workspace_edit(
            edit,
            &test_ctx(),
            &workspace_roots,
            "rename edit",
            &mut ItemBudget::new(),
        )
        .await;

        assert!(
            changes.is_empty(),
            "changes takes precedence even though every one of its entries was withheld"
        );
        assert_eq!(
            dropped.out_of_workspace, 2,
            "both changes-branch drops must be tallied, not lost by falling back to documentChanges"
        );
    }

    /// #475 M1 (second bug): `changes` can be present as a literally empty
    /// map (`"changes": {}`, legal per `lsp_types::WorkspaceEdit`) rather
    /// than omitted -- branching on `Option::is_some()` alone would treat
    /// that as "use `changes`", silently discarding a populated
    /// `documentChanges` and returning a result indistinguishable from
    /// "nothing to rename".
    #[tokio::test]
    #[allow(
        clippy::mutable_key_type,
        reason = "`lsp_types::Uri` keys are never mutated"
    )]
    async fn test_convert_workspace_edit_falls_back_to_document_changes_when_changes_map_is_present_but_empty()
     {
        use std::collections::HashMap;

        use url::Url;

        let dir = tempfile::TempDir::new().unwrap();
        let file_path = dir.path().join("kept.rs");
        fs::write(&file_path, "fn kept() {}").unwrap();
        let uri_string = Url::from_file_path(&file_path).unwrap().to_string();
        let uri = lsp_types::Uri::from(uri_string.as_str());

        let text_document_edit = lsp_types::TextDocumentEdit {
            text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                version: Some(1),
                text_document_identifier: TextDocumentIdentifier { uri },
            },
            edits: vec![lsp_types::Edit::TextEdit(lsp_types::TextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: 2,
                    },
                },
                new_text: "kept".to_string(),
            })],
        };

        let edit = lsp_types::WorkspaceEdit {
            changes: Some(HashMap::new()),
            document_changes: Some(vec![lsp_types::DocumentChange::TextDocumentEdit(
                text_document_edit,
            )]),
            change_annotations: None,
        };

        let workspace_roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();
        let (changes, dropped) = convert_workspace_edit(
            edit,
            &test_ctx(),
            &workspace_roots,
            "rename edit",
            &mut ItemBudget::new(),
        )
        .await;

        assert_eq!(
            changes.len(),
            1,
            "an empty-but-present `changes` map must not be treated as authoritative over a \
             populated documentChanges"
        );
        assert_eq!(changes[0].uri, uri_string);
        assert!(dropped.is_empty());
    }

    /// #475: `RenameResult::dropped` must round-trip through the wire format
    /// used by MCP responses -- present with per-reason counts when something
    /// was withheld, and omitted entirely (not `"dropped":{}`) when nothing
    /// was, so existing clients that ignore unknown fields see no change.
    #[test]
    fn test_rename_result_dropped_field_serde_presence() {
        let clean = RenameResult {
            changes: vec![],
            dropped: DroppedEdits::default(),
            positions_degraded: None,
        };
        let clean_json = serde_json::to_value(&clean).unwrap();
        assert!(
            clean_json.get("dropped").is_none(),
            "an empty DroppedEdits must be omitted from the serialized result, not `dropped: {{}}`"
        );

        let incomplete = RenameResult {
            changes: vec![],
            dropped: DroppedEdits {
                out_of_workspace: 1,
                unsupported_file_operation: 2,
                unsupported_snippet_edit: 0,
                ..DroppedEdits::default()
            },
            positions_degraded: None,
        };
        let incomplete_json = serde_json::to_value(&incomplete).unwrap();
        let dropped_json = incomplete_json
            .get("dropped")
            .expect("non-empty DroppedEdits must be serialized");
        assert_eq!(dropped_json["out_of_workspace"], 1);
        assert_eq!(dropped_json["unsupported_file_operation"], 2);
        assert!(
            dropped_json.get("unsupported_snippet_edit").is_none(),
            "a zero-valued reason must itself be omitted per-field"
        );
    }

    #[tokio::test]
    async fn test_convert_code_action_with_command() {
        let lsp_action = lsp_types::CodeAction {
            title: "Run command".to_string(),
            kind: Some(lsp_types::CodeActionKind::Refactor),
            diagnostics: None,
            edit: None,
            command: Some(lsp_types::Command {
                title: "Execute refactor".to_string(),
                command: "refactor.extract".to_string(),
                arguments: Some(vec![serde_json::json!("arg1"), serde_json::json!(42)]),
                tooltip: None,
            }),
            is_preferred: None,
            disabled: None,
            tags: None,
            data: None,
        };

        let result = convert_code_action(
            lsp_action,
            &test_ctx(),
            &test_uri(),
            &WorkspaceRoots::default(),
            &mut ItemBudget::new(),
        )
        .await;
        assert!(result.command.is_some());
        let cmd = result.command.unwrap();
        assert_eq!(cmd.title, "Execute refactor");
        assert_eq!(cmd.command, "refactor.extract");
        assert_eq!(cmd.arguments.len(), 2);
    }

    /// End-to-end: `handle_code_actions` must surface
    /// `Error::WorkspaceIndexing` -- not an empty result -- while the routed
    /// server is still `Loading`, without reaching the fake LSP server.
    #[tokio::test(start_paused = true)]
    async fn test_handle_code_actions_returns_workspace_indexing_error_when_loading() {
        use std::sync::Arc;

        use tempfile::TempDir;
        use tokio::sync::Mutex;

        use crate::bridge::NotificationCache;
        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            code_action_provider: Some(lsp_types::CodeActionProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, _server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = translator.with_notification_cache(cache);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let err = translator
            .handle_code_actions(
                client_path(path.to_string_lossy().into_owned()),
                bounded(Position::at(1, 1), Position::at(1, 10)),
                None,
            )
            .await
            .unwrap_err();

        assert_matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        );
    }

    /// Companion: when the cache reports `Ready`, `handle_code_actions` must
    /// dispatch normally.
    #[tokio::test]
    async fn test_handle_code_actions_dispatches_when_indexing_ready() {
        use std::sync::Arc;

        use tempfile::TempDir;
        use tokio::io::BufReader;
        use tokio::sync::Mutex;

        use crate::bridge::NotificationCache;
        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            code_action_provider: Some(lsp_types::CodeActionProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        let translator = Arc::new(translator.with_notification_cache(cache));

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_code_actions(
                        client_path(path),
                        bounded(Position::at(1, 1), Position::at(1, 10)),
                        Some(CodeActionKindFilter::SourceOrganizeImports),
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/codeAction");
        assert_eq!(
            request["params"]["context"]["only"],
            serde_json::json!(["source.organizeImports"])
        );

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([]),
        )
        .await;

        let result = handle.await.unwrap().unwrap();
        assert!(result.actions.is_empty());
    }

    /// #432: an action returned with `data` but no `edit`, from a server
    /// advertising `codeActionProvider.resolveProvider: true`, must trigger
    /// a `codeAction/resolve` follow-up whose edit ends up in the result.
    #[tokio::test]
    async fn test_handle_code_actions_resolves_deferred_edit_when_supported() {
        use std::sync::Arc;

        use tempfile::TempDir;
        use tokio::io::BufReader;
        use tokio::sync::Mutex;
        use url::Url;

        use crate::bridge::NotificationCache;
        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            code_action_provider: Some(lsp_types::CodeActionProvider::CodeActionOptions(
                lsp_types::CodeActionOptions {
                    resolve_provider: Some(true),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        let translator = Arc::new(translator.with_notification_cache(cache));

        let file_path = canonical_dir(&dir).join("main.rs");
        fs::write(&file_path, "fn main() {}").unwrap();
        let file_uri = Url::from_file_path(&file_path).unwrap().to_string();

        let handle = {
            let translator = Arc::clone(&translator);
            let path = file_path.to_str().unwrap().to_string();
            tokio::spawn(async move {
                translator
                    .handle_code_actions(
                        client_path(path),
                        bounded(Position::at(1, 1), Position::at(1, 10)),
                        None,
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/codeAction");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "title": "Add missing import",
                "kind": "quickfix",
                "data": {"id": 42},
            }]),
        )
        .await;

        let resolve_request = read_framed_message(&mut wire).await;
        assert_eq!(resolve_request["method"], "codeAction/resolve");
        assert_eq!(resolve_request["params"]["title"], "Add missing import");

        let mut changes_map = serde_json::Map::new();
        changes_map.insert(
            file_uri,
            serde_json::json!([{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 0}
                },
                "newText": "use std::fmt;\n",
            }]),
        );

        write_response(
            &mut server.read_half_stdin,
            &resolve_request["id"],
            serde_json::json!({
                "title": "Add missing import",
                "kind": "quickfix",
                "data": {"id": 42},
                "edit": { "changes": changes_map }
            }),
        )
        .await;

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result.actions.len(), 1);
        let edit = result.actions[0]
            .edit
            .as_ref()
            .expect("edit must be populated by codeAction/resolve");
        assert_eq!(edit.changes.len(), 1);
        assert_eq!(edit.changes[0].edits[0].new_text, "use std::fmt;\n");
    }

    /// #432 companion: a server that does not advertise
    /// `codeActionProvider.resolveProvider: true` must never receive a
    /// `codeAction/resolve` follow-up, even for an action with `data` but no
    /// `edit` -- the action is returned as-is, without an edit.
    #[tokio::test]
    async fn test_handle_code_actions_skips_resolve_when_not_supported() {
        use std::sync::Arc;
        use std::time::Duration;

        use tempfile::TempDir;
        use tokio::io::BufReader;
        use tokio::sync::Mutex;
        use tokio::time::timeout;

        use crate::bridge::NotificationCache;
        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            code_action_provider: Some(lsp_types::CodeActionProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        let translator = Arc::new(translator.with_notification_cache(cache));

        let file_path = dir.path().join("main.rs");
        fs::write(&file_path, "fn main() {}").unwrap();

        let handle = {
            let translator = Arc::clone(&translator);
            let path = file_path.to_str().unwrap().to_string();
            tokio::spawn(async move {
                translator
                    .handle_code_actions(
                        client_path(path),
                        bounded(Position::at(1, 1), Position::at(1, 10)),
                        None,
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/codeAction");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "title": "Add missing import",
                "kind": "quickfix",
                "data": {"id": 42},
            }]),
        )
        .await;

        // No resolve request must ever arrive.
        let no_more_requests =
            timeout(Duration::from_millis(200), read_framed_message(&mut wire)).await;
        assert!(
            no_more_requests.is_err(),
            "codeAction/resolve must not be sent when resolveProvider is unset"
        );

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result.actions.len(), 1);
        assert!(result.actions[0].edit.is_none());
    }

    /// #432: a `codeAction/resolve` request that comes back as a JSON-RPC
    /// error must not fail the whole `code_actions` call -- the action is
    /// still returned, just without an edit (`resolve_code_action`'s `Err`
    /// fallback).
    #[tokio::test]
    async fn test_handle_code_actions_falls_back_when_resolve_errors() {
        use std::sync::Arc;

        use tempfile::TempDir;
        use tokio::io::BufReader;
        use tokio::sync::Mutex;

        use crate::bridge::NotificationCache;
        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            code_action_provider: Some(lsp_types::CodeActionProvider::CodeActionOptions(
                lsp_types::CodeActionOptions {
                    resolve_provider: Some(true),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        let translator = Arc::new(translator.with_notification_cache(cache));

        let file_path = dir.path().join("main.rs");
        fs::write(&file_path, "fn main() {}").unwrap();

        let handle = {
            let translator = Arc::clone(&translator);
            let path = file_path.to_str().unwrap().to_string();
            tokio::spawn(async move {
                translator
                    .handle_code_actions(
                        client_path(path),
                        bounded(Position::at(1, 1), Position::at(1, 10)),
                        None,
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/codeAction");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "title": "Add missing import",
                "kind": "quickfix",
                "data": {"id": 42},
            }]),
        )
        .await;

        let resolve_request = read_framed_message(&mut wire).await;
        assert_eq!(resolve_request["method"], "codeAction/resolve");

        write_error_response(
            &mut server.read_half_stdin,
            &resolve_request["id"],
            -32603,
            "internal error",
        )
        .await;

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result.actions.len(), 1);
        assert_eq!(result.actions[0].title, "Add missing import");
        assert!(
            result.actions[0].edit.is_none(),
            "a resolve error must not propagate, only leave the edit unset"
        );
    }

    /// #432 companion: an action already carrying `edit: Some(_)` must never
    /// be re-resolved, even when it also carries `data: Some(_)` against a
    /// `resolveProvider: true` server.
    #[tokio::test]
    async fn test_handle_code_actions_skips_resolve_when_edit_already_present() {
        use std::sync::Arc;
        use std::time::Duration;

        use tempfile::TempDir;
        use tokio::io::BufReader;
        use tokio::sync::Mutex;
        use tokio::time::timeout;
        use url::Url;

        use crate::bridge::NotificationCache;
        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            code_action_provider: Some(lsp_types::CodeActionProvider::CodeActionOptions(
                lsp_types::CodeActionOptions {
                    resolve_provider: Some(true),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        let translator = Arc::new(translator.with_notification_cache(cache));

        let file_path = canonical_dir(&dir).join("main.rs");
        fs::write(&file_path, "fn main() {}").unwrap();
        let file_uri = Url::from_file_path(&file_path).unwrap().to_string();

        let handle = {
            let translator = Arc::clone(&translator);
            let path = file_path.to_str().unwrap().to_string();
            tokio::spawn(async move {
                translator
                    .handle_code_actions(
                        client_path(path),
                        bounded(Position::at(1, 1), Position::at(1, 10)),
                        None,
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/codeAction");

        let mut changes_map = serde_json::Map::new();
        changes_map.insert(
            file_uri,
            serde_json::json!([{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 0}
                },
                "newText": "use std::fmt;\n",
            }]),
        );

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "title": "Add missing import",
                "kind": "quickfix",
                "data": {"id": 42},
                "edit": { "changes": changes_map },
            }]),
        )
        .await;

        // No resolve request must ever arrive: the action already has an edit.
        let no_more_requests =
            timeout(Duration::from_millis(200), read_framed_message(&mut wire)).await;
        assert!(
            no_more_requests.is_err(),
            "codeAction/resolve must not be sent when edit is already present"
        );

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result.actions.len(), 1);
        let edit = result.actions[0]
            .edit
            .as_ref()
            .expect("original edit must be preserved");
        assert_eq!(edit.changes[0].edits[0].new_text, "use std::fmt;\n");
    }

    /// #432 companion: an action with `data: None` must never be resolved,
    /// even against a `resolveProvider: true` server -- per LSP 3.16, `data`
    /// presence is what signals an action is resolvable.
    #[tokio::test]
    async fn test_handle_code_actions_skips_resolve_when_data_absent() {
        use std::sync::Arc;
        use std::time::Duration;

        use tempfile::TempDir;
        use tokio::io::BufReader;
        use tokio::sync::Mutex;
        use tokio::time::timeout;

        use crate::bridge::NotificationCache;
        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            code_action_provider: Some(lsp_types::CodeActionProvider::CodeActionOptions(
                lsp_types::CodeActionOptions {
                    resolve_provider: Some(true),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        let translator = Arc::new(translator.with_notification_cache(cache));

        let file_path = dir.path().join("main.rs");
        fs::write(&file_path, "fn main() {}").unwrap();

        let handle = {
            let translator = Arc::clone(&translator);
            let path = file_path.to_str().unwrap().to_string();
            tokio::spawn(async move {
                translator
                    .handle_code_actions(
                        client_path(path),
                        bounded(Position::at(1, 1), Position::at(1, 10)),
                        None,
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/codeAction");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!([{
                "title": "Organize imports",
                "kind": "source.organizeImports",
            }]),
        )
        .await;

        // No resolve request must ever arrive: the action has no `data`.
        let no_more_requests =
            timeout(Duration::from_millis(200), read_framed_message(&mut wire)).await;
        assert!(
            no_more_requests.is_err(),
            "codeAction/resolve must not be sent when data is absent"
        );

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result.actions.len(), 1);
        assert!(result.actions[0].edit.is_none());
    }

    /// `handle_rename` needs the same whole-workspace reference index as
    /// `get_references`; it must surface `Error::WorkspaceIndexing` -- not
    /// attempt a rename against a partial index -- while the routed server
    /// is still `Loading`, without reaching the fake LSP server.
    #[tokio::test(start_paused = true)]
    async fn test_handle_rename_returns_workspace_indexing_error_when_loading() {
        use std::sync::Arc;

        use tempfile::TempDir;
        use tokio::sync::Mutex;

        use crate::bridge::NotificationCache;
        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::RenameProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, _server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": false})),
        );
        let translator = translator.with_notification_cache(cache);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn old_name() {}").unwrap();

        let err = translator
            .handle_rename(
                client_path(path.to_string_lossy().into_owned()),
                Position::at(1, 4),
                NewName::try_new("new_name").unwrap(),
            )
            .await
            .unwrap_err();

        assert_matches!(
            err,
            Error::WorkspaceIndexing { server_id: id, .. } if id == server_id
        );
    }

    /// Companion: when the cache reports `Ready`, `handle_rename` must
    /// dispatch normally.
    #[tokio::test]
    async fn test_handle_rename_dispatches_when_indexing_ready() {
        use std::sync::Arc;

        use tempfile::TempDir;
        use tokio::io::BufReader;
        use tokio::sync::Mutex;

        use crate::bridge::NotificationCache;
        use crate::config::ServerId;

        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from_static("rust");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::RenameProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        cache.lock().await.observe_indexing_signal(
            &server_id,
            "experimental/serverStatus",
            Some(&serde_json::json!({"quiescent": true})),
        );
        let translator = Arc::new(translator.with_notification_cache(cache));

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn old_name() {}").unwrap();

        let handle = {
            let translator = Arc::clone(&translator);
            let path = path.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_rename(
                        client_path(path),
                        Position::at(1, 4),
                        NewName::try_new("new_name").unwrap(),
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/rename");

        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::Value::Null,
        )
        .await;

        let result = handle.await.unwrap().unwrap();
        assert!(result.changes.is_empty());
    }

    fn file_uri(dir: &tempfile::TempDir, name: &str) -> String {
        let path = dir.path().join(name);
        fs::write(&path, "fn f() {}").unwrap();
        url::Url::from_file_path(&path).unwrap().to_string()
    }

    fn json_edit(text: &str) -> serde_json::Value {
        serde_json::json!({
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 1}
            },
            "newText": text
        })
    }

    fn json_snippet_edit() -> serde_json::Value {
        serde_json::json!({
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 0}
            },
            "snippet": { "value": "x", "kind": "snippet" }
        })
    }

    fn json_text_document_edit(uri: &str, edits: &[serde_json::Value]) -> serde_json::Value {
        serde_json::json!({
            "textDocument": { "uri": uri, "version": 1 },
            "edits": edits
        })
    }

    async fn convert_json(
        dir: &tempfile::TempDir,
        edit: serde_json::Value,
    ) -> (Vec<DocumentChanges>, DroppedEdits) {
        let edit: lsp_types::WorkspaceEdit = serde_json::from_value(edit).unwrap();
        let roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();
        convert_workspace_edit(
            edit,
            &test_ctx(),
            &roots,
            "rename edit",
            &mut ItemBudget::new(),
        )
        .await
    }

    /// #498 site 1: with a non-empty `changes`, `documentChanges` entries it
    /// does not mirror must be tallied instead of vanishing.
    #[tokio::test]
    async fn test_convert_workspace_edit_tallies_document_changes_shadowed_by_changes() {
        let dir = tempfile::TempDir::new().unwrap();
        let in_changes = file_uri(&dir, "a.rs");
        let only_in_document_changes = file_uri(&dir, "b.rs");

        let (changes, dropped) = convert_json(
            &dir,
            serde_json::json!({
                "changes": { in_changes.clone(): [json_edit("x")] },
                "documentChanges": [
                    json_text_document_edit(&only_in_document_changes, &[json_edit("y")]),
                    { "kind": "create", "uri": "file:///workspace/new.rs" },
                    { "kind": "delete", "uri": "file:///workspace/gone.rs" }
                ]
            }),
        )
        .await;

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].uri, in_changes);
        assert_eq!(dropped.shadowed_by_changes, 1);
        assert_eq!(dropped.unsupported_file_operation, 2);
        assert_eq!(dropped.out_of_workspace, 0);
    }

    /// #498 / #475 M1: a URI present in both fields is a mirror, not a drop.
    #[tokio::test]
    async fn test_convert_workspace_edit_mirrored_document_changes_are_not_tallied() {
        let dir = tempfile::TempDir::new().unwrap();
        let uri = file_uri(&dir, "a.rs");

        let (changes, dropped) = convert_json(
            &dir,
            serde_json::json!({
                "changes": { uri.clone(): [json_edit("x")] },
                "documentChanges": [json_text_document_edit(&uri, &[json_edit("x")])]
            }),
        )
        .await;

        assert_eq!(changes.len(), 1);
        assert!(dropped.is_empty(), "mirror tallied as a drop: {dropped:?}");
    }

    /// #498 site 2: an entry whose every edit was dropped is not listed, while
    /// one that arrived empty still round-trips.
    #[tokio::test]
    async fn test_convert_workspace_edit_omits_entry_whose_edits_were_all_dropped() {
        let dir = tempfile::TempDir::new().unwrap();
        let all_snippets = file_uri(&dir, "snippets.rs");
        let empty_on_arrival = file_uri(&dir, "empty.rs");

        let (changes, dropped) = convert_json(
            &dir,
            serde_json::json!({
                "documentChanges": [
                    json_text_document_edit(
                        &all_snippets,
                        &[json_snippet_edit(), json_snippet_edit()]
                    ),
                    json_text_document_edit(&empty_on_arrival, &[])
                ]
            }),
        )
        .await;

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].uri, empty_on_arrival);
        assert_eq!(dropped.unsupported_snippet_edit, 2);
    }

    fn json_edits(count: usize) -> Vec<serde_json::Value> {
        (0..count).map(|_| json_edit("x")).collect()
    }

    /// #487 M1: a file's edits are admitted whole or not at all, and files are
    /// considered in URI order, so the second of two files that jointly exceed
    /// the cap is the one dropped -- never a half-applied first file.
    #[tokio::test]
    async fn test_convert_workspace_edit_admits_files_whole_in_uri_order() {
        use super::super::navigation::MAX_NORMALIZED_LOCATIONS;

        let dir = tempfile::TempDir::new().unwrap();
        let first = file_uri(&dir, "a.rs");
        let second = file_uri(&dir, "b.rs");
        let per_file = MAX_NORMALIZED_LOCATIONS / 2 + 250;
        let edits = json_edits(per_file);

        let (changes, dropped) = convert_json(
            &dir,
            serde_json::json!({ "changes": { second: edits.clone(), first.clone(): edits } }),
        )
        .await;

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].uri, first);
        assert_eq!(changes[0].edits.len(), per_file);
        assert_eq!(dropped.exceeds_item_cap, per_file);
    }

    /// #487: exactly `MAX_NORMALIZED_LOCATIONS` edits is within the cap.
    #[tokio::test]
    async fn test_convert_workspace_edit_exactly_at_cap_drops_nothing() {
        use super::super::navigation::MAX_NORMALIZED_LOCATIONS;

        let dir = tempfile::TempDir::new().unwrap();
        let uri = file_uri(&dir, "a.rs");

        let (changes, dropped) = convert_json(
            &dir,
            serde_json::json!({ "changes": { uri: json_edits(MAX_NORMALIZED_LOCATIONS) } }),
        )
        .await;

        assert_eq!(changes[0].edits.len(), MAX_NORMALIZED_LOCATIONS);
        assert!(dropped.is_empty(), "{dropped:?}");
    }

    /// #487: one edit past the cap drops the whole file and counts every edit.
    #[tokio::test]
    async fn test_convert_workspace_edit_one_past_cap_drops_whole_file() {
        use super::super::navigation::MAX_NORMALIZED_LOCATIONS;

        let dir = tempfile::TempDir::new().unwrap();
        let uri = file_uri(&dir, "a.rs");

        let (changes, dropped) = convert_json(
            &dir,
            serde_json::json!({ "changes": { uri: json_edits(MAX_NORMALIZED_LOCATIONS + 1) } }),
        )
        .await;

        assert!(changes.is_empty());
        assert_eq!(dropped.exceeds_item_cap, MAX_NORMALIZED_LOCATIONS + 1);
    }

    /// #487 x #475 M1: a `documentChanges` entry mirroring a `changes` URI
    /// draws nothing from the budget, and a shadowed entry that is not a
    /// mirror is tallied as shadowed without counting against the cap.
    #[tokio::test]
    async fn test_convert_workspace_edit_mirror_and_shadowed_entries_spend_no_budget() {
        use super::super::navigation::MAX_NORMALIZED_LOCATIONS;

        let dir = tempfile::TempDir::new().unwrap();
        let mirrored = file_uri(&dir, "a.rs");
        let shadowed = file_uri(&dir, "b.rs");
        let full = json_edits(MAX_NORMALIZED_LOCATIONS);

        let (changes, dropped) = convert_json(
            &dir,
            serde_json::json!({
                "changes": { mirrored.clone(): full.clone() },
                "documentChanges": [
                    json_text_document_edit(&mirrored, &full),
                    json_text_document_edit(&shadowed, &[json_edit("y")])
                ]
            }),
        )
        .await;

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].edits.len(), MAX_NORMALIZED_LOCATIONS);
        assert_eq!(dropped.exceeds_item_cap, 0);
        assert_eq!(dropped.shadowed_by_changes, 1);
    }

    /// #487 S1: one budget is shared across every code action of a response,
    /// so N actions cannot each normalize a full cap's worth of edits.
    #[tokio::test]
    async fn test_convert_code_action_shares_budget_across_actions() {
        use super::super::navigation::MAX_NORMALIZED_LOCATIONS;

        let dir = tempfile::TempDir::new().unwrap();
        let uri = file_uri(&dir, "a.rs");
        let per_action = MAX_NORMALIZED_LOCATIONS / 2 + 250;
        let action = |title: &str| -> lsp_types::CodeAction {
            serde_json::from_value(serde_json::json!({
                "title": title,
                "edit": { "changes": { uri.clone(): json_edits(per_action) } }
            }))
            .unwrap()
        };
        let roots = WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap();
        let mut budget = ItemBudget::new();

        let first =
            convert_code_action(action("one"), &test_ctx(), &test_uri(), &roots, &mut budget).await;
        let second =
            convert_code_action(action("two"), &test_ctx(), &test_uri(), &roots, &mut budget).await;

        assert!(first.edit.unwrap().dropped.is_empty());
        let second_edit = second.edit.unwrap();
        assert!(second_edit.changes.is_empty());
        assert_eq!(second_edit.dropped.exceeds_item_cap, per_action);
        assert!(budget.truncated());
    }

    /// #487: a file reached after the budget is spent is left out entirely
    /// and counted, not listed with no edits.
    #[tokio::test]
    async fn test_convert_workspace_edit_skips_files_once_budget_is_spent() {
        use super::super::navigation::MAX_NORMALIZED_LOCATIONS;

        let dir = tempfile::TempDir::new().unwrap();
        let full = file_uri(&dir, "full.rs");
        let overflow = file_uri(&dir, "overflow.rs");
        let full_edits: Vec<_> = (0..MAX_NORMALIZED_LOCATIONS)
            .map(|_| json_edit("x"))
            .collect();

        let (changes, dropped) = convert_json(
            &dir,
            serde_json::json!({
                "documentChanges": [
                    json_text_document_edit(&full, &full_edits),
                    json_text_document_edit(&overflow, &[json_edit("y"), json_edit("z")])
                ]
            }),
        )
        .await;

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].uri, full);
        assert_eq!(dropped.exceeds_item_cap, 2);
    }
}
