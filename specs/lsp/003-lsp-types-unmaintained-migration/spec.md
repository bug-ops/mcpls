---
aliases:
  - lsp-types unmaintained
  - ls-types migration research
tags:
  - sdd
  - spec
  - research
  - dependency-health
  - lsp
created: 2026-08-05
status: draft
related:
  - "[[constitution]]"
  - "[[bridge/001-position-encoding-layer/spec]]"
---

# Feature: Migrate off unmaintained `lsp-types` (gluon-lang) to the maintained `ls-types` fork

> [!info] Metadata
> **Author**: rust-researcher (filed from dependency-health research cycle)
> **Branch**: n/a (research spec — no implementation branch yet)
> **Type**: research / dependency-health
> **Priority**: P2

> [!abstract]
> This is a research spec. It documents a supply-chain risk (stale upstream
> dependency) and the case for migrating to a maintained fork. It intentionally
> does NOT prescribe a step-by-step migration plan — that belongs in a future
> `/sdd plan` once the decision to migrate is confirmed and the API diff is known.

## 1. Overview

### Problem Statement

mcpls's workspace `Cargo.toml` pins `lsp-types = "0.97"` (resolved `0.97.0` in
`Cargo.lock`), sourced from [gluon-lang/lsp-types](https://github.com/gluon-lang/lsp-types)
on crates.io. This crate underlies mcpls's entire LSP type modeling: the
critical position-encoding path (`crates/mcpls-core/src/bridge/encoding.rs`)
and all LSP request/response types across `crates/mcpls-core/src/lsp/` and
`crates/mcpls-core/src/bridge/`.

Verified via `gh api repos/gluon-lang/lsp-types`:
- Repository is not archived, but shows no recent maintenance activity
- Last commit: `2024-06-04T12:38:24Z` (over 2 years stale as of 2026-08-05)
- 46 open issues, no indication of active triage

The Rust LSP ecosystem has already moved on. `tower-lsp-community/tower-lsp-server`
— itself listed as mcpls's own reference project in
`.claude/rules/continuous-improvement.md` — switched its LSP types dependency
in release [v0.23.0](https://github.com/tower-lsp-community/tower-lsp-server/releases/tag/v0.23.0)
(published 2025-12-07) from `gluon-lang/lsp-types` to
[tower-lsp-community/ls-types](https://github.com/tower-lsp-community/ls-types),
a fork created 2025-06-04. Their release notes state verbatim:

> [!quote]
> "Change the LSP specification types library from `gluon-lang/lsp-types`
> (which was unmaintained) to `tower-lsp-community/ls-types` (our fork)."

`ls-types`'s README states it supports LSP 3.17 stable, plus LSP 3.18 draft
features behind a `proposed` feature flag. Migrating would therefore also open
a path to LSP 3.18 feature adoption (inline completions, folding range
refresh, multi-range formatting, nullable `SignatureHelp.activeParameter`,
workspace edit snippets, etc.) — features `gluon-lang/lsp-types` is very
unlikely to ever receive given its inactivity.

This is a supply-chain health / dependency-risk finding, not an active
security advisory: `cargo deny check advisories` reported clean, and no
RUSTSEC entry exists yet for `lsp-types`.

### Goal

mcpls's core LSP-type dependency tracks an actively maintained upstream, so
that LSP 3.18 spec changes, bugfixes, and typo/documentation corrections
continue to land without mcpls having to vendor or patch a dead dependency
itself.

### Out of Scope

> [!danger] Explicit exclusions
> - The actual migration implementation (code changes, `Cargo.toml` edit,
>   `Cargo.lock` update) — this spec captures the *decision case*, not the
>   *how*. A follow-up `/sdd plan` should own the migration mechanics.
> - A full symbol-by-symbol API diff between `lsp-types 0.97` and `ls-types`
>   current release — not yet performed (see Open Questions).
> - Adoption of LSP 3.18 `proposed`-flagged features themselves — that is a
>   separate feature decision gated on this migration, not part of it.
> - Evaluating alternative LSP-types crates beyond `ls-types` (e.g. writing
>   an in-house minimal type set) — `ls-types` is the ecosystem's de facto
>   successor and the only fork evaluated here.

## 2. User Stories

### US-001: Maintainer avoids stale-dependency risk
AS A mcpls maintainer
I WANT the project's core LSP type dependency to come from an actively
maintained upstream
SO THAT critical-path code (position encoding, request/response modeling)
is not built on a crate that will never receive further fixes, spec
corrections, or security patches

**Acceptance criteria:**
```
GIVEN a future LSP 3.18 spec correction or bugfix lands in ls-types
WHEN mcpls depends on ls-types instead of gluon-lang/lsp-types
THEN mcpls can pull in that fix via a routine dependency bump

GIVEN gluon-lang/lsp-types remains uncommitted-to for another year
WHEN this finding is reviewed at that time
THEN the migration decision documented here is either already resolved
     or explicitly re-affirmed as still pending
```

### US-002: Contributor evaluates migration feasibility before committing to it
AS A mcpls contributor considering the migration
I WANT a documented API-compatibility risk assessment (not just "it's a fork
so it's fine")
SO THAT the migration is not scheduled as a "trivial drop-in swap" without
verifying actual surface compatibility with mcpls's usage of `lsp-types`
across `crates/mcpls-core/src/lsp/` and `crates/mcpls-core/src/bridge/`

**Acceptance criteria:**
```
GIVEN this spec is read by someone about to start the migration
WHEN they check the Non-Functional Requirements and Open Questions sections
THEN they find an explicit call to diff the two crates' public APIs before
     writing any migration code, rather than assuming zero breakage
```

### US-003: End user benefits from eventual LSP 3.18 support
AS A user of an AI client that talks to mcpls
I WANT mcpls to be able to eventually support newer LSP 3.18 capabilities
(inline completions, folding range refresh, multi-range formatting, nullable
`SignatureHelp.activeParameter`, workspace edit snippets)
SO THAT mcpls does not fall permanently behind the LSP servers it bridges to
(e.g. rust-analyzer, pyright) as they adopt newer protocol features

**Acceptance criteria:**
```
GIVEN mcpls has migrated to ls-types
WHEN a future feature spec proposes adopting an LSP 3.18 capability
THEN the underlying types already exist behind ls-types's `proposed` feature
     flag, removing "no available Rust types" as a blocker
```

## 3. Functional Requirements

Use EARS notation. Prefix with FR-NNN. Since this is a research finding
rather than a feature under active development, these requirements describe
the target state the project SHOULD move toward, not code to write today.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHOULD depend on an actively maintained LSP-types crate (commits and issue triage within the last 12 months) rather than one with 2+ years of upstream inactivity | should |
| FR-002 | WHEN the migration from `lsp-types` to `ls-types` is undertaken THE SYSTEM SHALL preserve all existing LSP request/response behavior with no regression in `crates/mcpls-core/src/bridge/encoding.rs` position-encoding logic | must |
| FR-003 | WHEN the migration from `lsp-types` to `ls-types` is undertaken THE SYSTEM SHALL pass the full existing test suite (`cargo nextest run --workspace --all-features`) without behavioral changes attributable to the type-library swap alone | must |
| FR-004 | WHERE `ls-types` exposes LSP 3.18 `proposed` features behind a feature flag THE SYSTEM SHALL NOT enable that feature flag as part of this migration — 3.18 feature adoption is a separate decision | must |
| FR-005 | THE SYSTEM SHOULD re-evaluate this finding periodically (e.g. each continuous-improvement research cycle) until either the migration is completed or a documented decision is made to stay on `gluon-lang/lsp-types` | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Compatibility | `ls-types` is a fork of `gluon-lang/lsp-types`, so its public API surface is expected to be near drop-in, but this has **not** been verified for mcpls's specific usage — a symbol-level diff of types/traits consumed in `crates/mcpls-core/src/lsp/` and `crates/mcpls-core/src/bridge/` must be performed before migration work starts |
| NFR-002 | Maintainability | Migrating to a crate with active issue triage reduces the maintenance burden of working around unfixed upstream bugs (e.g. any of the 46 open `gluon-lang/lsp-types` issues that affect mcpls) |
| NFR-003 | Risk (supply chain) | This is not a security advisory (`cargo deny check advisories` is clean, no RUSTSEC entry exists) — the risk is *future* unpatched issues, not a known current vulnerability. Migration urgency should be assessed as P2 (suboptimal, not broken) accordingly |
| NFR-004 | Extensibility | Adopting `ls-types` is a prerequisite enabler for future LSP 3.18 feature work, not a requirement to adopt those features immediately |
| NFR-005 | Verification | Before switching, `cargo tree` and a build against `ls-types` should confirm no transitive version conflicts with other workspace dependencies (e.g. `rmcp`, `tower-lsp`-adjacent crates if any) |

## 5. Data Model

No new domain entities — this finding concerns a dependency swap of shared
type definitions used throughout the bridge layer, not new business data.

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| `lsp-types` crate (current) | Upstream source of all LSP protocol types used by mcpls | v0.97.0, from `gluon-lang/lsp-types`, last commit 2024-06-04 |
| `ls-types` crate (proposed) | Maintained fork intended as replacement | Created 2025-06-04 by `tower-lsp-community`; supports LSP 3.17 stable + LSP 3.18 draft behind `proposed` feature flag |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `ls-types` public API diverges from `lsp-types` in a type/trait mcpls depends on | Migration plan (future `/sdd plan`) must enumerate and resolve each divergence before merge — not discovered mid-migration |
| `ls-types` version resolution conflicts with another workspace dependency | Caught by `cargo tree` / `cargo build` during migration; not expected to occur per NFR-005 but must be checked, not assumed away |
| `gluon-lang/lsp-types` resumes active maintenance before migration starts | Re-evaluate whether migration is still warranted; document the reversal in this spec's Open Questions rather than proceeding on stale premises |
| `ls-types` itself becomes unmaintained in the future | Out of scope for this finding, but the periodic re-evaluation in FR-005 would surface it |

## 7. Success Criteria

Since this is a research spec, success criteria describe the research
outcome, not implementation completion.

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Decision recorded: migrate, defer, or reject | Documented in this spec's status or a follow-up decision note before the next continuous-improvement cycle closes this finding |
| SC-002 | If migration is approved, a `/sdd plan` exists covering the API diff and concrete migration steps | Plan created and linked from this spec before any `Cargo.toml` change is made |
| SC-003 | No source code, `Cargo.toml`, or `Cargo.lock` changes result from this spec alone | Verified — this spec is research-only by design |

## 8. Agent Boundaries

### Always (without asking)
- Treat this spec as research/documentation only — do not modify `Cargo.toml`, `Cargo.lock`, or any source file as part of fulfilling this spec
- Re-check `gluon-lang/lsp-types` activity (`gh api repos/gluon-lang/lsp-types --jq '.pushed_at,.open_issues_count'`) if this finding is revisited in a future cycle, to confirm the premise still holds

### Ask First
- Starting the actual migration implementation (this requires a separate `/sdd plan` and explicit maintainer approval, since it touches the critical position-encoding path)
- Enabling `ls-types`'s `proposed` feature flag for LSP 3.18 types (separate decision from the migration itself, per FR-004)

### Never
- Swap the dependency in `Cargo.toml` without first producing the API-compatibility diff called for in NFR-001
- Treat "it's a fork" as sufficient justification to skip verification — forks diverge

## 9. Open Questions

- [NEEDS CLARIFICATION: Exact API diff between `lsp-types 0.97.0` and the current `ls-types` release has not been performed. Needs a symbol-level comparison (types, trait impls, feature flags) scoped to what mcpls actually imports in `crates/mcpls-core/src/lsp/` and `crates/mcpls-core/src/bridge/` before any migration plan is written.]
- [NEEDS CLARIFICATION: Which `ls-types` version/tag should mcpls target? Not yet pinned down — needs checking crates.io or the `tower-lsp-community/ls-types` repo for its latest published release compatible with LSP 3.17 stable.]
- [NEEDS CLARIFICATION: Does `ls-types` publish to crates.io under a different crate name (e.g. `ls-types`) requiring a rename of the `lsp_types::` import path throughout mcpls, or does it re-export under the same `lsp_types` namespace for easier swapping? This directly affects migration blast radius across `crates/mcpls-core/src/lsp/` and `crates/mcpls-core/src/bridge/`.]
- [NEEDS CLARIFICATION: Should this migration be bundled with any other dependency-health cleanup in the same PR, or land as an isolated, easily-revertible commit given it touches the critical position-encoding path? Recommend isolated, given the project's emphasis on graceful degradation and the sensitivity of `bridge/encoding.rs`.]

## 10. See Also

- [[constitution]] — project principles (not yet created for this project)
- [[MOC-specs]] — all specifications
- [gluon-lang/lsp-types](https://github.com/gluon-lang/lsp-types) — current upstream dependency, unmaintained since 2024-06-04
- [tower-lsp-community/ls-types](https://github.com/tower-lsp-community/ls-types) — proposed replacement, maintained fork
- [tower-lsp-server v0.23.0 release notes](https://github.com/tower-lsp-community/tower-lsp-server/releases/tag/v0.23.0) — precedent for this exact migration in a reference project
- `crates/mcpls-core/src/bridge/encoding.rs` — critical position-encoding path most sensitive to any type-level divergence
- `crates/mcpls-core/src/lsp/` — LSP client, primary consumer of `lsp-types`
- `crates/mcpls-core/src/bridge/` — MCP-LSP translation layer, secondary consumer of `lsp-types`
