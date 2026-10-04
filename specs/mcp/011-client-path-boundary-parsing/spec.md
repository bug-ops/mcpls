---
aliases:
  - Client path boundary parsing
  - Invalid file path error classification
tags:
  - sdd
  - spec
  - mcp
  - bridge
  - error-handling
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp-resources-diagnostics]]"
  - "[[bridge/008-workspace-root-configured-spelling/spec|workspace-root-configured-spelling]]"
---

# Feature: Parse Client File Paths at the Boundary and Report Malformed Paths as Invalid Params

> [!info] Metadata
> **Type**: bug
> **Priority**: P2
> **Related issues**: #575 (also touches #479, which classified a missing file as caller-fault)

## 1. Overview

### Problem Statement

A caller-supplied path that can never be valid was reported as JSON-RPC `-32603` (internal error)
while an equally caller-caused missing path was `-32602` (invalid params). `""` failed in
`std::path::absolute`, a NUL byte failed in `canonicalize`, and a path running through a regular
file (`<file>/x`) failed with `ENOTDIR`; all surfaced as `Error::FileIo`, which only classified
`NotFound` as caller-fault. A client may retry or treat these as server bugs.

> [!bug] Reproduction (release binary at `ad90190`)
> `get_hover {"file_path": ""}` gives `-32603 "file I/O error for \"\": cannot make an empty path absolute"`;
> a NUL byte gives `-32603`; `<root>/src/missing.rs` gives `-32602`.

### Goal

Every malformed client path is `-32602`. Shapes that can never name a file are made
unrepresentable by parsing the path once at the edge, and the remaining path-shape IO failures are
classified as caller-fault.

### Out of Scope

- Changing which paths are inside the workspace (see 008) or the `PathOutsideWorkspace` classification.
- Treating permission or other IO failures as caller-fault; they stay internal.
- Validating file contents, size or extension (unchanged).

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a tool receives an empty `file_path` or one containing a NUL byte THE SYSTEM SHALL reject the call before any filesystem access and report JSON-RPC `-32602` with a message naming the cause | must |
| FR-002 | WHEN validating a client path (`validate_path_against_roots`) fails with an IO error of kind `NotADirectory`, `InvalidFilename` or `InvalidInput` THE SYSTEM SHALL report `Error::MalformedPath`, classified as invalid params; `NotFound` stays `Error::FileIo` (invalid params); every other IO kind, and any `FileIo` raised after validation, SHALL stay an internal error | must |
| FR-003 | THE SYSTEM SHALL represent a client path as `ClientPath`, a non-empty, NUL-free path whose only constructor is `TryFrom<PathBuf>` | must |
| FR-004 | Every path-taking tool method (`file_path`, including the optional one of `get_tool_support`) SHALL parse its parameter into a `ClientPath` before use, and `validate_path_against_roots` and the translator handlers SHALL take it. The parameter struct keeps a plain `PathBuf`, because rmcp reports a parameter deserialization failure as a tool-result error (`isError`), not as JSON-RPC `-32602` | must |
| FR-005 | A path decoded from a `lsp-diagnostics:///` URI or a `file://` URI SHALL pass through the same construction; a decoded NUL byte SHALL be an invalid-URI or invalid-params error | must |
| FR-006 | THE SYSTEM SHALL report a rejected path with a dedicated typed error (`Error::InvalidClientPath`) classified as invalid params, not a free-form string error | must |
| FR-007 | THE published tool schema SHALL be unchanged: `file_path` stays `{"type": "string"}` with its existing description | must |

## 3. Error Classification

| Input | Before | After |
|-------|--------|-------|
| `""` | `-32603` (`FileIo`) | `-32602`, rejected when the tool method parses the path |
| NUL byte in path | `-32603` (`FileIo`) | `-32602`, rejected when the tool method parses the path |
| `<file>/x` (`ENOTDIR`) | `-32603` (`FileIo`) | `-32602` (`MalformedPath`) |
| invalid or over-long file name | `-32603` (`FileIo`) | `-32602` (`MalformedPath`) |
| `get_tool_support` with `file_path: ""` | no filter | `-32602`, like every other tool |
| `FileIo` of kind `InvalidInput` raised while reading an already validated file | `-32603` | `-32603` (environmental) |
| missing file | `-32602` | `-32602` |
| permission denied | `-32603` | `-32603` |
| `resources/subscribe` URI decoding to a NUL byte | accepted path with NUL, later `-32603` | `-32602` (invalid URI) |

## 4. Edge Cases

| Scenario | Expected Behavior |
|----------|-------------------|
| Relative path | Accepted by `ClientPath`; resolved and checked by `validate_path_against_roots` as before |
| Path with `..` | Accepted by `ClientPath`; rejected lexically if it leaves the workspace |
| Non-UTF-8 path bytes | Not reachable from JSON; irrelevant to the check |
| Windows reserved name | `InvalidFilename` or `NotFound` from the OS; either is invalid params |
| `get_tool_support` without `file_path` | Unchanged; the optional parameter stays optional |

## 5. Acceptance and Regression-Test Expectations

| ID | Test | Asserts |
|----|------|---------|
| RT-001 | `ClientPath` unit tests: empty, NUL, valid, relative, dotted | FR-001, FR-003 |
| RT-002 | `get_hover` over an in-memory MCP connection with `""`, a NUL byte and `<file>/x` returns JSON-RPC `-32602` | FR-001, FR-002, FR-004 |
| RT-003 | `error.rs`: `MalformedPath` of each IO kind and `InvalidClientPath` are invalid params; permission denied and a post-validation `FileIo` of kind `InvalidInput` stay internal | FR-002, FR-006 |
| RT-004 | `<file>/x` through `validate_path_against_roots` maps to `-32602` | FR-002 |
| RT-005 | `parse_uri` rejects a `%00` URI; subscribe resolution classifies it as invalid params | FR-005 |
| RT-006 | Golden `tool_surface.json` test unchanged by this change | FR-007 |

## 6. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | The three reproduction calls | All `-32602` |
| SC-002 | Tool schema diff caused by this change | None |

## 7. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]] — tool parameters and error mapping
- Code: `crates/mcpls-core/src/bridge/client_path.rs`, `crates/mcpls-core/src/error.rs` (`mcp_error_kind`), `crates/mcpls-core/src/mcp/tools.rs`
