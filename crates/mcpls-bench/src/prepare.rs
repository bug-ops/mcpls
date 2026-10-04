//! Repository checkout and untimed setup (`mcpls-bench prepare`).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::pin::resolve_in_path;
use crate::scenario::{CommitSha, RepoSource, Scenario, SetupStep};

/// A directory for cloned repositories that no Cargo workspace encloses.
///
/// A clone below an ancestor `Cargo.toml` would make cargo treat it as a
/// workspace member and rust-analyzer could not load it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkDir(PathBuf);

impl WorkDir {
    /// Validates `path` (creating it if absent) as a work directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be created or lies inside a Cargo workspace.
    pub fn new(path: &Path) -> Result<Self> {
        let absolute = std::path::absolute(path)
            .with_context(|| format!("failed to resolve work dir {}", path.display()))?;
        if let Some(manifest) = absolute
            .ancestors()
            .map(|dir| dir.join("Cargo.toml"))
            .find(|manifest| manifest.exists())
        {
            bail!(
                "work dir {} is inside a Cargo workspace ({}); pass --work-dir outside of it",
                absolute.display(),
                manifest.display()
            );
        }
        std::fs::create_dir_all(&absolute)
            .with_context(|| format!("failed to create work dir {}", absolute.display()))?;
        let canonical = dunce::canonicalize(&absolute)
            .with_context(|| format!("failed to resolve work dir {}", absolute.display()))?;
        Ok(Self(canonical))
    }

    /// The default location: `<cache dir>/mcpls-bench`.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform has no cache directory.
    pub fn default_path() -> Result<PathBuf> {
        Ok(dirs::cache_dir()
            .context("no platform cache directory; pass --work-dir")?
            .join("mcpls-bench"))
    }

    fn repo_dir(&self, scenario: &Scenario) -> PathBuf {
        self.0.join("repos").join(scenario.name.as_str())
    }
}

/// Makes `path` absolute without touching the filesystem.
///
/// mcpls and every setup command run with the repository as working
/// directory, so paths handed to them must not depend on the caller's.
///
/// # Errors
///
/// Returns an error when the current directory cannot be determined.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use mcpls_bench::prepare::absolute;
///
/// assert!(absolute(Path::new("work")).unwrap().is_absolute());
/// ```
pub fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("failed to absolutize {}", path.display()))
}

fn source_dir(scenario: &Scenario, scenario_dir: &Path, work_dir: &Path) -> Result<PathBuf> {
    match &scenario.source {
        RepoSource::Local { path } => Ok(scenario_dir.join(path)),
        RepoSource::Git { .. } => Ok(WorkDir::new(work_dir)?.repo_dir(scenario)),
    }
}

/// Resolves the directory of the benchmarked repository.
///
/// `Local` paths are resolved relative to `scenario_dir`; `Git` repositories
/// live under `work_dir` and must have been created by [`prepare`].
///
/// # Errors
///
/// Returns an error when the directory does not exist (run `prepare` first).
pub fn repo_dir(scenario: &Scenario, scenario_dir: &Path, work_dir: &Path) -> Result<PathBuf> {
    let dir = source_dir(scenario, scenario_dir, work_dir)?;
    if !dir.is_dir() {
        bail!(
            "repository directory {} does not exist; run `mcpls-bench prepare` first",
            dir.display()
        );
    }
    dunce::canonicalize(&dir).with_context(|| format!("failed to resolve {}", dir.display()))
}

/// Checks out the pinned repository (if `Git`) and runs the setup steps.
///
/// # Errors
///
/// Returns an error when a git or setup command fails, or an existing
/// checkout is not at the pinned commit.
pub async fn prepare(scenario: &Scenario, scenario_dir: &Path, work_dir: &Path) -> Result<PathBuf> {
    let work_dir = absolute(work_dir)?;
    let work_dir = work_dir.as_path();
    let marker = marker_path(scenario, work_dir);
    if marker.exists() {
        std::fs::remove_file(&marker)
            .with_context(|| format!("failed to remove {}", marker.display()))?;
    }
    let dir = source_dir(scenario, scenario_dir, work_dir)?;
    if let RepoSource::Git { url, commit } = &scenario.source {
        checkout(url, commit, &dir).await?;
    }
    let dir = dunce::canonicalize(&dir)
        .with_context(|| format!("failed to resolve {}", dir.display()))?;
    for step in &scenario.setup {
        run_checked(&step.command, &step.args, &dir).await?;
    }
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let text = serde_json::to_string(&PrepareMarker {
        setup: scenario.setup.clone(),
    })
    .context("failed to serialize the prepare marker")?;
    std::fs::write(&marker, text)
        .with_context(|| format!("failed to write {}", marker.display()))?;
    Ok(dir)
}

/// Record written by a successful `prepare`, proving the setup steps ran.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PrepareMarker {
    setup: Vec<SetupStep>,
}

fn marker_path(scenario: &Scenario, work_dir: &Path) -> PathBuf {
    work_dir
        .join("markers")
        .join(format!("{}.json", scenario.name.as_str()))
}

/// Verifies the repository is at the pinned commit and that `prepare` completed
/// the scenario's current setup steps.
///
/// Returns the commit observed in the checkout (`None` for a `Local` source).
///
/// # Errors
///
/// Returns an error when HEAD differs from the pinned commit, or when setup
/// steps exist and no matching `prepare` marker is present.
pub async fn verify_prepared(
    scenario: &Scenario,
    repo: &Path,
    work_dir: &Path,
) -> Result<Option<CommitSha>> {
    let observed = match &scenario.source {
        RepoSource::Git { commit, .. } => {
            let head = head_commit(repo).await?;
            if &head != commit {
                bail!(
                    "{} is at {}, scenario pins {}; delete it and re-run prepare",
                    repo.display(),
                    head.as_str(),
                    commit.as_str()
                );
            }
            Some(head)
        }
        RepoSource::Local { .. } => None,
    };
    if !scenario.setup.is_empty() {
        let path = marker_path(scenario, work_dir);
        let expected = PrepareMarker {
            setup: scenario.setup.clone(),
        };
        let found = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<PrepareMarker>(&text).ok());
        if found.as_ref() != Some(&expected) {
            bail!("setup steps were not completed by `prepare`; run `mcpls-bench prepare` first");
        }
    }
    Ok(observed)
}

async fn head_commit(dir: &Path) -> Result<CommitSha> {
    let head = git_output(dir, &["rev-parse", "HEAD"]).await?;
    CommitSha::try_from(head).map_err(anyhow::Error::msg)
}

async fn checkout(url: &str, commit: &CommitSha, dir: &Path) -> Result<()> {
    if dir.join(".git").exists() {
        let head = head_commit(dir).await?;
        if &head == commit {
            return Ok(());
        }
        bail!(
            "{} is at {}, expected {}; delete it and re-run prepare",
            dir.display(),
            head.as_str(),
            commit.as_str()
        );
    }
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    git(dir, &["init", "--quiet"]).await?;
    git(dir, &["remote", "add", "origin", url]).await?;
    git(
        dir,
        &[
            "fetch",
            "--quiet",
            "--depth",
            "1",
            "origin",
            commit.as_str(),
        ],
    )
    .await?;
    git(dir, &["checkout", "--quiet", "--detach", "FETCH_HEAD"]).await?;
    let head = head_commit(dir).await?;
    if &head != commit {
        bail!(
            "checked out {}, expected {}",
            head.as_str(),
            commit.as_str()
        );
    }
    Ok(())
}

async fn git(dir: &Path, args: &[&str]) -> Result<()> {
    run_checked("git", args, dir).await
}

async fn git_output(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(resolve_in_path("git")?)
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .context("failed to run git")?;
    if !output.status.success() {
        bail!("git {} exited with {}", args.join(" "), output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn run_checked<S: AsRef<OsStr> + Sync>(command: &str, args: &[S], dir: &Path) -> Result<()> {
    let status = Command::new(resolve_in_path(command)?)
        .args(args)
        .current_dir(dir)
        .status()
        .await
        .with_context(|| format!("failed to run `{command}`"))?;
    if !status.success() {
        let args: Vec<_> = args.iter().map(|a| a.as_ref().to_string_lossy()).collect();
        bail!("`{command} {}` exited with {status}", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn work_dir_inside_a_cargo_workspace_is_refused() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let inside = manifest_dir.join("target-work");
        assert!(WorkDir::new(&inside).is_err());
        assert!(!inside.exists());
    }

    #[tokio::test]
    async fn setup_steps_require_a_matching_prepare_marker() {
        let mut scenario: Scenario =
            toml::from_str(include_str!("../scenarios/smoke-fixture.toml")).unwrap();
        scenario.setup = vec![SetupStep {
            command: "true".to_owned(),
            args: Vec::new(),
        }];
        let work = tempfile::tempdir().unwrap();
        let repo = Path::new(".");

        let missing = verify_prepared(&scenario, repo, work.path()).await;
        assert!(missing.is_err());

        let marker = marker_path(&scenario, work.path());
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        let text = serde_json::to_string(&PrepareMarker {
            setup: scenario.setup.clone(),
        })
        .unwrap();
        std::fs::write(&marker, text).unwrap();
        assert_eq!(
            verify_prepared(&scenario, repo, work.path()).await.unwrap(),
            None
        );

        scenario.setup[0].args.push("changed".to_owned());
        assert!(verify_prepared(&scenario, repo, work.path()).await.is_err());
    }

    #[test]
    fn relative_paths_are_made_absolute() {
        assert!(absolute(Path::new("work")).unwrap().is_absolute());
    }

    #[test]
    fn work_dir_outside_any_workspace_is_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(WorkDir::new(&tmp.path().join("bench")).is_ok());
    }
}
