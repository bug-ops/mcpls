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
> **Priority**: P3 (bumped from P4 on 2026-09-21 per SC-003 — see Re-assessment callout and Success Criteria below)
> **Related issues**: #116 (P3, LSP 3.17-era tools — resolved by #124, see note below), #290 (P2, negotiated position encoding not consumed — resolved, see Resolution below)

> [!success] #290 resolved
> Issue #290 (negotiated LSP position encoding not consumed) is now closed, in two parts:
> commit `bc95b89` (PR #289) wired configured `workspace.position_encodings` into the LSP
> `initialize` handshake, and commit `81fd7d3` (PR #291) made `mcp_to_lsp_position`/
> `lsp_to_mcp_position` actually consume the *negotiated* `PositionEncodingKind` (rather than
> assuming a fixed encoding) when converting positions, including an async
> `DocumentTracker`-backed line-text lookup and a `character_to_byte_offset`/
> `byte_offset_to_character` char-boundary guard. Both commits carry `BREAKING CHANGE` notes
> (`ServerConfig::validate()` strictness / new `ServerInitConfig::position_encodings` field for
> #289; several `bridge::translator` methods becoming `async` for #291). This did not change this
> spec's own scope at the time it was written — noted here only because #290 was listed as a
> related issue. (This spec's priority has since moved to P3; see the Re-assessment callout below
> — for a different reason, unrelated to #290.)

> [!important] Re-assessment (2026-09-21) — SC-003 triggered
> Two facts verified today trigger this spec's own SC-003 re-assessment condition:
> 1. **LSP 3.18 has finalized.** The [LSP 3.18 specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/)
>    page now shows "3.18 (Current)" in its version nav, with "3.19 (Upcoming)" as the new draft.
>    This supersedes LSP 3.17, which `.claude/rules/continuous-improvement.md` still cites as the
>    project's "authoritative protocol reference" — flagged here as stale but intentionally left
>    unedited; updating that rules file is a separate followup.
> 2. **This spec's original `ls-types` premise (Section 1, "Out of Scope", FR-006, "See Also")
>    was already superseded before today**, by [[lsp/003-lsp-types-unmaintained-migration/spec|spec lsp/003]]'s
>    own resolution: `ls-types` turned out to be archived/superseded upstream, and mcpls migrated
>    (PR #375, closing #297) to **`gen-lsp-types` 0.11.0** instead — a different, actively maintained,
>    LSP-3.18-metamodel-generated fork (`Cargo.toml`: `lsp-types = { package = "gen-lsp-types", version = "=0.11.0" }`).
>    Verified directly against the vendored source
>    (`gen-lsp-types-0.11.0/src/generated/{requests,structures}.rs`): `InlineCompletionRequest`,
>    `DocumentRangesFormattingRequest`, `TextDocumentContentRequest`/`TextDocumentContentRefreshRequest`,
>    and a nullable `active_parameter` on the signature-help structures are all present
>    **unconditionally — no `proposed` feature flag gating them**. The type-level prerequisite this
>    spec assumed for FR-001, FR-002, FR-004, and FR-006 is therefore already met; what remains for
>    those items is MCP tool design and wiring, not a dependency migration. FR-006 specifically is
>    already *wired*, not just type-available — see the FR-006 row below.
>
> Reference-project check (the other half of SC-003): `isaacphi/mcp-language-server` and
> `Tritlo/lsp-mcp` (this spec's two originally tracked reference projects) show no 3.18-feature
> adoption in recent activity; `zeenix/rust-analyzer-mcp`'s commits since 2026-09-01 are
> workspace-symbol/hierarchical-symbols/wait-for-index work, unrelated to 3.18. Only the
> "spec finalizes" half of SC-003 fired, not the "reference project adopts" half.
>
> **Net effect**: priority bumped P4 → P3 (see Success Criteria, SC-003). `status` stays `draft` —
> this project's spec-status vocabulary (`draft` / `implemented` / `superseded`, per other specs
> in this block) has no intermediate "re-reviewed but not promoted" state, and no FR has been
> promoted into an implementation spec (that remains an "Ask First" action per Section 8). The
> stale `ls-types`/`proposed`-flag references below are corrected in place rather than rewritten
> wholesale, consistent with this project's convention of preserving research history and marking
> what changed (see [[lsp/003-lsp-types-unmaintained-migration/spec|spec lsp/003]]'s own Resolution
> callout for precedent).

## 1. Overview

### Problem Statement

mcpls's reference-projects list in `.claude/rules/continuous-improvement.md` still cites LSP 3.17
as the project's authoritative protocol reference — stale as of the finalization noted in the
Re-assessment callout above, flagged here but left unedited (separate followup). mcpls already
tracks a known gap of 3.17-era tools in [[lsp/002-lsp317-missing-tools/spec|spec lsp/002]] / issue
#116, resolved by PR #124 (`get_signature_help`, `go_to_implementation`, `go_to_type_definition`,
`get_inlay_hints`, `prepare_type_hierarchy` are all implemented) — plus the separate issue #290
(P2) about negotiated LSP position encoding, also resolved (see Resolution callout above).

LSP 3.18 has finalized at the
[LSP 3.18 specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/)
(verified 2026-09-21: version nav shows "3.18 (Current)", "3.19 (Upcoming)" as the new draft),
with features tagged `@since 3.18.0`. Verified from the spec document, the notable new/changed
capabilities are:

| 3.18 capability | Nature of change | Relation to existing mcpls surface |
|---|---|---|
| Inline Completions | New language feature | No equivalent MCP tool concept exists yet |
| Dynamic Text Document Content refresh | Server-initiated refresh | New notification-refresh pattern |
| Folding Range refresh support | Server-initiated client refresh | Similar push-model to diagnostics, which `bridge/notifications.rs` already handles |
| Multiple Range Formatting | Extends formatting request | Extends the existing single-range format tool |
| WorkspaceEdit snippet support + metadata | Extends edit payload | Extends whatever tool surfaces `workspace/applyEdit`-style edits |
| SignatureHelp/SignatureInformation `activeParameter` nullable | Type-signature change | Already implemented in `get_signature_help` — see FR-006 below |
| Code Action `kind` documentation, Command tooltip support | Metadata/documentation addition | Extends existing code-action tooling, if any |
| CompletionList `applyKind` property | New completion-list property | Extends completion tooling, if any |
| Relative pattern support in document filters / notebook filters | Filter-matching extension | Affects document/notebook filter matching internals, not a user-facing tool |

Neither of mcpls's two originally tracked reference projects — isaacphi/mcp-language-server or
Tritlo/lsp-mcp — implements any 3.18 feature yet (verified via
`gh api repos/isaacphi/mcp-language-server/commits` and `gh api repos/Tritlo/lsp-mcp/commits`, most
recently re-verified 2026-09-21; most recent commits from mid-2025, predating 3.18). A third
project tracked in later research cycles, `zeenix/rust-analyzer-mcp`, was also re-checked
2026-09-21: its commits since 2026-09-01 are workspace-symbol/hierarchical-symbols/wait-for-index
work, unrelated to 3.18. This absence of competitive pressure is why most items here remain P4
even now that the spec has finalized — see the Re-assessment callout above for why the spec as a
whole is now P3.

Separately, [[lsp/003-lsp-types-unmaintained-migration/spec|spec lsp/003]] (implemented, PR #375)
already migrated mcpls's `lsp-types` dependency off the unmaintained `gluon-lang/lsp-types` — not
to `ls-types` as originally planned (that fork turned out to be archived/superseded upstream), but
to **`gen-lsp-types` 0.11.0**, an actively maintained, LSP-3.18-metamodel-generated fork. Its
generated types already expose `InlineCompletionRequest`, `DocumentRangesFormattingRequest`,
`TextDocumentContentRequest`/`TextDocumentContentRefreshRequest`, and a nullable `active_parameter`
on the signature-help structures unconditionally, with no `proposed`-style feature flag. The
type-level prerequisite for FR-001, FR-002, FR-004, and FR-006 is therefore already satisfied — see
the Re-assessment callout above. What remains undone for those items is MCP tool design and
wiring (still out of scope for this research spec), not a dependency migration.

### Goal

Establish a tracked, reviewable watch-item for LSP 3.18 capabilities so that a future
continuous-improvement cycle can promote a specific capability out of this research spec into a
proper implementation spec (following [[lsp/002-lsp317-missing-tools/spec|spec lsp/002]]'s pattern) once
either (a) the LSP 3.18 spec finalizes — **satisfied as of 2026-09-21, see Re-assessment callout
above** — or (b) a reference project (isaacphi/mcp-language-server, Tritlo/lsp-mcp) or real user
demand creates competitive/urgency pressure — **not yet satisfied**. Promotion of any individual FR
still requires its own gating condition (per-FR, Section 3) and remains an "Ask First" action
(Section 8), not automatic on spec finalization alone.

### Out of Scope

- Implementing any LSP 3.18 capability now — even though the spec has finalized and the type-level
  prerequisite is met for several items (see Re-assessment callout above), no MCP tool design
  exists for most of these (e.g. Inline Completions has no established MCP tool shape in any
  reference project) and no adoption/demand signal has appeared
- Any further `lsp-types` dependency work — [[lsp/003-lsp-types-unmaintained-migration/spec|spec lsp/003]]
  already migrated it (to `gen-lsp-types`, not `ls-types` as originally planned) and is implemented;
  this spec only notes that migration's outcome as context
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
> [!success] Already realized
> Unlike the other user stories in this section, US-003 is **not speculative** as of 2026-09-21.
> `get_signature_help` is implemented (issue #116, PR #124), and `active_parameter:
> Option<u32>` in `crates/mcpls-core/src/bridge/translator/assist.rs` already converts
> `gen-lsp-types`'s nullable `ActiveParameter::{Int, Null}` into `Some(n)`/`None` rather than
> defaulting to `0`. See FR-006 below.

AS A user of `get_signature_help`
I WANT the `activeParameter` field to be nullable rather than defaulting to `0`
SO THAT I can tell "cursor is past all known parameters" apart from "cursor is on the first
parameter"

### US-004: AI agent reacts to server-pushed folding-range invalidation
AS A future user of a folding-range tool
I WANT the server's folding-range refresh notification to invalidate any cached result
SO THAT stale folding ranges aren't returned after the underlying document changes — this
follows the same push-then-poll caching pattern `bridge/notifications.rs` already implements
for diagnostics

## 3. Functional Requirements

> [!warning] Speculative — candidates, not commitments
> Every requirement below is a **candidate**, not a commitment. LSP 3.18 has now finalized (see
> Re-assessment callout, Section 1), so the "spec finalization" half of each gating condition is
> satisfied where noted; what remains for most items is a reference-project/user-demand adoption
> signal and/or MCP tool design work — neither of which exists yet. None should be implemented
> without first satisfying the full gating condition and going through Section 8's "Ask First"
> promotion step; text uses EARS notation only to keep the candidate requirement well-formed for a
> future promotion into an implementation spec.

| ID | Candidate Requirement (speculative) | Gating condition |
|----|------------|----------|
| FR-001 | WHEN LSP 3.18 finalizes Inline Completions AND a reference project or user demand signal appears THE SYSTEM MAY expose an inline-completion MCP tool | Spec finalization **(met, 2026-09-21)** + adoption signal (**not met**) |
| FR-002 | WHEN LSP 3.18 finalizes Dynamic Text Document Content refresh THE SYSTEM MAY invalidate cached document content on server-pushed refresh, following the existing notification-caching pattern in `bridge/notifications.rs` | Spec finalization **(met, 2026-09-21; MCP tool design/wiring still undone)** |
| FR-003 | WHEN LSP 3.18 finalizes Folding Range refresh support AND mcpls exposes a folding-range tool THE SYSTEM MAY invalidate cached folding ranges on server-pushed refresh | Spec finalization + folding-range tool existing |
| FR-004 | WHEN LSP 3.18 finalizes Multiple Range Formatting THE SYSTEM MAY extend the existing single-range format tool to accept multiple ranges in one request | Spec finalization **(met, 2026-09-21; MCP tool design/wiring still undone)** |
| FR-005 | WHEN LSP 3.18 finalizes WorkspaceEdit snippet support and metadata THE SYSTEM MAY surface snippet placeholders and edit metadata through whatever tool applies workspace edits | Spec finalization **(met, 2026-09-21)**; note `rename_symbol` currently drops LSP-3.18 snippet-shaped edits rather than surfacing them (see [[lsp/003-lsp-types-unmaintained-migration/spec\|spec lsp/003]] Resolution) |
| FR-006 | WHEN the `lsp-types` dependency exposes a nullable `activeParameter` on `SignatureHelp`/`SignatureInformation` THE SYSTEM MAY propagate `null` (rather than defaulting to `0`) through `get_signature_help` | **Already satisfied and already implemented** — `gen-lsp-types` migration ([[lsp/003-lsp-types-unmaintained-migration/spec\|spec lsp/003]], implemented #375) + issue #116 resolution (#124) both landed; `bridge/translator/assist.rs` already converts `ActiveParameter::{Int,Null}` to `Option<u32>`. No further work needed for this FR; see US-003 |
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
| SC-003 | This spec's priority is re-assessed if the LSP 3.18 spec finalizes or either reference project adopts a listed capability | **Triggered and satisfied, 2026-09-21**: LSP 3.18 finalized (version nav shows "3.18 (Current)"); the reference-project-adoption half did not fire (no adoption in `isaacphi/mcp-language-server`, `Tritlo/lsp-mcp`, or `zeenix/rust-analyzer-mcp`). Priority bumped P4 → P3 in this cycle (see Re-assessment callout, Section 1). Recommend P3 over P2: the type-level blocker is gone and FR-006 is fully implemented, but the remaining FRs still have no MCP tool design and no reference-project/user-demand adoption signal — the two things that would justify P2. |

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
  `draft`, even though LSP 3.18 itself has now finalized (finalization only satisfies this spec's
  own SC-003 re-assessment trigger, not the "promote to implementation spec" bar in Section 8's
  Ask First list)
- Enable any `proposed`/pre-stabilization feature flag on the `lsp-types`/`gen-lsp-types`
  dependency to consume not-yet-finalized upstream types — not currently applicable (`gen-lsp-types`
  0.11.0 has no such flag and 3.18 has finalized), but kept as a standing boundary for a future
  3.19-draft cycle

## 9. Open Questions

- [RESOLVED 2026-09-21: LSP 3.18 has finalized. Requirements FR-001–FR-009 are no longer gated on
  draft volatility; each retains its own remaining gating condition per Section 3 (mostly
  reference-project/user-demand adoption, plus MCP tool design work).]
- [NEEDS CLARIFICATION: Should this spec be re-filed (new number) each time the LSP working group
  cuts a new draft revision (e.g. the now-open 3.19 draft), or should it be edited in place with a
  changelog note at the top? This 2026-09-21 update followed the "edit in place" precedent
  established by [[lsp/003-lsp-types-unmaintained-migration/spec|spec lsp/003]]'s own Resolution
  callout, consistent with how issue #116 is amended rather than re-filed. Recommend keeping that
  convention for any future 3.19-driven update to this spec.]
- [RESOLVED 2026-09-21: The `ls-types` migration question is moot — mcpls migrated to
  `gen-lsp-types` instead (spec lsp/003, implemented #375), which ships 3.18 types unconditionally
  with no `proposed`-style flag to decide on.]

## 10. See Also

- [LSP 3.18 specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/) — finalized as of 2026-09-21 ("3.18 (Current)"); "3.19 (Upcoming)" is the new draft
- [gen-lsp-types (crates.io)](https://crates.io/crates/gen-lsp-types) — the actively maintained, LSP-3.18-metamodel-generated fork mcpls actually migrated to (not `ls-types`, which turned out to be archived/superseded upstream)
- [[lsp/002-lsp317-missing-tools/spec|spec lsp/002]] / issue #116 — LSP 3.17-era tool gap, implemented via PR #124 (`get_signature_help`, `go_to_implementation`, `go_to_type_definition`, `get_inlay_hints`, `prepare_type_hierarchy`)
- issue #290 — negotiated LSP position encoding not consumed by position conversion (P2, resolved by PR #289/#291)
- [[lsp/003-lsp-types-unmaintained-migration/spec|spec lsp/003]] — companion finding on the unmaintained `lsp-types` dependency; implemented (PR #375), migrated to `gen-lsp-types`, which already satisfies this spec's type-level prerequisites for FR-001, FR-002, FR-004, and FR-006
- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
