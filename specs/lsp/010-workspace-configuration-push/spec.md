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

# Feature: Push workspace configuration after `initialized`

> [!info] Metadata
> **Type**: bug (handshake gap)
> **Priority**: P1
> **Author**: Andrei G.
> **Issue**: #578, #598
> **Observed at**: `e3d9c66`

## 1. Overview

### Problem Statement

Some language servers hold every request until the client has pushed its configuration with
`workspace/didChangeConfiguration`. pyright and basedpyright are the observed cases: after
`initialize` + `initialized` they answer nothing, so `get_hover` times out on `main`. mcpls never
sent the notification, and it answers `workspace/configuration` with an array of nulls, so such a
server never leaves its waiting state.

### Goal

After `initialized`, mcpls sends `workspace/didChangeConfiguration` once, so servers that wait for a
push start answering, and servers that do not need it are unaffected. The settings are `null` unless
the server has a configured `settings` table (#598), in which case that table is pushed and served
on `workspace/configuration`.

### Out of Scope

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
| FR-001 | WHEN `initialized` has been sent THE SYSTEM SHALL send exactly one `workspace/didChangeConfiguration` before the server is registered, with `params == {"settings": null}` when the server has no `settings` table | must |
| FR-002 | WITHOUT a `settings` table THE settings SHALL be `null`, not `{}`: a server that rebuilds its preferences from an empty settings map (jdtls) would drop its `initialization_options`, while a non-map value is ignored; rust-analyzer skips both `null` and `{}` client configuration, so its `initialization_options` survive | must |
| FR-003 | WHEN the notification cannot be written THE SYSTEM SHALL fail the handshake with `Error::LspInitFailed`, as for `initialized` | must |
| FR-005 | `LspServerConfig.settings` SHALL be a non-empty `LspSettings` object; an empty table, a TOML datetime anywhere in it, and an empty key segment SHALL be rejected at load with a typed error | must |
| FR-006 | THE SYSTEM SHALL expand dotted keys at the top level of `settings` only (`"python.analysis.typeCheckingMode"` becomes nested objects), rejecting a conflicting pair of settings with a typed error; keys inside a setting's value (gopls `"ui.semanticTokens"`, yaml-language-server URL keys) SHALL NOT be altered | must |
| FR-007 | WHEN `settings` is configured THE SYSTEM SHALL push the whole expanded object in the notification of FR-001 | must |
| FR-008 | WHEN `settings` is configured THE SYSTEM SHALL advertise `workspace.configuration` and answer `workspace/configuration` with one entry per item: the section found by walking the dotted `section` over the nested object, `null` when missing, the whole object when `section` is absent; `scopeUri` is never read, so an unparseable one does not fail the request; an item that cannot be interpreted (a non-string `section`, a non-object entry) SHALL get `null`; params without an `items` array SHALL be answered with `-32602` | must |
| FR-009 | WHEN no `settings` is configured THE SYSTEM SHALL NOT advertise `workspace.configuration` and SHALL keep answering it with one `null` per item, whatever the item shape, and with `[]` when `items` is absent or not an array (the reply before #598) | must |
| FR-010 | THE secret-named string leaves of `settings` SHALL be added to the server's redaction set | must |
| FR-004 | A server that dereferences `settings` and logs a handler error for the null value is acceptable as long as later requests succeed | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Compatibility | Without `settings` no new capability is advertised and no request is added; servers that never read the notification see one extra frame. With `settings`, servers that pull `workspace/configuration` after each push (rust-analyzer, jdtls) replace the client configuration that `initialization_options` filled, so a `settings` table without that server's own section (`rust-analyzer`, `java`) drops the options; settings must use the server's own section names (`rust-analyzer.*`, `python.*`, `gopls`, ...) or they are ignored silently; mcpls warns when both are set |
| NFR-002 | Type safety | The params are the typed `DidChangeConfigurationParams`, not a hand-built map |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Server exits right after `initialized` | The write fails or the connection is lost; handled as an init failure with the stderr excerpt |
| Server logs a handler error for the null settings | Ignored; startup continues (FR-004) |
| Server with `initialization_options` and no `settings` | Options are sent in `initialize` and are not overwritten by the push (FR-002) |
| `settings` with `python.x = 1` and `python = { x = 2 }` | Rejected as a conflict (FR-006) |
| `settings = {}` | Rejected (FR-005) |
| `workspace/configuration` item that is not an object (`"x"`, `7`, `null`) | `null` for that item (FR-008) |
| `"" = 1` (empty top-level key) | Rejected as an empty segment (FR-005) |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Wire order seen by a fake server | `initialize`, `initialized`, then `workspace/didChangeConfiguration` with `{"settings": null}` |
| SC-004 | Fake server with `settings` | pushed object equals the expanded table; `workspace/configuration` items get their sections; `workspace.configuration` is advertised only then |
| SC-002 | Live check, pyright and basedpyright | `get_hover` answers in about 1 s; the same build without the push times out |
| SC-003 | Live check, rust-analyzer, typescript-language-server, clangd | hover and diagnostics identical to the previous build, with a non-default `initialization_options` value still honored by rust-analyzer |

## 10. See Also

- [[constitution]] -- project principles
- [[MOC-specs]] -- all specifications
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] -- handshake and lifecycle
- Code: `crates/mcpls-core/src/lsp/lifecycle.rs` (`LspServer::initialize`), `crates/mcpls-core/src/lsp/client.rs` (`workspace_configuration_result`), `crates/mcpls-core/src/config/settings.rs`
