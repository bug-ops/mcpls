---
aliases:
  - Reject unknown tool arguments
  - Strict tool argument names
tags:
  - sdd
  - spec
  - mcp
  - tool-arguments
  - schema
  - error-handling
created: 2026-10-06
status: implemented
related:
  - "[[constitution]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]]"
  - "[[mcp/005-tool-capability-discoverability/spec|tool-capability-discoverability]]"
  - "[[mcp/011-client-path-boundary-parsing/spec|client-path-boundary-parsing]]"
  - "[[mcp/014-tools-list-payload-size/spec|tools-list-payload-size]]"
---

# Feature: Reject Unknown Tool Argument Names

> [!info] Metadata
> **Type**: enhancement
> **Priority**: P3
> **Related issues**: #705
> **Source**: continuous-improvement cycle 040 live test
> **Baseline commit**: 12ee3a1 (31 tools)

## 1. Overview

### Problem Statement

A tool call whose `arguments` object carries a name that is not a parameter of the tool is accepted
and the unknown name is dropped. The parameter structs behind the tools (`crates/mcpls-core/src/mcp/tools.rs`
and the translator parameter types) do not use `#[serde(deny_unknown_fields)]`, and the `tools/list`
`inputSchema` documents carry no `additionalProperties: false`. A client that misspells or guesses an
optional argument name gets a normal answer with no signal that part of its request was ignored.

Verified on the release binary at `12ee3a1` with a Rust language server:

| Call | Result |
|------|--------|
| `get_code_actions` with `kinds: ["bogus"]` (the real name is `kind_filter`) | all 5 actions, identical to `kinds: ["quickfix"]` and `kinds: [""]` |
| `workspace_symbol_search` with `kind: "bogus"` (the real name is `kind_filter`) | unfiltered symbols |
| `workspace_symbol_search` with `kind_filter: "bogus"` | rejected with a deserialization error |
| `restart_server` with `server_ids` instead of `servers` | generic "give `servers` ... or `all: true`" error, which does not name the misspelled field |
| `get_hover` with an arbitrary extra property | accepted |

The behavior is inconsistent: a correct name with a bad value is rejected, while a wrong name with
any value is accepted. MCP clients are mostly LLM agents, which routinely guess argument names. A
silently dropped filter produces a plausible but wrong answer (an unfiltered list presented as a
filtered one), which is worse than an error because the agent has no reason to retry.

### Goal

An argument name that is not declared by the tool's input schema is rejected with an error that names
the unknown field and the accepted ones, and the advertised `inputSchema` of every tool states
`additionalProperties: false`, while `tools/list` stays within the budget of
[[mcp/014-tools-list-payload-size/spec|mcp/014]].

### Out of Scope

- Rejecting unknown fields in tool results (`outputSchema` and `structuredContent` stay open for forward-compatible additions).
- Rejecting unknown fields in the TOML configuration; already covered by `deny_unknown_fields` on the config types.
- Rejecting unknown fields in MCP protocol messages (`initialize`, `resources/*`, `tasks/*`) or in the JSON-RPC envelope; those are owned by the protocol layer.
- Accepting misspelled names through aliases or fuzzy matching (non-goal; see FR-009).
- Changing which tools or parameters exist, or the typed-value validation that already works.
- Technical design: to be recorded in a plan after this spec is accepted.

## 2. User Stories

### US-001: Agent learns immediately that it misspelled a parameter

AS AN AI coding agent calling `get_code_actions`
I WANT a call with `kinds` instead of `kind_filter` to fail with the accepted parameter names
SO THAT I correct the call instead of acting on an unfiltered result I believe is filtered.

**Acceptance criteria:**
```
GIVEN a running mcpls with a Rust language server
WHEN get_code_actions is called with an argument named "kinds"
THEN the call fails as invalid parameters
  AND the message names the unknown field "kinds"
  AND the message lists the accepted fields, including "kind_filter"
  AND no language-server request is sent
```

### US-002: Client author sees the closed schema before calling

AS AN author of an MCP client or schema validator
I WANT every tool `inputSchema` to declare `additionalProperties: false`
SO THAT my client rejects or autocompletes against the real parameter set without calling the tool.

**Acceptance criteria:**
```
GIVEN the tools/list response
WHEN each tool inputSchema is inspected
THEN its top-level object schema has "additionalProperties": false
  AND every nested object schema that is deserialized from client input has "additionalProperties": false
```

### US-003: Existing correct calls keep working

AS AN operator whose client sends only declared parameters
I WANT every currently valid call to behave exactly as before
SO THAT tightening the argument check costs me nothing.

**Acceptance criteria:**
```
GIVEN a call whose arguments are all declared parameters (including optional ones set to null)
WHEN the call is dispatched
THEN the result is byte-identical to the pre-change result
```

### US-004: Maintainer is stopped from adding an open parameter type

AS A mcpls maintainer adding a tool
I WANT CI to fail when a tool parameter type accepts unknown names
SO THAT the guarantee does not erode one new tool at a time.

**Acceptance criteria:**
```
GIVEN a tool whose parameter type lacks the strict-fields attribute
WHEN the unit test suite runs
THEN a test fails naming the tool and the schema path that is missing additionalProperties: false
```

## 3. Functional Requirements

Priorities: `must` / `should` / `may`. "Tool parameters" means the type deserialized from the
`arguments` object of `tools/call`, including nested objects the client supplies (for example the
`context` options and typed item inputs).

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a `tools/call` `arguments` object contains a name that is not a declared parameter of the tool THE SYSTEM SHALL reject the call before any filesystem access, document synchronization or language-server request | must |
| FR-002 | THE rejection SHALL name the first unknown field and list the accepted field names of the object in which it appeared. Only the first unknown field is named: serde reports the first failure and NFR-002 forbids scanning a `Value` for the rest | must |
| FR-003 | THE rejection SHALL use the same error shape as the existing parameter deserialization failures (a tool-result error, `isError: true`, per [[mcp/011-client-path-boundary-parsing/spec\|mcp/011]] FR-004), so a client has one failure path for malformed arguments; the classification SHALL NOT be an internal error | must |
| FR-004 | EVERY tool parameter type, including nested client-supplied object types, SHALL reject unknown fields at deserialization (`#[serde(deny_unknown_fields)]` or an equivalent typed mechanism), so no tool can opt out by omission. `#[serde(flatten)]` is incompatible with it, so the eight flattening parameter types are explicit wire structs; the client-supplied hierarchy item is an input-only `HierarchyItemInput` (with `RangeInput` and `Position2DInput`) sharing the schema names of the output types, so no `outputSchema` is closed; its `data` stays open | must |
| FR-005 | EVERY tool `inputSchema` SHALL declare `"additionalProperties": false` on its top-level object schema and on each nested object schema reachable from it that is built from client input | must |
| FR-006 | THE advertised `additionalProperties: false` SHALL be derived from the same type attribute that enforces FR-004 (one source of truth), not added by hand per tool or by post-processing serialized JSON text | must |
| FR-007 | THE change SHALL NOT alter any property name, `required` list, property type, enum value, nullability or description in any `inputSchema`; the only new keyword is `additionalProperties: false` | must |
| FR-008 | THE `tools/list` payload SHALL remain within the budgets of [[mcp/014-tools-list-payload-size/spec\|mcp/014]] (FR-001, FR-002); the added keyword is about 27 B per object schema and is accounted for in the measured figures. The total budget is raised by the user's decision from 135,000 B to 135,500 B: the measured total after #688, #705 and #700 is 135,143 B (43 keywords, +1,247 B), rounded up to the next 500 B; the per-tool (9,000 B) and description (35,000 B) budgets are unchanged | must |
| FR-009 | THE system SHALL NOT accept aliases, near-miss spellings or legacy names for any parameter; the only accepted names are those in the schema. WHEN the rejected name is within a small edit distance of exactly one accepted name THE SYSTEM MAY add a "did you mean" hint to the message | may |
| FR-010 | WHEN a declared `Option` parameter (nullable in its schema) is present with the JSON value `null` THE SYSTEM SHALL treat it as absent exactly as before; the strict check applies to names, not to null values. A defaulted non-`Option` parameter (`include_declaration`, `context`, `tab_size`, `insert_spaces`, `limit`, `kind`, `all`) has a non-nullable schema, so `null` stays a type error (`isError`), as it was before this change | must |
| FR-011 | WHEN a name is unknown in a nested object (for example a hierarchy item) THE rejection SHALL name that field and the accepted fields of the nested object; it carries no JSON path, because rmcp's `from_value` discards the path and the typed error text is all that survives | should |
| FR-012 | WHEN `restart_server` receives `server_ids` or any other unknown name THE SYSTEM SHALL report the unknown field first, before the "give `servers` or `all: true`" selection error | must |
| FR-013 | THE system SHALL provide a unit test that, for every tool in `build_tool_router(None).list_all()`, asserts FR-005 on the `inputSchema` tree (top-level and every nested object schema), reporting tool name and JSON path for each violation | must |
| FR-014 | THE system SHALL provide a regression test per reproduction in the Problem Statement (`get_code_actions` `kinds`, `workspace_symbol_search` `kind`, `restart_server` `server_ids`, `get_hover` extra property) that asserts the rejection, the field name in the message and that no language-server request was issued | must |
| FR-015 | THE golden snapshot `tool_surface.json` SHALL be regenerated once for this change and the existing golden test SHALL remain the guard against further drift; the regenerated diff SHALL consist of `additionalProperties: false` additions only (FR-007) | must |
| FR-016 | `CHANGELOG.md` SHALL record the change under `[Unreleased]` as a breaking change: clients that send undeclared argument names now receive an error instead of a silently ignored field | must |
| FR-017 | `book/src/tools/overview.md` (the Arguments section) SHALL state that undeclared argument names are rejected and SHALL stay accurate for every parameter name | should |
| FR-018 | THE client text echoed in the rejection SHALL be bounded: an object key longer than `MAX_SYMBOL_NAME_BYTES` (256 B), at any depth, is reported as `<N-byte name>`, and the whole message is capped at 4 KiB (`MAX_ERROR_MESSAGE_CALLER_BYTES`), which also covers string values serde echoes in `invalid type`. The one exception: when the re-parse with renamed keys succeeds, the original message is reported (a rename never turns a failed call into a success) and only the 4 KiB cap bounds it. The `failed to deserialize parameters:` prefix is kept so FR-003 holds | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Protocol compatibility | `tools/list` SHALL remain a valid MCP response and every `inputSchema` SHALL remain valid JSON Schema 2020-12; `additionalProperties: false` is a standard keyword in that dialect |
| NFR-002 | Type safety | Strictness SHALL be enforced by the type system at deserialization (a closed set of named fields), not by a runtime scan of a `serde_json::Value` map for unexpected keys |
| NFR-003 | Payload size | The measured `tools/list` total and every per-tool size SHALL stay under the [[mcp/014-tools-list-payload-size/spec\|mcp/014]] budgets; the expected growth is at most 27 B multiplied by the number of object schemas (31 tools plus nested objects, under 2 KB in total) |
| NFR-004 | Behavioural non-regression | Valid calls (all declared names) SHALL produce identical results, error text and `structuredContent`; only calls with undeclared names change |
| NFR-005 | Determinism | The schema output SHALL be independent of configuration, environment and tool-name prefix, so the golden snapshot is stable |
| NFR-006 | Performance | The strict check SHALL add no measurable per-call cost beyond the existing deserialization |
| NFR-007 | Error quality | The message SHALL be actionable for an LLM client: it names the offending field and the accepted fields in one line, without stack or internal paths |

## 5. Data Model

No new persistent data. Entities touched:

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Tool parameter type | Rust type deserialized from `tools/call` `arguments` | Declared field names, optional fields, strictness attribute |
| Nested parameter type | Client-supplied sub-object (`context` options, typed item inputs, kind filter inputs) | Declared field names, strictness attribute |
| Tool `inputSchema` | JSON Schema 2020-12 document derived from the parameter type | `properties`, `required`, `additionalProperties` |
| Argument rejection | Error returned for an unknown name | Unknown field name(s), accepted field names, nested location |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| MCP client-added metadata `_meta` | Per the MCP specification `_meta` lives in the request `params` next to `name` and `arguments` (including `progressToken` and the per-request protocol metadata in [[mcp/003-mcp-2026-stateless-adoption/spec\|mcp/003]]), not inside `arguments`; it is handled by the protocol layer and is unaffected. A `_meta` key placed inside `arguments` is an unknown name and is rejected; the message lists the accepted fields so the client can move it. Confirmed against rmcp 3.5.1: `CallToolRequestParams` carries `_meta`, `input_responses` and `request_state` as separate fields and only `arguments` reaches the parameter type (`test_request_meta_is_not_an_unknown_argument`) |
| `$schema` key inside `arguments` | Not a parameter; rejected like any unknown name. A `$schema` key in the advertised `inputSchema` document itself is unchanged by this feature |
| Client that echoes extra fields from a previous result back into arguments (for example `uri`, `kind`, `data`) | Rejected with the unknown field named; this is the intended break. The typed item inputs for hierarchy tools already declare the fields they accept and remain unchanged |
| Client that always sends a fixed superset of arguments to every tool | Breaks by design; the client must send only declared names. Recorded as a breaking change (FR-016); backward compatibility is not a constraint before v1.0.0 |
| Several unknown names in one call | Only the first is named (FR-002); the accepted fields are listed, so the client can fix the rest |
| Unknown name together with a bad value for a known name | One rejection for the first failure serde meets; first-failure reporting is accepted (decided with FR-002) |
| Unknown name inside a nested object (item input) | Rejected, naming the field and the accepted fields of that object, without a path (FR-011) |
| Name differing only by case (`Kind_Filter`) | Unknown; rejected. Names are case-sensitive and there are no aliases (FR-009) |
| Renamed parameter in a future release | The old name is unknown and rejected after the rename; no alias window is provided before v1.0.0, and the rename is recorded in `CHANGELOG.md` as a breaking change. After v1.0.0 a deprecation alias would be a separate spec |
| Optional (`Option`) parameter set to `null` | Accepted as absent (FR-010) |
| Defaulted non-`Option` parameter set to `null` | Rejected as a type error, matching its non-nullable schema (FR-010) |
| `get_tool_support` with no `file_path` or other optional name | Unchanged; strictness concerns undeclared names, not omitted optional ones |
| Tool-name prefix configured | Only the tool `name` changes; strictness and schema keyword are identical |
| Client strict about `additionalProperties` in schemas | Now sees the closed object, so a schema-validating client rejects the call locally; this is the intended benefit (US-002) |
| Payload growth pushes a tool over the per-tool budget | The budget test of mcp/014 fails with measured and permitted bytes; the author trims descriptions or raises the constant in a reviewed change. This change raised the total constant to 135,500 B (FR-008) |
| `outputSchema` objects | Not closed by this feature; result types may gain fields compatibly |

## 7. Success Criteria

| ID | Metric | Baseline | Target |
|----|--------|----------|--------|
| SC-001 | The four reproduction calls (`kinds`, `kind`, `server_ids`, extra `get_hover` property) | accepted or generic error | all rejected, unknown field named |
| SC-002 | Tools whose `inputSchema` declares `additionalProperties: false` at top level | 0 of 31 | 31 of 31 |
| SC-003 | Nested client-input object schemas without `additionalProperties: false` | not measured | 0 |
| SC-004 | `tools/list` total and largest tool versus the mcp/014 budgets | within budget | within budget (growth under 2 KB) |
| SC-005 | Valid-call regression (existing e2e and unit suites) | passing | passing, no expectation changes except tests that relied on ignored names |
| SC-006 | Diff of regenerated `tool_surface.json` outside `additionalProperties: false` additions | n/a | none |
| SC-007 | CI tests enforcing the schema invariant (FR-013) and the four reproductions (FR-014) | 0 | at least 5 |

## 8. Agent Boundaries

### Always (without asking)
- Derive the schema keyword from the type attribute so enforcement and advertisement cannot diverge (FR-006).
- Re-measure `tools/list` with the in-process serialization before and after and record the delta against the mcp/014 budgets.
- Regenerate `tool_surface.json` only for the intended change and confirm the diff is `additionalProperties: false` additions only.
- Run the commands in the project's "Before Every Commit" section, including `cargo nextest run --workspace --all-features --lib --bins`.
- Update `CHANGELOG.md` with the breaking-change entry and the testing documents under `.local/testing/`.

### Ask First
- Raising any mcp/014 budget constant to absorb the added keyword (done once, with the user's approval: FR-008).
- Adding an alias, a near-miss correction or a legacy name for any parameter (FR-009).
- Closing `outputSchema` objects or any non-tool-argument type.
- Accepting a specific undeclared key (for example `_meta` inside `arguments`) as a deliberate exception.

### Never
- Enforce strictness by scanning a `serde_json::Value` map for unexpected keys (NFR-002).
- Advertise `additionalProperties: false` without enforcing it, or enforce it without advertising it (FR-006).
- Change any property name, type, `required` list or enum value in an `inputSchema` as part of this change (FR-007).
- Report an unknown argument as an internal error.
- Reference or copy another project's argument-handling behavior in the spec text.

## 9. Open Questions

> [!question] Open
> [!success] Resolved
> - The rejection is the existing tool-result error (`isError: true`), as rmcp reports every parameter deserialization failure (FR-003).
> - `params._meta` and the other request-level fields never reach the parameter deserializer under rmcp 3.5.1 (edge case "MCP client-added metadata").
> - First-failure reporting only; no aggregation (FR-002, FR-011).
> - No "did you mean" hint (FR-009 stays unimplemented).
> - Item types shared between tool arguments and results (`HierarchyItem`, `Range`, `Position2D`) have input-only twins with the same schema names, so the output schemas stay open (FR-004).

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]] — tool surface, golden snapshot and parameter conventions
- [[mcp/014-tools-list-payload-size/spec|mcp/014]] — `tools/list` size budgets that this change must stay inside
- [[mcp/011-client-path-boundary-parsing/spec|mcp/011]] — why parameter deserialization failures are tool-result errors
- [[mcp/005-tool-capability-discoverability/spec|mcp/005]] — `tools/list` shape constraints preserved here
- Code: `crates/mcpls-core/src/mcp/tools.rs`, `crates/mcpls-core/src/mcp/tool_surface.json`
