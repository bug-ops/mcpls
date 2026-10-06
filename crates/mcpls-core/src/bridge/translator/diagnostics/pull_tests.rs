//! `get_diagnostics` as the writer of the pulled slot: storing, publishing and
//! the races a pull in flight can lose.

use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use std::{assert_matches, fs};

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{BufReader, DuplexStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::*;
use crate::bridge::resources::PublishedDiagnosticsUri;
use crate::bridge::translator::testing::*;
use crate::bridge::{DiagnosticsRole, NotificationReceivers, NotificationWiring};
use crate::config::{LanguageId, ServerId, ToolRouter};
use crate::test_lsp::{client_path, fake_lsp_client_with_redactions};

#[derive(Debug, Default)]
struct RecordingWiring {
    subscribed: std::sync::atomic::AtomicBool,
    changed: StdMutex<Vec<Uri>>,
    invalidated: StdMutex<Vec<DiagnosticsKey>>,
}

impl RecordingWiring {
    fn changed(&self) -> Vec<Uri> {
        self.changed.lock().unwrap().clone()
    }
}

impl NotificationWiring for RecordingWiring {
    fn spawn_pump(
        &self,
        _id: ServerId,
        _receivers: NotificationReceivers,
        _role: DiagnosticsRole,
    ) -> tokio::task::AbortHandle {
        tokio::spawn(async {}).abort_handle()
    }

    fn publish_invalidated<'a>(
        &'a self,
        cleared: &'a [DiagnosticsKey],
    ) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            self.invalidated.lock().unwrap().extend_from_slice(cleared);
        })
    }

    fn has_subscriptions(&self) -> futures::future::BoxFuture<'_, bool> {
        Box::pin(async move { self.subscribed.load(std::sync::atomic::Ordering::Relaxed) })
    }

    fn publish_changed<'a>(&'a self, file: &'a Uri) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            self.changed.lock().unwrap().push(file.clone());
        })
    }
}

type Pull = JoinHandle<Result<DocumentDiagnosticsResult>>;

struct Fixture {
    _dir: TempDir,
    translator: Arc<Translator>,
    cache: Arc<Mutex<NotificationCache>>,
    wiring: Arc<RecordingWiring>,
    path: PathBuf,
    uri: Uri,
}

impl Fixture {
    fn new(redactions: crate::redaction::Redactions) -> (Self, FakeServer) {
        let dir = TempDir::new().unwrap();
        let mut translator = Translator::new()
            .with_extensions(crate::test_lsp::test_extensions())
            .with_router(ToolRouter::catch_all([(
                ServerId::from_static("rust"),
                LanguageId::from_static("rust"),
            )]));
        translator
            .set_workspace_roots(WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap());
        let (client, server, _lanes) = fake_lsp_client_with_redactions(redactions);
        translator.register_client(ServerId::from_static("rust"), client);
        let wiring = Arc::new(RecordingWiring {
            subscribed: std::sync::atomic::AtomicBool::new(true),
            ..RecordingWiring::default()
        });
        translator.install_wiring(wiring.clone());

        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path = dunce::canonicalize(path).unwrap();
        let uri = path_to_uri(&path).unwrap();
        let fixture = Self {
            _dir: dir,
            translator: Arc::new(translator),
            cache: Arc::new(Mutex::new(NotificationCache::new())),
            wiring,
            path,
            uri,
        };
        (fixture, server)
    }

    fn spawn_pull(&self) -> Pull {
        let (translator, cache) = (Arc::clone(&self.translator), Arc::clone(&self.cache));
        let path = self.path.clone();
        tokio::spawn(async move {
            translator
                .handle_diagnostics(client_path(path), ResultContext::None, &cache)
                .await
        })
    }

    async fn finish(pull: Pull) -> Result<DocumentDiagnosticsResult> {
        timeout(Duration::from_secs(5), pull)
            .await
            .expect("pull did not finish")
            .unwrap()
    }

    async fn sources(&self) -> Option<DiagnosticInfo> {
        self.cache
            .lock()
            .await
            .diagnostic_sources(&self.uri)
            .merge()
    }

    fn rust() -> ServerId {
        ServerId::from_static("rust")
    }
}

/// Reads client messages until the pull request arrives and returns it.
async fn next_pull_request(wire: &mut BufReader<&mut DuplexStream>) -> Value {
    loop {
        let message = read_framed_message(wire).await;
        if message["method"] == "textDocument/diagnostic" {
            return message;
        }
    }
}

fn full_report(items: Value) -> Value {
    let mut report = json!({"kind": "full"});
    report["items"] = items;
    report
}

fn error_item(line: u32, message: &str) -> Value {
    json!({
        "range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 4}},
        "severity": 1,
        "message": message,
        "code": "E0308"
    })
}

async fn answer(wire: &mut BufReader<&mut DuplexStream>, stdin: &mut DuplexStream, report: Value) {
    let request = next_pull_request(wire).await;
    write_response(stdin, &request["id"], report).await;
}

fn shown(result: &DocumentDiagnosticsResult) -> Vec<&str> {
    result
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect()
}

#[tokio::test]
async fn test_pull_is_visible_to_the_cache_and_published_once() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let mut wire = BufReader::new(&mut server.write_stdout);

    let pull = fx.spawn_pull();
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([error_item(0, "E0308 expected i32")])),
    )
    .await;
    let result = Fixture::finish(pull).await.unwrap();

    assert_eq!(shown(&result), ["E0308 expected i32"]);
    let cached = fx.sources().await.unwrap();
    assert_eq!(cached.diagnostics.len(), 1);
    assert_eq!(fx.wiring.changed(), std::slice::from_ref(&fx.uri));
}

#[tokio::test]
async fn test_identical_pulls_publish_nothing_after_the_first() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let mut wire = BufReader::new(&mut server.write_stdout);

    for _ in 0..5 {
        let pull = fx.spawn_pull();
        answer(
            &mut wire,
            &mut server.read_half_stdin,
            full_report(json!([error_item(0, "same")])),
        )
        .await;
        drop(Fixture::finish(pull).await.unwrap());
    }

    assert_eq!(fx.wiring.changed().len(), 1);
}

#[tokio::test]
async fn test_fixing_the_error_publishes_again_and_clears_every_read_path() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let mut wire = BufReader::new(&mut server.write_stdout);
    let pull = fx.spawn_pull();
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([error_item(0, "E0308")])),
    )
    .await;
    drop(Fixture::finish(pull).await.unwrap());

    let pull = fx.spawn_pull();
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([])),
    )
    .await;
    let result = Fixture::finish(pull).await.unwrap();

    assert!(result.diagnostics.is_empty());
    assert!(fx.sources().await.unwrap().diagnostics.is_empty());
    assert_eq!(fx.wiring.changed().len(), 2);
}

#[tokio::test]
async fn test_unchanged_and_partial_reports_leave_the_cache_and_subscribers_alone() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let mut wire = BufReader::new(&mut server.write_stdout);
    let pull = fx.spawn_pull();
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([error_item(0, "E0308")])),
    )
    .await;
    drop(Fixture::finish(pull).await.unwrap());

    for report in [
        json!({"kind": "unchanged", "resultId": "1"}),
        json!({"relatedDocuments": {}}),
    ] {
        let pull = fx.spawn_pull();
        answer(&mut wire, &mut server.read_half_stdin, report).await;
        let result = Fixture::finish(pull).await.unwrap();
        assert_eq!(shown(&result), ["E0308"]);
    }

    assert_eq!(fx.wiring.changed().len(), 1);
    assert_eq!(fx.sources().await.unwrap().diagnostics.len(), 1);
}

#[tokio::test]
async fn test_failed_pull_keeps_the_last_pulled_slot_and_publishes_nothing() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let mut wire = BufReader::new(&mut server.write_stdout);
    let pull = fx.spawn_pull();
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([error_item(0, "E0308")])),
    )
    .await;
    drop(Fixture::finish(pull).await.unwrap());

    let pull = fx.spawn_pull();
    let request = next_pull_request(&mut wire).await;
    write_error_response(&mut server.read_half_stdin, &request["id"], -32601, "no").await;
    let result = Fixture::finish(pull).await.unwrap();

    assert_eq!(shown(&result), ["E0308"]);
    assert_eq!(fx.wiring.changed().len(), 1);
}

#[tokio::test]
async fn test_failed_pull_of_an_unseen_file_is_still_an_error() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let mut wire = BufReader::new(&mut server.write_stdout);

    let pull = fx.spawn_pull();
    let request = next_pull_request(&mut wire).await;
    write_error_response(&mut server.read_half_stdin, &request["id"], -32603, "no").await;

    assert!(Fixture::finish(pull).await.is_err());
    assert!(fx.sources().await.is_none());
    assert!(fx.wiring.changed().is_empty());
}

impl Fixture {
    fn spawn_answer(&self) -> JoinHandle<Result<DiagnosticsAnswer>> {
        let (translator, cache) = (Arc::clone(&self.translator), Arc::clone(&self.cache));
        let path = self.path.clone();
        tokio::spawn(async move {
            let path = translator.validate_path(&client_path(path)).await?;
            translator
                .handle_validated_diagnostics(&path, ResultContext::None, &cache)
                .await
        })
    }

    async fn finish_answer(answer: JoinHandle<Result<DiagnosticsAnswer>>) -> DiagnosticsAnswer {
        timeout(Duration::from_secs(5), answer)
            .await
            .expect("the answer did not finish")
            .unwrap()
            .unwrap()
    }

    async fn publish(&self, items: Vec<lsp_types::Diagnostic>) {
        let published = PublishedDiagnosticsUri::for_test(self.uri.clone(), self.uri.clone());
        self.cache
            .lock()
            .await
            .store_published_diagnostics(&Self::rust(), &published, None, items);
    }
}

fn lsp_error(message: &str) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        message: message.to_owned().into(),
        severity: Some(lsp_types::DiagnosticSeverity::Error),
        ..lsp_types::Diagnostic::default()
    }
}

/// #666: a server that advertises no pull provider and answers `-32601` is
/// answered from the push cache -- no error, nothing logged at ERROR -- and is
/// not asked again.
#[tokio::test]
async fn test_refused_pull_answers_from_the_push_cache_and_is_not_repeated() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let captured = crate::test_lsp::CapturedLogs::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let mut wire = BufReader::new(&mut server.write_stdout);

    let first = fx.spawn_answer();
    let request = next_pull_request(&mut wire).await;
    write_error_response(&mut server.read_half_stdin, &request["id"], -32601, "no").await;
    let first = Fixture::finish_answer(first).await;

    assert_eq!(first.origin, DiagnosticsOrigin::PushCache);
    assert_eq!(first.availability, DiagnosticsAvailability::Pending);
    assert!(first.result.diagnostics.is_empty());
    assert_eq!(
        fx.translator.pull_support(&Fixture::rust()),
        PullSupport::Unsupported
    );

    fx.publish(vec![lsp_error("published")]).await;
    let second = Fixture::finish_answer(fx.spawn_answer()).await;
    assert_eq!(shown(&second.result), ["published"]);
    assert_eq!(second.origin, DiagnosticsOrigin::PushCache);
    assert_eq!(second.availability, DiagnosticsAvailability::Published);
    assert!(
        !captured
            .entries()
            .iter()
            .any(|(level, _)| *level == tracing::Level::ERROR),
        "{:?}",
        captured.entries()
    );
}

/// #666: a published empty list is a clean answer, not a pending one.
#[tokio::test]
async fn test_published_empty_list_of_a_push_only_server_is_clean() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let mut wire = BufReader::new(&mut server.write_stdout);
    let first = fx.spawn_answer();
    let request = next_pull_request(&mut wire).await;
    write_error_response(&mut server.read_half_stdin, &request["id"], -32601, "no").await;
    drop(Fixture::finish_answer(first).await);

    fx.publish(Vec::new()).await;
    let answer = Fixture::finish_answer(fx.spawn_answer()).await;

    assert!(answer.result.diagnostics.is_empty());
    assert_eq!(answer.availability, DiagnosticsAvailability::Published);
    assert_eq!(answer.origin, DiagnosticsOrigin::PushCache);
}

/// #666: pyright advertises no provider yet answers pulls; once one is
/// answered, pulls continue and the origin is `pull`.
#[tokio::test]
async fn test_unadvertised_but_answering_server_keeps_being_pulled() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let mut wire = BufReader::new(&mut server.write_stdout);
    for _ in 0..2 {
        let pull = fx.spawn_answer();
        answer(
            &mut wire,
            &mut server.read_half_stdin,
            full_report(json!([error_item(0, "E0308")])),
        )
        .await;
        let answered = Fixture::finish_answer(pull).await;
        assert_eq!(answered.origin, DiagnosticsOrigin::Pull);
        assert_eq!(answered.availability, DiagnosticsAvailability::Published);
    }
    assert_eq!(
        fx.translator.pull_support(&Fixture::rust()),
        PullSupport::Answers
    );
}

/// #666: only a server that advertises nothing is allowed to refuse; one that
/// advertises a provider and answers `-32601` keeps today's error.
#[tokio::test]
async fn test_advertising_server_refusing_a_pull_is_an_error() {
    let dir = TempDir::new().unwrap();
    let id = Fixture::rust();
    let caps = lsp_types::ServerCapabilities {
        diagnostic_provider: Some(lsp_types::DiagnosticProvider::DiagnosticOptions(
            lsp_types::DiagnosticOptions::new(
                None,
                false,
                false,
                lsp_types::WorkDoneProgressOptions::default(),
            ),
        )),
        ..lsp_types::ServerCapabilities::default()
    };
    let (translator, mut server) = translator_with_capabilities(&dir, &id, caps);
    let path = dir.path().join("lib.rs");
    fs::write(&path, "fn main() {}").unwrap();
    let translator = Arc::new(translator);
    let cache = Arc::new(Mutex::new(NotificationCache::new()));
    let pull = {
        let (translator, cache) = (Arc::clone(&translator), Arc::clone(&cache));
        tokio::spawn(async move {
            translator
                .handle_diagnostics(client_path(path), ResultContext::None, &cache)
                .await
        })
    };
    let mut wire = BufReader::new(&mut server.write_stdout);
    let request = next_pull_request(&mut wire).await;
    write_error_response(&mut server.read_half_stdin, &request["id"], -32601, "no").await;

    assert!(Fixture::finish(pull).await.is_err());
    assert_eq!(translator.pull_support(&id), PullSupport::Advertised);
}

type Answer = JoinHandle<Result<DiagnosticsAnswer>>;

/// Awaits `answer` without the wall-clock cap of `finish_answer`, which would
/// outrun a request timeout under paused time.
async fn settle(answer: Answer) -> Result<DiagnosticsAnswer> {
    answer.await.unwrap()
}

/// Drives one `get_diagnostics` whose pull requests the server answers with
/// the JSON-RPC error `code` each time it is asked (the client retries some).
async fn answer_with_error(
    fx: &Fixture,
    server: &mut FakeServer,
    code: i64,
) -> Result<DiagnosticsAnswer> {
    let mut wire = BufReader::new(&mut server.write_stdout);
    let mut pull = fx.spawn_answer();
    loop {
        tokio::select! {
            request = next_pull_request(&mut wire) => {
                write_error_response(&mut server.read_half_stdin, &request["id"], code, "no").await;
            }
            done = &mut pull => return done.unwrap(),
        }
    }
}

/// Drives one `get_diagnostics` whose pull request the server never answers.
async fn answer_with_timeout(fx: &Fixture, server: &mut FakeServer) -> Result<DiagnosticsAnswer> {
    let mut wire = BufReader::new(&mut server.write_stdout);
    let pull = fx.spawn_answer();
    drop(next_pull_request(&mut wire).await);
    settle(pull).await
}

/// #680: one probe timeout is no proof; two in a row refuse the server, and
/// each of them answers from the push cache.
#[tokio::test(start_paused = true)]
async fn test_second_consecutive_probe_timeout_refuses_the_server() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());

    let first = answer_with_timeout(&fx, &mut server).await.unwrap();
    assert_eq!(first.origin, DiagnosticsOrigin::PushCache);
    assert_eq!(
        fx.translator.pull_support(&Fixture::rust()),
        PullSupport::Probing
    );

    let second = answer_with_timeout(&fx, &mut server).await.unwrap();
    assert_eq!(second.origin, DiagnosticsOrigin::PushCache);
    assert_eq!(
        fx.translator.pull_support(&Fixture::rust()),
        PullSupport::Unsupported
    );
}

/// #680: an answer between two timeouts resets the count, and a server that
/// answers once is never refused for timing out later.
#[tokio::test(start_paused = true)]
async fn test_timeout_answer_timeout_does_not_refuse() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    drop(answer_with_timeout(&fx, &mut server).await.unwrap());

    let mut wire = BufReader::new(&mut server.write_stdout);
    let pull = fx.spawn_answer();
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([])),
    )
    .await;
    drop(Fixture::finish_answer(pull).await);
    assert_eq!(
        fx.translator.pull_support(&Fixture::rust()),
        PullSupport::Answers
    );
    drop(wire);

    let failed = answer_with_timeout(&fx, &mut server).await;
    assert_matches!(failed, Err(Error::Timeout(_)));
    assert_eq!(
        fx.translator.pull_support(&Fixture::rust()),
        PullSupport::Answers
    );
}

/// #680: a cancelled, content-modified or server-cancelled reply says nothing
/// about the method: the call answers from the push cache, the probe stays.
#[tokio::test(start_paused = true)]
async fn test_transient_probe_errors_leave_the_probe_unchanged() {
    for code in [-32800, -32801, -32802] {
        let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
        let answer = answer_with_error(&fx, &mut server, code).await.unwrap();
        assert_eq!(answer.origin, DiagnosticsOrigin::PushCache, "{code}");
        assert_eq!(
            fx.translator.pull_support(&Fixture::rust()),
            PullSupport::Probing,
            "{code}"
        );
    }
}

/// #680: any other server error while probing surfaces as it did before and
/// does not refuse the server.
#[tokio::test(start_paused = true)]
async fn test_other_probe_errors_surface_and_leave_the_probe_unchanged() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let failed = answer_with_error(&fx, &mut server, -32603).await;
    assert_matches!(failed, Err(Error::LspServerError { code: -32603, .. }));
    assert_eq!(
        fx.translator.pull_support(&Fixture::rust()),
        PullSupport::Probing
    );
}

/// #680: two first pulls in flight together that both time out are one failure
/// to answer in time; only a pull sent after that timeout can be the second.
#[tokio::test(start_paused = true)]
async fn test_overlapping_first_timeouts_do_not_refuse_the_server() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let mut wire = BufReader::new(&mut server.write_stdout);
    let first = fx.spawn_answer();
    let second = fx.spawn_answer();
    drop(next_pull_request(&mut wire).await);
    drop(next_pull_request(&mut wire).await);

    assert_eq!(
        settle(first).await.unwrap().origin,
        DiagnosticsOrigin::PushCache
    );
    assert_eq!(
        settle(second).await.unwrap().origin,
        DiagnosticsOrigin::PushCache
    );
    assert_eq!(
        fx.translator.pull_support(&Fixture::rust()),
        PullSupport::Probing
    );

    drop(wire);
    drop(answer_with_timeout(&fx, &mut server).await.unwrap());
    assert_eq!(
        fx.translator.pull_support(&Fixture::rust()),
        PullSupport::Unsupported
    );
}

/// #680: a transient reply between two timeouts neither counts nor resets, and
/// a refused server is not asked again.
#[tokio::test(start_paused = true)]
async fn test_transient_reply_between_timeouts_is_neutral_and_refusal_stops_pulls() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    drop(answer_with_timeout(&fx, &mut server).await.unwrap());
    drop(answer_with_error(&fx, &mut server, -32802).await.unwrap());
    assert_eq!(
        fx.translator.pull_support(&Fixture::rust()),
        PullSupport::Probing
    );
    drop(answer_with_timeout(&fx, &mut server).await.unwrap());
    assert_eq!(
        fx.translator.pull_support(&Fixture::rust()),
        PullSupport::Unsupported
    );

    let mut wire = BufReader::new(&mut server.write_stdout);
    let third = Fixture::finish_answer(fx.spawn_answer()).await;
    assert_eq!(third.origin, DiagnosticsOrigin::PushCache);
    assert!(
        timeout(Duration::from_millis(50), next_pull_request(&mut wire))
            .await
            .is_err(),
        "a refused server was pulled again"
    );
}

/// #680: a timeout followed by `-32601` refuses at once.
#[tokio::test(start_paused = true)]
async fn test_timeout_then_method_not_found_refuses() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    drop(answer_with_timeout(&fx, &mut server).await.unwrap());
    drop(answer_with_error(&fx, &mut server, -32601).await.unwrap());
    assert_eq!(
        fx.translator.pull_support(&Fixture::rust()),
        PullSupport::Unsupported
    );
}

/// SC-006 without disk timing: the tracker's synced version moves while the
/// request is in flight.
#[tokio::test]
async fn test_pull_answered_after_a_resync_is_returned_but_not_stored() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let mut wire = BufReader::new(&mut server.write_stdout);

    let pull = fx.spawn_pull();
    let request = next_pull_request(&mut wire).await;
    fx.translator
        .document_tracker
        .set_synced_version_for_test(&fx.path, &Fixture::rust(), 99);
    write_response(
        &mut server.read_half_stdin,
        &request["id"],
        full_report(json!([error_item(0, "stale")])),
    )
    .await;
    let result = Fixture::finish(pull).await.unwrap();

    assert_eq!(shown(&result), ["stale"]);
    assert!(fx.sources().await.is_none());
    assert!(fx.wiring.changed().is_empty());
}

/// S2: replies arriving in the opposite order of the requests.
#[tokio::test]
async fn test_older_pull_answered_last_does_not_overwrite_the_newer_report() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let mut wire = BufReader::new(&mut server.write_stdout);

    let first = fx.spawn_pull();
    let first_request = next_pull_request(&mut wire).await;
    let second = fx.spawn_pull();
    let second_request = next_pull_request(&mut wire).await;
    write_response(
        &mut server.read_half_stdin,
        &second_request["id"],
        full_report(json!([error_item(0, "newer")])),
    )
    .await;
    drop(Fixture::finish(second).await.unwrap());
    write_response(
        &mut server.read_half_stdin,
        &first_request["id"],
        full_report(json!([error_item(0, "older")])),
    )
    .await;
    let older = Fixture::finish(first).await.unwrap();

    assert_eq!(shown(&older), ["older"]);
    let cached = fx.sources().await.unwrap();
    assert_eq!(message_as_str(&cached.diagnostics[0].message), "newer");
    assert_eq!(fx.wiring.changed().len(), 1);
}

/// S3: the server's entries were cleared (respawn) while its old process
/// still answered.
#[tokio::test]
async fn test_pull_answered_across_a_server_clear_is_not_stored() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    let mut wire = BufReader::new(&mut server.write_stdout);

    let pull = fx.spawn_pull();
    let request = next_pull_request(&mut wire).await;
    fx.cache
        .lock()
        .await
        .clear_server_diagnostics(&Fixture::rust());
    write_response(
        &mut server.read_half_stdin,
        &request["id"],
        full_report(json!([error_item(0, "old process")])),
    )
    .await;
    let result = Fixture::finish(pull).await.unwrap();

    assert_eq!(shown(&result), ["old process"]);
    assert!(fx.sources().await.is_none());
    assert!(fx.wiring.changed().is_empty());
}

#[tokio::test]
async fn test_pull_is_merged_with_pushed_diagnostics_in_the_result() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.cache.lock().await.store_diagnostics(
        &Fixture::rust(),
        &fx.uri,
        Some(1),
        vec![lsp_diag(
            9,
            4,
            lsp_types::DiagnosticSeverity::Warning,
            "flycheck warning",
            None,
        )],
    );
    let mut wire = BufReader::new(&mut server.write_stdout);

    let pull = fx.spawn_pull();
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([error_item(0, "E0308")])),
    )
    .await;
    let result = Fixture::finish(pull).await.unwrap();

    assert_eq!(shown(&result), ["E0308", "flycheck warning"]);
    let pushed = fx.cache.lock().await.diagnostics(&fx.uri).cloned().unwrap();
    assert_eq!(pushed.diagnostics.len(), 1);
}

#[tokio::test]
async fn test_stored_pull_carries_no_redacted_secret() {
    let redactions = crate::redaction::Redactions::new([(
        "API_TOKEN".to_owned(),
        "SuperSecretValue123".to_owned(),
    )]);
    let (fx, mut server) = Fixture::new(redactions);
    let mut wire = BufReader::new(&mut server.write_stdout);

    let pull = fx.spawn_pull();
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([error_item(0, "leaked SuperSecretValue123")])),
    )
    .await;
    drop(Fixture::finish(pull).await.unwrap());

    let cached = serde_json::to_string(&fx.sources().await.unwrap().diagnostics).unwrap();
    assert!(!cached.contains("SuperSecretValue123"), "{cached}");
    assert!(cached.contains("[redacted:API_TOKEN]"), "{cached}");
}

#[tokio::test]
async fn test_pull_without_any_subscription_is_stored_but_publishes_nothing() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.wiring
        .subscribed
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let mut wire = BufReader::new(&mut server.write_stdout);

    let pull = fx.spawn_pull();
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([error_item(0, "E0308")])),
    )
    .await;
    drop(Fixture::finish(pull).await.unwrap());

    assert_eq!(fx.sources().await.unwrap().diagnostics.len(), 1);
    assert!(fx.wiring.changed().is_empty());
}

/// A first pull whose items the pushes already show leaves the merged view as
/// it was, so a subscriber is told nothing.
#[tokio::test]
async fn test_first_pull_duplicating_pushed_items_publishes_nothing() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.cache.lock().await.store_diagnostics(
        &Fixture::rust(),
        &fx.uri,
        Some(1),
        vec![lsp_diag(
            0,
            4,
            lsp_types::DiagnosticSeverity::Error,
            "E0308",
            Some("E0308"),
        )],
    );
    let mut wire = BufReader::new(&mut server.write_stdout);

    let pull = fx.spawn_pull();
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([error_item(0, "E0308")])),
    )
    .await;
    let result = Fixture::finish(pull).await.unwrap();

    assert_eq!(shown(&result), ["E0308"]);
    assert!(fx.wiring.changed().is_empty());
}
