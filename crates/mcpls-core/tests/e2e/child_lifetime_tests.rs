//! End-to-end tests for binding LSP child processes to the mcpls lifetime
//! (spec `lsp/007-lsp-child-process-lifetime`).

#![cfg(unix)]

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tempfile::TempDir;

use super::mcp_client::McpClient;

const POLL_CEILING: Duration = Duration::from_secs(5);

fn pid_alive(pid: u32) -> Result<bool> {
    Ok(std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()?
        .success())
}

fn read_pids(path: &std::path::Path) -> Option<Vec<u32>> {
    let pids: Vec<u32> = std::fs::read_to_string(path)
        .ok()?
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect();
    (pids.len() == 2).then_some(pids)
}

/// SC-001: after `SIGKILL` of mcpls, a configured server that never answers
/// `initialize` and its backgrounded grandchild are both gone.
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_child_lifetime_sigkill_reaps_server_and_grandchild() -> Result<()> {
    let dir = TempDir::new()?;
    let pid_file = dir.path().join("pids");
    let script = format!(
        "sleep 600 & echo \"$$ $!\" > '{path}.tmp' && mv '{path}.tmp' '{path}'; \
         while read -r _; do :; done",
        path = pid_file.display()
    );
    let config_path = dir.path().join("mcpls.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
            [workspace]
            roots = ["{root}"]

            [[lsp_servers]]
            language_id = "rust"
            command = "sh"
            args = ["-c", "{script}"]
            file_patterns = ["**/*.rs"]
            timeout_seconds = 600
            "#,
            root = dir.path().to_string_lossy().replace('\\', "\\\\"),
            script = script.replace('\\', "\\\\").replace('"', "\\\"")
        ),
    )?;

    let mut client = McpClient::spawn_with_args(&[
        "--config",
        config_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid config path"))?,
    ])?;
    client.initialize()?;

    let deadline = Instant::now() + Duration::from_secs(30);
    let pids = loop {
        if let Some(pids) = read_pids(&pid_file) {
            break pids;
        }
        if Instant::now() >= deadline {
            bail!("LSP server script never wrote its pids");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    for pid in &pids {
        assert!(
            pid_alive(*pid)?,
            "pid {pid} should be alive before the kill"
        );
    }

    let mcpls_pid = client.pid();
    assert!(
        std::process::Command::new("kill")
            .args(["-9", &mcpls_pid.to_string()])
            .status()?
            .success(),
        "failed to SIGKILL mcpls (pid {mcpls_pid})"
    );

    let deadline = Instant::now() + POLL_CEILING;
    for pid in pids {
        while pid_alive(pid)? {
            assert!(
                Instant::now() < deadline,
                "pid {pid} survived SIGKILL of mcpls"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(())
}
