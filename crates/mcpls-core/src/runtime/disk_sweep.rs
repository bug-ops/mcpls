//! Tells `lsp-diagnostics://` subscribers about on-disk edits no tool call
//! observes (#648, spec bridge/009 Option E).
//!
//! While some session subscribes to a tracked file, the sweep resyncs the file
//! with the disk every [`SWEEP_INTERVAL`] and, when that moved the document to
//! a state the cache holds no pulled report for, pulls it, which publishes the
//! change like a `get_diagnostics` call would. Files of one server are swept by
//! one job at a time, so a wedged server stalls only its own files.
//!
//! Not covered (spec bridge/009 Options B and C): flycheck-only diagnostics of
//! subscribed files and diagnostics that change because another file was
//! edited.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::task::{Id, JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::bridge::resources::DiagnosticsResourceUri;
use crate::bridge::{NotificationCache, SweepMemo, SweepStep, Translator, path_to_uri};
use crate::config::ServerId;
use crate::mcp::SubscriptionRegistry;

/// Time between two sweeps.
const SWEEP_INTERVAL: Duration = Duration::from_secs(2);

/// Most files one sweep visits; the rest follow on later ticks.
const MAX_SWEPT_FILES_PER_TICK: usize = 64;

/// Starts the sweep, which runs until `cancel` fires or the handle is
/// stopped with [`stop_disk_sweep`].
///
/// The task owns the per-server jobs: ending it aborts them too.
pub fn spawn_disk_sweep(
    translator: &Arc<Translator>,
    notification_cache: &Arc<Mutex<NotificationCache>>,
    subscriptions: &SubscriptionRegistry,
    cancel: &CancellationToken,
) -> JoinHandle<()> {
    let mut sweep = DiskSweep::new(
        Arc::clone(translator),
        Arc::clone(notification_cache),
        subscriptions.clone(),
    );
    let cancel = cancel.clone();
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(SWEEP_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                _ = ticks.tick() => sweep.tick(&cancel).await,
            }
        }
    })
}

/// Stops the sweep started by [`spawn_disk_sweep`] and waits until its task is
/// gone, so no job touches a server that shutdown is about to stop.
///
/// A job aborted inside `ensure_open` leaves nothing behind: a document whose
/// resync did not finish is resynced by the next call that needs it.
pub async fn stop_disk_sweep(handle: JoinHandle<()>) {
    handle.abort();
    match handle.await {
        Err(error) if error.is_panic() => warn!("disk sweep panicked: {error}"),
        Ok(()) | Err(_) => {}
    }
}

struct DiskSweep {
    translator: Arc<Translator>,
    notification_cache: Arc<Mutex<NotificationCache>>,
    subscriptions: SubscriptionRegistry,
    memo: SweepMemo,
    jobs: JoinSet<()>,
    /// Server each unfinished job sweeps.
    running: HashMap<Id, ServerId>,
    /// Where the next tick starts in the sorted list of swept files, so a
    /// backlog beyond [`MAX_SWEPT_FILES_PER_TICK`] is visited in turn.
    next_start: usize,
}

impl DiskSweep {
    fn new(
        translator: Arc<Translator>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        subscriptions: SubscriptionRegistry,
    ) -> Self {
        Self {
            translator,
            notification_cache,
            subscriptions,
            memo: SweepMemo::default(),
            jobs: JoinSet::new(),
            running: HashMap::new(),
            next_start: 0,
        }
    }

    async fn tick(&mut self, cancel: &CancellationToken) {
        self.reap_finished();
        if !self.subscriptions.any_subscription().await {
            self.memo.retain(|_| false);
            return;
        }
        let subscribed = self.subscribed_tracked_files().await;
        let kept: HashSet<&Path> = subscribed.iter().map(PathBuf::as_path).collect();
        self.memo.retain(|path| kept.contains(path));

        let mut by_server: HashMap<ServerId, Vec<PathBuf>> = HashMap::new();
        for (path, server) in self.next_window(&subscribed) {
            by_server.entry(server).or_default().push(path);
        }
        for (server, files) in by_server {
            let job = sweep_files(
                Arc::clone(&self.translator),
                Arc::clone(&self.notification_cache),
                self.memo.clone(),
                cancel.clone(),
                files,
            );
            let id = self.jobs.spawn(job).id();
            self.running.insert(id, server);
        }
    }

    fn reap_finished(&mut self) {
        while let Some(finished) = self.jobs.try_join_next_with_id() {
            match finished {
                Ok((id, ())) => {
                    self.running.remove(&id);
                }
                Err(error) => {
                    self.running.remove(&error.id());
                    if error.is_panic() {
                        warn!("disk sweep job panicked: {error}");
                    }
                }
            }
        }
    }

    /// Tracked files some session or listen stream subscribes to, sorted.
    async fn subscribed_tracked_files(&self) -> Vec<PathBuf> {
        let mut tracked = self.translator.document_tracker().open_paths();
        tracked.sort();
        let resources = tracked
            .into_iter()
            .filter_map(|path| {
                let resource = path_to_uri(&path)
                    .ok()
                    .and_then(|uri| DiagnosticsResourceUri::for_canonical_file(&uri))?;
                Some((path, resource))
            })
            .collect();
        self.subscriptions.retain_subscribed(resources).await
    }

    /// The files this tick visits with their servers: at most
    /// [`MAX_SWEPT_FILES_PER_TICK`], taken in order from where the previous
    /// tick stopped. Files of a server whose job still runs, or that no
    /// running server routes, are passed over without using up the window.
    fn next_window(&mut self, subscribed: &[PathBuf]) -> Vec<(PathBuf, ServerId)> {
        let len = subscribed.len();
        let start = self.next_start.checked_rem(len).unwrap_or(0);
        let mut window = Vec::new();
        let mut examined = 0_usize;
        for path in subscribed.iter().cycle().skip(start).take(len) {
            if window.len() >= MAX_SWEPT_FILES_PER_TICK {
                break;
            }
            examined = examined.saturating_add(1);
            let Some(server) = self.translator.sweep_server(path) else {
                continue;
            };
            if self.running.values().any(|running| *running == server) {
                continue;
            }
            window.push((path.clone(), server));
        }
        self.next_start = start.saturating_add(examined).checked_rem(len).unwrap_or(0);
        window
    }
}

/// Sweeps `files`, all of one server, one after another.
async fn sweep_files(
    translator: Arc<Translator>,
    notification_cache: Arc<Mutex<NotificationCache>>,
    memo: SweepMemo,
    cancel: CancellationToken,
    files: Vec<PathBuf>,
) {
    for path in files {
        if cancel.is_cancelled() {
            return;
        }
        let step = translator
            .sweep_file(&path, &memo, &notification_cache)
            .await;
        debug!(path = %path.display(), ?step, "disk sweep visited a file");
        if step == SweepStep::TimedOut {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::{assert_matches, fs};

    use serde_json::json;
    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::sync::mpsc;

    use super::*;
    use crate::bridge::{ResultContext, WorkspaceRoots};
    use crate::config::{LanguageId, ToolRouter};
    use crate::mcp::{SessionHandle, Target};
    use crate::runtime::pump::{DiagnosticsRoles, PumpShared, PumpWiring};
    use crate::test_lsp::{
        FakeServer, client_path, fake_lsp_client, read_framed_message, test_extensions,
        write_response,
    };

    impl DiskSweep {
        /// Waits for every running job and forgets it.
        async fn settle(&mut self) {
            while let Some(finished) = self.jobs.join_next_with_id().await {
                match finished {
                    Ok((id, ())) => self.running.remove(&id),
                    Err(error) => self.running.remove(&error.id()),
                };
            }
        }

        async fn tick_and_settle(&mut self, cancel: &CancellationToken) {
            self.tick(cancel).await;
            self.settle().await;
        }
    }

    /// Answers the first `answers` pull requests with a report that differs
    /// every time and counts all of them; later ones stay unanswered.
    fn spawn_responder(
        mut server: FakeServer,
        pulls: Arc<AtomicUsize>,
        answers: usize,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut wire = BufReader::new(&mut server.write_stdout);
            loop {
                let message = read_framed_message(&mut wire).await;
                if message["method"] != "textDocument/diagnostic" {
                    continue;
                }
                let n = pulls.fetch_add(1, Ordering::SeqCst) + 1;
                if n > answers {
                    continue;
                }
                let report = json!({"kind": "full", "items": [{
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                    "severity": 1,
                    "message": format!("pull {n}"),
                }]});
                write_response(&mut server.read_half_stdin, &message["id"], report).await;
            }
        })
    }

    struct Harness {
        _dir: TempDir,
        translator: Arc<Translator>,
        cache: Arc<Mutex<NotificationCache>>,
        subs: SubscriptionRegistry,
        path: PathBuf,
        resource: DiagnosticsResourceUri,
        pulls: Arc<AtomicUsize>,
        sweep: DiskSweep,
        cancel: CancellationToken,
        _responder: tokio::task::JoinHandle<()>,
    }

    impl Harness {
        /// A tracked file whose first pull was answered, with `answers` pull
        /// requests answered in all.
        async fn new(answers: usize) -> Self {
            let dir = TempDir::new().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            let path = root.join("lib.rs");
            fs::write(&path, "fn main() {}").unwrap();
            let server_id = ServerId::from_static("rust");
            let cache = Arc::new(Mutex::new(NotificationCache::new()));
            let subs = SubscriptionRegistry::new();
            let mut translator = Translator::new()
                .with_extensions(test_extensions())
                .with_router(ToolRouter::catch_all([(
                    server_id.clone(),
                    LanguageId::from_static("rust"),
                )]))
                .with_notification_cache(Arc::clone(&cache));
            let roots = WorkspaceRoots::from_paths(&[root]).unwrap();
            translator.set_workspace_roots(roots.clone());
            let (client, server) = fake_lsp_client();
            translator.register_client(server_id, client);
            let cancel = CancellationToken::new();
            translator.install_wiring(Arc::new(PumpWiring::new(
                PumpShared {
                    roles: DiagnosticsRoles::default(),
                    notification_cache: Arc::clone(&cache),
                    subs: subs.clone(),
                    workspace_roots: roots,
                },
                cancel.clone(),
            )));
            let translator = Arc::new(translator);
            cache
                .lock()
                .await
                .attach_documents(Arc::clone(translator.document_tracker()));
            let pulls = Arc::new(AtomicUsize::new(0));
            let responder = spawn_responder(server, Arc::clone(&pulls), answers);
            let resource =
                DiagnosticsResourceUri::for_canonical_file(&path_to_uri(&path).unwrap()).unwrap();
            let sweep = DiskSweep::new(Arc::clone(&translator), Arc::clone(&cache), subs.clone());
            let harness = Self {
                _dir: dir,
                translator,
                cache,
                subs,
                path,
                resource,
                pulls,
                sweep,
                cancel,
                _responder: responder,
            };
            harness.pull_by_tool().await;
            harness
        }

        /// What a `get_diagnostics` call does: tracks the file and pulls it.
        async fn pull_by_tool(&self) {
            self.translator
                .handle_diagnostics(client_path(&self.path), ResultContext::None, &self.cache)
                .await
                .unwrap();
        }

        async fn subscribe(&self) -> (SessionHandle, mpsc::Receiver<String>) {
            let session = SessionHandle::new(self.subs.clone());
            let (tx, rx) = mpsc::channel(16);
            session
                .subscribe_for_test(&self.resource, Target::Channel(tx))
                .await
                .unwrap();
            (session, rx)
        }

        fn edit(&self, content: &str) {
            fs::write(&self.path, content).unwrap();
        }

        async fn tick(&mut self) {
            self.sweep.tick_and_settle(&self.cancel).await;
        }

        fn pulls(&self) -> usize {
            self.pulls.load(Ordering::SeqCst)
        }
    }

    async fn next_update(rx: &mut mpsc::Receiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("no resources/updated within 5 s")
            .expect("delivery channel closed")
    }

    /// Lets the delivery task run, then checks nothing is queued.
    async fn assert_quiet(rx: &mut mpsc::Receiver<String>) {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    }

    /// Waits until the responder has seen `n` pull requests.
    async fn pulls_reach(pulls: &AtomicUsize, n: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while pulls.load(Ordering::SeqCst) < n {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the pull request never arrived");
    }

    #[tokio::test]
    async fn test_an_edit_before_the_first_tick_notifies_the_subscriber_once() {
        let mut h = Harness::new(usize::MAX).await;
        let (_session, mut rx) = h.subscribe().await;
        h.edit("fn main() { let x: i32 = \"s\"; }");

        h.tick().await;

        assert_eq!(next_update(&mut rx).await, h.resource.as_str());
        assert_quiet(&mut rx).await;
        assert_eq!(h.pulls(), 2);
    }

    #[tokio::test]
    async fn test_idle_ticks_pull_and_notify_nothing() {
        let mut h = Harness::new(usize::MAX).await;
        let (_session, mut rx) = h.subscribe().await;
        h.tick().await;
        let settled = h.pulls();
        assert_quiet(&mut rx).await;
        while rx.try_recv().is_ok() {}

        for _ in 0..100 {
            h.tick().await;
        }

        assert_eq!(h.pulls(), settled);
        assert_quiet(&mut rx).await;
    }

    #[tokio::test]
    async fn test_an_edit_made_while_nobody_subscribed_notifies_the_next_subscriber() {
        let mut h = Harness::new(usize::MAX).await;
        let (session, rx) = h.subscribe().await;
        drop((session, rx));
        h.edit("fn main() { let x: i32 = \"s\"; }");
        h.tick().await;
        assert_eq!(h.pulls(), 1);

        let (_session, mut rx) = h.subscribe().await;
        h.tick().await;

        assert_eq!(next_update(&mut rx).await, h.resource.as_str());
        assert_quiet(&mut rx).await;
    }

    #[tokio::test]
    async fn test_a_wedged_server_keeps_one_job_and_gets_no_second() {
        let mut h = Harness::new(1).await;
        let (_session, _rx) = h.subscribe().await;
        h.edit("fn main() { let x: i32 = \"s\"; }");

        h.sweep.tick(&h.cancel).await;
        pulls_reach(&h.pulls, 2).await;
        h.sweep.tick(&h.cancel).await;
        h.sweep.tick(&h.cancel).await;

        assert_eq!(h.sweep.running.len(), 1);
        assert_eq!(h.pulls(), 2);
    }

    /// A server that never answers stalls its own files only.
    #[tokio::test]
    async fn test_a_wedged_server_does_not_stall_the_files_of_another() {
        let dir = TempDir::new().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let (wedged_file, healthy_file) = (root.join("lib.rs"), root.join("app.tsx"));
        fs::write(&wedged_file, "fn main() {}").unwrap();
        fs::write(&healthy_file, "export {};").unwrap();
        let (wedged_id, healthy_id) = (ServerId::from_static("rust"), ServerId::from_static("ts"));
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let subs = SubscriptionRegistry::new();
        let mut translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(ToolRouter::catch_all([
                (wedged_id.clone(), LanguageId::from_static("rust")),
                (
                    healthy_id.clone(),
                    LanguageId::from_static("typescriptreact"),
                ),
            ]));
        let roots = WorkspaceRoots::from_paths(&[root]).unwrap();
        translator.set_workspace_roots(roots.clone());
        let (wedged_client, wedged_server) = fake_lsp_client();
        let (healthy_client, healthy_server) = fake_lsp_client();
        translator.register_client(wedged_id.clone(), wedged_client);
        translator.register_client(healthy_id, healthy_client);
        let cancel = CancellationToken::new();
        translator.install_wiring(Arc::new(PumpWiring::new(
            PumpShared {
                roles: DiagnosticsRoles::default(),
                notification_cache: Arc::clone(&cache),
                subs: subs.clone(),
                workspace_roots: roots,
            },
            cancel.clone(),
        )));
        let translator = Arc::new(translator);
        cache
            .lock()
            .await
            .attach_documents(Arc::clone(translator.document_tracker()));
        let (wedged_pulls, healthy_pulls) =
            (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let _wedged = spawn_responder(wedged_server, Arc::clone(&wedged_pulls), 1);
        let _healthy = spawn_responder(healthy_server, Arc::clone(&healthy_pulls), usize::MAX);
        let session = SessionHandle::new(subs.clone());
        let (tx, mut rx) = mpsc::channel(16);
        for file in [&wedged_file, &healthy_file] {
            translator
                .handle_diagnostics(client_path(file), ResultContext::None, &cache)
                .await
                .unwrap();
            let resource =
                DiagnosticsResourceUri::for_canonical_file(&path_to_uri(file).unwrap()).unwrap();
            session
                .subscribe_for_test(&resource, Target::Channel(tx.clone()))
                .await
                .unwrap();
        }
        let healthy_resource =
            DiagnosticsResourceUri::for_canonical_file(&path_to_uri(&healthy_file).unwrap())
                .unwrap();
        while rx.try_recv().is_ok() {}
        for file in [&wedged_file, &healthy_file] {
            fs::write(file, "// edited so the length differs").unwrap();
        }
        let mut sweep = DiskSweep::new(Arc::clone(&translator), Arc::clone(&cache), subs);

        sweep.tick(&cancel).await;
        pulls_reach(&wedged_pulls, 2).await;
        pulls_reach(&healthy_pulls, 2).await;
        assert_eq!(next_update(&mut rx).await, healthy_resource.as_str());
        sweep.tick(&cancel).await;
        assert_quiet(&mut rx).await;
        assert_eq!(wedged_pulls.load(Ordering::SeqCst), 2);
        assert_eq!(healthy_pulls.load(Ordering::SeqCst), 2);
        let wedged_jobs = sweep.running.values().filter(|id| **id == wedged_id);
        assert_eq!(wedged_jobs.count(), 1);
    }

    #[tokio::test]
    async fn test_a_cancelled_job_sweeps_nothing() {
        let h = Harness::new(usize::MAX).await;
        let (_session, _rx) = h.subscribe().await;
        h.edit("fn main() { let x: i32 = \"s\"; }");
        let cancel = CancellationToken::new();
        cancel.cancel();

        sweep_files(
            Arc::clone(&h.translator),
            Arc::clone(&h.cache),
            SweepMemo::default(),
            cancel,
            vec![h.path.clone()],
        )
        .await;

        assert_eq!(h.pulls(), 1);
    }

    fn sweep_over_two_servers() -> (DiskSweep, ServerId, ServerId) {
        let (rust, ts) = (ServerId::from_static("rust"), ServerId::from_static("ts"));
        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(ToolRouter::catch_all([
                (rust.clone(), LanguageId::from_static("rust")),
                (ts.clone(), LanguageId::from_static("typescriptreact")),
            ]));
        translator.register_client(rust.clone(), fake_lsp_client().0);
        translator.register_client(ts.clone(), fake_lsp_client().0);
        let sweep = DiskSweep::new(
            Arc::new(translator),
            Arc::new(Mutex::new(NotificationCache::new())),
            SubscriptionRegistry::new(),
        );
        (sweep, rust, ts)
    }

    fn files(prefix: &str, extension: &str, count: usize) -> Vec<PathBuf> {
        (0..count)
            .map(|i| PathBuf::from(format!("/w/{prefix}{i:03}.{extension}")))
            .collect()
    }

    #[tokio::test]
    async fn test_windows_cover_a_backlog_in_turn() {
        let (mut sweep, ..) = sweep_over_two_servers();
        let files = files("f", "rs", 100);

        let first = sweep.next_window(&files);
        let second = sweep.next_window(&files);

        assert_eq!(first.len(), MAX_SWEPT_FILES_PER_TICK);
        assert_eq!(second.len(), MAX_SWEPT_FILES_PER_TICK);
        let covered: HashSet<&PathBuf> = first.iter().chain(&second).map(|(p, _)| p).collect();
        assert_eq!(covered.len(), files.len());
    }

    /// Files of a server whose job still runs do not use up the window.
    #[tokio::test]
    async fn test_a_busy_server_does_not_starve_the_files_of_another() {
        let (mut sweep, _rust, ts) = sweep_over_two_servers();
        let busy = sweep.jobs.spawn(std::future::pending()).id();
        sweep.running.insert(busy, ts);
        let mut all = files("a", "tsx", 80);
        all.extend(files("b", "rs", 70));
        all.sort();

        let window = sweep.next_window(&all);

        assert_eq!(window.len(), MAX_SWEPT_FILES_PER_TICK);
        assert!(
            window
                .iter()
                .all(|(path, _)| path.extension().unwrap() == "rs")
        );
    }
}
