---
aliases:
  - LSP server lifecycle
  - Spawn, initialize, respawn, shutdown
tags:
  - sdd
  - spec
  - lsp
created: 2026-08-05
status: implemented
related:
  - "[[constitution]]"
  - "[[bridge/002-document-tracker-synchronization/spec]]"
  - "[[bridge/001-position-encoding-layer/spec]]"
  - "[[runtime/002-sigterm-stdin-blocking-pool-hang/spec]]"
---

# Feature: LSP Server Process Lifecycle — Spawn, Initialize, Crash-Recovery Respawn, Shutdown

> [!info] Metadata
> **Author**: retroactive spec, authored during the `.local/specs/` → `specs/` migration and gap
> analysis (no single originating commit/PR — this documents already-shipped, working
> functionality; the crash-detection/respawn mechanism references #249 (`has_exited`) and #359
> (respawn diagnostics-degradation flagging) in its own source comments, and the shutdown path is
> the mechanism [[runtime/002-sigterm-stdin-blocking-pool-hang/spec|spec runtime/002]] fixed the *process-exit*
> half of)
> **Type**: core subsystem
> **Priority**: P1 (retroactive — reflects centrality, not an open defect)

> [!success] Resolution
> This is a retroactive spec: `crates/mcpls-core/src/lsp/lifecycle.rs` (spawn/initialize/shutdown,
> graceful degradation across multiple servers), `crates/mcpls-core/src/lsp/client.rs` (JSON-RPC
> request/response with retry), `crates/mcpls-core/src/lsp/transport.rs` (stdio framing), and
> `crates/mcpls-core/src/bridge/translator/respawn.rs` (dead-server detection and
> backoff-bounded respawn) already implement everything described below.

## 1. Overview

### Problem Statement

mcpls bridges an AI client to potentially several LSP servers running as child processes
(rust-analyzer, pyright, etc.), each an independent, fallible subprocess that must be:

1. **Spawned** with a sanitized environment (the child's environment is cleared, then only an
   explicit allowlist plus user-configured overrides are passed through — the LSP server does not
   inherit mcpls's full process environment, which could otherwise leak secrets the server has no
   need to see).
2. **Initialized** via the LSP handshake (`initialize` → capability negotiation, including position
   encoding — see [[bridge/001-position-encoding-layer/spec|spec bridge/001]] — → `initialized`), with one
   server's failure not blocking the others (**graceful degradation**: `spawn_batch` attempts every
   configured server in sequence and returns both the servers that succeeded and the failures for
   the ones that didn't, rather than failing all-or-nothing).
3. **Monitored for crashes** during normal operation, since a language server can die at any time
   (OOM, a bug in the server itself, a signal from the OS). A crashed server must be distinguished
   from "still initializing" or "never configured," and a crash-looping server must not consume a
   full `timeout_seconds` of latency on every single subsequent tool call.
4. **Shut down gracefully** on mcpls's own exit (`shutdown` request → `exit` notification → wait
   for the child to exit on its own → `kill_on_drop` as a last resort), so LSP child processes are
   never orphaned.

Each of the four phases has its own genuine failure modes (a broken `PATH`, a slow `initialize`
against a large workspace, a server that crashes mid-session, a child that ignores `exit` and must
be killed), and getting any of them wrong either orphans processes, cascades one server's failure
into an outage for every language, or makes crash recovery itself a performance hazard.

### Goal

mcpls spawns, initializes, monitors, transparently respawns, and gracefully shuts down every
configured LSP server such that (a) one server's spawn/initialize failure never prevents any other
configured server from running; (b) a server that crashes mid-session is detected and
transparently replaced before the next tool call that needs it, without the caller needing to know
a respawn happened; (c) a crash-looping server backs off exponentially rather than eating a full
timeout on every tool call; and (d) every spawned child process is terminated (gracefully or via
`kill_on_drop`) when mcpls exits, with no orphans.

### Out of Scope

- Position-encoding conversion math itself — [[bridge/001-position-encoding-layer/spec|spec bridge/001]].
- Which server handles which tool once several are configured for one language —
  [[mcp/001-mcp-tool-surface-and-routing/spec|spec mcp/001]]'s `ToolRouter`.
- The OS-process-level SIGTERM/SIGINT handling that makes the whole mcpls *process* exit promptly
  — already covered by [[runtime/002-sigterm-stdin-blocking-pool-hang/spec|spec runtime/002]]; this spec covers
  the LSP *child* process shutdown sequence that fix's `Translator::shutdown_servers` step
  triggers, not the parent process's own exit mechanics.
- Reconnecting live diagnostics push notifications for a respawned server to the original
  notification pump — explicitly out of scope per `respawn_if_dead`'s own doc comment (the pump's
  remaining dependencies live outside `Translator`'s scope); a respawned diagnostics-route server's
  degraded-push state is only surfaced (via `NotificationCache::mark_push_degraded`), not repaired,
  until the whole mcpls process restarts.

## 2. User Stories

### US-001: One misconfigured server doesn't take down every language

AS A developer with a monorepo configuring 6 LSP servers, one of which has a typo'd command path
I WANT the other 5 servers to spawn and initialize normally
SO THAT a single misconfiguration doesn't make every language's tools unavailable

**Acceptance criteria:**
```
GIVEN 6 configured LSP servers, one with a nonexistent command
WHEN LspServer::spawn_batch runs
THEN the result has_servers() is true, partial_success() is true, and the 5 valid servers are
     usable for their respective languages while the failed one is reported in `failures`
```

### US-002: A crashed language server is transparently replaced

AS AN AI agent mid-session when rust-analyzer crashes
I WANT my next tool call to transparently succeed against a freshly respawned rust-analyzer
instead of failing outright
SO THAT a single server crash doesn't end my whole session

**Acceptance criteria:**
```
GIVEN a registered LSP server whose child process has exited unexpectedly
WHEN the next tool call routes to that server
THEN Translator::respawn_if_dead detects the death, respawns and re-initializes the server, and
     the tool call proceeds against the new instance transparently
```

### US-003: A crash-looping server backs off instead of stalling every request

AS A mcpls operator whose configured LSP server keeps crashing immediately after each respawn
(e.g. a persistent misconfiguration)
I WANT each subsequent respawn attempt to back off exponentially rather than retrying (and
timing out) on every single tool call
SO THAT a crash-looping server doesn't make every tool call for that language slow

**Acceptance criteria:**
```
GIVEN a server that has failed 3 consecutive respawn attempts
WHEN a 4th tool call arrives for that server within the current backoff window
THEN respawn_if_dead returns Error::ServerUnavailable immediately (naming the remaining backoff
     duration) rather than attempting another spawn and waiting for it to fail/timeout
```

### US-004: No orphaned LSP child processes after mcpls exits

AS A mcpls operator
I WANT every spawned LSP child process (rust-analyzer, pyright, etc.) to be terminated when mcpls
shuts down, whether gracefully or forcibly
SO THAT restarting/redeploying mcpls never leaves orphaned language-server processes consuming
resources

**Acceptance criteria:**
```
GIVEN a running LSP server child process
WHEN LspServer::shutdown is called
THEN it sends `shutdown`+`exit`, waits up to a fixed grace period for the child to exit on its
     own, and falls back to kill_on_drop (SIGKILL) if it hasn't -- in every case, the child
     process is gone by the time shutdown returns
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN spawning an LSP server child process THE SYSTEM SHALL clear the child's environment and pass through only an explicit allowlist (`PATH`, `HOME`/`USERPROFILE`, `TMPDIR`/`TEMP`/`TMP`, plus Windows-specific additions under `cfg(windows)`), then apply the server's configured `env` overrides last | must |
| FR-002 | THE SYSTEM SHALL perform the `initialize` → capability-negotiation → `initialized` handshake for each server, using the server's configured `timeout_seconds` (clamped to `[1, MAX_TIMEOUT_SECONDS]`) as the handshake timeout | must |
| FR-003 | WHEN spawning multiple configured servers (`spawn_batch`) THE SYSTEM SHALL attempt every server regardless of earlier failures and return both the successfully-initialized servers and a list of failures, never failing all-or-nothing | must |
| FR-004 | THE SYSTEM SHALL expose `has_exited()` (non-blocking `try_wait`) so callers can detect a crashed child process without blocking on it | must |
| FR-005 | WHEN a tool call routes to a server whose child process has exited THE SYSTEM SHALL respawn and re-initialize it before the request proceeds, transparently to the caller | must |
| FR-006 | WHEN two concurrent tool calls both route to the same dead server THE SYSTEM SHALL single-flight the respawn (via a per-server lock) so only one respawn attempt is made; the other caller waits for it and rechecks rather than racing a second, redundant spawn | must |
| FR-007 | WHEN a respawn attempt fails THE SYSTEM SHALL record it and apply exponential backoff (base 1s, doubling per consecutive failure, capped at 30s) before permitting another respawn attempt for that server | must |
| FR-008 | WHEN a respawn attempt succeeds but the server dies again before surviving at least the backoff base duration THE SYSTEM SHALL count this as a failure (extending the backoff), rather than treating "successfully re-initialized" as proof of stability | must |
| FR-009 | WHEN a server is respawned THE SYSTEM SHALL clear that server's document-sync history in `DocumentTracker` (see [[bridge/002-document-tracker-synchronization/spec|spec bridge/002]]), since the new process has no memory of any document the old one had open, and must receive `didOpen` (not `didChange`) for every document going forward | must |
| FR-010 | WHEN the diagnostics-route server for a language is respawned THE SYSTEM SHALL mark that language's cached diagnostics as push-degraded (`NotificationCache::mark_push_degraded`) rather than silently continuing to serve stale cached diagnostics as current | must |
| FR-011 | WHEN shutting down a server THE SYSTEM SHALL send the LSP `shutdown` request, then the `exit` notification, then wait up to a fixed grace period (3s) for the child process to exit on its own, falling back to `kill_on_drop` (SIGKILL on drop) if it does not | must |
| FR-012 | THE SYSTEM SHALL perform the graceful-shutdown sequence (FR-011) even if the `shutdown`/`exit` handshake itself fails or times out — the child process must still be torn down (killed if necessary) regardless of handshake outcome | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Resilience | No single server's spawn/initialize failure may prevent any other configured server from becoming available (graceful degradation, FR-003) |
| NFR-002 | Performance | A crash-looping server must not cost more than the bounded backoff window per tool call once backed off — never a repeated full `timeout_seconds` wait per call (FR-007) |
| NFR-003 | Security | A spawned LSP server must not inherit mcpls's full process environment by default — only the explicit allowlist plus configured overrides (FR-001); configured `env` keys/values are never logged, only an allowlist-presence count and an override count (secret-bearing env var names like `AWS_SECRET_ACCESS_KEY` must not leak via debug logs) |
| NFR-004 | Correctness | A respawned server must never be treated as having synced state (open documents) it never actually saw (FR-009) |
| NFR-005 | Observability | A respawn (successful or failed), a crash-loop backoff, and a diagnostics-degradation event must each be logged (`tracing::warn!`) with enough context (server id, language, backoff duration) to diagnose from logs alone |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|-----------------|
| `LspServer` | One managed, initialized LSP server instance | `client: LspClient`, `capabilities: ServerCapabilities`, `position_encoding: PositionEncodingKind`, `notification_rx`, `child: tokio::process::Child` |
| `ServerInitConfig` | Everything needed to spawn+initialize one server | `server_config`, `workspace_roots`, `initialization_options`, `position_encodings`, `notification_tx` |
| `ServerInitResult` | Outcome of `spawn_batch` across all configured servers | `servers: HashMap<ServerId, LspServer>`, `failures: Vec<ServerSpawnFailure>` |
| `ServerState` | Coarse lifecycle state of a server connection | `Uninitialized`, `Initializing`, `Ready`, `ShuttingDown`, `Shutdown` |
| `RespawnBackoff` (translator-internal) | Per-server respawn-attempt bookkeeping | `consecutive_failures: u32`, `last_attempt: Instant`, `last_attempt_succeeded: bool` |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|--------------------|
| A server's command is not on `PATH` | `spawn` returns `Error::ServerSpawnFailed`; `spawn_batch` records it as a failure and continues with the rest |
| A large workspace makes `initialize` slow | Bounded by the server's configured `timeout_seconds` (clamped to `MAX_TIMEOUT_SECONDS`), not a hardcoded 30s |
| Server crashes between two tool calls | First call after the crash detects it via `has_exited`, respawns, and proceeds; the crash is otherwise invisible to the caller beyond added latency |
| Two tool calls race a dead-server detection simultaneously | Single-flighted via `respawn_lock`; the loser waits for the winner's attempt and rechecks rather than double-spawning |
| Server respawns successfully but crashes again within 1s | Counted as a failure (not a fresh, unbacked-off start), extending `consecutive_failures` |
| Server proves stable (survives ≥1s after a successful respawn) | Backoff state cleared entirely; a later unrelated crash starts a fresh backoff sequence |
| `shutdown`/`exit` handshake itself fails or times out | Child process is still torn down (gracefully within the grace period, or killed) regardless; the handshake error is still returned to the caller after teardown completes |
| Child does not exit within the grace period after `exit` | `kill_on_drop` (SIGKILL) fires when the `Child` handle drops |
| A respawned server is the diagnostics-route server for its language | That language's cached diagnostics are marked push-degraded; a healthy sibling server's own language's diagnostics are unaffected (scoped by per-server ownership, not a blanket cache clear) |
| A respawned server is *not* the diagnostics-route server | No diagnostics-cache side effect — only document-sync history (FR-009) is cleared |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | `cargo nextest run -E 'package(mcpls-core) and (test(lifecycle) or test(respawn))'` | All existing spawn/initialize/shutdown/respawn unit tests pass |
| SC-002 | `spawn_batch` with N configured servers, M of which are invalid | `has_servers()` true iff N-M > 0; `partial_success()` true iff both 0 < (N-M) and M > 0 |
| SC-003 | Live repro: kill a spawned rust-analyzer mid-session, issue a hover call for a Rust file | Call succeeds against a freshly respawned rust-analyzer, no error surfaced to the MCP caller |
| SC-004 | Live repro: repeatedly kill a server immediately after each respawn | Respawn attempts space out exponentially (1s, 2s, 4s, ... capped at 30s), not immediately retried each time |

## 8. Agent Boundaries

### Always (without asking)
- Preserve graceful degradation in `spawn_batch` — never make one server's failure abort the whole
  batch
- Preserve the exponential-backoff respawn policy exactly (base 1s, cap 30s) unless explicitly
  asked to change it
- Keep the diagnostics-degradation flag (FR-010) scoped to the diagnostics-route server only,
  never clearing a healthy sibling server's cache entries

### Ask First
- Reconnecting a respawned server's push notifications to a live pump (currently explicitly out
  of scope; doing so would need to solve the pump's-dependencies-live-elsewhere problem noted in
  `respawn_if_dead`'s doc comment)
- Changing the child-exit grace period (3s) or the backoff base/cap constants

### Never
- Let a respawn attempt for one server block or fail tool calls routed to a different, healthy
  server
- Skip the graceful `shutdown`/`exit` handshake attempt before falling back to `kill_on_drop` —
  the handshake gives well-behaved servers a chance to flush/clean up even though the fallback
  guarantees termination either way

## 9. Open Questions

None — this is a retroactive spec documenting stable, already-shipped, well-tested behavior.

## 10. See Also

- [[constitution]] — project principles (not yet created for this project)
- [[MOC-specs]] — all specifications
- [[runtime/002-sigterm-stdin-blocking-pool-hang/spec|spec runtime/002]] — the OS-process-exit half of shutdown
  this spec's `Translator::shutdown_servers`/`LspServer::shutdown` step feeds into
- [[bridge/001-position-encoding-layer/spec|spec bridge/001]] — the conversion math for the `PositionEncodingKind`
  negotiated during this spec's `initialize` handshake
- [[config/001-config-discovery-and-heuristics/spec|spec config/001]] — `LspServerConfig`/`ServerHeuristics`,
  the configuration this spec's `spawn`/`spawn_batch` consume
- [[bridge/002-document-tracker-synchronization/spec|spec bridge/002]] — `DocumentTracker::forget_server`,
  invoked on respawn per FR-009
- `crates/mcpls-core/src/lsp/lifecycle.rs` — `LspServer::spawn`/`spawn_batch`/`shutdown`,
  `ServerInitConfig`, `ServerInitResult`
- `crates/mcpls-core/src/lsp/client.rs` — `LspClient` JSON-RPC request/response, retry-on-cancel
- `crates/mcpls-core/src/lsp/transport.rs` — stdio header-content framing
- `crates/mcpls-core/src/bridge/translator/respawn.rs` — `respawn_if_dead`, `RespawnBackoff`
