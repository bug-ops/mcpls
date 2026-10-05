---
aliases:
  - Manual LSP server restart
  - restart_server tool
tags:
  - sdd
  - spec
  - enhancement
  - mcp
  - lsp
  - competitor-gap
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001-lsp-server-lifecycle-and-respawn]]"
  - "[[lsp/007-lsp-child-process-lifetime/spec|lsp/007-lsp-child-process-lifetime]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001-mcp-tool-surface-and-routing]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp/002-mcp-resources-diagnostics]]"
  - "[[mcp/005-tool-capability-discoverability/spec|mcp/005-tool-capability-discoverability]]"
  - "[[bridge/002-document-tracker-synchronization/spec|bridge/002-document-tracker-synchronization]]"
  - "[[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006-lsp-indexing-readiness-gate]]"
---

# Feature: On-Demand Restart of a Stuck or Stale LSP Server via MCP

> [!info] Metadata
> **Type**: enhancement (competitor gap)
> **Priority**: P2 (raised from P3 because three comparable bridges expose it)
> **Related issues**: #564; depends on or interacts
> with #541, #542 (process-tree reaping on Unix); builds on #249 (dead-server detection) and
> #359 (push-diagnostics degradation flag)

> [!warning] Status
> Implemented with #564 (together with #542, #567 and #563). The open product decisions are
> resolved in "Decisions" below; the `[NEEDS CLARIFICATION]` markers in the body keep the
> original questions for context.

> [!success] Decisions
> - **Name and selector (FR-001, FR-002):** `restart_server`, taking `servers` (non-empty list of
>   server ids) or `all: true`; a bare call, an empty list, a blank id, or both fields are
>   rejected at deserialization. `language_id` and `file_path` selectors are not offered.
> - **Process-tree reaping (FR-006):** #542 is fixed in the same change (per-server watchdog
>   process group, see lsp/007), so a restart kills the server's whole group; `setsid`
>   descendants (#541) may survive.
> - **Pump re-wiring (FR-009):** the diagnostics pump is re-wired on restart and the push-degraded
>   flag is cleared. Automatic crash respawn keeps discarding notifications (a follow-up).
> - **Cooldown and backoff (FR-008):** a manual restart bypasses the crash-loop backoff; the same
>   server is throttled for 5 s (`throttled { retry_in_ms }`).
> - **Annotations (FR-010):** `read_only_hint = false`, `destructive_hint = true`,
>   `idempotent_hint = false` (each repeat tears down and respawns a process). Destructive
>   because the group kill also kills shared daemons (Gradle, Bloop) that other clients may be
>   using, so clients should ask before running it.
> - **Always on (FR-013):** no configuration flag.
> - **Startup-failed servers (FR-015):** reported as `not_running`; starting them is a follow-up.
>   A server still starting reports `initializing`, and so does every restart while startup is
>   still settling (a restarted pump's diagnostics role is fixed at spawn).
> - **Termination order:** kill-then-respawn: the old server gets 3 s to answer `shutdown`; one
>   that answered may then take up to 10 s to exit on its own (rust-analyzer and jdtls flush
>   caches), after which the whole process group is killed; one that did not answer is killed
>   at once. Terminate errors are not fatal. Worst cases per server: about 3.1 s for a wedged
>   server, about 13 s for a healthy slow-to-stop one; `All` over N servers takes about
>   ceil(N/4) times that (four restart concurrently), plus each replacement's `initialize`.
> - **Failed restart:** the stopped server's cached diagnostics are cleared, it is flagged
>   push-degraded and subscribers are notified, so stale data is never served as live (SC-006).
> - **Pending requests:** requests in flight on the old process fail with the retryable
>   `server_restarted` error, code `-32054`.
> - **Subscribers:** resource subscribers are told to re-read the diagnostics cleared by the
>   restart (`resources/updated`).
> - **Pinned documents:** the tracker has no pinning; "documents tracked as open" are re-sent on
>   next access.

## 1. Overview

### Problem Statement

mcpls replaces an LSP server automatically only after its process has **died**
(`Translator::respawn_if_dead` in `crates/mcpls-core/src/bridge/translator/respawn.rs`, see
[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]]). There is no agent-callable way to
recover a server that is **alive but unusable**:

- the server is wedged (rust-analyzer or pyright stuck in a state where requests time out, or a
  flycheck/`cargo check` holds a lock);
- the server holds a stale index or cache after a manifest change (`Cargo.toml`, `package.json`,
  `tsconfig.json`, `pyproject.toml`) that it does not pick up on its own;
- the server answers, but with results the agent has reason to distrust.

The only recovery today is restarting the whole mcpls process, which the agent usually cannot do
(the MCP client owns the process) and which discards every other language's healthy server. The
mcpls tool descriptions already concede the gap: diagnostics results "may be incomplete until
mcpls restarts".

**Prior art.** Three comparable code-intelligence bridges ship this capability. One exposes a
restart tool with an optional filter by file extension that restarts the matching servers, or all
of them when the filter is omitted. One exposes an optional tool that restarts the language
server. One exposes a session-level tool that restarts the LSP server.

Three comparable bridges make this a parity gap, which meets the P2 bar of the parity rubric.
Automatic respawn already covers crashes, so the wedged/stale cases are the less frequent ones.

**Why this is not a trivial wrapper over `respawn_if_dead`.** The existing path is built for a
*dead* process and carries assumptions that do not hold for a live one:

1. It starts from a process that has already exited. A wedged server must first be torn down,
   bounded by the existing shutdown deadline, without trusting it to answer `shutdown`.
2. Respawn deliberately **discards** the replacement's push notifications and permanently marks the
   diagnostics-route server push-degraded (`NotificationCache::mark_push_degraded`, #359) because
   the diagnostics pump is not re-wired. Applied to a *healthy-but-stale* server, that would turn a
   working live-diagnostics server into a permanently degraded one until mcpls restarts.
3. On Unix only the leader process is killed on respawn; its descendants survive until mcpls exits
   (#542, and #541 for `setsid` descendants). For a manual restart of a wedged server, a stuck
   descendant (for example a hung `cargo check`) is often the very thing causing the wedge.
4. Every other mcpls tool is annotated read-only (`rename_symbol` and `format_document` return
   edits, they do not apply them). This would be the first tool with a side effect on mcpls's
   own state, so the client-visible annotations and the question of whether it is on by default
   need an explicit decision.

### Goal

An AI agent can ask mcpls to restart one specific LSP server (or, explicitly, all of them) and
afterwards observe a fresh, initialized server with a clean per-server state, without restarting
the mcpls process and without affecting servers it did not name.

### Out of Scope

- Changing the automatic crash-respawn behavior or its backoff policy for dead servers
  ([[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]]), beyond the interaction rules in
  FR-007 and FR-008.
- Re-reading the configuration file or re-running LSP server discovery. A restart reuses the
  server's existing spawn configuration; adding, removing or reconfiguring servers still requires
  restarting mcpls (see [[config/001-config-discovery-and-heuristics/spec|config/001]]).
- Notifying a server of a changed manifest without restarting it
  (`workspace/didChangeWatchedFiles`, `workspace/didChangeConfiguration`): a possible lighter
  alternative, tracked separately if wanted.
- Restarting the mcpls process itself.
- Fixing descendant-process reaping on Unix (#541, #542). This spec only states how restart
  depends on it (FR-006, section 9).
- Technical design: to be recorded in `plan.md`.

## 2. User Stories

### US-001: Recover a wedged server without losing the session

AS AN AI coding agent whose `get_hover` calls on Rust files keep timing out while rust-analyzer is
alive
I WANT to restart just the rust-analyzer server
SO THAT I regain working Rust tools without restarting mcpls or disturbing the Python and
TypeScript servers.

**Acceptance criteria:**
```
GIVEN rust-analyzer (server "rust") is alive but not answering requests, and pyright is healthy
WHEN the agent calls the restart tool for server "rust"
THEN the old rust-analyzer process is terminated within the shutdown deadline
  AND a new rust-analyzer is spawned and initialized
  AND the result reports "rust" as restarted and does not mention pyright
  AND a subsequent get_hover on a Rust file reaches the new process
  AND pyright's process, document state and cached diagnostics are untouched
```

### US-002: Refresh a stale index after a manifest change

AS AN AI coding agent that just edited `Cargo.toml` (adding a dependency)
I WANT to restart the server that serves that workspace
SO THAT symbol, diagnostics and reference results reflect the new dependency graph.

**Acceptance criteria:**
```
GIVEN a running server whose index predates a manifest edit
WHEN the agent restarts that server and then calls a read tool on an affected file
THEN the file is re-opened on the new process with its current on-disk content
  AND the tool is gated on the new process's indexing readiness (bridge/006), not served from
      the previous process's state
```

### US-003: Know exactly what a restart did

AS AN AI coding agent
I WANT a structured per-server outcome from the restart call
SO THAT I can distinguish "restarted", "restart failed (and why)", "crash-loop backoff active",
and "no such server" and decide whether to retry, wait for indexing, or fall back.

**Acceptance criteria:**
```
GIVEN a restart request naming two servers, one of which fails to respawn (binary removed from PATH)
WHEN the call completes
THEN the structured result contains one entry per requested server
  AND the successful server is reported restarted
  AND the failed server is reported failed with a typed reason (not only free text)
  AND the call as a whole does not abort the successful restart
```

### US-004: Operator keeps control of a non-read-only tool

AS A mcpls operator
I WANT MCP clients to see that this tool is not read-only, and (if so decided) to be able to turn
it off
SO THAT client permission prompts and my own policy reflect that it kills and restarts processes.

**Acceptance criteria:**
```
GIVEN the server is running with default configuration
WHEN a client lists tools
THEN the restart tool's annotations declare read_only_hint = false and are explicit about
  destructive_hint and idempotent_hint (see FR-010)
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHALL expose one MCP tool that restarts one or more configured LSP servers on demand. Name: `restart_server`. [NEEDS CLARIFICATION: confirm the name; mcpls tool names are `get_*`/`go_to_*`/`*_symbol`, and the `tool_prefix` mechanism applies to it like every other tool.] | must |
| FR-002 | THE SYSTEM SHALL select the servers to restart through a typed selector that is parsed at the MCP boundary into existing typed identities (`ServerId` and an explicit `All` variant), never matched as free-form strings deep in the bridge. The selector forms are: one or more server ids; or an explicit all-servers choice. [NEEDS CLARIFICATION: (a) also accept a `language_id` (resolves to every server for that language, relevant when two servers share `python`); (b) also accept a `file_path` (route-based, consistent with every other tool); (c) whether a call with no selector is an error (recommended, so a bare call cannot restart everything) or means "all", as one comparable bridge does. Recommended default: server ids or an explicit all choice, no bare call.] | must |
| FR-003 | WHEN the selector names a server id that is not configured THE SYSTEM SHALL reject the call with a typed error that lists the configured ids, and SHALL restart nothing | must |
| FR-004 | WHEN a restart is requested for a live, registered server THE SYSTEM SHALL terminate the old process (graceful `shutdown`/`exit` attempt bounded by the existing LSP shutdown deadline, then forced kill), spawn a replacement from the server's existing spawn configuration, and complete the LSP `initialize` handshake before reporting success | must |
| FR-005 | WHEN a restart succeeds THE SYSTEM SHALL reset all per-server state that belonged to the old process, exactly as the dead-server path does today: fail requests still pending on the old client with a typed error (not a timeout), clear the document tracker's sync history for that server so documents are re-sent with `didOpen` on next access, clear that server's cached diagnostics, and reset its tracked indexing state to `Unknown` so the readiness gate (bridge/006) applies to the new process | must |
| FR-006 | WHEN a restart terminates an old process THE SYSTEM SHALL also terminate that process's descendants to the same extent the platform lifetime mechanism allows ([[lsp/007-lsp-child-process-lifetime/spec\|lsp/007]]): the whole tree on Windows; on Unix the server's process group (#542), with `setsid` descendants subject to #541, and shared daemons such as Gradle or Bloop killed too. [NEEDS CLARIFICATION: ship the tool before #542 is fixed (a manual restart of a wedged server would then leak the wedged descendants until mcpls exits), or make #542 a prerequisite. Recommended default: fix #542 first or in the same batch, and document the residual `setsid` case (#541) in the tool description.] | should |
| FR-007 | WHEN a restart and an automatic respawn (or two restarts) target the same server concurrently THE SYSTEM SHALL serialize them per server so that exactly one new process results and no caller observes a half-replaced server (single-flight, as `respawn_if_dead` provides for respawn) | must |
| FR-008 | WHEN a server is within a crash-loop backoff window THE SYSTEM SHALL treat an explicit restart as operator intent and attempt it immediately, then record its outcome in the same failure/stability bookkeeping as an automatic respawn, so a restart that fails keeps the backoff honest. [NEEDS CLARIFICATION: confirm bypass-on-manual-restart, and whether to impose a minimum interval between manual restarts of the same server to stop an agent from restart-looping a slow-indexing server. Recommended default: bypass backoff, and apply a short per-server cooldown that returns a typed `Throttled { retry_in }` outcome.] | should |
| FR-009 | WHEN the restarted server is the diagnostics-route server for its language THE SYSTEM SHALL leave push-diagnostics state truthful: if the replacement's push notifications are not delivered into the cache, the server SHALL be flagged degraded exactly as today (`push_notifications_degraded`, #359) and the restart result SHALL say so; a restart SHALL NOT silently leave resource subscribers believing diagnostics are live. [NEEDS CLARIFICATION: re-wire the diagnostics pump on restart so a restart returns the server to non-degraded, which is the main reason a user would restart a healthy-but-stale rust-analyzer, versus accepting the existing permanent degradation. Recommended default: re-wiring is a prerequisite, because accepting degradation makes restart worse than the stale state for diagnostics users.] | must |
| FR-010 | THE SYSTEM SHALL annotate the tool with `read_only_hint = false`, `destructive_hint = true` (it discards in-memory server state and kills the server's whole process group, including shared daemons other clients may use; no user files are touched), `idempotent_hint = false` (each repeat tears down and respawns a process), and an `open_world_hint` consistent with the other tools. | must |
| FR-011 | THE SYSTEM SHALL return a structured result (`structuredContent` with `outputSchema`, the existing tool-output convention) containing one entry per targeted server with a closed, typed outcome set: restarted, failed (with typed reason), throttled/backing-off (with `retry_in`), and the server's resulting indexing state; plus the push-degradation flag from FR-009 | must |
| FR-012 | WHEN some targeted servers restart and others fail THE SYSTEM SHALL complete every requested restart independently (one failure does not stop the rest) and report all outcomes, consistent with the graceful-degradation principle in the constitution | must |
| FR-013 | THE SYSTEM SHALL decide, as a recorded product decision, whether the tool is enabled by default or opt-in through configuration. [NEEDS CLARIFICATION: the project otherwise exposes a read-only tool surface, but the constitution (VII) prefers one code path over flags. Recommended default: always on, with accurate annotations (FR-010) so MCP clients can prompt, and no config flag. If opt-in is chosen, the disabled tool must be absent from `tools/list` and `get_tool_support`, not present-but-refusing.] | must |
| FR-014 | WHEN `get_tool_support` is queried THE SYSTEM SHALL describe the restart tool as not capability-gated by any LSP capability, consistent with the other bridge-local tools (`get_server_logs`, `get_server_messages`), and keep the `get_tool_support` parity matrix passing | should |
| FR-015 | WHEN a restart targets a server that is registered but not yet initialized (`ServerInitializing`) THE SYSTEM SHALL either wait for or reject with a typed outcome, never start a second concurrent spawn for the same id. [NEEDS CLARIFICATION: also support starting a server that failed at startup (for example after the user installed the missing binary, see lsp/006), which is a natural extension of "restart" but is not a restart of a live process. Recommended default: out of scope for this spec; follow up separately.] | should |
| FR-016 | THE translator lifecycle SHALL be one phase (idle, settling, settled, init panicked, shutting down; shutting down terminal) and restart eligibility one decision over the phase and the server slot, in the order shutting down, not running or failed, init panicked, settling or unwired, cooldown (#619) | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | The selector, the per-server outcome and the failure reason SHALL be closed types (newtype `ServerId`, enums), exhaustively matched; no stringly-typed server names or outcome strings past the MCP parameter boundary. Illegal combinations (an "all" selector carrying ids, an empty id list) SHALL be unrepresentable or rejected at deserialization |
| NFR-002 | Isolation | A restart SHALL NOT change the process, documents, diagnostics cache, indexing state or backoff state of any server not named in the request |
| NFR-003 | Bounded latency | Termination of the old process SHALL be bounded by the existing LSP shutdown deadline (`lsp::SHUTDOWN_TIMEOUT` and its child-exit grace); the call SHALL NOT wait for workspace indexing to finish. Total call duration is bounded by termination plus the configured `initialize` timeout |
| NFR-004 | No orphans | After a successful restart no leader process of the old instance remains; descendants follow FR-006 |
| NFR-005 | Safety | No `unsafe` code (`unsafe_code = "forbid"` workspace-wide) and no new dependency without CHANGELOG justification |
| NFR-006 | Protocol compatibility | The tool conforms to the standard MCP `Tool` schema (name, description, `inputSchema`, `outputSchema`, annotations). Adding it is a visible `tools/list` change (22 to 23 tools together with `go_to_declaration`) and a breaking-surface item for CHANGELOG |
| NFR-007 | Non-regression | Automatic crash respawn, its backoff and `get_tool_support` behavior (mcp/005 NFR-002 and NFR-003) SHALL remain unchanged for callers that never use the restart tool |
| NFR-008 | Observability | Each restart SHALL be logged (`tracing`) with server id, trigger (manual vs automatic) and outcome |

## 5. Data Model

No persistent data is introduced. Relevant entities:

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| `ServerId` | Existing typed server identity (`config/routing.rs`): explicit `name`, else `language_id` | Newtype over `String`; used as the key of every per-server registry |
| Restart target | New closed selector parsed at the MCP boundary | Variants: specific `ServerId`s (non-empty) or `All`; optional `language_id`/`file_path` forms per FR-002 clarification |
| Restart outcome (per server) | New closed result type | `Restarted { indexing_state, push_degraded }`, `Failed { reason }`, `Throttled { retry_in }`, `NotRegistered`/`Initializing` as decided by FR-015 |
| Per-server bridge state reset by restart | Existing state, listed so none is missed | Client and server handles, document tracker sync history (`DocumentTracker::forget_server`), cached diagnostics ownership, `IndexingState`, `push_degraded` flag, respawn backoff entry and single-flight lock, lifecycle forwarder task, pending requests |

`ToolKind` and `ToolKind::ALL` (`config/routing.rs`) are unchanged: they enumerate tools routed
per `(language, tool)` through the `handles` configuration. A restart addresses servers directly
and is not a routable LSP request, so it is not a `ToolKind`, and `handles` lists never mention
it. It is added to the MCP tool enumeration (`McpTool::ALL`) and the `tool_surface.json` snapshot
as a bridge-local tool.

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Restart requested for an unknown server id | Typed error listing configured ids; nothing restarted (FR-003) |
| Server is already dead when the call arrives | Treated as a restart of a dead server: reuse of the respawn outcome, reported as restarted or failed, never as a no-op |
| Server ignores `shutdown` (wedged) | Forced kill after the shutdown deadline (FR-004); the call still completes |
| Replacement fails to spawn or `initialize` fails (binary gone, bad config) | `Failed { reason }` for that server; old process is already gone; existing backoff bookkeeping records the failure (FR-008); other servers unaffected (FR-012) |
| Requests in flight on the old process (including from other MCP sessions on an HTTP transport) | Failed promptly with a typed error that tells the caller the server was restarted and the call may be retried; none left to time out |
| Two agents or sessions restart the same server at once | Serialized per server; the second call observes the fresh server (FR-007); it does not tear down the new process unless throttling rules allow it (FR-008) |
| Restart during crash-loop backoff | Attempted immediately per FR-008; failure keeps extending backoff |
| Restart immediately followed by a read tool | The read tool re-opens documents and is gated on indexing readiness (US-002); it returns the existing `WorkspaceIndexing` error rather than empty results while the new server loads |
| Restarted server is the diagnostics-route server | FR-009: truthful degradation flag; resource subscribers are notified that cached diagnostics for that server's URIs were invalidated, per mcp/002 semantics. [NEEDS CLARIFICATION: confirm subscribers receive `resources/updated` for invalidated URIs on restart] |
| Documents with unsaved agent edits tracked as open | Restart discards the old process's in-memory buffers; documents are re-sent from the tracker's current content on next access. [NEEDS CLARIFICATION: the finding mentions "pinned documents"; the current `DocumentTracker` has no pinning concept. Confirm that the intended meaning is "documents currently tracked as open" and that no additional pinned set is required] |
| Descendants that escape the group (`setsid`) on Unix | May survive a restart (#541/#542); documented, not a restart defect (FR-006) |
| Stale `ServerId` after config hot-change | Not applicable: configuration is not reloaded at runtime (out of scope) |
| Single configured server, selector `All` | Equivalent to restarting that server; result contains one entry |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | End-to-end test with a fake LSP server that stops answering: restart then a read tool call | Read tool succeeds against the new process; old process pid is gone within the shutdown deadline |
| SC-002 | Isolation test with two servers, restart one | Second server's pid, tracked documents, cached diagnostics and indexing state are byte-for-byte unchanged |
| SC-003 | Document re-sync test | After restart, the first access to a previously opened document sends `didOpen` (not `didChange`) to the new process |
| SC-004 | Outcome-typing test for mixed success/failure across two servers | Result has exactly one typed entry per requested server; one failure does not prevent the other restart |
| SC-005 | Tool-surface guard tests | `test_all_tools_carry_annotations` and the value-level annotation table updated with the new tool's `(false, false, false)` classification; `McpTool::ALL`, `tool_surface.json` and the `get_tool_support` parity matrix updated; count 23 |
| SC-006 | Diagnostics-route test (FR-009) | After restarting the diagnostics-route server, `get_cached_diagnostics` and the diagnostics resource either deliver live push again, or carry the degraded flag; never stale-as-live |
| SC-007 | Concurrency test: restart racing automatic respawn and a second restart | Exactly one new process; no leaked child |
| SC-008 | Documentation gate | README tool table, tool description (including the Unix descendant caveat) and CHANGELOG entry present |

## 8. Agent Boundaries

### Always (without asking)
- Reuse the existing respawn bookkeeping (single-flight lock, backoff, document-tracker forget,
  indexing reset, pending-request failure) rather than duplicating it.
- Keep `ServerId` and typed enums at every internal boundary; convert to strings only at the MCP
  parameter and result edge.
- Update `McpTool::ALL`, the annotation guard tests, `tool_surface.json`, README and CHANGELOG
  together with the tool.

### Ask First
- Enabling the tool by default versus gating it behind configuration (FR-013).
- Accepting `language_id` or `file_path` selectors, or a bare call meaning "all" (FR-002).
- Shipping before #542 is fixed, or before diagnostics-pump re-wiring exists (FR-006, FR-009).
- Any change to automatic respawn backoff policy or to `lsp/007` process-group design.
- Adding a dependency.

### Never
- Restart a server the request did not name, or fall back to "all" on an unparsable selector.
- Weaken, bypass or reorder capability gating (`Translator::require_capability`) or the
  indexing-readiness gate for requests that follow a restart.
- Add `unsafe`, `pre_exec` or a mcpls re-exec supervisor to implement termination.
- Report success before the replacement has completed `initialize`.
- Leave a restarted diagnostics-route server silently stale-as-live (FR-009).

## 9. Open Questions

> [!question] Decisions needed before `/sdd plan`
> - [NEEDS CLARIFICATION: Tool name, `restart_server` vs a name aligned with mcpls conventions (FR-001)]
> - [NEEDS CLARIFICATION: Selector shape: server ids / all only, plus `language_id` and `file_path` forms, and whether a bare call is an error (FR-002)]
> - [NEEDS CLARIFICATION: Sequencing against #542 and #541: ship now with a documented caveat, or after (FR-006)]
> - [NEEDS CLARIFICATION: Diagnostics pump re-wiring as a prerequisite versus accepting permanent push degradation after restart (FR-009)]
> - [NEEDS CLARIFICATION: Per-server cooldown for manual restarts, and bypass of crash-loop backoff (FR-008)]
> - [NEEDS CLARIFICATION: `idempotent_hint = true` acceptable for a tool that tears down and respawns a process (FR-010)]
> - [NEEDS CLARIFICATION: Always on versus opt-in via configuration (FR-013)]
> - [NEEDS CLARIFICATION: Should the same tool also start a server that failed at startup (FR-015)]
> - [NEEDS CLARIFICATION: Meaning of "pinned documents" in the finding (section 6)]
> - [NEEDS CLARIFICATION: Issue number for this finding (metadata `#TBD`)]

## 10. See Also

- [[constitution]] — project principles (type safety, simplicity, graceful degradation)
- [[MOC-specs]] — all specifications
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] — spawn, dead-server detection,
  backoff-bounded respawn; the machinery this feature reuses and must not regress
- [[lsp/007-lsp-child-process-lifetime/spec|lsp/007]] — process-tree lifetime; per-platform
  behavior on respawn (#541, #542)
- [[lsp/006-server-spawn-install-hint/spec|lsp/006]] — startup spawn failures (FR-015 context)
- [[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]] — tool router, `ToolKind`, `ServerId`
- [[mcp/002-mcp-resources-diagnostics/spec|mcp/002]] — diagnostics resources, subscriptions and
  the push-degraded flag
- [[mcp/005-tool-capability-discoverability/spec|mcp/005]] — `get_tool_support` and its parity
  matrix (FR-014)
- [[bridge/002-document-tracker-synchronization/spec|bridge/002]] — document tracker re-open
  behavior after a server swap
- [[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]] — readiness gate that governs calls
  right after a restart
