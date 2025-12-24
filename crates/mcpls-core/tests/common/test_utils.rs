use std::path::PathBuf;

/// Checks if rust-analyzer is available in the system.
///
/// Returns true if rust-analyzer can be executed.
#[must_use]
pub fn rust_analyzer_available() -> bool {
    std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok()
}

/// Returns the path to the Rust workspace test fixture.
pub fn rust_workspace_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_workspace")
}

/// Returns the path to a configuration fixture.
pub fn config_fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/configs")
        .join(name)
}

/// Macro to skip tests if rust-analyzer is not available.
#[macro_export]
macro_rules! skip_if_no_rust_analyzer {
    () => {
        if !$crate::common::test_utils::rust_analyzer_available() {
            eprintln!("Skipping test: rust-analyzer not available");
            return;
        }
    };
}
