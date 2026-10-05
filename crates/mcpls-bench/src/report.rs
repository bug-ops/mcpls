//! Measurement records and their summary statistics.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::scenario::{CommitSha, ScenarioName};

/// An elapsed time in whole microseconds, saturating at `u64::MAX`.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use mcpls_bench::report::Micros;
///
/// assert_eq!(Micros::from(Duration::from_millis(2)), Micros(2_000));
/// assert_eq!(Micros::from(Duration::MAX), Micros(u64::MAX));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Micros(pub u64);

impl From<Duration> for Micros {
    fn from(d: Duration) -> Self {
        Self(u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
    }
}

/// A resident set size in kibibytes, as reported by `ps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Kib(pub u64);

/// Resident memory of one process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRss {
    /// Process id.
    pub pid: u32,
    /// Resident set size.
    pub rss: Kib,
    /// Executable name as printed by `ps` (`comm`).
    pub command: String,
}

/// A point of a run at which process-tree memory is sampled, never during a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCheckpoint {
    /// Right after the ready probe passed.
    Ready,
    /// After the last probe iteration.
    AfterProbes,
}

impl MemoryCheckpoint {
    /// The `snake_case` name used in the JSON report.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::AfterProbes => "after_probes",
        }
    }
}

impl fmt::Display for MemoryCheckpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// Resident memory of the mcpls process tree, summed over its members.
///
/// The tree is the parent-pid closure of mcpls, so it includes the language
/// servers and descendants that left the group with `setsid`, as long as
/// their parent is alive. The sum double-counts pages shared between processes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RssReading {
    /// The group was observed.
    Measured {
        /// Sum of the member sizes.
        total: Kib,
        /// Every member of the group.
        processes: Vec<ProcessRss>,
    },
    /// The process table could not be read, or mcpls was no longer in it.
    Unavailable {
        /// Why no reading exists.
        reason: String,
    },
}

/// One memory observation of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// Where in the run the reading was taken.
    pub checkpoint: MemoryCheckpoint,
    /// The reading.
    pub reading: RssReading,
}

/// A measured phase of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    /// Spawn until the MCP `initialize` handshake completes.
    Startup,
    /// Spawn until the scenario's ready probe first passes.
    Ready,
    /// One `get_diagnostics` call.
    Diagnostics,
    /// One `get_hover` call.
    Hover,
    /// One `get_definition` call.
    Definition,
    /// One `get_references` call.
    References,
    /// One `get_document_symbols` call.
    DocumentSymbols,
}

impl Region {
    /// The `snake_case` name used in the JSON report.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Ready => "ready",
            Self::Diagnostics => "diagnostics",
            Self::Hover => "hover",
            Self::Definition => "definition",
            Self::References => "references",
            Self::DocumentSymbols => "document_symbols",
        }
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// Result of a single measured call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    /// The call succeeded and met the probe's expectation.
    Ok,
    /// The call succeeded but the answer did not meet the expectation.
    Incorrect {
        /// What was expected versus observed.
        detail: String,
    },
    /// The call failed (transport, protocol or tool error).
    Failed {
        /// Error message.
        error: String,
    },
    /// The call did not finish within the call timeout.
    TimedOut,
    /// The target has no tool for this probe, so nothing was called.
    Unsupported,
}

/// One timed observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    /// Measured phase.
    pub region: Region,
    /// Whether the call was correct.
    pub outcome: Outcome,
    /// Wall-clock duration in microseconds.
    pub elapsed_us: Micros,
    /// Zero-based repetition of the probe within the run; always 0 for `startup` and `ready`.
    pub iteration: u32,
}

/// How the mcpls process ended after the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownOutcome {
    /// The process exited on its own within the grace period.
    Clean,
    /// The process had to be killed.
    Killed,
    /// The process exited, but members of its tree (same pid and start time) outlived it and were killed.
    OrphansKilled,
}

/// How the ready probe fared in one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyRecord {
    /// Number of ready-probe attempts until it passed, failed permanently or timed out.
    pub attempts: u32,
    /// The last failing attempt, kept only when the server never became ready.
    pub last_failure: Option<Outcome>,
}

/// One spawn-to-shutdown cycle of mcpls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    /// Zero-based run index, warm-up runs first.
    pub index: u32,
    /// Warm-up runs are kept raw but excluded from the summary.
    pub warmup: bool,
    /// Ready-probe attempts and, on failure, the reason.
    pub ready: ReadyRecord,
    /// Set when a call timed out and the remaining probes were skipped, because
    /// later calls would queue behind the abandoned one and be inflated.
    pub truncated_after_timeout: bool,
    /// All samples of the run in execution order.
    pub samples: Vec<Sample>,
    /// Process-tree memory observations in execution order.
    pub memory: Vec<MemoryRecord>,
    /// Capped capture of the stderr of the process under test.
    pub stderr_log: StderrLogRecord,
    /// How the process ended.
    pub shutdown: ShutdownOutcome,
}

/// What became of the stderr capture of one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StderrLogRecord {
    /// File receiving the stderr.
    pub path: PathBuf,
    /// Bytes written to the file (the truncation marker is not counted).
    pub written_bytes: u64,
    /// Bytes read from the process but not written because the cap was reached.
    pub dropped_bytes: u64,
    /// False when the stream had not ended after the drain grace and the copy was abandoned.
    pub drain_complete: bool,
    /// A read or write failure of the capture, if any.
    pub error: Option<String>,
}

impl StderrLogRecord {
    /// A record for a log that received nothing.
    #[must_use]
    pub fn untouched(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            written_bytes: 0,
            dropped_bytes: 0,
            drain_complete: true,
            error: None,
        }
    }
}

/// Resolved executable identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinRecord {
    /// Command as written in the scenario or CLI.
    pub requested: String,
    /// Absolute path actually executed.
    pub path: PathBuf,
    /// Output of the version command.
    pub version_output: String,
    /// Version substring the scenario required, if any.
    pub expected_version: Option<String>,
}

impl PinRecord {
    /// Whether `expected_version` is set and absent from `version_output`.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_bench::report::PinRecord;
    ///
    /// let mut record = PinRecord {
    ///     requested: "rust-analyzer".to_owned(),
    ///     path: "/bin/rust-analyzer".into(),
    ///     version_output: "rust-analyzer 1.99.0".to_owned(),
    ///     expected_version: Some("1.99".to_owned()),
    /// };
    /// assert!(!record.mismatch());
    /// record.expected_version = Some("1.98".to_owned());
    /// assert!(record.mismatch());
    /// record.expected_version = None;
    /// assert!(!record.mismatch());
    /// ```
    #[must_use]
    pub fn mismatch(&self) -> bool {
        self.expected_version
            .as_deref()
            .is_some_and(|expected| !self.version_output.contains(expected))
    }
}

/// Provenance of the benchmarked sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceRecord {
    /// Pinned to an exact commit.
    Pinned {
        /// Checked-out commit.
        commit: CommitSha,
    },
    /// An unpinned local directory.
    Unpinned {
        /// Directory that was benchmarked.
        path: PathBuf,
    },
}

/// Build profile of the mcpls binary, inferred from its parent directory name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildProfile {
    /// Located in a `debug` directory.
    Debug,
    /// Located in a `release` directory.
    Release,
    /// Any other location.
    Unknown,
}

/// Cheap fingerprint of the mcpls binary that was measured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryRecord {
    /// Build profile inferred from the path (`target/<profile>/mcpls`).
    pub profile: BuildProfile,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Modification time as seconds since the Unix epoch.
    pub modified_unix_secs: Option<u64>,
}

/// Parameters the run was executed with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunParams {
    /// Measured runs.
    pub runs: u32,
    /// Warm-up runs executed before the measured runs.
    pub warmup_runs: u32,
    /// Iterations of every probe per run.
    pub iterations: u32,
    /// Ready-probe deadline in seconds.
    pub ready_timeout_secs: u64,
    /// Per-call deadline in seconds.
    pub call_timeout_secs: u64,
    /// Pause between ready-probe attempts in milliseconds.
    pub ready_retry_interval_ms: u64,
    /// Whether version mismatches were tolerated.
    pub allow_version_mismatch: bool,
    /// Largest stderr log per run, in bytes.
    pub stderr_log_cap_bytes: u64,
}

/// Fewest samples for which a 95th percentile is distinct from the maximum.
pub const P95_MIN_SAMPLES: usize = 20;

/// Min/median/p95/max of a set of successful samples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// Fastest sample.
    pub min_us: Micros,
    /// Lower median.
    pub median_us: Micros,
    /// Nearest-rank 95th percentile; `None` below [`P95_MIN_SAMPLES`] samples.
    pub p95_us: Option<Micros>,
    /// Slowest sample.
    pub max_us: Micros,
}

/// Min/median/max of the total resident memory at one checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStats {
    /// Smallest total.
    pub min_kib: Kib,
    /// Lower median.
    pub median_kib: Kib,
    /// Largest total.
    pub max_kib: Kib,
}

/// Memory statistics of one checkpoint across measured runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySummary {
    /// Summarised checkpoint.
    pub checkpoint: MemoryCheckpoint,
    /// Readings that were [`RssReading::Measured`].
    pub measured: usize,
    /// Readings that were [`RssReading::Unavailable`].
    pub unavailable: usize,
    /// Statistics of the measured totals.
    pub total: Option<MemoryStats>,
}

/// Statistics of one region, split into the first repetition and the rest.
///
/// The first repetition of a probe in a fresh process pays one-off costs
/// (document open, lazy initialisation); `steady` excludes it. `startup` and
/// `ready` occur once per run and are therefore reported under `first`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionSummary {
    /// Summarised region.
    pub region: Region,
    /// Successful samples across measured runs.
    pub ok: usize,
    /// Samples that failed, were incorrect or timed out.
    pub not_ok: usize,
    /// Samples of probes the target has no tool for; a capability gap, not a failure.
    pub unsupported: usize,
    /// Successful samples with `iteration == 0`.
    pub first: Option<Stats>,
    /// Successful samples with `iteration > 0`.
    pub steady: Option<Stats>,
}

/// How a target's answers were checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// Decoded into mcpls's typed results.
    Structured,
    /// Searched as text for substrings and marker counts; weaker than `Structured`.
    Textual,
}

/// The system that was measured, with the identity of everything that determined its behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetRecord {
    /// mcpls itself, driving the scenario's language server.
    Mcpls {
        /// The mcpls binary under test.
        binary: PinRecord,
        /// Fingerprint of the mcpls binary under test.
        build: BinaryRecord,
        /// The language server under test.
        server: PinRecord,
    },
    /// A comparison target that brings its own language servers.
    External {
        /// Target name from its definition file.
        name: String,
        /// The executable that launches it (`uvx`, `npx`).
        launcher: PinRecord,
        /// The pinned version or commit that appears in the launch arguments.
        pinned: String,
        /// How its answers were checked.
        verification: Verification,
    },
}

impl TargetRecord {
    /// Every executable pin of the target, for version-mismatch checks.
    #[must_use]
    pub fn pins(&self) -> Vec<&PinRecord> {
        match self {
            Self::Mcpls { binary, server, .. } => vec![binary, server],
            Self::External { launcher, .. } => vec![launcher],
        }
    }
}

/// The full machine-readable result of `mcpls-bench run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    /// Scenario that was run.
    pub scenario: ScenarioName,
    /// Source provenance.
    pub source: SourceRecord,
    /// What was measured.
    pub target: TargetRecord,
    /// Other recorded executables.
    pub runtime: Vec<PinRecord>,
    /// Run parameters.
    pub params: RunParams,
    /// Raw runs.
    pub runs: Vec<RunRecord>,
    /// Set when later runs were skipped because a run never became ready.
    pub aborted: bool,
    /// Latency statistics over non-warm-up runs.
    pub summary: Vec<RegionSummary>,
    /// Memory statistics over non-warm-up runs.
    pub memory_summary: Vec<MemorySummary>,
}

/// Sorts `values` and returns `(min, lower median, max)`.
fn order_stats<T: Copy + Ord>(values: &mut [T]) -> Option<(T, T, T)> {
    values.sort_unstable();
    Some((
        *values.first()?,
        *values.get((values.len() - 1) / 2)?,
        *values.last()?,
    ))
}

fn stats(mut values: Vec<Micros>) -> Option<Stats> {
    let (min_us, median_us, max_us) = order_stats(&mut values)?;
    let p95_us = if values.len() >= P95_MIN_SAMPLES {
        values.get((values.len() * 95).div_ceil(100) - 1).copied()
    } else {
        None
    };
    Some(Stats {
        min_us,
        median_us,
        p95_us,
        max_us,
    })
}

/// Summarises the memory readings of the non-warm-up runs, one entry per observed checkpoint.
///
/// # Examples
///
/// ```
/// use mcpls_bench::report::{
///     summarize_memory, Kib, MemoryCheckpoint, MemoryRecord, ReadyRecord, RssReading, RunRecord,
///     ShutdownOutcome, StderrLogRecord,
/// };
///
/// let run = RunRecord {
///     index: 0,
///     warmup: false,
///     ready: ReadyRecord { attempts: 1, last_failure: None },
///     truncated_after_timeout: false,
///     samples: Vec::new(),
///     memory: vec![MemoryRecord {
///         checkpoint: MemoryCheckpoint::Ready,
///         reading: RssReading::Measured { total: Kib(2048), processes: Vec::new() },
///     }],
///     stderr_log: StderrLogRecord::untouched("run-0.log"),
///     shutdown: ShutdownOutcome::Clean,
/// };
/// let summary = summarize_memory(&[run]);
/// assert_eq!(summary[0].total.as_ref().unwrap().max_kib, Kib(2048));
/// ```
#[must_use]
pub fn summarize_memory(runs: &[RunRecord]) -> Vec<MemorySummary> {
    let records: Vec<&MemoryRecord> = runs
        .iter()
        .filter(|r| !r.warmup)
        .flat_map(|r| &r.memory)
        .collect();
    let mut checkpoints: Vec<MemoryCheckpoint> = records.iter().map(|m| m.checkpoint).collect();
    checkpoints.sort_unstable();
    checkpoints.dedup();

    checkpoints
        .into_iter()
        .map(|checkpoint| {
            let mut totals: Vec<Kib> = records
                .iter()
                .filter(|m| m.checkpoint == checkpoint)
                .filter_map(|m| match &m.reading {
                    RssReading::Measured { total, .. } => Some(*total),
                    RssReading::Unavailable { .. } => None,
                })
                .collect();
            let observed = records
                .iter()
                .filter(|m| m.checkpoint == checkpoint)
                .count();
            let measured = totals.len();
            MemorySummary {
                checkpoint,
                measured,
                unavailable: observed - measured,
                total: order_stats(&mut totals).map(|(min_kib, median_kib, max_kib)| MemoryStats {
                    min_kib,
                    median_kib,
                    max_kib,
                }),
            }
        })
        .collect()
}

/// Summarises the non-warm-up samples of `runs`, one entry per observed region.
///
/// The median is the lower median, so it is always an observed value.
///
/// # Examples
///
/// ```
/// use mcpls_bench::report::{
///     summarize, Micros, Outcome, ReadyRecord, Region, RunRecord, Sample, ShutdownOutcome,
///     StderrLogRecord,
/// };
///
/// let sample = |iteration, us| Sample {
///     region: Region::Hover,
///     outcome: Outcome::Ok,
///     elapsed_us: Micros(us),
///     iteration,
/// };
/// let run = RunRecord {
///     index: 0,
///     warmup: false,
///     ready: ReadyRecord { attempts: 1, last_failure: None },
///     truncated_after_timeout: false,
///     samples: vec![sample(0, 900), sample(1, 30), sample(2, 10), sample(3, 20)],
///     memory: Vec::new(),
///     stderr_log: StderrLogRecord::untouched("run-0.log"),
///     shutdown: ShutdownOutcome::Clean,
/// };
/// let summary = summarize(&[run]);
/// assert_eq!(summary[0].first.as_ref().unwrap().max_us, Micros(900));
/// assert_eq!(summary[0].steady.as_ref().unwrap().median_us, Micros(20));
/// ```
#[must_use]
pub fn summarize(runs: &[RunRecord]) -> Vec<RegionSummary> {
    let samples: Vec<&Sample> = runs
        .iter()
        .filter(|r| !r.warmup)
        .flat_map(|r| &r.samples)
        .collect();
    let mut regions: Vec<Region> = samples.iter().map(|s| s.region).collect();
    regions.sort_unstable();
    regions.dedup();

    regions
        .into_iter()
        .map(|region| {
            let in_region: Vec<&&Sample> = samples.iter().filter(|s| s.region == region).collect();
            let ok = |first: bool| {
                stats(
                    in_region
                        .iter()
                        .filter(|s| s.outcome == Outcome::Ok && (s.iteration == 0) == first)
                        .map(|s| s.elapsed_us)
                        .collect(),
                )
            };
            let ok_count = in_region
                .iter()
                .filter(|s| s.outcome == Outcome::Ok)
                .count();
            let unsupported = in_region
                .iter()
                .filter(|s| s.outcome == Outcome::Unsupported)
                .count();
            RegionSummary {
                region,
                ok: ok_count,
                not_ok: in_region.len() - ok_count - unsupported,
                unsupported,
                first: ok(true),
                steady: ok(false),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(region: Region, outcome: Outcome, us: u64, iteration: u32) -> Sample {
        Sample {
            region,
            outcome,
            elapsed_us: Micros(us),
            iteration,
        }
    }

    fn run(warmup: bool, samples: Vec<Sample>) -> RunRecord {
        RunRecord {
            index: 0,
            warmup,
            ready: ReadyRecord {
                attempts: 1,
                last_failure: None,
            },
            truncated_after_timeout: false,
            samples,
            memory: Vec::new(),
            stderr_log: StderrLogRecord::untouched("run.log"),
            shutdown: ShutdownOutcome::Clean,
        }
    }

    #[test]
    fn warmup_runs_are_excluded() {
        let runs = [
            run(true, vec![sample(Region::Hover, Outcome::Ok, 999, 0)]),
            run(false, vec![sample(Region::Hover, Outcome::Ok, 5, 0)]),
        ];
        let summary = summarize(&runs);
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].first.as_ref().unwrap().max_us, Micros(5));
    }

    #[test]
    fn first_iteration_is_kept_out_of_steady_state() {
        let runs = [run(
            false,
            vec![
                sample(Region::Hover, Outcome::Ok, 3_000, 0),
                sample(Region::Hover, Outcome::Ok, 10, 1),
            ],
        )];
        let summary = summarize(&runs);
        assert_eq!(summary[0].steady.as_ref().unwrap().max_us, Micros(10));
        assert_eq!(summary[0].first.as_ref().unwrap().max_us, Micros(3_000));
    }

    #[test]
    fn failures_are_counted_not_timed() {
        let runs = [run(
            false,
            vec![
                sample(Region::Hover, Outcome::Ok, 7, 0),
                sample(Region::Hover, Outcome::TimedOut, 1_000_000, 1),
            ],
        )];
        let summary = summarize(&runs);
        assert_eq!(summary[0].ok, 1);
        assert_eq!(summary[0].not_ok, 1);
        assert_eq!(summary[0].steady, None);
    }

    #[test]
    fn unsupported_probes_are_not_failures() {
        let runs = [run(
            false,
            vec![
                sample(Region::Hover, Outcome::Unsupported, 0, 0),
                sample(Region::Hover, Outcome::TimedOut, 5, 1),
                sample(Region::Hover, Outcome::Ok, 7, 2),
            ],
        )];
        let summary = summarize(&runs);
        assert_eq!(
            (summary[0].ok, summary[0].not_ok, summary[0].unsupported),
            (1, 1, 1)
        );
    }

    #[test]
    fn region_without_successes_has_no_statistics() {
        let runs = [run(
            false,
            vec![sample(Region::Ready, Outcome::TimedOut, 10, 0)],
        )];
        assert_eq!(summarize(&runs)[0].first, None);
    }

    #[test]
    fn checkpoint_display_matches_the_json_name() {
        for checkpoint in [MemoryCheckpoint::Ready, MemoryCheckpoint::AfterProbes] {
            assert_eq!(
                serde_json::to_string(&checkpoint).unwrap(),
                format!("\"{checkpoint}\"")
            );
        }
    }

    #[test]
    fn p95_needs_enough_samples_and_is_an_observed_value() {
        let few: Vec<Micros> = (1..=19).map(Micros).collect();
        assert_eq!(stats(few).unwrap().p95_us, None);
        let enough: Vec<Micros> = (1..=20).map(Micros).collect();
        assert_eq!(stats(enough).unwrap().p95_us, Some(Micros(19)));
        let many: Vec<Micros> = (1..=100).map(Micros).collect();
        assert_eq!(stats(many).unwrap().p95_us, Some(Micros(95)));
    }

    #[test]
    fn memory_summary_counts_unavailable_readings() {
        let reading = |reading| MemoryRecord {
            checkpoint: MemoryCheckpoint::Ready,
            reading,
        };
        let mut record = run(false, Vec::new());
        record.memory = vec![
            reading(RssReading::Measured {
                total: Kib(10),
                processes: Vec::new(),
            }),
            reading(RssReading::Unavailable {
                reason: "no ps".to_owned(),
            }),
        ];
        let summary = summarize_memory(&[record]);
        assert_eq!(summary.len(), 1);
        assert_eq!((summary[0].measured, summary[0].unavailable), (1, 1));
        assert_eq!(summary[0].total.as_ref().unwrap().median_kib, Kib(10));
    }

    #[test]
    fn shutdown_outcomes_serialize_in_snake_case() {
        let json = |outcome| serde_json::to_string(&outcome).unwrap();
        assert_eq!(json(ShutdownOutcome::OrphansKilled), "\"orphans_killed\"");
        assert_eq!(json(ShutdownOutcome::Clean), "\"clean\"");
        assert_eq!(json(ShutdownOutcome::Killed), "\"killed\"");
    }

    #[test]
    fn micros_saturates() {
        assert_eq!(Micros::from(Duration::MAX), Micros(u64::MAX));
    }
}
