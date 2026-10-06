---
aliases:
  - Request cancellation propagation
  - MCP and LSP request cancellation
tags:
  - sdd
  - spec
  - mcp
  - lsp
  - cancellation
  - resource-hygiene
created: 2026-10-06
status: draft
related:
  - "[[constitution]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]]"
  - "[[mcp/003-mcp-2026-stateless-adoption/spec|mcp-2026-stateless-adoption]]"
  - "[[mcp/008-manual-lsp-server-restart/spec|manual-lsp-server-restart]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp-server-lifecycle-and-respawn]]"
  - "[[lsp/005-lsp-content-modified-retry/spec|lsp-content-modified-retry]]"
  - "[[bridge/002-document-tracker-synchronization/spec|document-tracker-synchronization]]"
  - "[[mcp/015-request-cancellation-propagation/plan|plan]]"
---

# Feature: Request Cancellation Propagation (MCP to LSP)

> [!info] Metadata
> **Type**: enhancement (protocol compliance, resource hygiene)
> **Priority**: P3
> **Related issues**: #687
> **Baseline commit**: 12ee3a1 (31 tools, `rmcp` 3.5.1)

## 1. Overview

### Problem Statement

A language server does real work for every request mcpls sends it: a workspace-wide reference search
or a diagnostics pull can occupy a server for tens of seconds and block its other requests. Today
that work is never stopped, in two situations.

**A client cancels a tool call.** The MCP cancellation utility says a receiver of
`notifications/cancelled` SHOULD stop processing the request, free the resources associated with it,
and not send a response. At HEAD the transport library (`rmcp` 3.5.1, serve loop in `service.rs`)
does the part it owns: it cancels the per-request `RequestContext::ct` token and discards the
response when it eventually arrives. It does not abort the spawned handler future. The `tools/call`
path (`call_tool` in `crates/mcpls-core/src/mcp/server.rs`, then the `#[tool_router]` methods) never
observes that token; only the `subscriptions/listen` handler awaits `context.cancelled()`. A
cancelled `get_references`, `get_diagnostics` or `workspace_symbol` therefore runs to completion,
bounded only by the LSP request timeout, plus up to three `ServerCancelled` or `ContentModified`
retries with exponential backoff (`LspClient::request_inner` in `crates/mcpls-core/src/lsp/client.rs`,
500 ms initial delay), and its result is thrown away.

**A client-side LSP request times out.** In the `timeout(...)` error branch of `request_inner` the
pending-map entry is removed and `Error::Timeout` is returned, but the server is never told. The
server keeps computing an answer nobody will read. When it finally replies, the reply finds no
pending entry and is logged at `warn!` ("Received response for unknown request ID") in
`handle_inbound_message`, so a timeout today also produces a spurious warning later.

The LSP base protocol has the mechanism for exactly this: the `$/cancelRequest` notification
(`{ id }`), after which a server SHOULD answer the request with error `-32800` (`RequestCancelled`)
or with its result if it had already finished. The typed method constant exists in the dependency
(`LspNotificationMethod::CancelRequest`); a search finds no use of it in `mcpls-core`.

Related facts at HEAD that shape the requirement:

| Fact | Where | Consequence |
|------|-------|-------------|
| Every request attempt allocates a fresh `RequestId::Number` from an atomic counter; a retry is a new id | `request_inner` | Cancel must target the id of the attempt in flight, and a retry loop must stop once cancelled |
| A request reply `-32800` is not special-cased in `request_inner`; it surfaces as a generic server error (logged at `error!`) | `request_inner`, `UnclassifiedError::server_response` | A reply to a request mcpls itself cancelled must not be logged as a failure |
| Request-issuing call sites number about 30 across `bridge/translator/*` (navigation, symbols, edits, hierarchy, diagnostics, enclosing, selection and folding ranges, highlights, assist) | grep of `request_typed` | The cancel signal has to reach all of them through one mechanism, not per-site edits that can drift |
| Some calls issue several LSP requests: target resolution plus the main request; per-file `documentSymbol` for `context: enclosing_symbol`; up to `MAX_CODE_ACTION_RESOLVES` concurrent `codeAction/resolve` in a `JoinSet`; `restart_server` fans out across servers with `buffer_unordered` | `translator/edits.rs`, `enclosing.rs`, `restart.rs` | One MCP cancel must reach every outstanding LSP request of the call and stop further ones |
| Requests may be failed in bulk (`fail_pending_requests`, message loop exit) when a server is restarted, respawned or dies | `lsp/client.rs`, `respawn.rs` | Cancellation must cooperate with, not duplicate or contradict, these drains |
| `ensure_open` serializes per path and sends `didOpen`/`didChange` before recording the new version; eviction closes run inline on the request path under a claim | `bridge/state.rs`, `routing.rs` | Abandoning a call at an arbitrary await point must not desynchronize tracker state from what the server was sent |
| rmcp cancels request tokens when the serve loop is cancelled, but not at transport EOF: at EOF it waits up to 5 s for in-flight handlers to finish, then closes; handler tasks are detached (also noted in the `listen` doc comment) | `rmcp-3.5.1/src/service.rs` (`QuitReason::Closed` branch) | A closed connection currently leaves handlers and their LSP requests running; connection close needs its own cancellation signal |

### Goal

When an MCP client cancels a tool call, or the connection it came from closes, the in-flight handler
stops at its next await point, every LSP request issued for that call is cancelled on its server
with `$/cancelRequest` exactly once, no retry or follow-up request is issued, and no document-state
or resource leak results. When an LSP request times out, the server is told to stop working on it.
Calls that are not cancelled behave exactly as before.

### Out of Scope

- Cancelling work in the language server beyond sending `$/cancelRequest` (no process kill, no
  restart as a cancellation strategy; a server that ignores `$/cancelRequest` keeps working).
- Work-done progress cancellation (`window/workDoneProgress/cancel`) and partial-result streaming.
- Client-initiated cancellation of server-to-client requests, and cancellation in the direction
  mcpls to MCP client (requests mcpls sends to the client, if any).
- Cancelling the MCP `initialize` request (the MCP utility forbids it) and the subscription
  lifecycle (`subscriptions/listen` already ends on its token; `resources/subscribe` and
  `unsubscribe` are unchanged).
- MCP tasks (`tasks/cancel`) semantics: see [[mcp/004-mcp-tasks-sep2663-adoption/spec|mcp/004]].
- Changing request timeouts, retry budgets, backoff constants or the retry allowlist
  ([[lsp/005-lsp-content-modified-retry/spec|lsp/005]]); only their interaction with cancellation.
- New configuration keys, environment variables or CLI flags.
- Technical design: recorded in [[mcp/015-request-cancellation-propagation/plan|plan]].

## 2. User Stories

### US-001: Client abandons a slow call and the language server is freed

AS AN AI client (or its user) that cancels a long `get_references` or `get_diagnostics` call
I WANT mcpls to stop the call and tell the language server to stop computing it
SO THAT the server is available for my next request instead of finishing work nobody will read.

**Acceptance criteria:**
```
GIVEN a tool call whose LSP request is in flight on a server that is slow to answer
WHEN the client sends notifications/cancelled for that request id
THEN the handler returns at its next await point
  AND the server receives exactly one $/cancelRequest carrying the id of the in-flight LSP request
  AND mcpls sends the client no response for the cancelled call
  AND the pending-request entry is removed
```

### US-002: Timeout does not leave an orphaned computation

AS AN operator running mcpls against a large workspace
I WANT a timed-out LSP request to be cancelled on the server
SO THAT repeated timeouts do not stack up unfinished work and later spurious warnings in the log.

**Acceptance criteria:**
```
GIVEN an LSP request that exceeds its timeout
WHEN the client-side timeout fires
THEN exactly one $/cancelRequest is sent for that request id
  AND the caller still receives Error::Timeout unchanged
  AND a late reply for that id, including a -32800 error, is dropped without a warn or error log
```

### US-003: Cancellation does not trigger retries

AS A maintainer relying on the retry behavior in [[lsp/005-lsp-content-modified-retry/spec|lsp/005]]
I WANT a cancelled call to stop retrying
SO THAT a cancel never costs up to 3.5 s of extra backoff or spawns a new LSP request.

**Acceptance criteria:**
```
GIVEN a call that received -32802 and is sleeping in retry backoff
WHEN the call is cancelled
THEN the sleep ends immediately, no new LSP request id is allocated or sent, and no $/cancelRequest is sent for the already-answered id
GIVEN the server replies -32800 or -32802 to a request mcpls cancelled
WHEN the reply arrives
THEN it is not retried and not logged as a failure
```

### US-004: Multi-request calls are cancelled as a unit

AS A client calling a tool that issues several LSP requests (target resolution, enclosing-symbol
enrichment, code-action resolution, restart across servers)
I WANT one cancel to stop the whole call
SO THAT cancelling does not leave siblings running or start the next step.

**Acceptance criteria:**
```
GIVEN a call with N concurrent outstanding LSP requests on one or more servers
WHEN the call is cancelled
THEN each outstanding request id receives exactly one $/cancelRequest on its own server
  AND no further LSP request is issued for the call
  AND servers with no outstanding request for the call receive nothing
```

### US-005: Closed connection stops its in-flight work

AS AN operator whose MCP client disconnects (stdio EOF, HTTP session end) or whose mcpls is shutting down
I WANT in-flight handlers to stop and their LSP requests to be cancelled
SO THAT no work continues for a client that is gone.

**Acceptance criteria:**
```
GIVEN tool calls in flight when the transport closes or serve is cancelled
WHEN the connection ends
THEN each handler stops at its next await point
  AND outstanding LSP requests are cancelled once each, unless the server is already shutting down
  AND no handler outlives the connection by more than the bound in NFR-003
```

### US-006: Maintainer can trust the behavior is tested

AS A mcpls maintainer
I WANT deterministic tests for cancel-before-start, cancel-during-request, cancel-during-backoff, timeout cancel and the completion race
SO THAT a later refactor of the request path cannot silently drop cancellation.

**Acceptance criteria:**
```
GIVEN a scripted fake language server that records inbound messages
WHEN each scenario in section 6 runs
THEN the recorded $/cancelRequest count and ids match the table, with no flaky timing assertions
```

## 3. Functional Requirements

Priorities: `must` / `should` / `may`. "Outstanding" means a request that has been registered in the
pending map (and possibly written to the server) and has not received a reply, timed out, or been
failed in bulk.

### Handler observation

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN the request token of a `tools/call` is cancelled THE SYSTEM SHALL stop the handler future at its next await point and return without producing a tool result | must |
| FR-002 | WHEN the request token is already cancelled at handler entry THE SYSTEM SHALL issue no LSP request, send no LSP notification, and open no document on behalf of that call | must |
| FR-003 | THE SYSTEM SHALL NOT send a result or an error response for a cancelled request itself; discarding any late response is left to the transport | must |
| FR-004 | THE SYSTEM SHALL log a cancelled tool call at `debug` level with tool name and the phase it was in, and SHALL NOT log it as a handler failure, panic or error | should |

### LSP cancellation

| ID | Requirement | Priority |
|----|------------|----------|
| FR-005 | WHEN a call is cancelled while an LSP request it issued is outstanding THE SYSTEM SHALL send `$/cancelRequest` with that request's id to the server it was sent to, exactly once | must |
| FR-006 | WHEN an LSP request exceeds its client-side timeout THE SYSTEM SHALL send `$/cancelRequest` for that id exactly once, then remove the pending entry and return `Error::Timeout` as today | must |
| FR-007 | WHEN a request has already received its reply, or was never registered, or its entry was already removed THE SYSTEM SHALL NOT send `$/cancelRequest` for it | must |
| FR-008 | THE SYSTEM SHALL guarantee at most one `$/cancelRequest` per request id for every combination of cancel, timeout, completion and bulk failure racing each other | must |
| FR-009 | THE `$/cancelRequest` SHALL be enqueued on the same ordered channel as the request it cancels, so it is never written before that request | must |
| FR-010 | WHEN the future awaiting an LSP request is dropped for any reason (cancellation, abort of a spawned subtask, panic containment) THE SYSTEM SHALL cancel the outstanding id as in FR-005 | must |
| FR-011 | THE SYSTEM SHALL NOT fail, block indefinitely, or surface an error to any caller because a `$/cancelRequest` could not be sent (server dead, channel closed, queue full); such a failure is logged at `debug` | must |
| FR-012 | THE SYSTEM SHALL NOT send `$/cancelRequest` to a server once its `shutdown` request has been issued, and SHALL NOT send it to a client instance that has been failed in bulk, restarted or respawned | must |

### Retry and reply handling

| ID | Requirement | Priority |
|----|------------|----------|
| FR-013 | WHEN a call is cancelled during retry backoff THE SYSTEM SHALL end the backoff immediately and SHALL NOT allocate or send another request id for that call | must |
| FR-014 | WHEN a reply to a request that mcpls cancelled or timed out arrives (a result, `-32800`, `-32801` or `-32802`) THE SYSTEM SHALL drop it silently at `trace` level: no retry, no `warn!`, no `error!` | must |
| FR-015 | WHEN a server replies `-32800` to a request mcpls did not cancel THE SYSTEM SHALL surface it as a normal server error without retry, as today | must |
| FR-016 | THE SYSTEM SHALL decide "cancelled by us" from mcpls's own record of the request id, not from the error code alone | must |

### Multi-request and multi-server calls

| ID | Requirement | Priority |
|----|------------|----------|
| FR-017 | WHEN a call has several outstanding LSP requests (concurrent subtasks or sequential steps) THE SYSTEM SHALL cancel each outstanding id once and SHALL start no further request or subtask for that call | must |
| FR-018 | WHEN a call fans out to several servers THE SYSTEM SHALL send `$/cancelRequest` to each server holding an outstanding request of the call, each with that server's own id for it, and nothing to other servers | must |
| FR-019 | WHEN one server of a fan-out fails, times out or is cancelled THE SYSTEM SHALL keep today's graceful-degradation behavior for the others (no behavior change for the uncancelled siblings) | must |
| FR-020 | WHEN a spawned subtask set (for example the code-action resolve `JoinSet`) is dropped by cancellation THE SYSTEM SHALL abort its subtasks and cancel their outstanding ids per FR-010 | must |

### Interplay with restart, respawn and shutdown

| ID | Requirement | Priority |
|----|------------|----------|
| FR-021 | WHEN a server is restarted or respawned and its pending requests are failed in bulk THE SYSTEM SHALL fail the waiting callers as today (`ServerRestarted` or `ServerTerminated`) and SHALL NOT send `$/cancelRequest` to the old process or to its replacement | must |
| FR-022 | WHEN a call is cancelled while its request is being failed in bulk THE SYSTEM SHALL treat cancellation as the outcome (no result, no error response) and send no `$/cancelRequest` for an id already failed | must |
| FR-023 | WHEN the connection closes or serve is cancelled THE SYSTEM SHALL cancel every in-flight `tools/call` handler, including on transport EOF where the transport library does not cancel request tokens itself | must |
| FR-024 | WHEN mcpls begins shutting down THE SYSTEM SHALL let the existing LSP `shutdown`/`exit` sequence proceed unchanged and SHALL NOT emit a burst of `$/cancelRequest` that delays it | must |
| FR-025 | WHEN `restart_server` is cancelled before it has begun restarting a server THE SYSTEM SHALL not start it; once a restart of a server has begun THE SYSTEM SHALL take it to a defined end state (restarted, or failed with the existing outcome) rather than abandon it half-done | must |

### Cancel-safety of shared state

| ID | Requirement | Priority |
|----|------------|----------|
| FR-026 | WHEN a call is cancelled during `ensure_open` THE SYSTEM SHALL leave the document tracker consistent with what each server was actually sent: a version is recorded as synced to a server only if the matching `didOpen` or `didChange` was enqueued, and a `didOpen` already enqueued is never left unrecorded | must |
| FR-027 | WHEN a call is cancelled THE SYSTEM SHALL release every per-path lock, pending-close claim, concurrency permit and settle/indexing waiter it holds, with no resource remaining attributable to the call | must |
| FR-028 | WHEN a call that opened a document is cancelled THE SYSTEM SHALL keep that document tracked under the normal lazy-open and eviction policy (cancellation neither closes it nor pins it) | should |
| FR-029 | WHEN a diagnostics pull is cancelled or its reply is `-32800` after our cancel THE SYSTEM SHALL NOT store anything in the `Pulled` slot, SHALL NOT notify resource subscribers, and SHALL NOT cache an empty result | must |
| FR-030 | WHEN an indexing-readiness wait, settle wait or any other bounded wait inside a handler is in progress and the call is cancelled THE SYSTEM SHALL end the wait immediately | must |

### Typing and observability

| ID | Requirement | Priority |
|----|------------|----------|
| FR-031 | THE cancel API SHALL take and carry the typed LSP `RequestId` (or an opaque typed handle that owns one), never a bare integer, and the "this id was cancelled by us" record SHALL be a typed set, not a string or integer flag | must |
| FR-032 | THE SYSTEM SHOULD expose cancellation counts (cancelled by client, cancelled by timeout, sent, suppressed because already completed) at `debug`/`trace` so live tests can verify exactly-once behavior without a language-server-specific tool | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Behavioral non-regression | For a call that is neither cancelled nor timed out, request sequence, request ids, retry sequence and timing, result bytes, error text, log lines at `warn` and above, and document-tracker effects SHALL be identical to HEAD; the golden `tool_surface.json` and `tools/list` payload SHALL be unchanged |
| NFR-002 | Bounded overhead | Per LSP request: O(1) extra work (one token clone or guard, one map operation under the existing pending lock); no extra LSP traffic or allocation proportional to history; the record of cancelled ids SHALL be bounded by a named constant (oldest evicted first) and cleaned up when the reply or a bulk failure resolves it |
| NFR-003 | Latency of stop | A cancelled handler SHALL return within 100 ms of the token firing (excluding non-cancellable critical sections bounded by FR-025 and FR-026, each of which SHALL be bounded and documented); after connection close no handler SHALL outlive the serve shutdown drain window |
| NFR-004 | Robustness | A dead, wedged or full-queue server SHALL NOT make a cancel block the cancelled caller (FR-011); the cancel enqueue SHALL be non-blocking or bounded by a named constant |
| NFR-005 | Type safety | Cancellation state SHALL be modeled with typed values: the existing `RequestId`, a named cancellation scope or token type, and an enum for the cause (client cancel, timeout, connection close); no stringly-typed causes, no raw `i64` ids crossing module boundaries |
| NFR-006 | Layering | The cancellation signal SHALL be one mechanism threaded from `mcp/` through the translator to `LspClient`; request sites SHALL NOT each re-implement it (DRY); `lsp/` SHALL NOT depend on `mcp/` types |
| NFR-007 | Compatibility with servers | `$/cancelRequest` is part of the LSP base protocol and needs no capability negotiation; a server that ignores it SHALL behave as it does today (the late reply is dropped per FR-014) |
| NFR-008 | Portability | Tests SHALL pass on Linux, macOS and Windows; fake language-server fixtures follow the project rule that fake executables get an `.exe` name on Windows; no timing assertion SHALL depend on wall-clock sleeps shorter than CI jitter (use paused time or explicit synchronization) |
| NFR-009 | Safety | `unsafe_code = "forbid"` remains; no new dependency (the cancellation token type is already a dependency of `mcpls-core`) |
| NFR-010 | Breaking changes | Before v1.0.0 a signature change to public `LspClient` request methods is acceptable and SHALL be recorded in `CHANGELOG.md` under `[Unreleased]` |
| NFR-011 | Documentation | Doc comments on every new public item with the contract (what callers may assume, what implementors guarantee); the user-facing behavior (cancel stops the call; a timed-out request is cancelled on the server) SHALL be added to the user guide and the testing playbooks under `.local/testing/` |

## 5. Data Model

No persistent data. Entities touched or introduced:

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| MCP request token | Per-request `RequestContext::ct`, cancelled on `notifications/cancelled` | Parent: serve-loop token (cancelled at serve cancel, not at EOF) |
| Connection token | Cancelled when the connection ends for any reason, including EOF (FR-023) | Observed by every handler together with the request token |
| Cancellation scope | The per-call handle threaded to every LSP request of one call; fires on request-token cancel, connection close, or call completion | Cause: client cancel, connection close |
| Pending LSP request | Entry in `LspClient::pending_requests` keyed by typed `RequestId` | Oneshot responder; cancel-sent state |
| Cancelled-id record | Typed set of ids mcpls has cancelled and not yet seen answered | Bounded, cleaned on reply, bulk failure or client drop |
| Document tracker state | Per-path version and per-server synced version | Must stay consistent with notifications enqueued (FR-026) |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Cancel arrives before the handler starts (token already cancelled) | No LSP request, no `didOpen`, no pending entry (FR-002); nothing sent to any server |
| Cancel while the request is still queued on the command channel, not yet written | Request and cancel are both written in order (FR-009); the server sees request then cancel |
| Cancel after the server has replied but before the handler consumed the reply | The reply wins or the cancel wins, never both a cancel and a result; no `$/cancelRequest` for a completed id (FR-007, FR-008) |
| Completion and timeout fire in the same instant | One outcome only; at most one cancel (FR-008) |
| Cancel and timeout fire together | One `$/cancelRequest`; the cancel cause wins, no `Error::Timeout` is produced for a cancelled call |
| Cancel during retry backoff after `-32802` | Backoff ends, no new id, no cancel for the answered id (FR-013, US-003) |
| Server replies `-32800` to our cancel | Dropped at `trace` (FR-014) |
| Server replies with a normal result to a request we cancelled (it had finished) | Dropped at `trace`; no warn, no state change |
| Server ignores `$/cancelRequest` and replies minutes later | Reply dropped at `trace` once (FR-014); the cancelled-id record entry is removed on that reply |
| Server never replies to a cancelled request | The cancelled-id record is bounded and swept (NFR-002); no growth proportional to uptime |
| Server died or channel closed when the cancel is sent | Cancel skipped, `debug` log, caller unaffected (FR-011) |
| `restart_server` or respawn runs while a call is outstanding | Waiting callers fail as today (FR-021); a cancel racing with the drain sends nothing (FR-022) |
| Cancel arrives while `restart_server` is mid-restart of a server | The restart of that server finishes to its defined end state (FR-025); servers not yet started are skipped |
| Cancel during `ensure_open` between reading the file and enqueuing `didOpen` | Tracker is not advanced; the next call re-opens normally (FR-026) |
| Cancel after `didOpen` was enqueued but before the tracker was updated | The tracker records it (critical section runs to completion) or the step is made idempotent, never a double `didOpen` (FR-026) |
| Cancel during eviction `didClose` flush holding a pending-close claim | The claim is released, unsent closes stay pending (FR-027) |
| Cancel of a diagnostics pull | Nothing stored in `Pulled`, no subscriber notification (FR-029) |
| Cancel during `JoinSet` of code-action resolves | Subtasks aborted, each outstanding resolve id cancelled once (FR-020) |
| Fan-out where one server is slow and one fast | Fast server's finished request gets no cancel; slow server's gets one (FR-018) |
| Transport EOF with calls in flight | Handlers stop and outstanding LSP requests are cancelled (FR-023); no process-lifetime leak |
| Client cancels a request id that is unknown or already completed | Ignored by the transport; no effect |
| Client cancels `initialize` | Ignored, per the MCP utility; unchanged |
| Duplicate `notifications/cancelled` for one id | No second `$/cancelRequest` (FR-008) |
| Cancelled mutating tool (a tool whose result a client applies, or `restart_server`) | Only the stop-at-next-await and cancel rules apply; no rollback is implied; see FR-025 |

## 7. Success Criteria

| ID | Metric | Baseline | Target |
|----|--------|----------|--------|
| SC-001 | `$/cancelRequest` sent for a client-cancelled in-flight LSP request | 0 | exactly 1 per outstanding id |
| SC-002 | `$/cancelRequest` sent for a timed-out LSP request | 0 | exactly 1 per timed-out id |
| SC-003 | Time from client cancel to handler return | up to request timeout plus retries (tens of seconds) | at most 100 ms (NFR-003) |
| SC-004 | New LSP request ids allocated after a cancel was observed | up to 3 (retries) plus follow-up steps | 0 |
| SC-005 | `warn!` or `error!` lines caused by a late reply to a cancelled or timed-out request | 1 per timeout today (unknown-id warning) | 0 |
| SC-006 | Duplicate `$/cancelRequest` for one id under any race in the test matrix | n/a | 0 |
| SC-007 | Behavioral diff for uncancelled calls (request sequence, results, warn+ logs, `tool_surface.json`) | n/a | 0 |
| SC-008 | Document-tracker inconsistencies after cancel at each await point of `ensure_open` (property or exhaustive-await-point test) | untested | 0 |
| SC-009 | Handlers still running after serve shutdown following stdio EOF | unbounded | 0 |
| SC-010 | Deterministic tests: cancel-before-start, mid-request, mid-backoff, timeout, completion race, fan-out, restart race, EOF | 0 | all present and green on 3 OSes |

## 8. Agent Boundaries

### Always (without asking)
- Thread cancellation through the single shared mechanism; reuse the existing `tokio-util` `CancellationToken` and typed `RequestId`.
- Keep uncancelled behavior byte-identical (NFR-001); run the golden `tool_surface.json` test unchanged.
- Write scripted fake-server tests that record `$/cancelRequest`; use paused time, not sleeps.
- Update `CHANGELOG.md` `[Unreleased]`, the user guide and `.local/testing/` playbooks.
- Run the commands in the project's "Before Every Commit" section.

### Ask First
- Aborting a `restart_server` or a `didOpen`/`didChange` critical section by dropping it rather than running it to a safe point (FR-025, FR-026).
- Adding any dependency, config key, env var or CLI flag for cancellation.
- Changing request timeouts, retry budgets or the retry allowlist.
- Cancelling request types other than `tools/call` (resources, completion).

### Never
- Kill or restart a language server as a way to cancel a request.
- Send `$/cancelRequest` for an id that completed, timed out already, or belongs to a different client instance.
- Block a cancelled caller on a dead or full server queue.
- Add an unbounded record of cancelled ids.
- Convert a cancel into an error response to the client, or log it as a tool failure.
- Match on error message text to decide "cancelled by us" (FR-016).

## 9. Open Questions

> [!question] Unresolved
> - [NEEDS CLARIFICATION: issue number for this enhancement; none exists yet]
> - [NEEDS CLARIFICATION: how to deliver FR-023 on EOF. Verified that rmcp 3.5.1 does not cancel request tokens at EOF (it waits up to 5 s then closes; handlers are detached). Is an mcpls-owned connection token fired when `serve` returns the accepted approach, or should a drop-guard on the running service be used? Plan assumes the connection token.]
> - [NEEDS CLARIFICATION: `restart_server` (FR-025): confirm "run a begun restart to its defined end state" rather than abandon-at-await. Plan assumes yes.]
> - [NEEDS CLARIFICATION: bound for the cancel enqueue (NFR-004): non-blocking `try_send` with a debug log on a full queue (capacity 100), or a short bounded wait? Plan assumes `try_send` plus a spill guarantee that the pending entry is removed regardless.]
> - [NEEDS CLARIFICATION: scope beyond `tools/call`: do `resources/read` or any non-tool handler issue LSP requests at HEAD? Plan assumes no and keeps them out (Ask First).]
> - [NEEDS CLARIFICATION: `$/cancelRequest` for requests whose reply mcpls no longer needs but that are idempotent warm-ups (for example indexing probes): cancel as well, or let the probe finish to warm the server cache? Plan assumes cancel for all request-issuing paths, with the probe path called out for live testing.]
> - [NEEDS CLARIFICATION: which tools fan out across more than one server for a single request at HEAD? Verified: `restart_server` (`buffer_unordered`), code-action resolve (one server, concurrent), enclosing-symbol enrichment (one server per file). Whether per-file multi-claimant diagnostics issue concurrent pulls to several servers needs confirmation in the plan's codebase pass.]

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[mcp/015-request-cancellation-propagation/plan|plan]] — technical plan for this spec
- [[lsp/005-lsp-content-modified-retry/spec|lsp/005]] — the retry loop that cancellation must interrupt
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] — bulk failure of pending requests on respawn
- [[mcp/008-manual-lsp-server-restart/spec|mcp/008]] — `restart_server` fan-out and generation handling
- [[bridge/002-document-tracker-synchronization/spec|bridge/002]] — `ensure_open` serialization that FR-026 must preserve
- [[mcp/003-mcp-2026-stateless-adoption/spec|mcp/003]] — `subscriptions/listen` is the one handler that already observes its token
