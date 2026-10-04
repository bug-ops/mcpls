//! Exclusive per-scenario lock on the work directory.
//!
//! `prepare` stages a clone in a shared `.partial` directory and `run` writes a
//! shared config and log tree, so two invocations for one scenario would corrupt
//! each other. The kernel drops the lock when the process exits, however it dies.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::scenario::ScenarioName;

const WINDOWS_ATTEMPTS: u32 = 20;
const RETRY_DELAY: Duration = Duration::from_millis(100);

/// Outcome of one attempt to take the lock file.
#[derive(Debug)]
enum Attempt {
    Acquired(File),
    Held,
}

/// Holds the exclusive lock on one scenario of a work directory until dropped.
#[derive(Debug)]
pub struct WorkDirLock {
    _file: File,
    path: PathBuf,
}

impl WorkDirLock {
    /// Takes the exclusive lock for `scenario` below `work_dir`, creating the lock file if needed.
    ///
    /// On Windows a sharing violation is retried for about two seconds, because
    /// an antivirus scan of a freshly created file looks the same as a second holder.
    ///
    /// # Errors
    ///
    /// Returns an error when another `mcpls-bench` invocation holds the lock, or the
    /// lock file cannot be created.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn demo() -> anyhow::Result<()> {
    /// use std::path::Path;
    /// use mcpls_bench::lock::WorkDirLock;
    /// use mcpls_bench::scenario::ScenarioName;
    ///
    /// let name = ScenarioName::try_from("fd-rust-analyzer".to_owned()).unwrap();
    /// let _lock = WorkDirLock::acquire(Path::new("/tmp/work"), &name).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn acquire(work_dir: &Path, scenario: &ScenarioName) -> Result<Self> {
        let dir = work_dir.join("locks");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        let path = dir.join(format!("{}.lock", scenario.as_str()));
        let attempts = if cfg!(windows) { WINDOWS_ATTEMPTS } else { 1 };
        match acquire_with(|| try_lock(&path), attempts, RETRY_DELAY).await? {
            Some(file) => Ok(Self { _file: file, path }),
            None => bail!(
                "scenario `{}` is in use by another mcpls-bench process ({} is locked)",
                scenario.as_str(),
                path.display()
            ),
        }
    }

    /// The lock file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

async fn acquire_with(
    mut attempt: impl FnMut() -> Result<Attempt>,
    attempts: u32,
    delay: Duration,
) -> Result<Option<File>> {
    for remaining in (0..attempts.max(1)).rev() {
        if let Attempt::Acquired(file) = attempt()? {
            return Ok(Some(file));
        }
        if remaining > 0 {
            tokio::time::sleep(delay).await;
        }
    }
    Ok(None)
}

#[cfg(unix)]
fn try_lock(path: &Path) -> Result<Attempt> {
    use rustix::fs::{FlockOperation, flock};
    use rustix::io::Errno;

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Attempt::Acquired(file)),
        Err(Errno::WOULDBLOCK) => Ok(Attempt::Held),
        Err(error) => Err(anyhow::Error::new(std::io::Error::from(error))
            .context(format!("failed to lock {}", path.display()))),
    }
}

#[cfg(windows)]
fn try_lock(path: &Path) -> Result<Attempt> {
    use std::os::windows::fs::OpenOptionsExt;

    const ERROR_SHARING_VIOLATION: i32 = 32;
    match OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .share_mode(0)
        .open(path)
    {
        Ok(file) => Ok(Attempt::Acquired(file)),
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(Attempt::Held),
        Err(error) => {
            Err(anyhow::Error::new(error).context(format!("failed to lock {}", path.display())))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn name(text: &str) -> ScenarioName {
        ScenarioName::try_from(text.to_owned()).unwrap()
    }

    #[tokio::test]
    async fn second_holder_is_refused_until_the_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let first = WorkDirLock::acquire(dir.path(), &name("fd")).await.unwrap();
        let error = WorkDirLock::acquire(dir.path(), &name("fd"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("in use"), "{error}");
        drop(first);
        WorkDirLock::acquire(dir.path(), &name("fd")).await.unwrap();
    }

    #[tokio::test]
    async fn scenarios_lock_independently() {
        let dir = tempfile::tempdir().unwrap();
        let _a = WorkDirLock::acquire(dir.path(), &name("a")).await.unwrap();
        let b = WorkDirLock::acquire(dir.path(), &name("b")).await.unwrap();
        assert!(b.path().ends_with(Path::new("locks").join("b.lock")));
    }

    #[tokio::test]
    async fn contention_is_retried_up_to_the_attempt_budget() {
        let calls = Cell::new(0_u32);
        let mut spare = Some(tempfile::tempfile().unwrap());
        let acquired = acquire_with(
            || {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    Ok(Attempt::Held)
                } else {
                    Ok(Attempt::Acquired(spare.take().unwrap()))
                }
            },
            5,
            Duration::from_millis(1),
        )
        .await
        .unwrap();
        assert!(acquired.is_some());
        assert_eq!(calls.get(), 3);

        calls.set(0);
        let none = acquire_with(
            || {
                calls.set(calls.get() + 1);
                Ok(Attempt::Held)
            },
            4,
            Duration::from_millis(1),
        )
        .await
        .unwrap();
        assert!(none.is_none());
        assert_eq!(calls.get(), 4);
    }
}
