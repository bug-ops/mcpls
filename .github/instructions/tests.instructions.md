---
applyTo: "crates/mcpls-core/tests/**,crates/mcpls-core/src/**/tests.rs,crates/mcpls-core/src/**/*_tests.rs"
---

## Test design

Tests for async code must use `#[tokio::test]` and exercise the actual async paths, not
synchronous wrappers that bypass scheduler interactions. Concurrency bugs (lock
contention, channel saturation) are only reachable through genuine async execution.

Tests that require an external binary must be marked `#[ignore = "Requires <binary>
in PATH"]`. CI runners do not have language server binaries; an unmarked test will fail
on every clean run.

## Correctness invariants to cover

Any code path that pairs a filesystem operation with a subsequent network notification
(open, close, re-open after external edit) must have a test that exercises the full
sequence using a mock network client and a real temporary file — not just the filesystem
primitive in isolation.

Glob matching tests must use absolute path strings as input. Passing a bare filename
to a glob set will pass even when the production code fails on absolute paths, because
`globset` anchoring behaviour differs between the two cases.

Round-trip tests for serialisable types must cover every variant, and must assert the
serialised string value, not just that deserialisation succeeds. A `rename_all` typo
produces a wrong on-disk format that round-trips correctly in isolation.

## Idiomatic test code

Prefer `assert_eq!` over `assert!(a == b)` — `assert_eq!` prints both values on
failure, making diagnosis faster.

Use `tempfile::tempdir()` for tests that need real filesystem paths. Hard-coded `/tmp`
paths cause test interference when multiple test threads run in parallel.

Prefer `#[tokio::test]` over `tokio::runtime::Runtime::block_on` in test bodies —
`block_on` does not integrate with tokio's test utilities (`time::pause`,
`time::advance`) needed for testing debounce and timeout logic.
