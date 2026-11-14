//! Shared by any `#[cfg(test)]` module in this crate that needs to mutate
//! the process-wide working directory (`std::env::set_current_dir`). Such
//! tests must not run concurrently with each other or with any other test
//! that relies on cwd -- nextest runs each test in its own process, so this
//! only matters under a plain `cargo test`, but a single shared lock is what
//! makes that true across every module's tests in this crate, not just
//! within one module (#348).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// A `file:` URI of exactly `bytes` bytes, for probing [`crate::lsp::BoundedUri`].
pub fn uri_of_len(bytes: usize) -> lsp_types::Uri {
    let prefix = "file:///";
    lsp_types::Uri::from(format!("{prefix}{}", "u".repeat(bytes - prefix.len())))
}

static CWD_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that serializes CWD-mutating tests behind [`CWD_LOCK`] and
/// switches into `dir` for the guard's lifetime, restoring the original
/// working directory on drop — including on an early return or panic.
pub struct CwdGuard {
    _lock: MutexGuard<'static, ()>,
    original_dir: PathBuf,
}

impl CwdGuard {
    pub fn enter(dir: &Path) -> Self {
        let lock = CWD_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        Self {
            _lock: lock,
            original_dir,
        }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let restored = std::env::set_current_dir(&self.original_dir);
        // A failure here during an already-unwinding panic must not
        // panic again (double panic aborts the process, losing the
        // original failure's message). On the normal path, though,
        // silently swallowing this would leave the process cwd wrong
        // for every subsequent test with no diagnostic — panic loudly
        // instead, since that's exactly the failure mode this guard
        // exists to prevent.
        if !std::thread::panicking() {
            restored.expect("CwdGuard failed to restore original working directory");
        }
    }
}

/// Writes an executable file named `name` in `dir`, holding `body`, and
/// returns its path, creating `dir` first.
///
/// An extensionless `name` gets `EXE_SUFFIX` (`.exe` on Windows, where an
/// extensionless command spawns as `<command>.exe`; empty elsewhere). A `name`
/// that already carries an extension (`.cmd`, `.mjs`, `.exe`) is kept as is,
/// so it never becomes `x.exe.exe`. The file mode is `0o755` on Unix.
pub fn fake_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let suffix = if Path::new(name).extension().is_some() {
        ""
    } else {
        std::env::consts::EXE_SUFFIX
    };
    let path = dir.join(format!("{name}{suffix}"));
    fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

#[cfg(test)]
mod tests {
    use super::{CwdGuard, fake_executable};

    #[test]
    fn test_fake_executable_applies_the_platform_suffix_and_mode() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = fake_executable(&dir.path().join("bin"), "srv", "#!/bin/sh\n");

        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("srv{}", std::env::consts::EXE_SUFFIX)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "#!/bin/sh\n");
        for kept in ["tool.cmd", "cli.mjs", "srv.exe"] {
            let path = fake_executable(dir.path(), kept, "");
            assert_eq!(path.file_name().unwrap().to_str().unwrap(), kept);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o755);
        }
    }

    #[test]
    fn test_cwd_guard_restores_cwd_on_panic() {
        let original_dir = std::env::current_dir().unwrap();
        let tmp_dir = tempfile::TempDir::new().unwrap();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = CwdGuard::enter(tmp_dir.path());
            panic!("boom");
        }));

        assert!(result.is_err());
        assert_eq!(std::env::current_dir().unwrap(), original_dir);
    }
}
