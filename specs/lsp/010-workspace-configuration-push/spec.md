---
aliases:
  - Workspace configuration push
  - didChangeConfiguration after initialized
tags:
  - sdd
  - spec
  - bug
  - lsp-bridge
  - handshake
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp-server-lifecycle-and-respawn]]"
  - "[[lsp/009-incremental-server-registration/spec|incremental-server-registration]]"
---

# Feature: Push an empty workspace configuration after `initialized`

> [!info] Metadata
> **Type**: bug (handshake gap)
> **Priority**: P1
> **Author**: Andrei G.
> **Issue**: #578
> **Observed at**: `e3d9c66`

## 1. Overview

### Problem Statement

Some language servers hold every request until the client has pushed its configuration with
`workspace/didChangeConfiguration`. pyright and basedpyright are the observed cases: after
`initialize` + `initialized` they answer nothing, so `get_hover` times out on `main`. mcpls never
sent the notification, and it answers `workspace/configuration` with an array of nulls, so such a
server never leaves its waiting state.

### Goal

After `initialized`, mcpls sends `workspace/didChangeConfiguration` once, with `settings: null`, so
servers that wait for a push start answering, and servers that do not need it are unaffected.

### Out of Scope

- Pushing configured per-server settings instead of null, and answering `workspace/configuration`
  from them (follow-up #598).
- Dynamic registration of `didChangeConfiguration`.

## 2. User Stories

### US-001: pyright answers right after startup

AS A user of a pyright or basedpyright route
I WANT `get_hover` to answer as soon as the server has initialized
SO THAT I do not wait out a request timeout on the first call

```
GIVEN a configured pyright server
WHEN the handshake completes
THEN the first get_hover answers within about one second
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN `initialized` has been sent THE SYSTEM SHALL send exactly one `workspace/didChangeConfiguration` with `params == {"settings": null}` before the server is registered | must |
| FR-002 | THE settings SHALL be `null`, not `{}`: a server that rebuilds its preferences from an empty settings map (jdtls) would drop its `initialization_options`, while a non-map value is ignored; rust-analyzer skips both `null` and `{}` client configuration, so its `initialization_options` survive | must |
| FR-003 | WHEN the notification cannot be written THE SYSTEM SHALL fail the handshake with `Error::LspInitFailed`, as for `initialized` | must |
| FR-004 | A server that dereferences `settings` and logs a handler error for the null value is acceptable as long as later requests succeed | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Compatibility | No new capability is advertised and no request is added; servers that never read the notification see one extra frame |
| NFR-002 | Type safety | The params are the typed `DidChangeConfigurationParams`, not a hand-built map |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Server exits right after `initialized` | The write fails or the connection is lost; handled as an init failure with the stderr excerpt |
| Server logs a handler error for the null settings | Ignored; startup continues (FR-004) |
| Server with `initialization_options` | Options are sent in `initialize` and are not overwritten by the push (FR-002) |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Wire order seen by a fake server | `initialize`, `initialized`, then `workspace/didChangeConfiguration` with `{"settings": null}` |
| SC-002 | Live check, pyright and basedpyright | `get_hover` answers in about 1 s; the same build without the push times out |
| SC-003 | Live check, rust-analyzer, typescript-language-server, clangd | hover and diagnostics identical to the previous build, with a non-default `initialization_options` value still honored by rust-analyzer |

## 10. See Also

- [[constitution]] -- project principles
- [[MOC-specs]] -- all specifications
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] -- handshake and lifecycle
- Code: `crates/mcpls-core/src/lsp/lifecycle.rs` (`LspServer::initialize`)
