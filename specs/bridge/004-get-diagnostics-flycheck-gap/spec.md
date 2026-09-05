---
aliases:
  - get_diagnostics flycheck gap
tags:
  - sdd
  - spec
  - bug
  - bridge
  - diagnostics
created: 2026-07-25
status: draft
related:
  - "[[MOC-specs]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec]]"
---

# Feature: Fix `get_diagnostics` Silently Omitting Flycheck-Sourced Diagnostics

> [!info] Metadata
> **Author**: k05h31
> **Branch**: fix/get-diagnostics-flycheck-gap
> **Type**: bug
> **Priority**: P1

## 1. Overview

### Problem Statement

`mcp/server.rs`'s `get_diagnostics` tool (implemented by
`Translator::handle_diagnostics`, `crates/mcpls-core/src/bridge/translator.rs`
lines 954-1000) uses the LSP **pull** model exclusively: it sends
`textDocument/diagnostic` to the LSP server and returns only that response's
`items`. Separately, `NotificationCache`
(`crates/mcpls-core/src/bridge/notifications.rs`) is populated by the LSP
**push** model (`textDocument/publishDiagnostics`), and is exposed via the
`get_cached_diagnostics` tool and the `resources/read` / `resources/subscribe`
MCP resource endpoints (see [[mcp/002-mcp-resources-diagnostics/spec|Spec mcp/002]]).

For rust-analyzer these two sources diverge. rust-analyzer's pull endpoint
only returns diagnostics from its synchronous native analysis (syntax errors,
type errors, borrow-checker errors). It never includes diagnostics sourced
from its background "flycheck" process (`cargo check`, or clippy if
configured) — those are lint-style warnings like `unused_imports` and
`dead_code`, and they are delivered *only* via `publishDiagnostics` push
notifications, never via the pull endpoint. This is a documented rust-analyzer
behavior, not an mcpls timing bug: mcpls already holds the flycheck
diagnostics in `NotificationCache` (proven by `get_cached_diagnostics`
returning them correctly) at the exact moment `get_diagnostics` reports the
file clean.

`get_diagnostics` is the tool most likely to be reached for by an AI agent —
it is the one named and described for exactly this purpose ("Diagnostics for
a file. Returns errors, warnings, and hints with severity and location.").
Its silent, incomplete result is worse than an error: it actively implies the
file has no unused-import/dead-code warnings when it does, so an agent may
skip fixing an issue it would have caught if a different tool name had been
used.

### Goal

`get_diagnostics` returns the same diagnostics for a file that
`get_cached_diagnostics` / `resources/read` would return for that file at the
same point in time — i.e., it never silently omits warnings mcpls already
knows about from `publishDiagnostics` pushes, for any LSP server exhibiting
this pull/push divergence (not just rust-analyzer).

### Out of Scope

- Changing `get_cached_diagnostics` or the `resources/*` endpoints — they
  already behave correctly and are not part of this bug.
- Forcing rust-analyzer to run flycheck synchronously before responding to a
  pull request — this is not controllable from the client side of the LSP
  protocol.
- Adding a way to distinguish "native" vs "flycheck" diagnostics in the tool
  output (could be a follow-up enhancement, not required to close this gap).
- Changing behavior for LSP servers whose pull and push diagnostics are
  already consistent (e.g. servers with no separate background-check
  process) — the fix must not regress or change output for those servers.

## 2. User Stories

### US-001: Agent gets complete diagnostics from the primary tool

AS AN AI coding agent
I WANT `get_diagnostics` to include every diagnostic mcpls already knows
about for a file
SO THAT I don't skip fixing warnings (e.g. unused imports, dead code) just
because I used the tool with the more obvious name instead of
`get_cached_diagnostics`

**Acceptance criteria:**
```
GIVEN a Rust file with an unused import
  AND rust-analyzer has already pushed a publishDiagnostics notification
      for that file containing the unused_imports warning
WHEN the AI agent calls get_diagnostics for that file
THEN the response includes the unused_imports warning
  AND the response is consistent with what get_cached_diagnostics returns
      for the same file at the same point in time
```

### US-002: Native (pull-only) diagnostics still work

AS AN AI coding agent
I WANT `get_diagnostics` to still report native LSP errors correctly (e.g.
type errors) with the fix applied
SO THAT the fix for the flycheck gap does not regress the currently-working
pull path

**Acceptance criteria:**
```
GIVEN a Rust file with a genuine type error (e.g. E0308 mismatched types)
WHEN the AI agent calls get_diagnostics for that file
THEN the response includes the type error
  AND the fix does not require rust-analyzer to have pushed a
      publishDiagnostics notification first for this case to work
```

## 3. Functional Requirements

The fix must guarantee:

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `get_diagnostics` is called for a file THE SYSTEM SHALL return a diagnostics set that is a superset of (or equal to) what `NotificationCache` currently holds for that file's URI, merged with what the LSP pull response (`textDocument/diagnostic`) returns | must |
| FR-002 | WHEN the pull response (`textDocument/diagnostic`) for a file is empty or incomplete relative to the cached diagnostics THE SYSTEM SHALL NOT report the file as having zero diagnostics if `NotificationCache` holds non-empty diagnostics for that URI | must |
| FR-003 | WHEN the pull response and the cached diagnostics both contain a diagnostic that is semantically the same (same range, severity, code, message) THE SYSTEM SHALL NOT return duplicate entries for it | must |
| FR-004 | WHEN `NotificationCache` has no entry yet for a file (e.g., no `publishDiagnostics` has arrived) THE SYSTEM SHALL fall back to the pull response alone, exactly as today | must |
| FR-005 | WHEN the merge/fallback logic runs THE SYSTEM SHALL NOT introduce an additional LSP round-trip beyond the existing `textDocument/diagnostic` pull request (the cache read is in-process and already exists) | must |
| FR-006 | WHEN `get_diagnostics` is called for a language server whose pull and push diagnostics are already consistent THE SYSTEM SHALL produce the same observable output as before this fix (no regression for non-rust-analyzer or non-divergent servers) | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Merge/dedup logic must be O(n) or O(n log n) in the number of diagnostics for the file (typically single/low double digits) — no measurable added latency to `get_diagnostics` beyond the existing pull round-trip |
| NFR-002 | Concurrency | Reading `NotificationCache` inside `handle_diagnostics` must follow the existing lock-ordering discipline documented in `translator.rs` (cache lock must not be held across the LSP pull await point) — see [[bridge/003-rwlock-translator/spec|Spec bridge/003]] |
| NFR-003 | Consistency | Merged output must use the same `Diagnostic` → MCP mapping (severity, range normalization, code) already used by both `handle_diagnostics` and `diagnostics_from_cache_entry`, so the two code paths do not diverge in formatting |

## 5. Data Model

No new entities. The fix operates on two existing shapes:

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Pull diagnostics | Result of `textDocument/diagnostic` LSP request, as consumed today in `handle_diagnostics` | `items: Vec<lsp_types::Diagnostic>` |
| `DiagnosticInfo` (cached) | Existing cache entry keyed by document URI in `NotificationCache`, populated by `publishDiagnostics` | `uri`, `version`, `diagnostics: Vec<lsp_types::Diagnostic>` |
| Merged `DiagnosticsResult` | MCP-facing output of `get_diagnostics` after the fix | `diagnostics: Vec<Diagnostic>` (existing MCP type, unchanged shape) |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| No cache entry exists yet for the file (fresh file, no push received) | Fall back to pull response only, unchanged from current behavior (FR-004) |
| Cache entry exists but is empty (server previously cleared diagnostics via an empty `publishDiagnostics`) | Empty cache and empty pull both yield an empty result — no false positives from stale cache data |
| Cache entry is stale relative to the current document version (file edited since last `publishDiagnostics`) | `[NEEDS CLARIFICATION: should merge check DiagnosticInfo.version against the current document version and skip/flag stale cache entries, or is "cache is eventually consistent, same as get_cached_diagnostics already accepts" an acceptable behavior for this fix?]` |
| Pull and cache both return the same diagnostic with minor formatting differences (e.g. different code representation) | Dedup must compare on normalized (range, severity, message, code) tuples, not raw equality, to avoid duplicate-looking entries |
| LSP server does not support `textDocument/diagnostic` pull at all | Existing error/fallback behavior for missing pull capability is unchanged; cache-only result should still be considered, per FR-001 |
| Workspace/path validation failure for the requested file | Unchanged — existing `prepare_document` validation runs before any diagnostics logic, as today |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Reproduction scenario from the bug report (unused import via rust-analyzer flycheck) | `get_diagnostics` returns the `unused_imports` warning, matching `get_cached_diagnostics` output |
| SC-002 | Control scenario from the bug report (E0308 type error) | `get_diagnostics` continues to return the error correctly, unchanged |
| SC-003 | No duplicate diagnostics reported when pull and cache overlap | 0 duplicate entries in test coverage for overlapping pull+cache scenarios |
| SC-004 | Existing `handle_diagnostics` unit/integration tests | All pass unchanged (or updated only where the merge behavior is the explicit subject of the test) |

## 8. Agent Boundaries

### Always (without asking)
- Reuse the existing `diagnostics_from_cache_entry` / `Diagnostic` mapping helpers rather than duplicating conversion logic
- Add/update unit tests covering: cache-empty fallback, cache-and-pull merge with overlap, cache-and-pull merge with no overlap, dedup
- Follow the lock-ordering discipline already documented around `NotificationCache` in `translator.rs` (see comments near `cached_diagnostics_uri`)

### Ask First
- Any change to `get_cached_diagnostics` or `resources/*` behavior (out of scope; flag if the fix seems to require touching them)
- Resolving the staleness question in the Edge Cases table (version-check vs. eventually-consistent) — pick a default but confirm before merging if it affects the public `DiagnosticsResult` shape

### Never
- Add an additional LSP request/round-trip to work around this (violates NFR-001/FR-005)
- Silently change the JSON shape of `DiagnosticsResult` (e.g. adding a `source: pull|cache` field) without treating it as a documented, deliberate API addition

## 9. Open Questions

- [NEEDS CLARIFICATION: Merge strategy — should the fix (a) always merge pull ∪ cache, (b) prefer cache and only fall back to pull when cache is empty, or (c) explicitly query flycheck status via some LSP extension if available? The bug report leaves this open as "at minimum... merge/fall back to the NotificationCache when the pull response is empty or incomplete."]
- [NEEDS CLARIFICATION: Should merged results indicate provenance (native vs. flycheck-sourced) to help agents distinguish severity classes, or is a flat merged list sufficient for v1?]
- [NEEDS CLARIFICATION: Cache staleness handling — see Edge Cases table row on document version mismatch.]
- [NEEDS CLARIFICATION: Does this same divergence affect other LSP servers used by mcpls (pyright, typescript-language-server, etc.), or is it rust-analyzer-specific? If server-specific, should the merge be conditional on `language_id`/server capabilities, or applied uniformly since a uniform merge is provably safe per FR-006?]

## 10. See Also

- [[MOC-specs]] — all specifications
- [[bridge/003-rwlock-translator/spec|Spec bridge/003]] — `NotificationCache` / `Translator` lock-ordering discipline this fix must respect
- [[mcp/002-mcp-resources-diagnostics/spec|Spec mcp/002]] — the `NotificationCache` / `get_cached_diagnostics` / resources design this fix reuses
- `crates/mcpls-core/src/bridge/translator.rs::handle_diagnostics` (lines 954-1000) — code to be modified
- `crates/mcpls-core/src/bridge/translator.rs::diagnostics_from_cache_entry` (lines 1626-1652) — existing cache→MCP mapping to reuse
- `crates/mcpls-core/src/bridge/notifications.rs::NotificationCache` — existing push-based cache
