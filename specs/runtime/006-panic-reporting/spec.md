---
aliases:
  - Panic reporting
  - Panic hook
tags:
  - sdd
  - spec
  - runtime
  - hardening
created: 2026-11-15
status: implemented
related:
  - "[[constitution]]"
  - "[[runtime/004-server-text-hygiene/spec|runtime/004-server-text-hygiene]]"
  - "[[mcp/017-bounded-client-string-echoes/spec|mcp/017-bounded-client-string-echoes]]"
  - "[[lsp/009-incremental-server-registration/spec|lsp/009-incremental-server-registration]]"
---

# Feature: Escaped, bounded panic reporting

> [!info] Metadata
> **Type**: enhancement (hardening, retroactive)
> **Priority**: P3
> **Issues**: #918, #922, #933 (shipped in #923, #924, #936)

## 1. Overview

### Problem Statement

A panic payload can carry text derived from untrusted input (a path, a server message). The default
panic hook prints it raw to stderr, so it can forge log lines (CWE-117) and is unbounded; caught
task panics were logged through several ad hoc formats.

### Goal

Every panic payload mcpls prints or logs, from the process hook or from a caught background task,
passes through one escaped and bounded type, and a panicking background task never takes the
process down silently.

### Out of Scope

- Panics before logging is initialised keep the default hook (their input is the operator's own command line).
- Backtraces: the hook prints no `RUST_BACKTRACE` output.

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE SYSTEM SHALL represent a panic payload as `PanicMessage` (`util.rs`): the `&str` or `String` payload, else the fixed text `non-string panic payload`, escaped and bounded to `MAX_LOG_STRING_BYTES` (200) once at construction; its `Display` is the only way the text reaches a log or stderr | must |
| FR-002 | THE `mcpls` binary SHALL install a process-wide panic hook after logging is initialised, replacing the default hook | must |
| FR-003 | WHEN a panic occurs THE hook SHALL emit one line with the location (file, line, column) and the `PanicMessage`: through tracing (event `mcpls panicked`, structured fields) when the ERROR level is enabled, else directly to stderr as plain text, also under `--log-format json`; a failed stderr write is ignored | must |
| FR-004 | WHEN a spawned background task panics and the panic is caught THE `TaskPanicked` error SHALL hold a `PanicMessage`, so every caller logs the same escaped, bounded text; the background task kinds are the closed enum `BackgroundTask` | must |
| FR-005 | THE `util::supervise(what, run, cancel, delays)` helper SHALL rerun a task after a caught panic with `RestartBackoff` until cancelled, and the delivery and session supervisors SHALL use it; the backoff's panic log SHALL escape its payload | must |
| FR-006 | WHEN the runtime body panics THE `mcpls` binary SHALL shut the runtime down with a bounded grace period (dropping every task and so every LSP child) and exit with the panicked outcome | must |

## 3. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Safety | The hook never panics |
| NFR-002 | Type safety | No raw payload `String` crosses a module boundary; the text is a `PanicMessage` |

## 4. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Payload with control characters or ANSI escapes | Escaped in the single output line |
| Payload longer than 200 bytes | Cut between characters with the truncation marker |
| `--log-level off` | One plain-text line on stderr |
| Payload that is neither `&str` nor `String` | `non-string panic payload` |

## 5. Agent Boundaries

### Always (without asking)
- Format any panic text through `PanicMessage`.

### Never
- Print or log a raw `info.payload()` or a joined panic's payload.

## 6. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- Code: `crates/mcpls-cli/src/main.rs` (`install_panic_hook`, `report_panic`, `block_on_guarded`), `crates/mcpls-core/src/util.rs` (`PanicMessage`, `TaskPanicked`, `supervise`)
