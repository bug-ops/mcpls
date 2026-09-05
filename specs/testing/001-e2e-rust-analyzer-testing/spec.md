---
aliases:
  - E2E rust-analyzer test suite
  - ra_e2e
tags:
  - sdd
  - spec
  - testing
  - lsp
created: 2026-08-05
status: implemented
related:
  - "[[constitution]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec]]"
  - "[[lsp/002-lsp317-missing-tools/spec]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec]]"
---

# Feature: End-to-End MCP Tool Coverage Against a Real rust-analyzer

> [!info] Metadata
> **Author**: architect (design doc `e2e-rust-analyzer.md`, Rev 2, 2026-04-29); folded into the
> numbered spec package during the `.local/specs/` → `specs/` migration
> **Branch**: originally implemented across several PRs (see Resolution)
> **Source finding**: `MockLspServer`-based unit/integration tests exercise the MCP↔LSP
> translation layer in isolation, but nothing in the test suite drives the full
> MCP→mcpls→LSP→rust-analyzer path against a real language server, so a class of bugs
> (encoding negotiation, capability mismatches, real diagnostic timing/debounce) was only
> ever caught live, not in CI

> [!success] Resolution
> Implemented incrementally, starting from this doc's own Rev 2 design:
> - PR #125 (commit `6cae0e1`) — initial `tests/ra_e2e.rs` single-driver suite covering all
>   16 MCP tools that existed at the time, plus `tests/common/ra_probe.rs` (skip/found/missing
>   resolution) and the staged-`TempDir` fixture workflow, matching this doc's Sections 1-6.
> - PR #126 (commit `0f40608`) — added `#[ignore = "Requires rust-analyzer in PATH; ..."]` to
>   `ra_e2e_suite` and installed rust-analyzer in the dedicated e2e CI job. This is a **deviation**
>   from Section 2's designed default policy ("fail closed if rust-analyzer is missing" for
>   default jobs): the shipped suite is opt-in via `cargo nextest run -- --ignored` (or
>   `cargo test -- --ignored`) in every job, not merely skip-able via `MCPLS_SKIP_RA=1` in the
>   MSRV/minimal job as originally designed. `MCPLS_SKIP_RA=1` and `MCPLS_RUST_ANALYZER=<path>`
>   (Section 2's `resolve_rust_analyzer`) are both still implemented as documented, in
>   `tests/common/ra_probe.rs`.
> - PR #139 (commit `027da84`) — extended coverage from 16 to the LSP 3.17-era tools added since
>   (`get_signature_help`, `go_to_implementation`, `go_to_type_definition`, `get_inlay_hints` —
>   closing [[lsp/002-lsp317-missing-tools/spec|spec lsp/002]]/#116) and the MCP resources surface
>   (`list_resources`, `read_resource`, `subscribe`/`unsubscribe` — closing
>   [[mcp/002-mcp-resources-diagnostics/spec|spec mcp/002]]).
> - PR #225 — fixed off-by-one hover positions and a diagnostics-replay-on-subscribe bug the
>   suite itself surfaced, demonstrating the suite catching a real regression before release.
>
> As of this writing `tests/ra_e2e.rs` runs 24 sub-cases (`sc_*` functions in a `sub_cases`
> registry) covering all 20 current MCP tools plus the four resource-protocol sub-cases, still
> as the single `#[test] fn ra_e2e_suite()` driver this doc's Section 1 (C1) specified — cold-start
> amortization holds under both `cargo test` and `cargo nextest run`, one rust-analyzer spawn per
> full suite run (plus the documented second spawn for the empty-workspace edge case).
>
> The full original Rev 2 design (`_source.md` in this directory) is preserved verbatim as the
> most detailed available account of the pre-implementation design rationale; this `spec.md`
> restates it in the project's standard template, and [[plan|plan.md]] carries the architecture
> sections forward with resolution notes against what actually shipped.

## 1. Overview

### Problem Statement

Before this suite existed, mcpls's test coverage for the MCP→LSP bridge was entirely
`MockLspServer`-based (protocol-level unit/integration tests under `tests/e2e/protocol_tests.rs`
and `tests/integration/`). A mock server can only ever respond the way its own stub logic says
it should — it cannot catch bugs in the *real* negotiation between mcpls and an actual language
server: capability mismatches, encoding negotiation (`bridge/encoding.rs`'s non-UTF-16 paths),
real diagnostic push/pull timing (rust-analyzer's flycheck debounce), or any assumption baked
into mcpls's LSP client that happens to hold against the mock but not against genuine
rust-analyzer behavior.

`tests/integration/rust_analyzer_tests.rs` already exercises some real-rust-analyzer paths, but
narrowly — not as a systematic, registry-driven walk of every MCP tool mcpls exposes.

### Goal

Every MCP tool mcpls exposes has at least one automated test exercising the full
MCP→mcpls→LSP→rust-analyzer round trip against a real rust-analyzer binary, run as part of the
project's normal CI (opt-in via `--ignored`, per the Resolution's noted deviation from the
original fail-closed design), with rust-analyzer's cold-start/indexing cost paid at most twice
per suite run regardless of how many tools are covered.

### Out of Scope

- Replacing `MockLspServer`-based protocol-level unit tests — those remain the fast, deterministic
  first line of coverage; this suite is a complementary, slower, real-server layer.
- Performance benchmarking of rust-analyzer itself.
- Multi-language e2e coverage (pyright, typescript-language-server, gopls, clangd, zls) — this
  suite is Rust/rust-analyzer only, per the original design's Non-Goals.
- New MCP tools — this spec covers testing the tools that exist, not adding new ones.

## 2. User Stories

### US-001: A capability-negotiation regression is caught in CI, not live

AS A mcpls maintainer changing `bridge/encoding.rs` or the `initialize` handshake
I WANT an automated suite that drives every MCP tool against a real rust-analyzer
SO THAT a regression in position-encoding negotiation or capability handling fails CI instead of
surfacing only when a live user hits it

**Acceptance criteria:**
```
GIVEN rust-analyzer is installed and MCPLS_SKIP_RA is unset
WHEN `cargo nextest run --workspace --all-features -- --ignored` (or the dedicated e2e CI job) runs
THEN every currently-implemented MCP tool is exercised against real rust-analyzer and the suite
     fails if any sub-case's assertion does not hold
```

### US-002: The suite stays fast despite testing every tool

AS A CI maintainer
I WANT rust-analyzer's cold-start/indexing cost paid once per suite run, not once per tool
SO THAT adding tool coverage does not make the e2e job's wall-clock time scale linearly with the
number of MCP tools

**Acceptance criteria:**
```
GIVEN the e2e suite covers N tools (N = 20 as of this writing)
WHEN the suite runs under cargo nextest (process-per-test by default)
THEN rust-analyzer is spawned at most twice for the whole run (main suite + the dedicated
     empty-workspace edge case), not once per tool
```

### US-003: A missing rust-analyzer binary fails loudly in the dedicated e2e job, cleanly elsewhere

AS A contributor running the default test suite locally without rust-analyzer installed
I WANT the e2e suite to not run at all by default (opt-in via `--ignored`)
SO THAT `cargo nextest run --workspace --all-features` succeeds without requiring rust-analyzer,
while the dedicated e2e CI job (which does pass `--ignored` and does install rust-analyzer) still
fails loudly if the binary is missing or `MCPLS_SKIP_RA`/`MCPLS_RUST_ANALYZER` are misconfigured

**Acceptance criteria:**
```
GIVEN rust-analyzer is not installed and the caller does not pass `--ignored`
WHEN `cargo nextest run --workspace --all-features` (the default, non-e2e invocation) runs
THEN ra_e2e_suite does not execute and the run succeeds

GIVEN the dedicated e2e CI job passes `--ignored` and rust-analyzer is missing, with
     MCPLS_SKIP_RA unset
WHEN that job runs
THEN ra_e2e_suite panics with a message naming MCPLS_SKIP_RA=1 as the opt-out
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHALL provide one `#[test]` driver (`ra_e2e_suite`) that spawns rust-analyzer at most once for the main suite plus once for the documented empty-workspace edge case, regardless of the number of MCP tools covered | must |
| FR-002 | THE SYSTEM SHALL cover every currently-implemented MCP tool with at least one sub-case exercising the real MCP→mcpls→LSP→rust-analyzer path | must |
| FR-003 | WHEN `MCPLS_SKIP_RA=1` is set THE SYSTEM SHALL print a skip line and return success without attempting to spawn rust-analyzer | must |
| FR-004 | WHEN `MCPLS_RUST_ANALYZER=<path>` is set THE SYSTEM SHALL use that binary instead of resolving `rust-analyzer` from `PATH` | must |
| FR-005 | WHEN rust-analyzer cannot be resolved (not skipped, no override, not found in `PATH`) THE SYSTEM SHALL panic with a message naming `MCPLS_SKIP_RA=1` as the opt-out, rather than silently passing | must |
| FR-006 | THE SYSTEM SHALL NOT run `ra_e2e_suite` under a default (non-`--ignored`) `cargo nextest run`/`cargo test` invocation, since the test is marked `#[ignore]` | must |
| FR-007 | WHEN one sub-case fails THE SYSTEM SHALL continue running the remaining sub-cases and report every failure in one aggregated panic message, rather than aborting at the first failure | must |
| FR-008 | THE SYSTEM SHALL stage the committed `tests/fixtures/rust_workspace/` fixture into a fresh `TempDir` per suite run, so writable sub-cases (rename, format) never mutate the committed fixture | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Wall-clock time for the full suite (cold rust-analyzer start + indexing + all sub-cases) must stay practical for a dedicated CI job; the original design targeted under 180s |
| NFR-002 | Isolation | Each suite run operates on its own `TempDir` copy of the fixture workspace, so concurrent CI runs (or a future second e2e binary) do not race on shared `target/`/index state |
| NFR-003 | Determinism | Readiness (rust-analyzer finished indexing) must be detected via an authoritative signal (`$/progress`/`rustAnalyzer/Indexing` end, or `serverStatus` quiescent), not a fixed sleep or a hover-poll heuristic that cannot distinguish "still loading" from "genuine miss" |
| NFR-004 | Maintainability | Adding a new MCP tool should require adding one `sc_*` sub-case function and one registry entry, not restructuring the driver |

## 5. Data Model

Not a data-model feature — the relevant "entities" are test-harness constructs:

| Entity | Description | Key Attributes |
|--------|-------------|-----------------|
| `Resolution` (`tests/common/ra_probe.rs`) | Outcome of resolving which rust-analyzer binary (if any) to use | `Found(PathBuf)`, `Skipped(&'static str)`, `Missing` |
| `SubCase` (`tests/ra_e2e.rs`) | One registry entry: a named sub-case function | `name: &'static str`, `run: fn(&mut McpClient, &Path) -> Result<(), String>` |
| `SubResult` (`tests/ra_e2e.rs`) | Outcome of running one `SubCase` | `name`, `outcome` (pass/fail with message) |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|--------------------|
| rust-analyzer missing, `MCPLS_SKIP_RA` unset, suite invoked via `--ignored` | Panics naming `MCPLS_SKIP_RA=1` as the opt-out (FR-005) |
| rust-analyzer missing, `MCPLS_SKIP_RA=1` | Prints a skip line, suite returns success (FR-003) |
| One sub-case fails (e.g. a hover assertion) | Suite continues through the remaining sub-cases; aggregated failure report names every failed sub-case (FR-007) |
| Empty workspace (no source files beyond `Cargo.toml` + empty `src/lib.rs`) | Separate sub-case spawns a second rust-analyzer instance against a minimal `TempDir`, verifying init succeeds and `workspace_symbol_search` returns empty — the one sanctioned second spawn per suite run |
| A future MCP tool is added without a corresponding `sc_*` sub-case | Not automatically caught by the test framework — relies on the Agent Boundaries "Always" rule below being followed at implementation time |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | `cargo nextest run --workspace --all-features -- --ignored` (or the dedicated e2e CI job) with rust-analyzer installed | Suite runs, exercises all 20 current MCP tools + 4 resource sub-cases, exits 0 |
| SC-002 | `cargo nextest run --workspace --all-features` (default, no `--ignored`) | `ra_e2e_suite` does not run; suite completes without requiring rust-analyzer |
| SC-003 | rust-analyzer spawn count per full suite run | At most 2 (main suite + empty-workspace edge case) |
| SC-004 | A deliberately broken sub-case assertion | Suite fails with a report naming the specific failed sub-case, not a generic failure |

## 8. Agent Boundaries

### Always (without asking)
- Add a new `sc_*` sub-case (and registry entry) whenever a new MCP tool is added to
  `crates/mcpls-core/src/mcp/server.rs`, so `ra_e2e.rs` coverage stays in step with the tool surface
- Keep the single-driver-test model (`#[test] fn ra_e2e_suite()`) rather than splitting into
  per-tool `#[test]` functions, to preserve the cold-start amortization this spec requires (FR-001)
- Preserve the `MCPLS_SKIP_RA`/`MCPLS_RUST_ANALYZER` env-var contract exactly (FR-003/FR-004)

### Ask First
- Changing the suite from `#[ignore]`-gated (opt-in via `--ignored`) back toward the original
  fail-closed-by-default design (Section 2 of the Rev 2 doc) — this is a CI-policy decision with
  cost implications (every default CI run would then require rust-analyzer installed)
- Adding a second e2e binary or splitting the suite — changes the cold-start amortization
  guarantee and reintroduces the process-per-test problem this design exists to avoid

### Never
- Restructure `ra_e2e.rs` such that rust-analyzer is spawned once per tool/sub-case — defeats
  the entire point of this design (NFR-001, FR-001)
- Let a sub-case abort the whole suite on first failure — must collect and report all failures
  (FR-007)

## 9. Open Questions

- [NEEDS CLARIFICATION: Should the suite eventually move off `#[ignore]`-gating toward the
  original fail-closed default policy now that rust-analyzer installation is already automated in
  the dedicated e2e CI job? The current opt-in-via-`--ignored` approach is a safer default for
  contributors running the full suite locally without rust-analyzer, but it does mean a
  regression only surfaces in the dedicated e2e job, not every CI run.]
- [NEEDS CLARIFICATION: Should multi-language e2e coverage (pyright, gopls, clangd, etc.) be
  added as a follow-up, given this suite's single-driver-test pattern generalizes per language
  server? Out of scope for this spec, per the original Non-Goals, but worth tracking as a
  potential future spec.]

## 10. See Also

- [[plan|plan.md]] — architecture and rollout of this suite, adapted from the original Rev 2 design doc
- [[constitution]] — project principles (not yet created for this project)
- [[MOC-specs]] — all specifications
- [[mcp/002-mcp-resources-diagnostics/spec|spec mcp/002]] — MCP resources design this suite's `sc_list_resources`/`sc_read_resource`/`sc_subscribe_unsubscribe_resource` sub-cases exercise
- [[lsp/002-lsp317-missing-tools/spec|spec lsp/002]] — LSP 3.17 tools this suite's `sc_get_signature_help`/`sc_go_to_implementation`/`sc_go_to_type_definition`/`sc_get_inlay_hints` sub-cases exercise
- `crates/mcpls-core/tests/ra_e2e.rs` — the suite itself
- `crates/mcpls-core/tests/common/ra_probe.rs` — rust-analyzer resolution (`Found`/`Skipped`/`Missing`)
- `_source.md` (this directory) — the original Rev 2 design doc, preserved verbatim
- PR #125 (commit `6cae0e1`), #126 (commit `0f40608`), #139 (commit `027da84`), #225 — implementation history
