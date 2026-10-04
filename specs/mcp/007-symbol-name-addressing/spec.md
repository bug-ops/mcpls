---
aliases:
  - Symbol-name addressing
  - Name-based tool addressing
tags:
  - sdd
  - spec
  - research
  - mcp
  - parity
  - ux
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]]"
  - "[[mcp/005-tool-capability-discoverability/spec|tool-capability-discoverability]]"
  - "[[bridge/006-lsp-indexing-readiness-gate/spec|lsp-indexing-readiness-gate]]"
  - "[[bridge/001-position-encoding-layer/spec|position-encoding-layer]]"
---

# Feature: Symbol-Name Addressing for Position-Based Tools

> [!info] Metadata
> **Type**: research (competitor parity gap)
> **Priority**: P2 (two comparable bridges ship it; see parity rubric)
> **Related issues**: #563

> [!success] Implemented (MVP, #563)
> - **Tools (FR-002):** `get_hover`, `get_definition`, `get_references`, `go_to_implementation`,
>   `go_to_type_definition`, `prepare_call_hierarchy` and `rename_symbol`. `get_signature_help` is
>   deferred: a definition identifier is not a call site.
> - **Shape (FR-006):** optional name fields on each existing tool: `symbol_name`, `symbol_kind`
>   (name or numeric, validated like `workspace_symbol_search`'s `kind_filter`) and `container`.
>   Exactly one addressing form is representable after deserialization; both, neither, half a
>   position, or qualifiers without a name are `-32602`.
> - **Resolution (FR-001, FR-003, FR-004, FR-007):** against `textDocument/documentSymbol` (flat
>   or hierarchical), exact name first, then the simple name with generic arguments, receivers and
>   qualifiers (`Type::method`, `(*T).M`) normalized; kind and container narrow before ambiguity.
>   Outcomes are typed `-32602` errors with `data.resolution`: `ambiguous` (all candidates, capped
>   at 50, with positions), `not_found`, `not_defined_in_file` (the name occurs as a whole word
>   in the document but nothing defines it there; no text-occurrence fallback binds to it) and
>   `position_unverified`.
> - **Position verification (NFR-002):** the identifier position must be verified against the
>   tracked document text: `selectionRange.start` must spell the simple name as a whole word
>   (Unicode-aware), otherwise the name must occur exactly once as a whole word inside the
>   symbol's range (at most 500 lines), else `position_unverified`. The result's
>   `resolved_symbol.position_source` says which (`selection_range` or `inferred`; FR-009).
> - **Gating (FR-010, US-004):** the target tool's capability is checked before the
>   document-symbol request; the final query uses the position form's own indexing gate and
>   `positions_degraded`.
> - **Deferred:** `go_to_declaration` (same shape as `get_definition`) takes a position only.
> - **Matching:** a trailing parameter list (`bar(int)`, as jdtls names methods) and generic
>   arguments are stripped; every symbol with the simple name is a candidate, narrowed by kind
>   and container, so a free function never shadows a same-named method.
> - **Not done:** nested name paths beyond `Type::method`, cross-file resolution, a name form in
>   `get_tool_support` (FR-013 is met through `tools/list` descriptions and schemas).

## 1. Overview

### Problem Statement

Every position-based tool in mcpls addresses its target symbol as `file_path` plus a 1-based `line`
plus a `character` (`get_hover`, `get_definition`, `get_references`, `go_to_implementation`,
`go_to_type_definition`, `prepare_call_hierarchy`, `rename_symbol`, `get_signature_help`; see
`crates/mcpls-core/src/mcp/tool_surface.json`). The only name-based entry points are
`workspace_symbol_search` (`query`, `kind_filter`, `limit`) and `get_document_symbols`, and
neither feeds into the position tools except by the caller copying `line`/`character` values out
of one response into the next request.

LLM agents are unreliable at counting exact line and column numbers in source text. A miscounted
position fails in two ways:

- it resolves to a **different symbol** than the one the agent meant (an adjacent identifier,
  a keyword, a type name on the same line), and the response looks plausible, so the agent acts
  on information about the wrong symbol; or
- it resolves to **nothing** (whitespace, comment, out-of-range), which is indistinguishable from
  "this symbol genuinely has no references / no definition".

Both are silent. This is the same hazard class that [[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]]
addressed for indexing: an empty or wrong answer that an agent interprets as a fact (for example
"unused, safe to delete"). The workaround today is a mandatory two-call chain: a name search or
document-symbol listing, then a position call using copied coordinates. That costs a round trip,
pushes position bookkeeping onto the model, and still relies on the model copying numbers
correctly.

### Prior art

| Bridge | Name-based surface | Notes |
|--------|--------------------|-------|
| One comparable bridge | Definition, references, and rename tools take a file path, a symbol name, and an optional symbol kind. The server resolves the symbol via `documentSymbol`, tries position candidates, and returns all matching candidates when ambiguous. A separate strict variant of rename accepts an exact position. | The `documentSymbol` approach fails for imported or referenced (non-defined) symbols; a text-occurrence fallback is a known proposed remedy. |
| A second comparable bridge, widely adopted and actively developed | Symbol lookup, referencing-symbol lookup, declaration, implementation, and rename tools all operate on symbol name paths rather than line numbers. | Its documentation presents name-based, high-level addressing as a deliberate alternative to low-level line numbers. |

Two comparable bridges converge on name-based addressing, one of them widely adopted in the
category. By the parity rubric this is P2.

### Goal

An AI agent can direct a navigation, inspection, or rename-proposal tool at a symbol by its name
(plus optional disambiguating context) in a single call, and can never receive an answer about a
symbol other than the one it named without being told so.

### Out of Scope

- Technical design (resolution algorithm, parameter shapes, DTO layout). Those belong in a plan.
- Applying edits. mcpls is a bridge, not an editor: `rename_symbol` keeps returning a proposed
  workspace edit and name addressing must not change that (see NFR-005).
- Removing or deprecating the existing position-based parameters. Position addressing remains the
  precise, unambiguous form and the fallback when name resolution cannot answer.
- Semantic or fuzzy search over code bodies (returning symbol bodies, pattern search by content),
  and symbol-level *editing* tools (insert before/after symbol, replace symbol body).
- Changes to how LSP servers are discovered, spawned, or routed
  ([[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]]).
- Gating `workspace_symbol_search` on indexing readiness as a general fix (tracked separately
  in #422); this spec only states how name resolution must behave when the index is not ready.

## 2. User Stories

### US-001: Agent inspects a symbol it can name but cannot locate precisely

AS AN AI coding agent that has read a file and knows a function's name
I WANT to ask for that function's hover, definition, references, or implementations by name in one call
SO THAT I do not count lines and columns by hand or chain a search call before every query.

**Acceptance criteria:**
```
GIVEN a file containing exactly one symbol named "parse_config"
WHEN the agent requests references for "parse_config" in that file by name
THEN the response describes references to that symbol
  AND no line or character was supplied by the agent
```

### US-002: Agent is told when a name is ambiguous

AS AN AI coding agent
I WANT every candidate returned when a name matches more than one symbol
SO THAT I choose deliberately and never act on an arbitrary pick.

**Acceptance criteria:**
```
GIVEN a file with two methods named "new" on different types
WHEN the agent requests the definition of "new" by name without further qualification
THEN the response lists both candidates with enough context to tell them apart
  AND the response states that the request was ambiguous
  AND no single candidate is presented as the answer
```

### US-003: Agent proposes a rename by name and stays in control of the position fallback

AS AN AI coding agent proposing a rename
I WANT to name the symbol, and to fall back to an exact position when the name is ambiguous or unresolvable
SO THAT a rename proposal never targets the wrong symbol because of a miscounted column.

**Acceptance criteria:**
```
GIVEN a name that resolves to exactly one symbol
WHEN the agent requests a rename by name with a new name
THEN the response is the same proposed workspace edit the position-based rename would return
  AND mcpls applies nothing to disk

GIVEN a name that resolves to several symbols
WHEN the agent requests a rename by name
THEN no workspace edit is produced
  AND the response lists the candidates with their exact positions
  AND the agent can re-issue the request using one candidate's position
```

### US-004: Operator relies on name addressing in a multi-language session

AS A mcpls operator running several LSP servers
I WANT name-based requests routed by file exactly as position-based requests are
SO THAT behavior (server selection, capability errors, degradation) is predictable across both addressing forms.

**Acceptance criteria:**
```
GIVEN a session where the file's language server lacks the capability required by the tool
WHEN the agent calls the tool by name
THEN the same capability error is returned as for a position-based call
  AND no LSP request for the unsupported capability is issued
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN an agent calls a symbol-addressable tool with a symbol name instead of a position THE SYSTEM SHALL resolve the name to a position within the requested file and answer as if that position had been supplied | must |
| FR-002 | THE SYSTEM SHALL make name addressing available for at least `get_definition`, `get_references`, and `rename_symbol`, and SHOULD make it available for `get_hover`, `go_to_implementation`, `go_to_type_definition`, `prepare_call_hierarchy`, and `get_signature_help` | must (first three), should (rest) |
| FR-003 | WHEN a name resolves to more than one candidate symbol THE SYSTEM SHALL return all candidates, each with kind, container (enclosing symbol) and position, and SHALL NOT silently select one | must |
| FR-004 | WHEN a name resolves to no symbol THE SYSTEM SHALL return an explicit not-found outcome distinct from "the symbol exists and has no references/definition", and SHALL NOT fall through to an empty success | must |
| FR-005 | WHERE the caller supplies an optional symbol kind or container qualifier THE SYSTEM SHALL use it to narrow candidates before ambiguity is evaluated, and SHALL validate a kind given by name against the known kinds the way `workspace_symbol_search`'s `kind_filter` does | should |
| FR-006 | THE SYSTEM SHALL keep position-based addressing fully supported, and SHALL define a request shape in which a caller supplies exactly one addressing form, rejecting a request that supplies both or neither | must |
| FR-007 | WHEN a name cannot be resolved through the file's document symbols (an imported, re-exported, or otherwise referenced-but-not-defined symbol) THE SYSTEM SHALL either resolve it by a documented fallback or return a distinct "name not defined in this file" outcome that tells the caller to use position addressing or `workspace_symbol_search`; it SHALL NOT report not-found-as-empty | must |
| FR-008 | WHEN a resolved name is used for `rename_symbol` THE SYSTEM SHALL refuse to produce a proposed edit if resolution was ambiguous (FR-003), and SHALL otherwise return the same edit structure and capability gating as position-based rename | must |
| FR-009 | WHEN name resolution is performed THE SYSTEM SHALL report which candidate (position) it ultimately queried, so the agent can audit and reuse the position | should |
| FR-010 | WHEN the target file's server is still indexing or reports an unknown readiness state THE SYSTEM SHALL apply the same readiness behavior to the final query as the position-based tool does ([[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]]), and SHALL make name resolution's own result distinguishable from "index not ready" | must |
| FR-011 | WHEN the file's server negotiated a non-UTF-16 position encoding and positions are degraded THE SYSTEM SHALL surface `positions_degraded` on name-addressed results using the existing `PositionDegradation` semantics, and SHALL NOT claim exactness it cannot provide (see section 6) | must |
| FR-012 | WHEN a name-addressed call is routed THE SYSTEM SHALL select the LSP server through the same per-file, per-tool routing as the position form ([[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]]) | must |
| FR-013 | WHEN a name-addressed tool is listed or described to the agent THE SYSTEM SHALL make the name-addressing form discoverable from `tools/list` without requiring a failing call, and SHALL keep `get_tool_support` consistent with the name-addressed form's capability requirements ([[mcp/005-tool-capability-discoverability/spec|mcp/005]]) | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | Addressing form, symbol kind, resolution outcome (`Resolved`, `Ambiguous`, `NotFound`, `NotDefinedInFile`) and candidate lists SHALL be modeled as closed Rust types (enums / newtypes) that make illegal states unrepresentable, per [[constitution]] and the project's type-safety principle. "Both position and name" and "neither" SHALL NOT be representable past the MCP deserialization boundary. Symbol kind SHALL NOT be passed internally as a raw string or bare integer |
| NFR-002 | Correctness | Name resolution SHALL never produce a wrong-symbol answer silently. Any path that cannot guarantee the named symbol was queried SHALL say so in the response |
| NFR-003 | No extra staleness | Name resolution SHALL operate on the document content the server currently holds (tracked open document), so a just-edited file does not resolve against stale symbols |
| NFR-004 | Performance | Name addressing SHALL add at most one extra LSP round trip (document symbols) per call over the position form, and SHALL NOT add a workspace-wide search to the default path. Candidate lists SHALL be bounded (cap and a truncation indicator consistent with `workspace_symbol_search`'s `limit`/`truncated`) |
| NFR-005 | Bridge, not editor | Name addressing SHALL NOT introduce any tool that writes files. `rename_symbol` SHALL remain proposal-only |
| NFR-006 | Encoding centralization | Any position produced or consumed during resolution SHALL go through `bridge/encoding.rs`; no hand-written column conversion elsewhere ([[constitution]] IV) |
| NFR-007 | Backward compatibility | Position-based calls SHALL behave identically to today. Pre-v1.0.0 breaking changes to parameter shapes are acceptable if documented in `CHANGELOG.md`, but the position form's semantics SHALL NOT change |
| NFR-008 | Security | Name and qualifier inputs are untrusted MCP input: they SHALL be size-bounded and validated at the boundary, and file paths SHALL go through the existing workspace-root validation |

## 5. Data Model

No persistent data. Conceptual entities:

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Symbol reference (addressing) | Closed choice between "by position" and "by name" | position form: `file_path`, `line`, `character`; name form: `file_path`, `symbol_name`, optional kind, optional container qualifier |
| Resolution outcome | Result of resolving a name | resolved (one position), ambiguous (candidate list), not found, not defined in file |
| Candidate | One possible meaning of a name | name, kind, container, position, optionally range |
| Resolution provenance | Which position was actually queried | candidate position, whether it was chosen by resolution or supplied by the caller |

Existing data reused: LSP `DocumentSymbol` (name, kind, `range`, `selectionRange`, children) as
returned by `get_document_symbols`, and `PositionDegradation` from `bridge/translator/dto.rs`.

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Name matches multiple symbols (overloads, same-named methods on different types, shadowing) | All candidates returned with kind, container, position; the request is reported ambiguous; nothing is picked (FR-003) |
| Name matches nothing in the file | Explicit not-found outcome, not an empty success (FR-004) |
| Name refers to an imported or re-exported symbol, so `documentSymbol` has no entry | "Not defined in this file" outcome with guidance, or a documented text-occurrence fallback whose provenance is reported; never an empty reference list (FR-007). [NEEDS CLARIFICATION: choose fallback vs. explicit refusal; a text-occurrence fallback can bind to comments, strings, or shadowed identifiers and so risks violating NFR-002] |
| Qualified names (`Type::method`, `Class.method`, nested name paths) | [NEEDS CLARIFICATION: whether the name form accepts a path of nested names or only a simple name plus a container qualifier; kind and container narrowing (FR-005) is the minimum] |
| Server does not return hierarchical `DocumentSymbol` (returns flat `SymbolInformation`) | Resolution works on the flat form with container taken from `containerName`; if a server returns neither, the outcome is a capability error identical to `get_document_symbols`'s |
| File's server lacks `documentSymbolProvider` but has the target capability | Name addressing is unavailable for that file; the error names the missing resolution capability, and position addressing still works. `get_tool_support` should reflect this (FR-013) |
| Server still indexing | The final query is subject to the existing readiness gate (FR-010). Document symbols are file-local and valid mid-index, but results such as references may be incomplete; the outcome distinguishes resolution success from "index not ready" |
| Non-UTF-16 server encoding, so positions may be inexact | `positions_degraded` is surfaced per existing semantics. The `Request` level (the queried position was sent unconverted, so the result may describe a different symbol) is the worst case for name addressing, because it defeats the guarantee the feature exists for. [NEEDS CLARIFICATION: whether a name-addressed call must refuse, rather than merely flag, when resolution would send an unconverted position] |
| Name resolves to a position whose `selectionRange` differs from `range` | The identifier position (selection range start), not the declaration start, is queried; the candidate reports the position it will use |
| Same name in a very large file (candidate explosion) | Candidate list is capped with a truncation indicator (NFR-004) |
| File not open or not yet tracked | Resolution opens the document through the same lazy tracking as other tools ([[bridge/002-document-tracker-synchronization/spec|bridge/002]]) before asking for symbols |
| Both `symbol_name` and position supplied, or neither | Rejected at the MCP boundary with a clear invalid-params error (FR-006, NFR-001) |
| Empty or oversized name | Rejected at the boundary (NFR-008) |
| Concurrent edit between resolution and the final query | The two LSP requests operate on the same tracked document version, or the response reports the content changed. [NEEDS CLARIFICATION: acceptable race window; position drift between the document-symbol request and the final request is the name-form analogue of a stale position] |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Calls an agent needs to get references/definition/rename proposal for a uniquely named symbol in a known file | 1 (down from 2 with copy-through today) |
| SC-002 | Silent wrong-symbol answers on the name-addressed path in the test matrix (ambiguous, shadowed, imported, degraded-encoding cases) | 0 |
| SC-003 | Name-addressed vs. position-addressed parity: for each supported tool, the same final query result for a uniquely resolved name | identical in all matrix cases |
| SC-004 | Ambiguous names return every candidate and no single answer | 100% of ambiguity test cases |
| SC-005 | Non-regression: existing position-based tool tests | pass unchanged |
| SC-006 | Name-addressed capability errors, routing and `positions_degraded` behavior | match the position form in every shared case |

## 8. Agent Boundaries

### Always (without asking)
- Reuse the existing document-symbol and workspace-symbol translation, the typed `SymbolKind` handling, `bridge/encoding.rs`, and the per-file routing; do not add a parallel implementation (DRY, [[constitution]] VII).
- Model addressing forms and resolution outcomes as closed enums/newtypes; convert to primitives only at the MCP boundary.
- Test the ambiguity, not-found, imported-symbol, degraded-encoding and capability-error cases before any success-path polish.

### Ask First
- Choosing between a text-occurrence fallback and an explicit refusal for non-defined symbols (FR-007).
- Whether to introduce a separate strict, position-only variant of each tool (the approach one comparable bridge takes for rename) or keep a single tool with a closed addressing choice.
- Adding any dependency (for example for text search) or any tool that is not an addressing variant of an existing tool.
- Any breaking change to existing tool parameter shapes beyond what NFR-007 allows.

### Never
- Silently pick one candidate when a name is ambiguous (FR-003, NFR-002).
- Report not-found or not-defined-here as an empty success (FR-004, FR-007).
- Apply an edit to disk from a name-addressed rename (NFR-005).
- Weaken `Translator::require_capability`, the indexing-readiness gate, or `positions_degraded` reporting for name-addressed calls.
- Perform position/column conversion outside `bridge/encoding.rs`.

## 9. Open Questions

> [!question] Needs a decision before planning
> - [NEEDS CLARIFICATION: scope of FR-007 for imported/referenced symbols: text-occurrence fallback (broader coverage, risk of binding to the wrong occurrence) vs. refuse and direct the agent to `workspace_symbol_search` or position addressing (safer, narrower).]
> - [NEEDS CLARIFICATION: nested name paths (for example `Type/method`) vs. simple name + kind + container qualifier. Affects the candidate model and how far ambiguity can be narrowed without a second call.]
> - [NEEDS CLARIFICATION: tool shape. Options: (a) optional name parameters on each existing tool with a closed either/or choice, (b) separate name-addressed tools, (c) a resolve-only tool returning a position the agent passes on. Pre-v1.0.0, so breaking changes are allowed, but the `tools/list` surface growth (23 tools after #563, #564, #567) and `get_tool_support` consistency matter.]
> - [NEEDS CLARIFICATION: behavior when `positions_degraded` would be `Request` for the resolved query: refuse vs. flag (see section 6).]
> - [NEEDS CLARIFICATION: should `workspace_symbol_search` results be directly usable as name-addressing input (cross-file resolution), or is the first release strictly per-file, as in the file-scoped name form used by comparable bridges?]
> - [NEEDS CLARIFICATION: measurable evidence that agents actually miscount positions in mcpls sessions (error rate, retries). Not required for P2 parity classification, but would inform priority escalation.]

## 10. See Also

- [[constitution]] — project principles (type safety, encoding centralization, simplicity)
- [[MOC-specs]] — all specifications
- [[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]] — tool router and per-file, per-tool server selection that FR-012 reuses
- [[mcp/005-tool-capability-discoverability/spec|tool-capability-discoverability]] — `get_tool_support` must stay consistent with any name-addressed form (FR-013)
- [[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]] — readiness gate; document symbols and workspace symbols are currently ungated there, and #422 tracks generic readiness
- [[bridge/001-position-encoding-layer/spec|bridge/001]] — 1-based/0-based conversion and encoding negotiation that FR-011 and NFR-006 depend on
- #322 — position `u32` arguments swappable without a compile error: a related type-safety hazard in the position-based form that name addressing sidesteps
