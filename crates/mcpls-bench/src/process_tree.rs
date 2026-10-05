//! Lifetime and memory accounting of the mcpls process tree.
//!
//! mcpls leads its own process group (a Job Object on Windows), but since the
//! language servers sit in a per-server watchdog group, and helpers such as
//! rust-analyzer's flycheck call `setsid`, group membership alone no longer
//! covers the tree. Memory is therefore summed over the parent-pid closure of
//! mcpls, and survivors are found and killed by pid plus start time.

use std::collections::BTreeSet;
use std::process::ExitStatus;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mcpls_core::lsp::LIFELINE_SWEEP_BUDGET;
#[cfg(unix)]
use tokio::process::Child;
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

use crate::process_table::{ParentLink, ProcessIdentity, ProcessRow, ProcessTable};
use crate::report::{Kib, ProcessRss, RssReading, ShutdownOutcome};

/// How long mcpls gets to exit on its own once its session is closed.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// How long the tree may outlive the leader: the per-server watchdogs get
/// [`LIFELINE_SWEEP_BUDGET`] to sweep, plus a second for the final poll.
pub const ORPHAN_GRACE: Duration = LIFELINE_SWEEP_BUDGET.saturating_add(Duration::from_secs(1));
const ORPHAN_POLL_INTERVAL: Duration = Duration::from_millis(200);
const CAPTURE_TIMEOUT: Duration = if cfg!(windows) {
    Duration::from_secs(30)
} else {
    Duration::from_secs(5)
};

/// How the MCP session with the process under test ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// The client closed the session and the server acknowledged within the grace period.
    Closed,
    /// The handshake failed, or closing timed out or errored.
    Abandoned,
}

/// The id of a process group, equal to the pid of its leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessGroupId(u32);

impl ProcessGroupId {
    /// Wraps a raw group id.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw group id.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// `None` for ids that must never be signalled as a group: 0 and 1 would address
    /// the caller's own group or, via `kill(-1, ..)`, every process.
    #[cfg(unix)]
    fn to_pid(self) -> Option<rustix::process::Pid> {
        rustix::process::Pid::from_raw(crate::process_table::signalable_pid(self.0)?)
    }
}

#[cfg(unix)]
type Leader = Child;
#[cfg(windows)]
type Leader = Box<dyn process_wrap::tokio::ChildWrapper>;

/// Owns a spawned child and the process group (Unix) or Job Object (Windows) it leads.
///
/// The group id is captured at spawn, because it is unobtainable once the
/// leader is reaped. While the leader is unreaped or any member lives, the
/// kernel keeps the id reserved, so a sweep cannot hit a recycled group.
/// [`finish`](Self::finish) and [`wait_status`](Self::wait_status) consume the
/// guard and disarm it afterwards, so `Drop` never sweeps a finished group. One
/// race remains and is accepted: the last member exits between the emptiness
/// probe and the kill, and the id is reused as a new group within that window.
/// Dropping an armed guard (a cancelled future, a signal) kills the group.
#[derive(Debug)]
pub struct ProcessGroup {
    leader: Leader,
    pid: u32,
    armed: bool,
}

impl ProcessGroup {
    /// Spawns `command` as the leader of a new process group, killing the tree if the guard is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error when the process cannot be spawned.
    pub fn spawn(mut command: Command) -> Result<Self> {
        command.kill_on_drop(true);
        #[cfg(unix)]
        let leader = {
            command.process_group(0);
            command.spawn().context("failed to spawn the process")?
        };
        #[cfg(windows)]
        let leader = {
            use process_wrap::tokio::{CommandWrap, JobObject, KillOnDrop};

            let mut wrapped = CommandWrap::from(command);
            wrapped.wrap(JobObject).wrap(KillOnDrop);
            wrapped.spawn().context("failed to spawn the process")?
        };
        let pid = leader
            .id()
            .context("the process exited before its id was read")?;
        Ok(Self {
            leader,
            pid,
            armed: true,
        })
    }

    /// The pid of the leader.
    #[must_use]
    pub const fn leader_pid(&self) -> u32 {
        self.pid
    }

    /// The id of the Unix process group this guard leads.
    #[cfg(unix)]
    #[must_use]
    pub const fn pgid(&self) -> ProcessGroupId {
        ProcessGroupId::new(self.pid)
    }

    /// Takes the piped stdout and stdin of the child.
    ///
    /// # Errors
    ///
    /// Returns an error when either stream was not piped or was already taken.
    pub fn take_stdio(&mut self) -> Result<(ChildStdout, ChildStdin)> {
        #[cfg(unix)]
        let (stdout, stdin) = (self.leader.stdout.take(), self.leader.stdin.take());
        #[cfg(windows)]
        let (stdout, stdin) = (self.leader.stdout().take(), self.leader.stdin().take());
        Ok((
            stdout.context("stdout was not captured")?,
            stdin.context("stdin was not captured")?,
        ))
    }

    /// Takes the piped stderr of the child.
    ///
    /// # Errors
    ///
    /// Returns an error when stderr was not piped or was already taken.
    pub fn take_stderr(&mut self) -> Result<ChildStderr> {
        #[cfg(unix)]
        let stderr = self.leader.stderr.take();
        #[cfg(windows)]
        let stderr = self.leader.stderr().take();
        stderr.context("stderr was not captured")
    }

    /// Waits for the leader only, not for its descendants.
    async fn wait_leader(&mut self) -> std::io::Result<ExitStatus> {
        #[cfg(unix)]
        return self.leader.wait().await;
        #[cfg(windows)]
        return self.leader.inner_mut().wait().await;
    }

    /// Kills every member of the group.
    #[cfg(unix)]
    fn sweep(&self) {
        if let Err(error) = kill_group(self.pgid()) {
            eprintln!(
                "warning: failed to kill process group {}: {error}",
                self.pid
            );
        }
    }

    /// Kills every member of the job.
    #[cfg(windows)]
    fn sweep(&mut self) {
        if let Err(error) = self.leader.start_kill() {
            eprintln!(
                "warning: failed to kill the job of pid {}: {error}",
                self.pid
            );
        }
    }

    #[cfg(unix)]
    async fn kill_leader(&mut self) {
        if let Err(error) = self.leader.kill().await {
            eprintln!("warning: failed to kill pid {}: {error}", self.pid);
        }
    }

    #[cfg(windows)]
    fn kill_leader(&mut self) -> std::future::Ready<()> {
        if let Err(error) = self.leader.start_kill() {
            eprintln!("warning: failed to kill pid {}: {error}", self.pid);
        }
        std::future::ready(())
    }

    /// Waits for a short-lived command, then kills whatever its group still holds.
    ///
    /// Dropping the future before it completes kills the whole group, so an
    /// interrupted setup step leaves no grandchildren behind.
    ///
    /// # Errors
    ///
    /// Returns an error when waiting for the leader fails.
    pub async fn wait_status(mut self) -> Result<ExitStatus> {
        let status = self
            .wait_leader()
            .await
            .with_context(|| format!("failed to wait for pid {}", self.pid))?;
        self.sweep();
        self.armed = false;
        Ok(status)
    }

    /// Waits for the leader, then for the rest of its tree, killing whatever outlives its grace.
    ///
    /// `session` tells whether the MCP session shut down cleanly. A leader that
    /// does not exit within [`SHUTDOWN_GRACE`] is killed together with its group.
    /// `watch` supplies the processes seen during the run: any that still live
    /// (same pid and start time) after [`ORPHAN_GRACE`] are reported as
    /// [`ShutdownOutcome::OrphansKilled`] and killed.
    pub async fn finish(self, session: SessionEnd, watch: &TreeWatch) -> ShutdownOutcome {
        self.finish_within(session, watch, SHUTDOWN_GRACE, ORPHAN_GRACE)
            .await
    }

    async fn finish_within(
        mut self,
        session: SessionEnd,
        watch: &TreeWatch,
        shutdown_grace: Duration,
        orphan_grace: Duration,
    ) -> ShutdownOutcome {
        let exited = matches!(
            tokio::time::timeout(shutdown_grace, self.wait_leader()).await,
            Ok(Ok(_))
        );
        let outcome = if !exited {
            self.sweep();
            self.kill_leader().await;
            if let Some(survivors) = self.leftovers(watch, orphan_grace).await {
                kill_identified(&survivors).await;
                ShutdownOutcome::OrphansKilled
            } else {
                ShutdownOutcome::Killed
            }
        } else if let Some(survivors) = self.leftovers(watch, orphan_grace).await {
            self.sweep();
            kill_identified(&survivors).await;
            ShutdownOutcome::OrphansKilled
        } else if session == SessionEnd::Closed {
            ShutdownOutcome::Clean
        } else {
            ShutdownOutcome::Killed
        };
        self.armed = false;
        outcome
    }

    /// Polls until the tree is gone; `Some(identities)` when something outlives `grace`.
    ///
    /// The identities may be empty when only the group probe, used if the process
    /// table cannot be read, saw a member.
    async fn leftovers(&self, watch: &TreeWatch, grace: Duration) -> Option<Vec<ProcessIdentity>> {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            let alive = match capture_table().await {
                Ok(table) => watch.survivors(&table, cfg!(unix).then_some(self.pid)),
                Err(error) => {
                    eprintln!("warning: cannot read the process table: {error:#}");
                    if group_is_empty_for(self.pid) {
                        Vec::new()
                    } else {
                        return Some(Vec::new());
                    }
                }
            };
            if alive.is_empty() {
                return None;
            }
            if tokio::time::Instant::now() >= deadline {
                return Some(alive);
            }
            tokio::time::sleep(ORPHAN_POLL_INTERVAL).await;
        }
    }
}

/// Whether the group led by `leader` is gone; Windows has no groups, so nothing can be left in one.
#[cfg(unix)]
fn group_is_empty_for(leader: u32) -> bool {
    group_is_empty(ProcessGroupId::new(leader))
}

#[cfg(not(unix))]
const fn group_is_empty_for(_leader: u32) -> bool {
    true
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if self.armed {
            self.sweep();
        }
    }
}

#[cfg(unix)]
fn group_is_empty(group: ProcessGroupId) -> bool {
    use rustix::io::Errno;

    let Some(leader) = group.to_pid() else {
        return true;
    };
    match rustix::process::test_kill_process_group(leader) {
        Ok(()) => false,
        Err(Errno::SRCH) => true,
        Err(error) => {
            eprintln!(
                "warning: cannot probe process group {}: {error}",
                group.get()
            );
            false
        }
    }
}

#[cfg(unix)]
fn kill_group(group: ProcessGroupId) -> std::io::Result<()> {
    use rustix::io::Errno;

    let Some(leader) = group.to_pid() else {
        return Ok(());
    };
    match rustix::process::kill_process_group(leader, rustix::process::Signal::KILL) {
        Err(Errno::SRCH) => Ok(()),
        other => other.map_err(Into::into),
    }
}

#[cfg(unix)]
/// Pids in `table` that still hold exactly one of `targets` and may be signalled.
fn killable(table: &ProcessTable, targets: &[ProcessIdentity]) -> Vec<i32> {
    targets
        .iter()
        .filter(|target| table.find(target).is_some())
        .filter_map(|target| crate::process_table::signalable_pid(target.pid()))
        .collect()
}

/// Kills the processes of `targets` that are still alive with the same start time.
///
/// The check runs against a table captured immediately before, so a recycled pid
/// is skipped; the window between that snapshot and the signal is one `ps` call.
/// Nothing is killed by bare pid on Windows: the Job Object covers the tree there.
#[cfg(unix)]
async fn kill_identified(targets: &[ProcessIdentity]) {
    {
        use rustix::io::Errno;
        use rustix::process::{Pid, Signal, kill_process};

        let table = match capture_table().await {
            Ok(table) => table,
            Err(error) => {
                eprintln!("warning: cannot verify survivors before killing them: {error:#}");
                return;
            }
        };
        for raw in killable(&table, targets) {
            let Some(pid) = Pid::from_raw(raw) else {
                continue;
            };
            match kill_process(pid, Signal::KILL) {
                Ok(()) | Err(Errno::SRCH) => {}
                Err(error) => eprintln!("warning: failed to kill pid {raw}: {error}"),
            }
        }
    }
}

#[cfg(not(unix))]
fn kill_identified(_targets: &[ProcessIdentity]) -> std::future::Ready<()> {
    std::future::ready(())
}

/// Reads the system process table.
///
/// # Errors
///
/// Returns an error when the platform tool cannot be run, times out or prints
/// something unrecognisable.
pub async fn capture_table() -> Result<ProcessTable> {
    #[cfg(unix)]
    {
        let ps = crate::pin::resolve_in_path("ps")?;
        let output = run_to_string(
            Command::new(ps)
                .args(["-A", "-o", "pid=,ppid=,pgid=,stat=,rss=,lstart=,comm="])
                .env("LC_ALL", "C"),
        )
        .await?;
        ProcessTable::parse_unix(&output)
    }
    #[cfg(windows)]
    {
        const SCRIPT: &str = "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,WorkingSetSize,@{n='Created';e={if ($_.CreationDate) { $_.CreationDate.ToString('o') } else { '' }}},Name | ConvertTo-Csv -NoTypeInformation";
        let shell = crate::pin::resolve_in_path("powershell")?;
        let output = run_to_string(Command::new(shell).args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ]))
        .await?;
        ProcessTable::parse_windows(&output)
    }
}

async fn run_to_string(command: &mut Command) -> Result<String> {
    let output = tokio::time::timeout(
        CAPTURE_TIMEOUT,
        command
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("the process listing timed out")?
    .context("failed to run the process listing")?;
    if !output.status.success() {
        bail!("the process listing exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy_owned(output.stdout))
}

/// Tracks the mcpls process tree across a run.
///
/// Every observation sums the resident memory of the parent-pid closure of
/// mcpls and remembers each member's identity, so that survivors can be told
/// from recycled pids after shutdown.
#[derive(Debug)]
pub struct TreeWatch {
    leader_pid: u32,
    leader: Option<ProcessIdentity>,
    seen: BTreeSet<ProcessIdentity>,
}

impl TreeWatch {
    /// Starts watching the tree rooted at `leader_pid`.
    #[must_use]
    pub const fn new(leader_pid: u32) -> Self {
        Self {
            leader_pid,
            leader: None,
            seen: BTreeSet::new(),
        }
    }

    /// Records the tree found in `table` and returns its memory.
    pub fn observe(&mut self, table: &ProcessTable, link: ParentLink) -> RssReading {
        if self.leader.is_none() {
            self.leader = table.identity_of(self.leader_pid).cloned();
        }
        let tree = self
            .leader
            .as_ref()
            .map_or_default(|leader| table.tree_of(leader, link));
        if tree.is_empty() {
            return RssReading::Unavailable {
                reason: format!(
                    "mcpls (pid {}) is not in the process table",
                    self.leader_pid
                ),
            };
        }
        self.seen
            .extend(tree.iter().map(|row| row.identity.clone()));
        RssReading::Measured {
            total: Kib(tree.iter().map(|row| row.rss.0).sum()),
            processes: tree.into_iter().map(process_rss).collect(),
        }
    }

    /// Reads the process table and observes the tree.
    pub async fn sample(&mut self) -> RssReading {
        match capture_table().await {
            Ok(table) => self.observe(&table, ParentLink::PLATFORM),
            Err(error) => RssReading::Unavailable {
                reason: format!("{error:#}"),
            },
        }
    }

    /// Identities of the observed processes that are alive in `table`, plus live members of `group`.
    #[must_use]
    pub fn survivors(&self, table: &ProcessTable, group: Option<u32>) -> Vec<ProcessIdentity> {
        table
            .rows()
            .iter()
            .filter(|row| !row.zombie)
            .filter(|row| {
                self.seen.contains(&row.identity) || (group.is_some() && row.pgid == group)
            })
            .map(|row| row.identity.clone())
            .collect()
    }
}

fn process_rss(row: &ProcessRow) -> ProcessRss {
    ProcessRss {
        pid: row.identity.pid(),
        rss: row.rss,
        command: row.command.clone(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::process_table::StartTime;

    const T0: &str = "Sun Oct  4 16:00:00 2026";
    const T1: &str = "Sun Oct  4 16:00:09 2026";

    fn row(id: u32, parent: u32, group: u32, started: &str, rss: u64) -> ProcessRow {
        ProcessRow {
            identity: ProcessIdentity::new(id, StartTime::new(started)),
            ppid: parent,
            pgid: Some(group),
            zombie: false,
            rss: Kib(rss),
            command: format!("p{id}"),
        }
    }

    fn id(pid: u32, started: &str) -> ProcessIdentity {
        ProcessIdentity::new(pid, StartTime::new(started))
    }

    #[test]
    fn observation_sums_the_whole_tree_including_setsid_descendants() {
        let table = ProcessTable::from_rows(vec![
            row(10, 1, 10, T0, 100),
            row(11, 10, 11, T0, 200),
            row(12, 11, 11, T0, 300),
            row(13, 12, 13, T0, 400),
            row(99, 1, 99, T0, 5000),
        ]);
        let mut watch = TreeWatch::new(10);
        let RssReading::Measured { total, processes } = watch.observe(&table, ParentLink::Pid)
        else {
            panic!("tree should be measured");
        };
        assert_eq!(total, Kib(1000));
        assert_eq!(processes.len(), 4);
    }

    #[test]
    fn a_vanished_leader_is_unavailable() {
        let mut watch = TreeWatch::new(10);
        let reading = watch.observe(&ProcessTable::default(), ParentLink::Pid);
        assert_matches!(reading, RssReading::Unavailable { .. });
    }

    #[test]
    fn a_recycled_leader_pid_is_not_mistaken_for_the_leader() {
        let mut watch = TreeWatch::new(10);
        watch.observe(
            &ProcessTable::from_rows(vec![row(10, 1, 10, T0, 1)]),
            ParentLink::Pid,
        );
        let recycled =
            ProcessTable::from_rows(vec![row(10, 1, 10, T1, 777), row(11, 10, 10, T1, 1)]);
        assert_matches!(
            watch.observe(&recycled, ParentLink::Pid),
            RssReading::Unavailable { .. }
        );
    }

    #[test]
    fn survivors_match_on_pid_and_start_time_only() {
        let mut watch = TreeWatch::new(10);
        let before = ProcessTable::from_rows(vec![
            row(10, 1, 10, T0, 1),
            row(11, 10, 11, T0, 1),
            row(12, 11, 12, T0, 1),
        ]);
        watch.observe(&before, ParentLink::Pid);
        let after = ProcessTable::from_rows(vec![
            row(11, 1, 11, T0, 1),
            row(12, 1, 12, T1, 1),
            row(40, 1, 40, T1, 1),
        ]);
        assert_eq!(watch.survivors(&after, None), [id(11, T0)]);
    }

    #[test]
    fn group_members_survive_even_if_never_observed() {
        let watch = TreeWatch::new(10);
        let mut zombie = row(12, 1, 10, T0, 0);
        zombie.zombie = true;
        let table =
            ProcessTable::from_rows(vec![row(11, 1, 10, T0, 1), zombie, row(13, 1, 13, T0, 1)]);
        assert_eq!(watch.survivors(&table, Some(10)), [id(11, T0)]);
        assert!(watch.survivors(&table, None).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn only_processes_with_a_matching_start_time_are_killable() {
        let table = ProcessTable::from_rows(vec![
            row(11, 1, 11, T0, 1),
            row(12, 1, 12, T1, 1),
            row(1, 0, 1, T0, 1),
        ]);
        let targets = [id(11, T0), id(12, T0), id(1, T0), id(77, T0)];
        assert_eq!(killable(&table, &targets), [11]);
    }

    #[cfg(unix)]
    mod unix {
        use std::process::Stdio;

        use tokio::io::{AsyncBufReadExt, BufReader};

        use super::*;

        fn sh(script: &str) -> Command {
            let mut command = Command::new("sh");
            command
                .args(["-c", script])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            command
        }

        async fn first_line(group: &mut ProcessGroup) -> String {
            let (stdout, _stdin) = group.take_stdio().unwrap();
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).await.unwrap();
            line.trim().to_owned()
        }

        async fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
            for _ in 0..60 {
                if condition() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            false
        }

        fn alive(pid: &str) -> bool {
            let pid = rustix::process::Pid::from_raw(pid.parse().unwrap()).unwrap();
            rustix::process::test_kill_process(pid).is_ok()
        }

        const SETSID_SLEEP: &str =
            "perl -MPOSIX -e 'POSIX::setsid(); exec q(sleep), q(30)' & echo $!";

        #[tokio::test]
        async fn clean_exit_is_clean_only_when_the_session_closed() {
            let watch = TreeWatch::new(0);
            let group = ProcessGroup::spawn(sh("exit 0")).unwrap();
            assert_eq!(
                group.finish(SessionEnd::Closed, &watch).await,
                ShutdownOutcome::Clean
            );
            let group = ProcessGroup::spawn(sh("exit 0")).unwrap();
            assert_eq!(
                group.finish(SessionEnd::Abandoned, &watch).await,
                ShutdownOutcome::Killed
            );
        }

        #[tokio::test]
        async fn members_outliving_the_leader_are_killed() {
            let mut group = ProcessGroup::spawn(sh("sleep 30 & echo $!")).unwrap();
            let pgid = group.pgid();
            let member = first_line(&mut group).await;
            assert!(!member.is_empty());
            let watch = TreeWatch::new(group.leader_pid());
            let outcome = group
                .finish_within(
                    SessionEnd::Closed,
                    &watch,
                    SHUTDOWN_GRACE,
                    Duration::from_millis(400),
                )
                .await;
            assert_eq!(outcome, ShutdownOutcome::OrphansKilled);
            assert!(wait_until(|| group_is_empty(pgid)).await);
        }

        #[tokio::test]
        async fn a_setsid_descendant_is_counted_and_killed_after_the_leader_exits() {
            let mut group = ProcessGroup::spawn(sh(&format!("{SETSID_SLEEP}; sleep 1"))).unwrap();
            let escapee = first_line(&mut group).await;
            let mut watch = TreeWatch::new(group.leader_pid());
            let reading = watch.sample().await;
            let RssReading::Measured { processes, .. } = reading else {
                panic!("tree should be measured: {reading:?}");
            };
            assert!(
                processes.iter().any(|p| p.pid.to_string() == escapee),
                "setsid child {escapee} missing from {processes:?}"
            );
            let outcome = group
                .finish_within(
                    SessionEnd::Closed,
                    &watch,
                    SHUTDOWN_GRACE,
                    Duration::from_millis(600),
                )
                .await;
            assert_eq!(outcome, ShutdownOutcome::OrphansKilled);
            assert!(wait_until(|| !alive(&escapee)).await);
        }

        #[tokio::test]
        async fn a_forced_kill_still_finds_and_kills_escaped_descendants() {
            let script = format!("{SETSID_SLEEP}; sleep 30");
            let mut group = ProcessGroup::spawn(sh(&script)).unwrap();
            let escapee = first_line(&mut group).await;
            let mut watch = TreeWatch::new(group.leader_pid());
            watch.sample().await;
            let outcome = group
                .finish_within(
                    SessionEnd::Closed,
                    &watch,
                    Duration::from_millis(200),
                    Duration::from_millis(600),
                )
                .await;
            assert_eq!(outcome, ShutdownOutcome::OrphansKilled);
            assert!(wait_until(|| !alive(&escapee)).await);
        }

        #[tokio::test]
        async fn cancelling_wait_status_kills_grandchildren() {
            let mut group = ProcessGroup::spawn(sh("sleep 30 & echo $!; wait")).unwrap();
            let member = first_line(&mut group).await;
            assert!(alive(&member));
            let cancelled =
                tokio::time::timeout(Duration::from_millis(300), group.wait_status()).await;
            assert!(cancelled.is_err());
            assert!(wait_until(|| !alive(&member)).await);
        }

        #[tokio::test]
        async fn wait_status_sweeps_leftovers_of_a_finished_command() {
            let mut group = ProcessGroup::spawn(sh("sleep 30 & echo $!")).unwrap();
            let member = first_line(&mut group).await;
            let status = group.wait_status().await.unwrap();
            assert!(status.success());
            assert!(wait_until(|| !alive(&member)).await);
        }

        #[tokio::test]
        async fn dropping_the_guard_kills_the_whole_group() {
            let mut group = ProcessGroup::spawn(sh("sleep 30 & echo $!; wait")).unwrap();
            let member = first_line(&mut group).await;
            assert!(alive(&member));
            drop(group);
            assert!(wait_until(|| !alive(&member)).await);
        }

        #[test]
        fn init_and_own_group_ids_are_never_signalled() {
            assert!(ProcessGroupId::new(0).to_pid().is_none());
            assert!(ProcessGroupId::new(1).to_pid().is_none());
            assert!(ProcessGroupId::new(2).to_pid().is_some());
            assert!(kill_group(ProcessGroupId::new(1)).is_ok());
        }

        #[tokio::test]
        async fn the_live_process_table_contains_this_process_with_a_start_time() {
            let table = capture_table().await.unwrap();
            let me = table.identity_of(std::process::id()).unwrap();
            assert!(!me.started().as_str().is_empty());
            assert!(!table.tree_of(me, ParentLink::Pid).is_empty());
        }
    }
}
