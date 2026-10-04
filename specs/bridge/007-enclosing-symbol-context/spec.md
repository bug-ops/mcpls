---
aliases:
  - Enclosing symbol context
  - Symbol-aware references and diagnostics
tags:
  - sdd
  - spec
  - research
  - bridge
  - token-efficiency
  - competitor-gap
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[bridge/001-position-encoding-layer/spec|position-encoding-layer]]"
  - "[[bridge/006-lsp-indexing-readiness-gate/spec|lsp-indexing-readiness-gate]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]]"
---

# Feature: Enclosing-Symbol Context for References, Definitions, and Diagnostics

> [!info] Metadata
> **Type**: research (competitor gap, token-efficiency refinement)
> **Priority**: P4
> **Related issues**: #565

## Decision (#565): implemented, opt-in

> [!important] Resolved
> Shipped as an opt-in `context` input (`none` | `enclosing_symbol`, default `none`) on
> `get_references`, `get_definition`, `go_to_implementation`, `go_to_type_definition` and
> `get_diagnostics`. Default output and LSP traffic are unchanged (golden test).

Resolutions of the open questions in section 9:

| Question | Resolution |
|----------|------------|
| Opt-in shape | One closed enum `ResultContext` (`none`, `enclosing_symbol`), shared by all five tools |
| Hit-line snippet | Deferred; the enum leaves room for a later level |
| Diagnostics grouping | Annotate only; no grouped shape |
| FR-005 `get_cached_diagnostics` | Excluded: keeps "no new analysis" and zero extra LSP traffic. The tool has no `context` input |
| FR-011 file cap | 16 when `workspace.max_documents` is 0 (limit disabled), else `clamp(max_documents / 4, 1, 16)`; not charged to the primary item budget |
| NFR-002 deadline | One 30 s budget per call for all `documentSymbol` lookups; files not reached are `not_computed: deadline`, a lookup running past it is `unavailable: timed_out` |
| FR-015 approximate state | No item-level state. Containment is decided in MCP-normalized coordinates on both sides, and `positions_degraded` is the maximum of the primary and enrichment contexts |
| Straddling rule | Innermost symbol containing the whole range; else innermost containing the range start. Ties on identical ranges break by `(name, kind)`, so output is independent of server order |
| Other tools in scope | Call hierarchy and workspace symbols stay excluded |
| Constitution VI | The opt-in is the only path that adds LSP traffic, bounded by the file cap and deadline |

Wire shape: each item gains an `enclosing_symbol` field with an internally tagged `status`:
`resolved` (flattened `name_path`, `kind`, `range`, `fidelity` of `hierarchical` | `flat`),
`top_level`, `not_computed` (`reason`: `file_cap`, `out_of_workspace`, `tracker_limit`, `deadline`) or
`unavailable` (`reason`: `capability_absent`, `request_failed`, `timed_out`). The result gains
`enrichment { files_enriched, files_skipped, cut_short }`; both fields are omitted when `context` is
`none` or the result is empty. `item_budget` from the planning draft is not a reason: the primary
list is capped before enrichment and enrichment never spends that budget.

Security: every file named by a result is validated (`parse_file_uri`, then the workspace-root
check, canonical, so a symlink escaping the workspace is rejected) before it is opened, and
`documentSymbol` goes through that file's own `DocumentSymbols` route. A failing check yields
`not_computed: out_of_workspace`; the `out_of_workspace` flag of the location is not consulted.

Deviation from the "opt-in on a tool that does not support it is an error" edge case: the five
tools declare `context` in their schemas, and an unknown `context` field sent to any other tool is
ignored rather than rejected (`#[serde(flatten)]` position structs are incompatible with
`deny_unknown_fields`); schema absence is the contract.

Out of scope, recorded: `go_to_declaration` (added after this spec; it shares the location shape and can adopt `context` later), `get_cached_diagnostics`, hit-line snippets, grouping by symbol, call
hierarchy and workspace-symbol enrichment, cross-call symbol-tree caching.

## 1. Overview

### Problem Statement

`get_references`, `get_definition` (and its siblings `go_to_implementation` / `go_to_type_definition`)
and the diagnostics tools (`get_diagnostics`, `get_cached_diagnostics`) return **bare coordinates**:
a file URI plus a line/character range for each location, or a range plus message and severity for
each diagnostic (`crates/mcpls-core/src/bridge/translator/dto.rs`: `Location`, `Diagnostic`).
Nothing says *what code* a hit lives in.

To act on a reference list, an AI agent usually needs to know, per hit, "which function / method /
impl / class is this inside?". Today it must either call `get_document_symbols` for each distinct
file in the result and do the range-containment arithmetic itself, or read the file around every
location. Both cost extra MCP round trips and extra tokens (a full symbol tree or file excerpt to
answer a one-line question), and the arithmetic is error-prone work that every client reimplements.
A reference list of 200 hits across 30 files means up to 30 follow-up calls just to label the hits.

mcpls already holds every ingredient for answering this server-side:

- hierarchical `DocumentSymbol` support, including range normalization and the legacy flat
  `SymbolInformation` fallback (`crates/mcpls-core/src/bridge/translator/symbols.rs`);
- the position-encoding layer that makes 1-based MCP ranges and server ranges comparable
  (`bridge/encoding.rs`, see [[bridge/001-position-encoding-layer/spec|position-encoding-layer]]);
- result-size and I/O bounds for exactly this kind of per-item work: the shared normalized-item cap
  and `truncated` flag (#474, #487, #516, #519) and the per-response disk-read budget with the
  `positions_degraded` signal (#474, #497).

**Prior art.** One comparable code-intelligence bridge (an LSP-backed semantic toolkit exposed over
MCP) returns the enclosing symbol (name path and kind) of every reference from its
referencing-symbols tool, and groups diagnostics by symbol in its per-file diagnostics tool. Its
symbol tools also bound output through body-inclusion and depth parameters. It is the only
comparable bridge found to offer the capability, which is why the finding is P4: a niche refinement
of token efficiency, not a correctness defect. No result today is wrong or silently incomplete; each
is merely less informative than it could be.

**Why this is not a free addition.** Enrichment is not just a mapping step; it has a per-call cost
that interacts with several existing guarantees:

- It adds an LSP request (`textDocument/documentSymbol`) per distinct file in the result, plus a
  document open for any file not yet tracked. The constitution (section VI) states "no additional
  LSP round-trips added to existing tool flows". Enrichment is therefore only acceptable as
  something the caller asks for, with a hard bound, or as an explicit, reviewed constitutional
  exception.
- References and goto results routinely point outside the workspace (standard library, dependencies).
  Opening those files for symbol lookup must not bypass the path validation that the read tools apply
  at the inbound gate.
- Document-tracker limits and LRU eviction (`bridge/state.rs`, #495, #503) bound how many files can
  be open at once; an enrichment pass over many files must not starve or evict documents that other
  in-flight handlers depend on.
- Servers differ: some answer `documentSymbol` with a hierarchical `DocumentSymbol[]`, others with a
  flat `SymbolInformation[]` carrying only an optional `containerName`, and some do not advertise
  `documentSymbolProvider` at all.
- Position encoding: for non-UTF-16 servers a returned `character` may be inexact
  (`positions_degraded`). Deciding "is this hit inside that symbol" from inexact columns can misplace
  hits that sit on a symbol's first or last line.

### Goal

An AI agent can ask for a result in which every location or diagnostic carries the typed identity of
the innermost symbol that contains it (name path, kind, range), obtained in the same MCP call, with
bounded and explicitly reported cost, while the default output of every existing tool stays exactly
as it is today.

### Out of Scope

- Returning symbol **bodies** or source text of the enclosing symbol (body inclusion as offered by
  one comparable bridge). Returning the enclosing symbol's *range* lets the agent fetch text itself; body retrieval is a
  separate, larger output-size problem. Source-snippet context of the hit line itself is an open
  question (section 9), not committed scope.
- New standalone tools (for example a dedicated "find referencing symbols" tool). This spec
  concerns enriching the output of existing tools.
- Changing `get_document_symbols` output, `workspace_symbol_search` output, or the document-symbol
  conversion semantics (#361, #467 remain as they are).
- Symbol-aware **grouping or ordering** of results beyond attaching the enclosing symbol to each
  item, unless decided under the diagnostics open question.
- Indexing-readiness behavior changes: gating decisions of
  [[bridge/006-lsp-indexing-readiness-gate/spec|spec 006]] are reused, not revisited.
- Cross-call caching of symbol trees (see open questions).
- Technical design (plan phase).

> [!note] Constitution and type safety
> `specs/constitution.md` has no explicit type-safety clause; the type-safety-first requirement used
> here comes from the maintainer's standing rule (illegal states unrepresentable, no stringly-typed
> data, no null-overloading). It is expressed in section 4 (NFR-006, NFR-007) and in the data model
> in section 5. Section VI (no extra round trips on existing flows) is the binding constitutional
> constraint and drives FR-010.

## 2. User Stories

### US-001: Agent triages a reference list by containing symbol

AS AN AI coding agent evaluating the impact of changing a function
I WANT each returned reference to carry the symbol it lives in (for example "method `parse` of
`impl Parser`")
SO THAT I can group and prioritize call sites without a follow-up `get_document_symbols` call per
file or reading each file.

**Acceptance criteria:**
```
GIVEN a hierarchical-symbol server and a symbol referenced from 3 files, in 5 distinct functions
WHEN the agent calls get_references with enclosing-symbol context requested
THEN each returned location carries the name path, kind, and range of its innermost enclosing symbol
  AND the call needed no get_document_symbols call from the agent
```

### US-002: Agent understands a definition's container

AS AN AI coding agent navigating to a definition
I WANT the definition location to name the symbol that contains it
SO THAT I know whether it is a free function, a method of a specific type, or a nested item without
a second call.

**Acceptance criteria:**
```
GIVEN a go-to-definition result whose location is inside an impl block
WHEN enclosing-symbol context is requested
THEN the location carries the containing impl/type as part of its enclosing name path
```

### US-003: Agent reads diagnostics organized by symbol

AS AN AI coding agent fixing errors in a large file
I WANT each diagnostic to say which symbol it occurs in
SO THAT I can scope my edit to that symbol and batch diagnostics that share one.

**Acceptance criteria:**
```
GIVEN a file with 12 diagnostics spread over 4 functions
WHEN the agent requests diagnostics with enclosing-symbol context
THEN each diagnostic carries its enclosing symbol, and diagnostics outside any symbol say so explicitly
```

### US-004: Existing clients are unaffected

AS A maintainer of an existing MCP client or prompt that parses current mcpls output
I WANT output and latency of every existing call to be unchanged unless I opt in
SO THAT this feature cannot regress anything that works today, nor add LSP traffic to it.

**Acceptance criteria:**
```
GIVEN any existing tool call without the new opt-in input
WHEN the call is made before and after this feature ships
THEN the response is byte-identical and the number of LSP requests issued is unchanged
```

### US-005: Agent gets an honest result when enrichment is partial or impossible

AS AN AI coding agent using a flat-symbol server, a server without `documentSymbolProvider`, or a
very large result set
I WANT each item to say whether its enclosing symbol was resolved, absent, approximate, or not
computed (and why)
SO THAT I never mistake "not computed" for "this code is at top level".

**Acceptance criteria:**
```
GIVEN a server that answers documentSymbol with a flat SymbolInformation list, or does not advertise it
WHEN enclosing-symbol context is requested
THEN the primary result is still returned in full
  AND every item carries an explicit state distinguishing resolved / top-level / not computed / unavailable
```

## 3. Functional Requirements

Use EARS notation. Prefix with FR-NNN.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a caller requests enclosing-symbol context on `get_references` THE SYSTEM SHALL attach to each returned location the innermost symbol of its file whose range contains the location's range, expressed as an ordered name path (outermost ancestor first), the symbol kind, and the symbol range | must |
| FR-002 | WHEN a caller requests enclosing-symbol context on `get_definition` THE SYSTEM SHALL do the same for each returned definition location | must |
| FR-003 | WHEN a caller requests enclosing-symbol context on `go_to_implementation` or `go_to_type_definition` THE SYSTEM SHALL behave as for `get_definition` (all three share one location result shape and one enrichment path) | should |
| FR-004 | WHEN a caller requests enclosing-symbol context on `get_diagnostics` THE SYSTEM SHALL attach the enclosing symbol to each diagnostic, using the diagnostic's range | must |
| FR-005 | WHEN a caller requests enclosing-symbol context on `get_cached_diagnostics` THE SYSTEM SHALL either behave as for FR-004 or refuse the option with a clear input error `[NEEDS CLARIFICATION: that tool promises "no new analysis" and no extra LSP traffic; is enrichment, which needs a documentSymbol request, acceptable there, or should it be limited to files whose symbol tree is already available?]` | should |
| FR-006 | WHEN a location or diagnostic lies inside no symbol of its file THE SYSTEM SHALL report that explicitly as "top level / no enclosing symbol", distinguishable from every not-computed or unavailable state | must |
| FR-007 | WHEN the server returns hierarchical `DocumentSymbol` data THE SYSTEM SHALL derive the name path from the symbol's ancestor chain | must |
| FR-008 | WHEN the server returns the legacy flat `SymbolInformation` list THE SYSTEM SHALL choose the innermost containing symbol by range, SHALL build the name path from `containerName` where the server supplies it, and SHALL mark the item as reduced fidelity so the agent can tell a possibly incomplete name path from a full ancestor chain | must |
| FR-009 | WHEN the routed server does not advertise `documentSymbolProvider`, or the documentSymbol request for a file fails or times out, THE SYSTEM SHALL still return the full primary result and SHALL mark the affected items "unavailable" with a typed reason, never failing the whole call | must |
| FR-010 | WHEN enclosing-symbol context is not requested THE SYSTEM SHALL emit no documentSymbol request, no extra document open, and no new field, so existing output and LSP traffic stay unchanged | must |
| FR-011 | WHEN enclosing-symbol context is requested THE SYSTEM SHALL issue at most one documentSymbol request per distinct file per call, reuse its result for every item in that file, and enrich at most a fixed maximum number of distinct files per call `[NEEDS CLARIFICATION: cap value; candidate is a small constant well below the document-tracker limit]` | must |
| FR-012 | WHEN the file cap (FR-011) or the shared item budget (#516/#519) is reached THE SYSTEM SHALL return the remaining items unenriched with an explicit "not computed" state and typed reason, and SHALL report that enrichment was cut short at the result level, the same way `truncated` reports a capped list | must |
| FR-013 | WHEN a location points outside every configured workspace root THE SYSTEM SHALL NOT open that file for symbol lookup beyond what the tool's existing path validation permits, and SHALL return the item with a "not computed" state naming that reason | must |
| FR-014 | WHEN enrichment needs a file that is not yet open THE SYSTEM SHALL open it through the existing lazy document-tracking path, subject to the tracker's limits, and a limit refusal SHALL degrade only that file's items per FR-009 | must |
| FR-015 | WHEN any range involved in a containment decision could not be converted exactly (`positions_degraded` would be set for it) THE SYSTEM SHALL propagate the existing `positions_degraded` signal on the result, and SHALL NOT present an enclosing symbol derived from inexact positions as exact `[NEEDS CLARIFICATION: add a typed "approximate" item state, or rely on the result-level positions_degraded flag alone?]` | must |
| FR-016 | WHEN the routed server is still indexing THE SYSTEM SHALL NOT add any readiness wait for enrichment: gated tools keep their existing gate, and file-local documentSymbol lookup is performed under the already-resolved gating decision of [[bridge/006-lsp-indexing-readiness-gate/spec\|spec 006]] (FR-008 resolution: document symbols are valid mid-index) | must |
| FR-017 | WHEN enclosing-symbol context is requested THE SYSTEM SHALL describe the new input and the new output fields in the tool's published `inputSchema` and `outputSchema` (`crates/mcpls-core/src/mcp/tool_surface.json`) and in the tool description, including the meaning of every state | must |
| FR-018 | WHEN two items are contained in nested symbols THE SYSTEM SHALL select the innermost; WHEN two candidate symbols have identical ranges THE SYSTEM SHALL pick deterministically so repeated calls return identical output | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance / constitution VI | The opt-in is the only path that adds LSP traffic; the added work is bounded by FR-011 (distinct-file cap) plus the existing item budget, so one MCP call can never trigger unbounded documentSymbol requests or document opens |
| NFR-002 | Performance | Enrichment SHALL NOT be on the critical path of an item's primary data: the primary result is computed first and is complete irrespective of enrichment outcome; added latency is bounded by the cap times the existing per-request timeout, `[NEEDS CLARIFICATION: is a separate overall enrichment deadline needed so a slow server cannot multiply the call's latency by the file cap?]` |
| NFR-003 | Security | Enrichment SHALL NOT widen the set of files mcpls opens or reads beyond what the tool's existing path validation and workspace-root rules allow (FR-013); server-supplied URIs SHALL NOT be trusted for file reads (same stance as the flat-symbol handling in `symbols.rs`) |
| NFR-004 | Robustness | One failing server or file SHALL NOT affect other items or other servers (graceful degradation, constitution I); no `unwrap`/`expect` on enrichment paths |
| NFR-005 | Compatibility | Output changes SHALL be additive and omitted when not requested; the MCP response stays schema-valid and structured-output validated against the updated `outputSchema` |
| NFR-006 | Type safety | The enclosing-symbol outcome SHALL be a closed, typed sum with explicit states (resolved, top level, not computed with reason, unavailable with reason), never `null`, an empty string, or an empty list standing in for several meanings. Reasons SHALL be enums, not free text |
| NFR-007 | Type safety | The name path SHALL be a sequence of name segments, not a delimiter-joined string (delimiters are language-specific and ambiguous); the symbol kind SHALL reuse the existing numeric LSP kind representation used by `get_document_symbols` (#467) so a client can feed it back to the existing `kind_filter` |
| NFR-008 | Consistency | One shared enrichment path SHALL serve all enriched tools; position conversion SHALL go through `bridge/encoding.rs` with no hand-written encoding logic elsewhere (constitution IV) |
| NFR-009 | Determinism | Identical inputs against an unchanged server state SHALL produce identical enrichment, independent of server response ordering |
| NFR-010 | Testability | Hierarchical, flat, capability-less, failing, over-cap, out-of-workspace, and degraded-position servers SHALL each be coverable with the existing in-process fake-server test harness without a real language server; at least one live check per class is recorded in the testing documents under `.local/testing/` |

## 5. Data Model

Conceptual entities only; concrete types belong to the plan.

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Enclosing Symbol | The innermost symbol of a file that contains a hit | name path (ordered segments, outermost first), kind (numeric LSP kind), range (normalized MCP range), fidelity (full hierarchy vs flat `containerName`-derived) |
| Enclosing Symbol Outcome | Closed set describing what is known for one item | one of: resolved(Enclosing Symbol), top level (no enclosing symbol), not computed(reason: file cap, item budget, out of workspace, tracker limit), unavailable(reason: capability absent, request failed, timed out) |
| Context Request | Caller's opt-in on a tool call | requested level, drawn from a closed enum so later levels (for example hit-line snippet) can be added without a boolean explosion `[NEEDS CLARIFICATION: see section 9]` |
| Enrichment Summary | Result-level statement about completeness of enrichment | whether enrichment was cut short, count of files enriched vs skipped; complements the existing `truncated` and `positions_degraded` flags |
| Symbol Tree (per file, per call) | The documentSymbol answer used to resolve all items in one file | origin (hierarchical or flat), lifetime limited to the single call |

Existing entities reused unchanged: `Location`, `Diagnostic`, `Symbol` (`kind` representation),
`PositionDegradation`, the normalized-item budget, and the per-response disk-read budget.

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Hit lies in no symbol (module-level statement, import, attribute) | Outcome "top level", never confused with not-computed (FR-006) |
| Hit spans multiple symbols (range starts in one symbol, ends in another) | Innermost symbol that fully contains the range; if none does, the innermost symbol containing the range start `[NEEDS CLARIFICATION: confirm rule]`; deterministic either way (FR-018) |
| Hit is the symbol's own name (e.g. the declaration included via `include_declaration`) | The symbol itself is the innermost enclosing symbol; no special-casing beyond the stated rule |
| Reference list of N hits in 1 file | Exactly one documentSymbol request for that file (FR-011) |
| Result spans more files than the enrichment cap | First files in result order enriched, rest "not computed: file cap", result-level summary says so (FR-012) |
| Location outside workspace roots (stdlib, dependency) | Returned as today, outcome "not computed: out of workspace" (FR-013); no new file opened for it |
| Server lacks `documentSymbolProvider` | Items "unavailable: capability absent"; primary result intact (FR-009) |
| documentSymbol returns `ContentModified` or times out | Items for that file "unavailable" with reason; existing content-modified retry behavior (spec lsp/005) applies at the request layer, enrichment adds no retry loop of its own |
| Server returns flat `SymbolInformation` with no `containerName` | Innermost containing symbol found by range, name path of one segment, fidelity "flat" (FR-008) |
| Flat server returns entries whose `location.uri` differs from the queried file | Entry URIs are not trusted; all entries are interpreted against the queried document (consistent with `symbols.rs`) |
| Non-UTF-16 server, hit on the boundary line of a symbol | `positions_degraded` propagated; enclosing symbol not presented as exact (FR-015) |
| Document changed between the primary request and the documentSymbol request | Outcome may describe a newer document version than the hit; accepted as eventually consistent, the same stance as the diagnostics cache merge, `[NEEDS CLARIFICATION: is a version check needed, or is this acceptable?]` |
| Document tracker at its limit | The tracker's existing refusal degrades only the affected file's items (FR-014); no eviction of documents with in-flight handlers (#503) |
| Respawn of the server mid-call | Items for affected files "unavailable"; no crash, no hang (spec lsp/001) |
| Empty result (no locations / no diagnostics) | No documentSymbol request issued; response identical to non-enriched empty result |
| Opt-in on a tool that does not support it | Clear input validation error naming the tool, never a silent no-op |

## 7. Success Criteria

Measurable metrics that prove the feature works:

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | MCP calls an agent needs to label a reference list with enclosing symbols (N hits, F distinct files) | 1, versus 1 + F calls (or 1 + N file reads) today |
| SC-002 | LSP requests added by an enriched call | At most F_enriched documentSymbol requests, with F_enriched at or below the cap (FR-011); zero when not requested |
| SC-003 | Default output and LSP traffic of every existing tool call | Unchanged: byte-identical response and identical request count versus the pre-change baseline (golden tests) |
| SC-004 | Items with a determinate outcome state under hierarchical, flat, capability-less, failing, over-cap, and out-of-workspace servers | 100%: no item carries an implicit or null outcome |
| SC-005 | Response-size overhead of enrichment relative to the primary result | `[NEEDS CLARIFICATION: target, to be set once a representative reference list is measured; the point of the feature is a net token saving for the agent, so the overhead must be well below a get_document_symbols payload per file]` |
| SC-006 | Live verification against rust-analyzer (hierarchical) and one flat-symbol server | Recorded in the `.local/testing/` playbooks and coverage status |

## 8. Agent Boundaries

### Always (without asking)
- Keep the default (non-opt-in) path of every tool byte-identical and free of added LSP requests.
- Reuse the existing document-symbol conversion, position-encoding layer, item budget, and disk-read
  budget; do not add a parallel implementation (DRY, constitution VII).
- Model outcomes and reasons as closed enums (NFR-006, NFR-007); update `tool_surface.json` and its
  consistency tests together with the DTOs.
- Run the full pre-commit check suite from the constitution before any commit.

### Ask First
- Changing the default (making enrichment on by default) or amending constitution section VI.
- Adding a new dependency, a new standalone tool, or a cross-call symbol-tree cache.
- Choosing the enrichment file cap, overall enrichment deadline, and opt-in input shape if the
  plan's recommendation differs from section 9.
- Extending scope to `get_cached_diagnostics`, call hierarchy, or workspace-symbol results.

### Never
- Issue documentSymbol requests or open extra documents when the caller did not request enrichment.
- Open or read a file outside what the tool's existing path validation allows in order to resolve a
  symbol (NFR-003).
- Fail or truncate the primary result because enrichment failed (FR-009).
- Represent "not computed" or "unavailable" as "top level", `null`, or an empty path.
- Change `get_document_symbols` or `workspace_symbol_search` output semantics.

## 9. Open Questions

- [NEEDS CLARIFICATION: issue number for the finding.]
- [NEEDS CLARIFICATION: opt-in input shape. Recommendation: a single closed-enum input shared by all
  enriched tools (for example none / enclosing symbol), not a per-tool boolean, so adding a
  hit-line snippet level later is additive. Parameter name and wire spelling to be decided in plan.]
- [NEEDS CLARIFICATION: is a bounded source snippet of the hit line (and not the symbol body) in
  scope? It is cheap (the line is already read for encoding conversion) but costs tokens per item and
  is the half of the finding's title ("source-snippet context") that one comparable bridge covers
  with symbol-body inclusion. Recommendation: defer; ship enclosing symbol first.]
- [NEEDS CLARIFICATION: for diagnostics, attach the symbol to each diagnostic (committed in FR-004)
  or also offer a grouped-by-symbol shape as one comparable bridge does? Grouping changes the response structure and
  is a larger breaking-risk surface; recommendation: annotate only.]
- [NEEDS CLARIFICATION: FR-005, `get_cached_diagnostics` promises no new analysis; include or refuse.]
- [NEEDS CLARIFICATION: FR-011 file cap value and NFR-002 overall enrichment deadline.]
- [NEEDS CLARIFICATION: FR-015, typed "approximate" item state versus result-level flag only.]
- [NEEDS CLARIFICATION: containment rule when a hit range straddles symbol boundaries (section 6).]
- [NEEDS CLARIFICATION: after real-world measurement, what evidence would justify flipping the
  default to on? Under constitution VI this requires an explicit amendment.]
- [NEEDS CLARIFICATION: should a per-call symbol-tree cache be allowed to outlive the call
  (staleness versus fewer requests across successive references/diagnostics calls)? Out of scope
  for now.]
- [NEEDS CLARIFICATION: scope extension to `get_incoming_calls` / `get_outgoing_calls` (they already
  name call-hierarchy items) and `workspace_symbol_search` (already carries `container_name`):
  excluded here, confirm.]

## 10. See Also

- [[constitution]] — project principles (section VI: no extra LSP round trips on existing flows)
- [[MOC-specs]] — all specifications
- [[bridge/001-position-encoding-layer/spec|position-encoding-layer]] — coordinate conversion and
  the `positions_degraded` contract this feature must propagate
- [[bridge/006-lsp-indexing-readiness-gate/spec|lsp-indexing-readiness-gate]] — gating decisions
  (document symbols valid mid-index; diagnostics ungated with an `indexing_in_progress` flag)
- [[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]] — tool surface and
  `tool_surface.json` / `outputSchema` conventions
- #474, #487, #497, #516, #519 — existing item caps, disk-read budget, and `positions_degraded`
- #361, #467 — document-symbol conversion and numeric `kind` contract reused here
- #495, #503 — document-tracker limits and eviction rules enrichment must respect
