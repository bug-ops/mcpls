//! `get_diagnostics` as the writer of the pulled slot: storing, publishing and
//! the races a pull in flight can lose.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{BufReader, DuplexStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::*;
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
                ServerId::from("rust"),
                LanguageId::from_static("rust"),
            )]));
        translator.set_workspace_roots(
            WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap(),
        );
        let (client, server, _lanes) = fake_lsp_client_with_redactions(redactions);
        translator.register_client("rust".to_string(), client);
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
        ServerId::from("rust")
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
    write_error_response(&mut server.read_half_stdin, &request["id"], -32601, "no").await;

    assert!(Fixture::finish(pull).await.is_err());
    assert!(fx.sources().await.is_none());
    assert!(fx.wiring.changed().is_empty());
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
