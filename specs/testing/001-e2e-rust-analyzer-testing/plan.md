---
aliases:
  - E2E rust-analyzer test suite plan
tags:
  - sdd
  - plan
  - testing
  - lsp
created: 2026-08-05
status: implemented
related:
  - "[[spec]]"
  - "[[constitution]]"
---

# Technical Plan: End-to-End MCP Tool Coverage Against a Real rust-analyzer

> [!info] References
> **Spec**: [[spec]]
> **Original design doc**: `_source.md` (this directory) — "Design: E2E Tests with Real
> rust-analyzer (Rev 2)", 2026-04-29, superseding a Rev 1 that a critic pass had rejected

> [!important] What shipped vs. what was designed
> This plan restates the Rev 2 design's architecture sections, each annotated with how the
> shipped implementation (PRs #125, #126, #139, #225 — see spec.md's Resolution) matches or
> deviates from it. The single largest deviation: **Section 2 (Skip Policy)** specified a
> fail-closed *default* CI policy (every default job either installs rust-analyzer or explicitly
> sets `MCPLS_SKIP_RA=1`); the shipped suite instead gates the whole test behind `#[ignore]`, so it
> never runs under a default `cargo nextest run`/`cargo test` invocation at all — only under a
> dedicated e2e job that passes `--ignored` (PR #126). The `MCPLS_SKIP_RA`/`MCPLS_RUST_ANALYZER`
> mechanism itself shipped exactly as designed; it now only matters for that dedicated job's
> internal MSRV/minimal variant, not for every default job as originally intended.

## 1. Architecture

### Process Model (resolves C1 from the original design)

**Problem**: cargo-nextest's default execution model is process-per-test — each `#[test]` runs in
its own `<binary> --exact <name>` subprocess. A per-binary harness re-spawning rust-analyzer for
every test would make the suite's wall-clock cost scale with the number of tools covered.

**Decision, as shipped**: one `#[test] fn ra_e2e_suite()` in `tests/ra_e2e.rs`, `#[ignore]`-gated.
It:
1. Probes + spawns rust-analyzer once (`common::ra_probe::resolve_rust_analyzer`).
2. Waits for indexing readiness.
3. Sequentially invokes a `sub_cases: &[SubCase]` registry, each producing a `SubResult { name,
   outcome }`.
4. After all sub-cases run, panics with an aggregated report iff any failed.

This preserves cold-start amortization under both `cargo test -- --ignored` and `cargo nextest
run -- --ignored`: nextest sees one test, runs one process, the harness lives for the whole suite.

**Trade-offs accepted** (unchanged from the original design):
- Loss of per-tool failure granularity in the nextest summary — mitigated by the aggregated
  failure report naming every failed sub-case, sub-cases continuing past a first failure, and
  `MCPLS_RA_FILTER`-style local filtering if a developer wants to isolate one case (verify exact
  filtering mechanism against current `ra_e2e.rs` before relying on it in a follow-up).
- Loss of nextest's per-test isolation — acceptable since the only shared mutable state is
  rust-analyzer itself, and write sub-cases operate on a per-run `TempDir` copy.
- No nextest retry policy on individual sub-cases — acceptable for a single-binary suite.

**Rejected alternatives** (unchanged from the original design): a `cargo test` carve-out for the
whole e2e binary (creates two divergent CI invocations); an out-of-process rust-analyzer daemon
keyed by a lockfile (unnecessary complexity until a second e2e binary exists); nextest test-groups
with `max-threads = 1` (solves serialization, not amortization — still process-per-test).

### Skip Policy (resolves S2 — deviation noted above)

```rust
// tests/common/ra_probe.rs (as shipped)
pub enum Resolution { Found(PathBuf), Skipped(&'static str), Missing }

pub fn resolve_rust_analyzer() -> Resolution {
    if env::var_os("MCPLS_SKIP_RA").is_some_and(|v| v == "1") {
        return Resolution::Skipped("MCPLS_SKIP_RA=1");
    }
    if let Some(p) = env::var_os("MCPLS_RUST_ANALYZER") {
        return Resolution::Found(PathBuf::from(p));
    }
    match which::which("rust-analyzer") {
        Ok(p) => Resolution::Found(p),
        Err(_) => Resolution::Missing,
    }
}
```

Driver behavior matches the original design (`Found` → run, `Skipped` → print + succeed,
`Missing` → panic naming `MCPLS_SKIP_RA=1`). What changed is *when this code path is reached at
all*: PR #126 added `#[ignore = "Requires rust-analyzer in PATH; set MCPLS_SKIP_RA=1 to skip or
MCPLS_RUST_ANALYZER=<path> to override"]` directly on `ra_e2e_suite`, so a default invocation
never calls `resolve_rust_analyzer` in the first place. The dedicated e2e CI job installs
rust-analyzer and passes `--ignored`, making `Missing` effectively unreachable there too in
practice; `MCPLS_SKIP_RA=1` remains available for any CI variant that explicitly wants to run the
`--ignored` tests without rust-analyzer present (e.g. a hypothetical MSRV/minimal `--ignored` run).

### Readiness Gate (resolves S1)

Shipped as designed: subscribes to LSP `$/progress` (`rustAnalyzer/Indexing`), with
`rust-analyzer/serverStatus` (`quiescent: true`) as a secondary signal, read through the existing
`get_server_messages` MCP tool (no new bridge code required — `bridge/notifications.rs` already
caches these). Hover-poll was rejected in the original design (cold rust-analyzer returns `null`,
indistinguishable from a genuine miss) and was not revisited.

### Fixture Isolation (resolves S4)

Shipped as designed: each run copies the committed `tests/fixtures/rust_workspace/` into a fresh
`TempDir` via `stage_workspace()`, so concurrent rust-analyzer instances never race on shared
`target/` metadata, and write sub-cases (rename, format) never touch the committed fixture.

### Tool Coverage (resolves S5 — grown since the original design)

The original design enumerated all 16 MCP tools that existed as of 2026-04-29. Coverage has since
grown alongside the tool surface: PR #139 added sub-cases for the four LSP 3.17-era tools
(`get_signature_help`, `go_to_implementation`, `go_to_type_definition`, `get_inlay_hints` —
[[lsp/002-lsp317-missing-tools/spec|spec lsp/002]]) and four MCP-resources sub-cases
(`sc_list_resources`, `sc_read_resource`, `sc_subscribe_unsubscribe_resource`,
`sc_subscribe_no_replay_without_cached_diagnostics` — [[mcp/002-mcp-resources-diagnostics/spec|spec
002]]), bringing the registry to 24 sub-cases covering all 20 current MCP tools plus the resource
protocol. See `tests/ra_e2e.rs`'s `sub_cases` registry for the authoritative, current list — do
not rely on the original design doc's 16-row table as current.

## 2. Project Structure

```
crates/mcpls-core/tests/
├── common/
│   ├── ra_probe.rs        # Resolution: Found/Skipped/Missing
│   └── assertions.rs      # shared assertion helpers
├── ra_e2e.rs              # single-test driver: harness + sub-case registry (1508 lines)
└── fixtures/
    └── rust_workspace/    # staged into a per-run TempDir by stage_workspace()
```

## 3. Testing Strategy

| Level | Framework | What to Test | Coverage Target |
|-------|-----------|---------------|-------------------|
| E2E (this suite) | `cargo nextest`/`cargo test`, `--ignored` | Every MCP tool's real round trip through mcpls to rust-analyzer | 24/24 sub-cases pass with rust-analyzer installed |
| Unit/integration (unchanged, out of scope here) | `MockLspServer` | Protocol-level translation logic in isolation | Existing coverage, not modified by this suite |

## 4. CI Gating (as shipped, deviating from the original design's Section 8)

- **Dedicated e2e job**: installs rust-analyzer, invokes with `--ignored`; this is the only job
  that exercises `ra_e2e_suite` at all.
- **All other jobs** (including MSRV/minimal): do not pass `--ignored`, so `ra_e2e_suite` never
  runs — `MCPLS_SKIP_RA` is not needed for these to pass, unlike the original design's assumption
  that MSRV/minimal would need to explicitly opt out of a fail-closed default.
- **Renovation**: the original design's proposal to pin and periodically bump a specific
  rust-analyzer release via Renovate/Dependabot was not confirmed as implemented — verify current
  CI workflow configuration before relying on a specific pinned version being tracked.

## 5. Rollout Plan (historical — already executed)

1. PR #125: initial suite, 16 tools, single driver, fixture staging, skip policy.
2. PR #126: `#[ignore]` gating + e2e CI job installs rust-analyzer (skip-policy deviation).
3. PR #139: extended to LSP 3.17 tools + MCP resources (24 sub-cases).
4. PR #225: fixed hover off-by-one and diagnostics-replay-on-subscribe bugs the suite itself
   surfaced — the suite's first documented regression catch.

## 6. Risks and Mitigations

| Risk | Impact | Probability | Mitigation |
|------|--------|--------------|-------------|
| A new MCP tool is added without a corresponding `sc_*` sub-case | medium | medium | No automated enforcement exists; relies on spec.md's Agent Boundaries "Always" rule and PR review |
| `#[ignore]`-gating means the suite doesn't run on every PR, only the dedicated e2e job | medium | low (job runs on every PR per current CI, verify before relying on this) | Tracked as an Open Question in spec.md; revisit if the dedicated job's trigger conditions ever narrow |
| Pinned rust-analyzer version in the dedicated e2e job drifts stale | low | medium | Original design proposed a Renovate/Dependabot bump cadence; verify this is actually configured, not just proposed |

## See Also

- [[spec]] — feature specification
- [[MOC-specs]] — all specifications
- `_source.md` — original Rev 2 design doc, preserved verbatim
