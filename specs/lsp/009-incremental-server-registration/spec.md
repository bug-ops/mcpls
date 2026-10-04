---
aliases:
  - Incremental server registration
  - Concurrent LSP server startup
tags:
  - sdd
  - spec
  - bug
  - lsp-bridge
  - startup
  - graceful-degradation
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp-server-lifecycle-and-respawn]]"
  - "[[lsp/007-lsp-child-process-lifetime/spec|lsp-child-process-lifetime]]"
  - "[[mcp/005-tool-capability-discoverability/spec|tool-capability-discoverability]]"
  - "[[bridge/006-lsp-indexing-readiness-gate/spec|lsp-indexing-readiness-gate]]"
---

# Feature: Per-Server Registration and Concurrent LSP Startup

> [!info] Metadata
> **Type**: bug (graceful-degradation gap)
> **Priority**: P2
> **Author**: Andrei G.
> **Issue**: #572
> **Observed at**: `ad90190`

## 1. Overview

### Problem Statement

`LspServer::spawn_batch` (`crates/mcpls-core/src/lsp/lifecycle.rs`) spawns and initializes the
configured servers strictly one after another. `init_lsp_servers` (`crates/mcpls-core/src/lib.rs`)
then registers the whole batch with the translator (`register_servers`: client + server inserts and a
single `rebind_router`) and calls `clear_expected_servers` only after the batch has returned.

Until that moment every tool call for every language, including a language whose server is already up,
returns the retryable `ServerInitializing` error (-32051), and `get_tool_support` reports every route as
`initializing`. Consequences:

1. Startup latency of the first healthy server equals the **sum** of all servers' `initialize` times.
2. A server that is slow, or hangs until its `timeout_seconds`, blocks every other language for that
   long. This contradicts the graceful-degradation principle of
   [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] (NFR-001: no single server's failure may
   prevent any other from becoming available), which `spawn_batch` honours for *outcomes* but not for
   *timing*.
3. Config order matters: a slow server listed first delays `rust-analyzer`.

> [!bug] Reproduction (release binary at `ad90190`, real stdio MCP session)
> Config with two servers: a fake language server (python script that sleeps 12 s before answering
> `initialize`, `language_id = "slow"`) and real `rust-analyzer` on a tiny cargo project. After MCP
> `initialize`, poll `tools/call get_hover` on a `.rs` file every 0.5 s.
>
> | Configured servers | First non-`-32051` answer |
> |--------------------|---------------------------|
> | `[slow, rust-analyzer]` | 13.9 s |
> | `[rust-analyzer]` only | 1.7 s |
> | `[rust-analyzer, slow]` | still `-32051` at the 4 s mark (registration is all-or-nothing, so order does not help) |

### Goal

Each configured server is registered, and its routes unblocked, as soon as its own `initialize`
completes; slow or failed siblings affect only their own languages; startup runs concurrently so wall
clock is bounded by the slowest single server rather than the sum.

### Out of Scope

- Lazy / on-demand server start (start a server only when its language is first used).
- Retrying a server that failed to start, or a manual restart tool (see
  [[mcp/008-manual-lsp-server-restart/spec|mcp/008]]). Respawn of a registered server that dies later is
  unchanged ([[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] FR-005..FR-010).
- Workspace-indexing readiness gating ([[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]]).
- Changing `initialize` handshake contents, capability negotiation, or `timeout_seconds` semantics.
- Changing the MCP handshake: `serve_with` already answers MCP `initialize` before LSP init finishes.
- Fan-out of workspace-wide tools across several servers (they resolve a single server today).

## 2. User Stories

### US-001: First healthy server is usable immediately

AS A developer using an AI client against a multi-language project
I WANT each language server to become usable the moment it finishes initializing
SO THAT a slow server for one language never delays tools for another.

**Acceptance criteria:**

```
GIVEN servers [slow (initialize takes 12 s), rust-analyzer (initialize takes ~1.7 s)] in that order
WHEN the MCP session starts and the client polls get_hover on a .rs file
THEN the first non--32051 answer arrives within 1.7 s + one poll interval, not after 12 s
```

### US-002: Config order does not affect availability

AS A user editing `[[lsp_servers]]`
I WANT startup latency of a server to be independent of its position in the file
SO THAT reordering or adding an entry never makes another language slower.

**Acceptance criteria:**

```
GIVEN the same two servers in either order
WHEN startup completes
THEN the time to first usable answer for the fast server differs by less than one poll interval between orders
```

### US-003: A failed server only affects its own languages

AS A developer whose environment lacks one language server
I WANT that server's failure reported precisely for its own files, as soon as it is known
SO THAT other languages keep working and the failing language gets `ServerFailedToStart` instead of
waiting on every other server first.

**Acceptance criteria:**

```
GIVEN server A fails to spawn immediately and server B takes 10 s to initialize
WHEN a tool call targets a file of A's language at t = 1 s
THEN it returns ServerFailedToStart with A's recorded failure, not ServerInitializing
AND a tool call targeting B's language at t = 1 s returns ServerInitializing naming B
AND the same call at t = 11 s succeeds
```

### US-004: Embedder gets the same behaviour

AS A library embedder of `mcpls-core`
I WANT `spawn_batch` and the registration path to start servers concurrently with a deterministic result
SO THAT library users get the same latency characteristics as the CLI.

**Acceptance criteria:**

```
GIVEN a batch of configs where several fail and several succeed
WHEN spawn_batch returns
THEN failures are listed in configuration order and the set of servers equals the set that initialized
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN background LSP initialization starts THE SYSTEM SHALL begin spawning and initializing every applicable server concurrently, so that every server's `initialize` request has been issued (or its spawn has failed) without waiting for any other server's `initialize` to complete; each remains bounded by its own `timeout_seconds` (clamped per [[lsp/001-lsp-server-lifecycle-and-respawn/spec\|lsp/001]] FR-002) | must |
| FR-002 | WHEN a server's `initialize` handshake succeeds THE SYSTEM SHALL register that server (routing client and server entry) and make its routes resolvable without waiting for any other server to settle, and SHALL remove only that server's id from the expected-server set | must |
| FR-003 | WHEN a server fails to start (spawn error, `initialize` error, `initialize` timeout, or a panic contained for that server) THE SYSTEM SHALL record its `StartupFailure`, rebind the routes that named it, and remove only its id from the expected-server set, without affecting any other server | must |
| FR-004 | THE SYSTEM SHALL perform the per-server settlement writes in this order: (1) record the failure, or register client then server; (2) update the router; (3) remove the server id from the expected-server set. A reader that reads the expected set first and the router last (as `Translator::tool_support_snapshot` does) SHALL never observe a server that is neither expected, nor registered, nor recorded as failed | must |
| FR-005 | WHILE any server is still initializing THE SYSTEM SHALL keep every route that names it intact: not dropped, not redirected to another server, and its position in the router's server order preserved, so that `resolve_any` (workspace-wide tools) resolves the same server it would have resolved before any registration | must |
| FR-006 | WHEN every server has settled THE SYSTEM SHALL hold a routing table identical to the one the batch rebind produces today for the same set of registered and failed servers, independent of the order in which servers settled | must |
| FR-007 | WHEN an explicit-route server fails while the language's catch-all is still initializing THE SYSTEM SHALL NOT bind the dead route to the catch-all until the catch-all has registered: the route keeps naming the failed server, but every lookup reports the retryable `ServerInitializing` naming the catch-all during the window (diagnostics readers: `Initializing`), so subscriptions and listen streams stay valid. WHEN the catch-all registers THE SYSTEM SHALL serve the route through it and notify subscribers of the files it now serves, and SHALL drop the route (reporting the explicit server's recorded failure) if the catch-all fails instead; a dead route is still never rebound to a narrowly-scoped (`handles = [...]`) live server | must |
| FR-008 | WHEN a tool call's route resolves to a server that is expected but not yet registered THE SYSTEM SHALL return `Error::ServerInitializing` (-32051) naming that server; WHEN it resolves to a registered server THE SYSTEM SHALL serve it normally; WHEN the route was dropped because the server failed THE SYSTEM SHALL return `Error::ServerFailedToStart` with the recorded failure. These semantics are unchanged from today and apply per route, at any moment during startup | must |
| FR-009 | THE SYSTEM SHALL report `get_tool_support` route status per route using the same registered / initializing / failed / no-server classification, so a language whose server is registered reports `supported` (or capability-specific status) while another language still reports `initializing` | must |
| FR-010 | WHEN a server registers THE SYSTEM SHALL apply its `IndexingPolicy` to the notification cache and then start its diagnostics pump, before any notification of that server can be consumed; pumps of other servers are neither delayed nor restarted | must |
| FR-011 | WHEN a server settles THE SYSTEM SHALL recompute the diagnostics-route count passed to `NotificationCache::set_diagnostics_route_count`, so that after the last server settles the count equals the value the batch computation produces today (number of registered servers that are the post-rebind `ToolKind::Diagnostics` route for their language) | must |
| FR-012 | WHEN a panic occurs while initializing one server THE SYSTEM SHALL attribute it to that server only (`StartupFailure::InitTaskPanicked`), keep initializing and registering its siblings, and log the panic message at `error!` | must |
| FR-013 | THE `run_init_supervised` outer net SHALL remain: a panic that escapes per-server containment settles every config that has not settled (recorded as `InitTaskPanicked` unless a failure is already recorded), rebinds against what registered, clears the expected set, and marks registered servers push-degraded, exactly as `Translator::settle_after_init_panic` does today | must |
| FR-014 | THE init body SHALL keep running on the supervised task (or on tasks owned by it such that aborting the outer task aborts them), so that aborting the outer task drops every not-yet-registered `Child` and no LSP process is orphaned ([[lsp/007-lsp-child-process-lifetime/spec\|lsp/007]]) | must |
| FR-015 | WHEN shutdown or cancellation arrives while servers are initializing THE SYSTEM SHALL abandon all in-flight spawns, with the same bounded wait and abort fallback as `await_lsp_init_handle` today | must |
| FR-016 | WHEN a server fails THE SYSTEM SHALL publish startup-failure notifications, at settlement time of that server (after its record, rebind and expected-set removal), to subscribers of every file whose route is now failed, including routes attributed to it through the configured router while a catch-all is pending; re-notifying an earlier failure is harmless, and nothing is published once per batch | should |
| FR-017 | THE SYSTEM SHALL emit per-server `info!`/`error!` lines as today; aggregate messages ("All N configured LSP server(s) failed to initialize", "Partial server initialization", "Proceeding with N LSP server(s)") SHALL be emitted once, when the last server settles | should |
| FR-018 | WHEN the last server settles and none registered THE SYSTEM SHALL end in the same state as today's all-failed path: router rebound against an empty set, expected set empty, every tool returning a terminal error (`AllServersFailedToInit` / `ServerFailedToStart`) | must |
| FR-019 | `LspServer::spawn_batch` SHALL run its configs concurrently and return a `ServerInitResult` whose `failures` are in configuration order | must |
| FR-020 | THE doc comment of `ToolRouter::rebind_to_registered` (which stated it is sound only because registration is all-or-nothing) SHALL be replaced by the contract of the mechanism that implements FR-005..FR-007: `ToolRouter::rebind` derives the active table from the immutable configured router and each server's `ServerSettlement` (`Pending`, `Registered`, `Failed`) on every settlement, a pure function of that state, and `rebind_to_registered` is its finished-startup special case | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Time to first usable answer for server S equals S's own `initialize` time plus a fixed spawn overhead, independent of other servers. Total startup wall clock is bounded by the slowest single server, not the sum. Measured with the reproduction above: `[slow(12 s), rust-analyzer]` and `[rust-analyzer, slow(12 s)]` both answer within 1.7 s + 0.5 s poll tolerance |
| NFR-002 | Resilience | A server that hangs until `timeout_seconds`, fails, or panics delays no other server by any amount (extends [[lsp/001-lsp-server-lifecycle-and-respawn/spec\|lsp/001]] NFR-001 from outcomes to timing) |
| NFR-003 | Compatibility | Error codes and shapes (`ServerInitializing` -32051, `WorkspaceServersInitializing`, `ServerFailedToStart`, `AllServersFailedToInit`) and `get_tool_support` route statuses are unchanged; only the moment a route leaves `initializing` changes |
| NFR-004 | Consistency | `get_tool_support` and tool routing never misreport a healthy, registering server as `no_server` for any interleaving of settlement writes with snapshot reads, for any number of servers (generalises `healthy_server_never_misreported_for_any_write_read_interleaving` from one registration event to N) |
| NFR-005 | Determinism | The final routing table, failure set and diagnostics-route count are a pure function of the per-server outcomes, never of completion order or scheduling |
| NFR-006 | Concurrency safety | No `std::sync::Mutex` guard (translator registries, router) is held across an `.await` or nested with another, as documented for `Translator`; settlement of different servers may interleave at any lock boundary |
| NFR-007 | Type safety | Per-server outcome is a closed type (registered server or `StartupFailure`), not a stringly or `Option`-pair representation; settlement of an id is representable at most once (no double-settle state) |
| NFR-008 | Resource usage | Concurrent initialization of N servers must not exceed one initialize-time memory/CPU spike per server already accepted today; whether to cap parallelism is open (see section 9) |
| NFR-009 | Observability | Each settlement logs the server id and elapsed time since init start, so a slow server is diagnosable from logs alone |
| NFR-010 | Documentation | Every changed `pub` item (`spawn_batch`, any new settlement API) has an updated `///` doc with `# Examples`; docs build with `-D warnings` |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Server outcome | Terminal result of one configured server's startup | server id, registered server or `StartupFailure` |
| Expected set | Servers configured and applicable that have not yet settled | shrinks one id at a time, empty when startup is settled |
| Startup failures | Recorded failures, queried per route | server id, language, command, `StartupFailure` (incl. `InitTaskPanicked`) |
| Routing table | Per-language explicit and catch-all routes plus server order | must keep pending servers' routes intact (FR-005) |
| Diagnostics route count | Divisor of the shared diagnostics cache budget | derived from settled servers (FR-011) |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Slow server listed first, fast second | Fast server registers at its own completion; slow one stays `ServerInitializing` until it settles (US-001) |
| Fast server fails instantly, slow one still initializing | Failed server's languages return `ServerFailedToStart` at once; slow server's languages return `ServerInitializing` |
| Explicit-route server fails, catch-all still initializing | Route is not rebound until the catch-all registers (FR-007), but the dead explicit tool reports the retryable `ServerInitializing` naming the catch-all (diagnostics readers: `Initializing`; `get_tool_support`: `initializing`). Workspace-wide tools likewise fall through to the pending catch-all and report `ServerInitializing` naming it, since the failed server leaves the server order. When the catch-all registers the route is served by it and subscribers are notified; if it fails instead, the route is dropped and the explicit server's failure is reported (`ServerFailedToStart`) |
| Catch-all registers after the explicit diagnostics server failed, or while it is pending | Pushes the catch-all made while its role was `Secondary` were dropped and are not replayed. For files the user never opens (workspace-wide flycheck diagnostics) no later publish may arrive, because mcpls sends no `didSave` ([[bridge/009-diagnostics-subscription-staleness/spec\|bridge/009]]); the cache fills at the next publish for the file |
| Catch-all fails after explicit server was bound to it | The redirect is dropped; affected tools report the explicit server's recorded failure |
| Two servers settle at the same instant | Result identical to either order (NFR-005); registration of one never observes a half-registered other |
| All servers fail | Same terminal state as today (FR-018); log once |
| One server panics during `initialize` | Only that server gets `InitTaskPanicked`; siblings unaffected (FR-012) |
| Panic escapes per-server containment | Outer net settles all unsettled configs (FR-013) |
| SIGTERM / cancellation during startup | In-flight spawns abandoned, children dropped (FR-014, FR-015) |
| `get_tool_support` called mid-startup | Per-route mixed status, no route reported as `no_server` while its server is expected or registering (FR-009, NFR-004) |
| Workspace-wide tool whose resolved server is pending but a later-ordered server already registered | Still `ServerInitializing` naming the pending server; never silently served by a different server (FR-005) |
| Diagnostics pump of server A producing notifications while B settles | A's pump and cache are untouched; count recompute affects only eviction tie-breaking (FR-011) |
| Resource subscriber for a file whose server fails | Notified at that server's settlement (FR-016) |
| Single configured server | Behaves as today; no observable change |
| `applicable_configs` empty | Protocol-only mode unchanged |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Time to first non--32051 answer for `rust-analyzer` with `[slow(12 s), rust-analyzer]` | within 0.5 s of the `[rust-analyzer]`-only time (observed 1.7 s) |
| SC-002 | Same with order `[rust-analyzer, slow(12 s)]` | within 0.5 s of the single-server time |
| SC-003 | Total startup wall clock for N servers with initialize times t1..tN | max(ti) + bounded overhead, not sum(ti) |
| SC-004 | Final router, failure set and diagnostics-route count over all permutations of completion order of a fixed set of outcomes | identical to the batch result (property test) |
| SC-005 | Interleaving test over N servers' settlement writes and `tool_support_snapshot` reads | zero misreports of a healthy server |
| SC-006 | Panic injected in one server's init with siblings healthy | siblings registered and usable; panicked server reports `InitTaskPanicked` |
| SC-007 | Abort of the init task mid-startup | no orphaned LSP process |

## 8. Agent Boundaries

### Always (without asking)

- Run the full pre-commit suite (fmt, clippy `-D warnings`, nextest `--lib --bins`, rustdoc `-D warnings`).
- Keep `Translator` lock discipline: short, never nested, never across `.await`.
- Read `bridge/translator/support.rs` read-order contract and `config/routing.rs` rebind contract before editing either.
- Update `CHANGELOG.md` (`[Unreleased]`, one line, PR link) and the live-testing documents under `.local/testing/` (playbook: slow-server-plus-rust-analyzer ordering matrix; coverage-status reset for lsp and bridge).

### Ask First

- Adding a concurrency cap or a new config option for startup parallelism.
- Changing public signatures of `spawn_batch`, `register_servers`, or adding a streaming startup API.
- Adding any dependency (e.g. `futures` utilities if not already present).

### Never

- Modify the `initialize` handshake, `timeout_seconds` clamping, or the respawn path.
- Make `rebind_router` destructive for a server that is still initializing.
- Hold a `std::sync::Mutex` guard across an `.await`.
- Reorder the settlement writes of FR-004.
- Weaken the typed server outcome into strings or untyped maps (project type-safety rule).

## 9. Open Questions

> [!question] Open items
> - [x] Mechanism for FR-005..FR-007: the active table is re-derived from the immutable `configured_router` and the three-state settlement on every settlement (`ToolRouter::rebind`); `rebind_to_registered` remains as the finished-startup wrapper.
> - [ ] Cap on parallel server initialization: unbounded for now (applicable set is small, project-marker filtered); revisit only if a measurement shows a spike.
> - [x] Provisional diagnostics-route count (FR-011): non-failed servers, pending ones included, that are the diagnostics route in the current re-derived router.
> - [x] Edge case "explicit-route server failed, catch-all pending": the dead tool reports the retryable `ServerInitializing` naming the catch-all (FR-007, section 6).
> - [x] Public API shape: `spawn_batch` is a concurrent join returning the full `ServerInitResult`; incremental registration lives in `init_lsp_servers` over the crate-private `LspServer::start_contained`.
> - [x] Panic containment: per-future `catch_unwind` on the supervised task (`start_contained`), so aborting the task drops every unregistered child (FR-014).
> - [x] Concurrent `LspServer::spawn` is safe with `lsp/process.rs` process-wide state: `LIFELINE` is a synchronous mutex never held across an await and `WARNED` is atomic.
> - [x] `Translator::tool_support_snapshot` still does not read recorded startup failures: a failed server's routes are reported `no_server`, as before; per-route enforcement (`client_for_file`, `diagnostics_route_for_path`) reports the recorded failure, including in the FR-007 window.

## 10. See Also

- [[constitution]] -- project principles
- [[MOC-specs]] -- all specifications
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] -- `spawn_batch` graceful degradation (FR-003, NFR-001)
- [[lsp/007-lsp-child-process-lifetime/spec|lsp/007]] -- child-process lifetime binding (FR-014)
- [[mcp/005-tool-capability-discoverability/spec|mcp/005]] -- `get_tool_support` route classification
- Code: `crates/mcpls-core/src/lsp/lifecycle.rs` (`spawn_batch`), `crates/mcpls-core/src/lib.rs` (`init_lsp_servers`, `register_servers`, `run_init_supervised`), `crates/mcpls-core/src/bridge/translator/{mod,routing,support}.rs`, `crates/mcpls-core/src/config/routing.rs` (`rebind_to_registered`), `crates/mcpls-core/src/bridge/notifications.rs` (`set_diagnostics_route_count`)
