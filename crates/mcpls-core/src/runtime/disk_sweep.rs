//! Tells `lsp-diagnostics://` subscribers about on-disk edits no tool call
//! observes (#648, #781, spec bridge/009 and bridge/016).
//!
//! While some session subscribes to a tracked file, the sweep resyncs the file
//! with the disk every [`SWEEP_INTERVAL`] and, when that moved the document to
//! a state the cache holds no pulled report for, pulls it, which publishes the
//! change like a `get_diagnostics` call would. A resync that found the content
//! changed also sends `didSave` to a server that advertises it, so diagnostics
//! a server produces only on save are refreshed.
//!
//! Tracked files nobody subscribes to, served by a server that has a
//! subscribed file, are visited as companions: resynced (and `didSave`d) but
//! never pulled, without touching their LRU position. A change to one advances
//! its server's epoch, which makes that server's subscribed files due for a
//! pull, so cross-file staleness is caught.
//!
//! Both sets are visited through bounded windows ([`MAX_SWEPT_FILES_PER_TICK`],
//! [`MAX_COMPANION_FILES`], [`MAX_COMPANION_FILES_PER_TICK`]), subscribed files
//! first. Files of one server are swept by one job at a time, and a pull that
//! does not answer leaves the job (see `SweepContext`), so a wedged server
//! stalls only its own files and an unanswered pull only its own file.
//!
//! Not covered: servers that register `textDocument/didSave` dynamically.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::task::{Id, JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::warn;

use crate::bridge::resources::DiagnosticsResourceUri;
use crate::bridge::{
    Interest, MAX_COMPANION_FILES, MAX_SWEPT_FILES_PER_TICK, NotificationCache, SWEEP_INTERVAL,
    SweepContext, SweepMemo, Translator, path_to_uri,
};
use crate::config::ServerId;
use crate::mcp::SubscriptionRegistry;

/// Most companions one sweep visits; the rest follow on later ticks.
const MAX_COMPANION_FILES_PER_TICK: usize = 32;

/// The server the sweep would use for each tracked file, resolved once per
/// tick; a file no running server routes is left out.
type ServerMap = HashMap<PathBuf, ServerId>;

/// A running sweep: the task that ticks and the token that stops it.
#[derive(Debug)]
pub struct DiskSweepHandle {
    stop: CancellationToken,
    task: JoinHandle<()>,
}

/// Starts the sweep, which runs until `cancel` fires or the handle is
/// stopped with [`stop_disk_sweep`].
///
/// The task owns the per-server jobs and the pulls they left in the
/// background: it ends only after both are gone.
pub fn spawn_disk_sweep(
    translator: &Arc<Translator>,
    notification_cache: &Arc<Mutex<NotificationCache>>,
    subscriptions: &SubscriptionRegistry,
    cancel: &CancellationToken,
) -> DiskSweepHandle {
    let stop = cancel.child_token();
    let mut sweep = DiskSweep::new(
        Arc::clone(translator),
        Arc::clone(notification_cache),
        subscriptions.clone(),
        stop.clone(),
    );
    let ticker_stop = stop.clone();
    let task = tokio::spawn(async move {
        let mut ticks = tokio::time::interval(SWEEP_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = ticker_stop.cancelled() => break,
                _ = ticks.tick() => sweep.tick().await,
            }
        }
        sweep.shutdown().await;
    });
    DiskSweepHandle { stop, task }
}

/// Stops the sweep started by [`spawn_disk_sweep`] and waits until its task is
/// gone, so no job or pull touches a server that shutdown is about to stop.
///
/// A job stopped inside `ensure_open` leaves nothing behind: a document whose
/// resync did not finish is resynced by the next call that needs it.
pub async fn stop_disk_sweep(handle: DiskSweepHandle) {
    handle.stop.cancel();
    match handle.task.await {
        Err(error) if error.is_panic() => warn!("disk sweep panicked: {error}"),
        Ok(()) | Err(_) => {}
    }
}

struct DiskSweep {
    translator: Arc<Translator>,
    subscriptions: SubscriptionRegistry,
    context: SweepContext,
    jobs: JoinSet<()>,
    /// Server each unfinished job sweeps.
    running: HashMap<Id, ServerId>,
    subscribed: SweepWindow,
    companions: SweepWindow,
}

impl DiskSweep {
    fn new(
        translator: Arc<Translator>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        subscriptions: SubscriptionRegistry,
        stop: CancellationToken,
    ) -> Self {
        let context = SweepContext::new(
            Arc::clone(&translator),
            notification_cache,
            SweepMemo::default(),
            TaskTracker::new(),
            stop,
        );
        Self {
            translator,
            subscriptions,
            context,
            jobs: JoinSet::new(),
            running: HashMap::new(),
            subscribed: SweepWindow::new(MAX_SWEPT_FILES_PER_TICK),
            companions: SweepWindow::new(MAX_COMPANION_FILES_PER_TICK),
        }
    }

    async fn tick(&mut self) {
        self.reap_finished();
        self.context.memo().begin_tick();
        if !self.subscriptions.any_subscription().await {
            self.context.memo().retain(|_| false);
            return;
        }
        let tracked = self.translator.document_tracker().open_paths();
        let servers = self.server_map(&tracked);
        let subscribed = self.subscribed_files(&tracked, &servers).await;
        let companions = self.companion_files(&subscribed, &servers);
        let kept: HashSet<&Path> = subscribed
            .iter()
            .chain(&companions)
            .map(|(path, _)| path.as_path())
            .collect();
        self.context.memo().retain(|path| kept.contains(path));

        let mut by_server: HashMap<ServerId, Vec<(PathBuf, Interest)>> = HashMap::new();
        let busy = self.busy_servers();
        let window = self
            .subscribed
            .advance(&subscribed, |server| busy.contains(server));
        for (path, server) in window {
            by_server
                .entry(server)
                .or_default()
                .push((path, Interest::Subscribed));
        }
        let window = self
            .companions
            .advance(&companions, |server| busy.contains(server));
        for (path, server) in window {
            by_server
                .entry(server)
                .or_default()
                .push((path, Interest::Companion));
        }
        for (server, visits) in by_server {
            let context = self.context.clone();
            let id = self
                .jobs
                .spawn(async move { context.run_job(visits).await })
                .id();
            self.running.insert(id, server);
        }
    }

    /// Ends every job and waits for the pulls they left in the background.
    async fn shutdown(&mut self) {
        self.jobs.shutdown().await;
        self.running.clear();
        self.context.pulls().close();
        self.context.pulls().wait().await;
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

    /// The server of each of `paths` that a running server routes.
    fn server_map(&self, paths: &[PathBuf]) -> ServerMap {
        paths
            .iter()
            .filter_map(|path| Some((path.clone(), self.translator.sweep_server(path)?)))
            .collect()
    }

    /// Tracked files some session or listen stream subscribes to, with their
    /// servers, sorted by path.
    async fn subscribed_files(
        &self,
        tracked: &[PathBuf],
        servers: &ServerMap,
    ) -> Vec<(PathBuf, ServerId)> {
        let mut tracked = tracked.to_vec();
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
        self.subscriptions
            .retain_subscribed(resources)
            .await
            .into_iter()
            .filter_map(|path| {
                let server = servers.get(&path)?.clone();
                Some((path, server))
            })
            .collect()
    }

    /// The servers whose job still runs.
    fn busy_servers(&self) -> HashSet<ServerId> {
        self.running.values().cloned().collect()
    }

    /// The most recently used [`MAX_COMPANION_FILES`] tracked files that are
    /// not subscribed but are routed to a server with a subscribed file,
    /// sorted by path.
    fn companion_files(
        &self,
        subscribed: &[(PathBuf, ServerId)],
        servers: &ServerMap,
    ) -> Vec<(PathBuf, ServerId)> {
        let with_subscription: HashSet<&ServerId> =
            subscribed.iter().map(|(_, server)| server).collect();
        let subscribed: HashSet<&PathBuf> = subscribed.iter().map(|(path, _)| path).collect();
        let mut companions: Vec<(PathBuf, ServerId)> = self
            .translator
            .document_tracker()
            .open_paths_by_recency()
            .into_iter()
            .filter(|path| !subscribed.contains(path))
            .filter_map(|path| {
                let server = servers.get(&path)?;
                with_subscription
                    .contains(server)
                    .then(|| (path, server.clone()))
            })
            .take(MAX_COMPANION_FILES)
            .collect();
        companions.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        companions
    }
}

/// A bounded view of a path-sorted set that moves round it tick by tick.
#[derive(Debug)]
struct SweepWindow {
    /// The last path the previous tick examined, so the next tick resumes at
    /// the first path after it however the set changed.
    after: Option<PathBuf>,
    /// Most items one tick takes.
    max: usize,
}

impl SweepWindow {
    const fn new(max: usize) -> Self {
        Self { after: None, max }
    }

    /// At most `max` of `items`, sorted by path, taken from the first path
    /// after the cursor and round again; the cursor becomes the last path
    /// examined. Items of a server `busy` says is occupied are passed over
    /// without using up the window.
    fn advance(
        &mut self,
        items: &[(PathBuf, ServerId)],
        busy: impl Fn(&ServerId) -> bool,
    ) -> Vec<(PathBuf, ServerId)> {
        let len = items.len();
        let start = self
            .after
            .as_ref()
            .map_or(0, |after| items.partition_point(|(path, _)| path <= after));
        let mut window = Vec::new();
        let mut last = None;
        for (path, server) in items
            .iter()
            .cycle()
            .skip(start.checked_rem(len).unwrap_or(0))
            .take(len)
        {
            if window.len() >= self.max {
                break;
            }
            last = Some(path);
            if busy(server) {
                continue;
            }
            window.push((path.clone(), server.clone()));
        }
        self.after = last.cloned();
        window
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use std::{assert_matches, fs};

    use serde_json::json;
    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::sync::mpsc;

    use super::*;
    use crate::bridge::{
        MAX_FAILED_PULLS_PER_STATE, ResourceLimits, ResultContext, WorkspaceRoots,
    };
    use crate::config::{DocumentLimit, LanguageId, ToolRouter};
    use crate::mcp::{SessionHandle, Target};
    use crate::runtime::pump::{DiagnosticsRoles, PumpShared, PumpWiring};
    use crate::test_lsp::{
        FakeServer, client_path, fake_lsp_client, next_framed_message, read_framed_message,
        test_extensions, write_response,
    };

    impl DiskSweep {
        fn next_subscribed(
            &mut self,
            subscribed: &[(PathBuf, ServerId)],
        ) -> Vec<(PathBuf, ServerId)> {
            let busy = self.busy_servers();
            self.subscribed
                .advance(subscribed, |server| busy.contains(server))
        }

        fn next_companions(
            &mut self,
            companions: &[(PathBuf, ServerId)],
        ) -> Vec<(PathBuf, ServerId)> {
            let busy = self.busy_servers();
            self.companions
                .advance(companions, |server| busy.contains(server))
        }

        /// Waits for every running job and every pull pass that answers.
        async fn settle(&mut self) {
            while let Some(finished) = self.jobs.join_next_with_id().await {
                match finished {
                    Ok((id, ())) => self.running.remove(&id),
                    Err(error) => self.running.remove(&error.id()),
                };
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while !self.context.pulls().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the pulls did not finish");
        }

        async fn tick_and_settle(&mut self) {
            self.tick().await;
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
        _responder: tokio::task::JoinHandle<()>,
    }

    impl Harness {
        /// A tracked file whose first pull was answered, with `answers` pull
        /// requests answered in all.
        async fn new(answers: usize) -> Self {
            Self::with_limits(answers, ResourceLimits::default()).await
        }

        async fn with_limits(answers: usize, limits: ResourceLimits) -> Self {
            let dir = TempDir::new().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            let path = root.join("lib.rs");
            fs::write(&path, "fn main() {}").unwrap();
            let server_id = ServerId::from_static("rust");
            let cache = Arc::new(Mutex::new(NotificationCache::new()));
            let subs = SubscriptionRegistry::new();
            let mut translator = Translator::new()
                .with_resource_limits(limits)
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
            let sweep = DiskSweep::new(
                Arc::clone(&translator),
                Arc::clone(&cache),
                subs.clone(),
                cancel.clone(),
            );
            let harness = Self {
                _dir: dir,
                translator,
                cache,
                subs,
                path,
                resource,
                pulls,
                sweep,
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
            self.sweep.tick_and_settle().await;
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

        h.sweep.tick().await;
        pulls_reach(&h.pulls, 2).await;
        h.sweep.tick().await;
        h.sweep.tick().await;

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
        let mut sweep = DiskSweep::new(
            Arc::clone(&translator),
            Arc::clone(&cache),
            subs,
            cancel.clone(),
        );

        sweep.tick().await;
        pulls_reach(&wedged_pulls, 2).await;
        pulls_reach(&healthy_pulls, 2).await;
        assert_eq!(next_update(&mut rx).await, healthy_resource.as_str());
        sweep.tick().await;
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

        SweepContext::new(
            Arc::clone(&h.translator),
            Arc::clone(&h.cache),
            SweepMemo::default(),
            TaskTracker::new(),
            cancel,
        )
        .run(vec![(h.path.clone(), Interest::Subscribed)])
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
            CancellationToken::new(),
        );
        (sweep, rust, ts)
    }

    /// `paths` with the servers `sweep` routes them to.
    fn served(sweep: &DiskSweep, paths: &[PathBuf]) -> Vec<(PathBuf, ServerId)> {
        let servers = sweep.server_map(paths);
        paths
            .iter()
            .filter_map(|path| Some((path.clone(), servers.get(path)?.clone())))
            .collect()
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

        let files = served(&sweep, &files);
        let first = sweep.next_subscribed(&files);
        let second = sweep.next_subscribed(&files);

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

        let all = served(&sweep, &all);
        let window = sweep.next_subscribed(&all);

        assert_eq!(window.len(), MAX_SWEPT_FILES_PER_TICK);
        assert!(
            window
                .iter()
                .all(|(path, _)| path.extension().unwrap() == "rs")
        );
    }

    fn unlimited_documents() -> ResourceLimits {
        ResourceLimits {
            max_documents: DocumentLimit::UNLIMITED,
            ..ResourceLimits::default()
        }
    }

    impl Harness {
        /// Tracks the text of a new file next to the harness's, which no
        /// session subscribes to.
        fn track(&self, name: &str) -> PathBuf {
            let path = self.path.with_file_name(name);
            fs::write(&path, "fn other() {}").unwrap();
            self.translator
                .document_tracker()
                .open(path.clone(), "fn other() {}".to_owned())
                .unwrap();
            path
        }
    }

    /// With far more tracked files than the windows hold, a subscribed file's
    /// own edit is pulled on the next tick: subscribed files go first.
    #[tokio::test]
    async fn test_a_subscribed_edit_is_pulled_on_the_next_tick_among_300_tracked_files() {
        let mut h = Harness::with_limits(usize::MAX, unlimited_documents()).await;
        for n in 0..300 {
            h.track(&format!("f{n:03}.rs"));
        }
        let (_session, mut rx) = h.subscribe().await;
        h.tick().await;
        while rx.try_recv().is_ok() {}
        let settled = h.pulls();
        h.edit("fn main() { let x: i32 = \"s\"; }");

        h.tick().await;

        assert_eq!(h.pulls(), settled + 1);
        assert_eq!(next_update(&mut rx).await, h.resource.as_str());
    }

    /// An edit to a tracked file nobody subscribes to refreshes the
    /// subscribed file of the same server once, and idle ticks add nothing.
    #[tokio::test]
    async fn test_a_companion_edit_refreshes_the_subscribed_file_once() {
        let mut h = Harness::new(usize::MAX).await;
        let other = h.track("other.rs");
        let (_session, mut rx) = h.subscribe().await;
        h.tick().await;
        while rx.try_recv().is_ok() {}
        let settled = h.pulls();
        fs::write(&other, "fn other() { let y = 2; }").unwrap();

        h.tick().await;
        h.tick().await;

        assert_eq!(h.pulls(), settled + 1);
        assert_eq!(next_update(&mut rx).await, h.resource.as_str());
        for _ in 0..100 {
            h.tick().await;
        }
        assert_eq!(h.pulls(), settled + 1);
        assert_quiet(&mut rx).await;
    }

    /// A tool call that resyncs a companion counts like the sweep's own
    /// resync: the next tick refreshes the subscribed file.
    #[tokio::test]
    async fn test_a_tool_call_resync_of_a_companion_refreshes_the_subscribed_file() {
        let mut h = Harness::new(usize::MAX).await;
        let other = h.track("other.rs");
        let (_session, mut rx) = h.subscribe().await;
        h.tick().await;
        while rx.try_recv().is_ok() {}
        fs::write(&other, "fn other() { let y = 2; }").unwrap();
        h.translator
            .handle_diagnostics(client_path(&other), ResultContext::None, &h.cache)
            .await
            .unwrap();
        let after_tool_call = h.pulls();

        h.tick().await;
        h.tick().await;

        assert_eq!(h.pulls(), after_tool_call + 1);
    }

    /// The memo holds subscribed and companion files only, whatever the
    /// tracker holds.
    #[tokio::test]
    async fn test_the_memo_is_bounded_by_the_subscribed_and_companion_files() {
        let mut h = Harness::with_limits(usize::MAX, unlimited_documents()).await;
        for n in 0..300 {
            h.track(&format!("f{n:03}.rs"));
        }
        let (_session, _rx) = h.subscribe().await;

        for _ in 0..20 {
            h.tick().await;
        }

        assert!(h.sweep.context.memo().tracked_files() <= 1 + 2 * MAX_COMPANION_FILES);
    }

    fn companions(prefix: &str, count: usize) -> Vec<(PathBuf, ServerId)> {
        files(prefix, "rs", count)
            .into_iter()
            .map(|path| (path, ServerId::from_static("rust")))
            .collect()
    }

    /// The companion window resumes after the last path it examined, so a
    /// path inserted before the cursor does not make later ones repeat or skip.
    #[tokio::test]
    async fn test_companion_windows_resume_after_the_last_visited_path() {
        let (mut sweep, ..) = sweep_over_two_servers();
        let mut all = companions("f", 100);

        let first = sweep.next_companions(&all);
        all.insert(
            0,
            (
                PathBuf::from("/w/a-new-file.rs"),
                ServerId::from_static("rust"),
            ),
        );
        let second = sweep.next_companions(&all);

        assert_eq!(first.len(), MAX_COMPANION_FILES_PER_TICK);
        assert_eq!(first[0].0, PathBuf::from("/w/f000.rs"));
        assert_eq!(second[0].0, PathBuf::from("/w/f032.rs"));
        assert_eq!(second.len(), MAX_COMPANION_FILES_PER_TICK);
    }

    /// Windows follow one another round the whole set.
    #[tokio::test]
    async fn test_companion_windows_cover_the_set_in_turn() {
        let (mut sweep, ..) = sweep_over_two_servers();
        let all = companions("f", 100);

        let covered: HashSet<PathBuf> = (0..4)
            .flat_map(|_| sweep.next_companions(&all))
            .map(|(path, _)| path)
            .collect();

        assert_eq!(covered.len(), 100);
    }

    /// Companions of a server whose job still runs do not use up the window.
    #[tokio::test]
    async fn test_a_busy_server_does_not_starve_the_companions_of_another() {
        let (mut sweep, _rust, ts) = sweep_over_two_servers();
        let busy = sweep.jobs.spawn(std::future::pending()).id();
        sweep.running.insert(busy, ts.clone());
        let mut all = companions("b", 70);
        all.extend(
            files("a", "tsx", 80)
                .into_iter()
                .map(|path| (path, ts.clone())),
        );
        all.sort_by(|(a, _), (b, _)| a.cmp(b));

        let window = sweep.next_companions(&all);

        assert_eq!(window.len(), MAX_COMPANION_FILES_PER_TICK);
        assert!(window.iter().all(|(_, server)| *server != ts));
    }

    /// Only the most recently used tracked files of a server with a
    /// subscribed file are companions, in path order.
    #[tokio::test(start_paused = true)]
    async fn test_companions_are_the_most_recent_files_of_a_served_server() {
        let (rust, ts) = (ServerId::from_static("rust"), ServerId::from_static("ts"));
        let translator = Translator::new()
            .with_resource_limits(unlimited_documents())
            .with_extensions(test_extensions())
            .with_router(ToolRouter::catch_all([
                (rust.clone(), LanguageId::from_static("rust")),
                (ts.clone(), LanguageId::from_static("typescriptreact")),
            ]));
        translator.register_client(rust.clone(), fake_lsp_client().0);
        translator.register_client(ts, fake_lsp_client().0);
        let tracker = Arc::clone(translator.document_tracker());
        let all = files("f", "rs", MAX_COMPANION_FILES + 44);
        for path in &all {
            tracker.open(path.clone(), String::new()).unwrap();
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        for path in files("t", "tsx", 10) {
            tracker.open(path, String::new()).unwrap();
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        let sweep = DiskSweep::new(
            Arc::new(translator),
            Arc::new(Mutex::new(NotificationCache::new())),
            SubscriptionRegistry::new(),
            CancellationToken::new(),
        );
        let open = sweep.translator.document_tracker().open_paths();
        let servers = sweep.server_map(&open);
        let subscribed = served(&sweep, &[all[0].clone()]);

        let found = sweep.companion_files(&subscribed, &servers);

        let expected: Vec<&PathBuf> = all[44..].iter().collect();
        let found_paths: Vec<&PathBuf> = found.iter().map(|(path, _)| path).collect();
        assert_eq!(found_paths, expected);
        assert!(found.iter().all(|(_, server)| *server == rust));
    }

    /// A request or notification the scripted server received.
    #[derive(Debug, Clone)]
    struct Seen {
        at: tokio::time::Instant,
        method: String,
        file: String,
    }

    /// What the scripted server leaves unanswered, and what it received.
    #[derive(Debug, Clone, Default)]
    struct Wedge {
        /// How long the server takes to answer a pull.
        delay: Arc<std::sync::Mutex<Duration>>,
        hung: Arc<std::sync::Mutex<HashSet<String>>>,
        seen: Arc<std::sync::Mutex<Vec<Seen>>>,
    }

    impl Wedge {
        /// Leaves the pulls of `file` unanswered from now on.
        fn hang(&self, file: &str) {
            self.hung.lock().unwrap().insert(file.to_owned());
        }

        fn seen(&self, method: &str, file: &str) -> Vec<Seen> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|seen| seen.method == method && seen.file == file)
                .cloned()
                .collect()
        }

        fn count(&self, method: &str, file: &str) -> usize {
            self.seen(method, file).len()
        }
    }

    const PULL: &str = "textDocument/diagnostic";
    const CHANGE: &str = "textDocument/didChange";

    /// Answers every pull that `wedge` does not hang with a report that
    /// differs every time, and records everything it receives.
    fn spawn_wedging_responder(server: FakeServer, wedge: Wedge) -> JoinHandle<()> {
        let FakeServer {
            read_half_stdin,
            mut write_stdout,
        } = server;
        let writer = Arc::new(Mutex::new(read_half_stdin));
        tokio::spawn(async move {
            let mut wire = BufReader::new(&mut write_stdout);
            let mut answered = 0_u32;
            loop {
                let message = next_framed_message(&mut wire).await;
                let Some(method) = message["method"].as_str() else {
                    continue;
                };
                let file = message["params"]["textDocument"]["uri"]
                    .as_str()
                    .and_then(|uri| uri.rsplit('/').next())
                    .unwrap_or_default()
                    .to_owned();
                wedge.seen.lock().unwrap().push(Seen {
                    at: tokio::time::Instant::now(),
                    method: method.to_owned(),
                    file: file.clone(),
                });
                if method != PULL || wedge.hung.lock().unwrap().contains(&file) {
                    continue;
                }
                answered += 1;
                let delay = *wedge.delay.lock().unwrap();
                let report = json!({"kind": "full", "items": [{
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                    "severity": 1,
                    "message": format!("pull {answered}"),
                }]});
                if delay.is_zero() {
                    write_response(&mut *writer.lock().await, &message["id"], report).await;
                    continue;
                }
                let (writer, id) = (Arc::clone(&writer), message["id"].clone());
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    write_response(&mut *writer.lock().await, &id, report).await;
                });
            }
        })
    }

    /// Several tracked files of one server, some subscribed and some not.
    struct Fleet {
        _tmp: TempDir,
        dir: PathBuf,
        sweep: DiskSweep,
        stop: CancellationToken,
        wedge: Wedge,
        request_timeout: Duration,
        updates: mpsc::Receiver<String>,
        edits: usize,
        _session: SessionHandle,
        _responder: JoinHandle<()>,
    }

    impl Fleet {
        /// Tracks and pulls every file once, with all pulls answered, and
        /// subscribes to `subscribed`.
        async fn new(subscribed: &[&str], companions: &[&str]) -> Self {
            let tmp = TempDir::new().unwrap();
            let root = dunce::canonicalize(tmp.path()).unwrap();
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
            let roots = WorkspaceRoots::from_paths(std::slice::from_ref(&root)).unwrap();
            translator.set_workspace_roots(roots.clone());
            let (client, server) = fake_lsp_client();
            let request_timeout = client.request_timeout();
            translator.register_client(server_id, client);
            let stop = CancellationToken::new();
            translator.install_wiring(Arc::new(PumpWiring::new(
                PumpShared {
                    roles: DiagnosticsRoles::default(),
                    notification_cache: Arc::clone(&cache),
                    subs: subs.clone(),
                    workspace_roots: roots,
                },
                stop.clone(),
            )));
            let translator = Arc::new(translator);
            cache
                .lock()
                .await
                .attach_documents(Arc::clone(translator.document_tracker()));
            let wedge = Wedge::default();
            let responder = spawn_wedging_responder(server, wedge.clone());
            let session = SessionHandle::new(subs.clone());
            let (tx, updates) = mpsc::channel(256);
            for name in subscribed.iter().chain(companions) {
                let path = root.join(name);
                fs::write(&path, "fn f() {}").unwrap();
                translator
                    .handle_diagnostics(client_path(&path), ResultContext::None, &cache)
                    .await
                    .unwrap();
            }
            for name in subscribed {
                let uri = path_to_uri(&root.join(name)).unwrap();
                let resource = DiagnosticsResourceUri::for_canonical_file(&uri).unwrap();
                session
                    .subscribe_for_test(&resource, Target::Channel(tx.clone()))
                    .await
                    .unwrap();
            }
            let sweep = DiskSweep::new(translator, cache, subs, stop.clone());
            Self {
                _tmp: tmp,
                dir: root,
                sweep,
                stop,
                wedge,
                request_timeout,
                updates,
                edits: 0,
                _session: session,
                _responder: responder,
            }
        }

        /// Rewrites `name` with content of another length.
        fn edit(&mut self, name: &str) {
            self.edits += 1;
            fs::write(
                self.dir.join(name),
                format!("fn f() {{ {} }}", "x".repeat(self.edits)),
            )
            .unwrap();
        }

        /// Ticks every sweep interval until `done` holds, and returns how
        /// long that took.
        async fn tick_until(
            &mut self,
            within: Duration,
            done: impl Fn(&Wedge) -> bool,
        ) -> Duration {
            let start = tokio::time::Instant::now();
            loop {
                self.sweep.tick().await;
                tokio::time::sleep(SWEEP_INTERVAL).await;
                let elapsed = start.elapsed();
                if done(&self.wedge) {
                    return elapsed;
                }
                assert!(elapsed <= within, "not done within {within:?}");
            }
        }
    }

    const BOUND: Duration = Duration::from_secs(5);

    /// A server slow to answer, with more due files than background slots,
    /// still gets every file pulled: a pull cancelled for the full slots is not
    /// a failure of the file.
    #[tokio::test(start_paused = true)]
    async fn test_files_beyond_the_background_slots_are_pulled_once_slots_free() {
        let names = ["a.rs", "b.rs", "c.rs", "d.rs"];
        let mut fleet = Fleet::new(&names, &[]).await;
        fleet.sweep.tick_and_settle().await;
        *fleet.wedge.delay.lock().unwrap() = Duration::from_secs(10);
        for name in names {
            fleet.edit(name);
        }
        let mut notified = HashSet::new();

        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_secs(120) && notified.len() < names.len() {
            fleet.sweep.tick().await;
            tokio::time::sleep(SWEEP_INTERVAL).await;
            while let Ok(uri) = fleet.updates.try_recv() {
                notified.insert(uri);
            }
        }

        assert_eq!(notified.len(), names.len(), "{notified:?}");
    }

    /// An unanswered pull delays only its own file (spec 016 SC-001).
    #[tokio::test(start_paused = true)]
    async fn test_a_hung_pull_delays_only_its_own_file() {
        let mut fleet = Fleet::new(&["a.rs", "b.rs", "c.rs"], &[]).await;
        fleet.sweep.tick_and_settle().await;
        fleet.wedge.hang("a.rs");
        fleet.edit("a.rs");
        fleet.sweep.tick().await;
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let pulls_of_b = fleet.wedge.count(PULL, "b.rs");
        fleet.edit("b.rs");

        let elapsed = fleet
            .tick_until(BOUND, |wedge| {
                wedge.count(CHANGE, "b.rs") >= 1 && wedge.count(PULL, "b.rs") > pulls_of_b
            })
            .await;

        assert!(elapsed <= BOUND, "{elapsed:?}");
        assert_eq!(
            next_update(&mut fleet.updates).await,
            DiagnosticsResourceUri::for_canonical_file(
                &path_to_uri(&fleet.dir.join("b.rs")).unwrap()
            )
            .unwrap()
            .as_str()
        );
    }

    /// A file edited while its server's pass is still waiting on hung pulls is
    /// resynced at once and pulled by that pass, not a window cycle later
    /// (#809).
    #[tokio::test(start_paused = true)]
    async fn test_a_file_edited_while_the_pass_runs_is_handed_to_it() {
        let mut fleet = Fleet::new(&["a.rs", "b.rs", "c.rs"], &[]).await;
        fleet.sweep.tick_and_settle().await;
        for name in ["a.rs", "c.rs"] {
            fleet.wedge.hang(name);
            fleet.edit(name);
        }
        let pulls_of_b = fleet.wedge.count(PULL, "b.rs");
        fleet.sweep.tick().await;
        tokio::time::sleep(SWEEP_INTERVAL).await;
        fleet.edit("b.rs");

        fleet.sweep.tick().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(fleet.wedge.count(CHANGE, "b.rs"), 1);
        assert_eq!(fleet.wedge.count(PULL, "b.rs"), pulls_of_b);
        let rust = ServerId::from_static("rust");
        assert_eq!(fleet.sweep.context.memo().queued_files(&rust), 1);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(fleet.wedge.count(PULL, "b.rs"), pulls_of_b + 1);
        assert_eq!(fleet.sweep.context.memo().queued_files(&rust), 0);
    }

    /// A pull that never answers is sent the capped number of times for its
    /// state, one at a time and a request timeout apart (spec 016 SC-006).
    #[tokio::test(start_paused = true)]
    async fn test_a_hung_file_gets_the_capped_number_of_pulls_for_its_state() {
        let mut fleet = Fleet::new(&["a.rs"], &[]).await;
        fleet.sweep.tick_and_settle().await;
        fleet.wedge.hang("a.rs");
        let before = fleet.wedge.count(PULL, "a.rs");
        fleet.edit("a.rs");
        let horizon = fleet.request_timeout * 10;

        let start = tokio::time::Instant::now();
        while start.elapsed() < horizon {
            fleet.sweep.tick().await;
            tokio::time::sleep(SWEEP_INTERVAL).await;
        }

        let pulls = fleet.wedge.seen(PULL, "a.rs");
        let sent = &pulls[before..];
        assert_eq!(sent.len(), usize::from(MAX_FAILED_PULLS_PER_STATE));
        for pair in sent.windows(2) {
            assert!(
                pair[1].at.duration_since(pair[0].at) >= fleet.request_timeout,
                "a second pull was sent while the first was outstanding"
            );
        }
    }

    /// With the background slots full, the pulls after the first expiry get a
    /// grace only, each hung file is still asked once, and the resync of the
    /// next ticks is not held up.
    #[tokio::test(start_paused = true)]
    async fn test_a_full_cap_costs_a_pass_little_and_never_delays_a_resync() {
        let hung = ["h1.rs", "h2.rs", "h3.rs", "h4.rs", "h5.rs", "h6.rs"];
        let mut fleet = Fleet::new(&hung, &["d.rs"]).await;
        fleet.sweep.tick_and_settle().await;
        for name in hung {
            fleet.wedge.hang(name);
            fleet.edit(name);
        }
        let pulls_before: usize = hung.iter().map(|name| fleet.wedge.count(PULL, name)).sum();
        fleet.sweep.tick().await;
        tokio::time::sleep(Duration::from_millis(2500)).await;
        fleet.edit("d.rs");

        let elapsed = fleet
            .tick_until(BOUND, |wedge| wedge.count(CHANGE, "d.rs") >= 1)
            .await;

        assert!(elapsed <= BOUND, "{elapsed:?}");
        tokio::time::sleep(SWEEP_INTERVAL * 3).await;
        let pulls: usize = hung
            .iter()
            .map(|name| fleet.wedge.count(PULL, name))
            .sum::<usize>()
            - pulls_before;
        assert_eq!(pulls, hung.len());
    }

    /// Handing files to a running pass never makes the sweep ask a hung file
    /// again within the same pass: a pass outlasts a tick, so over many ticks
    /// a file is pulled at most once per two ticks (spec 016 NFR-002).
    #[tokio::test(start_paused = true)]
    async fn test_a_hung_file_is_pulled_at_most_once_per_pass_over_many_ticks() {
        const TICKS: usize = 10;
        let hung = ["h1.rs", "h2.rs", "h3.rs", "h4.rs", "h5.rs", "h6.rs"];
        let mut fleet = Fleet::new(&hung, &[]).await;
        fleet.sweep.tick_and_settle().await;
        for name in hung {
            fleet.wedge.hang(name);
            fleet.edit(name);
        }
        let before: Vec<usize> = hung
            .iter()
            .map(|name| fleet.wedge.count(PULL, name))
            .collect();

        for _ in 0..TICKS {
            fleet.sweep.tick().await;
            tokio::time::sleep(SWEEP_INTERVAL).await;
        }

        let pulls: Vec<usize> = hung
            .iter()
            .zip(before)
            .map(|(name, before)| fleet.wedge.count(PULL, name) - before)
            .collect();
        assert!(
            pulls.iter().all(|&n| (1..=TICKS / 2).contains(&n)),
            "pulls per hung file over {TICKS} ticks: {pulls:?}"
        );
    }

    /// Edits `hung` and `z.rs` together, hangs the pulls of `hung`, and returns
    /// how long after the edit the answering `z.rs` is pulled (spec 016
    /// FR-002, "two files hang at once").
    async fn answering_file_pulled_after(hung: &[&str]) -> Duration {
        let mut names: Vec<&str> = hung.to_vec();
        names.push("z.rs");
        let mut fleet = Fleet::new(&names, &[]).await;
        fleet.sweep.tick_and_settle().await;
        for name in hung {
            fleet.wedge.hang(name);
            fleet.edit(name);
        }
        let before = fleet.wedge.count(PULL, "z.rs");
        fleet.edit("z.rs");
        let start = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(1900)).await;
        loop {
            fleet.sweep.tick().await;
            for _ in 0..20 {
                tokio::time::sleep(SWEEP_INTERVAL / 20).await;
                if fleet.wedge.count(PULL, "z.rs") > before {
                    return start.elapsed();
                }
            }
            assert!(start.elapsed() < Duration::from_secs(60), "never pulled");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_two_hung_files_do_not_delay_an_answering_one() {
        let elapsed = Box::pin(answering_file_pulled_after(&["a1.rs", "a2.rs"])).await;
        assert!(elapsed <= BOUND, "{elapsed:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn test_three_hung_files_do_not_delay_an_answering_one() {
        let elapsed = Box::pin(answering_file_pulled_after(&["a1.rs", "a2.rs", "a3.rs"])).await;
        assert!(elapsed <= BOUND, "{elapsed:?}");
    }

    /// Files whose own state is new are pulled before one that another file's
    /// change made stale, whatever their path order.
    #[tokio::test(start_paused = true)]
    async fn test_a_pass_pulls_new_states_before_stale_ones() {
        let mut fleet = Fleet::new(&["a.rs", "b.rs", "c.rs"], &[]).await;
        fleet.sweep.tick_and_settle().await;
        let seen_before = fleet.wedge.seen.lock().unwrap().len();
        fleet.edit("a.rs");
        fleet.edit("c.rs");

        fleet.sweep.tick_and_settle().await;

        let pulled: Vec<String> = fleet.wedge.seen.lock().unwrap()[seen_before..]
            .iter()
            .filter(|seen| seen.method == PULL)
            .map(|seen| seen.file.clone())
            .collect();
        assert_eq!(pulled, ["a.rs", "c.rs", "b.rs"]);
    }

    /// Stopping the sweep ends a job that is still running before it returns.
    #[tokio::test]
    async fn test_stopping_the_sweep_ends_a_job_in_flight() {
        let (mut sweep, rust, _ts) = sweep_over_two_servers();
        let in_flight = sweep.jobs.spawn(std::future::pending()).id();
        sweep.running.insert(in_flight, rust);

        tokio::time::timeout(Duration::from_secs(5), sweep.shutdown())
            .await
            .expect("the shutdown waited for a job that never ends");

        assert!(sweep.jobs.is_empty());
        assert!(sweep.running.is_empty());
    }

    /// Stopping the sweep ends the pulls it left in the background before it
    /// returns.
    #[tokio::test(start_paused = true)]
    async fn test_stopping_the_sweep_ends_its_background_pulls() {
        let mut fleet = Fleet::new(&["a.rs"], &[]).await;
        fleet.sweep.tick_and_settle().await;
        fleet.wedge.hang("a.rs");
        fleet.edit("a.rs");
        fleet.sweep.tick().await;
        tokio::time::sleep(SWEEP_INTERVAL * 2).await;
        assert!(!fleet.sweep.context.pulls().is_empty());

        fleet.stop.cancel();
        fleet.sweep.shutdown().await;

        assert!(fleet.sweep.context.pulls().is_empty());
        assert!(fleet.sweep.running.is_empty());
    }
}
