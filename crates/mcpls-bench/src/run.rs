//! Driving mcpls over MCP stdio and recording timed samples (`mcpls-bench run`).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use mcpls_core::ServerConfig;
use mcpls_core::bridge::IndexingPolicy;
use mcpls_core::config::{LspServerConfig, TimeoutSecs};
use rmcp::model::{ContentBlock, ErrorCode};
use rmcp::service::{RunningService, ServiceError};
use rmcp::{RoleClient, ServiceExt};
use tokio::process::Command;

use crate::lock::WorkDirLock;
use crate::pin::{ensure_matches, pin};
use crate::prepare::{absolute, repo_dir, verify_prepared};
use crate::probe::Incorrect;
use crate::process_tree::{ProcessGroup, SHUTDOWN_GRACE, SessionEnd, TreeWatch};
use crate::report::{
    BinaryRecord, BuildProfile, MemoryCheckpoint, MemoryRecord, Micros, Outcome, ReadyRecord,
    Region, RunParams, RunRecord, RunReport, Sample, SourceRecord, TargetRecord, summarize,
    summarize_memory,
};
use crate::scenario::{Executable, Probe, Scenario};
use crate::stderr_log::{DRAIN_GRACE, StderrDrain, StderrLogCap};
use crate::target::{ProbeCall, Target};

const READY_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const LSP_TIMEOUT_SECS: TimeoutSecs = match TimeoutSecs::new(60) {
    Some(secs) => secs,
    None => panic!("the LSP timeout must be in range"),
};

/// Options of one `run` invocation.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// What is measured: mcpls or a comparison server.
    pub target: Target,
    /// Largest stderr log kept per run.
    pub stderr_log_cap: StderrLogCap,
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
        settings: None,
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
    let _lock = WorkDirLock::acquire(work_dir, &scenario.name).await?;
    let repo = repo_dir(scenario, scenario_dir, work_dir)?;
    let observed_commit = verify_prepared(scenario, &repo, work_dir).await?;

    let mut runtime = Vec::new();
    for executable in &scenario.runtime {
        runtime.push(pin(executable, &repo).await?);
    }
    let (launch, target_record) = launch_target(&options.target, scenario, &repo, work_dir).await?;
    ensure_matches(
        target_record.pins().into_iter().chain(&runtime),
        options.allow_version_mismatch,
    )?;
    let ready_probe = ready_probe_for(scenario, &options.target)?;

    let log_dir = invocation_log_dir(work_dir, scenario, SystemTime::now());
    let env = RunEnv {
        scenario,
        ready_probe,
        repo: &repo,
        launch: &launch,
        log_dir: &log_dir,
        options,
    };
    let total = options.warmup_runs.saturating_add(options.runs);
    let mut runs = Vec::new();
    let mut aborted = false;
    for index in 0..total {
        let warmup = index < options.warmup_runs;
        let record = one_run(index, warmup, &env).await?;
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
        target: target_record,
        runtime,
        params: RunParams {
            runs: options.runs,
            warmup_runs: options.warmup_runs,
            iterations: options.iterations,
            ready_timeout_secs: options.ready_timeout.as_secs(),
            call_timeout_secs: options.call_timeout.as_secs(),
            ready_retry_interval_ms: u64::try_from(READY_RETRY_INTERVAL.as_millis())
                .unwrap_or(u64::MAX),
            allow_version_mismatch: options.allow_version_mismatch,
            stderr_log_cap_bytes: options.stderr_log_cap.bytes(),
        },
        aborted,
        summary: summarize(&runs),
        memory_summary: summarize_memory(&runs),
        runs,
    })
}

/// Treats `--mcpls` strictly as a file path: a bare name is relative to the
/// current directory, never looked up on `PATH`.
fn mcpls_executable(path: &Path) -> Result<Executable> {
    Ok(Executable {
        command: absolute(path)?.to_string_lossy().into_owned(),
        version_args: vec!["--version".to_owned()],
        version_command: None,
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

/// The process to spawn for every run.
struct Launch {
    program: PathBuf,
    args: Vec<OsString>,
}

/// Pins the target and builds how to launch it.
async fn launch_target(
    target: &Target,
    scenario: &Scenario,
    repo: &Path,
    work_dir: &Path,
) -> Result<(Launch, TargetRecord)> {
    match target {
        Target::Mcpls { binary } => {
            let binary = pin(&mcpls_executable(binary)?, repo).await?;
            let build = binary_record(&binary.path)?;
            let server = pin(&scenario.server.executable, repo).await?;
            let config = write_config(scenario, repo, work_dir, &server.path)?;
            let launch = Launch {
                program: binary.path.clone(),
                args: vec!["--config".into(), config.into_os_string()],
            };
            Ok((
                launch,
                TargetRecord::Mcpls {
                    binary,
                    build,
                    server,
                },
            ))
        }
        Target::External(external) => {
            let launcher = pin(&external.launcher, repo).await?;
            let launch = Launch {
                program: launcher.path.clone(),
                args: external.launch_args(repo),
            };
            Ok((
                launch,
                TargetRecord::External {
                    name: external.name.clone(),
                    launcher,
                    pinned: external.pinned.clone(),
                    verification: target.verification(),
                },
            ))
        }
    }
}

/// The scenario's ready probe, or the first probe the target can answer if it cannot answer that one.
fn ready_probe_for<'a>(scenario: &'a Scenario, target: &Target) -> Result<&'a Probe> {
    let answerable =
        |probe: &&Probe| matches!(target.request(probe, Path::new("")), Ok(ProbeCall::Call(_)));
    std::iter::once(&scenario.ready_probe)
        .chain(&scenario.probes)
        .find(answerable)
        .with_context(|| {
            format!(
                "the target supports none of the probes of scenario `{}`",
                scenario.name.as_str()
            )
        })
}

/// Removes the paths the target writes into the repository, so one run never sees another's state.
fn clear_target_state(target: &Target, repo: &Path) -> Result<()> {
    let Target::External(external) = target else {
        return Ok(());
    };
    for path in external.cleanup_paths(repo) {
        let removal = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path),
            Ok(_) => std::fs::remove_file(&path),
            Err(_) => continue,
        };
        removal.with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

/// Everything one run needs besides its index.
struct RunEnv<'a> {
    scenario: &'a Scenario,
    ready_probe: &'a Probe,
    repo: &'a Path,
    launch: &'a Launch,
    log_dir: &'a Path,
    options: &'a RunOptions,
}

/// What a session records besides its samples.
struct Recorder {
    samples: Vec<Sample>,
    memory: Vec<MemoryRecord>,
    watch: TreeWatch,
}

impl Recorder {
    const fn new(leader_pid: u32) -> Self {
        Self {
            samples: Vec::new(),
            memory: Vec::new(),
            watch: TreeWatch::new(leader_pid),
        }
    }

    async fn sample_memory(&mut self, checkpoint: MemoryCheckpoint) {
        self.memory.push(MemoryRecord {
            checkpoint,
            reading: self.watch.sample().await,
        });
    }
}

/// `<work_dir>/logs/<scenario>/<unix millis of the invocation>`, so a later invocation never overwrites these logs.
fn invocation_log_dir(work_dir: &Path, scenario: &Scenario, now: SystemTime) -> PathBuf {
    let millis = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    work_dir
        .join("logs")
        .join(scenario.name.as_str())
        .join(millis.to_string())
}

fn stderr_log_path(log_dir: &Path, index: u32) -> PathBuf {
    log_dir.join(format!("run-{index}.log"))
}

async fn one_run(index: u32, warmup: bool, env: &RunEnv<'_>) -> Result<RunRecord> {
    clear_target_state(&env.options.target, env.repo)?;
    let started = Instant::now();
    let stderr_path = stderr_log_path(env.log_dir, index);
    let mut command = Command::new(&env.launch.program);
    command
        .args(&env.launch.args)
        .current_dir(env.repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut group = ProcessGroup::spawn(command)
        .with_context(|| format!("failed to spawn {}", env.launch.program.display()))?;
    let transport = group.take_stdio()?;
    let drain = StderrDrain::start(
        group.take_stderr()?,
        &stderr_path,
        env.options.stderr_log_cap,
    )?;

    let mut recorder = Recorder::new(group.leader_pid());
    let session = drive(transport, env, started, &mut recorder).await;
    let shutdown = group.finish(session.end, &recorder.watch).await;
    let stderr_log = drain.finish(DRAIN_GRACE).await;
    clear_target_state(&env.options.target, env.repo)?;
    Ok(RunRecord {
        index,
        warmup,
        ready: session.ready,
        truncated_after_timeout: session.truncated,
        samples: recorder.samples,
        memory: recorder.memory,
        stderr_log,
        shutdown,
    })
}

struct Session {
    ready: ReadyRecord,
    truncated: bool,
    end: SessionEnd,
}

impl Session {
    /// A session that never got going: the failure is both the startup sample and the ready verdict.
    fn failed(error: String, started: Instant, recorder: &mut Recorder, end: SessionEnd) -> Self {
        let failure = Outcome::Failed { error };
        recorder.samples.push(Sample {
            region: Region::Startup,
            outcome: failure.clone(),
            elapsed_us: started.elapsed().into(),
            iteration: 0,
        });
        Self {
            ready: ReadyRecord {
                attempts: 0,
                last_failure: Some(failure),
            },
            truncated: false,
            end,
        }
    }
}

/// The tools of `target` that `client` does not export, so a drifted pin fails fast.
async fn missing_tools(client: &Client, target: &Target, timeout: Duration) -> Result<Vec<String>> {
    let Target::External(external) = target else {
        return Ok(Vec::new());
    };
    let listed = tokio::time::timeout(timeout, client.list_all_tools())
        .await
        .context("tools/list timed out")?
        .context("tools/list failed")?;
    Ok(external
        .required_tools()
        .filter(|name| !listed.iter().any(|tool| tool.name == *name))
        .map(str::to_owned)
        .collect())
}

/// Runs one MCP session, recording its samples and memory readings.
async fn drive(
    transport: (tokio::process::ChildStdout, tokio::process::ChildStdin),
    env: &RunEnv<'_>,
    started: Instant,
    recorder: &mut Recorder,
) -> Session {
    let RunEnv {
        scenario,
        ready_probe,
        repo,
        options,
        ..
    } = *env;
    let client = match ().serve(transport).await {
        Ok(client) => client,
        Err(error) => {
            return Session::failed(error.to_string(), started, recorder, SessionEnd::Abandoned);
        }
    };
    match missing_tools(&client, &options.target, options.call_timeout).await {
        Ok(missing) if missing.is_empty() => {}
        Ok(missing) => {
            let error = format!("the target does not export the tools {missing:?}");
            let end = close(client).await;
            return Session::failed(error, started, recorder, end);
        }
        Err(error) => {
            let end = close(client).await;
            return Session::failed(format!("{error:#}"), started, recorder, end);
        }
    }
    recorder.samples.push(Sample {
        region: Region::Startup,
        outcome: Outcome::Ok,
        elapsed_us: started.elapsed().into(),
        iteration: 0,
    });
    let (ready, ready_sample) =
        wait_until_ready(&client, ready_probe, repo, started, options).await;
    let is_ready = ready.last_failure.is_none();
    recorder.samples.push(ready_sample);
    let truncated = if is_ready {
        recorder.sample_memory(MemoryCheckpoint::Ready).await;
        let truncated =
            measure_probes(&client, scenario, repo, options, &mut recorder.samples).await;
        if !truncated {
            recorder.sample_memory(MemoryCheckpoint::AfterProbes).await;
        }
        truncated
    } else {
        false
    };
    // One last look before shutdown records helpers that appeared after the checkpoints.
    recorder.watch.sample().await;
    let end = close(client).await;
    Session {
        ready,
        truncated,
        end,
    }
}

async fn close(client: Client) -> SessionEnd {
    match tokio::time::timeout(SHUTDOWN_GRACE, client.cancel()).await {
        Ok(Ok(_)) => SessionEnd::Closed,
        _ => SessionEnd::Abandoned,
    }
}

/// Retries the ready probe until it passes, fails permanently, or the deadline passes.
async fn wait_until_ready(
    client: &Client,
    ready_probe: &Probe,
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
        let call = timed_call(client, ready_probe, repo, budget, 0, &options.target).await;
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
            let call = timed_call(
                client,
                probe,
                repo,
                options.call_timeout,
                iteration,
                &options.target,
            )
            .await;
            let outcome = call.sample.outcome.clone();
            samples.push(call.sample);
            match outcome {
                Outcome::TimedOut => return true,
                Outcome::Unsupported => break,
                _ => {}
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
    target: &Target,
) -> Call {
    let region = probe.region();
    let request = match target.request(probe, repo) {
        Ok(ProbeCall::Call(request)) => request,
        Ok(ProbeCall::Unsupported) => {
            return Call {
                sample: Sample {
                    region,
                    outcome: Outcome::Unsupported,
                    elapsed_us: Micros(0),
                    iteration,
                },
                permanent: true,
            };
        }
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
        Ok(Ok(result)) => match target.verdict(probe, &result) {
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
    fn stderr_logs_are_grouped_per_invocation() {
        let scenario: Scenario =
            toml::from_str(include_str!("../scenarios/smoke-fixture.toml")).unwrap();
        let at = |millis| UNIX_EPOCH + Duration::from_millis(millis);
        let first = invocation_log_dir(Path::new("/w"), &scenario, at(1_000));
        let second = invocation_log_dir(Path::new("/w"), &scenario, at(2_000));
        assert_eq!(first, Path::new("/w/logs/smoke-fixture/1000"));
        assert_ne!(first, second);
        assert_eq!(
            stderr_log_path(&first, 3),
            Path::new("/w/logs/smoke-fixture/1000/run-3.log")
        );
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
