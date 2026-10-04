//! Spawned LSP server process bound to the lifetime of the mcpls process.
//!
//! On Unix every server leads its own process group behind a per-server
//! `/bin/sh` watchdog that SIGKILLs the whole group once mcpls's end of its
//! stdin pipe closes, which the kernel does on any exit (clean, panic,
//! `exit(1)`, `SIGKILL`, OOM) and which dropping or terminating the
//! [`ServerProcess`] does explicitly. On Windows every server is assigned to a
//! Job Object with `KILL_ON_JOB_CLOSE`. See `specs/lsp/007-lsp-child-process-lifetime`.

use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

/// One spawned LSP server, bound to the mcpls process lifetime.
///
/// Dropping it kills the server and its whole process tree (descendants that
/// call `setsid()`/`setpgid()` on Unix excepted).
#[derive(Debug)]
pub struct ServerProcess {
    #[cfg(unix)]
    child: tokio::process::Child,
    #[cfg(unix)]
    lifeline: Option<lifeline::Lifeline>,
    #[cfg(windows)]
    child: Box<dyn process_wrap::tokio::ChildWrapper>,
}

#[cfg(unix)]
impl ServerProcess {
    /// Spawn `command` as a member of a fresh per-server watchdog group.
    ///
    /// If the watchdog cannot be started, the server is spawned unbound.
    pub(crate) fn spawn(mut command: Command) -> io::Result<Self> {
        command.kill_on_drop(true);
        lifeline::spawn(command).map(|(child, lifeline)| Self { child, lifeline })
    }

    pub(crate) const fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub(crate) const fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub(crate) const fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Wait for the leader process only, not its descendants.
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    /// Kill the leader and every descendant still in its group, waiting up to
    /// `within` for each step. Idempotent.
    pub(crate) async fn terminate_tree(&mut self, within: Duration) {
        if let Err(e) = self.child.start_kill() {
            tracing::debug!(error = %e, "leader kill signal failed during tree termination");
        }
        if let Some(lifeline) = self.lifeline.take() {
            lifeline.close(within).await;
        }
        if tokio::time::timeout(within, self.child.wait())
            .await
            .is_err()
        {
            tracing::warn!("LSP server leader did not exit after tree termination");
        }
    }

    #[cfg(test)]
    pub(crate) const fn from_unbound(child: tokio::process::Child) -> Self {
        Self {
            child,
            lifeline: None,
        }
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

    pub(crate) fn take_stderr(&mut self) -> Option<ChildStderr> {
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

#[cfg(unix)]
mod lifeline {
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;

    use tokio::process::{Child, ChildStdin, Command};
    use tracing::warn;

    /// Ignores the usual termination signals, blocks until stdin reaches EOF,
    /// then SIGKILLs its own process group (itself included).
    const WATCHDOG_SCRIPT: &str = "trap '' HUP INT TERM QUIT USR1 USR2 ALRM PIPE\nwhile read -r _; do :; done\nkill -s KILL 0\n";

    static WARNED: AtomicBool = AtomicBool::new(false);

    /// Serializes "start watchdog + spawn server" across all servers, so no
    /// server inherits another watchdog pipe's write end between `pipe()` and
    /// `FD_CLOEXEC` on platforms where std sets it non-atomically.
    ///
    /// Invariant: every child process mcpls-core spawns must be spawned while
    /// holding this lock (today only [`spawn`] spawns any). A spawn outside it
    /// could inherit a write end, and the group would then survive mcpls.
    static SPAWN_LOCK: Mutex<()> = Mutex::new(());

    /// A process group id that is only constructed from a live watchdog's pid.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct ProcessGroupId(i32);

    // TODO(#541): descendants that call `setsid()` leave the group and survive termination.
    /// One server's watchdog. It leads the server's process group, so the
    /// group id cannot be recycled while any group member lives.
    #[derive(Debug)]
    pub(super) struct Lifeline {
        watchdog: Child,
        stdin: Option<ChildStdin>,
        group: ProcessGroupId,
    }

    impl Lifeline {
        fn start() -> io::Result<Self> {
            let mut command = std::process::Command::new("/bin/sh");
            command
                .args(["-c", WATCHDOG_SCRIPT])
                .env_clear()
                .current_dir("/")
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut watchdog = Command::from(command).spawn()?;
            let group = watchdog
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .map(ProcessGroupId)
                .ok_or_else(|| io::Error::other("watchdog exited before its pid was read"))?;
            let stdin = watchdog.stdin.take();
            Ok(Self {
                watchdog,
                stdin,
                group,
            })
        }

        fn is_alive(&mut self) -> bool {
            !is_gone(&self.watchdog.try_wait())
        }

        /// Close the pipe so the watchdog SIGKILLs the group, then reap it.
        pub(super) async fn close(mut self, within: Duration) {
            drop(self.stdin.take());
            if tokio::time::timeout(within, self.watchdog.wait())
                .await
                .is_err()
            {
                warn!("LSP child watchdog did not exit after its pipe closed");
            }
        }
    }

    /// Only a reaped watchdog is gone: a failed status query must not be read
    /// as the watchdog having died.
    const fn is_gone(status: &io::Result<Option<std::process::ExitStatus>>) -> bool {
        matches!(status, Ok(Some(_)))
    }

    fn start_or_warn() -> Option<Lifeline> {
        match Lifeline::start() {
            Ok(lifeline) => Some(lifeline),
            Err(e) => {
                if !WARNED.swap(true, Ordering::Relaxed) {
                    warn!(error = %e, "failed to start LSP child watchdog; servers are spawned unbound");
                }
                None
            }
        }
    }

    /// Spawn `command` as a member of a fresh per-server watchdog group.
    pub(super) fn spawn(command: Command) -> io::Result<(Child, Option<Lifeline>)> {
        let _guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        spawn_with(command, start_or_warn())
    }

    /// Spawn `command` into `lifeline`'s group; the watchdog is dropped (and
    /// therefore exits) on every spawn error, so failing spawns never leak it.
    fn spawn_with(
        mut command: Command,
        mut lifeline: Option<Lifeline>,
    ) -> io::Result<(Child, Option<Lifeline>)> {
        match spawn_into(&mut command, lifeline.as_ref()) {
            // EPERM from `setpgid` and EACCES from `exec` are both PermissionDenied;
            // a dead watchdog means the former.
            Err(e)
                if e.kind() == io::ErrorKind::PermissionDenied
                    && lifeline.as_mut().is_some_and(|l| !l.is_alive()) =>
            {
                warn!(error = %e, "LSP child watchdog group vanished; restarting it");
                lifeline = start_or_warn();
                spawn_into(&mut command, lifeline.as_ref()).map(|child| (child, lifeline))
            }
            Ok(child) => Ok((child, lifeline)),
            Err(e) => Err(e),
        }
    }

    fn spawn_into(command: &mut Command, lifeline: Option<&Lifeline>) -> io::Result<Child> {
        command.process_group(lifeline.map_or(0, |l| l.group.0));
        command.spawn()
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
        async fn test_close_reaps_watchdog_and_kills_group_members() {
            let lifeline = Lifeline::start().unwrap();
            let mut member = sleeper(lifeline.group).spawn().unwrap();

            lifeline.close(Duration::from_secs(5)).await;

            let status = timeout(Duration::from_secs(5), member.wait())
                .await
                .expect("group member must be dead once close returns")
                .unwrap();
            assert_eq!(status.signal(), Some(9));
        }

        #[tokio::test]
        async fn test_joining_a_vanished_group_fails_and_spawn_starts_a_fresh_one() {
            let mut leader = std::process::Command::new("sh");
            leader.args(["-c", "exit 0"]).process_group(0);
            let mut leader = Command::from(leader).spawn().unwrap();
            let dead = ProcessGroupId(i32::try_from(leader.id().unwrap()).unwrap());
            leader.wait().await.unwrap();
            let stale = Lifeline {
                watchdog: leader,
                stdin: None,
                group: dead,
            };

            let mut command = sleeper(dead);
            let first = spawn_into(&mut command, Some(&stale));
            assert_eq!(
                first.expect_err("joining a vanished group fails").kind(),
                io::ErrorKind::PermissionDenied
            );
            drop(stale);

            let (member, lifeline) =
                spawn(sleeper(dead)).expect("a per-server spawn never joins a stale group");
            assert_ne!(lifeline.as_ref().unwrap().group.0, dead.0);
            drop(member);
        }

        #[tokio::test]
        async fn test_non_executable_server_fails_with_permission_denied() {
            let dir = tempfile::tempdir().unwrap();
            let not_executable = dir.path().join("server");
            std::fs::write(&not_executable, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&not_executable, std::fs::Permissions::from_mode(0o644))
                .unwrap();

            let err = spawn(Command::new(&not_executable))
                .expect_err("a non-executable file must not spawn");

            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        }

        #[tokio::test]
        async fn test_repeated_failed_spawns_leave_no_watchdog_behind() {
            let mut pids = Vec::new();
            for _ in 0..10 {
                let lifeline = Lifeline::start().unwrap();
                pids.push(lifeline.watchdog.id().unwrap());
                spawn_with(Command::new("mcpls-test-missing-server"), Some(lifeline))
                    .expect_err("a missing binary must not spawn");
            }

            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while pids.iter().any(|pid| pid_is_running(*pid)) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "an idle watchdog survived a failed spawn"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        #[tokio::test]
        async fn test_failed_spawn_does_not_leak_the_idle_watchdog() {
            let lifeline = Lifeline::start().unwrap();
            let watchdog_pid = lifeline.watchdog.id().unwrap();

            spawn_with(Command::new("mcpls-test-missing-server"), Some(lifeline))
                .expect_err("a missing binary must not spawn");

            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while pid_is_running(watchdog_pid) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the idle watchdog survived a failed spawn"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        fn pid_is_running(pid: u32) -> bool {
            let output = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let stat = String::from_utf8_lossy(&output.stdout);
            let stat = stat.trim();
            !stat.is_empty() && !stat.starts_with('Z')
        }

        #[test]
        fn test_failed_status_query_does_not_count_as_gone() {
            assert!(!is_gone(&Err(io::Error::from(io::ErrorKind::Other))));
            assert!(!is_gone(&Ok(None)));
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::process::Stdio;

    use tokio::io::{AsyncBufReadExt, BufReader};

    use super::*;

    /// Spawn a server script that starts a background `sleep` in its group and
    /// prints the sleeper's pid, returning the process and that pid.
    async fn spawn_server_with_grandchild() -> (ServerProcess, u32) {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 600 & echo $!; wait"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        let mut process = ServerProcess::spawn(command).unwrap();
        let stdout = process.take_stdout().unwrap();
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).await.unwrap();
        (process, line.trim().parse().unwrap())
    }

    fn pid_is_running(pid: u32) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&output.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
    }

    async fn wait_until_gone(pid: u32) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while pid_is_running(pid) {
            assert!(tokio::time::Instant::now() < deadline, "pid {pid} survived");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn test_terminate_tree_kills_descendants() {
        let (mut process, grandchild) = spawn_server_with_grandchild().await;
        assert!(pid_is_running(grandchild));

        process.terminate_tree(Duration::from_secs(5)).await;

        wait_until_gone(grandchild).await;
        assert!(process.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn test_dropping_server_process_kills_descendants() {
        let (process, grandchild) = spawn_server_with_grandchild().await;

        drop(process);

        wait_until_gone(grandchild).await;
    }

    #[tokio::test]
    async fn test_terminating_one_server_leaves_the_other_servers_descendants_alive() {
        let (mut first, first_grandchild) = spawn_server_with_grandchild().await;
        let (second, second_grandchild) = spawn_server_with_grandchild().await;

        first.terminate_tree(Duration::from_secs(5)).await;

        wait_until_gone(first_grandchild).await;
        assert!(pid_is_running(second_grandchild));
        drop(second);
        wait_until_gone(second_grandchild).await;
    }

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

    async fn spawn_with_grandchild() -> (ServerProcess, u32) {
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
        (process, pid)
    }

    async fn wait_until_unlisted(pid: u32, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_listed(pid).await {
            assert!(Instant::now() < deadline, "grandchild survived {what}");
            sleep(Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    async fn test_dropping_server_process_kills_grandchild() {
        let (process, pid) = spawn_with_grandchild().await;

        drop(process);

        wait_until_unlisted(pid, "the job close").await;
    }

    #[tokio::test]
    async fn test_terminate_tree_kills_grandchild() {
        let (mut process, pid) = spawn_with_grandchild().await;

        process.terminate_tree(Duration::from_secs(5)).await;

        wait_until_unlisted(pid, "tree termination").await;
        assert!(process.try_wait().unwrap().is_some());
    }
}
