---
aliases:
  - Client publishDiagnostics capability
  - TypeScript diagnostics never published
tags:
  - sdd
  - spec
  - bug
  - lsp-bridge
  - handshake
  - diagnostics
  - typescript
created: 2026-10-06
status: draft
related:
  - "[[constitution]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001-lsp-server-lifecycle-and-respawn]]"
  - "[[lsp/010-workspace-configuration-push/spec|lsp/010-workspace-configuration-push]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp/002-mcp-resources-diagnostics]]"
  - "[[bridge/004-get-diagnostics-flycheck-gap/spec|bridge/004-get-diagnostics-flycheck-gap]]"
  - "[[bridge/011-push-only-server-diagnostics/spec|bridge/011-push-only-server-diagnostics]]"
  - "[[config/002-typescript-7-native-server-support/spec|config/002-typescript-7-native-server-support]]"
---

# Feature: Advertise `textDocument.publishDiagnostics` so push-model servers deliver diagnostics

> [!info] Metadata
> **Type**: bug (handshake gap)
> **Priority**: P1
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle, live-test finding
> **Issue**: #665

> [!abstract]
> The `initialize` request that mcpls sends does not declare the `textDocument.publishDiagnostics`
> client capability. `typescript-language-server` switches its entire diagnostics feature off when
> the capability is absent, so a TypeScript or JavaScript file with a type error reads as clean
> through every diagnostics surface mcpls offers. This spec makes the client declare the capability
> so that every push-model server delivers diagnostics.

## 1. Overview

### Problem Statement

The client capabilities built for `initialize` (`crates/mcpls-core/src/lsp/lifecycle.rs`, the
`text_document` block) declare `documentSymbol`, `hover`, `definition`, `declaration`,
`references`, `rename`, `foldingRange`, `selectionRange` and `codeAction`. They declare neither
`publishDiagnostics` nor `diagnostic`.

`typescript-language-server` gates its whole diagnostics feature on the first of these: it sets its
internal diagnostics support to "the client sent a `publishDiagnostics` capability object". Without
it the server never computes or publishes a diagnostic, and it has no pull provider to fall back
on. rust-analyzer, clangd, pyright and gopls publish regardless of the capability, which is why the
gap went unnoticed.

**Reproduced live (typescript-language-server 5.1.3 and 6.0.1).**

- Direct LSP session, file `export const n: number = "str";`:
  - capability absent: 0 `publishDiagnostics` notifications;
  - capability present as an empty object: 1 notification carrying
    `Type 'string' is not assignable to type 'number'.` and the unused-variable hint.
- Through mcpls with the default TypeScript entry: `get_diagnostics` fails with `-32603`
  `Unhandled method textDocument/diagnostic` (the server has no pull provider), and
  `get_cached_diagnostics` stays `{"diagnostics": []}` for 18 s even after the file was opened by a
  hover. A TypeScript file with a type error therefore looks clean.
- `resources/subscribe` on a TypeScript or JavaScript diagnostics resource never receives an
  update, for the same reason.

The failure is silent for the caller. An agent running its verify loop against TypeScript concludes
that its change compiles, which is the same class of defect as the silent omission in
[[bridge/004-get-diagnostics-flycheck-gap/spec|bridge/004]]. The separate defect that
`get_diagnostics` errors on any push-only server, even after this fix, is covered by
[[bridge/011-push-only-server-diagnostics/spec|bridge/011]].

### Goal

Every language server that decides whether to publish diagnostics from the client's declared
capabilities receives an explicit `textDocument.publishDiagnostics` capability, so that a TypeScript
or JavaScript file with a type error yields diagnostics through `get_cached_diagnostics`,
`get_diagnostics` (via the push cache) and diagnostics resources, with no regression for the four
servers that already publish.

### Out of Scope

- The pull-model `textDocument/diagnostic` client capability and `workspace/diagnostic`: the
  servers mcpls targets either offer pull on their own or do not offer it; whether to also declare
  pull support is a separate question.
- How `get_diagnostics` behaves for a server that has no pull provider
  ([[bridge/011-push-only-server-diagnostics/spec|bridge/011]]).
- Extending the MCP diagnostics shape (tags, related information, code descriptions); this spec
  only keeps what the client declares consistent with what the shape can carry.
- Dynamic registration of diagnostics capabilities.
- Technical design: recorded in a plan after this spec is approved.

## 2. User Stories

### US-001: TypeScript type errors are reported

AS AN AI coding agent editing TypeScript
I WANT diagnostics for a file with a type error to appear in the cached diagnostics and in
`get_diagnostics`
SO THAT my verify loop catches the error instead of reading the file as clean.

**Acceptance criteria:**
```
GIVEN the default TypeScript server entry and a file containing `export const n: number = "str";`
WHEN the file has been opened (for example by a hover) and the server has had time to analyze it
THEN get_cached_diagnostics returns an error whose message states that type 'string' is not
     assignable to type 'number'
```

### US-002: Existing servers keep their behavior

AS A user of rust-analyzer, clangd, pyright or gopls
I WANT diagnostics to be identical to those of the previous build
SO THAT declaring the capability changes nothing for servers that already publish.

**Acceptance criteria:**
```
GIVEN a file with a known error for each of the four servers
WHEN diagnostics are read before and after the change
THEN the set of reported diagnostics (range, severity, code, message) is the same
```

### US-003: Diagnostics subscriptions work for TypeScript

AS A client that subscribes to a file's diagnostics resource
I WANT `resources/updated` to fire when the TypeScript server publishes
SO THAT I learn about a new error without polling.

**Acceptance criteria:**
```
GIVEN a subscription to the diagnostics resource of a TypeScript file
WHEN the server publishes diagnostics for that file
THEN the subscriber receives a resources/updated notification
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE `initialize` request SHALL declare `textDocument.publishDiagnostics` as a present capability object for every server | must |
| FR-002 | THE sub-capabilities inside that object SHALL be limited to what the diagnostics cache and the MCP diagnostics shape can represent; a sub-capability SHALL NOT be declared when mcpls would discard the data it makes the server send | must |
| FR-003 | WHEN the declared capabilities are built THE SYSTEM SHALL build them in the single place that already builds the other `text_document` capabilities, with no per-server special case | must |
| FR-004 | WHEN typescript-language-server is the routed server THE SYSTEM SHALL cache the diagnostics it publishes, so `get_cached_diagnostics` and diagnostics resources reflect a file's type errors | must |
| FR-005 | WHEN rust-analyzer, clangd, pyright or gopls is the routed server THE SYSTEM SHALL produce the same diagnostics as before the change | must |
| FR-006 | THE SYSTEM SHALL NOT declare the pull `textDocument/diagnostic` client capability as part of this change | must |
| FR-007 | A unit test SHALL pin the advertised `publishDiagnostics` capability, so removing or weakening it fails the test suite | must |
| FR-008 | THE testing playbooks under `.local/testing/` SHALL gain a TypeScript diagnostics case (file with a type error, cached and pulled diagnostics, subscription) and the coverage status for diagnostics SHALL be reset | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | The capability is built with the typed `lsp_types` capability structs, not a hand-built JSON value, per [[constitution]] |
| NFR-002 | Compatibility | Servers that ignore the capability see only a larger `initialize` payload; no new request or notification is added to the handshake |
| NFR-003 | Consistency | Whatever the client declares SHALL match what the cache and the DTO retain; adding a field to the diagnostics shape later is the moment to widen the declaration |
| NFR-004 | Load | Declaring the capability may increase the number of diagnostics a server publishes; the cache's existing bounds apply unchanged |
| NFR-005 | Pre-1.0 | Backward compatibility is not a constraint; the visible change (TypeScript now reports diagnostics) is recorded in `CHANGELOG.md` |

## 5. Data Model

No new entities. One declared client capability.

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Client `publishDiagnostics` capability | Declaration in `initialize` that the client accepts `textDocument/publishDiagnostics` | present or absent; sub-capabilities (related information, tags, version, code description, data) |
| Cached diagnostics entry | Existing per-URI entry fed by the push notification | `uri`, `version`, `diagnostics` |
| MCP `Diagnostic` | Existing tool-facing shape | `range`, `severity`, `message`, `code` |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Server that does not read the capability (rust-analyzer, clangd, pyright, gopls) | Unchanged output (FR-005) |
| Server that publishes tagged hints (unused variable) | Delivered as hints through the existing severity mapping; tags are not represented (FR-002) |
| Server that publishes related information | Delivered without the related information if that sub-capability is not declared (FR-002) |
| Server publishes versioned diagnostics | The cache keeps the version it already stores; declaring version support is allowed because the cache holds the field |
| Server publishes a very large diagnostics set | Existing cache bounds apply (NFR-004) |
| TypeScript file that is never opened through mcpls | No diagnostics: the server publishes after `didOpen`, which mcpls sends lazily on first access, as for every server |
| Native TypeScript server (`tsc --lsp --stdio`) | Unchanged: it answers pull; the new capability is accepted and ignored ([[config/002-typescript-7-native-server-support/spec\|config/002]]) |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Live check on typescript-language-server (5.1.3 and 6.0.1) with `export const n: number = "str";` | `get_cached_diagnostics` returns the type error within a bounded time after the file is opened; before the change it stayed empty for 18 s |
| SC-002 | Live no-regression check on rust-analyzer, clangd, pyright and gopls with a known error each | Same diagnostics as the previous build |
| SC-003 | Unit test on the built client capabilities | Asserts `publishDiagnostics` is present and its sub-capabilities equal the decided set |
| SC-004 | Diagnostics resource subscription on a TypeScript file | One `resources/updated` after the server publishes |
| SC-005 | Wire capture of `initialize` | The `publishDiagnostics` capability object is present |

## 8. Agent Boundaries

### Always (without asking)
- Reproduce the finding with a direct LSP script and through mcpls before changing behavior, and keep the repro as a regression case under `.local/testing/`.
- Use the typed `lsp_types` structs for the capability.
- Run the full pre-commit suite and update `CHANGELOG.md` with the PR link.

### Ask First
- Declaring a sub-capability whose data the cache or the DTO would drop.
- Declaring the pull `textDocument/diagnostic` capability.
- Any change to the diagnostics shape returned to MCP clients.

### Never
- Special-case typescript-language-server in the client capabilities.
- Match on a server name to decide whether to declare the capability.
- Change `get_diagnostics` pull or merge behavior as part of this fix.

## 9. Open Questions

- [NEEDS CLARIFICATION: which sub-capabilities to declare. The cache stores the document version, so declaring version support looks consistent. The MCP `Diagnostic` shape carries range, severity, message and code only, so related information, tags, code description and data would be declared and then dropped. Proposed default: declare version support only.]
- [NEEDS CLARIFICATION: does declaring tag support change which hints typescript-language-server sends (unused or deprecated markers as separate hint diagnostics versus tagged diagnostics)? Verify live before deciding; the live proof with an empty capability object already carries the unused-variable hint.]
- [NEEDS CLARIFICATION: do any other built-in servers (for example a server not covered by the four verified ones) also gate on this capability? Check the remaining built-in entries during the live no-regression pass.]

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] — handshake and lifecycle
- [[lsp/010-workspace-configuration-push/spec|lsp/010]] — precedent: a handshake gap that left one server family silent
- [[bridge/004-get-diagnostics-flycheck-gap/spec|bridge/004]] — precedent: silently incomplete diagnostics treated as P1
- [[bridge/011-push-only-server-diagnostics/spec|bridge/011]] — `get_diagnostics` on a server with no pull provider
- [[mcp/002-mcp-resources-diagnostics/spec|mcp/002]] — diagnostics resources and subscriptions fed by the push cache
- Code: `crates/mcpls-core/src/lsp/lifecycle.rs` (client `text_document` capabilities), `crates/mcpls-core/src/bridge/notifications.rs` (diagnostics cache), `crates/mcpls-core/src/bridge/translator/diagnostics.rs` (MCP mapping)
