---
applyTo: "crates/mcpls-core/tests/**,crates/mcpls-core/src/**/tests.rs,crates/mcpls-core/src/**/*_tests.rs"
---

## Integration and e2e tests

Tests that require an external binary (language server or the project binary itself)
must be marked `#[ignore = "Requires <binary> in PATH"]`. Unmarked tests that shell out
to external processes break CI on clean runners where those binaries are absent.

Position values in assertions must use 1-based line and column numbers (MCP convention).
Using 0-based values produces off-by-one failures that are difficult to diagnose because
both the test and the production code look correct in isolation.

## Unit tests

New async code paths must have async unit tests using real temporary files and a mock
client where network I/O is involved. A test that only verifies the underlying primitive
(e.g. that a filesystem stat changes) does not cover the coordination logic that calls it.

Glob matching tests must use absolute path strings as inputs. A test that passes a bare
filename to a glob set will pass even when the production code fails on absolute paths,
because `globset` anchoring behaviour differs between the two cases.

New serialisable config types must have round-trip tests covering every variant in at
least one format. For enums with `#[serde(rename_all)]`, also verify that the
serialised string matches the documented on-disk value.

## Coverage gaps to watch

Any code path that involves both a filesystem operation and a subsequent network
notification (open, close, resync) should have a test that exercises the full sequence
with a mock network client, not just the filesystem part in isolation.
