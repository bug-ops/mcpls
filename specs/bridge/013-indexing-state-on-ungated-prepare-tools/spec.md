---
aliases:
  - Indexing state on ungated tools
  - prepare_call_hierarchy empty mid-index
tags:
  - sdd
  - spec
  - enhancement
  - bridge
  - lsp
  - reliability
created: 2026-10-06
status: draft
related:
  - "[[constitution]]"
  - "[[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006-lsp-indexing-readiness-gate]]"
  - "[[bridge/012-indexing-gate-after-restart/spec|bridge/012-indexing-gate-after-restart]]"
  - "[[bridge/011-push-only-server-diagnostics/spec|bridge/011-push-only-server-diagnostics]]"
  - "[[mcp/005-tool-capability-discoverability/spec|mcp/005-tool-capability-discoverability]]"
---

# Feature: Tell the caller when an ungated tool answered while the server was still indexing

> [!info] Metadata
> **Type**: enhancement
> **Priority**: P3
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle, live-test finding
> **Issue**: #668

> [!abstract]
> `prepare_call_hierarchy` and `prepare_type_hierarchy` are deliberately not gated on indexing
> readiness (decision of #423). Mid-index they return an empty `items` list, and the caller cannot
> tell "no symbol here" from "the server is still loading". The diagnostics tools already solve the
> same problem with an `indexing_in_progress` flag. This spec decides how ungated tools whose answer
> depends on name resolution should disclose an in-progress index, and records the trade-off
> against the #423 decision.

## 1. Overview

### Problem Statement

The readiness gate ([[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]]) has two settings per
tool: `IndexingGate::Required` (wait while the routed server is `Loading`, or return the retryable
`WorkspaceIndexing` error) and `IndexingGate::NotRequired`. The doc comment on the gate records
that `prepare_call_hierarchy` is `NotRequired` as a scope decision of #423, not because its answer
is file-local: it performs position-based name resolution, like `get_definition` (which is
`Required`), and "mid-index it degrades to an empty prepare result rather than an explicit error".
`prepare_type_hierarchy` has the same setting. The incoming and outgoing calls that follow the
prepare step are `Required`.

**Reproduced live** (cold rust-analyzer start under CPU load, 3 of 3):

- `prepare_call_hierarchy` on a function name returned `{"items": []}` after 0.2 to 0.4 s.
- `get_hover` and `get_references` on the same position waited 44 s and 13 s and returned data.
- With no load, the same prepare call returns the item after about 2 s.

The caller therefore receives, for the same position and the same moment, a correct answer from
two tools and a plausible "nothing here" from the third. An agent that starts a call-graph walk with
`prepare_call_hierarchy` and gets an empty list concludes that the symbol has no hierarchy, and it
does not learn that a retry in a few seconds would work. The diagnostics tools (`get_diagnostics`,
`get_cached_diagnostics`) already carry `indexing_in_progress` for exactly this reason (#445), so
the project already accepts a flag as a valid way to disclose the state without blocking.

**Which tools are in question.** Ungated tools fall into three groups:

| Group | Tools | Why ungated | Name-resolution dependent |
|-------|-------|-------------|---------------------------|
| File-local analysis | `get_document_symbols`, selection ranges, folding ranges, format document, format range | The answer is valid mid-index | no |
| Prepare steps | `prepare_call_hierarchy`, `prepare_type_hierarchy` | #423 scope decision | yes |
| Resolution-assisted | `get_signature_help`, `get_document_highlights`, `get_inlay_hints` | No recorded decision | likely, `[NEEDS CLARIFICATION: audit]` |

`workspace_symbol_search` and `get_diagnostics` bypass the gate chokepoint by design; the latter
already has the flag.

### Goal

Every ungated tool whose answer depends on name resolution either discloses that the routed server
was still indexing when it answered, using the same structured signal the diagnostics tools use, or
is gated. The caller can tell an empty result caused by an unfinished index from a genuinely empty
result.

### Out of Scope

- Changing the gating of `get_diagnostics`, `get_cached_diagnostics` and `workspace_symbol_search`.
- Gating or flagging file-local tools (document symbols, folding, selection, formatting).
- The state after a restart or respawn ([[bridge/012-indexing-gate-after-restart/spec|bridge/012]]).
- Generic readiness detection for servers without a recognized signal (#422).
- Changing the 30 s bounded wait.
- Technical design: recorded in a plan after this spec is approved.

## 2. User Stories

### US-001: Distinguish "no symbol" from "still loading" on prepare

AS AN AI coding agent starting a call-hierarchy walk
I WANT `prepare_call_hierarchy` to tell me when it answered while the server was indexing
SO THAT an empty list does not end my walk when a retry would succeed.

**Acceptance criteria:**
```
GIVEN a routed server whose tracked indexing state is Loading
WHEN prepare_call_hierarchy is called on a function name and the server returns no items
THEN the response carries indexing_in_progress = true
  AND the caller can tell it apart from an empty result with indexing_in_progress = false
```

### US-002: Same signal for the type hierarchy prepare

AS AN AI coding agent
I WANT `prepare_type_hierarchy` to behave like `prepare_call_hierarchy`
SO THAT the two hierarchy tools are consistent.

**Acceptance criteria:**
```
GIVEN a server in the Loading state
WHEN prepare_type_hierarchy returns no items
THEN the response carries indexing_in_progress = true
```

### US-003: No regression on a ready server

AS A user whose server has finished indexing
I WANT unchanged latency and unchanged results
SO THAT the disclosure costs nothing in the common case.

**Acceptance criteria:**
```
GIVEN a server whose indexing state is Ready or Unknown
WHEN an affected tool is called
THEN the response carries indexing_in_progress = false (or omits it, as decided)
  AND latency and result are as before
```

### US-004: A reviewer can see the decision for each ungated tool

AS A maintainer
I WANT a recorded decision (flag, gate, or leave as is, with the reason) for each ungated tool
SO THAT the next tool added does not silently become a false-empty source.

**Acceptance criteria:**
```
GIVEN the set of tools registered at the time of the change
WHEN the gate settings are reviewed
THEN every ungated tool has a recorded reason (file-local, or discloses indexing state, or exempt by decision)
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE SYSTEM SHALL audit every tool with `IndexingGate::NotRequired` and classify it as file-local or name-resolution dependent, and SHALL record the classification | must |
| FR-002 | WHEN a name-resolution-dependent ungated tool answers while the routed server's tracked indexing state is `Loading` THE SYSTEM SHALL disclose that fact in its response through a structured field, or SHALL have waited under the gate | must |
| FR-003 | THE disclosure SHALL use the same field name and meaning as `indexing_in_progress` on the diagnostics tools: true if the routed server was indexing at some point during the read | must |
| FR-004 | WHEN a disclosure field is added THE SYSTEM SHALL add it to every affected tool in one change, so the hierarchy tools and the resolution-assisted tools do not diverge | must |
| FR-005 | WHEN the routed server's state is `Ready` or `Unknown` THE SYSTEM SHALL NOT delay the response and SHALL report the field as false | must |
| FR-006 | THE SYSTEM SHALL NOT use `Unknown` as evidence of indexing; only a recognized `Loading` signal sets the flag, as the diagnostics tools do | must |
| FR-007 | THE tool descriptions and output schemas SHALL document the field and say that an empty result with the field true may be incomplete | must |
| FR-008 | THE SYSTEM SHALL record the chosen option and its trade-off against the #423 decision in this spec's resolution section and in the doc comment of `IndexingGate` | must |
| FR-009 | THE gated incoming and outgoing call tools SHALL keep their current gate | must |
| FR-010 | WHERE the chosen option is "gate" for a tool THE SYSTEM SHALL use the existing `Required` setting and the existing bounded wait, with no new timeout | should |
| FR-011 | THE playbooks under `.local/testing/` SHALL gain a case for prepare tools under load on a cold start and the coverage status SHALL be reset | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | The disclosure is a typed field on a typed response, produced by one shared sampler with the diagnostics tools, not by per-tool boolean code, per [[constitution]] |
| NFR-002 | Latency | The flag is a cache read; no extra LSP round-trip and no wait (unless the decision is to gate) |
| NFR-003 | Consistency | One definition of "indexing in progress" is shared by all surfaces that report it |
| NFR-004 | Discoverability | `get_tool_support` or the tool descriptions let a client know which tools may report the field, per [[mcp/005-tool-capability-discoverability/spec\|mcp/005]] |
| NFR-005 | Output size | The field adds one boolean per response; the `tools/list` payload grows by the description text only |
| NFR-006 | Pre-1.0 | The response shape change is additive and is recorded in `CHANGELOG.md` |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Indexing signal | Per-read sample of the routed server's tracked indexing state | `indexing_in_progress: bool`, derived only from `Loading` |
| Prepare result (existing) | Result of `prepare_call_hierarchy` or `prepare_type_hierarchy` | `items` |
| Gate decision (existing) | Per-tool setting | `Required` or `NotRequired`, plus a recorded reason |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Prepare returns items while the state is `Loading` | Items returned; flag true (the read overlapped indexing); items are trusted as found |
| Prepare returns empty while `Loading` | Empty list with flag true: "may be incomplete, retry" |
| Prepare returns empty while `Ready` | Empty list with flag false: genuinely nothing |
| State flips from `Loading` to `Ready` during the read | Flag true if it was `Loading` at any sampled point, as the diagnostics tools define it |
| Server never reports a signal (state `Unknown`) | Flag false; the caller cannot be told more than the server told mcpls (FR-006) |
| Server restarted a moment ago | Covered by [[bridge/012-indexing-gate-after-restart/spec\|bridge/012]]; once that lands, the state is `Loading` and the flag follows |
| File-local tool during indexing | No flag; the answer is valid |
| `get_signature_help` or `get_inlay_hints` mid-index returns empty | `[NEEDS CLARIFICATION: classify in the audit]` |
| Multi-server routing where the prepare tool resolves one server | The flag reflects the routed server only |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Cold rust-analyzer start under CPU load, `prepare_call_hierarchy` on a function name, 10 repetitions | Every empty result carries `indexing_in_progress = true`; every non-empty result is correct |
| SC-002 | The same with `prepare_type_hierarchy` | Same |
| SC-003 | Ready server, 10 repetitions of each affected tool | Flag false; latency within noise of the previous build |
| SC-004 | Audit table in the plan | Every `NotRequired` site has a classification and a recorded decision |
| SC-005 | Output schemas | Each affected tool documents the field |
| SC-006 | A single sampler | The diagnostics tools and the new tools call one function for the signal |

## 8. Agent Boundaries

### Always (without asking)
- Reuse the diagnostics signal sampler rather than writing a second definition of "indexing in progress".
- Keep the field name `indexing_in_progress`.
- Run the full pre-commit suite and update `CHANGELOG.md` with the PR link.

### Ask First
- Choosing "gate" over "flag" for any tool: it reverses part of the #423 decision and adds latency.
- Adding the field to tools beyond the prepare steps after the audit.
- Changing the response shape of a tool whose output schema is already published to clients.

### Never
- Set the flag from an `Unknown` state.
- Add a new timeout or poll loop for a flagged tool.
- Gate a file-local tool.
- Change the gate of `get_diagnostics`, `get_cached_diagnostics` or `workspace_symbol_search` in this change.

## 9. Open Questions

- [NEEDS CLARIFICATION: option choice. (a) Flag: add `indexing_in_progress` to the ungated name-resolution tools; no latency, caller must handle the flag; consistent with the diagnostics tools. (b) Gate: switch the prepare tools to `Required`; the caller needs no new logic, but a prepare call on a cold start can wait up to 30 s, and the #423 decision explicitly avoided that. (c) Hybrid: flag now, gate later if agents ignore the flag. Recommended default: (a), since the diagnostics tools set the precedent and the failure is disclosure, not wrongness.]
- [NEEDS CLARIFICATION: why did #423 leave the prepare step ungated? The recorded reason is scope, not correctness. Confirm there is no hidden reason (for example prepare being used as a cheap readiness probe by clients) before reversing any part of it.]
- [NEEDS CLARIFICATION: audit the resolution-assisted tools (`get_signature_help`, `get_document_highlights`, `get_inlay_hints`) and decide for each: flag, gate, or exempt. Inlay hints depend on type inference and may be partial mid-index; highlights are largely file-local.]
- [NEEDS CLARIFICATION: where does the flag live in each response (a top-level field next to `items`, or inside a shared signals object as on the diagnostics responses)? Recommended default: the same flattened signals shape the diagnostics responses use.]
- [NEEDS CLARIFICATION: should `get_tool_support` list which tools can report the field?]

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]] — the gate and the #423 scope decision
- [[bridge/012-indexing-gate-after-restart/spec|bridge/012]] — the state of a replaced server, which feeds this flag
- [[bridge/011-push-only-server-diagnostics/spec|bridge/011]] — another response-state disclosure on the diagnostics tools
- [[mcp/005-tool-capability-discoverability/spec|mcp/005]] — `get_tool_support`
- Code: `crates/mcpls-core/src/bridge/translator/routing.rs` (`IndexingGate` and its doc), `crates/mcpls-core/src/bridge/translator/call_hierarchy.rs`, `crates/mcpls-core/src/bridge/translator/type_hierarchy.rs`, `crates/mcpls-core/src/mcp/server.rs` (`DiagnosticsRouteSignals`)
