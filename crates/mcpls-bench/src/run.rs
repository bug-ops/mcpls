//! Driving mcpls over MCP stdio and recording timed samples (`mcpls-bench run`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result};
use mcpls_core::ServerConfig;
use mcpls_core::bridge::IndexingPolicy;
use mcpls_core::config::LspServerConfig;
use rmcp::model::{ContentBlock, ErrorCode};
use rmcp::service::{RunningService, ServiceError};
use rmcp::{RoleClient, ServiceExt};
use tokio::process::Command;

use crate::pin::{ensure_matches, pin};
use crate::prepare::{absolute, repo_dir, verify_prepared};
use crate::probe::Incorrect;
use crate::report::{
    BinaryRecord, BuildProfile, Micros, Outcome, PinRecord, ReadyRecord, Region, RunParams,
    RunRecord, RunReport, Sample, ShutdownOutcome, SourceRecord, summarize,
};
use crate::scenario::{Executable, Probe, Scenario};

const READY_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const LSP_TIMEOUT_SECS: u64 = 60;

/// Options of one `run` invocation.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// The mcpls binary under test.
    pub mcpls: PathBuf,
    /// Measured runs.
    pub runs: u32,
    /// Warm-up runs executed first and excluded from the summary.
    pub warmup_runs: u32,
    /// Iterations of every probe per run.
    pub iterations: u32,
    /// Deadline for the ready probe to pass.
    pub ready_timeout: Duration,
    /// Deadline of a single tool call.
    pub call_timeout: Duration,
    /// Tolerate executables whose version differs from the scenario pin.
    pub allow_version_mismatch: bool,
}

type Client = RunningService<RoleClient, ()>;

/// Builds the mcpls configuration that routes the scenario's server over `repo`.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use mcpls_bench::run::mcpls_config;
/// use mcpls_bench::scenario::Scenario;
///
/// let scenario: Scenario =
///     toml::from_str(include_str!("../scenarios/smoke-fixture.toml")).unwrap();
/// let config = mcpls_config(&scenario, Path::new("/repo"), Path::new("/bin/rust-analyzer"));
/// assert_eq!(config.lsp_servers.len(), 1);
/// assert_eq!(config.lsp_servers[0].language_id, "rust");
/// ```
#[must_use]
pub fn mcpls_config(scenario: &Scenario, repo: &Path, server_path: &Path) -> ServerConfig {
    let mut config = ServerConfig::default();
    config.workspace.roots = vec![repo.to_path_buf()];
    config.lsp_servers = vec![LspServerConfig {
        language_id: scenario.server.language_id.clone(),
        command: server_path.to_string_lossy().into_owned(),
        args: scenario.server.args.clone(),
        env: HashMap::new(),
        file_patterns: scenario.server.file_patterns.clone(),
        initialization_options: None,
        timeout_seconds: LSP_TIMEOUT_SECS,
        request_timeout_seconds: LSP_TIMEOUT_SECS,
        heuristics: None,
        name: None,
        handles: None,
        indexing: IndexingPolicy::Auto,
    }];
    config
}

/// Runs the scenario against an already prepared repository and returns the report.
///
/// # Errors
///
/// Returns an error for harness failures (missing or mismatching repository,
/// unprepared setup, unpinned executables, unspawnable mcpls). Failures of
/// individual calls are recorded as samples, not returned.
pub async fn run(
    scenario: &Scenario,
    scenario_dir: &Path,
    work_dir: &Path,
    options: &RunOptions,
) -> Result<RunReport> {
    let work_dir = absolute(work_dir)?;
    let work_dir = work_dir.as_path();
    let repo = repo_dir(scenario, scenario_dir, work_dir)?;
    let observed_commit = verify_prepared(scenario, &repo, work_dir).await?;

    let mcpls = pin(&mcpls_executable(&options.mcpls)?, &repo).await?;
    let mcpls_binary = binary_record(&mcpls.path)?;
    let server = pin(&scenario.server.executable, &repo).await?;
    let mut runtime = Vec::new();
    for executable in &scenario.runtime {
        runtime.push(pin(executable, &repo).await?);
    }
    ensure_matches(
        std::iter::once(&server).chain(&runtime),
        options.allow_version_mismatch,
    )?;

    let config_path = write_config(scenario, &repo, work_dir, &server.path)?;

    let total = options.warmup_runs.saturating_add(options.runs);
    let mut runs = Vec::new();
    let mut aborted = false;
    for index in 0..total {
        let warmup = index < options.warmup_runs;
        let record = one_run(
            index,
            warmup,
            scenario,
            &repo,
            &config_path,
            &mcpls,
            options,
        )
        .await?;
        eprintln!(
            "run {}/{total} ({}) done",
            index + 1,
            if warmup { "warmup" } else { "measured" }
        );
        let never_ready = record.ready.last_failure.is_some();
        runs.push(record);
        if never_ready {
            aborted = index + 1 < total;
            eprintln!("the server never became ready; skipping the remaining runs");
            break;
        }
    }

    Ok(RunReport {
        scenario: scenario.name.clone(),
        source: observed_commit.map_or_else(
            || SourceRecord::Unpinned { path: repo.clone() },
            |commit| SourceRecord::Pinned { commit },
        ),
        mcpls,
        mcpls_binary,
        server,
        runtime,
        params: RunParams {
            runs: options.runs,
            warmup_runs: options.warmup_runs,
            iterations: options.iterations,
            ready_timeout_secs: options.ready_timeout.as_secs(),
            call_timeout_secs: options.call_timeout.as_secs(),
            allow_version_mismatch: options.allow_version_mismatch,
        },
        aborted,
        summary: summarize(&runs),
        runs,
    })
}

/// Treats `--mcpls` strictly as a file path: a bare name is relative to the
/// current directory, never looked up on `PATH`.
fn mcpls_executable(path: &Path) -> Result<Executable> {
    Ok(Executable {
        command: absolute(path)?.to_string_lossy().into_owned(),
        version_args: vec!["--version".to_owned()],
        expected_version: None,
    })
}

fn build_profile(binary: &Path) -> BuildProfile {
    match binary
        .parent()
        .and_then(Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
    {
        Some("debug") => BuildProfile::Debug,
        Some("release") => BuildProfile::Release,
        _ => BuildProfile::Unknown,
    }
}

fn binary_record(binary: &Path) -> Result<BinaryRecord> {
    let metadata = std::fs::metadata(binary)
        .with_context(|| format!("failed to stat {}", binary.display()))?;
    Ok(BinaryRecord {
        profile: build_profile(binary),
        size_bytes: metadata.len(),
        modified_unix_secs: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_secs()),
    })
}

fn write_config(
    scenario: &Scenario,
    repo: &Path,
    work_dir: &Path,
    server_path: &Path,
) -> Result<PathBuf> {
    let dir = work_dir.join("configs");
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join(format!("{}.toml", scenario.name.as_str()));
    let text = toml::to_string(&mcpls_config(scenario, repo, server_path))
        .context("failed to serialize the mcpls config")?;
    std::fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

async fn one_run(
    index: u32,
    warmup: bool,
    scenario: &Scenario,
    repo: &Path,
    config_path: &Path,
    mcpls: &PinRecord,
    options: &RunOptions,
) -> Result<RunRecord> {
    let started = Instant::now();
    let mut child = Command::new(&mcpls.path)
        .arg("--config")
        .arg(config_path)
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to spawn {}", mcpls.path.display()))?;
    let stdin = child.stdin.take().context("mcpls stdin was not captured")?;
    let stdout = child
        .stdout
        .take()
        .context("mcpls stdout was not captured")?;

    let mut samples = Vec::new();
    let session = drive(
        (stdout, stdin),
        scenario,
        repo,
        started,
        options,
        &mut samples,
    )
    .await;
    Ok(RunRecord {
        index,
        warmup,
        ready: session.ready,
        truncated_after_timeout: session.truncated,
        samples,
        shutdown: finish(&mut child, session.closed).await,
    })
}

struct Session {
    ready: ReadyRecord,
    truncated: bool,
    closed: bool,
}

/// Runs one MCP session and appends its samples.
async fn drive(
    transport: (tokio::process::ChildStdout, tokio::process::ChildStdin),
    scenario: &Scenario,
    repo: &Path,
    started: Instant,
    options: &RunOptions,
    samples: &mut Vec<Sample>,
) -> Session {
    let client = match ().serve(transport).await {
        Ok(client) => client,
        Err(error) => {
            let failure = Outcome::Failed {
                error: error.to_string(),
            };
            samples.push(Sample {
                region: Region::Startup,
                outcome: failure.clone(),
                elapsed_us: started.elapsed().into(),
                iteration: 0,
            });
            return Session {
                ready: ReadyRecord {
                    attempts: 0,
                    last_failure: Some(failure),
                },
                truncated: false,
                closed: false,
            };
        }
    };
    samples.push(Sample {
        region: Region::Startup,
        outcome: Outcome::Ok,
        elapsed_us: started.elapsed().into(),
        iteration: 0,
    });
    let (ready, ready_sample) = wait_until_ready(&client, scenario, repo, started, options).await;
    let is_ready = ready.last_failure.is_none();
    samples.push(ready_sample);
    let truncated = if is_ready {
        measure_probes(&client, scenario, repo, options, samples).await
    } else {
        false
    };
    let closed = matches!(
        tokio::time::timeout(SHUTDOWN_GRACE, client.cancel()).await,
        Ok(Ok(_))
    );
    Session {
        ready,
        truncated,
        closed,
    }
}

async fn finish(child: &mut tokio::process::Child, closed: bool) -> ShutdownOutcome {
    let exited = matches!(
        tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await,
        Ok(Ok(_))
    );
    if closed && exited {
        return ShutdownOutcome::Clean;
    }
    if let Err(error) = child.kill().await {
        eprintln!("warning: failed to kill mcpls: {error}");
    }
    ShutdownOutcome::Killed
}

/// Retries the ready probe until it passes, fails permanently, or the deadline passes.
async fn wait_until_ready(
    client: &Client,
    scenario: &Scenario,
    repo: &Path,
    started: Instant,
    options: &RunOptions,
) -> (ReadyRecord, Sample) {
    let deadline = started.checked_add(options.ready_timeout);
    let mut attempts = 0;
    loop {
        attempts += 1;
        let budget = deadline.map_or(options.call_timeout, |d| {
            d.saturating_duration_since(Instant::now())
                .min(options.call_timeout)
        });
        let call = timed_call(client, &scenario.ready_probe, repo, budget, 0).await;
        let ready_sample = |outcome| Sample {
            region: Region::Ready,
            outcome,
            elapsed_us: started.elapsed().into(),
            iteration: 0,
        };
        if call.sample.outcome == Outcome::Ok {
            let record = ReadyRecord {
                attempts,
                last_failure: None,
            };
            return (record, ready_sample(Outcome::Ok));
        }
        let expired = deadline.is_some_and(|d| Instant::now() >= d);
        if call.permanent || expired {
            let outcome = if call.permanent {
                call.sample.outcome.clone()
            } else {
                Outcome::TimedOut
            };
            let record = ReadyRecord {
                attempts,
                last_failure: Some(call.sample.outcome),
            };
            return (record, ready_sample(outcome));
        }
        tokio::time::sleep(READY_RETRY_INTERVAL).await;
    }
}

/// Measures every probe `iterations` times; returns whether a timeout cut the run short.
///
/// After a timeout the abandoned call is still being served by mcpls, so any
/// later call would queue behind it and be inflated; the rest is skipped.
async fn measure_probes(
    client: &Client,
    scenario: &Scenario,
    repo: &Path,
    options: &RunOptions,
    samples: &mut Vec<Sample>,
) -> bool {
    for probe in &scenario.probes {
        for iteration in 0..options.iterations {
            let call = timed_call(client, probe, repo, options.call_timeout, iteration).await;
            let timed_out = call.sample.outcome == Outcome::TimedOut;
            samples.push(call.sample);
            if timed_out {
                return true;
            }
        }
    }
    false
}

struct Call {
    sample: Sample,
    /// Retrying cannot help: the request is invalid or the transport is gone.
    permanent: bool,
}

fn is_permanent(error: &ServiceError) -> bool {
    match error {
        ServiceError::McpError(data) => data.code == ErrorCode::INVALID_PARAMS,
        ServiceError::TransportClosed | ServiceError::TransportSend(_) => true,
        _ => false,
    }
}

async fn timed_call(
    client: &Client,
    probe: &Probe,
    repo: &Path,
    timeout: Duration,
    iteration: u32,
) -> Call {
    let region = probe.region();
    let request = match probe.request(repo) {
        Ok(request) => request,
        Err(error) => {
            return Call {
                sample: Sample {
                    region,
                    outcome: Outcome::Failed {
                        error: format!("{error:#}"),
                    },
                    elapsed_us: Micros(0),
                    iteration,
                },
                permanent: true,
            };
        }
    };
    let started = Instant::now();
    let response = tokio::time::timeout(timeout, client.call_tool(request)).await;
    let elapsed_us = Micros::from(started.elapsed());
    let (outcome, permanent) = match response {
        Err(_) => (Outcome::TimedOut, false),
        Ok(Err(error)) => (
            Outcome::Failed {
                error: error.to_string(),
            },
            is_permanent(&error),
        ),
        Ok(Ok(result)) if result.is_error == Some(true) => (
            Outcome::Failed {
                error: result
                    .content
                    .iter()
                    .find_map(ContentBlock::as_text)
                    .map_or_else(|| "tool error".to_owned(), |t| t.text.clone()),
            },
            false,
        ),
        Ok(Ok(result)) => match probe.verdict(&result) {
            Ok(()) => (Outcome::Ok, false),
            Err(Incorrect(detail)) => (Outcome::Incorrect { detail }, false),
        },
    };
    Call {
        sample: Sample {
            region,
            outcome,
            elapsed_us,
            iteration,
        },
        permanent,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn generated_config_round_trips_through_server_config() {
        let scenario: Scenario =
            toml::from_str(include_str!("../scenarios/smoke-fixture.toml")).unwrap();
        let config = mcpls_config(
            &scenario,
            Path::new("/repo"),
            Path::new("/bin/rust-analyzer"),
        );
        let text = toml::to_string(&config).unwrap();
        let parsed: ServerConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.lsp_servers.len(), 1);
        assert_eq!(parsed.lsp_servers[0].command, "/bin/rust-analyzer");
        assert_eq!(parsed.workspace.roots, [PathBuf::from("/repo")]);
    }

    #[test]
    fn mcpls_option_is_always_a_path() {
        let bare = mcpls_executable(Path::new("mcpls")).unwrap();
        assert!(Path::new(&bare.command).is_absolute());
        assert!(Path::new(&bare.command).starts_with(std::env::current_dir().unwrap()));
        let dotted = mcpls_executable(Path::new("./mcpls")).unwrap();
        assert!(Path::new(&dotted.command).is_absolute());
    }

    #[test]
    fn build_profile_is_inferred_from_the_parent_directory() {
        assert_eq!(
            build_profile(Path::new("/t/release/mcpls")),
            BuildProfile::Release
        );
        assert_eq!(
            build_profile(Path::new("/t/debug/mcpls")),
            BuildProfile::Debug
        );
        assert_eq!(
            build_profile(Path::new("/usr/bin/mcpls")),
            BuildProfile::Unknown
        );
    }

    #[test]
    fn only_invalid_params_and_dead_transport_are_permanent() {
        let invalid = ServiceError::McpError(rmcp::model::ErrorData::invalid_params("bad", None));
        let internal = ServiceError::McpError(rmcp::model::ErrorData::internal_error("oops", None));
        assert!(is_permanent(&invalid));
        assert!(is_permanent(&ServiceError::TransportClosed));
        assert!(!is_permanent(&internal));
    }
}
