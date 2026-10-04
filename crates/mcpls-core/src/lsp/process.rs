//! Spawned LSP server process bound to the lifetime of the mcpls process.
//!
//! On Unix every server gets its own lifeline (see `lifeline`): an anchor
//! that leads the server's process group and a watchdog that, once mcpls's end
//! of its socket closes (which the kernel does on any exit, clean, panic,
//! `exit(1)`, `SIGKILL`, OOM), freezes the group, finds descendants that left
//! it through `setsid`/`setpgid`, and SIGKILLs the whole tree. On Windows every
//! server is assigned to a Job Object with `KILL_ON_JOB_CLOSE`. See
//! `specs/lsp/007-lsp-child-process-lifetime`.

use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::{ChildStdin, ChildStdout, Command};

#[cfg(unix)]
mod lifeline;

/// Upper bound on how long dropping or terminating a server may take to sweep
/// its whole process tree.
///
/// Callers that wait for descendants to disappear after killing mcpls (for
/// example a benchmark harness) should allow at least this long.
pub const LIFELINE_SWEEP_BUDGET: Duration = Duration::from_secs(8);

/// Whether an out-of-process watchdog guards a server.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// A live watchdog sweeps the server's tree if mcpls dies, so the server
    /// must not be handed the mcpls pid to watch.
    Bound,
    /// No watchdog: the server sees the real mcpls pid.
    Unbound,
}

/// Why a server must not be allowed to exit gracefully after a mark.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkFailure {
    /// The watchdog could not be reached or ended its socket first.
    Unreachable,
    /// The watchdog did not confirm in time.
    NoConfirmation,
    /// The watchdog's process snapshot timed out, so escapees may be unmarked.
    SnapshotTimedOut,
}

/// Result of freezing and recording a server's escapees before `exit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkOutcome {
    /// The escapees are recorded (or there are none to record): `exit` may go.
    Confirmed,
    /// `exit` must be withheld and the tree terminated at once.
    #[cfg(unix)]
    Failed(MarkFailure),
}

/// Read end of a server's stderr.
#[cfg(unix)]
pub type ServerStderr = tokio::net::unix::pipe::Receiver;
/// Read end of a server's stderr.
#[cfg(windows)]
pub type ServerStderr = tokio::process::ChildStderr;

/// One spawned LSP server, bound to the mcpls process lifetime.
///
/// Dropping it sweeps the server's whole process tree (on Unix asynchronously,
/// through the watchdog; on Windows through the job).
#[derive(Debug)]
pub struct ServerProcess {
    #[cfg(unix)]
    child: Option<tokio::process::Child>,
    #[cfg(unix)]
    stderr: Option<ServerStderr>,
    #[cfg(unix)]
    lifeline: Option<lifeline::Lifeline>,
    #[cfg(windows)]
    child: Box<dyn process_wrap::tokio::ChildWrapper>,
}

#[cfg(unix)]
impl ServerProcess {
    /// Spawn `command` bound to a fresh per-server lifeline.
    ///
    /// If the lifeline cannot be started, the server is spawned unbound in its
    /// own process group.
    pub(crate) fn spawn(command: Command) -> io::Result<Self> {
        Self::spawn_with(command, &lifeline::Options::default())
    }

    fn spawn_with(command: Command, options: &lifeline::Options) -> io::Result<Self> {
        let spawned = lifeline::spawn(command, options)?;
        Ok(Self {
            child: Some(spawned.child),
            stderr: Some(spawned.stderr),
            lifeline: spawned.lifeline,
        })
    }

    /// Whether an out-of-process watchdog still guards this server.
    pub(crate) fn binding(&mut self) -> Binding {
        self.lifeline
            .as_mut()
            .map_or(Binding::Unbound, lifeline::Lifeline::binding)
    }

    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.as_mut()?.stdin.take()
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    pub(crate) const fn take_stderr(&mut self) -> Option<ServerStderr> {
        self.stderr.take()
    }

    /// Non-blocking status check of the leader. Once the leader is gone its
    /// remaining tree is swept at once, since nothing can use it any more.
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.leader()?.try_wait()?;
        if let Some(lifeline) = &mut self.lifeline {
            lifeline.warn_if_watchdog_lost();
        }
        if status.is_some()
            && let Some(mut lifeline) = self.lifeline.take()
        {
            lifeline.forget_leader();
            lifeline.release(None);
        }
        Ok(status)
    }

    /// Wait for the leader process only, not its descendants.
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.leader()?.wait().await?;
        if let Some(lifeline) = &mut self.lifeline {
            lifeline.forget_leader();
        }
        Ok(status)
    }

    /// Freeze and record the descendants that escaped the server's process
    /// group, before the server is told to exit. Anything but
    /// [`MarkOutcome::Confirmed`] means the server must not be allowed to exit
    /// on its own: the caller should [`Self::terminate_tree`] it instead.
    pub(crate) async fn mark_escapees(&mut self) -> MarkOutcome {
        match &mut self.lifeline {
            Some(lifeline) => lifeline.mark().await,
            None => MarkOutcome::Confirmed,
        }
    }

    /// Kill the leader and the server's whole process tree (including
    /// descendants that left its group) and wait for it to be gone, up to
    /// `within` (at most [`LIFELINE_SWEEP_BUDGET`] is ever useful); after that
    /// whatever the watchdog has not finished is killed from here. A shorter
    /// budget trades attribution time for a lower worst case. Idempotent: the
    /// reaped leader stays in place, so `try_wait` keeps reporting its status.
    pub(crate) async fn terminate_tree(&mut self, within: Duration) {
        if let Some(lifeline) = self.lifeline.take() {
            self.child = lifeline.terminate(self.child.take(), within).await;
        } else if let Some(child) = self.child.as_mut() {
            if let Err(e) = child.start_kill() {
                tracing::debug!(error = %e, "leader kill signal failed during tree termination");
            }
            if tokio::time::timeout(within, child.wait()).await.is_err() {
                tracing::warn!("LSP server leader did not exit after tree termination");
            }
        }
    }

    fn leader(&mut self) -> io::Result<&mut tokio::process::Child> {
        self.child
            .as_mut()
            .ok_or_else(|| io::Error::other("server process already torn down"))
    }

    #[cfg(test)]
    pub(crate) const fn from_unbound(child: tokio::process::Child) -> Self {
        Self {
            child: Some(child),
            stderr: None,
            lifeline: None,
        }
    }

    #[cfg(test)]
    pub(crate) async fn kill(&mut self) -> io::Result<()> {
        self.leader()?.kill().await
    }
}

#[cfg(unix)]
impl Drop for ServerProcess {
    fn drop(&mut self) {
        if let Some(lifeline) = self.lifeline.take() {
            lifeline.release(self.child.take());
        }
    }
}

#[cfg(windows)]
impl ServerProcess {
    /// Spawn `command` inside a fresh Job Object that kills on close.
    pub(crate) fn spawn(command: Command) -> io::Result<Self> {
        use process_wrap::tokio::{CommandWrap, JobObject, KillOnDrop};

        let mut wrapped = CommandWrap::from(command);
        wrapped.wrap(JobObject).wrap(KillOnDrop);
        wrapped.spawn().map(|child| Self { child })
    }

    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin().take()
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout().take()
    }

    pub(crate) fn take_stderr(&mut self) -> Option<ServerStderr> {
        self.child.stderr().take()
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Wait for the leader process only: the job wrapper's own `wait` blocks
    /// until every process in the job is gone.
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.inner_mut().wait().await
    }

    /// Terminate the whole job, waiting up to `within` for the leader to exit.
    pub(crate) async fn terminate_tree(&mut self, within: Duration) {
        if let Err(e) = self.child.start_kill() {
            tracing::debug!(error = %e, "job termination failed during tree termination");
        }
        if tokio::time::timeout(within, self.child.inner_mut().wait())
            .await
            .is_err()
        {
            tracing::warn!("LSP server leader did not exit after tree termination");
        }
    }
}

#[cfg(all(test, windows))]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::time::{Instant, sleep};

    use super::*;

    async fn pid_listed(pid: u32) -> bool {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .await
            .unwrap();
        String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
    }

    #[tokio::test]
    async fn test_dropping_server_process_kills_grandchild() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let script = format!(
            "$p=Start-Process ping -ArgumentList '-n','600','127.0.0.1' -PassThru \
             -WindowStyle Hidden; Set-Content -Path '{}' -Value $p.Id; Start-Sleep 600",
            pid_file.display()
        );
        let mut command = Command::new("powershell");
        command
            .args(["-NoProfile", "-Command", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let process = ServerProcess::spawn(command).unwrap();

        let deadline = Instant::now() + Duration::from_secs(30);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "grandchild pid file never appeared"
            );
            sleep(Duration::from_millis(100)).await;
        };
        assert!(pid_listed(pid).await);

        drop(process);

        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_listed(pid).await {
            assert!(
                Instant::now() < deadline,
                "grandchild survived the job close"
            );
            sleep(Duration::from_millis(100)).await;
        }
    }
}
