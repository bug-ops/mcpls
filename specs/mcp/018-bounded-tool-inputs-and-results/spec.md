---
aliases:
  - Bounded tool inputs and results
  - Typed tool input bounds
tags:
  - sdd
  - spec
  - mcp
  - bridge
  - input-validation
  - resource-hygiene
created: 2026-10-07
status: implemented
related:
  - "[[constitution]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]]"
  - "[[mcp/011-client-path-boundary-parsing/spec|client-path-boundary-parsing]]"
  - "[[mcp/016-reject-unknown-tool-arguments/spec|reject-unknown-tool-arguments]]"
  - "[[bridge/001-position-encoding-layer/spec|position-encoding-layer]]"
---

# Feature: Bounded Tool Inputs and Results

> [!info] Metadata
> **Type**: enhancement (retroactive, hardening)
> **Priority**: P3
> **Related issues**: #519, #606, #607, #730
> **Baseline commit**: c408aa5 (31 tools)

> [!abstract]
> This is a retroactive spec for behavior shipped between 0.6.0 and 0.7.0. Client-supplied scalar
> inputs are parsed into bounded types at the MCP boundary, a line beyond the document is rejected
> as invalid params, and every tool that expands a server answer into a list reports truncation
> instead of growing without bound.

## 1. Overview

### Problem Statement

A tool argument that reaches a language server unchecked can be meaningless (a zero or huge tab
width, a blank rename target, a line past the end of the file) and a server answer can be arbitrarily
large (a reference search, a call hierarchy, a workspace edit). Before 0.7.0 these cases either
produced a server-specific result that varied between servers or consumed memory proportional to the
answer.

### Goals

- Make illegal input unrepresentable: the parameter types carry the bound, so a handler never sees
  an out-of-range value.
- Report a caller-fault input as invalid params, not as an internal or server error.
- Bound every expanded result with one shared budget and say so in the response when it was cut.

### Out of Scope

- Bounding client strings echoed in error messages ([[mcp/017-bounded-client-string-echoes/spec|mcp/017]]).
- Rejecting unknown argument names ([[mcp/016-reject-unknown-tool-arguments/spec|mcp/016]]).
- Per-server request timeouts and retry policy.

## 2. User Stories

### US-001: Predictable rejection of bad input

AS AN AI client
I WANT a bad argument to fail fast with a message naming the limit
SO THAT I can correct the call without guessing whether the server or mcpls misbehaved

### US-002: Bounded answers

AS AN AI client
I WANT an oversized answer to be cut and flagged
SO THAT one call cannot flood my context or the mcpls process

## 3. Functional Requirements

### Inputs

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE `tab_size` parameter of the formatting tools SHALL be a `TabSize` holding a value from 1 to `MAX_TAB_SIZE` (32), defaulting to 4; any other value SHALL fail deserialization of the call | must |
| FR-002 | WHEN a positioned tool receives a `line` greater than the last line of the tracked document THE SYSTEM SHALL fail with `PositionBeyondDocument`, naming the requested line and the document's last line, classified as invalid params, and SHALL NOT forward the request to the server | must |
| FR-003 | THE SYSTEM SHALL NOT reject a `character` past the end of its line; it is forwarded and the server clamps it (LSP 3.17 `Position`) | must |
| FR-004 | THE rename `new_name` parameter SHALL be a `NewName`: a blank or whitespace-only value and a value longer than `MAX_NEW_NAME_LENGTH` (1,000 bytes) SHALL fail deserialization, and surrounding whitespace of an accepted value SHALL be kept | must |
| FR-005 | THE completion trigger and symbol-addressing targets SHALL be parsed by serde into typed values at the MCP boundary, with typed errors in place of strings: `NewName`, `CompletionTrigger` and `SymbolName` are built with `new` and report `InvalidTextField`/`TextField`, and an empty completion `trigger` is rejected while parsing, as invalid params (#745) | must |
| FR-006 | THE code action kind filters SHALL be closed enums shared with the kinds advertised to servers, and SHALL include `source.fixAll` and `refactor.move` | should |

### Results

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-007 | THE call hierarchy, type hierarchy, inlay hint, location and workspace-edit handlers SHALL expand a server answer through one `ItemBudget`, capped at `MAX_NORMALIZED_LOCATIONS` (10,000) items per call | must |
| FR-008 | THE selection range chain SHALL be bounded by `MAX_SELECTION_CHAIN` (32) levels through the same budget type | must |
| FR-009 | WHEN a budget is exhausted THE response SHALL carry `truncated: true`; a response that was not cut SHALL carry `truncated: false` | must |
| FR-010 | WHEN a workspace edit holds a non-empty `changes` map THE SYSTEM SHALL give it precedence, SHALL tally the `documentChanges` text-document edits it shadows (those naming a URI `changes` does not) in `shadowed_by_changes`, and SHALL skip text document edits that are empty | must |
| FR-011 | WHEN a code action's edits are dropped by the item cap THE per-action `dropped.exceeds_item_cap` SHALL say so | must |
| FR-012 | WHILE an LSP request for a document is outstanding THE document SHALL be pinned so tracker eviction never picks it, for the whole round trip | must |
| FR-013 | THE `get_completions` result SHALL hold at most `MAX_COMPLETION_ITEMS` (100) items for every server; WHEN a server returns more THE SYSTEM SHALL rank them before the cut by how well `filterText` (else label) matches the identifier before the cursor (prefix, then subsequence, then the rest; within a tier by `sortText`, else label), keep the first 100 and set `truncated: true`. At or under 100 items the server's order is unchanged, and the server's `isIncomplete` is not folded into `truncated` (see [[config/004-clangd-launch-defaults-result-limits/spec\|config/004]]) | must |

### Transport input

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-014 | THE stdio transport SHALL bound one incoming JSON-RPC line at `StdioMessageLimit` (4 MiB by default, the bound the HTTP transport applies to a body; not configurable; zero is unrepresentable). The count excludes the terminating `\n`; a line of exactly the limit is accepted | must |
| FR-015 | WHEN a stdio line exceeds the limit and is a request THE SYSTEM SHALL discard it and answer with JSON-RPC error `-32600` carrying the request `id` recovered by a streaming scan of the top-level members (a missing or unusable `id` is answered without an `id` member); WHEN it is a notification THE SYSTEM SHALL discard it without a reply; either way the session stays open and later lines are served | must |
| FR-016 | Lines within the limit SHALL be decoded by the MCP SDK's own codec; a line the codec rejects is handled per FR-017, and every other framing rule is unchanged | must |
| FR-017 | WHEN a stdio line within the limit fails to decode THE SYSTEM SHALL classify it by a streaming scan of its top-level members, as for FR-015. A request (`method` and a fully read `id`, also in a truncated line) that is not JSON or nests too deeply SHALL be answered `-32700` carrying that `id`; one that is JSON but not a JSON-RPC message SHALL be answered `-32600` carrying that `id`. A notification, a response (`result` or `error` without `method`), an `id`-only object and a non-object SHALL keep today's handling: no reply for undecodable JSON, `-32600` without `id` for a non-message. Such lines SHALL be logged at warn at most once a minute. The session stays open | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | Bounds live in the parameter types, per [[constitution]]; handlers contain no range checks for these values |
| NFR-002 | Compatibility | The change is breaking for clients that sent a zero or oversized `tab_size` or a blank `new_name`; recorded in the changelog |
| NFR-003 | Compatibility | The `get_completions` cap and the stdio line limit are breaking for clients relying on larger answers or lines (before, stdio lines over 16 MiB were dropped without a reply); recorded in the changelog |

## 5. Edge Cases and Error Handling

| Case | Behavior |
|------|----------|
| `tab_size` of 0 or 33 | Invalid params naming the accepted range |
| `line` 0 | Rejected by the positional type before the document check |
| `line` equal to the document's last line | Accepted |
| Empty document | Its single empty line is line 1 |
| `new_name` of spaces | Rejected as blank |
| Result of exactly the cap | Not truncated |
| Result above the cap | First items kept, `truncated: true` |
| `get_completions` answer of 101 items | Best 100 by prefix match kept, `truncated: true` |
| Stdio request line of 4 MiB plus one byte | Discarded, `-32600` with the request id, session continues |
| Stdio notification line over 4 MiB | Discarded, no reply, warning logged |
| Stdio request nested 125 levels deep or more (within the limit) | `-32700` with the request id, session continues |
| Stdio request line cut off after its `id` and `method` | `-32700` with the request id |
| Stdio request line that breaks after its `method` and full `id` (cut off, missing comma, unquoted key) | `-32700` with the request id; a line end ends a number id, for LF and CRLF alike |
| Stdio request cut off inside a string `id`, or an oversized line cut inside its `id` | No reply (the id cannot be trusted) |
| Stdio request the SDK skips for compatibility (for example method `notifications/x` with an `id`) | `-32600` with the request id |
| Stdio notification, or a response with `result`/`error`, nested too deeply | No reply |
| Stdio `{"jsonrpc":"2.0","id":1}` (an `id` and nothing else) | `-32600` without an id |
| Stdio request whose `id` key is written with escapes (`"\u0069d"`) | No reply: an escaped key is never matched (known limit) |

## 6. Success Criteria

| ID | Metric |
|----|--------|
| SC-001 | A tool call with a line beyond the document sends no LSP request |
| SC-002 | A 100,000-item server answer yields at most 10,000 items and `truncated: true` |
| SC-003 | Eviction never closes a document with an in-flight request |
| SC-004 | An oversized stdio request is answered with `-32600` and its id, and an in-limit request that does not decode with `-32700` and its id, instead of being dropped |

## 7. Agent Boundaries

### Always

- Add a new bounded scalar as a typed newtype whose constructor enforces the bound.

### Ask first

- Raising a cap, which changes the response size budget.

### Never

- Validate these bounds with an `if` inside a handler instead of the type.

## 8. See Also

- [[mcp/016-reject-unknown-tool-arguments/spec|mcp/016]]
- [[mcp/017-bounded-client-string-echoes/spec|mcp/017]]
- [[bridge/001-position-encoding-layer/spec|bridge/001]]
