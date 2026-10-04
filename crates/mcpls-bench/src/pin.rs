//! Executable resolution and version pinning.
//!
//! The version is read by running the resolved absolute path in the benchmarked
//! repository, because mcpls spawns the language server there and a toolchain
//! proxy (e.g. rustup) selects its target from the working directory.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::report::PinRecord;
use crate::scenario::Executable;

/// Finds `command` on `PATH` and returns its absolute path.
///
/// Symlinks are deliberately not resolved: toolchain proxies such as rustup
/// dispatch on the name they are invoked by.
///
/// Relative and empty `PATH` entries are skipped: they would resolve against
/// the benchmarked repository, which is the working directory of every spawn.
///
/// # Errors
///
/// Returns an error when no matching file exists in an absolute `PATH` entry.
///
/// # Examples
///
/// ```
/// use mcpls_bench::pin::resolve_in_path;
///
/// assert!(resolve_in_path("definitely-not-a-real-command-xyz").is_err());
/// ```
pub fn resolve_in_path(command: &str) -> Result<PathBuf> {
    let direct = Path::new(command);
    if direct.components().count() > 1 {
        return std::path::absolute(direct)
            .with_context(|| format!("failed to resolve `{command}`"));
    }
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(format!("{command}{}", std::env::consts::EXE_SUFFIX)))
        .find(|candidate| candidate.is_file())
        .with_context(|| format!("`{command}` was not found in an absolute PATH entry"))
}

/// Resolves `executable`, runs its version command in `cwd`, and records the result.
///
/// # Errors
///
/// Returns an error when the executable cannot be resolved or run, or exits unsuccessfully.
pub async fn pin(executable: &Executable, cwd: &Path) -> Result<PinRecord> {
    let path = resolve_in_path(&executable.command)?;
    let output = Command::new(&path)
        .args(&executable.version_args)
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("failed to run {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "`{} {}` exited with {}",
            path.display(),
            executable.version_args.join(" "),
            output.status
        );
    }
    let version_output = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(PinRecord {
        requested: executable.command.clone(),
        path,
        version_output,
        expected_version: executable.expected_version.clone(),
    })
}

/// Fails if any record mismatched its expected version, unless `allow` is set.
///
/// # Errors
///
/// Returns an error naming the first mismatching executable.
///
/// # Examples
///
/// ```
/// use mcpls_bench::pin::ensure_matches;
/// use mcpls_bench::report::PinRecord;
///
/// let record = PinRecord {
///     requested: "tool".to_owned(),
///     path: "/bin/tool".into(),
///     version_output: "tool 1".to_owned(),
///     expected_version: Some("tool 2".to_owned()),
/// };
/// assert!(ensure_matches([&record], false).is_err());
/// assert!(ensure_matches([&record], true).is_ok());
/// ```
pub fn ensure_matches<'a>(
    records: impl IntoIterator<Item = &'a PinRecord>,
    allow: bool,
) -> Result<()> {
    if allow {
        return Ok(());
    }
    match records.into_iter().find(|r| r.mismatch()) {
        Some(r) => bail!(
            "`{}` reports `{}`, expected it to contain `{}` (pass --allow-version-mismatch to override)",
            r.requested,
            r.version_output,
            r.expected_version.as_deref().unwrap_or_default()
        ),
        None => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn record(mismatch: bool) -> PinRecord {
        PinRecord {
            requested: "tool".to_owned(),
            path: "/bin/tool".into(),
            version_output: "tool 1".to_owned(),
            expected_version: Some(if mismatch { "tool 2" } else { "tool 1" }.to_owned()),
        }
    }

    #[test]
    fn mismatch_aborts_unless_allowed() {
        let records = [record(false), record(true)];
        assert!(ensure_matches(&records, false).is_err());
        assert!(ensure_matches(&records, true).is_ok());
        assert!(ensure_matches(&records[..1], false).is_ok());
    }

    #[test]
    fn missing_command_is_reported() {
        assert!(resolve_in_path("definitely-not-a-real-command-xyz").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pin_records_version_and_detects_mismatch() {
        let exe = Executable {
            command: "sh".to_owned(),
            version_args: vec!["-c".to_owned(), "echo shver 9".to_owned()],
            expected_version: Some("shver 9".to_owned()),
        };
        let cwd = std::env::current_dir().unwrap();
        let ok = pin(&exe, &cwd).await.unwrap();
        assert_eq!(ok.version_output, "shver 9");
        assert!(!ok.mismatch());

        let wrong = Executable {
            expected_version: Some("shver 1".to_owned()),
            ..exe
        };
        assert!(pin(&wrong, &cwd).await.unwrap().mismatch());
    }
}
