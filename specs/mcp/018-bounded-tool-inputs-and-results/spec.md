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
| FR-005 | THE completion trigger and symbol-addressing targets SHALL be parsed by serde into typed values at the MCP boundary, with typed errors in place of strings | must |
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

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | Bounds live in the parameter types, per [[constitution]]; handlers contain no range checks for these values |
| NFR-002 | Compatibility | The change is breaking for clients that sent a zero or oversized `tab_size` or a blank `new_name`; recorded in the changelog |

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

## 6. Success Criteria

| ID | Metric |
|----|--------|
| SC-001 | A tool call with a line beyond the document sends no LSP request |
| SC-002 | A 100,000-item server answer yields at most 10,000 items and `truncated: true` |
| SC-003 | Eviction never closes a document with an in-flight request |

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
