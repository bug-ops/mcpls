//! Repository checkout and untimed setup (`mcpls-bench prepare`).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::pin::resolve_in_path;
use crate::scenario::{CommitSha, GitUrl, RepoSource, Scenario, SetupStep};

const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// Names among `vars` that git reads as configuration, in any letter case.
fn inherited_git_vars(vars: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    vars.into_iter()
        .filter(|name| {
            name.to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
        })
        .collect()
}

/// A file or directory in an ancestor that changes how the language servers resolve a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceMarker {
    /// Makes cargo treat a clone below it as a workspace member.
    CargoManifest,
    /// Makes pnpm treat a clone below it as a workspace member.
    PnpmWorkspace,
    /// Its `@types` are picked up by every TypeScript project below it.
    NodeModules,
}

impl WorkspaceMarker {
    const ALL: [Self; 3] = [Self::CargoManifest, Self::PnpmWorkspace, Self::NodeModules];

    const fn file_name(self) -> &'static str {
        match self {
            Self::CargoManifest => "Cargo.toml",
            Self::PnpmWorkspace => "pnpm-workspace.yaml",
            Self::NodeModules => "node_modules",
        }
    }
}

fn find_marker(dir: &Path) -> Option<PathBuf> {
    WorkspaceMarker::ALL
        .iter()
        .map(|marker| dir.join(marker.file_name()))
        .find(|candidate| candidate.exists())
}

/// Splits `path` into its deepest existing ancestor-or-self and the missing components below it.
///
/// The missing components are returned outermost last. Fails on a `..` below the existing part.
fn split_existing(path: &Path) -> Result<(&Path, Vec<&OsStr>)> {
    let mut existing = path;
    let mut missing = Vec::new();
    while !existing.exists() {
        let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else {
            bail!(
                "work dir {} does not exist and cannot be created below a `..` component",
                path.display()
            );
        };
        missing.push(name);
        existing = parent;
    }
    Ok((existing, missing))
}

/// A directory for cloned repositories that no Cargo, pnpm or `node_modules` project encloses.
///
/// A clone below such an ancestor would be adopted by it: cargo would treat the
/// clone as a workspace member and rust-analyzer could not load it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkDir(PathBuf);

impl WorkDir {
    /// Validates `path` as a work directory and creates it if absent.
    ///
    /// The ancestors are checked before anything is created, so a refused
    /// path leaves no directory behind.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be created or lies inside a
    /// Cargo workspace, a pnpm workspace or a `node_modules` directory.
    pub fn new(path: &Path) -> Result<Self> {
        let target = Self::validate(path)?;
        std::fs::create_dir_all(&target.0)
            .with_context(|| format!("failed to create work dir {}", target.0.display()))?;
        let canonical = dunce::canonicalize(&target.0)
            .with_context(|| format!("failed to resolve work dir {}", target.0.display()))?;
        if canonical != target.0 {
            bail!(
                "work dir {} resolved to {} after creation",
                target.0.display(),
                canonical.display()
            );
        }
        Ok(target)
    }

    /// Validates `path` exactly like [`new`](Self::new) but creates nothing.
    ///
    /// # Errors
    ///
    /// Returns an error when the path lies inside a Cargo workspace, a pnpm
    /// workspace or a `node_modules` directory, or cannot be resolved.
    pub fn validate(path: &Path) -> Result<Self> {
        let absolute = absolute(path)?;
        let (existing, missing) = split_existing(&absolute)?;
        let base = dunce::canonicalize(existing)
            .with_context(|| format!("failed to resolve work dir {}", existing.display()))?;
        let target = missing.iter().rev().fold(base, |dir, name| dir.join(name));
        for dir in target.ancestors() {
            if let Some(marker) = find_marker(dir) {
                bail!(
                    "work dir {} is inside a project ({}); pass --work-dir outside of it",
                    target.display(),
                    marker.display()
                );
            }
        }
        Ok(Self(target))
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

fn source_dir(
    scenario: &Scenario,
    scenario_dir: &Path,
    work_dir: impl FnOnce() -> Result<WorkDir>,
) -> Result<PathBuf> {
    match &scenario.source {
        RepoSource::Local { path } => Ok(scenario_dir.join(path)),
        RepoSource::Git { .. } => Ok(work_dir()?.repo_dir(scenario)),
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
    let dir = source_dir(scenario, scenario_dir, || WorkDir::validate(work_dir))?;
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
    let dir = source_dir(scenario, scenario_dir, || WorkDir::new(work_dir))?;
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

/// The commit HEAD resolves to; `None` when git ran and HEAD is unborn (exit status 1 of `--quiet`).
///
/// Every other failure, including git being unavailable, is an error.
async fn try_head_commit(dir: &Path) -> Result<Option<CommitSha>> {
    let output = git_command(dir)?
        .args(["--git-dir=.git", "rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .await
        .context("failed to run git")?;
    match output.status.code() {
        Some(0) => {
            let head = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            CommitSha::try_from(head)
                .map(Some)
                .map_err(anyhow::Error::msg)
        }
        Some(1) => Ok(None),
        _ => bail!(
            "git rev-parse HEAD in {} exited with {}",
            dir.display(),
            output.status
        ),
    }
}

async fn head_commit(dir: &Path) -> Result<CommitSha> {
    try_head_commit(dir)
        .await?
        .with_context(|| format!("{} has no commit checked out", dir.display()))
}

/// State of the final checkout directory before `prepare` touches it.
#[derive(Debug, PartialEq, Eq)]
enum Existing {
    Absent,
    Matching,
    WrongCommit(CommitSha),
    /// An interrupted clone of an earlier version: a `.git` without a resolvable HEAD, or an empty directory.
    Partial,
}

fn ensure_not_symlink(path: &Path) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        bail!(
            "{} is a symlink; remove it and re-run prepare",
            path.display()
        );
    }
    Ok(())
}

async fn inspect(dir: &Path, commit: &CommitSha) -> Result<Existing> {
    ensure_not_symlink(dir)?;
    if !dir.exists() {
        return Ok(Existing::Absent);
    }
    if !dir.join(".git").exists() {
        let empty = dir
            .read_dir()
            .with_context(|| format!("failed to read {}", dir.display()))?
            .next()
            .is_none();
        if empty {
            return Ok(Existing::Partial);
        }
        bail!(
            "{} exists but is not a git checkout; delete it and re-run prepare",
            dir.display()
        );
    }
    Ok(match try_head_commit(dir).await? {
        Some(head) if &head == commit => Existing::Matching,
        Some(head) => Existing::WrongCommit(head),
        None => Existing::Partial,
    })
}

fn staging_dir(dir: &Path) -> Result<PathBuf> {
    let name = dir
        .file_name()
        .and_then(OsStr::to_str)
        .with_context(|| format!("{} has no directory name", dir.display()))?;
    let parent = dir
        .parent()
        .with_context(|| format!("{} has no parent directory", dir.display()))?;
    Ok(parent.join(format!(".{name}.partial")))
}

fn remote_add_args(url: &GitUrl) -> [&str; 5] {
    ["remote", "add", "--", "origin", url.as_str()]
}

fn fetch_args(commit: &CommitSha) -> [&str; 7] {
    [
        "fetch",
        "--quiet",
        "--depth",
        "1",
        "--",
        "origin",
        commit.as_str(),
    ]
}

async fn checkout(url: &GitUrl, commit: &CommitSha, dir: &Path) -> Result<()> {
    match inspect(dir, commit).await? {
        Existing::Matching => return Ok(()),
        Existing::WrongCommit(head) => bail!(
            "{} is at {}, expected {}; delete it and re-run prepare",
            dir.display(),
            head.as_str(),
            commit.as_str()
        ),
        Existing::Partial => {
            eprintln!("removing the incomplete checkout {}", dir.display());
            std::fs::remove_dir_all(dir)
                .with_context(|| format!("failed to remove {}", dir.display()))?;
        }
        Existing::Absent => {}
    }
    let staging = staging_dir(dir)?;
    ensure_not_symlink(&staging)?;
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("failed to remove {}", staging.display()))?;
    }
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("failed to create {}", staging.display()))?;
    git(&staging, &["init", "--quiet"]).await?;
    git(&staging, &remote_add_args(url)).await?;
    git(&staging, &fetch_args(commit)).await?;
    git(&staging, &["checkout", "--quiet", "--detach", "FETCH_HEAD"]).await?;
    let head = head_commit(&staging).await?;
    if &head != commit {
        bail!(
            "checked out {}, expected {}",
            head.as_str(),
            commit.as_str()
        );
    }
    std::fs::rename(&staging, dir)
        .with_context(|| format!("failed to move {} to {}", staging.display(), dir.display()))
}

/// The single way git is invoked: no inherited repository location, no user or
/// system configuration (so no `insteadOf` rewrites), https only, no prompts.
fn git_command(dir: &Path) -> Result<Command> {
    let mut command = Command::new(resolve_in_path("git")?);
    for name in inherited_git_vars(std::env::vars_os().map(|(name, _)| name)) {
        command.env_remove(name);
    }
    command
        .args([
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
        ])
        .current_dir(dir)
        .kill_on_drop(true)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("GIT_TERMINAL_PROMPT", "0");
    Ok(command)
}

async fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let status = git_command(dir)?
        .args(args)
        .status()
        .await
        .context("failed to run git")?;
    if !status.success() {
        bail!("git {} exited with {status}", args.join(" "));
    }
    Ok(())
}

async fn run_checked<S: AsRef<OsStr> + Sync>(command: &str, args: &[S], dir: &Path) -> Result<()> {
    let status = Command::new(resolve_in_path(command)?)
        .args(args)
        .current_dir(dir)
        .kill_on_drop(true)
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

    fn commit_in(dir: &Path) -> CommitSha {
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args(["commit", "--quiet", "--allow-empty", "-m", "x"])
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()
            .unwrap();
        assert!(status.success());
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        CommitSha::try_from(String::from_utf8(out.stdout).unwrap().trim().to_owned()).unwrap()
    }

    fn some_commit() -> CommitSha {
        CommitSha::try_from("0".repeat(40)).unwrap()
    }

    #[test]
    fn work_dir_inside_a_cargo_workspace_is_refused() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let inside = manifest_dir.join("target-work");
        assert!(WorkDir::new(&inside).is_err());
        assert!(!inside.exists());
    }

    #[test]
    fn markers_refuse_a_work_dir_without_creating_it() {
        for marker in ["Cargo.toml", "pnpm-workspace.yaml", "node_modules"] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(marker);
            if marker == "node_modules" {
                std::fs::create_dir(&path).unwrap();
            } else {
                std::fs::write(&path, "").unwrap();
            }
            let target = tmp.path().join("deep/er/bench");
            assert!(WorkDir::new(&target).is_err(), "{marker}");
            assert!(!tmp.path().join("deep").exists(), "{marker}");
        }
    }

    #[test]
    fn validation_alone_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("a/b");
        assert!(WorkDir::validate(&target).is_ok());
        assert!(!tmp.path().join("a").exists());
    }

    #[test]
    fn a_bare_ancestor_package_json_is_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        assert!(WorkDir::new(&tmp.path().join("bench")).is_ok());
    }

    #[test]
    fn missing_components_are_created_and_dotdot_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let work = WorkDir::new(&tmp.path().join("a/b/c")).unwrap();
        assert!(work.0.is_dir());
        assert!(WorkDir::new(&tmp.path().join("missing/../x")).is_err());
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

    #[test]
    fn staging_dir_is_a_hidden_sibling() {
        assert_eq!(
            staging_dir(Path::new("/w/repos/fd")).unwrap(),
            Path::new("/w/repos/.fd.partial")
        );
    }

    #[test]
    fn positional_git_arguments_follow_a_double_dash() {
        let url = GitUrl::try_from("https://github.com/a/b".to_owned()).unwrap();
        let sha = some_commit();
        let remote = remote_add_args(&url);
        assert_eq!(remote[2], "--");
        assert_eq!(&remote[3..], ["origin", url.as_str()]);
        let fetch = fetch_args(&sha);
        assert_eq!(fetch[4], "--");
        assert_eq!(&fetch[5..], ["origin", sha.as_str()]);
    }

    #[test]
    fn git_runs_without_inherited_repository_location_or_user_config() {
        let command = git_command(Path::new(".")).unwrap();
        let envs: std::collections::HashMap<_, _> = command.as_std().get_envs().collect();
        assert_eq!(
            envs.get(OsStr::new("GIT_CONFIG_GLOBAL")),
            Some(&Some(OsStr::new(NULL_DEVICE)))
        );
        assert_eq!(
            envs.get(OsStr::new("GIT_CONFIG_NOSYSTEM")),
            Some(&Some(OsStr::new("1")))
        );
    }

    #[test]
    fn every_inherited_git_variable_is_stripped() {
        let vars = [
            "GIT_DIR",
            "git_ssl_no_verify",
            "GIT_TEMPLATE_DIR",
            "PATH",
            "HOME",
        ]
        .map(OsString::from);
        assert_eq!(
            inherited_git_vars(vars),
            ["GIT_DIR", "git_ssl_no_verify", "GIT_TEMPLATE_DIR"].map(OsString::from)
        );
    }

    #[tokio::test]
    async fn existing_checkouts_are_classified() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = some_commit();
        let dir = tmp.path().join("repo");

        assert_eq!(inspect(&dir, &sha).await.unwrap(), Existing::Absent);

        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert!(inspect(&dir, &sha).await.is_err());
        std::fs::remove_dir_all(&dir).unwrap();

        std::fs::create_dir(&dir).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(inspect(&dir, &sha).await.unwrap(), Existing::Partial);

        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let head = commit_in(&real);
        assert_eq!(inspect(&real, &head).await.unwrap(), Existing::Matching);
        assert_eq!(
            inspect(&real, &sha).await.unwrap(),
            Existing::WrongCommit(head)
        );
    }

    #[tokio::test]
    async fn foreign_directories_are_not_mistaken_for_partial_checkouts() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = some_commit();
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(inspect(&empty, &sha).await.unwrap(), Existing::Partial);

        let foreign = tmp.path().join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join("notes.txt"), "keep").unwrap();
        assert!(inspect(&foreign, &sha).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_checkouts_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(inspect(&link, &some_commit()).await.is_err());
    }
}
