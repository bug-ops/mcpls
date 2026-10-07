---
aliases:
  - Bounded client string echoes
  - Bounded error echoes
tags:
  - sdd
  - spec
  - mcp
  - hardening
  - error-handling
  - logging
created: 2026-10-07
status: draft
related:
  - "[[constitution]]"
  - "[[mcp/011-client-path-boundary-parsing/spec|mcp/011-client-path-boundary-parsing]]"
  - "[[mcp/016-reject-unknown-tool-arguments/spec|mcp/016-reject-unknown-tool-arguments]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp/002-mcp-resources-diagnostics]]"
  - "[[runtime/004-server-text-hygiene/spec|runtime/004-server-text-hygiene]]"
  - "[[runtime/005-untrusted-unlisted-wrapper-workspace-program/spec|runtime/005-untrusted-unlisted-wrapper-workspace-program]]"
---

# Feature: Bound every client-supplied string echoed in an error response

> [!info] Metadata
> **Type**: enhancement (hardening)
> **Priority**: P3
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle, live-test finding at HEAD 09b686c
> **Issue**: #749

> [!abstract]
> Since #713/#716 the unknown-tool-argument rejection echoes a client key as `<N-byte name>` and
> caps the message at 4 KiB, and #725 bounds a wrapped program path in untrusted refusals. Other
> error paths still echo a client-supplied file path, resource URI, pagination cursor or JSON-RPC
> method name at full length. One request therefore produces a JSON-RPC error, and two log lines,
> as long as the request itself (up to the transport body limit). This spec requires every such
> echo to be escaped and bounded with the helpers that already exist, so a response and its log
> line stay under a fixed small size whatever the input size.

## 1. Overview

### Problem Statement

`mcp/016` FR-018 bounded the text echoed for an unknown argument name, and `runtime/005` and the
launcher refusals bound a configured program path with `EchoedPath`. Error variants that carry a
client string for another reason were not given a bound, and their `Display` output is the
JSON-RPC error message verbatim (`map_bridge_error` joins it with its causes through
`ErrorChain`). The `rmcp::service` layer then logs the whole error object at WARN
(`response error id=... error=ErrorData{...}`), so the same text is written again.

**Reproduction (HEAD 09b686c, release build with `--features transport-http`, stdio, Rust language
server or a fake one, `--log-level warn`).** `B` is `'Z' * 1048576` unless stated. `resp_len` is the
byte length of the JSON-RPC error response line.

| Request | Error | `resp_len` |
|---------|-------|-----------|
| `tools/call get_hover {file_path: "/" + B, line: 1, character: 1}` | `-32602 path outside workspace: /ZZZ...` | 1048632 |
| `tools/call get_hover` with a 5000-byte and a 300000-byte `.fk` file name | `malformed file path "<whole path>": File name too long` | 5152 and 300152 |
| `resources/read {uri: "lsp-diagnostics://" + B}` | `-32602 expected 'lsp-diagnostics:///' prefix in URI: non-empty authority in URI: ...` | 1048699 |
| `resources/subscribe`, `resources/unsubscribe` with the same URI | same shape | about 1048.7 K |
| `resources/read {uri: B}` | `-32602 expected 'lsp-diagnostics:///' prefix in URI: ZZZ...` | 1048653 |
| `resources/read {uri: "lsp-diagnostics:///" + B}` | `-32602 path outside workspace: ...` | 1048632 |
| `resources/list {cursor: B}` | `-32602 invalid pagination cursor: ZZZ...` | 1048634 |
| unknown JSON-RPC method `"x/" + B` | `-32601`, message is the method name | 1048609 |

After these calls the log holds lines of 1,048,751 and 1,048,805 bytes, two copies per request.
A path with control characters is escaped (`\u{1b}`) but not bounded.

**Controls (already bounded, responses under 4.2 KiB for a 1 MiB input):** `symbol_name`,
`symbol_kind`, `container`, `kind_filter`, `trigger`, `new_name`, `context` (4171 B), the ids of
`restart_server`, the `workspace_symbol_search` query, and the unknown-argument key.

**Root cause.** The bound is applied per call site, by choosing a bounded type for the field
(`SymbolName`, `ServerId`, `EchoedArgument`, `EchoedPath`) or by `escape_bounded`. The path-bearing
`Error` variants (`FileIo`, `MalformedPath`, `PathOutsideWorkspace`, `NotARegularFile`,
`NoWorkspaceRoots`), the resource URI errors (`ResourceUriError`) and the cursor error in
`paginate_resource_paths` format the raw value, and the unknown-method message comes from the
protocol SDK's default `on_custom_request`.

### Reusable parts

| Part | Where | Role |
|------|-------|------|
| `escape_bounded(text, max_bytes)` | `crates/mcpls-core/src/util.rs` | Escapes control and deceptive characters, then bounds the escaped text between characters and appends `TRUNCATION_MARKER` |
| `EchoedPath` | `crates/mcpls-core/src/error.rs` | Typed echo of a path, bounded to `MAX_ECHOED_PATH_BYTES` (1024, currently private to `error.rs`) |
| `EchoedArgument` | `crates/mcpls-core/src/error.rs` | Typed echo of an option or program name, bounded to `MAX_SYMBOL_NAME_BYTES` (256) |
| `MAX_SYMBOL_NAME_BYTES` | `crates/mcpls-core/src/bridge/translator/addressing.rs` | 256-byte bound for a client name |
| `MAX_ERROR_MESSAGE_CALLER_BYTES` | `crates/mcpls-core/src/util.rs` | 4 KiB cap on a whole message forwarded to the caller |
| `<N-byte name>` replacement | `crates/mcpls-core/src/mcp/parameters.rs` | Shows the original byte length instead of the text |

No new bounding scheme is required; this spec only extends where the existing one is applied (see
FR-002 and the open question on the length marker).

### Goal

Any client-supplied string echoed in an error response is escaped and bounded with the existing
helpers, so the JSON-RPC error and the log lines it produces stay under a fixed size independent of
the input size, and the one echo owned by the protocol SDK (the unknown method name) is either
bounded the same way or explicitly recorded as inherited with a decision.

### Out of Scope

- Bounding the request itself (the HTTP body limit, the stdio line length): owned by `mcp/012` and the transport.
- The DEBUG-level `received request` log line of the protocol layer, which records the whole request at `--log-level debug`; it is opt-in diagnostics, not an echo in an error.
- Strings that originate in an LSP server (owned by `runtime/004` and `MAX_ERROR_MESSAGE_CALLER_BYTES` on `LspServerError`).
- Strings that originate in configuration (owned by `runtime/003` and `runtime/005` through `EchoedArgument` and `EchoedPath`).
- Changing which errors are returned, their JSON-RPC codes, or their classification (`McpErrorKind`).
- Redacting configured secrets (owned by `Redactions`).
- Technical design: to be recorded in a plan after this spec is accepted.

## 2. User Stories

### US-001: A hostile path does not inflate responses and logs

AS AN operator exposing mcpls over HTTP or to an untrusted agent
I WANT an oversized `file_path` to produce a small error and a small log line
SO THAT one request cannot write a megabyte-long line to my log, twice

**Acceptance criteria:**
```
GIVEN a running mcpls with a configured workspace
WHEN get_hover is called with file_path "/" followed by 1 MiB of 'Z'
THEN the call fails with -32602 "path outside workspace"
  AND the response line is at most 4 KiB
  AND the echoed path ends with the truncation marker
  AND no log line written for the request exceeds 4 KiB plus the log prefix
```

### US-002: Resource and pagination errors are bounded

AS A client author probing the resource API
I WANT a malformed URI or cursor to be reported with a bounded echo
SO THAT my error handling sees a message of predictable size

**Acceptance criteria:**
```
GIVEN a running mcpls
WHEN resources/read, resources/subscribe or resources/unsubscribe receive a 1 MiB uri
  OR resources/list receives a 1 MiB cursor
THEN each fails with -32602 naming the cause
  AND each response line is at most 4 KiB
```

### US-003: An unknown method does not echo an unbounded name

AS AN operator
I WANT a JSON-RPC request with a very long unknown method name to be answered with a bounded message
SO THAT the protocol layer's default handler does not reintroduce the problem

**Acceptance criteria:**
```
GIVEN a running mcpls
WHEN a request with method "x/" followed by 1 MiB of 'Z' is sent
THEN the response is -32601
  AND the response line is at most 4 KiB
```

### US-004: Ordinary errors read as before

AS AN agent that reads error text
I WANT an error for a normal path, URI or cursor to be unchanged
SO THAT bounding costs nothing for honest input

**Acceptance criteria:**
```
GIVEN a path, URI or cursor within the bound
WHEN it is rejected
THEN the message is identical to the message before this change
```

### US-005: A maintainer cannot add an unbounded echo unnoticed

AS A mcpls maintainer adding an error variant that carries client text
I WANT a test to fail when its message grows with the input
SO THAT the bound does not erode one variant at a time

## 3. Functional Requirements

Priorities: `must` / `should` / `may`. A "client-supplied string" is text that arrives in a request
(a tool argument, a resource URI, a cursor, a JSON-RPC method name) and is placed in an error
message or `data`.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN an error message includes a client-supplied string THE SYSTEM SHALL escape control and deceptive characters and bound the escaped text, cutting between characters and ending the cut with `TRUNCATION_MARKER`, so the message length no longer depends on the input length beyond the bound | must |
| FR-002 | THE bound SHALL reuse the existing helpers and constants: `escape_bounded` for the cut, `MAX_ECHOED_PATH_BYTES` (1024) for file paths and resource URIs, `MAX_SYMBOL_NAME_BYTES` (256) for a pagination cursor and a method name, and `MAX_ERROR_MESSAGE_CALLER_BYTES` (4 KiB) as the cap on the whole message; no second truncation routine and no new numeric limit SHALL be introduced unless a value above cannot serve, and then it is a named constant next to these | must |
| FR-003 | THE bound SHALL apply to the rendered text of each path-bearing error variant, namely `Error::FileIo`, `Error::MalformedPath`, `Error::PathOutsideWorkspace`, `Error::NotARegularFile` and `Error::NoWorkspaceRoots`, at one place per variant (its `Display`), so every consumer of the message (the JSON-RPC error, `ErrorChain`, logs) is bounded without a per-call-site edit | must |
| FR-004 | THE bound SHALL apply to every `ResourceUriError` variant that carries client text (`InvalidPath`, `InvalidScheme`, `NotAFilePath`), including the nested text of the "non-empty authority" case, for `resources/read`, `resources/subscribe`, `resources/unsubscribe` and the URIs of `subscriptions/listen` | must |
| FR-005 | WHEN `resources/list` receives a cursor that is not a page-start index THE SYSTEM SHALL report `invalid pagination cursor` with the cursor bounded per FR-002 | must |
| FR-006 | WHEN a JSON-RPC request names a method the server does not implement THE SYSTEM SHALL answer `-32601` with a message that contains at most the bounded method name. The default handler of the protocol SDK echoes the method verbatim, so the handler SHALL override `on_custom_request` (or an equivalent hook) to apply FR-002; if no hook can bound it, the inheritance SHALL be documented in the spec's resolved questions and in `SECURITY.md` or the book | must |
| FR-007 | THE JSON-RPC error `message` SHALL be the only place the echo is bounded: because the WARN line of the protocol layer prints the same `ErrorData`, bounding the message SHALL bound that log line; no mcpls-side log statement SHALL print an unbounded client string for these errors | must |
| FR-008 | WHEN a client-supplied string is cut THE SYSTEM SHOULD make the original byte length visible next to the marker, in the manner of `<N-byte name>` in `mcp/016`, so a client can tell a cut value from a short one | should |
| FR-009 | WHEN the client string is within its bound THE SYSTEM SHALL produce the same message text as before this change for ordinary printable input (US-004); the quoting of `{path:?}` in `MalformedPath` and `FileIo` SHALL be preserved | must |
| FR-010 | THE stored value (`PathBuf`, `String`) carried by an error variant SHALL stay the whole client value where other code reads it (for example `Error::FileIo { path, .. }` used by classification); only the rendered text is bounded | must |
| FR-011 | WHEN an echoed string contains control or deceptive format characters THE SYSTEM SHALL escape them exactly as `escape_control` does today, so existing escaping behavior is unchanged and the bound is measured on the escaped text | must |
| FR-012 | THE system SHALL provide a table-driven in-process test that sends a 1 MiB value to each reproduction row in section 1 (tool path, `.fk` name of 5000 and 300000 bytes, each resource method, cursor, unknown method) and asserts the response line is at most `MAX_ERROR_MESSAGE_CALLER_BYTES` plus a fixed envelope allowance, the code is unchanged, and the echo ends with the marker | must |
| FR-013 | THE system SHALL provide unit tests per bounded variant for: input exactly at the bound (not cut), one byte over (cut), a multi-byte character straddling the cut (cut on a character boundary), and a string of control characters (escaped length bounded) | must |
| FR-014 | THE system SHALL provide a regression test that fails when a new `Error` or `ResourceUriError` variant whose `Display` includes client text produces a message that grows with the input, enumerating the variants through a constructor list so a new variant is a compile-time or test-time prompt | should |
| FR-015 | THE system SHALL record in the cycle notes under `.local/testing/` a live reproduction (the table above, run against the release binary with `--log-level warn`) that also measures the longest log line | must |
| FR-016 | `CHANGELOG.md` SHALL record the change under `[Unreleased]` in one line ending with the PR link | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Security | A request SHALL NOT be able to make mcpls emit an error response or WARN log line longer than the fixed bound plus a constant envelope; the ceiling is `MAX_ERROR_MESSAGE_CALLER_BYTES` for the message |
| NFR-002 | Type safety | The bound SHALL be carried by a typed echo (in the manner of `EchoedPath` and `EchoedArgument`), not by an ad-hoc `String` truncation at call sites; an error variant that echoes client text SHOULD hold or render through that type |
| NFR-003 | Simplicity | One bounding routine (`escape_bounded`) and the existing constants; no second scheme, no configuration key and no environment variable for the bound |
| NFR-004 | Behavioural non-regression | Error codes, `McpErrorKind` classification, `data` payloads and the text of messages for input within the bound are unchanged |
| NFR-005 | Performance | The extra work SHALL be O(bound) beyond the allocation of the already-received input; escaping stops at the cut |
| NFR-006 | Portability | Tests SHALL pass on Linux, macOS and Windows; the over-long `.fk` name rows use a name length that is valid-length-exceeding on each platform or assert only the bound, not the OS error text |
| NFR-007 | Documentation | Every new `pub` item carries `///` docs with an `# Examples` doc-test where non-trivial (constitution IV) |

## 5. Data Model

No new persistent data. Entities touched:

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Client string echo | Client text rendered inside an error message | escaped text, bound, cut flag, original byte length |
| Path-bearing `Error` variant | `FileIo`, `MalformedPath`, `PathOutsideWorkspace`, `NotARegularFile`, `NoWorkspaceRoots` | whole stored value, bounded rendered text |
| `ResourceUriError` | Resource URI codec failure | variant, bounded client text |
| Pagination cursor error | `invalid pagination cursor` from `resources/list` | bounded cursor text |
| Unknown-method error | `-32601` answer to an unimplemented method | bounded method name |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Path of exactly the bound | Echoed whole, no marker |
| Path one byte over the bound | Cut with the marker; the cut is between characters |
| Multi-byte character straddling the cut | Cut before the whole character; never a split code point |
| 1 MiB of control characters | Each escapes to several bytes; the escaped text is bounded, so the message stays small |
| Path with an embedded newline or bidi override | Escaped as today; cannot forge a log line |
| Path that includes `"` or `\` in `MalformedPath` | Quoting as today (FR-009) |
| Non-UTF-8 path on Unix | Rendered through lossy conversion, then bounded |
| Resource URI with a very long percent-encoded path | Bounded as a URI string; the decoded path is not rendered unbounded either |
| `NotAFilePath` carrying the rewritten `file://...` string | Bounded like the other URI echoes |
| Message that also carries an I/O cause | `ErrorChain` joins the bounded text with the cause; the cause text (OS error) is already constant-size |
| Several echoes in one message | Each is bounded; the whole message stays under `MAX_ERROR_MESSAGE_CALLER_BYTES` |
| Method name within the bound | Same bare name as before (FR-009) |
| `tools/call` of an unknown tool name | Answered by the router with a constant `tool not found`; nothing is echoed |
| Log level DEBUG | The protocol layer records the whole request at DEBUG; out of scope, opt-in |
| Redaction of configured secrets | Applied after bounding on the same message; unchanged |
| Windows UNC or drive paths | Same bound, measured on the escaped text |

## 7. Success Criteria

| ID | Metric | Baseline | Target |
|----|--------|----------|--------|
| SC-001 | Longest response line for the reproduction rows with a 1 MiB input | 1,048,599 to 1,048,699 B | at most 4 KiB plus envelope for every row |
| SC-002 | Longest log line after the reproduction rows | 1,048,805 B | at most 4 KiB plus the log prefix |
| SC-003 | `.fk` name of 5000 and 300000 bytes | 5152 B and 300152 B | at most 4 KiB plus envelope |
| SC-004 | Messages for input within the bound | n/a | byte-identical to the baseline |
| SC-005 | Echo sites with an unbounded client string (audit of `Error`, `ResourceUriError`, cursor and method name) | 5 variants, 3 `ResourceUriError` variants, 1 cursor, 1 method | 0, or the method recorded as inherited with a decision (FR-006) |
| SC-006 | CI tests covering FR-012, FR-013 | 0 | at least one table test and one unit test per bounded variant |

## 8. Agent Boundaries

### Always (without asking)
- Reuse `escape_bounded` and the existing bound constants before adding anything.
- Keep the whole value stored in the error and bound only the rendered text (FR-010).
- Run the commands in the project's "Before Every Commit" section, including `cargo nextest run --workspace --all-features --lib --bins`.
- Update `CHANGELOG.md` and the testing documents under `.local/testing/` (playbook, coverage-status, process-notes, regression).

### Ask First
- Changing the marker text or `escape_bounded`'s output for existing callers to add the original length (FR-008), because it alters existing refusal messages.
- Introducing a new bound constant or a new echo type.
- Overriding other default handlers of the protocol SDK beyond `on_custom_request`.
- Bounding the DEBUG-level request log line.

### Never
- Truncate with a raw byte index (not on a character boundary) or with a second truncation routine.
- Hide the error cause or change an error code or classification.
- Add an environment variable or config key for the bound.
- Reference or copy another project's behavior in the spec text.

## 9. Open Questions

> [!question] Open
> - [NEEDS CLARIFICATION: Should the original byte length be shown for every cut echo (FR-008)? `escape_bounded` appends only the marker today. Options: (a) extend `escape_bounded` so every caller, including `EchoedPath` and `EchoedArgument`, gains the length; (b) add a thin sibling that appends the length only for the new sites; (c) keep the marker only.]
> - [NEEDS CLARIFICATION: Is one 1024-byte bound right for resource URIs, given that a URI is longer than its path through percent-encoding, or should URIs use a separate named constant?]
> - [NEEDS CLARIFICATION: Should `MAX_ECHOED_PATH_BYTES` become `pub(crate)` in `util.rs` next to `escape_bounded`, so `bridge/` and `mcp/` use the same constant as `error.rs`?]
> - [NEEDS CLARIFICATION: Does `on_custom_request` receive every unknown method of the pinned protocol SDK (3.5.1), including methods that fail to deserialize before reaching the handler? The reproduction shows the default handler, but a malformed envelope may take another path.]
> - [NEEDS CLARIFICATION: Should `Error::InvalidToolParams(String)` and other `String`-carrying variants be audited in this change, or only the variants listed in section 1?]
> - [NEEDS CLARIFICATION: Should the message-level cap `MAX_ERROR_MESSAGE_CALLER_BYTES` also be applied by `map_bridge_error` as a final guard, or is per-echo bounding enough?]

## 10. Documentation Impact

- `CHANGELOG.md` `[Unreleased]`: one line ending with the PR link (FR-016).
- `.local/testing/`: playbook for the reproduction table, `coverage-status.md` row reset, `process-notes.md` note on measuring the longest log line, regression case per bounded echo site.
- `SECURITY.md` or the book error-handling page: only if FR-006 ends as "inherited"; otherwise no change.

## 11. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[mcp/016-reject-unknown-tool-arguments/spec|mcp/016]] — bounded unknown-argument echo (`<N-byte name>`, 4 KiB cap)
- [[mcp/011-client-path-boundary-parsing/spec|mcp/011]] — client path parsing and the path-bearing error variants
- [[mcp/002-mcp-resources-diagnostics/spec|mcp/002]] — resource URIs, subscriptions and pagination
- [[runtime/004-server-text-hygiene/spec|runtime/004]] — escaping and bounding of attacker-influenced text
- [[runtime/005-untrusted-unlisted-wrapper-workspace-program/spec|runtime/005]] — `EchoedPath` in untrusted refusals
- Code: `crates/mcpls-core/src/util.rs` (`escape_bounded`, `MAX_ERROR_MESSAGE_CALLER_BYTES`), `crates/mcpls-core/src/error.rs` (`EchoedPath`, `EchoedArgument`, path-bearing variants, `ErrorChain`), `crates/mcpls-core/src/bridge/resources.rs` (`ResourceUriError`), `crates/mcpls-core/src/mcp/server.rs` (`paginate_resource_paths`, `map_bridge_error`), `crates/mcpls-core/src/mcp/parameters.rs` (`<N-byte name>`)
