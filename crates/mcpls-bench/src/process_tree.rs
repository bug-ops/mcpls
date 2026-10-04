//! Lifetime and memory accounting of the mcpls process tree.
//!
//! mcpls runs as the leader of its own process group so that the language
//! server and its helpers (cargo, proc-macro servers) can be measured and
//! reaped together.

#[cfg(unix)]
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::report::{Kib, ProcessRss, RssReading, ShutdownOutcome};

/// How long mcpls gets to exit on its own once its session is closed.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// How long members of the group may outlive the leader before they are killed.
pub const ORPHAN_GRACE: Duration = Duration::from_secs(1);
#[cfg(unix)]
const ORPHAN_POLL_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(unix)]
const PS_TIMEOUT: Duration = Duration::from_secs(5);

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
        if self.0 <= 1 {
            return None;
        }
        rustix::process::Pid::from_raw(i32::try_from(self.0).ok()?)
    }
}

/// Owns the mcpls child and the process group it leads.
///
/// The group id is captured at spawn, because it is unobtainable once the
/// leader is reaped. While the leader is unreaped or any member lives, the
/// kernel keeps the id reserved, so a sweep cannot hit a recycled group.
/// [`finish`](Self::finish) consumes the guard and disarms it afterwards, so
/// `Drop` never sweeps a finished group. One race remains and is accepted: the
/// last member exits between the emptiness probe and the kill, and the id is
/// reused as a new group within that window.
#[derive(Debug)]
pub struct ProcessGroup {
    child: Child,
    pgid: ProcessGroupId,
    armed: bool,
}

impl ProcessGroup {
    /// Spawns `command` as the leader of a new process group, killing the tree if the guard is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error when the process cannot be spawned.
    pub fn spawn(command: &mut Command) -> Result<Self> {
        command.kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().context("failed to spawn the process")?;
        let pid = child
            .id()
            .context("the process exited before its id was read")?;
        Ok(Self {
            child,
            pgid: ProcessGroupId::new(pid),
            armed: true,
        })
    }

    /// The id of the group this guard leads.
    #[must_use]
    pub const fn pgid(&self) -> ProcessGroupId {
        self.pgid
    }

    /// Takes the piped stdout and stdin of the child.
    ///
    /// # Errors
    ///
    /// Returns an error when either stream was not piped or was already taken.
    pub fn take_stdio(&mut self) -> Result<(ChildStdout, ChildStdin)> {
        let stdout = self
            .child
            .stdout
            .take()
            .context("stdout was not captured")?;
        let stdin = self.child.stdin.take().context("stdin was not captured")?;
        Ok((stdout, stdin))
    }

    /// Waits for the leader, then for the rest of the group, killing whatever outlives its grace.
    ///
    /// `closed` tells whether the MCP session shut down cleanly. A leader that
    /// does not exit within [`SHUTDOWN_GRACE`] is killed together with its group.
    pub async fn finish(mut self, closed: bool) -> ShutdownOutcome {
        let exited = matches!(
            tokio::time::timeout(SHUTDOWN_GRACE, self.child.wait()).await,
            Ok(Ok(_))
        );
        let outcome = if !exited {
            self.sweep();
            if let Err(error) = self.child.kill().await {
                eprintln!("warning: failed to kill mcpls: {error}");
            }
            ShutdownOutcome::Killed
        } else if self.orphans_outlive_leader().await {
            self.sweep();
            ShutdownOutcome::OrphansKilled
        } else if closed {
            ShutdownOutcome::Clean
        } else {
            ShutdownOutcome::Killed
        };
        self.armed = false;
        outcome
    }

    #[cfg(unix)]
    async fn orphans_outlive_leader(&self) -> bool {
        let deadline = tokio::time::Instant::now() + ORPHAN_GRACE;
        loop {
            if group_is_empty(self.pgid) {
                return false;
            }
            if tokio::time::Instant::now() >= deadline {
                return true;
            }
            tokio::time::sleep(ORPHAN_POLL_INTERVAL).await;
        }
    }

    #[cfg(not(unix))]
    async fn orphans_outlive_leader(&self) -> bool {
        false
    }

    #[cfg(unix)]
    fn sweep(&self) {
        if let Err(error) = kill_group(self.pgid) {
            eprintln!(
                "warning: failed to kill process group {}: {error}",
                self.pgid.get()
            );
        }
    }

    #[cfg(not(unix))]
    fn sweep(&mut self) {
        if let Err(error) = self.child.start_kill() {
            eprintln!("warning: failed to kill mcpls: {error}");
        }
    }
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

/// Parses `ps -A -o pgid=,pid=,rss=,comm=` output into the members of `group`.
///
/// `comm` is the last column because it may contain spaces on macOS.
///
/// # Errors
///
/// Returns an error for a line of `group` that does not start with three
/// integers followed by a command name. Lines of other groups are not inspected,
/// so an unrelated malformed line cannot hide the reading.
///
/// # Examples
///
/// ```
/// use mcpls_bench::process_tree::{parse_ps, ProcessGroupId};
///
/// let output = "  10   10  2048 mcpls\n  10   11  4096 rust-analyzer\n  99   99   100 sh\n";
/// let members = parse_ps(output, ProcessGroupId::new(10)).unwrap();
/// assert_eq!(members.len(), 2);
/// assert_eq!(members[1].command, "rust-analyzer");
/// ```
pub fn parse_ps(output: &str, group: ProcessGroupId) -> Result<Vec<ProcessRss>> {
    let mut members = Vec::new();
    for line in output.lines() {
        let mut fields = line.split_ascii_whitespace();
        let belongs_to_group = fields
            .clone()
            .next()
            .and_then(|first| first.parse::<u32>().ok())
            == Some(group.get());
        if !belongs_to_group {
            continue;
        }
        let mut number = |name: &str| -> Result<u32> {
            fields
                .next()
                .with_context(|| format!("ps line `{line}` has no {name}"))?
                .parse()
                .with_context(|| format!("ps line `{line}` has a malformed {name}"))
        };
        number("pgid")?;
        let pid = number("pid")?;
        let rss = number("rss")?;
        let command = fields.collect::<Vec<_>>().join(" ");
        if command.is_empty() {
            bail!("ps line `{line}` has no command");
        }
        members.push(ProcessRss {
            pid,
            rss: Kib(u64::from(rss)),
            command,
        });
    }
    Ok(members)
}

/// Turns `ps` output into the reading for `group`.
#[must_use]
pub fn rss_reading(output: &str, group: ProcessGroupId) -> RssReading {
    match parse_ps(output, group) {
        Err(error) => RssReading::Unavailable {
            reason: format!("{error:#}"),
        },
        Ok(processes) if processes.is_empty() => RssReading::Unavailable {
            reason: format!("process group {} has no members", group.get()),
        },
        Ok(processes) => RssReading::Measured {
            total: Kib(processes.iter().map(|p| p.rss.0).sum()),
            processes,
        },
    }
}

/// Sums the resident memory of every member of `group` via `ps`.
#[cfg(unix)]
pub async fn sample_rss(group: ProcessGroupId) -> RssReading {
    match run_ps().await {
        Ok(output) => rss_reading(&output, group),
        Err(error) => RssReading::Unavailable {
            reason: format!("{error:#}"),
        },
    }
}

/// Sums the resident memory of every member of `group` via `ps`.
#[cfg(not(unix))]
pub async fn sample_rss(_group: ProcessGroupId) -> RssReading {
    RssReading::Unavailable {
        reason: "process-group memory is sampled on unix only".to_owned(),
    }
}

#[cfg(unix)]
async fn run_ps() -> Result<String> {
    let ps = crate::pin::resolve_in_path("ps")?;
    let output = tokio::time::timeout(
        PS_TIMEOUT,
        Command::new(ps)
            .args(["-A", "-o", "pgid=,pid=,rss=,comm="])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("ps timed out")?
    .context("failed to run ps")?;
    if !output.status.success() {
        bail!("ps exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy_owned(output.stdout))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::assert_matches;

    use super::*;

    const GROUP: ProcessGroupId = ProcessGroupId::new(500);

    #[test]
    fn parses_members_of_the_group_only() {
        let output =
            "  500   500  2048 mcpls\n  500   501 40960 rust-analyzer\n    1     1   900 launchd\n";
        let members = parse_ps(output, GROUP).unwrap();
        assert_eq!(
            members.iter().map(|m| (m.pid, m.rss)).collect::<Vec<_>>(),
            [(500, Kib(2048)), (501, Kib(40_960))]
        );
        assert_eq!(
            rss_reading(output, GROUP),
            RssReading::Measured {
                total: Kib(43_008),
                processes: members
            }
        );
    }

    #[test]
    fn command_names_may_contain_spaces() {
        let members = parse_ps("500 502 1024 Google Chrome Helper\n", GROUP).unwrap();
        assert_eq!(members[0].command, "Google Chrome Helper");
    }

    #[test]
    fn malformed_lines_are_errors() {
        for line in ["500 501 notanumber cmd", "500 501", "500 501 10"] {
            assert!(parse_ps(line, GROUP).is_err(), "{line}");
        }
        assert_matches!(
            rss_reading("500 501 oops\n", GROUP),
            RssReading::Unavailable { .. }
        );
    }

    #[test]
    fn malformed_lines_of_other_processes_are_ignored() {
        let output = "garbage\n  77 notapid 5 odd\n\n500 500 2048 mcpls\n  x\n";
        let members = parse_ps(output, GROUP).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].pid, 500);
    }

    #[test]
    fn empty_group_has_no_reading() {
        assert_matches!(
            rss_reading("1 1 10 init\n", GROUP),
            RssReading::Unavailable { .. }
        );
    }

    #[cfg(unix)]
    mod unix {
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

        async fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
            for _ in 0..60 {
                if condition() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            false
        }

        #[tokio::test]
        async fn clean_exit_is_clean_only_when_the_session_closed() {
            let group = ProcessGroup::spawn(&mut sh("exit 0")).unwrap();
            assert_eq!(group.finish(true).await, ShutdownOutcome::Clean);
            let group = ProcessGroup::spawn(&mut sh("exit 0")).unwrap();
            assert_eq!(group.finish(false).await, ShutdownOutcome::Killed);
        }

        #[tokio::test]
        async fn members_outliving_the_leader_are_killed() {
            let mut group = ProcessGroup::spawn(&mut sh("sleep 30 & echo $!")).unwrap();
            let pgid = group.pgid();
            let (stdout, _stdin) = group.take_stdio().unwrap();
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).await.unwrap();
            assert!(!line.trim().is_empty());
            assert_eq!(group.finish(true).await, ShutdownOutcome::OrphansKilled);
            assert!(wait_until(|| group_is_empty(pgid)).await);
        }

        #[tokio::test]
        async fn cancelling_finish_during_the_orphan_grace_still_sweeps() {
            let mut group = ProcessGroup::spawn(&mut sh("sleep 30 & echo $!")).unwrap();
            let (stdout, _stdin) = group.take_stdio().unwrap();
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).await.unwrap();
            let member = rustix::process::Pid::from_raw(line.trim().parse().unwrap()).unwrap();
            let cancelled =
                tokio::time::timeout(Duration::from_millis(300), group.finish(true)).await;
            assert!(cancelled.is_err());
            assert!(wait_until(|| rustix::process::test_kill_process(member).is_err()).await);
        }

        #[tokio::test]
        async fn dropping_the_guard_kills_the_whole_group() {
            let mut group = ProcessGroup::spawn(&mut sh("sleep 30 & echo $!; wait")).unwrap();
            let (stdout, _stdin) = group.take_stdio().unwrap();
            let mut line = String::new();
            BufReader::new(stdout).read_line(&mut line).await.unwrap();
            let member = rustix::process::Pid::from_raw(line.trim().parse().unwrap()).unwrap();
            assert!(rustix::process::test_kill_process(member).is_ok());
            drop(group);
            assert!(wait_until(|| rustix::process::test_kill_process(member).is_err()).await);
        }

        #[test]
        fn init_and_own_group_ids_are_never_signalled() {
            assert!(ProcessGroupId::new(0).to_pid().is_none());
            assert!(ProcessGroupId::new(1).to_pid().is_none());
            assert!(ProcessGroupId::new(2).to_pid().is_some());
            assert!(kill_group(ProcessGroupId::new(1)).is_ok());
        }

        #[tokio::test]
        async fn rss_of_a_live_group_is_measured() {
            let group = ProcessGroup::spawn(&mut sh("sleep 5")).unwrap();
            let pgid = group.pgid();
            let RssReading::Measured { total, processes } = sample_rss(pgid).await else {
                panic!("ps should be available on unix");
            };
            assert!(processes.iter().any(|p| p.pid == pgid.get()));
            assert!(total.0 > 0);
            group.sweep();
            group.finish(false).await;
        }
    }
}
