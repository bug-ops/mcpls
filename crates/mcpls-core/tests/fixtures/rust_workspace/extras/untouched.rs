/// This file is intentionally never opened by any MCP tool call in the e2e suite,
/// and is NOT part of the crate's module tree — it lives in `extras/` and is copied
/// into the staged workspace's src/ directory to exist on disk (subscribe() requires
/// the path to exist) without rust-analyzer ever running diagnostics on it.
pub fn never_diagnosed() -> i32 {
    42
}
