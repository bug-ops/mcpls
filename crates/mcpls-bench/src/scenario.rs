//! Typed benchmark scenario definitions loaded from TOML.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use mcpls_core::bridge::Position2D;
use mcpls_core::config::LanguageId;
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

/// Hosts a scenario may clone from.
const ALLOWED_GIT_HOSTS: [&str; 1] = ["github.com"];

/// An `https://github.com/<owner>/<repo>` clone URL without credentials, port, query or fragment.
///
/// # Examples
///
/// ```
/// use mcpls_bench::scenario::GitUrl;
///
/// assert!(GitUrl::try_from("https://github.com/sharkdp/fd".to_owned()).is_ok());
/// assert!(GitUrl::try_from("http://github.com/sharkdp/fd".to_owned()).is_err());
/// assert!(GitUrl::try_from("https://github.com:8443/sharkdp/fd".to_owned()).is_err());
/// assert!(GitUrl::try_from("https://evil.example/sharkdp/fd".to_owned()).is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GitUrl(String);

impl GitUrl {
    /// The URL as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for GitUrl {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let reject = |why: &str| format!("git url `{value}` {why}");
        let url = url::Url::parse(&value).map_err(|error| reject(&error.to_string()))?;
        if url.as_str() != value {
            return Err(reject(
                "is not in canonical form (git and the url parser would read it differently)",
            ));
        }
        if url.scheme() != "https" {
            return Err(reject("must use https"));
        }
        if !url
            .host_str()
            .is_some_and(|host| ALLOWED_GIT_HOSTS.contains(&host))
        {
            return Err(reject(&format!("must be hosted on {ALLOWED_GIT_HOSTS:?}")));
        }
        if url.port().is_some() {
            return Err(reject("must not carry a port"));
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(reject("must not carry credentials, a query or a fragment"));
        }
        let segments = url
            .path_segments()
            .map_or(0, |s| s.filter(|part| !part.is_empty()).count());
        if segments != 2 {
            return Err(reject(
                "must have the form https://github.com/<owner>/<repo>",
            ));
        }
        Ok(Self(value))
    }
}

impl From<GitUrl> for String {
    fn from(url: GitUrl) -> Self {
        url.0
    }
}

/// Whether `text` is non-empty and made of `[a-z0-9-]` only, so it is safe as a directory name.
#[must_use]
pub fn is_slug(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
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
        if is_slug(&value) {
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
    /// The path as written, with forward slashes.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

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
        url: GitUrl,
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
    /// Separate command that prints the version, for servers that have no version flag of their own.
    #[serde(default)]
    pub version_command: Option<String>,
    /// Substring the version output must contain; mismatch aborts the run.
    #[serde(default)]
    pub expected_version: Option<String>,
}

/// The language server under test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSpec {
    /// LSP language identifier the server handles.
    pub language_id: LanguageId,
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
        /// Identifier at the position; only comparison targets that address symbols by name need it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        symbol: Option<String>,
    },
    /// `get_definition`; some location uri must end with `uri_suffix`.
    Definition {
        /// File to query.
        file: RepoPath,
        /// 1-based position.
        position: Position2D,
        /// Suffix of a returned location uri.
        uri_suffix: String,
        /// Identifier at the position; only comparison targets that address symbols by name need it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        symbol: Option<String>,
    },
    /// `get_references`; at least `min_count` locations (declaration included).
    References {
        /// File to query.
        file: RepoPath,
        /// 1-based position.
        position: Position2D,
        /// Minimum number of locations.
        min_count: usize,
        /// Identifier at the position; only comparison targets that address symbols by name need it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        symbol: Option<String>,
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

    fn bundled() -> Vec<(String, Scenario)> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios");
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            let scenario = Scenario::load(&path).unwrap();
            found.push((stem, scenario));
        }
        found
    }

    #[test]
    fn bundled_scenarios_parse_and_are_named_after_their_file() {
        let scenarios = bundled();
        assert!(scenarios.len() >= 11, "{} scenarios", scenarios.len());
        for (stem, scenario) in &scenarios {
            assert_eq!(scenario.name.as_str(), stem);
            assert!(!scenario.probes.is_empty());
        }
    }

    #[test]
    fn the_language_matrix_is_covered() {
        let names: Vec<String> = bundled().into_iter().map(|(stem, _)| stem).collect();
        for wanted in [
            "httpx-pyright",
            "httpx-ty",
            "react-hook-form-tsgo",
            "cobra-gopls",
            "fmt-clangd",
            "zls-zig-args",
            "mcp-typescript-sdk-tsls",
            "vscode-tsls",
        ] {
            assert!(names.iter().any(|n| n == wanted), "{wanted}");
        }
    }

    #[test]
    fn scenario_names_are_unique() {
        let scenarios = bundled();
        let mut names: Vec<&str> = scenarios.iter().map(|(_, s)| s.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), scenarios.len());
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
    fn git_urls_are_restricted_to_plain_github_https() {
        for bad in [
            "http://github.com/a/b",
            "ssh://git@github.com/a/b",
            "https://gitlab.com/a/b",
            "https://github.com:8443/a/b",
            "https://github.com:443/a/b",
            "https://user@github.com/a/b",
            "https://github.com/a/b?x=1",
            "https://github.com/a/b#frag",
            "https://github.com/a",
            "https://github.com/a/b/c",
            "-oProxyCommand=x",
            "https://github.com\\@evil.invalid/a",
            " https://github.com/a/b",
            "https://github.com/a/b\n",
            "https://github.com/a/\tb",
            "https://github.com/a/../b",
            "https://github.com/a/b/..",
            "HTTPS://github.com/a/b",
            "https://GitHub.com/a/b",
            "https://github.com/a b",
        ] {
            assert!(GitUrl::try_from(bad.to_owned()).is_err(), "{bad}");
        }
        assert!(GitUrl::try_from("https://github.com/a/b.git".to_owned()).is_ok());
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = SCENARIOS[2].replace("[server]", "[server]\nbogus = 1");
        assert!(toml::from_str::<Scenario>(&text).is_err());
    }
}
