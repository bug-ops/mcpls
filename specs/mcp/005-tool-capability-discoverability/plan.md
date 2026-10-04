---
aliases:
  - Tool capability discoverability plan
tags:
  - sdd
  - plan
  - mcp
  - discoverability
created: 2026-10-04
status: implemented
related:
  - "[[mcp/005-tool-capability-discoverability/spec|spec]]"
---

# Plan: Tool Capability Discoverability (`get_tool_support`)

> [!info] Metadata
> **Spec**: [[mcp/005-tool-capability-discoverability/spec|tool-capability-discoverability]]
> **Related issues**: #461

## 1. Decision

Add one read-only meta-tool, `get_tool_support`, that reports for every tool and every configured
language whether a call would be dispatched to a server advertising the tool's capability. The user
approved this mechanism (spec section 8, "Ask First", satisfied; SC-001).

`tools/list` is unchanged apart from the new entry, so NFR-001 holds.

## 2. Rejected alternatives

| Alternative | Why rejected |
|-------------|--------------|
| Prune `tools/list` to the intersection of server capabilities | Hides tools that work for some languages (NFR-003) |
| Prune `tools/list` to the union | Still advertises tools that fail for some languages; no discoverability gain |
| Per-tool annotations or `description` text listing servers | Static: goes stale on respawn and late registration (NFR-004); descriptions are pinned by the golden surface snapshot and would churn with config |
| Per-server `get_server_capabilities` dump | Pushes the capability-to-tool mapping onto the agent |

## 3. Design

- **Single source of truth.** `Capability` carries `tool_kind()` and `for_tool()` (exhaustive,
  `Diagnostics -> None`); gated handlers pass only a `Capability` and derive their `ToolKind` from it.
- **Shared decisions.** `lookup_route`, `lookup_workspace_route` and `check_capability` in
  `bridge/translator/routing.rs` are used by enforcement (`client_for_file`, `handle_workspace_symbol`,
  `require_capability`) and by the report, so the two cannot disagree (NFR-002: enforcement is
  unchanged and stays authoritative).
- **Snapshot.** `Translator::tool_support_snapshot` copies `expected_servers`, `lsp_servers`,
  `lsp_clients`, then the router, one lock at a time. Registration writes the client, the server,
  the router rebind, then clears `expected_servers`; reading `expected_servers` first means a
  server mid-registration is never seen as neither expected nor registered. The unit test
  `healthy_server_never_misreported_for_any_write_read_interleaving` checks all 70 interleavings
  of those four writes with the four reads. The snapshot holds the
  router as an `Arc` (copy-on-write on rebind) and each server's advertised `Capability` set, not
  a clone of its `ServerCapabilities`.
- **Report.** `McpTool` (21 variants) declares each tool's name and backend (`Document`,
  `Workspace`, `Local`) in one `spec`. `coverage` is `all`, `some`, `none`, `unknown` or `always`;
  routes carry `supported`, `capability_not_advertised`, `initializing` or `no_server` (NFR-003).
  Document routes group the languages sharing an identical status (`languages` array, first-seen
  order); a workspace route has no `languages`.
- **Staleness.** Computed per query from live registries, so respawns are reflected (NFR-004, FR-003).

## 4. Edge cases (spec section 6)

| Scenario | Report |
|----------|--------|
| No servers configured | `languages: []`; every routed tool `none` |
| Server not yet initialized | `initializing`, coverage `unknown` |
| Language whose servers all failed to spawn | Language still listed; `no_server` |
| Server respawning | Reads the registered client and last-known capabilities |
| Universal support | `all`, `routes` omitted |

## 5. Known limits

- `supported` means "will be dispatched", not "will succeed" (push-only diagnostics, respawn backoff, indexing).
- Dynamic registrations (`client/registerCapability`) are invisible to both the report and `require_capability`.
- A client registered without its `LspServer` reads as `initializing` in the report, while enforcement fails open.

## 6. Verification (SC-002)

- Parity matrix in `mcp/server.rs` tests: for each capability advertised alone (plus none), every
  `McpTool`'s real handler is refused with `CapabilityNotSupported` iff the report says
  `capability_not_advertised`, against a live client (`respawn_if_dead` runs before `require_capability`).
- Catalogue bijection with the macro-generated router; coverage, prefix, failed-spawn,
  pre-registration and snapshot-ordering tests.
