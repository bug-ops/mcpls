use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::time::Instant;

use tempfile::TempDir;
use tokio::time::sleep;

use super::*;
use crate::lsp::process::ServerProcess;

const POLL: Duration = Duration::from_millis(50);

/// Leader that exits on stdin EOF (like real servers), writes to stdout
/// continuously, keeps an in-group helper and a `setsid` grandchild.
const EOF_EXITING_SERVER: &str = r#"
perl -MPOSIX -e 'POSIX::setsid(); exec "sleep", "601"' & echo $! > "$0/escapee.tmp"; mv "$0/escapee.tmp" "$0/escapee"
sleep 602 & echo $! > "$0/helper.tmp"; mv "$0/helper.tmp" "$0/helper"
(while :; do echo '{}'; sleep 0.001; done) &
while read -r _; do :; done
exit 0
"#;

fn server_command(script: &str, dir: &Path) -> Command {
    let mut command = Command::new("sh");
    command
        .args(["-c", script])
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    command
}

async fn read_pid(path: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        sleep(POLL).await;
    }
}

fn pid_state(pid: i32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let state = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    (!state.is_empty() && !state.starts_with('Z')).then_some(state)
}

fn pgid_of(pid: i32) -> Option<i32> {
    let output = std::process::Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .ok()
}

/// Waits until `pid` leads its own group, i.e. its `setsid` has run.
async fn wait_for_setsid(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while pgid_of(pid) != Some(pid) {
        assert!(Instant::now() < deadline, "pid {pid} never called setsid");
        sleep(Duration::from_millis(10)).await;
    }
}

fn is_stopped(pid: i32) -> bool {
    pid_state(pid).is_some_and(|state| state.starts_with('T'))
}

fn kill_leftover(pid: i32) {
    if let Some(pid) = Pid::from_raw(pid) {
        Escapee::Process(pid).kill();
    }
}

async fn assert_gone_within_budget(pids: &[i32]) {
    let deadline = Instant::now() + LIFELINE_SWEEP_BUDGET;
    for pid in pids {
        while let Some(state) = pid_state(*pid) {
            assert!(
                Instant::now() < deadline,
                "pid {pid} (state {state}) outlived the sweep budget"
            );
            sleep(POLL).await;
        }
    }
}

struct Fixture {
    _dir: TempDir,
    process: ServerProcess,
    leader: i32,
    escapee: i32,
    helper: i32,
}

impl Fixture {
    async fn start(options: &Options) -> Self {
        Self::start_with(EOF_EXITING_SERVER, options).await
    }

    async fn start_with(script: &str, options: &Options) -> Self {
        let dir = TempDir::new().unwrap();
        let process =
            ServerProcess::spawn_with(server_command(script, dir.path()), options).unwrap();
        let leader = process
            .child
            .as_ref()
            .unwrap()
            .id()
            .unwrap()
            .try_into()
            .unwrap();
        let escapee = read_pid(&dir.path().join("escapee")).await;
        let helper = read_pid(&dir.path().join("helper")).await;
        wait_for_setsid(escapee).await;
        Self {
            _dir: dir,
            process,
            leader,
            escapee,
            helper,
        }
    }

    fn watchdog_mut(&mut self) -> &mut Watchdog {
        self.process
            .lifeline
            .as_mut()
            .unwrap()
            .watchdog
            .as_mut()
            .unwrap()
    }

    fn all(&self) -> [i32; 3] {
        [self.leader, self.escapee, self.helper]
    }
}

fn hanging_ps(dir: &Path) -> Options {
    let ps = dir.join("hanging-ps");
    std::fs::write(&ps, "#!/bin/sh\nexec sleep 12\n").unwrap();
    std::fs::set_permissions(&ps, std::fs::Permissions::from_mode(0o755)).unwrap();
    Options {
        ps: ps.into_os_string(),
        scan_timeout: Duration::from_millis(300),
        ..Options::default()
    }
}

#[tokio::test]
async fn test_sweep_works_under_every_available_posix_shell() {
    for shell in ["/bin/dash", "/usr/bin/dash", "/bin/bash"] {
        if !Path::new(shell).exists() {
            continue;
        }
        let options = Options {
            watchdog_shell: PathBuf::from(shell),
            ..Options::default()
        };
        let mut fixture = Fixture::start(&options).await;
        assert_eq!(
            fixture.process.mark_escapees().await,
            MarkOutcome::Confirmed,
            "{shell}"
        );
        assert!(is_stopped(fixture.escapee), "{shell}");
        let pids = fixture.all();

        drop(fixture.process);

        assert_gone_within_budget(&pids).await;
    }
}

#[tokio::test]
async fn test_drop_sweeps_setsid_escapee_of_eof_exiting_server() {
    let fixture = Fixture::start(&Options::default()).await;
    let pids = fixture.all();
    for pid in pids {
        assert!(pid_state(pid).is_some(), "pid {pid} must start alive");
    }

    drop(fixture.process);

    assert_gone_within_budget(&pids).await;
}

#[tokio::test]
async fn test_terminate_returns_after_the_tree_is_gone() {
    let mut fixture = Fixture::start(&Options::default()).await;
    let pids = fixture.all();

    let started = Instant::now();
    fixture.process.terminate_tree(LIFELINE_SWEEP_BUDGET).await;

    assert!(started.elapsed() < LIFELINE_SWEEP_BUDGET);
    for pid in pids {
        assert!(pid_state(pid).is_none(), "pid {pid} survived terminate");
    }
}

#[tokio::test]
async fn test_leader_that_called_setsid_is_still_killed_on_drop() {
    let dir = TempDir::new().unwrap();
    let process = ServerProcess::spawn_with(
        server_command(
            r#"exec perl -MPOSIX -e 'POSIX::setsid(); exec "sleep", "603"'"#,
            dir.path(),
        ),
        &Options::default(),
    )
    .unwrap();
    let leader: i32 = process
        .child
        .as_ref()
        .unwrap()
        .id()
        .unwrap()
        .try_into()
        .unwrap();
    wait_for_setsid(leader).await;

    drop(process);

    assert_gone_within_budget(&[leader]).await;
}

#[tokio::test]
async fn test_helpers_of_a_crashed_leader_are_swept_when_the_crash_is_noticed() {
    let dir = TempDir::new().unwrap();
    let mut process = ServerProcess::spawn_with(
        server_command(
            r#"sleep 604 & echo $! > "$0/helper.tmp"; mv "$0/helper.tmp" "$0/helper"; exit 0"#,
            dir.path(),
        ),
        &Options::default(),
    )
    .unwrap();
    let helper = read_pid(&dir.path().join("helper")).await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while process.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "leader never exited");
        sleep(POLL).await;
    }

    assert_gone_within_budget(&[helper]).await;
    assert!(process.lifeline.is_none());
}

#[tokio::test]
async fn test_reaping_the_leader_tells_the_watchdog_to_forget_it() {
    let dir = TempDir::new().unwrap();
    let mut process =
        ServerProcess::spawn_with(server_command("exit 0", dir.path()), &Options::default())
            .unwrap();

    process.wait().await.unwrap();

    assert!(process.lifeline.as_ref().unwrap().leader_forgotten());
}

/// A pid the watchdog was told about but that now belongs to an unrelated
/// process must survive the sweep once the leader was forgotten.
async fn sweep_with_stale_leader(forget: bool, options: &Options) -> bool {
    let mut stale = std::process::Command::new("sleep")
        .arg("605")
        .process_group(0)
        .spawn()
        .unwrap();
    let stale_pid = Pid::from_raw(stale.id().try_into().unwrap()).unwrap();
    let (stderr_hold, _unused) = std::io::pipe().unwrap();
    let anchor = Anchor::start(&stderr_hold).unwrap();
    let watchdog = Watchdog::start(
        &anchor,
        stale_pid,
        ServerFds {
            stdin: None,
            stdout: None,
        },
        options,
    )
    .unwrap();
    let mut lifeline = Lifeline {
        anchor,
        watchdog: Some(watchdog),
        leader_forgotten: false,
        lost_warned: false,
    };
    if forget {
        lifeline.forget_leader();
    }

    lifeline.terminate(None, LIFELINE_SWEEP_BUDGET).await;

    let survived = stale.try_wait().unwrap().is_none();
    stale.kill().unwrap();
    stale.wait().unwrap();
    survived
}

#[tokio::test]
async fn test_watchdog_kills_a_still_registered_leader_tree() {
    assert!(!sweep_with_stale_leader(false, &Options::default()).await);
}

#[tokio::test]
async fn test_watchdog_spares_a_pid_whose_leader_was_forgotten() {
    assert!(sweep_with_stale_leader(true, &Options::default()).await);
}

#[tokio::test]
async fn test_mark_freezes_escapees_but_lets_the_server_run() {
    let mut fixture = Fixture::start(&Options::default()).await;

    assert_eq!(
        fixture.process.mark_escapees().await,
        MarkOutcome::Confirmed
    );

    let escapee = Pid::from_raw(fixture.escapee).unwrap();
    assert_eq!(
        fixture.watchdog_mut().channel.targets,
        [Escapee::Group(escapee)]
    );
    assert!(is_stopped(fixture.escapee));
    assert!(!is_stopped(fixture.leader));
    assert!(!is_stopped(fixture.helper));

    let pids = fixture.all();
    fixture.process.terminate_tree(LIFELINE_SWEEP_BUDGET).await;
    for pid in pids {
        assert!(pid_state(pid).is_none(), "pid {pid} survived terminate");
    }
}

#[tokio::test]
async fn test_mcpls_dying_after_mark_leaves_nothing_stopped() {
    let mut fixture = Fixture::start(&Options::default()).await;
    assert_eq!(
        fixture.process.mark_escapees().await,
        MarkOutcome::Confirmed
    );
    assert!(is_stopped(fixture.escapee));
    let pids = fixture.all();

    drop(fixture.process);

    assert_gone_within_budget(&pids).await;
}

#[tokio::test]
async fn test_mark_timeout_reports_failure_and_terminate_leaves_nothing_stopped() {
    let dir = TempDir::new().unwrap();
    let options = hanging_ps(dir.path());
    let mut fixture = Fixture::start(&options).await;

    let confirmed = fixture
        .process
        .lifeline
        .as_mut()
        .unwrap()
        .mark_within(Duration::from_millis(100))
        .await;
    assert_eq!(confirmed, MarkOutcome::Failed(MarkFailure::NoConfirmation));

    let in_group = [fixture.leader, fixture.helper];
    fixture.process.terminate_tree(LIFELINE_SWEEP_BUDGET).await;
    for pid in in_group {
        assert!(pid_state(pid).is_none(), "pid {pid} survived terminate");
    }
    assert!(!is_stopped(fixture.escapee));
    kill_leftover(fixture.escapee);
}

#[tokio::test]
async fn test_hung_ps_still_kills_the_group_within_the_budget() {
    let dir = TempDir::new().unwrap();
    let options = hanging_ps(dir.path());
    let fixture = Fixture::start(&options).await;
    let in_group = [fixture.leader, fixture.helper];

    let started = Instant::now();
    drop(fixture.process);

    assert_gone_within_budget(&in_group).await;
    assert!(started.elapsed() < LIFELINE_SWEEP_BUDGET);
    kill_leftover(fixture.escapee);
}

#[tokio::test]
async fn test_watchdog_spawn_failure_leaves_an_unbound_server_that_drop_still_kills() {
    let options = Options {
        watchdog_shell: PathBuf::from("/nonexistent/sh"),
        ..Options::default()
    };
    let mut fixture = Fixture::start(&options).await;
    assert_eq!(fixture.process.binding(), Binding::Unbound);
    let anchor = fixture
        .process
        .lifeline
        .as_ref()
        .unwrap()
        .anchor
        .group
        .as_raw_pid();
    let in_group = [fixture.leader, fixture.helper, anchor];

    drop(fixture.process);

    assert_gone_within_budget(&in_group).await;
    kill_leftover(fixture.escapee);
}

#[tokio::test]
async fn test_drop_after_the_watchdog_was_killed_falls_back_to_killing_by_hand() {
    let mut fixture = Fixture::start(&Options::default()).await;
    assert_eq!(
        fixture.process.mark_escapees().await,
        MarkOutcome::Confirmed
    );
    let anchor = fixture
        .process
        .lifeline
        .as_ref()
        .unwrap()
        .anchor
        .group
        .as_raw_pid();
    let pids = [fixture.leader, fixture.escapee, fixture.helper, anchor];
    let watchdog = fixture.watchdog_mut();
    watchdog.child.kill().await.unwrap();

    drop(fixture.process);

    assert_gone_within_budget(&pids).await;
}

#[tokio::test]
async fn test_bound_server_reports_the_lifeline_and_unbound_does_not() {
    let dir = TempDir::new().unwrap();
    let mut bound =
        ServerProcess::spawn_with(server_command("exit 0", dir.path()), &Options::default())
            .unwrap();
    let mut unbound = ServerProcess::spawn_with(
        server_command("exit 0", dir.path()),
        &Options {
            watchdog_shell: PathBuf::from("/nonexistent/sh"),
            ..Options::default()
        },
    )
    .unwrap();

    assert_eq!(bound.binding(), Binding::Bound);
    assert_eq!(unbound.binding(), Binding::Unbound);
}

fn own_ids() -> OwnIds {
    OwnIds::new(Pid::from_raw(500).unwrap(), Pid::from_raw(600).unwrap())
}

#[test]
fn test_report_parsing_accepts_marked_and_valid_targets() {
    let own = own_ids();

    assert_eq!(parse_report("marked", own), Some(Report::Marked));
    assert_eq!(
        parse_report("target group 4242", own),
        Some(Report::Target(Escapee::Group(Pid::from_raw(4242).unwrap())))
    );
    assert_eq!(
        parse_report("target pid 77", own),
        Some(Report::Target(Escapee::Process(Pid::from_raw(77).unwrap())))
    );
}

#[test]
fn test_report_parsing_rejects_init_own_groups_and_garbage() {
    let own = own_ids();

    for line in [
        "target group 1",
        "target group 0",
        "target pid -5",
        "target pid 500",
        "target group 600",
        "target group x",
        "target thread 7",
        "target pid 7 extra",
        "marked please",
        "",
    ] {
        assert_eq!(parse_report(line, own), None, "line {line:?}");
    }
    for id in [getpid(), getpgrp()] {
        for kind in ["pid", "group"] {
            assert_eq!(parse_report(&format!("target {kind} {id}"), own), None);
        }
    }
}

const MCPLS_PID: u32 = 800;

fn sweep_awk(rows: &str, self_pid: u32, group: u32, leader: Option<u32>) -> Vec<String> {
    let dir = TempDir::new().unwrap();
    let table = dir.path().join("ps");
    std::fs::write(&table, rows).unwrap();
    let output = std::process::Command::new("awk")
        .args([
            "-v",
            &format!("self={self_pid}"),
            "-v",
            &format!("grp={group}"),
            "-v",
            &format!("parent={MCPLS_PID}"),
            "-v",
            &format!("leader={}", leader.map_or(String::new(), |l| l.to_string())),
            SWEEP_AWK,
        ])
        .arg(&table)
        .output()
        .unwrap();
    let mut lines: Vec<String> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

const TREE: &str = "\
1 0 1
100 1 100
101 100 100
200 101 200
201 200 200
202 201 202
300 101 50
900 1 900
901 900 900
";

#[test]
fn test_awk_reports_escaped_groups_once_and_foreign_group_members_by_pid() {
    let lines = sweep_awk(TREE, 900, 100, None);

    assert_eq!(lines, ["group 200", "group 202", "pid 300"]);
}

#[test]
fn test_awk_excludes_the_watchdog_tree_and_in_group_members() {
    let lines = sweep_awk(
        "100 1 100\n101 100 100\n900 1 900\n901 900 900\n",
        900,
        100,
        None,
    );

    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn test_awk_never_emits_ids_at_or_below_one() {
    let lines = sweep_awk("1 0 1\n100 1 100\n101 100 100\n400 101 1\n", 900, 100, None);

    assert_eq!(lines, ["pid 400"]);
}

#[test]
fn test_awk_ignores_orphans_reparented_to_init() {
    let lines = sweep_awk("1 0 1\n100 1 100\n101 100 100\n600 1 600\n", 900, 100, None);

    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn test_awk_roots_at_the_leader_unconditionally_until_it_is_forgotten() {
    let rows = "1 0 1\n100 1 100\n500 1 500\n501 500 501\n";

    assert_eq!(
        sweep_awk(rows, 900, 100, Some(500)),
        ["group 500", "group 501"]
    );
    assert!(sweep_awk(rows, 900, 100, None).is_empty());
}

#[test]
fn test_awk_ignores_blank_and_non_numeric_rows_when_no_leader_is_registered() {
    let rows = "\n   \nabc def ghi\n1x 2 3\n0 0 0\n-5 1 1\n7 8\n1 0 1\n100 1 100\n101 100 100\n900 1 900\n";

    assert!(sweep_awk(rows, 900, 100, None).is_empty());
}

#[test]
fn test_awk_never_signals_mcpls_or_its_group() {
    let rows = "100 800 100\n101 100 100\n300 101 800\n800 1 800\n801 800 800\n";

    assert_eq!(sweep_awk(rows, 900, 100, None), ["pid 300"]);
}

fn garbage_ps(dir: &Path) -> Options {
    let ps = dir.join("garbage-ps");
    std::fs::write(
        &ps,
        "#!/bin/sh\nprintf '\\n  \\nabc def ghi\\n1x 2 3\\n0 0 0\\n'\nexec ps -A -o pid= -o ppid= -o pgid=\n",
    )
    .unwrap();
    std::fs::set_permissions(&ps, std::fs::Permissions::from_mode(0o755)).unwrap();
    Options {
        ps: ps.into_os_string(),
        ..Options::default()
    }
}

/// Regression: after `forget-leader` the empty leader once matched the pid 0
/// a malformed `ps` row produced, which pulled init into the kill set.
#[tokio::test]
async fn test_malformed_ps_rows_after_forget_leader_do_not_widen_the_sweep() {
    let dir = TempDir::new().unwrap();

    assert!(sweep_with_stale_leader(true, &garbage_ps(dir.path())).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_spawn_creates_its_fds_under_the_spawn_lock() {
    let dir = TempDir::new().unwrap();
    let command = server_command("exit 0", dir.path());
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _guard = SPAWN_LOCK.lock().unwrap();
        locked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    locked_rx.recv().unwrap();

    let spawning = tokio::task::spawn_blocking(move || {
        ServerProcess::spawn_with(command, &Options::default()).map(drop)
    });
    sleep(Duration::from_millis(300)).await;
    assert!(!spawning.is_finished(), "spawn must wait for SPAWN_LOCK");

    release_tx.send(()).unwrap();
    holder.join().unwrap();
    spawning.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_a_timed_out_snapshot_downgrades_mark_so_exit_is_withheld() {
    let dir = TempDir::new().unwrap();
    let mut fixture = Fixture::start(&hanging_ps(dir.path())).await;

    let confirmed = fixture
        .process
        .lifeline
        .as_mut()
        .unwrap()
        .mark_within(Duration::from_secs(5))
        .await;

    assert_eq!(
        confirmed,
        MarkOutcome::Failed(MarkFailure::SnapshotTimedOut)
    );
    let in_group = [fixture.leader, fixture.helper];
    let escapee = fixture.escapee;
    fixture.process.terminate_tree(LIFELINE_SWEEP_BUDGET).await;
    for pid in in_group {
        assert!(pid_state(pid).is_none(), "pid {pid} survived terminate");
    }
    kill_leftover(escapee);
}

#[tokio::test]
async fn test_terminate_tree_keeps_the_reaped_leader_reportable_and_is_idempotent() {
    let mut fixture = Fixture::start(&Options::default()).await;

    fixture.process.terminate_tree(LIFELINE_SWEEP_BUDGET).await;
    fixture.process.terminate_tree(LIFELINE_SWEEP_BUDGET).await;

    assert!(fixture.process.try_wait().unwrap().is_some());
    assert_gone_within_budget(&[fixture.helper, fixture.escapee]).await;
}

#[tokio::test]
async fn test_terminate_tree_of_an_unbound_server_kills_the_leader() {
    let dir = TempDir::new().unwrap();
    let mut process = ServerProcess::spawn_with(
        server_command("exec sleep 608", dir.path()),
        &Options {
            watchdog_shell: PathBuf::from("/nonexistent/sh"),
            ..Options::default()
        },
    )
    .unwrap();

    process.terminate_tree(Duration::from_secs(5)).await;

    assert!(process.try_wait().unwrap().is_some());
}

#[tokio::test]
async fn test_terminating_one_server_leaves_the_other_servers_tree_alive() {
    let mut first = Fixture::start(&Options::default()).await;
    let second = Fixture::start(&Options::default()).await;

    first.process.terminate_tree(LIFELINE_SWEEP_BUDGET).await;

    assert_gone_within_budget(&first.all()).await;
    for pid in second.all() {
        assert!(
            pid_state(pid).is_some(),
            "pid {pid} of the other server died"
        );
    }
    let pids = second.all();
    drop(second.process);
    assert_gone_within_budget(&pids).await;
}

#[tokio::test]
async fn test_a_short_terminate_budget_bounds_the_wait_and_kills_from_here() {
    let dir = TempDir::new().unwrap();
    let ps = hanging_ps(dir.path());
    let options = Options {
        scan_timeout: Duration::from_secs(5),
        ..ps
    };
    let mut fixture = Fixture::start(&options).await;
    let in_group = [fixture.leader, fixture.helper];

    let started = Instant::now();
    fixture
        .process
        .terminate_tree(Duration::from_millis(500))
        .await;

    assert!(started.elapsed() < Duration::from_secs(3));
    for pid in in_group {
        assert!(pid_state(pid).is_none(), "pid {pid} survived terminate");
    }
    kill_leftover(fixture.escapee);
}

#[tokio::test]
async fn test_a_watchdog_that_died_mid_session_is_no_longer_reported_as_bound() {
    let mut fixture = Fixture::start(&Options::default()).await;
    assert_eq!(fixture.process.binding(), Binding::Bound);
    fixture.watchdog_mut().child.kill().await.unwrap();

    fixture.process.try_wait().unwrap();

    assert_eq!(fixture.process.binding(), Binding::Unbound);
    let pids = fixture.all();
    drop(fixture.process);
    assert_gone_within_budget(&[pids[0], pids[2]]).await;
    kill_leftover(pids[1]);
}

#[tokio::test]
async fn test_missing_ps_is_reported_as_a_failed_scan_while_mark_still_confirms() {
    let options = Options {
        ps: OsString::from("/nonexistent/ps"),
        ..Options::default()
    };
    let mut fixture = Fixture::start(&options).await;

    assert_eq!(
        fixture.process.mark_escapees().await,
        MarkOutcome::Confirmed
    );

    let channel = &mut fixture.watchdog_mut().channel;
    channel.drain();
    assert_eq!(channel.scan, ScanOutcome::Failed);
    assert!(channel.targets.is_empty());
    let pids = fixture.all();
    fixture.process.terminate_tree(LIFELINE_SWEEP_BUDGET).await;
    for pid in [pids[0], pids[2]] {
        assert!(pid_state(pid).is_none(), "pid {pid} survived terminate");
    }
    kill_leftover(pids[1]);
}

#[tokio::test]
async fn test_drain_reads_the_last_line_without_waiting_for_reactor_readiness() {
    let (mine, theirs) = StdUnixStream::pair().unwrap();
    let mut channel = Channel::new(mine, own_ids()).unwrap();
    (&theirs).write_all(b"target group 4242\n").unwrap();
    drop(theirs);

    channel.drain();

    assert_eq!(
        channel.targets,
        [Escapee::Group(Pid::from_raw(4242).unwrap())]
    );
    assert!(channel.eof);
}

#[tokio::test]
async fn test_a_target_printed_just_before_an_overrun_is_still_killed() {
    let (mine, theirs) = StdUnixStream::pair().unwrap();
    let mut stray = std::process::Command::new("sleep")
        .arg("607")
        .process_group(0)
        .spawn()
        .unwrap();
    let channel = Channel::new(mine, own_ids()).unwrap();
    (&theirs)
        .write_all(format!("target pid {}\n", stray.id()).as_bytes())
        .unwrap();
    let (stderr_hold, _writer) = std::io::pipe().unwrap();
    let anchor = Anchor::start(&stderr_hold).unwrap();
    let mut sweep = Sweep {
        watchdog: None,
        channel: Some(channel),
        group: anchor.group,
        anchor: anchor.child,
        leader: None,
        budget: LIFELINE_SWEEP_BUDGET,
    };

    sweep.fallback_kill();

    assert!(
        stray.wait().unwrap().code().is_none(),
        "stray must be signalled"
    );
}

#[tokio::test]
async fn test_spawn_retries_once_with_a_fresh_anchor_when_the_group_vanished() {
    let (stderr_hold, _writer) = std::io::pipe().unwrap();
    let mut anchor = Some(Anchor::start(&stderr_hold).unwrap());
    let dead_group = anchor.as_ref().unwrap().group;
    anchor.as_mut().unwrap().child.kill().await.unwrap();
    let mut command = Command::new("sleep");
    command.arg("606");

    let mut child = spawn_leader(&mut command, &mut anchor, &stderr_hold)
        .expect("FR-007: the spawn must succeed after the retry");

    let fresh = anchor.take().unwrap();
    assert_ne!(fresh.group, dead_group);
    child.kill().await.unwrap();
    discard(fresh);
}

#[tokio::test]
async fn test_permission_denied_with_a_live_anchor_is_returned_unchanged() {
    let dir = TempDir::new().unwrap();
    let not_executable = dir.path().join("server");
    std::fs::write(&not_executable, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&not_executable, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (stderr_hold, _writer) = std::io::pipe().unwrap();
    let mut anchor = Some(Anchor::start(&stderr_hold).unwrap());
    let group = anchor.as_ref().unwrap().group;
    let mut command = Command::new(&not_executable);

    let error = spawn_leader(&mut command, &mut anchor, &stderr_hold).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    let kept = anchor.take().unwrap();
    assert_eq!(kept.group, group);
    discard(kept);
}
