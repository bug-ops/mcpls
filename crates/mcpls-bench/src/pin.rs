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
    let pathext = cfg!(windows)
        .then(|| std::env::var("PATHEXT").unwrap_or_else(|_| DEFAULT_PATHEXT.to_owned()));
    let names = candidate_names(command, pathext.as_deref());
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
        .with_context(|| format!("`{command}` was not found in an absolute PATH entry"))
}

const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// File names under which `command` may be installed, in lookup order.
///
/// With `pathext` (Windows), the command as written comes first when it already
/// carries an extension, then the command with each `PATHEXT` extension appended,
/// so `pnpm` finds `pnpm.cmd`. Without it, only the command itself.
fn candidate_names(command: &str, pathext: Option<&str>) -> Vec<String> {
    let Some(pathext) = pathext else {
        return vec![command.to_owned()];
    };
    let mut names = Vec::new();
    if Path::new(command).extension().is_some() {
        names.push(command.to_owned());
    }
    names.extend(
        pathext
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(|ext| format!("{command}{ext}")),
    );
    names
}

/// Resolves `executable`, runs its version command in `cwd`, and records the result.
///
/// # Errors
///
/// Returns an error when the executable cannot be resolved or run, or exits unsuccessfully.
pub async fn pin(executable: &Executable, cwd: &Path) -> Result<PinRecord> {
    let path = resolve_in_path(&executable.command)?;
    let version_path = match &executable.version_command {
        Some(command) => resolve_in_path(command)?,
        None => path.clone(),
    };
    let output = Command::new(&version_path)
        .args(&executable.version_args)
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("failed to run {}", version_path.display()))?;
    if !output.status.success() {
        bail!(
            "`{} {}` exited with {}",
            version_path.display(),
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
    fn pathext_extensions_are_tried_in_order() {
        assert_eq!(candidate_names("git", None), ["git"]);
        assert_eq!(
            candidate_names("pnpm", Some(".EXE;;.CMD")),
            ["pnpm.EXE", "pnpm.CMD"]
        );
        assert_eq!(
            candidate_names("tool.bat", Some(".EXE")),
            ["tool.bat", "tool.bat.EXE"]
        );
    }

    #[test]
    fn missing_command_is_reported() {
        assert!(resolve_in_path("definitely-not-a-real-command-xyz").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_separate_version_command_supplies_the_version() {
        let exe = Executable {
            command: "sh".to_owned(),
            version_args: vec!["-c".to_owned(), "echo viaecho 3".to_owned()],
            version_command: Some("sh".to_owned()),
            expected_version: None,
        };
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(pin(&exe, &cwd).await.unwrap().version_output, "viaecho 3");
        let missing = Executable {
            version_command: Some("no-such-version-command-xyz".to_owned()),
            ..exe
        };
        assert!(pin(&missing, &cwd).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pin_records_version_and_detects_mismatch() {
        let exe = Executable {
            command: "sh".to_owned(),
            version_args: vec!["-c".to_owned(), "echo shver 9".to_owned()],
            version_command: None,
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
