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
    /// How the process ended.
    pub shutdown: ShutdownOutcome,
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
    /// Whether version mismatches were tolerated.
    pub allow_version_mismatch: bool,
}

/// Min/median/max of a set of successful samples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// Fastest sample.
    pub min_us: Micros,
    /// Lower median.
    pub median_us: Micros,
    /// Slowest sample.
    pub max_us: Micros,
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
    /// Samples that were not [`Outcome::Ok`].
    pub not_ok: usize,
    /// Successful samples with `iteration == 0`.
    pub first: Option<Stats>,
    /// Successful samples with `iteration > 0`.
    pub steady: Option<Stats>,
}

/// The full machine-readable result of `mcpls-bench run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    /// Scenario that was run.
    pub scenario: ScenarioName,
    /// Source provenance.
    pub source: SourceRecord,
    /// The mcpls binary under test.
    pub mcpls: PinRecord,
    /// Fingerprint of the mcpls binary under test.
    pub mcpls_binary: BinaryRecord,
    /// The language server under test.
    pub server: PinRecord,
    /// Other recorded executables.
    pub runtime: Vec<PinRecord>,
    /// Run parameters.
    pub params: RunParams,
    /// Raw runs.
    pub runs: Vec<RunRecord>,
    /// Set when later runs were skipped because a run never became ready.
    pub aborted: bool,
    /// Statistics over non-warm-up runs.
    pub summary: Vec<RegionSummary>,
}

fn stats(mut values: Vec<Micros>) -> Option<Stats> {
    values.sort_unstable();
    Some(Stats {
        min_us: *values.first()?,
        median_us: *values.get((values.len() - 1) / 2)?,
        max_us: *values.last()?,
    })
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
            RegionSummary {
                region,
                ok: ok_count,
                not_ok: in_region.len() - ok_count,
                first: ok(true),
                steady: ok(false),
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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
    fn region_without_successes_has_no_statistics() {
        let runs = [run(
            false,
            vec![sample(Region::Ready, Outcome::TimedOut, 10, 0)],
        )];
        assert_eq!(summarize(&runs)[0].first, None);
    }

    #[test]
    fn micros_saturates() {
        assert_eq!(Micros::from(Duration::MAX), Micros(u64::MAX));
    }
}
