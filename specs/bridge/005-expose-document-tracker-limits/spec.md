---
aliases:
  - Document Tracker Resource Limits
tags:
  - sdd
  - spec
  - bridge
  - config
created: 2026-08-05
status: implemented
related:
  - "[[constitution]]"
  - "[[config/001-config-discovery-and-heuristics/spec]]"
  - "[[bridge/002-document-tracker-synchronization/spec]]"
---

# Feature: Expose DocumentTracker Resource Limits

> [!info] Metadata
> **Author**: rust-agents (CI cycle 017, live-testing finding)
> **Branch**: fix/010-expose-document-tracker-limits
> **Source finding**: enhancement, P2, filed during live-testing cycle 017

> [!success] Resolution
> Implemented by commit `e7a4dfe` (PR #324), closing #315 and #293. Matches the plan's resolved
> design exactly: `workspace.max_documents`/`workspace.max_file_size` are flat, optional TOML
> fields on `WorkspaceConfig` (mod.rs) with `0 = unlimited`, `WorkspaceConfig::resource_limits()`
> maps them onto `bridge::state::ResourceLimits`, and a new `Translator::with_resource_limits`
> builder wires the resolved limits into `serve()`'s `Translator` construction (composing
> correctly with the existing `with_extensions` builder in either call order). No CLI/env surface
> and no upper bound were added, per the plan's resolved design decisions. `docs/user-guide/configuration.md`
> was updated (FR-006) and `DocumentLimitExceeded`/`FileSizeLimitExceeded` messages gained a
> config-field hint (FR-005).
>
> **Deviation from the plan**: Section 10 of `plan.md` ("Constitution Compliance") assessed this
> change as fully backward-compatible with no breaking change needed. The shipped commit is
> `feat(config)!` and carries an explicit `BREAKING CHANGE` note covering two items outside this
> spec's original Functional Requirements: `NotificationCache::get_logs`/`get_messages` were
> renamed to `logs`/`messages` (dropping the redundant `get_` prefix per the Rust API Guidelines),
> and `ResourceLimits` — previously private to `bridge::state` — is now re-exported from `bridge`,
> making the already-`pub` `DocumentTracker::new` constructible from outside the crate for the
> first time. Neither change was anticipated in Section 1 (Overview) or Section 8 (Agent
> Boundaries, "Never: ... without explicit instruction") of this spec; they were bundled into the
> same PR rather than filed separately.

## 1. Overview

### Problem Statement

`DocumentTracker` (`crates/mcpls-core/src/bridge/state.rs`) enforces a
`ResourceLimits` cap in `open()` (state.rs:243-272): once
`documents.len() >= limits.max_documents`, every subsequent
`textDocument/didOpen`-triggering tool call (hover, definition, diagnostics,
etc.) against a *new* file hard-fails with `Error::DocumentLimitExceeded`
(`"document limit exceeded: N/100"`) — even though the underlying LSP server
is healthy and would happily serve the request.

`ResourceLimits::default()` (state.rs:143-150) sets `max_documents: 100` and
`max_file_size: 10 * 1024 * 1024` (10MB). Every construction site of
`ResourceLimits` in the codebase (`translator.rs:141, 249, 3280, 3295, 4095`)
calls `ResourceLimits::default()` — there is no TOML config field, CLI flag,
or environment variable anywhere in `crates/mcpls-cli/src/args.rs` or
`crates/mcpls-core/src/config/` that can override either limit (confirmed via
grep: zero matches for `max_documents`/`max_file_size`/`ResourceLimits`
outside `state.rs` and `translator.rs`'s `default()` call sites).

`ResourceLimits` already supports `0 = unlimited` for both fields internally
(see doc comments on `max_documents`/`max_file_size`, state.rs:137-141, and
the `> 0` guards in `open()`/`check_file_size()`) — the gap is purely that
nothing in the config/CLI/env layer can set a non-default value.

This matters because mcpls's stated purpose is bridging AI agent clients to
LSP servers, and agentic sessions commonly touch far more than 100 distinct
files over a session's lifetime (a broad refactor, a repo-wide audit, or
simply a long-running session accumulating opens over hours). There is
currently no way for an operator to raise this ceiling even if they know
about it, and nothing in the README documents that the limit exists, so a
user hitting it has no actionable remedy short of restarting the mcpls
process to reset `DocumentTracker`'s open-document set.

Secondary observation (context, not the primary ask): `RESOURCE_PAGE_SIZE =
100` in `crates/mcpls-core/src/mcp/server.rs:50-57` is deliberately decoupled
from `max_documents` per an existing code comment, but because
`max_documents` currently hard-caps open documents at exactly the same value
(100) and cannot be raised, the `resources/list` pagination path
(`next_cursor`/second page) is effectively unreachable in the shipped
product today. Confirmed live this cycle: opening 101-120 real documents
against the default config, the 101st and beyond all failed with
`DocumentLimitExceeded` before a second `resources/list` page could ever be
produced. The spec below treats this as a "why it matters" data point, not a
requirement to fix pagination itself.

### Goal

An operator can either (a) raise `max_documents`/`max_file_size` above their
current hardcoded defaults through a documented, validated configuration
surface, so long-running or broad-scope agent sessions don't hard-fail once
they cross 100 distinct open files; or, if the limits are intentionally
fixed, (b) discover the limit and its rationale from the README/config
reference before hitting it blind. Exact surface (TOML field vs. CLI flag vs.
both vs. docs-only) is `[NEEDS CLARIFICATION]` — see Section 9.

### Out of Scope

- Changing `RESOURCE_PAGE_SIZE` or `resources/list` pagination behavior
  itself (noted only as a downstream symptom).
- Automatic/dynamic eviction policies for `DocumentTracker` (e.g. LRU
  eviction of stale documents) — this spec only concerns making the static
  ceiling configurable/documented, not replacing it with a different
  resource-management strategy.
- Per-server or per-language resource limits — `ResourceLimits` is currently
  a single global cap shared by the whole `DocumentTracker`; splitting it
  per-server is not in scope unless the clarification in Section 9 resolves
  that way.
- Changing the default values (100 documents / 10MB) — this spec is about
  making the ceiling *configurable and/or documented*, not about picking new
  defaults.

## 2. User Stories

### US-001: Raise the document ceiling for a large workspace

AS A developer running mcpls against a large monorepo or a long agent session
I WANT to configure `max_documents` above 100
SO THAT hover/definition/diagnostics/etc. keep working past the 100th
distinct file opened in a session, without restarting mcpls

**Acceptance criteria:**
```
GIVEN a TOML config (or CLI flag/env var, per the chosen surface) that sets
  max_documents to 500
WHEN mcpls starts and a client opens more than 100 distinct documents in one
  session
THEN document #101 through #500 are opened successfully instead of failing
  with DocumentLimitExceeded
```

### US-002: Raise or remove the per-file size ceiling

AS A developer working with a project that contains files larger than 10MB
(e.g. generated code, data fixtures)
I WANT to configure `max_file_size` (including setting it to unlimited)
SO THAT opening those files for LSP-backed tools does not fail with
FileSizeLimitExceeded

**Acceptance criteria:**
```
GIVEN a TOML config (or equivalent surface) that sets max_file_size to 0
  (unlimited) or to a value above 10MB
WHEN a client requests a tool that opens a file larger than 10MB
THEN the file opens successfully instead of failing with
  FileSizeLimitExceeded
```

### US-003: Understand the limit when it is hit

AS A developer or operator who has not customized resource limits
I WANT the README/config reference to document that a document-count ceiling
exists, its default value, and how (if possible) to raise it
SO THAT hitting `DocumentLimitExceeded` mid-session is not a surprise with no
actionable next step

**Acceptance criteria:**
```
GIVEN the shipped README and/or config reference documentation
WHEN a user searches for "document limit" or reads the config field
  reference
THEN they find the default value (100 documents / 10MB), an explanation of
  what happens when it is hit, and the configuration knob (if one exists) or
  an explicit statement that restarting mcpls is the only remedy
```

## 3. Functional Requirements

Use EARS notation. Prefix with FR-NNN. Exact mechanism (TOML/CLI/env) is
`[NEEDS CLARIFICATION]` (Section 9) — requirements below are written against
whichever surface(s) the resolved design adopts.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN mcpls loads its configuration THE SYSTEM SHALL allow `max_documents` to be set to a value other than the hardcoded default of 100, via at least one of: TOML field, CLI flag, environment variable | must |
| FR-002 | WHEN mcpls loads its configuration THE SYSTEM SHALL allow `max_file_size` to be set to a value other than the hardcoded default of 10MB, via at least one of: TOML field, CLI flag, environment variable | must |
| FR-003 | WHEN a configured `max_documents` or `max_file_size` value is provided THE SYSTEM SHALL validate it (e.g. reject nonsensical values) following the same validation pattern used for `request_timeout_seconds` in `crates/mcpls-core/src/config/mod.rs` (reject `0` only where `0` is not the documented "unlimited" sentinel; reject values that would defeat the purpose of the limit if a sane upper/lower bound is warranted) | must |
| FR-004 | WHEN no explicit `max_documents`/`max_file_size` configuration is provided THE SYSTEM SHALL fall back to the current defaults (100 documents, 10MB) unchanged, preserving today's out-of-the-box behavior | must |
| FR-005 | WHEN `DocumentTracker::open()` rejects a document because the configured `max_documents` is exceeded THE SYSTEM SHALL include, in the `Error::DocumentLimitExceeded` message or accompanying context, a hint that the ceiling is configurable (if FR-001 lands) or a pointer to documentation explaining the limit (if only documentation lands) | should |
| FR-006 | WHEN the README or config reference documentation is generated/updated THE SYSTEM SHALL document `max_documents` and `max_file_size`: their defaults, what happens when exceeded, and how (if at all) to change them | must |
| FR-007 | WHEN `ResourceLimits` is constructed anywhere in the codebase (`translator.rs:141, 249, 3280, 3295, 4095`, and any call site added since) THE SYSTEM SHALL source the values from the resolved configuration surface rather than calling `ResourceLimits::default()` unconditionally, except in test-only construction sites which may continue using `ResourceLimits::default()` for test isolation | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Backward compatibility | Existing configs with no limits section/flags must continue to behave exactly as today (100 documents, 10MB, unchanged error messages except for the FR-005 hint addition) |
| NFR-002 | Consistency | The new config surface must follow the existing pattern used for `request_timeout_seconds` (per-field TOML with `#[serde(default = "...")]`, validated in `ServerConfig::validate`) or `heuristics_max_depth` (workspace-level TOML field), whichever the resolved design location (Section 9) dictates |
| NFR-003 | Safety | If `max_documents`/`max_file_size` are made unbounded-settable, the config validation should not silently accept obviously-unsafe values (e.g. `max_documents = 1`) that would break basic operation — mirrors the existing `MAX_TIMEOUT_SECONDS` upper-bound pattern in `crates/mcpls-core/src/config/server.rs` |
| NFR-004 | Documentation | README changes must not use GitHub-specific callout syntax if the README is also published outside GitHub (per repository doc conventions) — confirm current README publishing target before adding callouts |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|-----------------|
| `ResourceLimits` (existing, `state.rs:135-150`) | In-memory struct passed to `DocumentTracker::new`, caps open-document count and per-file byte size | `max_documents: usize` (0 = unlimited), `max_file_size: u64` (0 = unlimited) |
| Configurable limits surface (new, location TBD) | Wherever the resolved design places the user-facing knob — e.g. a new `[workspace.limits]` TOML table, new CLI flags/env vars, or both | Candidate fields: `max_documents`, `max_file_size`; must map 1:1 onto `ResourceLimits` |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|--------------------|
| Config sets `max_documents = 0` | Interpreted as unlimited, per the existing `ResourceLimits` doc comment and the `> 0` guard already in `open()` — no code change needed there, only plumbing the value through |
| Config sets `max_file_size = 0` | Interpreted as unlimited, same rationale as above via `check_file_size()`'s existing `> 0` guard |
| Config sets a negative or non-numeric value (TOML) | Rejected at config load/validation time with a clear error message, consistent with how `request_timeout_seconds = 0` is currently rejected in `crates/mcpls-core/src/config/mod.rs` |
| CLI flag and TOML both set the limit to different values (if both surfaces are chosen) | Follow existing precedence pattern in the codebase (CLI/env typically override TOML, mirroring `--config`/`MCPLS_CONFIG` precedence) — must be explicitly resolved and documented if both surfaces are implemented |
| User raises `max_documents` very high (e.g. 100,000) on a memory-constrained host | Out of scope to auto-detect and warn; NFR-003 only requires rejecting values that are unsafely *low*, not capping unsafely *high* values, unless the resolved design decides otherwise |
| `DocumentLimitExceeded` is hit even with limits raised (genuinely more open documents than the new ceiling) | Same error as today, but message references the *effective* configured value, not always "100" |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Reproduction from the finding (120 files, default config) | Continues to fail at file #101 with unchanged defaults — regression guard proving FR-004 holds |
| SC-002 | Reproduction with `max_documents` raised (e.g. to 200) via the chosen surface | Files #101-120 succeed, no `DocumentLimitExceeded` |
| SC-003 | `cargo nextest run --workspace --all-features` | New/updated tests for config parsing, validation, and `DocumentTracker` wiring pass |
| SC-004 | README / config reference | Contains a documented entry for `max_documents`/`max_file_size` discoverable by grep for "max_documents" or "document limit" |

## 8. Agent Boundaries

### Always (without asking)
- Run `cargo +nightly fmt --all`, `cargo clippy --all-targets --all-features --workspace -- -D warnings`, and `cargo nextest run --workspace --all-features --lib --bins` after implementation changes
- Follow the existing `request_timeout_seconds`/`heuristics_max_depth` config patterns (serde defaults, `ServerConfig::validate` checks) rather than inventing a new mechanism
- Preserve `ResourceLimits`'s existing `0 = unlimited` semantics without changing `open()`/`check_file_size()` guard logic
- Update `CHANGELOG.md` under `[Unreleased]`

### Ask First
- Which config surface to implement (TOML-only, CLI-only, both, or docs-only) — this is the open `[NEEDS CLARIFICATION]` in Section 9 and materially changes the plan
- Whether to add an upper bound to `max_documents`/`max_file_size` (NFR-003) and what that bound should be
- Whether `max_documents`/`max_file_size` should live under a new `[workspace.limits]` TOML table vs. flattened into `[workspace]` directly vs. a new top-level table

### Never
- Silently change the default values (100 documents, 10MB) without an explicit `BREAKING CHANGE`/major-version decision from the user
- Remove or weaken the `ResourceLimits` enforcement itself (e.g. defaulting to unlimited) without explicit instruction — this is a resource-safety mechanism, not dead code
- Touch `RESOURCE_PAGE_SIZE`/`resources/list` pagination logic — explicitly out of scope per Section 1

## 9. Open Questions

- [NEEDS CLARIFICATION: Should `max_documents`/`max_file_size` be exposed via TOML config field(s), CLI flag(s), environment variable(s), some combination, or (if the limits are judged intentionally fixed) documentation-only with no configurability at all? The finding explicitly leaves this open as a design decision for whoever picks up the issue.]
- [NEEDS CLARIFICATION: If TOML, does it belong under the existing `[workspace]` table (alongside `heuristics_max_depth`) or a new dedicated table, e.g. `[workspace.limits]`?]
- [NEEDS CLARIFICATION: Should there be an enforced upper bound on `max_documents`/`max_file_size` (mirroring `MAX_TIMEOUT_SECONDS`'s pattern in `config/server.rs`), or is any positive value (plus `0` for unlimited) acceptable?]
- [NEEDS CLARIFICATION: Is `ResourceLimits` intended to remain a single global cap, or should this work also consider per-server-config limits given `LspServerConfig` already carries per-server settings like `request_timeout_seconds`? Default assumption for this spec is global-only unless redirected.]

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- `crates/mcpls-core/src/bridge/state.rs` — `DocumentTracker`, `ResourceLimits`, `Error::DocumentLimitExceeded`
- `crates/mcpls-core/src/bridge/translator.rs` — all `ResourceLimits::default()` construction sites
- `crates/mcpls-core/src/config/mod.rs` — `ServerConfig`, `WorkspaceConfig`, existing validation pattern for `request_timeout_seconds`
- `crates/mcpls-core/src/config/server.rs` — `LspServerConfig`, `MAX_TIMEOUT_SECONDS` upper-bound validation pattern
- `crates/mcpls-cli/src/args.rs` — existing CLI flag/env var pattern (`clap` + `env = "MCPLS_*"`)
- `crates/mcpls-core/src/mcp/server.rs:50-57` — `RESOURCE_PAGE_SIZE`, decoupled-but-currently-coupled-in-practice pagination context
