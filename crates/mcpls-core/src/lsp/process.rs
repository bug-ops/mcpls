//! Spawned LSP server process bound to the lifetime of the mcpls process.
//!
//! On Unix every server joins the process group of a single `/bin/sh`
//! watchdog that SIGKILLs the whole group once mcpls's end of its stdin pipe
//! closes, which the kernel does on any exit (clean, panic, `exit(1)`,
//! `SIGKILL`, OOM). On Windows every server is assigned to a Job Object with
//! `KILL_ON_JOB_CLOSE`. See `specs/lsp/007-lsp-child-process-lifetime`.

use std::io;
use std::process::ExitStatus;

use tokio::process::{ChildStdin, ChildStdout, Command};

/// One spawned LSP server, bound to the mcpls process lifetime.
///
/// Dropping it kills the server (and, on Windows, its whole process tree).
#[derive(Debug)]
pub struct ServerProcess {
    #[cfg(unix)]
    child: tokio::process::Child,
    #[cfg(windows)]
    child: Box<dyn process_wrap::tokio::ChildWrapper>,
}

#[cfg(unix)]
impl ServerProcess {
    /// Spawn `command` into the lifeline process group.
    ///
    /// If the watchdog cannot be started, the server is spawned unbound.
    pub(crate) fn spawn(mut command: Command) -> io::Result<Self> {
        command.kill_on_drop(true);
        lifeline::spawn(&lifeline::LIFELINE, command).map(|child| Self { child })
    }

    pub(crate) const fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub(crate) const fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Wait for the leader process only, not its descendants.
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    #[cfg(test)]
    pub(crate) const fn from_unbound(child: tokio::process::Child) -> Self {
        Self { child }
    }

    #[cfg(test)]
    pub(crate) async fn kill(&mut self) -> io::Result<()> {
        self.child.kill().await
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

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Wait for the leader process only: the job wrapper's own `wait` blocks
    /// until every process in the job is gone.
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.inner_mut().wait().await
    }
}

#[cfg(unix)]
mod lifeline {
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, PoisonError};

    use tokio::process::Command;
    use tracing::warn;

    /// Ignores the usual termination signals, blocks until stdin reaches EOF,
    /// then SIGKILLs its own process group (itself included).
    const WATCHDOG_SCRIPT: &str = "trap '' HUP INT TERM QUIT USR1 USR2 ALRM PIPE\nwhile read -r _; do :; done\nkill -s KILL 0\n";

    static WARNED: AtomicBool = AtomicBool::new(false);

    /// Process-wide watchdog; never dropped while valid, since dropping it
    /// closes the pipe and kills every server in its group.
    pub(super) static LIFELINE: Mutex<Option<Lifeline>> = Mutex::new(None);

    /// A process group id that is only constructed from a live watchdog's pid.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct ProcessGroupId(i32);

    /// The watchdog child, which holds the pipe whose write end only mcpls has.
    #[derive(Debug)]
    pub(super) struct Lifeline {
        watchdog: Child,
        group: ProcessGroupId,
    }

    impl Lifeline {
        fn start() -> io::Result<Self> {
            let watchdog = std::process::Command::new("/bin/sh")
                .args(["-c", WATCHDOG_SCRIPT])
                .env_clear()
                .current_dir("/")
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let group = i32::try_from(watchdog.id())
                .map(ProcessGroupId)
                .map_err(io::Error::other)?;
            Ok(Self { watchdog, group })
        }

        fn is_alive(&mut self) -> bool {
            !is_gone(&self.watchdog.try_wait())
        }
    }

    /// Only a reaped watchdog is gone: a failed status query must not replace a
    /// possibly live lifeline, since dropping it SIGKILLs every running server.
    const fn is_gone(status: &io::Result<Option<std::process::ExitStatus>>) -> bool {
        matches!(status, Ok(Some(_)))
    }

    /// Returns the group of a live watchdog, starting a replacement if needed.
    fn ensure_group(slot: &mut Option<Lifeline>) -> Option<ProcessGroupId> {
        if slot.as_mut().is_some_and(Lifeline::is_alive) {
            return slot.as_ref().map(|l| l.group);
        }
        match Lifeline::start() {
            Ok(lifeline) => {
                let group = lifeline.group;
                *slot = Some(lifeline);
                Some(group)
            }
            Err(e) => {
                *slot = None;
                if !WARNED.swap(true, Ordering::Relaxed) {
                    warn!(error = %e, "failed to start LSP child watchdog; servers are spawned unbound");
                }
                None
            }
        }
    }

    /// Spawn `command` into the lifeline group under `slot`'s lock, so no
    /// server inherits the watchdog pipe's write end between `pipe()` and
    /// `FD_CLOEXEC` on platforms where std sets it non-atomically.
    pub(super) fn spawn(
        slot: &Mutex<Option<Lifeline>>,
        command: Command,
    ) -> io::Result<tokio::process::Child> {
        let mut guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
        let group = ensure_group(&mut guard);
        let spawned = spawn_bound(&mut guard, group, command);
        drop(guard);
        spawned
    }

    fn spawn_bound(
        slot: &mut Option<Lifeline>,
        group: Option<ProcessGroupId>,
        mut command: Command,
    ) -> io::Result<tokio::process::Child> {
        command.process_group(group.map_or(0, |g| g.0));
        match command.spawn() {
            // EPERM from `setpgid` and EACCES from `exec` are both PermissionDenied;
            // a live watchdog means the latter, and discarding it would kill the other servers.
            Err(e)
                if e.kind() == io::ErrorKind::PermissionDenied
                    && group.is_some()
                    && slot.as_mut().is_some_and(|l| !l.is_alive()) =>
            {
                warn!(error = %e, "LSP child watchdog group vanished; restarting it");
                *slot = None;
                command.process_group(ensure_group(slot).map_or(0, |g| g.0));
                command.spawn()
            }
            other => other,
        }
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    mod tests {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::process::ExitStatusExt;
        use std::time::Duration;

        use tokio::time::timeout;

        use super::*;

        fn sleeper(group: ProcessGroupId) -> Command {
            let mut command = Command::new("sleep");
            command.arg("600").process_group(group.0).kill_on_drop(true);
            command
        }

        #[tokio::test]
        async fn test_closing_lifeline_pipe_kills_group_members() {
            let lifeline = Lifeline::start().unwrap();
            let mut member = sleeper(lifeline.group).spawn().unwrap();

            drop(lifeline);

            let status = timeout(Duration::from_secs(5), member.wait())
                .await
                .expect("group member must die within 5s of the pipe closing")
                .unwrap();
            assert_eq!(status.signal(), Some(9));
        }

        #[tokio::test]
        async fn test_spawn_retries_once_when_group_vanished() {
            let mut leader = std::process::Command::new("sh")
                .args(["-c", "exit 0"])
                .process_group(0)
                .spawn()
                .unwrap();
            let dead = ProcessGroupId(i32::try_from(leader.id()).unwrap());
            leader.wait().unwrap();
            let mut slot = Some(Lifeline {
                watchdog: leader,
                group: dead,
            });

            let member = spawn_bound(&mut slot, Some(dead), sleeper(dead))
                .expect("spawn must succeed after the FR-007 retry");

            let fresh = slot.as_ref().expect("a replacement watchdog is installed");
            assert_ne!(fresh.group.0, dead.0);
            drop(member);
        }

        #[tokio::test]
        async fn test_permission_denied_with_live_watchdog_keeps_lifeline() {
            let dir = tempfile::tempdir().unwrap();
            let not_executable = dir.path().join("server");
            std::fs::write(&not_executable, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&not_executable, std::fs::Permissions::from_mode(0o644))
                .unwrap();
            let lifeline = Lifeline::start().unwrap();
            let group = lifeline.group;
            let mut slot = Some(lifeline);

            let err = spawn_bound(&mut slot, Some(group), Command::new(&not_executable))
                .expect_err("a non-executable file must not spawn");

            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            let kept = slot.as_mut().unwrap();
            assert!(kept.is_alive());
            assert_eq!(kept.group.0, group.0);
        }

        #[test]
        fn test_failed_status_query_does_not_count_as_gone() {
            assert!(!is_gone(&Err(io::Error::from(io::ErrorKind::Other))));
            assert!(!is_gone(&Ok(None)));
        }

        #[tokio::test]
        async fn test_spawn_replaces_reaped_watchdog_before_spawning() {
            let mut leader = std::process::Command::new("sh")
                .args(["-c", "exit 0"])
                .process_group(0)
                .spawn()
                .unwrap();
            let dead = ProcessGroupId(i32::try_from(leader.id()).unwrap());
            leader.wait().unwrap();
            let slot = Mutex::new(Some(Lifeline {
                watchdog: leader,
                group: dead,
            }));

            let member = spawn(&slot, sleeper(dead)).expect("FR-005: a fresh watchdog is started");

            let fresh = slot.lock().unwrap();
            assert_ne!(fresh.as_ref().unwrap().group.0, dead.0);
            drop(fresh);
            drop(member);
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::process::Stdio;

    use super::*;

    #[tokio::test]
    async fn test_spawn_exposes_pipes_and_leader_exit() {
        let mut command = Command::new("true");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);

        let mut process = ServerProcess::spawn(command).unwrap();

        assert!(process.take_stdin().is_some());
        assert!(process.take_stdout().is_some());
        assert!(process.take_stdin().is_none());
        assert!(process.wait().await.unwrap().success());
        assert!(process.try_wait().unwrap().is_some());
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
