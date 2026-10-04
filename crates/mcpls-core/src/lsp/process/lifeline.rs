//! Unix lifeline: binds one LSP server's whole process tree to mcpls.
//!
//! Per server mcpls starts two small `/bin/sh` helpers:
//!
//! - the **anchor** leads the server's process group `G` and does nothing but
//!   keep that group id reserved, so it cannot be recycled while a sweep
//!   signals `G`. It also holds the read end of the server's stderr pipe.
//! - the **watchdog** lives outside `G` and holds copies of the server's stdin
//!   and stdout pipe ends, so the server cannot learn that mcpls died before
//!   the watchdog has frozen it. It reads commands from a socketpair and, once
//!   mcpls closes its end (on any exit, including `SIGKILL`), freezes `G`,
//!   attributes processes that escaped `G` through `setsid`/`setpgid`, and
//!   kills everything it froze.
//!
//! See `specs/lsp/007-lsp-child-process-lifetime`.

use std::ffi::OsString;
use std::io::{self, PipeReader, PipeWriter, Read as _, Write as _};
use std::net::Shutdown;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use rustix::process::{Pid, Signal, getpgrp, getpid, kill_process, kill_process_group};
use tokio::net::UnixStream;
use tokio::net::unix::pipe::Receiver;
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tracing::{debug, warn};

use super::{Binding, LIFELINE_SWEEP_BUDGET, MarkFailure, MarkOutcome};

const ANCHOR_SCRIPT: &str =
    "trap '' HUP INT TERM QUIT USR1 USR2 ALRM PIPE\nwhile read -r _; do :; done\nexec sleep 60\n";
const WATCHDOG_SCRIPT: &str = include_str!("watchdog.sh");
const SWEEP_AWK: &str = include_str!("sweep.awk");

/// How long [`Lifeline::mark`] waits for the watchdog to confirm.
const MARK_BUDGET: Duration = Duration::from_secs(3);

/// How long reaping an already-killed helper may take before giving up.
const REAP_GRACE: Duration = Duration::from_secs(1);

const READ_CHUNK_BYTES: usize = 1024;

static ANCHOR_WARNED: AtomicBool = AtomicBool::new(false);
static WATCHDOG_WARNED: AtomicBool = AtomicBool::new(false);
static SCAN_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_scan_failed_once() {
    if !SCAN_WARNED.swap(true, Ordering::Relaxed) {
        warn!(
            "the LSP child watchdog could not list processes (is `ps` installed?); \
             descendants that left the server's process group will not be killed"
        );
    }
}

/// Serializes creating the lifeline fds and spawning the helpers and server.
///
/// std sets `FD_CLOEXEC` on pipes and socketpairs non-atomically on macOS, so
/// without this lock a concurrently spawned server could inherit the write
/// end of another server's lifeline and delay its EOF.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// External programs the helpers run; replaceable in tests.
#[derive(Debug, Clone)]
pub(super) struct Options {
    /// Program listing `pid ppid pgid` rows (`ps -A -o pid= -o ppid= -o pgid=`).
    pub(super) ps: OsString,
    /// Shell that runs the watchdog script.
    pub(super) watchdog_shell: PathBuf,
    /// How long one `ps` snapshot may take before the sweep gives up on it.
    pub(super) scan_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            ps: OsString::from("ps"),
            watchdog_shell: PathBuf::from("/bin/sh"),
            scan_timeout: Duration::from_secs(2),
        }
    }
}

/// A freshly spawned server together with its lifeline, if one could be bound.
#[derive(Debug)]
pub(super) struct Spawned {
    pub(super) child: Child,
    pub(super) stderr: Receiver,
    pub(super) lifeline: Option<Lifeline>,
}

/// A process or process group that left the server's group and was frozen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Escapee {
    Group(Pid),
    Process(Pid),
}

/// Ids that must never be signalled as an escapee: the anchor's group, the
/// watchdog's own group, and mcpls's own pid and process group.
#[derive(Debug, Clone, Copy)]
struct OwnIds {
    anchor: Pid,
    watchdog: Pid,
    mcpls: Pid,
    mcpls_group: Pid,
}

impl OwnIds {
    fn new(anchor: Pid, watchdog: Pid) -> Self {
        Self {
            anchor,
            watchdog,
            mcpls: getpid(),
            mcpls_group: getpgrp(),
        }
    }

    fn contains(self, pid: Pid) -> bool {
        [self.anchor, self.watchdog, self.mcpls, self.mcpls_group].contains(&pid)
    }
}

fn signalable(raw: &str, own: OwnIds) -> Option<Pid> {
    let pid = Pid::from_raw(raw.parse::<i32>().ok().filter(|id| *id > 1)?)?;
    (!own.contains(pid)).then_some(pid)
}

const SCAN_POLL: Duration = Duration::from_millis(20);

fn scan_polls(timeout: Duration) -> u128 {
    timeout
        .as_millis()
        .checked_div(SCAN_POLL.as_millis())
        .unwrap_or(1)
        .max(1)
}

impl Escapee {
    fn parse(kind: &str, id: &str, own: OwnIds) -> Option<Self> {
        let pid = signalable(id, own)?;
        match kind {
            "group" => Some(Self::Group(pid)),
            "pid" => Some(Self::Process(pid)),
            _ => None,
        }
    }

    fn kill(self) {
        let result = match self {
            Self::Group(pgid) => kill_process_group(pgid, Signal::KILL),
            Self::Process(pid) => kill_process(pid, Signal::KILL),
        };
        if let Err(e) = result {
            debug!(escapee = ?self, error = %e, "could not kill escaped process");
        }
    }
}

/// One line the watchdog prints on its socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Report {
    Target(Escapee),
    Marked,
    ScanFailed,
    ScanTimedOut,
}

fn parse_report(line: &str, own: OwnIds) -> Option<Report> {
    let mut words = line.split_whitespace();
    match (words.next()?, words.next(), words.next(), words.next()) {
        ("marked", None, _, _) => Some(Report::Marked),
        ("scan-failed", None, _, _) => Some(Report::ScanFailed),
        ("scan-timeout", None, _, _) => Some(Report::ScanTimedOut),
        ("target", Some(kind), Some(id), None) => Escapee::parse(kind, id, own).map(Report::Target),
        _ => None,
    }
}

/// A command line mcpls sends to the watchdog.
#[derive(Debug, Clone, Copy)]
enum WatchdogCommand {
    Mark,
    ForgetLeader,
}

impl WatchdogCommand {
    const fn line(self) -> &'static [u8] {
        match self {
            Self::Mark => b"mark\n",
            Self::ForgetLeader => b"forget-leader\n",
        }
    }
}

fn read_available(mut read: impl FnMut(&mut [u8]) -> io::Result<usize>) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; READ_CHUNK_BYTES];
    loop {
        match read(&mut chunk) {
            Ok(0) => return (bytes, true),
            Ok(count) => bytes.extend_from_slice(chunk.get(..count).unwrap_or_default()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return (bytes, false),
            Err(e) => {
                debug!(error = %e, "lifeline socket read failed");
                return (bytes, true);
            }
        }
    }
}

/// What the watchdog last reported about its process snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanOutcome {
    Clean,
    /// `ps` failed fast (missing or unsupported): attribution is impossible.
    Failed,
    /// `ps` stalled: a retry with the leader still alive may succeed.
    TimedOut,
}

/// mcpls's end of the watchdog socketpair.
///
/// Closing the write side half-closes the socket, which the watchdog reads
/// as EOF and answers with its sweep; the read side stays usable to collect
/// what the watchdog reports until it exits. Commands go through a plain std
/// handle because tokio's `try_write` fails until the reactor has reported
/// readiness for a fresh socket.
#[derive(Debug)]
struct Channel {
    reader: UnixStream,
    writer: Option<StdUnixStream>,
    spare: StdUnixStream,
    inbox: Vec<u8>,
    own: OwnIds,
    targets: Vec<Escapee>,
    marked: bool,
    scan: ScanOutcome,
    eof: bool,
}

impl Channel {
    fn new(socket: StdUnixStream, own: OwnIds) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        let writer = socket.try_clone()?;
        let spare = socket.try_clone()?;
        Ok(Self {
            reader: UnixStream::from_std(socket)?,
            writer: Some(writer),
            spare,
            inbox: Vec::new(),
            own,
            targets: Vec::new(),
            marked: false,
            scan: ScanOutcome::Clean,
            eof: false,
        })
    }

    fn send(&self, command: WatchdogCommand) -> io::Result<()> {
        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?;
        let mut writer = writer;
        writer.write_all(command.line())
    }

    fn close(&mut self) {
        if let Some(writer) = self.writer.take()
            && let Err(e) = writer.shutdown(Shutdown::Write)
        {
            debug!(error = %e, "could not half-close the lifeline socket");
        }
    }

    /// Reads what the reactor has reported ready, without waiting.
    fn read_ready(&mut self) {
        let (bytes, eof) = read_available(|chunk| self.reader.try_read(chunk));
        self.ingest(&bytes, eof);
    }

    /// Reads everything the kernel has buffered, without waiting and without
    /// depending on reactor readiness (which can lag behind the last write).
    fn drain(&mut self) {
        let (bytes, eof) = read_available(|chunk| (&self.spare).read(chunk));
        self.ingest(&bytes, eof);
    }

    fn ingest(&mut self, bytes: &[u8], eof: bool) {
        self.inbox.extend_from_slice(bytes);
        self.eof |= eof;
        self.parse_inbox();
    }

    fn parse_inbox(&mut self) {
        while let Some(newline) = self.inbox.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.inbox.drain(..=newline).collect();
            match parse_report(&String::from_utf8_lossy(&line), self.own) {
                Some(Report::Marked) => self.marked = true,
                Some(Report::ScanFailed) => {
                    self.scan = ScanOutcome::Failed;
                    warn_scan_failed_once();
                }
                Some(Report::ScanTimedOut) => self.scan = ScanOutcome::TimedOut,
                Some(Report::Target(escapee)) => self.targets.push(escapee),
                None => {}
            }
        }
    }

    /// Asks the watchdog to freeze and report every escapee.
    ///
    /// Anything but [`MarkOutcome::Confirmed`] means the caller must not let
    /// the server exit gracefully.
    async fn mark(&mut self, budget: Duration) -> MarkOutcome {
        self.marked = false;
        self.scan = ScanOutcome::Clean;
        if let Err(e) = self.send(WatchdogCommand::Mark) {
            warn!(error = %e, "could not reach the LSP child watchdog");
            return MarkOutcome::Failed(MarkFailure::Unreachable);
        }
        let waited = timeout(budget, async {
            loop {
                if self.marked {
                    return Ok(());
                }
                if self.eof || self.reader.readable().await.is_err() {
                    return Err(MarkFailure::Unreachable);
                }
                self.read_ready();
            }
        })
        .await
        .unwrap_or(Err(MarkFailure::NoConfirmation));
        let outcome = match waited {
            Ok(()) if self.scan == ScanOutcome::TimedOut => {
                MarkOutcome::Failed(MarkFailure::SnapshotTimedOut)
            }
            Ok(()) => MarkOutcome::Confirmed,
            Err(failure) => MarkOutcome::Failed(failure),
        };
        if let MarkOutcome::Failed(failure) = outcome {
            warn!(
                ?failure,
                "LSP child watchdog escapee mark failed; skipping graceful exit"
            );
        }
        outcome
    }
}

/// Reserves a server's process group id for as long as it lives.
#[derive(Debug)]
struct Anchor {
    child: Child,
    hold: PipeWriter,
    group: Pid,
}

impl Anchor {
    fn start(stderr_hold: &PipeReader) -> io::Result<Self> {
        let (stdin, hold) = std::io::pipe()?;
        let child = Command::new("/bin/sh")
            .args(["-c", ANCHOR_SCRIPT])
            .env_clear()
            .env("PATH", helper_path())
            .current_dir("/")
            .process_group(0)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stderr_hold.try_clone()?))
            .stderr(Stdio::null())
            .spawn()?;
        let group = child_pid(&child)?;
        Ok(Self { child, hold, group })
    }

    fn is_alive(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

fn child_pid(child: &Child) -> io::Result<Pid> {
    child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::other("helper process has no pid"))
}

fn helper_path() -> OsString {
    let mut path = OsString::from("/usr/bin:/bin:/usr/sbin:/sbin");
    if let Some(inherited) = std::env::var_os("PATH") {
        path.push(":");
        path.push(inherited);
    }
    path
}

/// The out-of-group watchdog and mcpls's channel to it.
#[derive(Debug)]
struct Watchdog {
    child: Child,
    channel: Channel,
}

impl Watchdog {
    fn start(
        anchor: &Anchor,
        leader: Pid,
        server: ServerFds<'_>,
        options: &Options,
    ) -> io::Result<Self> {
        let (mine, theirs) = StdUnixStream::pair()?;
        let child = Command::new(&options.watchdog_shell)
            .args(["-c", WATCHDOG_SCRIPT, "lsp-lifeline-watchdog"])
            .arg(anchor.group.to_string())
            .arg(leader.to_string())
            .arg(&options.ps)
            .arg(SWEEP_AWK)
            .arg(scan_polls(options.scan_timeout).to_string())
            .env_clear()
            .env("PATH", helper_path())
            .current_dir("/")
            .process_group(0)
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(server.stdin_hold()?)
            .stderr(server.stdout_hold()?)
            .spawn()?;
        let own = OwnIds::new(anchor.group, child_pid(&child)?);
        Ok(Self {
            child,
            channel: Channel::new(mine, own)?,
        })
    }
}

/// The server's stdin write end and stdout read end, duplicated for the watchdog.
#[derive(Debug, Clone, Copy)]
pub(super) struct ServerFds<'a> {
    pub(super) stdin: Option<BorrowedFd<'a>>,
    pub(super) stdout: Option<BorrowedFd<'a>>,
}

impl ServerFds<'_> {
    fn stdin_hold(self) -> io::Result<Stdio> {
        hold(self.stdin)
    }

    fn stdout_hold(self) -> io::Result<Stdio> {
        hold(self.stdout)
    }
}

fn hold(fd: Option<BorrowedFd<'_>>) -> io::Result<Stdio> {
    fd.map_or_else(
        || Ok(Stdio::null()),
        |fd| fd.try_clone_to_owned().map(Stdio::from),
    )
}

/// The per-server binding: an anchor, and a watchdog unless it failed to start.
#[derive(Debug)]
pub(super) struct Lifeline {
    anchor: Anchor,
    watchdog: Option<Watchdog>,
    leader_forgotten: bool,
    lost_warned: bool,
}

/// Starts `command` bound to a fresh lifeline, or unbound if no anchor can be
/// started (the server then lives in its own process group and is killed only
/// by in-process paths).
pub(super) fn spawn(mut command: Command, options: &Options) -> io::Result<Spawned> {
    let guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let (stderr_rx, stderr_tx) = std::io::pipe()?;
    command.stderr(Stdio::from(stderr_tx));

    let mut anchor = start_anchor(&stderr_rx);
    let child = match spawn_leader(&mut command, &mut anchor, &stderr_rx) {
        Ok(child) => child,
        Err(e) => {
            if let Some(anchor) = anchor {
                discard(anchor);
            }
            return Err(e);
        }
    };
    let lifeline = anchor.map(|anchor| bind(anchor, &child, options));
    drop(command);
    drop(guard);

    match Receiver::from_owned_fd(OwnedFd::from(stderr_rx)) {
        Ok(stderr) => Ok(Spawned {
            child,
            stderr,
            lifeline,
        }),
        Err(e) => {
            match lifeline {
                Some(lifeline) => lifeline.release(Some(child)),
                None => drop(child),
            }
            Err(e)
        }
    }
}

fn start_anchor(stderr_hold: &PipeReader) -> Option<Anchor> {
    match Anchor::start(stderr_hold) {
        Ok(anchor) => Some(anchor),
        Err(e) => {
            warn_once(
                &ANCHOR_WARNED,
                &e,
                "failed to start the LSP child anchor; server is spawned unbound",
            );
            None
        }
    }
}

fn warn_once(flag: &AtomicBool, error: &io::Error, message: &str) {
    if !flag.swap(true, Ordering::Relaxed) {
        warn!(error = %error, "{message}");
    }
}

/// Spawns the server into the anchor's group, retrying once with a fresh
/// anchor when the group vanished (`setpgid` fails with EPERM, which looks
/// like `PermissionDenied`, exactly as an `exec` EACCES does; only a dead
/// anchor distinguishes the two).
fn spawn_leader(
    command: &mut Command,
    anchor: &mut Option<Anchor>,
    stderr_hold: &PipeReader,
) -> io::Result<Child> {
    apply_group(command, anchor.as_ref());
    match command.spawn() {
        Err(e)
            if e.kind() == io::ErrorKind::PermissionDenied
                && anchor.as_mut().is_some_and(|a| !a.is_alive()) =>
        {
            warn!(error = %e, "LSP child anchor vanished; restarting it");
            if let Some(dead) = anchor.take() {
                discard(dead);
            }
            *anchor = start_anchor(stderr_hold);
            apply_group(command, anchor.as_ref());
            command.spawn()
        }
        other => other,
    }
}

fn apply_group(command: &mut Command, anchor: Option<&Anchor>) {
    match anchor {
        Some(anchor) => command
            .process_group(anchor.group.as_raw_pid())
            .kill_on_drop(false),
        None => command.process_group(0).kill_on_drop(true),
    };
}

fn discard(mut anchor: Anchor) {
    if let Err(e) = anchor.child.start_kill() {
        debug!(error = %e, "could not kill the discarded LSP child anchor");
    }
}

fn bind(anchor: Anchor, child: &Child, options: &Options) -> Lifeline {
    let watchdog = child_pid(child).and_then(|leader| {
        let fds = ServerFds {
            stdin: child.stdin.as_ref().map(AsFd::as_fd),
            stdout: child.stdout.as_ref().map(AsFd::as_fd),
        };
        Watchdog::start(&anchor, leader, fds, options)
    });
    let watchdog = match watchdog {
        Ok(watchdog) => Some(watchdog),
        Err(e) => {
            warn_once(
                &WATCHDOG_WARNED,
                &e,
                "failed to start the LSP child watchdog; escaped descendants will not be swept",
            );
            None
        }
    };
    Lifeline {
        anchor,
        watchdog,
        leader_forgotten: false,
        lost_warned: false,
    }
}

impl Lifeline {
    /// Whether an out-of-process watchdog is alive, so the server must not
    /// watch the mcpls pid itself.
    pub(super) fn binding(&mut self) -> Binding {
        let alive = self
            .watchdog
            .as_mut()
            .is_some_and(|watchdog| matches!(watchdog.child.try_wait(), Ok(None)));
        if alive {
            Binding::Bound
        } else {
            Binding::Unbound
        }
    }

    /// Notes (once) that the watchdog died mid-session: the server keeps
    /// running, but nothing sweeps its tree if mcpls is killed later.
    pub(super) fn warn_if_watchdog_lost(&mut self) {
        if self.watchdog.is_some() && self.binding() == Binding::Unbound && !self.lost_warned {
            self.lost_warned = true;
            warn!(
                "the LSP child watchdog died; this server's process tree is no longer \
                 swept if mcpls is killed"
            );
        }
    }

    #[cfg(test)]
    pub(super) const fn leader_forgotten(&self) -> bool {
        self.leader_forgotten
    }

    /// Tells the watchdog the leader was reaped, so its pid is never targeted
    /// again: the kernel may hand it to an unrelated process.
    pub(super) fn forget_leader(&mut self) {
        if std::mem::replace(&mut self.leader_forgotten, true) {
            return;
        }
        if let Some(watchdog) = &self.watchdog
            && let Err(e) = watchdog.channel.send(WatchdogCommand::ForgetLeader)
        {
            debug!(error = %e, "could not tell the LSP child watchdog the leader was reaped");
        }
    }

    /// Freezes and records every escapee of the server so it can exit
    /// gracefully without taking its attribution with it.
    pub(super) async fn mark(&mut self) -> MarkOutcome {
        self.mark_within(MARK_BUDGET).await
    }

    pub(super) async fn mark_within(&mut self, budget: Duration) -> MarkOutcome {
        match &mut self.watchdog {
            Some(watchdog) => watchdog.channel.mark(budget).await,
            None => MarkOutcome::Confirmed,
        }
    }

    /// Sweeps the tree and returns once it is gone (or the sweep budget ran out).
    ///
    /// Hands the reaped leader back so the owner can still report its exit status.
    pub(super) async fn terminate(self, leader: Option<Child>, budget: Duration) -> Option<Child> {
        self.begin(leader, budget).finish().await
    }

    /// Starts the sweep without waiting for it. The tree is gone only once
    /// the detached task finishes, or the watchdog does the job alone when no
    /// runtime is available.
    pub(super) fn release(self, leader: Option<Child>) {
        let sweep = self.begin(leader, LIFELINE_SWEEP_BUDGET);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                sweep.finish().await;
            });
        } else {
            debug!("no runtime to finish the LSP child sweep; the watchdog runs alone");
        }
    }

    /// Closes the lifeline, which triggers the watchdog's sweep. A dead
    /// watchdog is replaced by killing from here.
    fn begin(self, leader: Option<Child>, budget: Duration) -> Sweep {
        let Self {
            anchor, watchdog, ..
        } = self;
        let Anchor { child, hold, group } = anchor;
        drop(hold);
        let mut sweep = Sweep {
            watchdog: None,
            channel: None,
            anchor: child,
            group,
            leader,
            budget,
        };
        if let Some(Watchdog { child, mut channel }) = watchdog {
            channel.close();
            sweep.watchdog = Some(child);
            sweep.channel = Some(channel);
        }
        if sweep.watchdog_exited() {
            sweep.fallback_kill();
        }
        sweep
    }
}

/// A sweep in progress: the helpers and leader still to be waited for.
#[derive(Debug)]
struct Sweep {
    watchdog: Option<Child>,
    channel: Option<Channel>,
    anchor: Child,
    group: Pid,
    leader: Option<Child>,
    budget: Duration,
}

impl Sweep {
    fn watchdog_exited(&mut self) -> bool {
        self.watchdog
            .as_mut()
            .is_none_or(|watchdog| !matches!(watchdog.try_wait(), Ok(None)))
    }

    /// Waits for the watchdog, collecting what it reports meanwhile, then
    /// kills whatever it left and reaps the helpers.
    async fn finish(mut self) -> Option<Child> {
        if !self.await_watchdog().await {
            warn!(
                budget = ?self.budget,
                "LSP child watchdog did not finish its sweep in time; killing from here"
            );
            self.fallback_kill();
        }
        if let Some(leader) = &mut self.leader {
            reap(leader).await;
        }
        reap(&mut self.anchor).await;
        if let Some(watchdog) = &mut self.watchdog {
            reap(watchdog).await;
        }
        self.leader
    }

    async fn await_watchdog(&mut self) -> bool {
        let Some(watchdog) = &mut self.watchdog else {
            return true;
        };
        let exited = timeout(self.budget, async {
            let Some(channel) = &mut self.channel else {
                return watchdog.wait().await.map(drop);
            };
            loop {
                tokio::select! {
                    status = watchdog.wait() => return status.map(drop),
                    readable = channel.reader.readable(), if !channel.eof => {
                        if readable.is_ok() {
                            channel.read_ready();
                        } else {
                            channel.eof = true;
                        }
                    }
                }
            }
        })
        .await
        .is_ok();
        if let Some(channel) = &mut self.channel {
            channel.drain();
        }
        exited
    }

    /// Kills by hand what the watchdog was supposed to: the frozen escapees
    /// it reported, the whole group (only while the anchor still reserves its
    /// id), the leader and the helpers.
    fn fallback_kill(&mut self) {
        if let Some(channel) = &mut self.channel {
            channel.drain();
            channel.targets.drain(..).for_each(Escapee::kill);
        }
        if !matches!(self.anchor.try_wait(), Ok(Some(_)))
            && let Err(e) = kill_process_group(self.group, Signal::KILL)
        {
            debug!(error = %e, "could not kill the LSP server's process group");
        }
        for child in self
            .leader
            .iter_mut()
            .chain(std::iter::once(&mut self.anchor))
            .chain(self.watchdog.iter_mut())
        {
            if let Err(e) = child.start_kill() {
                debug!(error = %e, "could not kill an LSP server helper");
            }
        }
    }
}

async fn reap(child: &mut Child) {
    match timeout(REAP_GRACE, child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => debug!(error = %e, "could not reap an LSP server helper"),
        Err(_) => debug!("an LSP server helper did not exit after SIGKILL"),
    }
}

#[cfg(test)]
mod tests;
