---
aliases:
  - LSP 3.18 draft gaps
  - LSP 3.18 watch-item
tags:
  - sdd
  - spec
  - research
  - competitive-parity
  - lsp
created: 2026-08-05
status: draft
related:
  - "[[constitution]]"
---

# Feature: Track LSP 3.18 (Draft) Capabilities Against mcpls's MCP Tool Surface

> [!info] Metadata
> **Author**: rust-researcher (filed from competitive-parity research cycle)
> **Type**: research / competitive-parity
> **Priority**: P4
> **Related issues**: #116 (P3, unimplemented LSP 3.17-era tools), #290 (P2, negotiated position encoding not consumed — resolved, see Resolution below)

> [!success] #290 resolved
> Issue #290 (negotiated LSP position encoding not consumed) is now closed, in two parts:
> commit `bc95b89` (PR #289) wired configured `workspace.position_encodings` into the LSP
> `initialize` handshake, and commit `81fd7d3` (PR #291) made `mcp_to_lsp_position`/
> `lsp_to_mcp_position` actually consume the *negotiated* `PositionEncodingKind` (rather than
> assuming a fixed encoding) when converting positions, including an async
> `DocumentTracker`-backed line-text lookup and a `character_to_byte_offset`/
> `byte_offset_to_character` char-boundary guard. Both commits carry `BREAKING CHANGE` notes
> (`ServerConfig::validate()` strictness / new `ServerInitConfig::position_encodings` field for
> #289; several `bridge::translator` methods becoming `async` for #291). This does not change
> this spec's own scope (LSP 3.18 draft tracking remains speculative/P4) — noted here only
> because #290 was listed as a related issue.

> [!question] Draft-spec volatility
> [NEEDS CLARIFICATION: LSP 3.18 is still a draft; requirements may change before finalization]

## 1. Overview

### Problem Statement

mcpls currently targets LSP 3.17 (per the reference-projects list in
`.claude/rules/continuous-improvement.md`) and already tracks a known gap of unimplemented
3.17-era tools in [[lsp/002-lsp317-missing-tools/spec|spec lsp/002]] / issue #116 (P3) —
`get_signature_help`, `go_to_implementation`, `go_to_type_definition`, `get_inlay_hints`,
`prepare_type_hierarchy` — plus a separate open issue #290 (P2) about the negotiated LSP
position encoding not being consumed by position conversion.

LSP 3.18 is now under active development at the
[LSP 3.18 draft specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/),
with features tagged `@since 3.18.0`. Verified from the spec document, the notable new/changed
capabilities are:

| Draft capability | Nature of change | Relation to existing mcpls surface |
|---|---|---|
| Inline Completions | New language feature | No equivalent MCP tool concept exists yet |
| Dynamic Text Document Content refresh | Server-initiated refresh | New notification-refresh pattern |
| Folding Range refresh support | Server-initiated client refresh | Similar push-model to diagnostics, which `bridge/notifications.rs` already handles |
| Multiple Range Formatting | Extends formatting request | Extends the existing single-range format tool |
| WorkspaceEdit snippet support + metadata | Extends edit payload | Extends whatever tool surfaces `workspace/applyEdit`-style edits |
| SignatureHelp/SignatureInformation `activeParameter` nullable | Type-signature change | Affects the not-yet-implemented `get_signature_help` (tracked in issue #116) |
| Code Action `kind` documentation, Command tooltip support | Metadata/documentation addition | Extends existing code-action tooling, if any |
| CompletionList `applyKind` property | New completion-list property | Extends completion tooling, if any |
| Relative pattern support in document filters / notebook filters | Filter-matching extension | Affects document/notebook filter matching internals, not a user-facing tool |

Neither of mcpls's tracked reference projects — isaacphi/mcp-language-server or Tritlo/lsp-mcp —
implements any 3.18 draft feature yet (verified via `gh api repos/isaacphi/mcp-language-server/commits`
and `gh api repos/Tritlo/lsp-mcp/commits`; most recent commits from mid-2025, predating the 3.18
draft). This absence of competitive pressure, combined with the spec still being a draft, is why
most items here are P4 (cosmetic/niche, no reference-project adoption signal, no user demand
signal) rather than P2/P3.

Separately, the `ls-types` crate (tower-lsp-community's maintained fork of `lsp-types`, tracked
in [[lsp/003-lsp-types-unmaintained-migration/spec|spec lsp/003]]) already supports LSP 3.18 draft
features behind a `proposed` feature flag. Migrating to that dependency is therefore a
prerequisite for adopting any item in this list, since the current `lsp-types` dependency has no
3.18 types at all.

### Goal

Establish a tracked, reviewable watch-item for LSP 3.18 draft capabilities so that a future
continuous-improvement cycle can promote a specific capability out of this research spec into a
proper implementation spec (following [[lsp/002-lsp317-missing-tools/spec|spec lsp/002]]'s pattern) once
either (a) the LSP 3.18 spec finalizes, or (b) a reference project (isaacphi/mcp-language-server,
Tritlo/lsp-mcp) or real user demand creates competitive/urgency pressure.

### Out of Scope

- Implementing any LSP 3.18 capability now — the spec is a draft and no MCP tool design exists
  for most of these (e.g. Inline Completions has no established MCP tool shape in any reference
  project)
- Migrating the `lsp-types` → `ls-types` dependency (tracked separately in
  [[lsp/003-lsp-types-unmaintained-migration/spec|spec lsp/003]]); this spec only notes it as a
  prerequisite
- Resolving issue #116 (3.17-era gaps) or issue #290 (position encoding) — those are separate,
  already-tracked, higher-priority items
- Committing to any MCP tool naming, request/response shape, or API design for these
  capabilities — that is plan-phase work that should only happen once an item is promoted out of
  research status

## 2. User Stories

> [!note] Speculative, low priority
> These stories describe future value **conditional on** the LSP 3.18 draft finalizing and/or a
> corresponding MCP tool being designed. None are actionable today.

### US-001: AI agent gets real-time inline completions
AS A future user of an mcpls-exposed inline-completion tool
I WANT AI-generated inline completions surfaced the same way an LSP client would show them
SO THAT the AI agent gets ghost-text-style suggestions without polling a separate completion
tool, once Inline Completions lands in a finalized LSP spec and gains reference-project adoption

### US-002: AI agent formats a discontinuous selection in one call
AS A future user of mcpls's formatting tool
I WANT multiple, non-contiguous ranges formatted in a single request
SO THAT I don't need N sequential format calls for N disjoint edited regions, once Multiple
Range Formatting is available from the underlying LSP server

### US-003: AI agent distinguishes "no active parameter" from "parameter zero"
AS A future user of `get_signature_help` (tracked in issue #116, not yet implemented)
I WANT the `activeParameter` field to be nullable rather than defaulting to `0`
SO THAT I can tell "cursor is past all known parameters" apart from "cursor is on the first
parameter" — this only matters once `get_signature_help` itself is implemented and the
underlying `lsp-types`/`ls-types` dependency exposes the 3.18 nullable-`activeParameter` shape

### US-004: AI agent reacts to server-pushed folding-range invalidation
AS A future user of a folding-range tool
I WANT the server's folding-range refresh notification to invalidate any cached result
SO THAT stale folding ranges aren't returned after the underlying document changes — this
follows the same push-then-poll caching pattern `bridge/notifications.rs` already implements
for diagnostics

## 3. Functional Requirements

> [!warning] Speculative and draft-spec-dependent
> Every requirement below is a **candidate**, not a commitment. Each is gated on the LSP 3.18
> draft finalizing and/or the referenced prerequisite work landing. None should be implemented
> against the draft spec as-is; text uses EARS notation only to keep the candidate requirement
> well-formed for a future promotion into an implementation spec.

| ID | Candidate Requirement (speculative) | Gating condition |
|----|------------|----------|
| FR-001 | WHEN LSP 3.18 finalizes Inline Completions AND a reference project or user demand signal appears THE SYSTEM MAY expose an inline-completion MCP tool | Spec finalization + adoption signal |
| FR-002 | WHEN LSP 3.18 finalizes Dynamic Text Document Content refresh THE SYSTEM MAY invalidate cached document content on server-pushed refresh, following the existing notification-caching pattern in `bridge/notifications.rs` | Spec finalization |
| FR-003 | WHEN LSP 3.18 finalizes Folding Range refresh support AND mcpls exposes a folding-range tool THE SYSTEM MAY invalidate cached folding ranges on server-pushed refresh | Spec finalization + folding-range tool existing |
| FR-004 | WHEN LSP 3.18 finalizes Multiple Range Formatting THE SYSTEM MAY extend the existing single-range format tool to accept multiple ranges in one request | Spec finalization |
| FR-005 | WHEN LSP 3.18 finalizes WorkspaceEdit snippet support and metadata THE SYSTEM MAY surface snippet placeholders and edit metadata through whatever tool applies workspace edits | Spec finalization |
| FR-006 | WHEN the `lsp-types`/`ls-types` dependency exposes a nullable `activeParameter` on `SignatureHelp`/`SignatureInformation` THE SYSTEM MAY propagate `null` (rather than defaulting to `0`) through `get_signature_help` once that tool is implemented (issue #116) | `ls-types` migration ([[lsp/003-lsp-types-unmaintained-migration/spec\|spec lsp/003]]) + issue #116 resolution |
| FR-007 | WHEN LSP 3.18 finalizes Code Action `kind` documentation and Command tooltip support THE SYSTEM MAY surface tooltip text through any future code-action tool | Spec finalization + code-action tool existing |
| FR-008 | WHEN LSP 3.18 finalizes the CompletionList `applyKind` property THE SYSTEM MAY surface it through any future completion tool | Spec finalization + completion tool existing |
| FR-009 | WHEN LSP 3.18 finalizes relative pattern support in document/notebook filters THE SYSTEM MAY use relative patterns internally for document-filter matching, if mcpls's server-discovery heuristics in `config/` are extended to consume LSP-native filters | Spec finalization + internal filter-matching redesign |

## 4. Non-Functional Requirements

> [!note] Not applicable at research stage
> No implementation is planned, so no performance, security, or accessibility targets apply yet.
> Once any FR above is promoted to an implementation spec, that spec must define its own NFRs
> (following the pattern in [[lsp/002-lsp317-missing-tools/spec|spec lsp/002]] and
> [[runtime/001-log-json-bool-env-parsing/spec|spec runtime/001]]).

## 5. Data Model

Not applicable — this is a tracking/research spec, not an implementation spec. No new domain
entities are introduced.

## 6. Edge Cases and Error Handling

Not applicable — no code changes result from this spec.

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | This spec is re-reviewed in each future competitive-parity research cycle | Reviewed at least once per cycle that scans LSP spec / reference-project activity |
| SC-002 | An FR is promoted to its own implementation spec | Promotion happens only when its gating condition (spec finalization and/or reference-project/user-demand signal) is met — not before |
| SC-003 | This spec's priority is re-assessed if the LSP 3.18 spec finalizes or either reference project adopts a listed capability | Priority bumped from P4 in the cycle immediately following the trigger event |

## 8. Agent Boundaries

### Always (without asking)
- Re-check this spec's premises (draft status, reference-project adoption) during future
  competitive-parity research cycles
- Keep this spec's FRs marked speculative until their gating condition is met

### Ask First
- Promoting any FR from this spec into a dedicated implementation spec (new `.local/specs/NNN-*`
  directory), even after a gating condition appears to be met

### Never
- Implement any capability listed in Functional Requirements while this spec's status remains
  `draft` and the LSP 3.18 spec remains a draft
- Add a dependency on `ls-types`'s `proposed` feature flag to consume 3.18 draft types before the
  spec finalizes — draft-flagged upstream types are not a stable contract to build against

## 9. Open Questions

- [NEEDS CLARIFICATION: LSP 3.18 is still a draft; requirements may change before finalization]
- [NEEDS CLARIFICATION: Should this spec be re-filed (new number) each time the LSP working group
  cuts a new draft revision, or should it be edited in place with a changelog note at the top?
  Recommend editing in place and relying on git history, consistent with how issue #116 is
  amended rather than re-filed as new tracked gaps are found.]
- [NEEDS CLARIFICATION: Once `ls-types` migration ([[lsp/003-lsp-types-unmaintained-migration/spec|spec lsp/003]])
  lands, should the `proposed`/3.18-draft feature flag be enabled at all before 3.18 finalizes, or
  only the stable 3.17 surface? Recommend keeping the `proposed` flag off until 3.18 finalizes, to
  avoid depending on an upstream draft-flagged, potentially-breaking API surface.]

## 10. See Also

- [LSP 3.18 draft specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/)
- [ls-types README (notes the 3.18 `proposed` feature flag)](https://github.com/tower-lsp-community/ls-types)
- [[lsp/002-lsp317-missing-tools/spec|spec lsp/002]] / issue #116 — existing tracked gap of unimplemented LSP 3.17-era tools (`get_signature_help`, `go_to_implementation`, `go_to_type_definition`, `get_inlay_hints`, `prepare_type_hierarchy`)
- issue #290 — negotiated LSP position encoding not consumed by position conversion (P2, open)
- [[lsp/003-lsp-types-unmaintained-migration/spec|spec lsp/003]] — companion finding on the unmaintained `lsp-types` dependency; prerequisite for adopting any 3.18 draft type
- [[constitution]] — project principles (not yet created for this project)
- [[MOC-specs]] — all specifications
