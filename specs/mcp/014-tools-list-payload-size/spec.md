---
aliases:
  - tools/list payload size
  - Tool schema size budget
tags:
  - sdd
  - spec
  - mcp
  - tools-list
  - schema
  - performance
created: 2026-10-05
status: implemented
related:
  - "[[constitution]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]]"
  - "[[mcp/005-tool-capability-discoverability/spec|tool-capability-discoverability]]"
  - "[[mcp/014-tools-list-payload-size/plan|plan]]"
---

# Feature: Bounded `tools/list` Payload Size

> [!info] Metadata
> **Type**: enhancement
> **Priority**: P3
> **Related issues**: #630
> **Baseline commit**: 517cb53 (29 tools)

## 1. Overview

### Problem Statement

Every MCP client pays for `tools/list` once per session, and a client that places tool definitions
into a model context pays for it again in tokens on every request that carries them. At HEAD
`517cb53` the serialized `tools` array of mcpls is about 171 KB, roughly 45k tokens (the 179,030 B
figure in the original finding is the same payload measured with `", "` and `": "` separators;
compact serialization, which is what goes on the wire, is 171,465 B). Almost none of it is the
information a client needs to choose and call a tool.

Measured on the in-process `build_tool_router(None).list_all()` (the surface pinned by
`tool_surface.json`):

| Component | Bytes (compact) | Share |
|-----------|-----------------|-------|
| Total `tools` array (29 tools) | 171,465 | 100% |
| `outputSchema` sum | 120,109 | 70% |
| `inputSchema` sum | 27,483 | 16% |
| Tool-level `description` sum | 17,683 | 10% |
| All schema `description` strings (input and output schemas) | 73,444 | 43% |
| Schema structure with every schema `description` removed (names, types, tool descriptions kept) | 81,024 | 47% |

Three independent causes account for the bulk:

1. **Per-tool duplication of `$defs`.** MCP requires each tool's `inputSchema` and `outputSchema` to
   be a self-contained schema, so the schema generator inlines every referenced definition into
   every tool. There are 47 unique definitions, but `Position2D` and `PositionDegradation` appear in
   25 tools each, `Range` in 24, `ResolvedSymbol` and `PositionSource` in 7, and `EnclosingSymbol`,
   `EnclosingSymbolOutcome` and `EnrichmentSummary` in 6. `PositionDegradation` alone costs 12,650 B
   across the list; `Position2D` 8,050 B; `Range` 6,000 B.
2. **Rustdoc prose leaking into schemas.** Schema `description` fields are derived from type and
   field doc comments. Ten descriptions embed a full `# Examples` section with doctest code
   (for example `ResultContext` in `get_definition`'s `inputSchema`, and the
   `TypeHierarchyItemResult` description in `get_supertypes`); about 5.9 KB of description text
   contains code fences. Multi-paragraph field docs such as `out_of_workspace` (1,001 B) are
   repeated verbatim in five tools.
3. **Long-tail schemars noise.** Redundant keywords such as `format: "uint32"` plus
   `minimum: 0` on every integer field, and `oneOf` of per-variant `const` entries for plain
   string enums.

The five largest tools (`get_references` 12.5 KB, `go_to_type_definition` 12.4 KB,
`go_to_implementation` 12.4 KB, `get_definition` 12.4 KB, `get_diagnostics` 9.6 KB) are the five
that carry `context` enrichment plus `positions_degraded` plus `resolved_symbol`.

Schema shape is already tuned in places: `get_server_logs`' `min_level` is hand-flattened to avoid
`$ref` ([[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]] FR-013), and structured tool output
(`outputSchema` plus `structuredContent`) was introduced deliberately (#546). This spec builds on
both and does not revisit them.

### Goal

`tools/list` stays under a stated, test-enforced size budget, per tool and in total, without removing
any tool, parameter, result field, `outputSchema` or `structuredContent`, and without producing
schemas that a client unable to resolve `$ref` or `$defs` cannot read.

### Out of Scope

- Dropping `outputSchema` or `structuredContent` from any tool (non-goal; #546 is a deliberate
  decision).
- Removing, renaming or retyping any tool, input parameter, or result field.
- Making `tools/list` dynamic (per-session or per-client filtering, lazy or paginated listing,
  deferred tool loading). Capability-aware surfacing is covered by
  [[mcp/005-tool-capability-discoverability/spec|mcp/005]].
- Weakening rustdoc. Doc comments with `# Examples` and doctests remain mandatory on public items;
  only their projection into the wire schema changes.
- Shrinking the tool-level `description` strings below their current informational content; they are
  the primary guidance a client reads and are counted in the budget only as a ceiling against growth.
- Compressing the transport (HTTP `Content-Encoding`); not a property of the payload.
- Technical design: recorded in [[mcp/014-tools-list-payload-size/plan|plan]].

## 2. User Stories

### US-001: MCP client operator pays less context per session

AS AN operator of an MCP client that puts tool definitions into a model context
I WANT the `tools/list` response to be a fraction of today's 171 KB
SO THAT the fixed per-session token cost of enabling mcpls drops and leaves room for the task.

**Acceptance criteria:**
```
GIVEN the default unprefixed tool surface
WHEN tools/list is serialized compactly
THEN its byte length is at most the total budget in FR-001
  AND no single tool exceeds the per-tool budget in FR-002
```

### US-002: Agent still reads accurate, complete result schemas

AS AN AI coding agent reading a tool's `outputSchema`
I WANT every result field to remain declared with its type and a short, accurate description
SO THAT I can interpret `structuredContent` correctly (for example what `positions_degraded:
"request"` means) after the schemas have been shrunk.

**Acceptance criteria:**
```
GIVEN a tool whose result can carry positions_degraded
WHEN the agent reads the tool description and outputSchema
THEN the meaning of both "request" and "response" is stated at least once, in the tool description
  or in a schema description
  AND the field set, required list and types are identical to the pre-change schema
```

### US-003: Client without `$ref` support keeps working

AS AN author of an MCP client or schema validator that does not resolve `$ref` across `$defs`
I WANT shrinking not to introduce any new indirection mechanism
SO THAT a schema I can read today I can still read after the change.

**Acceptance criteria:**
```
GIVEN any tool inputSchema or outputSchema after the change
WHEN every $ref in it is collected
THEN each resolves to a $defs entry inside the same schema document
  AND no $ref targets another tool, an external URI, or a $id
  AND any type that was fully inline before remains fully inline
```

### US-004: Maintainer is stopped from regressing the budget silently

AS A mcpls maintainer adding a tool or a result type
I WANT CI to fail when `tools/list` grows past the budget or rustdoc code leaks into a schema
SO THAT the payload does not creep back up one doc comment at a time.

**Acceptance criteria:**
```
GIVEN a change that adds a doc comment containing a code fence to a schema-visible type
WHEN the unit test suite runs
THEN a test fails naming the tool, schema path and offending text
GIVEN a change that pushes the total or any tool past its budget
WHEN the unit test suite runs
THEN a test fails reporting the measured and permitted byte counts per tool
```

## 3. Functional Requirements

Priorities: `must` / `should` / `may`. Byte counts are UTF-8 bytes of the compact
`serde_json` serialization (no extra whitespace) of the `Tool` values returned by
`build_tool_router(None).list_all()`.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE serialized `tools` array SHALL NOT exceed the total budget: 135,000 B (baseline 171,465 B; stretch target 110,000 B) (135,000 B is the enforced figure, raised from 130,000 B for the diagnostics `availability`/`origin` fields, the `indexing_in_progress` flag on four more tools, the `push_only` route status and the inlined kind-filter enums of batch 27; the stretch target is not tracked) | must |
| FR-002 | EACH serialized `Tool` SHALL NOT exceed the per-tool budget: 9,000 B (baseline maximum 12,458 B, `get_references`) (9,000 B enforced for every tool; no per-tool override) | must |
| FR-003 | THE sum of all `description` string bytes inside every `inputSchema` and `outputSchema` SHALL NOT exceed 35,000 B (baseline 73,444 B) | should |
| FR-004 | THE system SHALL NOT emit, in any `inputSchema` or `outputSchema` `description`, a Markdown code fence (a line starting with three backticks), a Markdown heading line (a line starting with `#`), or a rustdoc section such as `# Examples`, `# Errors` or `# Panics` | must |
| FR-005 | WHEN a definition is referenced by more than one tool THE system SHALL carry its long-form description at most once in `tools/list` (or in a shortened form per use) rather than the full text in every tool, subject to FR-009 | must |
| FR-006 | WHEN a definition is small and non-recursive (for example `Position2D`, `PositionDegradation`, `Range`) THE system MAY inline it at its use site instead of emitting `$defs` plus `$ref`, WHERE the measured total is not larger than keeping the `$ref` | may |
| FR-007 | WHERE a definition is recursive (today only `Symbol` in document-symbol results) THE system SHALL keep it as a `$defs` entry referenced by `$ref` inside the same schema document | must |
| FR-008 | EVERY `$ref` in a published tool schema SHALL be a same-document JSON Pointer of the form `#/$defs/<Name>` that resolves to an existing entry; THE system SHALL NOT introduce a cross-tool reference, an external URI, a `$id`, or a shared top-level definitions block | must |
| FR-009 | THE shrinking SHALL NOT change the validation semantics of any schema: for every tool, the set of property names, the `required` list, property types, enum and `const` values, `oneOf`/`anyOf` structure and nullability SHALL be identical before and after; only annotation keywords (`description`, `title`, `examples`, `default` where redundant, `format` and `minimum` on unsigned integers) MAY be dropped or shortened | must |
| FR-010 | WHEN a shortened or removed description carried behaviour a client must know to interpret a result (the meaning of `positions_degraded` values, `truncated`, `out_of_workspace`, enrichment `status` values) THE system SHALL keep that statement at least once per tool, either in the tool `description` or in the schema, in one canonical wording | must |
| FR-011 | THE rustdoc on every public type and field SHALL remain unchanged in substance (including `# Examples` and doctests); the projection into wire schemas SHALL be done at schema-generation time, not by deleting or rewriting the documentation | must |
| FR-012 | THE system SHALL provide a unit test that computes the per-tool and total serialized sizes of `build_tool_router(None).list_all()` and fails when FR-001 or FR-002 is violated, reporting the measured and permitted byte counts for every offending tool | must |
| FR-013 | THE system SHALL provide a unit test that walks every schema `description` and fails on a FR-004 violation, reporting tool name, JSON path and the first offending line | must |
| FR-014 | THE system SHALL provide a unit test that collects every `$ref` in every tool schema and asserts FR-007 and FR-008, and that each recursive definition is the only one left as `$ref` when FR-006 inlining is enabled | must |
| FR-015 | THE system SHALL provide a unit test that asserts, for every tool, that the pre-change structural fingerprint (property names, `required`, types, enum values, nullability) equals the post-change one, so FR-009 is checked mechanically rather than by review (the test shapes the same tree and compares it with the unshaped router, so no fixture is committed; it asserts that everything except string `description` and `title` annotations is identical) | should |
| FR-016 | WHEN the schema shaping is applied THE system SHALL apply it in one place on the assembled router, so that the golden snapshot `tool_surface.json`, the prefixed router (`test_build_tool_router_with_prefix_renames_only_name`) and the e2e `tools/list` path all observe identical schemas | must |
| FR-017 | WHEN `tool_surface.json` is regenerated for this change THE system SHALL keep the existing golden test (`test_tool_surface_matches_golden_snapshot`) as the guard against unintended further drift | must |
| FR-018 | THE system SHOULD provide a repeatable way to print per-tool and total byte counts (for example the existing `dump_tool_surface` style `#[ignore]` test or a bench helper) so the baseline and budget can be re-measured without ad hoc scripts | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Protocol compatibility | `tools/list` SHALL remain a valid MCP response; every schema SHALL remain valid JSON Schema 2020-12 (the dialect the schema generator emits today) and every `outputSchema` SHALL still validate the `structuredContent` the tool returns |
| NFR-002 | Client interoperability | Schemas SHALL remain self-contained per tool (FR-008); a client with no `$ref` resolver SHALL be no worse off than today, and SHOULD be better off wherever FR-006 inlines a definition |
| NFR-003 | Behavioural non-regression | Tool dispatch, parameter deserialization, error text and `structuredContent` bytes SHALL be unchanged; the change affects only the advertised schemas and descriptions |
| NFR-004 | Maintainability | THE budget constants SHALL be named constants in the module of the test that enforces them, with the measured baseline recorded in this spec, so raising a budget is a deliberate, reviewable change |
| NFR-005 | Determinism | THE shaping SHALL be deterministic and independent of configuration, environment and tool-name prefix, so the golden snapshot is stable |
| NFR-006 | Startup cost | THE shaping SHALL run once at router construction and add no per-request cost to `tools/list` or `tools/call` |
| NFR-007 | Type safety | THE shaping SHALL operate on typed schema structures where the schema generator exposes them, and SHALL NOT pattern-match on substrings of serialized JSON text |
| NFR-008 | Documentation | `docs/user-guide/tools-reference.md` SHALL stay accurate; any statement dropped from a schema description SHALL still be present there or in the tool description |

## 5. Data Model

No new persistent data. Entities touched:

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| `Tool` (MCP) | One `tools/list` entry | `name`, `title`, `description`, `inputSchema`, `outputSchema`, `annotations` |
| Tool input schema | JSON Schema 2020-12 document derived from the parameter type | `properties`, `required`, optional `$defs` (15 `$ref` across 7 tools today) |
| Tool output schema | JSON Schema 2020-12 document derived from the result type | `properties`, `required`, `$defs` (all 29 tools) |
| Shared definition | A named `$defs` entry reused across tools | Name, recursion flag, per-tool copy count (`Position2D` 25, `PositionDegradation` 25, `Range` 24) |
| Size budget | The enforced limits | Total bytes, per-tool bytes, schema-description bytes |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| A new tool or result type is added and exceeds its per-tool budget | The budget test fails with the offending tool and byte counts; the author trims the schema or raises the constant in a reviewed change |
| A doc comment on a schema-visible type gains a code fence or `# Examples` | The description lint (FR-013) fails naming the path; the stripping layer (FR-004) normally removes it before it reaches the wire, so the test guards the stripper itself |
| Description is cut in the middle of a sentence or Markdown construct | Truncation happens only at a paragraph boundary or sentence end; an unterminated code fence or backtick span is never emitted |
| A type is recursive (`Symbol`) | Stays as `$defs` plus `$ref` (FR-007); inlining it would not terminate |
| A shared definition is inlined (FR-006) and referenced twice in one schema (for example `Range` containing two `Position2D`) | Both copies appear; the measured total, not the intent, decides whether inlining is kept (FR-006) |
| A type with a hand-flattened schema, such as `min_level` (`#[schemars(inline)]`) | Left as is; shaping must be idempotent over already-shaped schemas |
| A configured tool-name prefix is set | Only `name` changes (existing prefix test); the budget is measured on the unprefixed router, and the prefix adds at most `29 x (prefix + 1)` bytes (the budget test does not assert a worst-case prefix) |
| A client strict about `format` keywords sees `uint32` removed | Behaviour is unchanged for validation (`minimum` and `format` are annotations on top of `type: integer`); FR-009 allows dropping them only if the integer type and non-negativity remain expressible without them (resolved: keep both `format` and `minimum`) |
| The e2e wire payload differs slightly from the in-process serialization | The in-process figure is authoritative for the budget; an e2e check records the wire length as information, not as a gate, to avoid transport-framing flakiness |

## 7. Success Criteria

Baseline values are those measured at commit `517cb53`.

| ID | Metric | Baseline | Target |
|----|--------|----------|--------|
| SC-001 | Total serialized `tools` array (compact) | 171,465 B | at most 135,000 B (stretch 110,000 B) |
| SC-002 | Largest single `Tool` | 12,458 B (`get_references`) | at most 9,000 B |
| SC-003 | Sum of schema `description` bytes | 73,444 B | at most 35,000 B |
| SC-004 | Schema descriptions containing a code fence or rustdoc section heading | 10 (about 5.9 KB) | 0 |
| SC-005 | Copies of a shared definition's long-form description in `tools/list` | up to 25 (`PositionDegradation`) | 1 per distinct text, or a shortened form per use |
| SC-006 | `$ref` that does not resolve inside its own schema document | 0 | 0 |
| SC-007 | Tools, input parameters, result fields, `outputSchema`s lost | 0 | 0 |
| SC-008 | Structural fingerprint differences (properties, required, types, enums, nullability) | n/a | 0 |
| SC-009 | CI tests enforcing the budget, the description lint, `$ref` resolvability and the fingerprint | 0 | 4 |
| SC-010 | Estimated token cost of `tools/list` (about 4 bytes per token) | about 43k | at most 33k |

> [!note] Feasibility
> A throwaway measurement on the golden snapshot (not a design) showed that keeping only the first
> paragraph of each schema description, capping it at 160 B, and dropping the long descriptions of
> definitions shared by two or more tools brings the total to about 120 KB, the largest tool to
> about 7.9 KB, and schema description bytes to about 23 KB. The 135,000 B, 9,000 B and 35,000 B
> budgets leave roughly 8% to 15% headroom over that figure. Going below about 110 KB needs
> structural change (inlining small definitions, dropping redundant integer keywords, flattening
> string enums), not description trimming alone.

## 8. Agent Boundaries

### Always (without asking)
- Re-measure with the in-process `list_all()` serialization before and after any schema change and record the figures.
- Keep rustdoc, `# Examples` and doctests intact on public items; shape the wire schema, not the docs.
- Regenerate `tool_surface.json` only for the intended, reviewed shrink, and diff its structure to confirm FR-009.
- Run the commands in the project's "Before Every Commit" section, including `cargo nextest run --workspace --all-features --lib --bins`.

### Ask First
- Raising any budget constant above the values in FR-001 to FR-003.
- Dropping a keyword that a strict validator could rely on (`format`, `minimum`, `default`).
- Changing a result field's wire shape (for example turning a `oneOf` of constants into a plain string enum) in a way that is observable in `structuredContent`.
- Adding a dependency to implement shaping.

### Never
- Remove or weaken `outputSchema` or `structuredContent` (#546).
- Introduce a cross-tool `$ref`, an external `$ref`, a `$id`, or a shared top-level definitions block (FR-008).
- Delete or truncate public rustdoc to hit the budget (FR-011).
- Match on substrings of serialized JSON to rewrite schemas (NFR-007).
- Make `tools/list` depend on configuration, session state or client identity.

## 9. Open Questions

> [!success] Resolved
> - Issue number: #630. The change is not breaking: tools, parameters, result fields and schemas keep their structure; only `description` annotations shrink.
> - Budgets: 135,000 B total, 9,000 B per tool, 35,000 B schema description bytes, enforced; the 110,000 B stretch target is not tracked.
> - Phase 3 (dropping `format` or `minimum`, inlining small definitions) is not applied: `format` and `minimum` are kept, because phases 1 and 2 meet the budgets.
> - The structural check compares the shaped router with the unshaped router of the same tree.
> - A definition carried by two or more tools keeps its own description only when it fits in 80 B, and its nested descriptions only when they fit in 24 B; the meaning of `positions_degraded`, `truncated`, `out_of_workspace` and the enrichment statuses lives in the tool descriptions (`out_of_workspace` through a shared sentence added to the 15 tools that carry it).
> - Measured after #617 and #618 (typed hierarchy item inputs grew the unshaped surface): 181,350 B unshaped, 121,744 B shaped (6.4% headroom), largest tool 7,564 B, schema description bytes 26,995 B.

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[mcp/014-tools-list-payload-size/plan|plan]] — technical plan for this spec
- [[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]] — tool surface, golden snapshot, and the `min_level` flattening (FR-013)
- [[mcp/005-tool-capability-discoverability/spec|mcp/005]] — `tools/list` shape constraints (NFR-001) that this spec preserves
- #546 — structured tool output (`outputSchema` plus `structuredContent`), a deliberate non-goal to revert
