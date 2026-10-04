//! `mcpls-bench` command-line entry point.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use mcpls_bench::prepare::{WorkDir, prepare};
use mcpls_bench::report::{Kib, Micros, RunReport};
use mcpls_bench::run::{RunOptions, run};
use mcpls_bench::scenario::Scenario;
use mcpls_bench::signals::ShutdownSignals;
use mcpls_bench::stderr_log::{DEFAULT_CAP_MIB, StderrLogCap};
use mcpls_bench::target::{ExternalTarget, Target};

#[derive(Debug, Parser)]
#[command(name = "mcpls-bench", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, clap::Args)]
struct ScenarioArgs {
    /// Scenario TOML file.
    scenario: PathBuf,
    /// Directory for cloned repositories and generated configs; must not be inside a Cargo workspace.
    #[arg(long)]
    work_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check out the pinned repository and run the untimed setup steps.
    Prepare(ScenarioArgs),
    /// Measure mcpls against a prepared scenario and print a JSON report.
    Run {
        #[command(flatten)]
        scenario: ScenarioArgs,
        /// The mcpls binary; defaults to the `mcpls` next to this executable (never one from PATH).
        #[arg(long, conflicts_with = "target")]
        mcpls: Option<PathBuf>,
        /// Measure a comparison MCP server from this target definition instead of mcpls.
        #[arg(long)]
        target: Option<PathBuf>,
        /// Largest stderr log kept per run, in MiB; the rest is drained and dropped.
        #[arg(long, default_value_t = DEFAULT_CAP_MIB, value_parser = clap::value_parser!(u32).range(1..=4096))]
        stderr_log_max_mib: u32,
        /// Measured runs, each a fresh mcpls process.
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(..=1000))]
        runs: u32,
        /// Warm-up runs executed first and excluded from the summary.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(..=1000))]
        warmup_runs: u32,
        /// Iterations of every probe per run.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=100_000))]
        iterations: u32,
        /// Seconds to wait for the ready probe to pass.
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=86_400))]
        ready_timeout_secs: u64,
        /// Seconds before a single tool call counts as timed out.
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=86_400))]
        call_timeout_secs: u64,
        /// Continue when an executable's version differs from the scenario pin.
        #[arg(long)]
        allow_version_mismatch: bool,
        /// Also write the JSON report to this file.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

fn default_mcpls() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate the current executable")?;
    let sibling = exe.with_file_name(format!("mcpls{}", std::env::consts::EXE_SUFFIX));
    if sibling.is_file() {
        Ok(sibling)
    } else {
        bail!(
            "no `mcpls` next to {}; build it (`cargo build -p mcpls`) or pass --mcpls",
            exe.display()
        )
    }
}

fn scenario_dir(path: &Path) -> Result<PathBuf> {
    let dir = dunce::canonicalize(path)
        .with_context(|| format!("failed to resolve {}", path.display()))?
        .parent()
        .map(Path::to_path_buf)
        .context("scenario path has no parent directory")?;
    Ok(dir)
}

fn print_summary(report: &RunReport) {
    let cell = |us: Option<Micros>| us.map_or_else(|| "-".to_owned(), |m| m.0.to_string());
    eprintln!(
        "{:<16} {:>4} {:>6} {:>6} {:>11} {:>10} {:>10} {:>10} {:>10}",
        "region",
        "ok",
        "not_ok",
        "unsupp",
        "first_med",
        "steady_min",
        "steady_med",
        "steady_p95",
        "steady_max"
    );
    for row in &report.summary {
        eprintln!(
            "{:<16} {:>4} {:>6} {:>6} {:>11} {:>10} {:>10} {:>10} {:>10}",
            row.region,
            row.ok,
            row.not_ok,
            row.unsupported,
            cell(row.first.as_ref().map(|s| s.median_us)),
            cell(row.steady.as_ref().map(|s| s.min_us)),
            cell(row.steady.as_ref().map(|s| s.median_us)),
            cell(row.steady.as_ref().and_then(|s| s.p95_us)),
            cell(row.steady.as_ref().map(|s| s.max_us)),
        );
    }
    for row in &report.memory_summary {
        let total = |kib: Option<Kib>| kib.map_or_else(|| "-".to_owned(), |k| k.0.to_string());
        eprintln!(
            "rss {}: measured {} unavailable {} min_kib {} median_kib {} max_kib {}",
            row.checkpoint,
            row.measured,
            row.unavailable,
            total(row.total.as_ref().map(|t| t.min_kib)),
            total(row.total.as_ref().map(|t| t.median_kib)),
            total(row.total.as_ref().map(|t| t.max_kib)),
        );
    }
    if report.aborted {
        eprintln!("ABORTED: the server never became ready; later runs were skipped");
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let command = Cli::parse().command;
    let mut signals = match ShutdownSignals::install() {
        Ok(signals) => signals,
        Err(error) => {
            eprintln!("Error: failed to install signal handlers: {error}");
            return ExitCode::FAILURE;
        }
    };
    // Dropping `execute` on a signal drops the active process-group guard, which kills mcpls and its tree.
    tokio::select! {
        result = execute(command) => match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Error: {error:?}");
                ExitCode::FAILURE
            }
        },
        signal = signals.recv() => {
            eprintln!("received {signal}; stopped the benchmark and its process tree");
            ExitCode::from(signal.exit_code())
        }
    }
}

async fn execute(command: Command) -> Result<()> {
    match command {
        Command::Prepare(args) => {
            let scenario = Scenario::load(&args.scenario)?;
            let work_dir = args.work_dir.map_or_else(WorkDir::default_path, Ok)?;
            let repo = prepare(&scenario, &scenario_dir(&args.scenario)?, &work_dir).await?;
            eprintln!("prepared {}", repo.display());
        }
        Command::Run {
            scenario: args,
            mcpls,
            target,
            stderr_log_max_mib,
            runs,
            warmup_runs,
            iterations,
            ready_timeout_secs,
            call_timeout_secs,
            allow_version_mismatch,
            output,
        } => {
            let scenario = Scenario::load(&args.scenario)?;
            let work_dir = args.work_dir.map_or_else(WorkDir::default_path, Ok)?;
            let options = RunOptions {
                target: match target {
                    Some(path) => Target::External(Box::new(ExternalTarget::load(&path)?)),
                    None => Target::Mcpls {
                        binary: mcpls.map_or_else(default_mcpls, Ok)?,
                    },
                },
                stderr_log_cap: StderrLogCap::from_mib(stderr_log_max_mib),
                runs,
                warmup_runs,
                iterations,
                ready_timeout: Duration::from_secs(ready_timeout_secs),
                call_timeout: Duration::from_secs(call_timeout_secs),
                allow_version_mismatch,
            };
            let report = run(
                &scenario,
                &scenario_dir(&args.scenario)?,
                &work_dir,
                &options,
            )
            .await?;
            let json =
                serde_json::to_string_pretty(&report).context("failed to serialize report")?;
            if let Some(path) = output {
                std::fs::write(&path, &json)
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }
            println!("{json}");
            print_summary(&report);
        }
    }
    Ok(())
}
