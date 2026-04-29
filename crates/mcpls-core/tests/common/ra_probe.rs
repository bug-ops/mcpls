//! rust-analyzer binary detection for the e2e test suite.

use std::env;
use std::path::PathBuf;

/// Result of probing for the rust-analyzer binary.
pub enum Resolution {
    /// Binary found at this path.
    Found(PathBuf),
    /// Suite explicitly skipped via `MCPLS_SKIP_RA=1`.
    Skipped(&'static str),
    /// Binary not found and skip was not requested.
    Missing,
}

/// Resolve the rust-analyzer binary path.
///
/// Priority:
/// 1. `MCPLS_SKIP_RA=1` → `Skipped`
/// 2. `MCPLS_RUST_ANALYZER=<path>` → `Found(<path>)`
/// 3. `rust-analyzer` in PATH → `Found`
/// 4. Otherwise → `Missing`
pub fn resolve_rust_analyzer() -> Resolution {
    if env::var_os("MCPLS_SKIP_RA").is_some_and(|v| v == "1") {
        return Resolution::Skipped("MCPLS_SKIP_RA=1");
    }

    if let Some(p) = env::var_os("MCPLS_RUST_ANALYZER") {
        return Resolution::Found(PathBuf::from(p));
    }

    // Probe PATH: if rust-analyzer --version succeeds, resolve the full path.
    if std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
        && let Some(p) = find_in_path("rust-analyzer")
    {
        return Resolution::Found(p);
    }

    // Fallback: ask rustup where it installed the component.
    // This covers Windows CI where rustup toolchain bin dirs are not in PATH.
    if let Some(path) = std::process::Command::new("rustup")
        .args(["which", "rust-analyzer"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|p| !p.is_empty())
    {
        return Resolution::Found(PathBuf::from(path));
    }

    Resolution::Missing
}

/// Find a binary in PATH, returning its absolute path.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
