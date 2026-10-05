//! Smoke run of the harness against the in-repo fixture, so it tracks MCP output-shape changes.
//!
//! Needs a built `mcpls` binary and `rust-analyzer` on `PATH`, hence `#[ignore]`;
//! the e2e CI job runs it with `--run-ignored ignored-only`.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::path::PathBuf;
use std::process::Command;

use mcpls_bench::report::{Outcome, RunReport};

fn bench_binary() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_mcpls-bench")
        .map(PathBuf::from)
        .or_else(|| option_env!("CARGO_BIN_EXE_mcpls-bench").map(PathBuf::from))
        .expect("mcpls-bench binary path is not provided by cargo or nextest")
}

fn mcpls_binary() -> PathBuf {
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .or_else(|| std::env::var_os("CARGO_BUILD_TARGET_DIR"))
        .unwrap_or_else(|| "target".into());
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").into()),
    );
    manifest_dir
        .ancestors()
        .nth(2)
        .expect("manifest dir is nested under the workspace root")
        .join(target_dir)
        .join("debug/mcpls")
}

fn run_smoke(work_dir: &std::path::Path, cwd: Option<&std::path::Path>) -> RunReport {
    let scenario = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").into()),
    )
    .join("scenarios/smoke-fixture.toml");

    let mut command = Command::new(bench_binary());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .arg("run")
        .arg(&scenario)
        .arg("--mcpls")
        .arg(mcpls_binary())
        .arg("--work-dir")
        .arg(work_dir)
        .args([
            "--runs",
            "1",
            "--warmup-runs",
            "0",
            "--iterations",
            "1",
            "--ready-timeout-secs",
            "180",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mcpls-bench failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_all_ok(report: &RunReport) {
    assert_eq!(report.host.os, std::env::consts::OS);
    let samples: Vec<_> = report.runs.iter().flat_map(|run| &run.samples).collect();
    assert!(!samples.is_empty(), "the run produced no samples");
    let not_ok: Vec<_> = samples
        .iter()
        .filter(|sample| sample.outcome != Outcome::Ok)
        .collect();
    assert!(not_ok.is_empty(), "non-ok samples: {not_ok:#?}");
}

#[test]
#[ignore = "requires a built mcpls binary and rust-analyzer"]
fn smoke_scenario_samples_are_all_ok() {
    let work_dir = tempfile::tempdir().unwrap();
    assert_all_ok(&run_smoke(work_dir.path(), None));
}

#[test]
#[ignore = "requires a built mcpls binary and rust-analyzer"]
fn relative_work_dir_is_resolved_against_the_invocation_directory() {
    let cwd = tempfile::tempdir().unwrap();
    assert_all_ok(&run_smoke(
        std::path::Path::new("relative-work"),
        Some(cwd.path()),
    ));
}
