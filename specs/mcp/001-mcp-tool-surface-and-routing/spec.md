---
aliases:
  - MCP tool surface
  - Multi-server tool routing
  - ToolRouter
tags:
  - sdd
  - spec
  - mcp
  - config
  - bridge
created: 2026-08-05
status: implemented
related:
  - "[[constitution]]"
  - "[[config/001-config-discovery-and-heuristics/spec]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec]]"
---

# Feature: MCP Tool Surface and Multi-Server Request Routing

> [!info] Cross-block note
> This spec is filed under `mcp` (the tool-dispatch surface it documents is an MCP-facing
> concern), but its core data structure (`ToolRouter`/`ToolKind`/`ServerId`) physically lives in
> `crates/mcpls-core/src/config/routing.rs` — see [[config/001-config-discovery-and-heuristics/spec|config/001]]
> for the configuration layer it is built from, and [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]]
> for the server respawn mechanics FR-010 depends on.

> [!info] Metadata
> **Author**: retroactive spec, authored during the `.local/specs/` → `specs/` migration and gap
> analysis (no single originating commit/PR — this documents already-shipped, working
> functionality; the explicit per-tool routing layer specifically references #174 in its own
> source doc comment as the feature that introduced it)
> **Type**: core subsystem
> **Priority**: P1 (retroactive — reflects centrality, not an open defect)

> [!success] Resolution
> This is a retroactive spec: `crates/mcpls-core/src/mcp/server.rs` (20 `#[tool]` handlers plus
> the `resources/*` handlers), `crates/mcpls-core/src/mcp/tools.rs` (parameter schemas),
> `crates/mcpls-core/src/config/routing.rs` (`ToolRouter`, `ServerId`, `ToolKind`), and
> `crates/mcpls-core/src/bridge/translator/routing.rs` (`get_client_for_file`,
> `resolve_client_for_file`, path validation) already implement everything described below.
> [[mcp/002-mcp-resources-diagnostics/spec|spec mcp/002]] already covers the `resources/*` MCP surface in
> detail; this spec covers the *tool* surface (the 20 `#[tool]` handlers) and the request-routing
> layer beneath both, which spec mcp/002 references but does not itself specify.

## 1. Overview

### Problem Statement

mcpls exposes 20 MCP tools spanning navigation (hover, definition, type definition,
implementation, references), mutation-proposing operations (rename, format, code actions — all of
which return a *proposed* edit rather than writing to disk), symbol search (document and
workspace-wide), diagnostics (both pull and cache-only), call hierarchy (prepare/incoming/
outgoing as a linked trio), and cache-only introspection (server logs, server messages,
inlay hints, signature help). Each tool call must be dispatched to the correct LSP server for the
target file's detected language — but "correct server" is not always a 1:1 language→server
mapping: since #174, a single language can have *multiple* configured servers, each explicitly
claiming a disjoint subset of tools (e.g. pyright for hover/definition, a separate linter LSP for
diagnostics only), with at most one implicit catch-all server per language for whatever no server
explicitly claims.

Without an explicit routing layer, two servers configured for the same language would either
silently overwrite each other in every map keyed by server identity, or require every caller to
duplicate "which server handles this tool for this language" logic. Separately, every tool
handler needs consistent path validation (a requested file must be inside a configured workspace
root) and a consistent MCP-response shape (bridge-layer errors mapped to `McpError`), which must
not be duplicated 20 times.

### Goal

Every MCP tool call is dispatched to the single correct LSP server for its target file's language
and the specific tool being invoked (respecting explicit multi-server-per-language routing where
configured), with consistent path validation and error mapping shared across all 20 tool handlers,
and with a server that failed to spawn or crashed never silently receiving a request it cannot
serve.

### Out of Scope

- The `resources/*` (list/read/subscribe/unsubscribe) MCP surface — already
  [[mcp/002-mcp-resources-diagnostics/spec|spec mcp/002]].
- LSP server spawn/initialize/respawn mechanics themselves —
  [[lsp/001-lsp-server-lifecycle-and-respawn/spec|spec lsp/001]].
- Position/range conversion inside each handler — [[bridge/001-position-encoding-layer/spec|spec bridge/001]].
- Adding new MCP tools — this spec documents the routing/dispatch architecture the existing 20
  tools already use, not a proposal for new tools (see
  [[lsp/002-lsp317-missing-tools/spec|spec lsp/002]] for that history).

## 2. User Stories

### US-001: A tool call reaches the one server explicitly configured to handle it

AS A developer running two LSP servers for one language, each explicitly claiming a disjoint set
of tools via `handles`
I WANT each tool call routed to exactly the server that claims it, with the other server never
receiving that tool's requests
SO THAT I can split responsibilities across servers (e.g. a fast linter for diagnostics, a
full-featured server for everything else) without either server seeing requests it declined

**Acceptance criteria:**
```
GIVEN server A claims `handles = ["hover"]` and server B has no `handles` (catch-all) for the
     same language
WHEN a hover tool call arrives for a file in that language
THEN it is routed to server A

WHEN a definition tool call arrives for the same file
THEN it is routed to server B (the catch-all), since A did not claim `definition`
```

### US-002: A tool call for an unclaimed, no-catch-all combination fails clearly, not silently

AS A developer who configured a narrowly-scoped server with no catch-all sibling for its language
I WANT a tool call for a tool nobody claims to fail with a clear "no server for this tool" error
SO THAT I understand my configuration has a gap, rather than the request being silently forwarded
to a server that explicitly declined it

**Acceptance criteria:**
```
GIVEN a single server for a language, explicitly claiming only `handles = ["hover"]`
WHEN a definition tool call arrives for that language
THEN the response reports no server available for that tool/language combination, rather than
     being silently forwarded to the hover-only server
```

### US-003: A tool request for a file outside the workspace is rejected consistently

AS A mcpls operator
I WANT every one of the 20 tool handlers to reject a request for a file outside configured
workspace roots the same way
SO THAT path validation can't accidentally be skipped or duplicated inconsistently across handlers

**Acceptance criteria:**
```
GIVEN configured workspace roots that do not include /etc
WHEN any position- or file-based tool is called with file_path = "/etc/passwd"
THEN the request is rejected with the same PathOutsideWorkspace error regardless of which of the
     20 tools was called
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHALL expose 20 MCP tools covering hover, definition, type definition, implementation, references, diagnostics (pull + cached), rename, completions, signature help, document symbols, workspace symbol search, format document, code actions, call hierarchy (prepare/incoming/outgoing), inlay hints, server logs, and server messages | must |
| FR-002 | THE SYSTEM SHALL resolve, for every routable tool call, the server that should handle it via `ToolRouter`: an explicit `handles` claim wins over a language's catch-all server; if neither exists, the request fails naming the language and tool | must |
| FR-003 | THE SYSTEM SHALL reject a workspace whose applicable server configs contain two servers sharing one `ServerId` (name defaulting to `language_id`), two catch-all servers for one language, or two servers explicitly claiming the same tool for one language, at workspace-scoped validation time (`ToolRouter::from_configs`), distinct from and later than `ServerConfig::validate`'s workspace-independent checks | must |
| FR-004 | WHEN a server fails to spawn or is dropped from the registered set THE SYSTEM SHALL rebind any route pointing at it to that language's live catch-all if one exists, or drop the route entirely (reporting no server available) if not — never silently conscripting a narrowly-scoped live server outside its declared `handles` | must |
| FR-005 | FOR a workspace-wide tool with no per-file language to route by (e.g. `workspace_symbol_search`) THE SYSTEM SHALL resolve in two tiers, in config declaration order: first the first server explicitly claiming the tool, else the first catch-all server — never falling back to "the first server at all" if neither tier matches | must |
| FR-006 | THE SYSTEM SHALL resolve a file's server via its detected language first, falling back to its React base language (`.tsx`→`typescriptreact`→`typescript`, `.jsx`→`javascriptreact`→`javascript`) only if the language itself has no route, so an explicit `typescriptreact` server still wins over a `typescript` fallback when both are configured | must |
| FR-007 | THE SYSTEM SHALL validate every tool-handler's `file_path` against configured workspace roots via one shared function, used by every handler that takes a file path, rather than each handler duplicating validation logic | must |
| FR-008 | THE SYSTEM SHALL map every bridge-layer `Result<T, Error>` to the MCP tool response shape via one shared function, so error formatting stays consistent across all 20 tool handlers | must |
| FR-009 | THE SYSTEM SHALL classify all 20 tools as read-only (`ToolAnnotations`) at the router level, once, rather than repeating an identical annotation block on every `#[tool]` attribute, since every mcpls tool is a query or a proposed-edit generator that never itself writes to disk | must |
| FR-010 | WHEN a tool call resolves to a server whose process has died THE SYSTEM SHALL attempt to respawn it (per [[lsp/001-lsp-server-lifecycle-and-respawn/spec|spec lsp/001]]) before the request is treated as failed | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Consistency | Every tool handler must use the same path-validation and error-mapping helpers — no handler may implement its own divergent validation logic |
| NFR-002 | Extensibility | Adding a new routable MCP tool must require only extending `ToolKind::ALL` and adding one `#[tool]` handler, not restructuring `ToolRouter` itself |
| NFR-003 | Diagnosability | A workspace-configuration routing conflict (duplicate `ServerId`, two catch-alls, duplicate tool claim) must be reported with enough detail (command, args, or other distinguishing fields) to identify which two `[[lsp_servers]]` entries collided, even when neither has an explicit `name` |
| NFR-004 | Determinism | `resolve_any`'s two-tier resolution (explicit claimer, then catch-all) must be deterministic across repeated calls with the same configuration — driven by config declaration order, not map iteration order |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|-----------------|
| `ToolKind` | Every routable MCP tool (excludes cache-only tools that never reach a specific server: `get_cached_diagnostics`, `get_server_logs`, `get_server_messages`) | 15 variants, `ALL: [Self; 15]`, `as_str()` snake_case name |
| `ServerId` | Unique routing identity of a configured server within a workspace | Wraps `String`; a server's explicit `name` if set, else its `language_id` |
| `ToolRouter` | Resolves `(language, tool)` → `ServerId` | Built via `from_configs` (post-heuristics applicable configs), rebound via `rebind_to_registered` (post-spawn registered set) |
| `NoServerReason` | Why `resolve_any` found no server for a workspace-wide tool | `NothingRegistered`, `NoClaimant` |
| `BridgeContext` (`mcp/handlers.rs`) | Shared state every `#[tool]` handler dispatches through | `translator`, `notification_cache`, `workspace_roots`, `subscriptions`, `project_config_ignored`, `mcp` (presentation overrides) |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|--------------------|
| Two servers for one language, one explicit claimer + one catch-all | Explicit claim wins for its claimed tools; catch-all serves everything else for that language |
| One narrowly-scoped server, no catch-all, tool not claimed | `NoServerReason::NoClaimant` — not silently forwarded to the narrow server |
| No server registered at all for a language | `NoServerReason::NothingRegistered` |
| A workspace-wide tool (e.g. `workspace_symbol_search`) with only a narrowly-scoped server configured (no catch-all anywhere) | `resolve_any` returns `NoClaimant`, never falls back to an arbitrary configured server |
| Two `[[lsp_servers]]` entries collide on `ServerId` with neither setting `name` | Error message names both entries by `command`/`args`, not a positional index (which would usually name the wrong array position, since `from_configs` only sees the post-heuristics applicable subset) |
| A server explicitly claiming a tool fails to spawn, and a catch-all sibling is live | That tool's route rebinds to the live catch-all; a warning is logged naming the dead server and the rebind target |
| A server explicitly claiming a tool fails to spawn, no catch-all sibling | The route is dropped entirely (not rebound to some other unrelated server); that tool reports no server available for the language |
| A `.tsx` file with only a plain `typescript` server configured (no dedicated `typescriptreact` server) | Falls back to the `typescript` server via the React base-language fallback |
| A `.tsx` file with both `typescriptreact` and `typescript` servers configured | Routes to the `typescriptreact` server — the exact-match language wins over the fallback |
| A tool call for a file outside every configured workspace root | Rejected with `PathOutsideWorkspace`, identically regardless of which of the 20 tools was called |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | `cargo nextest run -E 'package(mcpls-core) and (test(routing) or test(server))'` | All existing `ToolRouter` and MCP tool-handler unit/integration tests pass |
| SC-002 | A workspace configuring two servers per language across several languages, with a mix of explicit `handles` and catch-alls | Every one of the 20 tools resolves to the correct server per FR-002/FR-005/FR-006 |
| SC-003 | A deliberately conflicting config (duplicate `ServerId`, two catch-alls, or duplicate tool claim) | `ToolRouter::from_configs` rejects it with a message identifying the colliding entries |

## 8. Agent Boundaries

### Always (without asking)
- Extend `ToolKind::ALL` and add a corresponding `#[tool]` handler together when adding a new
  routable MCP tool — never let the two drift out of sync
- Route every new file-path-taking tool handler through the shared path-validation helper
  (NFR-001)
- Preserve the "explicit claim wins over catch-all, never fall back to an arbitrary server" rule
  in any change to `resolve`/`resolve_any`

### Ask First
- Changing the React-variant fallback order (FR-006) — an explicit `typescriptreact`/
  `javascriptreact` server must continue to win over the plain-language fallback
- Adding a workspace-wide tool that needs a *different* resolution strategy than the existing
  two-tier (explicit claimer, then catch-all) approach

### Never
- Let `rebind_to_registered` conscript a narrowly-scoped live server for a tool outside its
  declared `handles` — this would silently violate that server's explicit configuration
- Duplicate path-validation or error-mapping logic inside an individual `#[tool]` handler instead
  of using the shared helpers (NFR-001)

## 9. Open Questions

None — this is a retroactive spec documenting stable, already-shipped, well-tested behavior.

## 10. See Also

- [[constitution]] — project principles (not yet created for this project)
- [[MOC-specs]] — all specifications
- [[mcp/002-mcp-resources-diagnostics/spec|spec mcp/002]] — the `resources/*` MCP surface, a sibling
  concern this spec does not duplicate
- [[lsp/002-lsp317-missing-tools/spec|spec lsp/002]] — history of 4 of the 20 tools this spec documents
  (`get_signature_help`, `go_to_implementation`, `go_to_type_definition`, `get_inlay_hints`)
- [[config/001-config-discovery-and-heuristics/spec|spec config/001]] — `LspServerConfig`/`ServerId` this
  spec's `ToolRouter` is built from
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|spec lsp/001]] — respawn mechanics triggered when a
  routed-to server has crashed (FR-010)
- `crates/mcpls-core/src/mcp/server.rs` — the 20 `#[tool]` handlers, `to_tool_result`,
  `declared_tool_router`
- `crates/mcpls-core/src/mcp/tools.rs` — MCP tool parameter schemas
- `crates/mcpls-core/src/mcp/handlers.rs` — `BridgeContext`
- `crates/mcpls-core/src/config/routing.rs` — `ToolRouter`, `ServerId`, `ToolKind`,
  `NoServerReason`
- `crates/mcpls-core/src/bridge/translator/routing.rs` — `get_client_for_file`,
  `resolve_client_for_file`, `validate_path_against_roots`
