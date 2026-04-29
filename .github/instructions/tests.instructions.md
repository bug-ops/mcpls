---
applyTo: "crates/mcpls-core/tests/**,crates/mcpls-core/src/**/tests.rs,crates/mcpls-core/src/**/*_tests.rs"
---

## Test review checklist

### Integration tests (`tests/integration/`)

- Tests requiring a live LSP binary (`rust-analyzer`, `tsgo`, etc.) must be marked
  `#[ignore = "Requires <binary> in PATH"]`. Unmarked tests that shell out to LSP
  binaries will break CI on clean runners.
- `rust_analyzer_tests.rs` probes must verify the post-PR #103 diagnostic pipeline:
  `get_diagnostics` should return errors for files with known type errors, and
  `get_cached_diagnostics` should return the same set (non-empty) after a
  `publishDiagnostics` notification has been received.

### E2e tests (`tests/e2e/`)

- Every e2e test spawns the mcpls binary. If the binary is not pre-built, the test
  must be `#[ignore]`. Document the required build step in the ignore message.
- Position values in e2e assertions must use 1-based line/column (MCP convention).
  Using 0-based values will produce off-by-one failures that are hard to diagnose.

### Unit tests

- `ensure_open` resync path (signature mismatch → `didClose` + `didOpen`) must be
  covered by an async test using a temp file and a mock `LspClient`. The stat-only
  test (`test_stat_signature_changes_when_file_grows`) does not cover this path.
- `FileWatcher::register` and `compute_changes` require unit tests. New glob
  registrations must be verified to actually match the intended file paths using
  absolute path strings — bare patterns like `*.rs` will not match.
- `DiagnosticsMode` serde tests must cover all three variants in both JSON and TOML
  deserialization, including the omitted-key default.
