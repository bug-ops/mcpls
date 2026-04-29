/// This file intentionally contains a type error for diagnostics testing.
///
/// It is NOT part of the crate's module tree — it lives in `extras/`
/// and is copied into the staged workspace's src/ directory by the e2e harness.
pub fn type_error() -> String {
    42
}
