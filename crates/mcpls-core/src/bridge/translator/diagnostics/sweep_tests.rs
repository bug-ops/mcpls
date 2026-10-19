//! The per-file sweep step (#648): when a tracked file is resynced and pulled,
//! and what the pull publishes.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::BufReader;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::pull_tests::{Fixture, answer, error_item, full_report, next_pull_request};
use super::sweep::{Interest, SweepMemo, SweepStep};
use super::*;
use crate::bridge::state::ResourceLimits;
use crate::bridge::translator::testing::*;
use crate::config::{DocumentLimit, WorkspaceTrust};
use crate::test_lsp::client_path;

type Sweep = JoinHandle<SweepStep>;

impl Fixture {
    fn spawn_sweep(&self, memo: &SweepMemo) -> Sweep {
        self.spawn_sweep_of(self.path.clone(), Interest::Subscribed, memo)
    }

    fn spawn_sweep_of(&self, path: PathBuf, interest: Interest, memo: &SweepMemo) -> Sweep {
        let (translator, cache) = (Arc::clone(&self.translator), Arc::clone(&self.cache));
        let memo = memo.clone();
        tokio::spawn(async move { translator.sweep_file(&path, interest, &memo, &cache).await })
    }

    async fn finish_sweep(sweep: Sweep) -> SweepStep {
        timeout(Duration::from_secs(5), sweep)
            .await
            .expect("the sweep step did not finish")
            .unwrap()
    }

    /// Rewrites the file with content of another length, which every
    /// filesystem's mtime granularity reports as a change.
    fn edit(&self, content: &str) {
        fs::write(&self.path, content).unwrap();
    }

    /// Lets the cache stamp pulled slots with the tracked opening, as the
    /// server does at startup.
    async fn attach_tracker(&self) {
        self.cache
            .lock()
            .await
            .attach_documents(Arc::clone(self.translator.document_tracker()));
    }

    /// Records that the server answers pulls, as a tool call's probe would.
    fn settle_probe(&self) {
        let connection = lock_std(&self.translator.servers)
            .client(&Self::rust())
            .unwrap()
            .connection_id();
        self.translator
            .record_pull_probe(&Self::rust(), connection, |_| PullProbe::Answered);
    }

    async fn pull_answered(&self, server: &mut crate::test_lsp::FakeServer, report: Value) {
        let mut wire = BufReader::new(&mut server.write_stdout);
        let pull = self.spawn_pull();
        answer(&mut wire, &mut server.read_half_stdin, report).await;
        drop(Self::finish(pull).await.unwrap());
    }
}

#[tokio::test]
async fn test_sweep_pulls_an_edited_file_once_and_is_quiet_afterwards() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.attach_tracker().await;
    fx.pull_answered(&mut server, full_report(json!([error_item(0, "first")])))
        .await;
    let memo = SweepMemo::default();

    assert_eq!(
        Fixture::finish_sweep(fx.spawn_sweep(&memo)).await,
        SweepStep::Synced
    );

    fx.edit("fn main() { let x: i32 = \"s\"; }");
    let sweep = fx.spawn_sweep(&memo);
    let mut wire = BufReader::new(&mut server.write_stdout);
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([error_item(0, "second")])),
    )
    .await;
    assert_eq!(Fixture::finish_sweep(sweep).await, SweepStep::Pulled);
    assert_eq!(fx.wiring.changed().len(), 2);

    for _ in 0..100 {
        assert_eq!(
            Fixture::finish_sweep(fx.spawn_sweep(&memo)).await,
            SweepStep::Synced
        );
    }
    assert_eq!(fx.wiring.changed().len(), 2);
}

/// An edit that lands before the first sweep of a file nobody pulled yet is
/// not swallowed as a baseline.
#[tokio::test]
async fn test_sweep_notifies_an_edit_made_before_its_first_tick() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.attach_tracker().await;
    fx.settle_probe();
    drop(
        fx.translator
            .prepare_document(&client_path(fx.path.clone()), ToolKind::Diagnostics)
            .await
            .unwrap(),
    );
    fx.edit("fn main() { let x: i32 = \"s\"; }");

    let sweep = fx.spawn_sweep(&SweepMemo::default());
    let mut wire = BufReader::new(&mut server.write_stdout);
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([error_item(0, "E0308")])),
    )
    .await;

    assert_eq!(Fixture::finish_sweep(sweep).await, SweepStep::Pulled);
    assert_eq!(fx.wiring.changed(), std::slice::from_ref(&fx.uri));
}

/// A report that is not stored leaves no stamp in the cache; the memo keeps
/// the file from being pulled again for the same state.
#[tokio::test]
async fn test_sweep_pulls_a_state_whose_report_is_not_stored_only_once() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.attach_tracker().await;
    fx.pull_answered(&mut server, full_report(json!([error_item(0, "first")])))
        .await;
    let memo = SweepMemo::default();
    fx.edit("fn main() { let x: i32 = \"s\"; }");

    let sweep = fx.spawn_sweep(&memo);
    let mut wire = BufReader::new(&mut server.write_stdout);
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        json!({"kind": "unchanged", "resultId": "1"}),
    )
    .await;
    assert_eq!(Fixture::finish_sweep(sweep).await, SweepStep::Pulled);

    for _ in 0..3 {
        assert_eq!(
            Fixture::finish_sweep(fx.spawn_sweep(&memo)).await,
            SweepStep::Synced
        );
    }
    assert_eq!(fx.wiring.changed().len(), 1);
}

/// A server that answers no pull is only brought up to date.
#[tokio::test]
async fn test_sweep_resyncs_a_push_only_server_without_pulling() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.attach_tracker().await;
    let mut wire = BufReader::new(&mut server.write_stdout);
    let first = tokio::spawn({
        let (translator, cache, path) = (
            Arc::clone(&fx.translator),
            Arc::clone(&fx.cache),
            fx.path.clone(),
        );
        async move {
            translator
                .handle_diagnostics(client_path(path), ResultContext::None, &cache)
                .await
        }
    });
    let request = next_pull_request(&mut wire).await;
    write_error_response(&mut server.read_half_stdin, &request["id"], -32601, "no").await;
    drop(Fixture::finish(first).await.unwrap());

    fx.edit("fn main() { let x: i32 = \"s\"; }");
    let step = Fixture::finish_sweep(fx.spawn_sweep(&SweepMemo::default())).await;

    assert_eq!(step, SweepStep::Synced);
    let resync = timeout(Duration::from_secs(5), async {
        loop {
            let message = read_framed_message(&mut wire).await;
            assert_ne!(message["method"], "textDocument/diagnostic");
            if message["method"] == "textDocument/didChange" {
                return message;
            }
        }
    })
    .await
    .expect("no didChange was sent");
    assert_eq!(resync["params"]["textDocument"]["version"], 2);
}

/// The sweep neither respawns a dead server nor sends it anything.
#[cfg(unix)]
#[tokio::test]
async fn test_sweep_leaves_a_dead_server_alone() {
    use crate::bridge::translator::testing::{stub_server_config, write_responder_script};

    let dir = tempfile::TempDir::new().unwrap();
    let id = ServerId::from_static("rust");
    let dead = crate::lsp::fake_lsp_server_with_dead_loop_and_live_child();
    let mut translator = Translator::new()
        .with_extensions(crate::test_lsp::test_extensions())
        .with_router(crate::config::ToolRouter::catch_all([(
            id.clone(),
            crate::config::LanguageId::from_static("rust"),
        )]));
    translator.set_workspace_roots(WorkspaceRoots::from_paths(&[canonical_dir(&dir)]).unwrap());
    translator.register_client(id.clone(), dead.client().clone());
    translator.register_server(id.clone(), dead);
    lock_std(&translator.servers)
        .server_mut(&id)
        .unwrap()
        .set_init_config(stub_server_config(
            "rust",
            &write_responder_script(dir.path(), 5),
        ));
    timeout(Duration::from_secs(2), async {
        while translator.dead_server_config(&id).is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the seed server never reported as dead");
    let file = canonical_dir(&dir).join("lib.rs");
    fs::write(&file, "fn main() {}").unwrap();

    assert_eq!(translator.sweep_server(&file), None);
    let cache = Mutex::new(NotificationCache::new());
    let step = translator
        .sweep_file(&file, Interest::Subscribed, &SweepMemo::default(), &cache)
        .await;

    assert_eq!(step, SweepStep::Skipped);
    assert!(translator.dead_server_config(&id).is_some());
}

/// A caller dropped once the report is stored still delivers the
/// notification: the store and the publication are one step.
#[tokio::test]
async fn test_pull_dropped_after_the_store_still_notifies_once() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.attach_tracker().await;
    fx.wiring.yields.store(true, Ordering::Relaxed);
    let responder = tokio::spawn(async move {
        let mut wire = BufReader::new(&mut server.write_stdout);
        loop {
            answer(
                &mut wire,
                &mut server.read_half_stdin,
                full_report(json!([error_item(0, "E0308")])),
            )
            .await;
        }
    });
    let path = fx
        .translator
        .validate_path(&client_path(fx.path.clone()))
        .await
        .unwrap();
    let mut pull = Box::pin(fx.translator.handle_validated_diagnostics(
        &path,
        ResultContext::None,
        &fx.cache,
    ));

    let mut polls = 0_u32;
    loop {
        assert!(
            futures::poll!(pull.as_mut()).is_pending(),
            "pull ended unobserved"
        );
        polls += 1;
        assert!(polls < 1_000, "the report was never stored");
        let stored = fx
            .cache
            .lock()
            .await
            .pulled_stamp(&Fixture::rust(), &fx.uri)
            .is_some();
        if stored {
            break;
        }
        tokio::task::yield_now().await;
    }
    drop(pull);
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    responder.abort();

    assert_eq!(fx.wiring.changed(), std::slice::from_ref(&fx.uri));
}

/// A tracked file that became a symlink out of the workspace is neither read
/// nor sent to the server.
#[cfg(unix)]
#[tokio::test]
async fn test_sweep_skips_a_tracked_file_replaced_by_a_symlink_out_of_the_roots() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.attach_tracker().await;
    fx.pull_answered(&mut server, full_report(json!([error_item(0, "first")])))
        .await;
    let outside = tempfile::TempDir::new().unwrap();
    let secret = outside.path().join("credentials");
    fs::write(&secret, "SECRET_OUTSIDE_ROOTS").unwrap();
    fs::remove_file(&fx.path).unwrap();
    std::os::unix::fs::symlink(&secret, &fx.path).unwrap();
    let memo = SweepMemo::default();

    let step = Fixture::finish_sweep(fx.spawn_sweep(&memo)).await;

    assert_eq!(step, SweepStep::Skipped);
    let mut wire = BufReader::new(&mut server.write_stdout);
    let mut sent = Vec::new();
    while let Ok(message) =
        timeout(Duration::from_millis(200), read_framed_message(&mut wire)).await
    {
        sent.push(message.to_string());
    }
    assert!(
        sent.iter().all(|m| !m.contains("SECRET_OUTSIDE_ROOTS")),
        "{sent:?}"
    );
    assert!(sent.iter().all(|m| !m.contains("textDocument/didChange")));
}

/// A server that does not answer the pull ends the sweep of its files.
#[tokio::test(start_paused = true)]
async fn test_sweep_reports_a_timed_out_pull() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.attach_tracker().await;
    fx.pull_answered(&mut server, full_report(json!([error_item(0, "first")])))
        .await;
    fx.edit("fn main() { let x: i32 = \"s\"; }");

    let step = timeout(
        Duration::from_secs(3600),
        fx.spawn_sweep(&SweepMemo::default()),
    )
    .await
    .expect("the pull never timed out")
    .unwrap();

    assert_eq!(step, SweepStep::TimedOut);
}

/// A server that fails the pull while it churns (`ContentModified` after a
/// checkout) is asked again on the next tick, and the edit is announced.
#[tokio::test]
async fn test_sweep_retries_a_failed_pull_and_then_notifies() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.attach_tracker().await;
    fx.pull_answered(&mut server, full_report(json!([error_item(0, "first")])))
        .await;
    let memo = SweepMemo::default();
    fx.edit("fn main() { let x: i32 = \"s\"; }");

    let sweep = fx.spawn_sweep(&memo);
    let mut wire = BufReader::new(&mut server.write_stdout);
    let request = next_pull_request(&mut wire).await;
    write_error_response(&mut server.read_half_stdin, &request["id"], -32603, "busy").await;
    assert_eq!(Fixture::finish_sweep(sweep).await, SweepStep::Pulled);
    assert_eq!(fx.wiring.changed().len(), 1);

    let sweep = fx.spawn_sweep(&memo);
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([error_item(0, "second")])),
    )
    .await;
    assert_eq!(Fixture::finish_sweep(sweep).await, SweepStep::Pulled);
    assert_eq!(fx.wiring.changed().len(), 2);
}

/// A state whose pull keeps failing is left alone after a few attempts.
#[tokio::test]
async fn test_sweep_gives_up_on_a_state_after_repeated_failures() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.attach_tracker().await;
    fx.pull_answered(&mut server, full_report(json!([error_item(0, "first")])))
        .await;
    let memo = SweepMemo::default();
    fx.edit("fn main() { let x: i32 = \"s\"; }");
    let mut wire = BufReader::new(&mut server.write_stdout);

    for _ in 0..3 {
        let sweep = fx.spawn_sweep(&memo);
        let request = next_pull_request(&mut wire).await;
        write_error_response(&mut server.read_half_stdin, &request["id"], -32603, "busy").await;
        assert_eq!(Fixture::finish_sweep(sweep).await, SweepStep::Pulled);
    }

    assert_eq!(
        Fixture::finish_sweep(fx.spawn_sweep(&memo)).await,
        SweepStep::Synced
    );
}

/// A file the tracker evicted meanwhile is not opened again for its
/// subscribers.
#[tokio::test]
async fn test_sweep_does_not_reopen_an_untracked_file() {
    let (fx, _server) = Fixture::new(crate::redaction::Redactions::default());

    let step = Fixture::finish_sweep(fx.spawn_sweep(&SweepMemo::default())).await;

    assert_eq!(step, SweepStep::Skipped);
    assert!(fx.translator.document_tracker().open_paths().is_empty());
}

/// The sweep never probes: a server that advertises nothing and has not shown
/// that it answers pulls is only resynced, so background pulls cannot turn it
/// into a push-only server for tool calls.
#[tokio::test]
async fn test_sweep_does_not_probe_a_server_whose_pull_support_is_unknown() {
    let (fx, _server) = Fixture::new(crate::redaction::Redactions::default());
    fx.attach_tracker().await;
    drop(
        fx.translator
            .prepare_document(&client_path(fx.path.clone()), ToolKind::Diagnostics)
            .await
            .unwrap(),
    );
    fx.edit("fn main() { let x: i32 = \"s\"; }");

    let step = Fixture::finish_sweep(fx.spawn_sweep(&SweepMemo::default())).await;

    assert_eq!(step, SweepStep::Synced);
    assert_eq!(
        fx.translator.pull_support(&Fixture::rust()),
        PullSupport::Probing
    );
}

fn sync_options(save: Option<lsp_types::Save>) -> lsp_types::ServerCapabilities {
    lsp_types::ServerCapabilities {
        text_document_sync: Some(lsp_types::TextDocumentSync::Options(
            lsp_types::TextDocumentSyncOptions {
                save,
                ..lsp_types::TextDocumentSyncOptions::default()
            },
        )),
        ..lsp_types::ServerCapabilities::default()
    }
}

/// Everything the server end of `wire` receives within a short wait.
async fn drain(wire: &mut BufReader<&mut tokio::io::DuplexStream>) -> Vec<Value> {
    let mut sent = Vec::new();
    while let Ok(message) = timeout(Duration::from_millis(300), read_framed_message(wire)).await {
        sent.push(message);
    }
    sent
}

/// The notifications a server receives from tracking a file, editing it and
/// sweeping it once.
async fn messages_for_a_swept_edit(capabilities: lsp_types::ServerCapabilities) -> Vec<Value> {
    let dir = TempDir::new().unwrap();
    let id = Fixture::rust();
    let (translator, mut server) = translator_with_capabilities(&dir, &id, capabilities);
    let file = canonical_dir(&dir).join("lib.rs");
    fs::write(&file, "fn main() {}").unwrap();
    drop(
        translator
            .prepare_document(&client_path(file.clone()), ToolKind::Diagnostics)
            .await
            .unwrap(),
    );
    fs::write(&file, "fn main() { let x = 1; }").unwrap();
    let cache = Mutex::new(NotificationCache::new());
    translator
        .sweep_file(&file, Interest::Subscribed, &SweepMemo::default(), &cache)
        .await;
    drain(&mut BufReader::new(&mut server.write_stdout)).await
}

fn methods(messages: &[Value]) -> Vec<&str> {
    messages
        .iter()
        .filter_map(|message| message["method"].as_str())
        .collect()
}

/// A server that asked for `didSave` gets it once, right after the change.
#[tokio::test]
async fn test_sweep_sends_did_save_after_the_change_without_text() {
    let sent = Box::pin(messages_for_a_swept_edit(sync_options(Some(
        lsp_types::Save::Bool(true),
    ))))
    .await;

    assert_eq!(
        methods(&sent),
        [
            "textDocument/didOpen",
            "textDocument/didChange",
            "textDocument/didSave"
        ]
    );
    assert!(sent[2]["params"].get("text").is_none());
}

#[tokio::test]
async fn test_sweep_sends_the_text_when_the_server_asked_for_it() {
    let save = lsp_types::Save::SaveOptions(lsp_types::SaveOptions {
        include_text: Some(true),
    });

    let sent = Box::pin(messages_for_a_swept_edit(sync_options(Some(save)))).await;

    assert_eq!(sent.len(), 3);
    assert_eq!(sent[2]["method"], "textDocument/didSave");
    assert_eq!(sent[2]["params"]["text"], "fn main() { let x = 1; }");
}

/// Without a save option the notification stays unsent, as the specification
/// requires.
#[tokio::test]
async fn test_sweep_sends_no_did_save_to_a_server_that_did_not_ask() {
    for capabilities in [
        sync_options(None),
        sync_options(Some(lsp_types::Save::Bool(false))),
        lsp_types::ServerCapabilities {
            text_document_sync: Some(lsp_types::TextDocumentSync::Kind(
                lsp_types::TextDocumentSyncKind::Full,
            )),
            ..lsp_types::ServerCapabilities::default()
        },
        lsp_types::ServerCapabilities::default(),
    ] {
        let sent = Box::pin(messages_for_a_swept_edit(capabilities)).await;

        assert_eq!(
            methods(&sent),
            ["textDocument/didOpen", "textDocument/didChange"]
        );
    }
}

/// A file that has not changed since the last visit is not saved again.
#[tokio::test]
async fn test_sweep_sends_did_save_once_per_change() {
    let dir = TempDir::new().unwrap();
    let id = Fixture::rust();
    let capabilities = sync_options(Some(lsp_types::Save::Bool(true)));
    let (translator, mut server) = translator_with_capabilities(&dir, &id, capabilities);
    let file = canonical_dir(&dir).join("lib.rs");
    fs::write(&file, "fn main() {}").unwrap();
    drop(
        translator
            .prepare_document(&client_path(file.clone()), ToolKind::Diagnostics)
            .await
            .unwrap(),
    );
    let (memo, cache) = (SweepMemo::default(), Mutex::new(NotificationCache::new()));
    fs::write(&file, "fn main() { let x = 1; }").unwrap();

    for _ in 0..3 {
        translator
            .sweep_file(&file, Interest::Subscribed, &memo, &cache)
            .await;
    }

    let sent = drain(&mut BufReader::new(&mut server.write_stdout)).await;
    let saves = methods(&sent)
        .into_iter()
        .filter(|method| *method == "textDocument/didSave")
        .count();
    assert_eq!(saves, 1);
}

fn count_of(sent: &[Value], method: &str) -> usize {
    methods(sent)
        .into_iter()
        .filter(|sent| *sent == method)
        .count()
}

/// Untrusted mode withholds `didSave`: it makes servers run build scripts.
#[tokio::test]
async fn test_sweep_sends_no_did_save_in_an_untrusted_workspace() {
    let dir = TempDir::new().unwrap();
    let id = Fixture::rust();
    let capabilities = sync_options(Some(lsp_types::Save::Bool(true)));
    let (translator, mut server) = translator_with_capabilities(&dir, &id, capabilities);
    let translator = translator.with_workspace_trust(WorkspaceTrust::untrusted([id]));
    let file = canonical_dir(&dir).join("lib.rs");
    fs::write(&file, "fn main() {}").unwrap();
    drop(
        translator
            .prepare_document(&client_path(file.clone()), ToolKind::Diagnostics)
            .await
            .unwrap(),
    );
    fs::write(&file, "fn main() { let x = 1; }").unwrap();

    translator
        .sweep_file(
            &file,
            Interest::Subscribed,
            &SweepMemo::default(),
            &Mutex::new(NotificationCache::new()),
        )
        .await;

    let sent = drain(&mut BufReader::new(&mut server.write_stdout)).await;
    assert_eq!(
        methods(&sent),
        ["textDocument/didOpen", "textDocument/didChange"]
    );
}

/// A file evicted and opened again with new content is saved too.
#[tokio::test]
async fn test_sweep_sends_did_save_for_a_reopened_file() {
    let dir = TempDir::new().unwrap();
    let id = Fixture::rust();
    let capabilities = sync_options(Some(lsp_types::Save::Bool(true)));
    let (translator, mut server) = translator_with_capabilities(&dir, &id, capabilities);
    let translator = translator.with_resource_limits(ResourceLimits {
        max_documents: DocumentLimit::new(1),
        ..ResourceLimits::default()
    });
    let (file, other) = (
        canonical_dir(&dir).join("lib.rs"),
        canonical_dir(&dir).join("other.rs"),
    );
    fs::write(&file, "fn main() {}").unwrap();
    fs::write(&other, "fn other() {}").unwrap();
    let (memo, cache) = (SweepMemo::default(), Mutex::new(NotificationCache::new()));
    drop(
        translator
            .prepare_document(&client_path(file.clone()), ToolKind::Diagnostics)
            .await
            .unwrap(),
    );
    translator
        .sweep_file(&file, Interest::Subscribed, &memo, &cache)
        .await;
    drop(
        translator
            .prepare_document(&client_path(other), ToolKind::Diagnostics)
            .await
            .unwrap(),
    );
    fs::write(&file, "fn main() { let x = 1; }").unwrap();
    drop(
        translator
            .prepare_document(&client_path(file.clone()), ToolKind::Diagnostics)
            .await
            .unwrap(),
    );

    translator
        .sweep_file(&file, Interest::Subscribed, &memo, &cache)
        .await;

    let sent = drain(&mut BufReader::new(&mut server.write_stdout)).await;
    assert_eq!(count_of(&sent, "textDocument/didSave"), 1);
}

/// A file edited on disk after its memo entry was trimmed is saved at its next
/// visit: the tracker's own state is the baseline.
#[tokio::test]
async fn test_sweep_sends_did_save_for_an_edit_made_after_the_memo_forgot_the_file() {
    let dir = TempDir::new().unwrap();
    let id = Fixture::rust();
    let capabilities = sync_options(Some(lsp_types::Save::Bool(true)));
    let (translator, mut server) = translator_with_capabilities(&dir, &id, capabilities);
    let file = canonical_dir(&dir).join("lib.rs");
    fs::write(&file, "fn main() {}").unwrap();
    drop(
        translator
            .prepare_document(&client_path(file.clone()), ToolKind::Diagnostics)
            .await
            .unwrap(),
    );
    let (memo, cache) = (SweepMemo::default(), Mutex::new(NotificationCache::new()));
    translator
        .sweep_file(&file, Interest::Companion, &memo, &cache)
        .await;
    memo.retain(|_| false);
    fs::write(&file, "fn main() { let x = 1; }").unwrap();

    translator
        .sweep_file(&file, Interest::Companion, &memo, &cache)
        .await;

    let sent = drain(&mut BufReader::new(&mut server.write_stdout)).await;
    assert_eq!(count_of(&sent, "textDocument/didSave"), 1);
}

impl Fixture {
    /// Tracks a second file next to the fixture's and returns its path.
    async fn track_companion(&self) -> PathBuf {
        let path = self.path.with_file_name("other.rs");
        fs::write(&path, "fn other() {}").unwrap();
        drop(
            self.translator
                .prepare_document(&client_path(path.clone()), ToolKind::Diagnostics)
                .await
                .unwrap(),
        );
        path
    }
}

/// An edit to a companion that nobody subscribes to is resynced without a
/// pull, and moves the subscribed file of the same server to a new pull.
#[tokio::test]
async fn test_a_companion_edit_makes_the_subscribed_file_pull_once() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.attach_tracker().await;
    fx.pull_answered(&mut server, full_report(json!([error_item(0, "first")])))
        .await;
    let companion = fx.track_companion().await;
    let memo = SweepMemo::default();
    assert_eq!(
        Fixture::finish_sweep(fx.spawn_sweep(&memo)).await,
        SweepStep::Synced
    );
    let visit = |interest| fx.spawn_sweep_of(companion.clone(), interest, &memo);
    assert_eq!(
        Fixture::finish_sweep(visit(Interest::Companion)).await,
        SweepStep::Synced
    );

    fs::write(&companion, "fn other() { let y = 2; }").unwrap();
    assert_eq!(
        Fixture::finish_sweep(visit(Interest::Companion)).await,
        SweepStep::Synced
    );
    let sweep = fx.spawn_sweep(&memo);
    let mut wire = BufReader::new(&mut server.write_stdout);
    answer(
        &mut wire,
        &mut server.read_half_stdin,
        full_report(json!([error_item(0, "second")])),
    )
    .await;

    assert_eq!(Fixture::finish_sweep(sweep).await, SweepStep::Pulled);
    for _ in 0..3 {
        assert_eq!(
            Fixture::finish_sweep(fx.spawn_sweep(&memo)).await,
            SweepStep::Synced
        );
    }
}

/// Visiting a companion neither pulls it nor makes it the most recently used
/// document.
#[tokio::test]
async fn test_a_companion_visit_leaves_the_lru_order_alone() {
    let (fx, mut server) = Fixture::new(crate::redaction::Redactions::default());
    fx.attach_tracker().await;
    fx.pull_answered(&mut server, full_report(json!([error_item(0, "first")])))
        .await;
    let companion = fx.track_companion().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(
        fx.translator
            .prepare_document(&client_path(fx.path.clone()), ToolKind::Diagnostics)
            .await
            .unwrap(),
    );
    let order = fx.translator.document_tracker().open_paths_by_recency();
    assert_eq!(order, [fx.path.clone(), companion.clone()]);
    fs::write(&companion, "fn other() { let y = 2; }").unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let step = Fixture::finish_sweep(fx.spawn_sweep_of(
        companion,
        Interest::Companion,
        &SweepMemo::default(),
    ))
    .await;

    assert_eq!(step, SweepStep::Synced);
    assert_eq!(
        fx.translator.document_tracker().open_paths_by_recency(),
        order
    );
}

/// Re-pulls that are due only because another file of the server changed are
/// limited per window, and the budget returns with the next one.
#[test]
fn test_epoch_driven_pulls_are_budgeted_per_window() {
    let memo = SweepMemo::default();
    let (rust, other) = (Fixture::rust(), ServerId::from_static("other"));
    let start = tokio::time::Instant::now();

    let granted = (0..20)
        .filter(|_| memo.take_epoch_pull(&rust, start))
        .count();

    assert_eq!(granted, 8);
    assert!(memo.take_epoch_pull(&other, start));
    assert!(!memo.take_epoch_pull(&rust, start + Duration::from_millis(1999)));
    assert!(memo.take_epoch_pull(&rust, start + Duration::from_secs(2)));
}
