//! End-to-end tests for binding LSP child processes to the mcpls lifetime
//! (spec `lsp/007-lsp-child-process-lifetime`).
//!
//! The fake server is a perl script that behaves like the servers that made
//! the original design fail: it exits on stdin EOF, streams valid
//! `window/logMessage` frames to stdout and text to stderr every millisecond,
//! and leaves a `setsid` grandchild behind (the shape of rust-analyzer's
//! flycheck `cargo check`).

#![cfg(unix)]

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::json;
use tempfile::TempDir;

use super::mcp_client::McpClient;

const POLL_CEILING: Duration = Duration::from_secs(5);
const START_CEILING: Duration = Duration::from_secs(30);

const FAKE_SERVER: &str = r#"
use strict;
use warnings;
use POSIX ();
use JSON::PP;

my ($dir, $mode) = @ARGV;

sub write_file {
    my ($name, $text) = @_;
    open my $file, '>', "$dir/$name.tmp" or die "open: $!";
    print $file $text;
    close $file;
    rename "$dir/$name.tmp", "$dir/$name" or die "rename: $!";
}

sub send_message {
    my ($body) = @_;
    syswrite(STDOUT, 'Content-Length: ' . length($body) . "\r\n\r\n" . $body);
}

sub spawn_sleep {
    my ($seconds, $new_session) = @_;
    my $pid = fork();
    die "fork: $!" unless defined $pid;
    if ($pid == 0) {
        POSIX::setsid() if $new_session;
        exec 'sleep', $seconds or exit 1;
    }
    return $pid;
}

sub read_message {
    my $length;
    while (1) {
        my $line = <STDIN>;
        return undef unless defined $line;
        last if $line =~ /^\r?\n$/;
        $length = $1 if $line =~ /^Content-Length:\s*(\d+)/i;
    }
    return undef unless defined $length;
    my $body = '';
    while (length($body) < $length) {
        my $got = read(STDIN, $body, $length - length($body), length($body));
        return undef unless $got;
    }
    return decode_json($body);
}

my $chatter = fork();
die "fork: $!" unless defined $chatter;
if ($chatter == 0) {
    my $log = '{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":3,"message":"chatter"}}';
    while (1) {
        send_message($log);
        syswrite(STDERR, "chatter\n");
        select(undef, undef, undef, 0.001);
    }
}

my $escapee = spawn_sleep(600, $mode ne 'crash');
write_file('pids', "$$ $escapee\n");

if ($mode eq 'silent') {
    1 while sysread(STDIN, my $buffer, 4096);
    exit 0;
}

my $crash = $mode eq 'crash' && !-e "$dir/crashed";
while (defined(my $message = read_message())) {
    my $method = $message->{method} // '';
    my $id = $message->{id};
    if ($method eq 'initialize') {
        send_message(encode_json({ jsonrpc => '2.0', id => $id, result => { capabilities => {} } }));
    } elsif ($method eq 'initialized') {
        write_file('initialized', "1\n");
        if ($crash) {
            write_file('helper', "$escapee\n");
            write_file('crashed', "1\n");
            exit 1;
        }
    } elsif ($method eq 'shutdown') {
        send_message(encode_json({ jsonrpc => '2.0', id => $id, result => undef }));
    } elsif ($method eq 'exit') {
        exit 0;
    } elsif (defined $id) {
        send_message(encode_json({ jsonrpc => '2.0', id => $id, error => { code => -32601, message => 'unsupported' } }));
    }
}
exit 0;
"#;

fn pid_alive(pid: u32) -> Result<bool> {
    Ok(std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()?
        .success())
}

fn read_pids(path: &Path) -> Option<Vec<u32>> {
    let pids: Vec<u32> = std::fs::read_to_string(path)
        .ok()?
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect();
    (pids.len() == 2).then_some(pids)
}

fn wait_for_pids(path: &Path) -> Result<Vec<u32>> {
    let deadline = Instant::now() + START_CEILING;
    loop {
        if let Some(pids) = read_pids(path) {
            return Ok(pids);
        }
        if Instant::now() >= deadline {
            bail!("LSP server script never wrote its pids");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_file(path: &Path) -> Result<()> {
    let deadline = Instant::now() + START_CEILING;
    while !path.exists() {
        if Instant::now() >= deadline {
            bail!("{} never appeared", path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn assert_all_gone(pids: &[u32], what: &str) -> Result<()> {
    let deadline = Instant::now() + POLL_CEILING;
    for pid in pids {
        while pid_alive(*pid)? {
            assert!(Instant::now() < deadline, "pid {pid} survived {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(())
}

fn signal(pid: u32, name: &str) -> Result<()> {
    let status = std::process::Command::new("kill")
        .args([name, &pid.to_string()])
        .status()?;
    assert!(status.success(), "failed to send {name} to {pid}");
    Ok(())
}

struct Run {
    _dir: TempDir,
    root: std::path::PathBuf,
    client: McpClient,
}

impl Run {
    fn start(mode: &str) -> Result<Self> {
        let dir = TempDir::new()?;
        let root = std::fs::canonicalize(dir.path())?;
        let script = root.join("server.pl");
        std::fs::write(&script, FAKE_SERVER)?;
        std::fs::write(root.join("main.rs"), "fn main() {}\n")?;
        let config_path = root.join("mcpls.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
                [workspace]
                roots = ['{root}']

                [[lsp_servers]]
                language_id = "rust"
                command = "perl"
                args = ['{script}', '{root}', '{mode}']
                file_patterns = ["**/*.rs"]
                timeout_seconds = 600
                "#,
                root = root.display(),
                script = script.display(),
            ),
        )?;
        let mut client = McpClient::spawn_with_args(&[
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid config path"))?,
        ])?;
        client.initialize()?;
        Ok(Self {
            _dir: dir,
            root,
            client,
        })
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.root.join(name)
    }
}

/// SC-001: after `SIGKILL` of mcpls, a configured server that never answers
/// `initialize` but exits on stdin EOF and writes to stdout/stderr, and its
/// `setsid` grandchild, are both gone.
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_child_lifetime_sigkill_reaps_server_and_setsid_grandchild() -> Result<()> {
    let run = Run::start("silent")?;
    let pids = wait_for_pids(&run.path("pids"))?;
    for pid in &pids {
        assert!(
            pid_alive(*pid)?,
            "pid {pid} should be alive before the kill"
        );
    }

    signal(run.client.pid(), "-9")?;

    assert_all_gone(&pids, "SIGKILL of mcpls")
}

/// SC-006: a graceful shutdown with a server that exits cleanly on `exit`
/// still takes its `setsid` grandchild with it before mcpls returns.
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_child_lifetime_graceful_shutdown_reaps_setsid_grandchild() -> Result<()> {
    let mut run = Run::start("serve")?;
    wait_for_file(&run.path("initialized"))?;
    let pids = wait_for_pids(&run.path("pids"))?;
    for pid in &pids {
        assert!(pid_alive(*pid)?, "pid {pid} should be alive before SIGTERM");
    }

    signal(run.client.pid(), "-TERM")?;

    let deadline = Instant::now() + START_CEILING;
    while run.client.try_wait()?.is_none() {
        assert!(Instant::now() < deadline, "mcpls did not exit on SIGTERM");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_all_gone(&pids, "graceful shutdown")
}

/// SC-007 (#542): when a server crashes and is respawned, the helper it left
/// in its process group is killed instead of living until mcpls exits.
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_child_lifetime_respawn_reaps_previous_servers_helper() -> Result<()> {
    let mut run = Run::start("crash")?;
    wait_for_file(&run.path("crashed"))?;
    let helper: u32 = std::fs::read_to_string(run.path("helper"))?
        .trim()
        .parse()?;
    assert!(
        pid_alive(helper)?,
        "helper should outlive its crashed leader"
    );

    let source = run.path("main.rs");
    let deadline = Instant::now() + START_CEILING;
    while pid_alive(helper)? {
        assert!(
            Instant::now() < deadline,
            "helper of the crashed server survived the respawn"
        );
        // The tool outcome is irrelevant: calling it is what notices the crash and respawns.
        let _outcome = run
            .client
            .call_tool(
                "get_hover",
                &json!({ "file_path": source, "line": 1, "character": 1 }),
            )
            .unwrap_or_default();
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}
