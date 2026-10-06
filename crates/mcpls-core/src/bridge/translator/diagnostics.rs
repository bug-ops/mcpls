//! Diagnostics pull/push merging, cache-derived diagnostics, and server
//! log/message retrieval.

use std::sync::{Arc, LazyLock};

use lsp_types::{
    DocumentDiagnosticParams, PartialResultParams, TextDocumentIdentifier, Uri,
    WorkDoneProgressParams,
};
use tokio::sync::Mutex;
use tracing::{debug, info};

use super::Translator;
use super::availability::{DiagnosticsAnswer, DiagnosticsAvailability, DiagnosticsOrigin};
use super::dto::{
    Diagnostic, DiagnosticSeverity, DiagnosticsResult, DocumentDiagnosticsResult, ServerLogsResult,
    ServerMessagesResult,
};
use super::enclosing::{Contextualized, ResultContext};
use super::encoding_ctx::EncodingCtx;
use super::pull_support::{PullProbe, PullSupport};
use super::routing::PreparedDocument;
use crate::bridge::encoding::PositionEncoding;
use crate::bridge::notifications::{
    BoundedDiagnostics, ChangeOutcome, LogLevel, PullStamp, PullWrite, ReportedSeverity,
    SlotChange, VersionCheck, message_as_str, reported_code,
};
use crate::bridge::{
    ClientPath, DiagnosticInfo, DiagnosticSources, DiagnosticsKey, DocumentTracker,
    NotificationCache, WorkspacePath, WorkspaceRoots, path_to_uri,
};
use crate::config::{ServerId, ToolKind};
use crate::error::{Error, Result};
use crate::lsp::{ConnectionId, UnclassifiedError};
use crate::util::lock_std;

/// Hand-rolled union of `textDocument/diagnostic`'s two possible result
/// shapes.
///
/// `gen-lsp-types` types `DocumentDiagnosticRequest::Result` as the
/// non-nullable `DocumentDiagnosticReport` alone, splitting the streaming
/// `Partial` shape off into `RequestWithPartialResults::PartialResult`
/// (`DocumentDiagnosticReportProgress`) -- binding this call to that typed
/// result via `LspClient::request_typed` would turn a partial response into
/// a deserialization error where today it degrades to an empty diagnostics
/// list. This preserves the union gluon's `DocumentDiagnosticReportResult`
/// used to provide, via the untyped `LspClient::request`.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum DocumentDiagnosticReportResult {
    Report(lsp_types::DocumentDiagnosticReport),
    // The partial shape's content is never read -- matching this variant at
    // all (rather than failing to deserialize) is the only thing that
    // matters, so today's behavior of degrading to an empty diagnostics list
    // is preserved.
    Partial(
        #[allow(
            dead_code,
            reason = "the partial-result payload is only deserialized, never read"
        )]
        lsp_types::DocumentDiagnosticReportPartialResult,
    ),
}

/// `textDocument/diagnostic` with [`DocumentDiagnosticReportResult`] as its
/// result, so it can go through `LspClient::request_typed_classified` and the
/// caller decides how an error response is logged.
enum PullDiagnosticRequest {}

impl lsp_types::Request for PullDiagnosticRequest {
    type Params = DocumentDiagnosticParams;
    type Result = DocumentDiagnosticReportResult;
    const METHOD: lsp_types::LspRequestMethod<'static> =
        <lsp_types::DocumentDiagnosticRequest as lsp_types::Request>::METHOD;
    const MESSAGE_DIRECTION: lsp_types::MessageDirection =
        <lsp_types::DocumentDiagnosticRequest as lsp_types::Request>::MESSAGE_DIRECTION;
}

/// How a pull request ended.
enum PullAttempt {
    /// The server answered; holds the items of a full report.
    Answered(Option<BoundedDiagnostics>),
    /// No pull answered and none is expected to: the request was not sent, or a
    /// server advertising no provider refused it with `-32601`.
    PushOnly,
    /// The request failed.
    Failed(Error),
}

/// What a `textDocument/diagnostic` answer says about the file.
enum PullReport {
    /// A full report: the file's diagnostics as the server sees them now.
    Full(Vec<lsp_types::Diagnostic>),
    /// An `unchanged` or partial answer, which says nothing a cache could
    /// replace its slot with.
    NotStored,
}

impl From<DocumentDiagnosticReportResult> for PullReport {
    fn from(response: DocumentDiagnosticReportResult) -> Self {
        match response {
            DocumentDiagnosticReportResult::Report(
                lsp_types::DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(full),
            ) => Self::Full(full.full_document_diagnostic_report.items),
            DocumentDiagnosticReportResult::Report(
                lsp_types::DocumentDiagnosticReport::RelatedUnchangedDocumentDiagnosticReport(_),
            )
            | DocumentDiagnosticReportResult::Partial(_) => Self::NotStored,
        }
    }
}

/// Whether a pull report became the file's pulled slot.
enum PullStorage {
    /// Stored; the change it made to the slot.
    Stored(SlotChange),
    /// Only part of the result, not of the cache.
    NotStored,
}

/// A pull report after it met the cache.
struct PullSettlement {
    /// Snapshot of the file's sources with the report in it.
    sources: DiagnosticSources,
    storage: PullStorage,
    /// Files whose entries were evicted to make room for the report.
    evicted: Vec<DiagnosticsKey>,
    /// Whether the cache has an answer for the file, read in the same
    /// critical section as `sources`.
    availability: DiagnosticsAvailability,
}

/// Whether a server answered a request with JSON-RPC "method not found".
fn is_method_not_found(error: &Error) -> bool {
    matches!(
        error,
        Error::LspServerError { code, .. }
            if lsp_types::ErrorCodes::from(*code) == lsp_types::ErrorCodes::MethodNotFound
    )
}

/// Whether a server answered a request with an LSP code that says the result
/// is not available right now, not that it never will be.
fn is_transient_server_error(error: &Error) -> bool {
    matches!(
        error,
        Error::LspServerError { code, .. }
            if matches!(
                lsp_types::LspErrorCodes::from(*code),
                lsp_types::LspErrorCodes::RequestCancelled
                    | lsp_types::LspErrorCodes::ContentModified
                    | lsp_types::LspErrorCodes::ServerCancelled
            )
    )
}

/// How a failed probe pull moves the probe of its server.
enum ProbeOutcome {
    /// The server does not know the method: refused at once.
    Refuse,
    /// The request timed out.
    TimedOut,
    /// The server could not answer right now; nothing is learned.
    Transient,
}

impl ProbeOutcome {
    /// The finding in `error`, or `None` when it is no probe finding and
    /// surfaces as it would for any pull.
    fn of(error: &Error) -> Option<Self> {
        if is_method_not_found(error) {
            Some(Self::Refuse)
        } else if matches!(error, Error::Timeout(_)) {
            Some(Self::TimedOut)
        } else if is_transient_server_error(error) {
            Some(Self::Transient)
        } else {
            None
        }
    }
}

/// Shared, never-mutated empty `workspace_roots` for an `EncodingCtx` built
/// where it's documented as never read -- avoids a per-poll `Arc` allocation.
static EMPTY_WORKSPACE_ROOTS: LazyLock<WorkspaceRoots> = LazyLock::new(WorkspaceRoots::default);

/// Convert an LSP diagnostic into the MCP-facing `Diagnostic` shape.
///
/// Shared by both the pull-model (`handle_diagnostics`) and cache-derived
/// (`diagnostics_from_cache_entry`) diagnostic paths, so their output never
/// diverges in formatting.
pub(super) async fn diagnostic_to_mcp(
    diag: &lsp_types::Diagnostic,
    ctx: &EncodingCtx,
    uri: &lsp_types::Uri,
) -> Diagnostic {
    Diagnostic {
        range: ctx.normalize_range(uri, diag.range).await,
        severity: match ReportedSeverity::of(diag) {
            ReportedSeverity::Error => DiagnosticSeverity::Error,
            ReportedSeverity::Warning => DiagnosticSeverity::Warning,
            ReportedSeverity::Information => DiagnosticSeverity::Information,
            ReportedSeverity::Hint => DiagnosticSeverity::Hint,
        },
        message: message_as_str(&diag.message).to_string(),
        code: reported_code(diag).map(std::borrow::Cow::into_owned),
    }
}

impl Translator {
    /// Resolve the LSP-side cache key (URI) for a cached-diagnostics lookup.
    ///
    /// Split out from the cache read itself so callers (e.g. the
    /// `get_cached_diagnostics` MCP tool) can do the path `canonicalize()` and
    /// workspace-boundary check *before* taking the `NotificationCache` lock —
    /// that lock is also needed by `diagnostics_pump` to store incoming
    /// notifications, so nothing that isn't a plain map lookup should run
    /// while it's held.
    ///
    /// # Errors
    ///
    /// Returns an error if the path is invalid or outside workspace boundaries.
    pub async fn cached_diagnostics_uri(
        workspace_roots: &WorkspaceRoots,
        file_path: &ClientPath,
    ) -> Result<Uri> {
        Self::cached_diagnostics_path_and_uri(workspace_roots, file_path)
            .await
            .map(|(_, uri)| uri)
    }

    /// As [`Self::cached_diagnostics_uri`], but also returns the validated,
    /// canonicalized path -- for a caller (e.g. `get_cached_diagnostics`,
    /// `read_resource`) that needs it too, such as to resolve a
    /// diagnostics-route server via [`Self::diagnostics_route_for_path`]
    /// from the same canonical path the URI itself is derived from, rather
    /// than re-canonicalizing or (worse) using an unvalidated raw path.
    ///
    /// # Errors
    ///
    /// Returns an error if the path is invalid or outside workspace boundaries.
    pub(crate) async fn cached_diagnostics_path_and_uri(
        workspace_roots: &WorkspaceRoots,
        file_path: &ClientPath,
    ) -> Result<(WorkspacePath, Uri)> {
        let validated_path = workspace_roots.validate(file_path).await?;

        // Use path_to_uri (strips \\?\ on Windows) so the key matches what
        // rust-analyzer stores in publishDiagnostics notifications.
        let uri = path_to_uri(validated_path.as_path())?;
        Ok((validated_path, uri))
    }

    /// Handle diagnostics request.
    ///
    /// Pulls `textDocument/diagnostic`, stores a full report as the file's
    /// pulled slot in `notification_cache` next to whatever the server pushed
    /// (`NotificationCache::store_pulled_diagnostics`), and returns the merged
    /// view of both -- the same diagnostics `get_cached_diagnostics` and
    /// `resources/read` return for the file right after. The push side is
    /// merged in because rust-analyzer's pull endpoint omits
    /// flycheck/clippy-sourced diagnostics, and empirically some native ones,
    /// that are only ever delivered via push (#244). When the merged view
    /// changed, subscribers of the file's `lsp-diagnostics://` resource are
    /// notified before this returns (#574).
    ///
    /// A report is not stored when it is not a full report (`unchanged` and
    /// partial results), when a pull issued later or a server clear got there
    /// first, or when the document was resynced to another version while the
    /// request was in flight; the result then still contains the report, but
    /// the cache and the subscribers are left alone. If the pull request itself
    /// fails (e.g. a timeout), a non-empty cache entry is returned as a
    /// cache-only result instead of propagating the error, since the cache is
    /// not required to be fresher than the pull response to be useful here.
    ///
    /// A server that advertises no `diagnosticProvider` is probed with its
    /// first pull: `-32601` is not an error but the learned fact that it is
    /// push-only, so this and every later call answers from the push cache
    /// without a request (#666); the [`DiagnosticsAnswer`] says so in its
    /// `origin` and says in its `availability` whether the cache knows the file.
    ///
    /// The cache is locked once, after the pull request settles (success or
    /// failure), for the store and the snapshot -- never across the LSP
    /// round-trip -- matching the lock-ordering discipline documented on
    /// `cached_diagnostics_uri`. Merging, conversion and publishing run after
    /// the lock is dropped. Like `get_cached_diagnostics`, the cache is treated
    /// as eventually consistent: a pushed entry may reflect a slightly older
    /// document version than the fresh pull result if an edit landed inside
    /// the server's flycheck debounce window.
    ///
    /// Deliberately not gated on workspace-indexing readiness the way
    /// `IndexingGate::Required` whole-workspace queries are (#445, see
    /// `routing::IndexingGate`'s doc): this is a poll-based read, not a live
    /// whole-workspace LSP request, so blocking it on
    /// `Translator::wait_for_indexing_ready` would be the wrong fix shape.
    /// The `get_diagnostics` MCP tool instead surfaces the routed server's
    /// indexing state as an explicit `indexing_in_progress` flag alongside this
    /// method's result.
    ///
    /// # Errors
    ///
    /// Returns an error if the LSP pull request fails and the cache holds no
    /// diagnostics for the file either, or if the file cannot be opened.
    pub async fn handle_diagnostics(
        &self,
        file_path: ClientPath,
        context: ResultContext,
        notification_cache: &Mutex<NotificationCache>,
    ) -> Result<DocumentDiagnosticsResult> {
        let path = self.validate_path(&file_path).await?;
        self.handle_validated_diagnostics(&path, context, notification_cache)
            .await
            .map(|answer| answer.result)
    }

    /// As [`Self::handle_diagnostics`], for a path the caller already
    /// validated, so it is not canonicalized a second time, returning the
    /// result with the file's availability and the origin of the answer.
    ///
    /// # Errors
    ///
    /// As [`Self::handle_diagnostics`], except for path validation.
    pub(crate) async fn handle_validated_diagnostics(
        &self,
        path: &WorkspacePath,
        context: ResultContext,
        notification_cache: &Mutex<NotificationCache>,
    ) -> Result<DiagnosticsAnswer> {
        let doc = self
            .prepare_document_for_path(path, ToolKind::Diagnostics)
            .await?;
        let (server_id, uri) = (doc.server_id(), doc.uri());
        let ctx = self.encoding_ctx(server_id);
        let support = self.pull_support(server_id);

        let stamp = match self.document_tracker.synced_version(doc.path(), server_id) {
            Some(version) if support.sends_pull() => Some(
                notification_cache
                    .lock()
                    .await
                    .begin_pull(server_id, version),
            ),
            _ => None,
        };

        let (pulled, failure, origin) = match self.request_pull(&doc, support).await {
            PullAttempt::Answered(pulled) => (pulled, None, DiagnosticsOrigin::Pull),
            PullAttempt::PushOnly => (None, None, DiagnosticsOrigin::PushCache),
            PullAttempt::Failed(e) => (None, Some(e), DiagnosticsOrigin::Pull),
        };

        let PullSettlement {
            sources,
            storage,
            evicted,
            availability,
        } = {
            let mut cache = notification_cache.lock().await;
            self.settle_pull(&mut cache, &doc, stamp, pulled)
        };
        let diag_info = sources.merge();

        if let Some(wiring) = self.wiring.get() {
            if let PullStorage::Stored(slot) = storage
                && wiring.has_subscriptions().await
            {
                let outcome = match slot {
                    SlotChange::Identical => ChangeOutcome::Unchanged,
                    SlotChange::Replaced { before } => {
                        ChangeOutcome::of(before.merge().as_ref(), diag_info.as_ref())
                    }
                };
                match outcome {
                    ChangeOutcome::Changed => wiring.publish_changed(uri).await,
                    ChangeOutcome::Unchanged => {}
                }
            }
            if !evicted.is_empty() {
                wiring.publish_invalidated(&evicted).await;
            }
        }

        let merged = Self::diagnostics_from_cache_entry(
            diag_info.as_ref(),
            ctx.encoding,
            &self.document_tracker,
        )
        .await;
        if let Some(e) = failure
            && merged.diagnostics.is_empty()
        {
            return Err(e);
        }

        let Contextualized {
            items: diagnostics,
            enrichment,
            positions_degraded,
        } = self
            .contextualize_diagnostics(
                uri.as_ref(),
                merged.diagnostics,
                context,
                merged.positions_degraded,
            )
            .await;
        Ok(DiagnosticsAnswer {
            result: DocumentDiagnosticsResult {
                diagnostics,
                positions_degraded,
                enrichment,
            },
            availability,
            origin,
        })
    }

    /// Sends the pull request `support` calls for, and learns from how it
    /// ends: a server advertising no provider that answers is pulled from now
    /// on, one that refuses with `-32601` or times out twice in a row is not
    /// pulled again (until it is replaced) and its refusal is logged at DEBUG
    /// or INFO, not ERROR.
    async fn request_pull(&self, doc: &PreparedDocument, support: PullSupport) -> PullAttempt {
        if !support.sends_pull() {
            return PullAttempt::PushOnly;
        }
        let (server_id, client, uri) = (doc.server_id(), doc.client(), doc.uri());
        let params = DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            identifier: None,
            previous_result_id: None,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let sent_with = self.pull_probe_of(server_id);
        let response = client
            .request_typed_classified::<PullDiagnosticRequest>(params, client.request_timeout())
            .await;
        match response {
            Ok(response) => {
                if support == PullSupport::Probing {
                    self.record_pull_probe(server_id, client.connection_id(), |_| {
                        PullProbe::Answered
                    });
                }
                PullAttempt::Answered(match PullReport::from(response) {
                    PullReport::Full(mut items) => {
                        let redactions = client.redactions();
                        if !redactions.is_empty() {
                            for d in &mut items {
                                redactions.redact_diagnostic(d);
                            }
                        }
                        Some(BoundedDiagnostics::new(uri, items))
                    }
                    PullReport::NotStored => None,
                })
            }
            Err(unclassified) if support == PullSupport::Probing => {
                self.conclude_probe(server_id, client.connection_id(), sent_with, unclassified)
            }
            Err(unclassified) => PullAttempt::Failed(unclassified.surface()),
        }
    }

    /// Learns from a pull request that failed while the server was still being
    /// probed. A server that refuses, or times out on a pull sent after an
    /// earlier timeout, is answered from the push cache from now on, and so is
    /// this call when the failure says nothing about the server; any other
    /// failure surfaces. `sent_with` is the probe when the failed request was
    /// sent.
    fn conclude_probe(
        &self,
        server_id: &ServerId,
        conn: ConnectionId,
        sent_with: PullProbe,
        unclassified: UnclassifiedError,
    ) -> PullAttempt {
        let Some(outcome) = ProbeOutcome::of(unclassified.error()) else {
            return PullAttempt::Failed(unclassified.surface());
        };
        let error = unclassified.handled();
        match outcome {
            ProbeOutcome::Refuse => {
                debug!(
                    %server_id,
                    %error,
                    "server advertises no diagnostic provider and refused a pull; \
                     answering from the push cache"
                );
                self.record_pull_probe(server_id, conn, |_| PullProbe::Refused);
            }
            ProbeOutcome::TimedOut => {
                let probe =
                    self.record_pull_probe(server_id, conn, |now| now.after_timeout(sent_with));
                if probe == Some(PullProbe::Refused) {
                    info!(
                        %server_id,
                        %error,
                        "server advertises no diagnostic provider and timed out on two pulls \
                         in a row; answering from the push cache until it is replaced \
                         (restart_server)"
                    );
                } else {
                    debug!(%server_id, %error, "probe pull timed out; answering from the push cache");
                }
            }
            ProbeOutcome::Transient => debug!(
                %server_id,
                %error,
                "probe pull could not be answered right now; answering from the push cache"
            ),
        }
        PullAttempt::PushOnly
    }

    /// What is known about `id` answering `textDocument/diagnostic`.
    pub(super) fn pull_support(&self, id: &ServerId) -> PullSupport {
        let servers = lock_std(&self.servers);
        let advertised = servers
            .server(id)
            .is_some_and(|server| server.capabilities().diagnostic_provider.is_some());
        PullSupport::of(advertised, servers.pull_probe(id))
    }

    fn pull_probe_of(&self, id: &ServerId) -> PullProbe {
        lock_std(&self.servers).pull_probe(id)
    }

    fn record_pull_probe(
        &self,
        id: &ServerId,
        conn: ConnectionId,
        next: impl FnOnce(PullProbe) -> PullProbe,
    ) -> Option<PullProbe> {
        lock_std(&self.servers).update_pull_probe(id, conn, next)
    }

    /// Meets a settled pull with the cache, under the caller's lock: stores
    /// `pulled` when it may be stored, and snapshots the file's sources with
    /// the report in them either way.
    fn settle_pull(
        &self,
        cache: &mut NotificationCache,
        doc: &PreparedDocument,
        stamp: Option<PullStamp>,
        pulled: Option<BoundedDiagnostics>,
    ) -> PullSettlement {
        let (server_id, uri) = (doc.server_id(), doc.uri());
        let unstored = |cache: &NotificationCache, items, version| PullSettlement {
            sources: cache
                .diagnostic_sources(uri)
                .with_pulled(uri, version, items),
            storage: PullStorage::NotStored,
            evicted: Vec::new(),
            availability: DiagnosticsAvailability::Published,
        };
        let Some(items) = pulled else {
            return PullSettlement {
                sources: cache.diagnostic_sources(uri),
                storage: PullStorage::NotStored,
                evicted: Vec::new(),
                availability: cache.availability(uri, Some(server_id)),
            };
        };
        let Some(stamp) = stamp else {
            return unstored(cache, items, None);
        };
        let check = if self.document_tracker.synced_version(doc.path(), server_id)
            == Some(stamp.version())
        {
            VersionCheck::Current
        } else {
            VersionCheck::Moved
        };
        match cache.store_pulled_diagnostics(server_id, uri, stamp, check, items) {
            PullWrite::Stored { slot, evicted } => PullSettlement {
                sources: cache.diagnostic_sources(uri),
                storage: PullStorage::Stored(slot),
                evicted,
                availability: DiagnosticsAvailability::Published,
            },
            PullWrite::Discarded {
                reason,
                evicted,
                items,
            } => {
                debug!(
                    "discarding the pulled diagnostics of {}: {reason:?}",
                    uri.as_ref()
                );
                PullSettlement {
                    evicted,
                    ..unstored(cache, items, Some(stamp.version()))
                }
            }
        }
    }

    /// Convert a cached diagnostics entry into the MCP-facing result shape.
    ///
    /// Takes an already-cloned `Option<&DiagnosticInfo>` (out of the
    /// `NotificationCache` lock) rather than the cache itself, so this
    /// mapping — which is not a bounded operation for a large diagnostics set
    /// — never runs while the cache is locked.
    ///
    /// `encoding` is the negotiated encoding of the server that published
    /// these diagnostics; pass `PositionEncoding::Utf16` when no live server
    /// context is available (e.g. a cache-only read with no resolved owner).
    #[must_use]
    pub async fn diagnostics_from_cache_entry(
        diag_info: Option<&DiagnosticInfo>,
        encoding: PositionEncoding,
        tracker: &Arc<DocumentTracker>,
    ) -> DiagnosticsResult {
        match diag_info {
            Some(diag_info) => {
                // Workspace roots are never read here -- see
                // `EMPTY_WORKSPACE_ROOTS`'s doc.
                let ctx =
                    EncodingCtx::new(encoding, tracker.clone(), EMPTY_WORKSPACE_ROOTS.clone());
                let mut result = Vec::with_capacity(diag_info.diagnostics.len());
                for d in &diag_info.diagnostics {
                    result.push(diagnostic_to_mcp(d, &ctx, &diag_info.uri).await);
                }
                DiagnosticsResult {
                    diagnostics: result,
                    positions_degraded: ctx.positions_degraded(),
                }
            }
            None => DiagnosticsResult {
                diagnostics: Vec::new(),
                positions_degraded: None,
            },
        }
    }

    /// Handle server logs request.
    ///
    /// Logs at least as severe as `min_level` are returned, or all of them
    /// when it is `None`.
    #[must_use]
    pub fn handle_server_logs(
        cache: &NotificationCache,
        limit: usize,
        min_level: Option<LogLevel>,
    ) -> ServerLogsResult {
        let logs = cache
            .logs()
            .iter()
            .filter(|log| min_level.is_none_or(|min| log.level.meets(min)))
            .take(limit)
            .cloned()
            .collect();

        ServerLogsResult { logs }
    }

    /// Handle server messages request.
    ///
    /// # Errors
    ///
    /// This method does not return errors.
    pub fn handle_server_messages(
        cache: &NotificationCache,
        limit: usize,
    ) -> Result<ServerMessagesResult> {
        let all_messages = cache.messages();
        let messages: Vec<_> = all_messages.iter().take(limit).cloned().collect();
        Ok(ServerMessagesResult { messages })
    }
}

#[cfg(test)]
mod pull_tests;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use std::{assert_matches, fs};

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::time::timeout;
    use url::Url;

    use super::*;
    use crate::bridge::translator::dto::PositionDegradation;
    use crate::bridge::translator::testing::*;
    use crate::config::{FileExtension, LanguageId, ServerId, ToolRouter};
    use crate::error::Error;
    use crate::test_lsp::client_path;

    /// Pins the upstream `lsp_types::DocumentDiagnosticParams` serde
    /// attributes (`skip_serializing_if` on both optionals) across future
    /// `gen-lsp-types` version bumps -- this behavior was previously
    /// guaranteed by a hand-rolled `DiagnosticRequestParams` (see #166),
    /// dropped in favor of direct construction once verified byte-identical.
    #[test]
    fn test_document_diagnostic_params_omit_optional_null_fields() {
        let uri = lsp_types::Uri::from("file:///test.ts");
        let params = DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier { uri },
            identifier: None,
            previous_result_id: None,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let value = serde_json::to_value(params).unwrap();

        assert_eq!(value["textDocument"]["uri"], "file:///test.ts");
        assert!(value.get("identifier").is_none());
        assert!(value.get("previousResultId").is_none());
    }

    #[tokio::test]
    async fn test_handle_cached_diagnostics_empty() {
        let cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let cache_key = Translator::cached_diagnostics_uri(
            &WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
            &client_path(&test_file),
        )
        .await
        .unwrap();
        let diag_info = cache.diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(
            diag_info.as_ref(),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;
        assert_eq!(diags.diagnostics.len(), 0);
    }

    #[test]
    fn test_handle_server_logs_with_filter() {
        let mut cache = NotificationCache::new();

        // Add some logs
        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        // Test with error filter
        let logs = Translator::handle_server_logs(&cache, 10, Some(LogLevel::Error));
        assert_eq!(logs.logs.len(), 1);
        assert_eq!(logs.logs[0].message, "error msg");

        // Test with warning filter (includes error and warning)
        let logs = Translator::handle_server_logs(&cache, 10, Some(LogLevel::Warning));
        assert_eq!(logs.logs.len(), 2);

        // Test with info filter (excludes debug)
        let logs = Translator::handle_server_logs(&cache, 10, Some(LogLevel::Info));
        assert_eq!(logs.logs.len(), 3);

        // Test with debug filter (includes all)
        let logs = Translator::handle_server_logs(&cache, 10, Some(LogLevel::Debug));
        assert_eq!(logs.logs.len(), 4);
    }

    #[test]
    fn test_handle_server_messages_limit() {
        use crate::bridge::notifications::MessageType;

        let mut cache = NotificationCache::new();

        // Add some messages
        for i in 0..10 {
            cache.store_message(MessageType::Info, format!("message {i}"));
        }

        // Test limit
        let result = Translator::handle_server_messages(&cache, 5);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 5);
        assert_eq!(messages.messages[0].message, "message 0");
        assert_eq!(messages.messages[4].message, "message 4");

        // Test limit larger than available
        let result = Translator::handle_server_messages(&cache, 100);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 10);
    }

    #[tokio::test]
    async fn test_handle_cached_diagnostics_with_data() {
        let mut cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri =
            lsp_types::Uri::from(Url::from_file_path(&canonical_path).unwrap().as_str());
        let diagnostic = lsp_types::Diagnostic {
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
            message: "test error".to_string().into(),
            code: Some(lsp_types::Code::String("E001".to_string())),
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(
            &ServerId::from_static("rust"),
            &uri,
            Some(1),
            vec![diagnostic],
        );

        let cache_key = Translator::cached_diagnostics_uri(
            &WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
            &client_path(&test_file),
        )
        .await
        .unwrap();
        let diag_info = cache.diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(
            diag_info.as_ref(),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;
        assert_eq!(diags.diagnostics.len(), 1);
        assert_eq!(diags.diagnostics[0].message, "test error");
        assert_eq!(diags.diagnostics[0].code, Some("E001".to_string()));
        assert_matches!(diags.diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_eq!(diags.diagnostics[0].range.start.line, 1);
        assert_eq!(diags.diagnostics[0].range.start.character, 1);
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one scenario covering every severity"
    )]
    async fn test_handle_cached_diagnostics_multiple_severities() {
        let mut cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri =
            lsp_types::Uri::from(Url::from_file_path(&canonical_path).unwrap().as_str());
        let diagnostics = vec![
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
                message: "error".to_string().into(),
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
                        line: 1,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 1,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::Warning),
                message: "warning".to_string().into(),
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
                        line: 2,
                        character: 0,
                    },
                    end: lsp_types::Position {
                        line: 2,
                        character: 5,
                    },
                },
                severity: Some(lsp_types::DiagnosticSeverity::Information),
                message: "info".to_string().into(),
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
                message: "hint".to_string().into(),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            },
        ];

        cache.store_diagnostics(&ServerId::from_static("rust"), &uri, Some(1), diagnostics);

        let cache_key = Translator::cached_diagnostics_uri(
            &WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
            &client_path(&test_file),
        )
        .await
        .unwrap();
        let diag_info = cache.diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(
            diag_info.as_ref(),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;
        assert_eq!(diags.diagnostics.len(), 4);
        assert_matches!(diags.diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_matches!(diags.diagnostics[1].severity, DiagnosticSeverity::Warning);
        assert_matches!(
            diags.diagnostics[2].severity,
            DiagnosticSeverity::Information
        );
        assert_matches!(diags.diagnostics[3].severity, DiagnosticSeverity::Hint);
    }

    #[tokio::test]
    async fn test_handle_cached_diagnostics_with_numeric_code() {
        let mut cache = NotificationCache::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri =
            lsp_types::Uri::from(Url::from_file_path(&canonical_path).unwrap().as_str());
        let diagnostic = lsp_types::Diagnostic {
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
            message: "test error".to_string().into(),
            code: Some(lsp_types::Code::Int(42)),
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(
            &ServerId::from_static("rust"),
            &uri,
            Some(1),
            vec![diagnostic],
        );

        let cache_key = Translator::cached_diagnostics_uri(
            &WorkspaceRoots::from_paths(&[temp_dir.path().to_path_buf()]).unwrap(),
            &client_path(&test_file),
        )
        .await
        .unwrap();
        let diag_info = cache.diagnostics(&cache_key).cloned();
        let diags = Translator::diagnostics_from_cache_entry(
            diag_info.as_ref(),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;
        assert_eq!(diags.diagnostics.len(), 1);
        assert_eq!(diags.diagnostics[0].code, Some("42".to_string()));
    }

    #[tokio::test]
    async fn test_handle_cached_diagnostics_invalid_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("nonexistent/path/file.rs");
        let result = Translator::cached_diagnostics_uri(
            &WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap(),
            &client_path(&missing),
        )
        .await;
        assert_matches!(result, Err(Error::FileIo { .. }));
    }

    /// #497: a non-UTF-16 server whose document cannot be resolved reports
    /// its positions as degraded, so the merged result carries the flag.
    #[tokio::test]
    async fn test_diagnostics_from_cache_entry_flags_degraded_positions() {
        let info = diag_info(vec![lsp_diag(
            0,
            10,
            lsp_types::DiagnosticSeverity::Warning,
            "unused import: `std::fmt`",
            None,
        )]);

        let result = Translator::diagnostics_from_cache_entry(
            Some(&info),
            PositionEncoding::Utf8,
            &test_tracker(),
        )
        .await;

        assert_eq!(
            result.positions_degraded,
            Some(PositionDegradation::Response)
        );
        let utf16 = Translator::diagnostics_from_cache_entry(
            Some(&info),
            PositionEncoding::Utf16,
            &test_tracker(),
        )
        .await;
        assert_eq!(utf16.positions_degraded, None);
    }

    #[test]
    fn test_handle_server_logs_no_filter() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        let logs = Translator::handle_server_logs(&cache, 10, None);
        assert_eq!(logs.logs.len(), 4);
    }

    #[test]
    fn test_handle_server_logs_error_filter_strict() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());

        let logs = Translator::handle_server_logs(&cache, 10, Some(LogLevel::Error));
        assert_eq!(logs.logs.len(), 1);
        assert_eq!(logs.logs[0].message, "error msg");
    }

    #[test]
    fn test_handle_server_logs_warning_filter_includes_errors() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());

        let logs = Translator::handle_server_logs(&cache, 10, Some(LogLevel::Warning));
        assert_eq!(logs.logs.len(), 2);
    }

    #[test]
    fn test_handle_server_logs_info_filter_excludes_debug() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        let logs = Translator::handle_server_logs(&cache, 10, Some(LogLevel::Info));
        assert_eq!(logs.logs.len(), 2);
    }

    #[test]
    fn test_handle_server_logs_debug_filter_includes_all() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error msg".to_string());
        cache.store_log(LogLevel::Warning, "warning msg".to_string());
        cache.store_log(LogLevel::Info, "info msg".to_string());
        cache.store_log(LogLevel::Debug, "debug msg".to_string());

        let logs = Translator::handle_server_logs(&cache, 10, Some(LogLevel::Debug));
        assert_eq!(logs.logs.len(), 4);
    }

    #[test]
    fn test_handle_server_logs_limit_applies_after_filter() {
        let mut cache = NotificationCache::new();

        for i in 0..10 {
            cache.store_log(LogLevel::Error, format!("error {i}"));
        }

        let logs = Translator::handle_server_logs(&cache, 5, Some(LogLevel::Error));
        assert_eq!(logs.logs.len(), 5);
        assert_eq!(logs.logs[0].message, "error 0");
        assert_eq!(logs.logs[4].message, "error 4");
    }

    #[test]
    fn test_handle_server_messages_empty() {
        let cache = NotificationCache::new();

        let result = Translator::handle_server_messages(&cache, 10);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 0);
    }

    #[test]
    fn test_handle_server_messages_with_different_types() {
        use crate::bridge::notifications::MessageType;

        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Error, "error".to_string());
        cache.store_message(MessageType::Warning, "warning".to_string());
        cache.store_message(MessageType::Info, "info".to_string());
        cache.store_message(MessageType::Log, "log".to_string());

        let result = Translator::handle_server_messages(&cache, 10);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 4);
        assert_eq!(messages.messages[0].message, "error");
        assert_eq!(messages.messages[1].message, "warning");
        assert_eq!(messages.messages[2].message, "info");
        assert_eq!(messages.messages[3].message, "log");
    }

    #[test]
    fn test_handle_server_messages_zero_limit() {
        use crate::bridge::notifications::MessageType;

        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Info, "test".to_string());

        let result = Translator::handle_server_messages(&cache, 0);
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert_eq!(messages.messages.len(), 0);
    }

    #[tokio::test]
    async fn test_handle_cached_diagnostics_path_outside_workspace() {
        let temp_dir1 = TempDir::new().unwrap();
        let temp_dir2 = TempDir::new().unwrap();

        let workspace_roots = vec![temp_dir1.path().to_path_buf()];

        let test_file = temp_dir2.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = Translator::cached_diagnostics_uri(
            &WorkspaceRoots::from_paths(&workspace_roots).unwrap(),
            &client_path(&test_file),
        )
        .await;
        assert_matches!(result, Err(Error::PathOutsideWorkspace(_)));
    }

    /// #583: pulled diagnostics are redacted like pushed ones, before they are
    /// merged, so a pulled item and its already-redacted cached twin dedupe.
    #[tokio::test]
    async fn test_handle_diagnostics_redacts_pull_items_before_merging_with_the_cache() {
        let dir = TempDir::new().unwrap();
        let mut translator = Translator::new()
            .with_extensions(crate::test_lsp::test_extensions())
            .with_router(ToolRouter::catch_all([(
                ServerId::from_static("rust"),
                LanguageId::from_static("rust"),
            )]));
        translator
            .set_workspace_roots(WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap());
        let redactions = crate::redaction::Redactions::new([(
            "API_TOKEN".to_owned(),
            "SuperSecretValue123".to_owned(),
        )]);
        let (client, mut server, _lanes) =
            crate::test_lsp::fake_lsp_client_with_redactions(redactions);
        translator.register_client(ServerId::from_static("rust"), client);

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();
        let uri = path_to_uri(&path.canonicalize().unwrap()).unwrap();
        let notification_cache = Mutex::new(NotificationCache::new());
        notification_cache.lock().await.store_diagnostics(
            &ServerId::from_static("rust"),
            &uri,
            Some(1),
            vec![lsp_diag(
                0,
                4,
                lsp_types::DiagnosticSeverity::Warning,
                "leaked [redacted:API_TOKEN]",
                None,
            )],
        );

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_diagnostics(
                        client_path(path_str),
                        ResultContext::None,
                        &notification_cache,
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = read_framed_message(&mut wire).await;
        assert_eq!(request["method"], "textDocument/diagnostic");
        let range = serde_json::json!({
            "start": {"line": 0, "character": 0},
            "end": {"line": 0, "character": 4}
        });
        write_response(
            &mut server.read_half_stdin,
            &request["id"],
            serde_json::json!({
                "kind": "full",
                "items": [
                    {"range": range, "severity": 2, "message": "leaked SuperSecretValue123"},
                    {
                        "range": range,
                        "severity": 1,
                        "message": {"kind": "plaintext", "value": "other SuperSecretValue123"},
                        "data": {"hint": "SuperSecretValue123"}
                    }
                ]
            }),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        let shown = serde_json::to_string(&result.diagnostics).unwrap();
        assert!(!shown.contains("SuperSecretValue123"), "{shown}");
        assert_eq!(result.diagnostics.len(), 2, "{shown}");
        assert!(shown.contains("leaked [redacted:API_TOKEN]"), "{shown}");
    }

    /// S1 regression (#244): a push-only server (or one that times out)
    /// answering `textDocument/diagnostic` with an LSP error must not
    /// discard diagnostics `handle_diagnostics` already knows about from the
    /// cache -- it should return the cache-only result instead of `Err`.
    #[tokio::test]
    async fn test_handle_diagnostics_pull_error_falls_back_to_nonempty_cache() {
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert(
            FileExtension::from_static("rs"),
            LanguageId::from_static("rust"),
        );

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([(
                    ServerId::from_static("rust"),
                    LanguageId::from_static("rust"),
                )]));
        translator
            .set_workspace_roots(WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap());

        let (client, mut server) = fake_lsp_client();
        translator.register_client(ServerId::from_static("rust"), client);

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        // Prime the cache under the exact URI handle_diagnostics will look
        // up (path_to_uri over the canonicalized path, same as
        // document_tracker uses to open the document).
        let canonical = path.canonicalize().unwrap();
        let uri = path_to_uri(&canonical).unwrap();
        let notification_cache = Mutex::new(NotificationCache::new());
        {
            let mut cache = notification_cache.lock().await;
            cache.store_diagnostics(
                &ServerId::from_static("rust"),
                &uri,
                Some(1),
                vec![lsp_diag(
                    0,
                    4,
                    lsp_types::DiagnosticSeverity::Warning,
                    "unused import: `std::fmt`",
                    None,
                )],
            );
        }

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_diagnostics(
                        client_path(path_str),
                        ResultContext::None,
                        &notification_cache,
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        write_error_response(
            &mut server.read_half_stdin,
            &diag_request["id"],
            -32601,
            "method not found",
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap();

        let diagnostics = result.expect("cache-only fallback should succeed despite pull error");
        assert_eq!(diagnostics.diagnostics.len(), 1);
        assert_eq!(
            diagnostics.diagnostics[0].message,
            "unused import: `std::fmt`"
        );
    }

    /// S4 lock-in for critic finding N1: `textDocument/diagnostic` keeps an
    /// untyped `LspClient::request` bound to the hand-rolled
    /// `DocumentDiagnosticReportResult` union specifically so that a
    /// streaming `Partial` response (the shape
    /// `DocumentDiagnosticRequest`'s typed `Result` cannot represent)
    /// degrades to an empty diagnostics list instead of a deserialization
    /// error. This test drives that exact response shape through the real
    /// `handle_diagnostics` handler.
    #[tokio::test]
    async fn test_handle_diagnostics_partial_response_degrades_to_empty_result() {
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert(
            FileExtension::from_static("rs"),
            LanguageId::from_static("rust"),
        );

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([(
                    ServerId::from_static("rust"),
                    LanguageId::from_static("rust"),
                )]));
        translator
            .set_workspace_roots(WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap());

        let (client, mut server) = fake_lsp_client();
        translator.register_client(ServerId::from_static("rust"), client);

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let notification_cache = Mutex::new(NotificationCache::new());

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_diagnostics(
                        client_path(path_str),
                        ResultContext::None,
                        &notification_cache,
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");

        // A `DocumentDiagnosticReportPartialResult`: no `kind` field (so it
        // cannot be a `Report`), only `relatedDocuments`.
        write_response(
            &mut server.read_half_stdin,
            &diag_request["id"],
            serde_json::json!({ "relatedDocuments": {} }),
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap();

        let diagnostics =
            result.expect("a Partial response must degrade to Ok(empty), not an error");
        assert!(
            diagnostics.diagnostics.is_empty(),
            "expected no diagnostics from a Partial response, got {diagnostics:?}"
        );
    }

    /// S1 counterpart: when the cache is also empty, the pull error must
    /// still propagate -- there is nothing to fall back to.
    #[tokio::test]
    async fn test_handle_diagnostics_pull_error_and_empty_cache_propagates_error() {
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert(
            FileExtension::from_static("rs"),
            LanguageId::from_static("rust"),
        );

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([(
                    ServerId::from_static("rust"),
                    LanguageId::from_static("rust"),
                )]));
        translator
            .set_workspace_roots(WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap());

        let (client, mut server) = fake_lsp_client();
        translator.register_client(ServerId::from_static("rust"), client);

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let notification_cache = Mutex::new(NotificationCache::new());

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_diagnostics(
                        client_path(path_str),
                        ResultContext::None,
                        &notification_cache,
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        write_error_response(
            &mut server.read_half_stdin,
            &diag_request["id"],
            -32603,
            "internal error",
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap();

        assert!(
            result.is_err(),
            "pull error with no cache data must propagate, got {result:?}"
        );
    }
}
