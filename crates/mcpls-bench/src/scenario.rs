//! Typed benchmark scenario definitions loaded from TOML.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use mcpls_core::bridge::Position2D;
use serde::{Deserialize, Serialize};

/// A full 40-character lowercase hexadecimal git commit id.
///
/// # Examples
///
/// ```
/// use mcpls_bench::scenario::CommitSha;
///
/// assert!(CommitSha::try_from("4f81778774463bf414a184cbe6d5219ad2229646".to_owned()).is_ok());
/// assert!(CommitSha::try_from("v10.5.0".to_owned()).is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommitSha(String);

impl CommitSha {
    /// The commit id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CommitSha {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let valid = value.len() == 40
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(value))
        } else {
            Err(format!(
                "`{value}` is not a full 40-char lowercase hex commit sha"
            ))
        }
    }
}

impl From<CommitSha> for String {
    fn from(sha: CommitSha) -> Self {
        sha.0
    }
}

/// A scenario identifier safe to use as a directory name (`[a-z0-9-]+`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ScenarioName(String);

impl ScenarioName {
    /// The name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ScenarioName {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let valid = !value.is_empty()
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if valid {
            Ok(Self(value))
        } else {
            Err(format!("scenario name `{value}` must match [a-z0-9-]+"))
        }
    }
}

impl From<ScenarioName> for String {
    fn from(name: ScenarioName) -> Self {
        name.0
    }
}

/// A relative path inside the benchmarked repository (no root, no `..`).
///
/// # Examples
///
/// ```
/// use mcpls_bench::scenario::RepoPath;
///
/// assert!(RepoPath::try_from("src/main.rs".to_owned()).is_ok());
/// assert!(RepoPath::try_from("../etc/passwd".to_owned()).is_err());
/// assert!(RepoPath::try_from("/etc/passwd".to_owned()).is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoPath(String);

impl RepoPath {
    /// Resolves this path against the repository root.
    #[must_use]
    pub fn in_repo(&self, repo: &Path) -> PathBuf {
        repo.join(&self.0)
    }
}

impl TryFrom<String> for RepoPath {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let path = Path::new(&value);
        let escapes = path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        });
        if value.is_empty() || escapes {
            Err(format!("`{value}` is not a path relative to the repo root"))
        } else {
            Ok(Self(value))
        }
    }
}

impl From<RepoPath> for String {
    fn from(path: RepoPath) -> Self {
        path.0
    }
}

/// Where the benchmarked repository comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepoSource {
    /// A git repository pinned to an exact commit.
    Git {
        /// Clone URL.
        url: String,
        /// Exact commit that is checked out.
        commit: CommitSha,
    },
    /// An unpinned local directory, resolved relative to the scenario file.
    Local {
        /// Directory path, relative to the scenario file's directory.
        path: PathBuf,
    },
}

/// An external executable whose resolved path and version are recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Executable {
    /// Command name looked up on `PATH`.
    pub command: String,
    /// Arguments that make the executable print its version.
    #[serde(default)]
    pub version_args: Vec<String>,
    /// Substring the version output must contain; mismatch aborts the run.
    #[serde(default)]
    pub expected_version: Option<String>,
}

/// The language server under test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSpec {
    /// LSP language identifier the server handles.
    pub language_id: String,
    /// Glob patterns of files routed to this server.
    pub file_patterns: Vec<String>,
    /// Arguments passed to the server on spawn.
    #[serde(default)]
    pub args: Vec<String>,
    /// The server executable and its version pin.
    pub executable: Executable,
}

/// A command that must succeed in the repository during `prepare`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupStep {
    /// Command name looked up on `PATH`.
    pub command: String,
    /// Command arguments.
    #[serde(default)]
    pub args: Vec<String>,
}

/// What a diagnostics probe must observe to count as correct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DiagnosticsExpect {
    /// No diagnostic of error severity.
    NoErrors,
    /// At least `count` diagnostics of any severity.
    AtLeast {
        /// Minimum number of diagnostics.
        count: usize,
    },
}

/// A semantic query against mcpls together with the answer it must produce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Probe {
    /// `get_hover`; the contents must contain `contains`.
    Hover {
        /// File to query.
        file: RepoPath,
        /// 1-based position.
        position: Position2D,
        /// Substring of the hover text.
        contains: String,
    },
    /// `get_definition`; some location uri must end with `uri_suffix`.
    Definition {
        /// File to query.
        file: RepoPath,
        /// 1-based position.
        position: Position2D,
        /// Suffix of a returned location uri.
        uri_suffix: String,
    },
    /// `get_references`; at least `min_count` locations (declaration included).
    References {
        /// File to query.
        file: RepoPath,
        /// 1-based position.
        position: Position2D,
        /// Minimum number of locations.
        min_count: usize,
    },
    /// `get_document_symbols`; a symbol named `symbol` must exist.
    DocumentSymbols {
        /// File to query.
        file: RepoPath,
        /// Name of a (possibly nested) symbol.
        symbol: String,
    },
    /// `get_diagnostics`; only for servers that support pull diagnostics.
    Diagnostics {
        /// File to query.
        file: RepoPath,
        /// Expected diagnostics.
        expect: DiagnosticsExpect,
    },
}

/// A complete benchmark scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Scenario identifier; also the repository directory name.
    pub name: ScenarioName,
    /// Repository source.
    pub source: RepoSource,
    /// Language server under test.
    pub server: ServerSpec,
    /// Other executables (toolchains) whose version is recorded.
    #[serde(default)]
    pub runtime: Vec<Executable>,
    /// Commands executed during `prepare`, not timed.
    #[serde(default)]
    pub setup: Vec<SetupStep>,
    /// Probe that defines "the server is ready"; retried until it passes.
    pub ready_probe: Probe,
    /// Probes measured per iteration after readiness.
    pub probes: Vec<Probe>,
}

impl Scenario {
    /// Loads and validates a scenario from a TOML file.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read scenario {}", path.display()))?;
        let scenario: Self = toml::from_str(&text)
            .with_context(|| format!("failed to parse scenario {}", path.display()))?;
        if scenario.probes.is_empty() {
            bail!("scenario `{}` has no probes", scenario.name.as_str());
        }
        Ok(scenario)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SCENARIOS: [&str; 3] = [
        include_str!("../scenarios/fd-rust-analyzer.toml"),
        include_str!("../scenarios/react-hook-form-tsls.toml"),
        include_str!("../scenarios/smoke-fixture.toml"),
    ];

    #[test]
    fn bundled_scenarios_parse() {
        for text in SCENARIOS {
            let scenario: Scenario = toml::from_str(text).unwrap();
            assert!(!scenario.probes.is_empty());
        }
    }

    #[test]
    fn typescript_scenario_has_no_diagnostics_probe() {
        let scenario: Scenario = toml::from_str(SCENARIOS[1]).unwrap();
        assert!(
            !scenario
                .probes
                .iter()
                .any(|p| matches!(p, Probe::Diagnostics { .. }))
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = SCENARIOS[2].replace("[server]", "[server]\nbogus = 1");
        assert!(toml::from_str::<Scenario>(&text).is_err());
    }
}
