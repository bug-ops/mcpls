//! Shared by any `#[cfg(test)]` module in this crate that needs to mutate
//! the process-wide working directory (`std::env::set_current_dir`). Such
//! tests must not run concurrently with each other or with any other test
//! that relies on cwd -- nextest runs each test in its own process, so this
//! only matters under a plain `cargo test`, but a single shared lock is what
//! makes that true across every module's tests in this crate, not just
//! within one module (#348).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

static CWD_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that serializes CWD-mutating tests behind [`CWD_LOCK`] and
/// switches into `dir` for the guard's lifetime, restoring the original
/// working directory on drop — including on an early return or panic.
pub struct CwdGuard {
    _lock: MutexGuard<'static, ()>,
    original_dir: PathBuf,
}

impl CwdGuard {
    #[allow(clippy::unwrap_used)]
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
            #[allow(clippy::expect_used)]
            restored.expect("CwdGuard failed to restore original working directory");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::CwdGuard;

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
