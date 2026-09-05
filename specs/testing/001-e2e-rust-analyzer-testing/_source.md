# Design: E2E Tests with Real rust-analyzer (Rev 2)

**Status:** revised proposal
**Owner:** architect
**Date:** 2026-04-29
**Scope:** crates/mcpls-core/tests/e2e
**Supersedes:** Rev 1 (2026-04-29T16-30); responds to critic 2026-04-29T17-07-10

## Changes from Rev 1

- C1: switched from nextest-shared `OnceCell` (broken under process-per-test) to a **single driver test** model + an explicit `cargo test` carve-out for the e2e binary. Daemon variant kept as a documented fallback.
- S1: readiness gate now subscribes to `$/progress` (`rustAnalyzer/Indexing`) with `rust-analyzer/serverStatus` as a secondary signal; hover-poll dropped.
- S2: required-by-default; opt-out via `MCPLS_SKIP_RA=1`. Skip path returns a panic in the default policy and is only neutered for the documented MSRV/minimal job.
- S4: each test binary copies the fixture into its own `TempDir` workspace.
- S5: explicit mapping of **all 16** existing MCP tools (verified against `crates/mcpls-core/src/mcp/server.rs`); deferrals marked.
- S3, M1–M8: addressed inline.

## Goals

1. Exercise the full MCP→mcpls→LSP→rust-analyzer path against a real language server.
2. Cover **all 16 currently-implemented MCP tools** with at least one happy-path assertion each.
3. Skip cleanly only in the documented MSRV/minimal CI job; default policy is "fail closed if rust-analyzer is missing".
4. Pay rust-analyzer cold-start + index cost **once for the e2e suite**, not once per test.

## Non-Goals

- Replacing `MockLspServer` for protocol-level unit tests.
- Performance benchmarks.
- Multi-language coverage (Rust only).
- New MCP tools (signature_help, type_definition, implementation, inlay_hint, etc. — not implemented in mcpls today; out of scope).

---

## 1. Process Model (resolves C1)

### Problem

cargo-nextest's default execution model is **process-per-test**: each `#[test]` runs in `<binary> --exact <name>`. A `static OnceCell` lives only for that single subprocess — meaning a per-binary harness re-spawns rust-analyzer for *every* test. Goal #4 cannot hold under nextest's default model.

### Decision: dual-mode harness, default = single driver test

The e2e suite is implemented as one `#[test] fn ra_e2e_suite()` in `tests/ra_e2e.rs` (a top-level integration test binary, *not* a nextest test_group case). It performs:

1. Probe + spawn rust-analyzer once.
2. Wait for indexing readiness (§3).
3. Sequentially invoke a registry of sub-cases, each producing a structured `SubResult { name, outcome }`.
4. After all sub-cases run, panic with an aggregated report iff any failed.

This **preserves the cold-start amortization under both `cargo test` and `cargo nextest run`**: nextest sees one test, runs one process, harness lives for the whole suite.

### Trade-offs accepted

- **Loss of per-tool failure granularity in test output.** Mitigated by: (a) the aggregated failure report names every failed sub-case with its assertion message, (b) sub-cases continue past the first failure (collected, not propagated) so one regression doesn't mask others, (c) `MCPLS_RA_FILTER=<substring>` env var lets developers run a single sub-case locally.
- Loss of nextest's per-test isolation. Acceptable because the only shared mutable state is rust-analyzer itself, and sub-cases avoid mutating fixture files — write tests use in-memory edits or per-case TempDir copies.
- Cannot use nextest retry policy on individual sub-cases. Acceptable for a single-binary suite.

### Rejected alternatives

- **`cargo test` carve-out for the whole e2e binary.** Possible (`cargo test --test ra_e2e -- --test-threads=1`) but creates two divergent CI invocations; the single-driver design works under both runners and is preferred.
- **Out-of-process rust-analyzer daemon keyed by `$CARGO_TARGET_TMPDIR/ra.lock`.** Most complex; defers no real benefit until we have >1 e2e binary. Documented as future option if the suite splits.
- **`nextest` test-groups with `max-threads = 1`.** Still process-per-test — solves serialization, not amortization. Does not address C1.

---

## 2. Skip Policy (resolves S2)

```rust
// tests/common/ra_probe.rs
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

Driver behavior:

```rust
match resolve_rust_analyzer() {
    Found(p)   => run_suite(&p),
    Skipped(r) => { println!("e2e suite skipped: {r}"); }   // visible under nextest --no-capture / cargo test default
    Missing    => panic!("rust-analyzer not found; set MCPLS_SKIP_RA=1 to skip"),
}
```

Notes:
- `MCPLS_SKIP_RA=1` exact match — empty string does not skip (closes the `is_ok()` ambiguity).
- Skip prints to stdout, not stderr — surfaces in the standard nextest summary.
- MSRV/minimal CI job is the only place that sets `MCPLS_SKIP_RA=1`; default jobs install rust-analyzer (`rustup component add rust-analyzer`) and let a missing binary fail.

---

## 3. Readiness Gate (resolves S1)

Hover-poll is rejected (cold rust-analyzer returns `null`, not a "loading" marker; result is indistinguishable from a real lookup miss).

### Authoritative oracle

Subscribe to LSP `$/progress` notifications during initialization. rust-analyzer reports indexing under the `rustAnalyzer/Indexing` (or `rust-analyzer/Indexing` on older versions) work-done token: `kind: "begin"` → ... → `kind: "end"`. Readiness = receipt of the `end` notification.

Secondary fallback: rust-analyzer's experimental `rust-analyzer/serverStatus` notification (`{ health: "ok", quiescent: true }`).

### Plumbing

mcpls already caches LSP push notifications via `bridge/notifications.rs`. The harness reads them through `get_server_messages` (one of the existing 16 MCP tools — also exercised as a tool test). No new bridge code required.

```rust
pub async fn wait_until_indexed(client: &mut McpClient, deadline: Instant) -> Result<()> {
    loop {
        let msgs = client.call_tool("get_server_messages", json!({ "server": "rust-analyzer" }))?;
        if has_progress_end(&msgs, "rustAnalyzer/Indexing")
            || has_status_quiescent(&msgs)
        {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("rust-analyzer not quiescent within timeout; last messages: {msgs}");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}
```

Timeout default 60 s, override via `MCPLS_RA_INDEX_TIMEOUT_SECS`. The harness lock is **not** held across this wait (M3 fix): the readiness gate runs before sub-cases acquire the harness handle.

### `WorkDoneProgress` initialization capability

The harness's MCP `initialize` request must advertise `window.workDoneProgress = true`. Currently mcpls's MCP-side capabilities advertise it; verify in implementation. If absent, rust-analyzer omits progress notifications and the gate falls back to `serverStatus` only.

---

## 4. Fixture Isolation (resolves S4)

Each suite run copies the committed `tests/fixtures/rust_workspace/` into a `TempDir` and points the generated `mcpls.toml` at the copy. Reasons:

- Concurrent rust-analyzer instances (across this binary and any future e2e binary) would otherwise share `target/rust-analyzer/`, racing on RocksDB-style metadata.
- Eliminates the `target/` and `Cargo.lock` pollution currently visible in `git status` (which prompted PR #120).
- Lets diagnostic and rename sub-cases write modifications without touching the committed fixture.

```rust
fn stage_workspace() -> Result<TempDir> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_workspace");
    let dst = TempDir::new()?;
    fs_extra::dir::copy(&src, dst.path(), &copy_opts_overwrite_inside())?;
    Ok(dst)
}
```

Writable sub-cases (rename, format, code_action) operate on per-case file copies inside this TempDir.

---

## 5. Tool Coverage (resolves S5)

The 16 implemented MCP tools, verified against `crates/mcpls-core/src/mcp/server.rs`:

| # | MCP tool | Sub-case | Assertion |
|---|----------|----------|-----------|
| 1 | `get_hover` | hover over `add` decl | text contains `fn add` and the substring `i32` (≥3 occurrences); no exact label match (S3) |
| 2 | `get_definition` | over `add` in `caller` | URI ends `/src/lib.rs`; line == decl line |
| 3 | `get_references` | on `add` decl | ≥ 2 locations including decl + call site |
| 4 | `get_diagnostics` | on per-case `broken.rs` (TempDir) — opens via tool call, debounces 1.5 s, then polls `get_cached_diagnostics` | severity Error; message matches regex `mismatched types\|expected.*found` |
| 5 | `rename_symbol` | call `prepare_rename` first (M7), then rename `add` → `plus` | edit set covers decl + every call site; no edits outside `lib.rs` |
| 6 | `get_completions` | inside `caller` after typing `ad` | candidate label `add` present |
| 7 | `get_document_symbols` | `lib.rs` | symbol set ⊇ `{add, caller, Point}` with kinds Function/Function/Struct |
| 8 | `format_document` | per-case copy of `bad_format.rs` (TempDir) | output equals committed golden file `tests/fixtures/golden/bad_format.fmt.rs`; rustfmt edition pinned via `rustfmt.toml` (S3) |
| 9 | `workspace_symbol_search` | query `"add"` | non-empty; at least one entry has kind Function and name `add` |
|10 | `get_code_actions` | at the diagnostic position from #4 | response contains an action whose `kind` starts with `quickfix` (S3 — narrowed from "≥1 action") |
|11 | `prepare_call_hierarchy` | on `add` | exactly one item, name == `add` |
|12 | `get_incoming_calls` | on the prepared item from #11 | ≥ 1 entry whose `from.name` == `caller` |
|13 | `get_outgoing_calls` | on the prepared item from #11 | empty list (`add` calls nothing) |
|14 | `get_cached_diagnostics` | after #4 has populated the cache | returns the same diagnostic as #4 (read path through cache only) |
|15 | `get_server_logs` | after suite warmup | non-empty array; entries have `server: "rust-analyzer"` |
|16 | `get_server_messages` | already exercised by readiness gate (§3) | progress-end or quiescent status observed |

**Deferrals:** none. All 16 are covered. If implementation discovers a sub-case is unstable on rust-analyzer's pinned version, it must be marked with:

```rust
// TODO(critic): cover <tool> in e2e suite — see handoff 2026-04-29T17-07-10-critic.md
```

placed in `tests/ra_e2e.rs`, per the critic's deferral protocol.

### Fixture surface (compilable as written) (M2 fix)

```rust
// tests/fixtures/rust_workspace/src/lib.rs
use std::fmt;

pub fn add(a: i32, b: i32) -> i32 { a + b }
pub fn caller() -> i32 { add(1, 2) }

pub struct Point { pub x: f64, pub y: f64 }

impl fmt::Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.x, self.y)
    }
}
```

`bad_format.rs` and `broken.rs` live in `tests/fixtures/rust_workspace/extras/` (excluded from the crate's mod tree by being in a non-mod path) and are copied into the per-case TempDir on demand.

---

## 6. Assertion Helpers

`tests/common/assertions.rs` — plain functions:

```rust
pub fn assert_mcp_ok(resp: &Value) -> &Value;
pub fn content_text(resp: &Value) -> String;
pub fn assert_uri_ends_with(uri: &str, suffix: &str);
pub fn assert_position(loc: &Value, line: u32, col: u32);
pub fn assert_contains_symbol(symbols: &Value, name: &str, kind: i64);
pub fn file_uri(p: &Path) -> String; // percent-encoded, scheme `file://`, handles macOS /var/folders (M1)
```

`file_uri` uses the `url` crate's `Url::from_file_path` which is already a transitive workspace dep — no new dependency required (verify before implementation).

---

## 7. Edge Cases

| Case | Strategy |
|------|----------|
| Missing file URI | `get_hover` on `file:///does/not/exist.rs` → MCP error response (assert non-null `error`); harness does not panic. |
| OOB position | Past EOF → empty MCP content (rust-analyzer returns `null` upstream). |
| Empty workspace | Separate sub-case spawns a *second* rust-analyzer against a TempDir with only `Cargo.toml` + empty `src/lib.rs`; verifies init succeeds and `workspace_symbol_search("x")` returns empty. Pays a second cold start — accepted because it specifically validates the empty-workspace path. |
| rust-analyzer crash | Driver wraps each sub-case in `AssertUnwindSafe + catch_unwind`; on transport error, all later sub-cases short-circuit to a "skipped — server dead" result; aggregated report still prints. |
| Slow indexing | Readiness gate timeout via `MCPLS_RA_INDEX_TIMEOUT_SECS` (default 60). |
| UTF-16 column (M6) | Sub-case uses a file with `let π = 1; π.checked_add(1)`; `get_hover` at the column of `.checked_add` must return a hover; assert that `bridge/encoding.rs` is converting UTF-16 columns by checking the response is non-null *and* that hovering one column earlier returns a hover for `π`, not `checked_add`. |
| Diagnostic timing (M4) | After opening `broken.rs` via `get_hover` (forces `didOpen`), wait up to 5 s polling `get_cached_diagnostics` before asserting; diagnostics are debounced by rust-analyzer. |

Concurrency tests remain out of scope here — covered in `integration/` against the translator-lock fix (issue #104).

---

## 8. CI Gating

- **Default jobs:** `rustup component add rust-analyzer` in setup; no env vars needed; missing binary → suite panics.
- **MSRV / minimal jobs:** set `MCPLS_SKIP_RA=1`; suite prints a skip line and returns success.
- **Pinned-version job:** explicit `rust-analyzer-2026-NN-NN` install; the canonical signal.
- **Nightly rust-analyzer job:** uses latest release; `continue-on-error: true`.
- **Renovation:** Renovate (or Dependabot for actions) configured to bump the pinned RA version on a 2-week cadence. Documented in `CHANGELOG.md`.

---

## 9. File Layout

```
crates/mcpls-core/tests/
├── common/
│   ├── ra_probe.rs        (new)
│   └── assertions.rs      (new)
├── ra_e2e.rs              (new) — single-test driver binary; contains harness + sub-case registry
└── fixtures/
    ├── rust_workspace/
    │   ├── src/lib.rs     (extend per §5)
    │   └── extras/
    │       ├── bad_format.rs   (new)
    │       └── broken.rs       (new)
    └── golden/
        └── bad_format.fmt.rs   (new — pinned rustfmt edition)
```

`ra_e2e.rs` is a top-level integration test binary (separate from `e2e/` mod tree) so the single-driver model is unambiguous to nextest.

---

## 10. Open Questions for Implementer

1. Verify mcpls advertises `window.workDoneProgress = true` in MCP→LSP `initialize`. If not, file a follow-up before merging the suite, since the readiness gate degrades to `serverStatus`-only.
2. Verify `which` is already a workspace dev-dependency and its current MSRV ≤ project MSRV (M5). If not, pin a version that is.
3. Confirm `url` crate is reachable as a non-dev dep for the `file_uri` helper, or add as `[dev-dependencies]`.
4. Choose pinned rust-analyzer version for CI; current recommendation: `rust-analyzer 2026-04-21`.

---

## 11. Acceptance Criteria

- `cargo nextest run --workspace --all-features` (default jobs) executes the e2e driver, exercises all 16 MCP tools, completes under 180 s wall clock, exits 0.
- `cargo test --test ra_e2e` produces equivalent results.
- Without rust-analyzer and without `MCPLS_SKIP_RA=1`, the suite fails (does not silently pass).
- With `MCPLS_SKIP_RA=1`, the suite prints a skip line and exits 0.
- Each suite invocation spawns rust-analyzer at most twice (main + empty-workspace edge case).
- Suite output names every failing sub-case; no failure aborts later sub-cases.
