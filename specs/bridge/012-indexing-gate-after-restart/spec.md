---
aliases:
  - Indexing gate after restart
  - False-empty results after restart_server
tags:
  - sdd
  - spec
  - bug
  - bridge
  - lsp
  - reliability
created: 2026-10-06
status: implemented
related:
  - "[[constitution]]"
  - "[[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006-lsp-indexing-readiness-gate]]"
  - "[[mcp/008-manual-lsp-server-restart/spec|mcp/008-manual-lsp-server-restart]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001-lsp-server-lifecycle-and-respawn]]"
  - "[[bridge/013-indexing-state-on-ungated-prepare-tools/spec|bridge/013-indexing-state-on-ungated-prepare-tools]]"
---

# Feature: Apply the indexing gate to a server replaced by a restart or respawn

> [!info] Metadata
> **Type**: bug
> **Priority**: P1
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle, live-test finding
> **Issue**: #667

> [!abstract]
> Right after `restart_server`, whole-workspace read tools return empty results in 0 ms: no hover,
> no references, no definition, no completions. A cold start waits correctly. The readiness gate
> ([[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]]) only waits on a positive "loading"
> signal, and a replaced server has none until it reports one, so for the first moments the gate
> is open. An agent that restarts a server (the documented recovery step) and queries a symbol
> concludes the symbol is unused. This spec makes a replaced server behave like a freshly started
> one.

## 1. Overview

### Problem Statement

The readiness gate waits for a server to finish indexing only while the tracked indexing state is
`Loading`, a positive signal (rust-analyzer's `experimental/serverStatus` with `quiescent: false`,
or a generic progress sequence). `Unknown` (no signal seen) and `Ready` do not wait, by design: a
server that never reports a signal must not be delayed.

A restart and an automatic respawn reset the replaced server's tracked state to `Unknown`
(see [[mcp/008-manual-lsp-server-restart/spec|mcp/008]] FR-005 and
[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]]). The new rust-analyzer needs about two
seconds after `initialize` before it reports `quiescent: false`. In that window the state is
`Unknown`, the gate is open, and rust-analyzer answers from an empty index.

**Observed live** (release binary, rust-analyzer 1.99.0, a tiny cargo project, `restart_server`
with `{"all": true}` followed at once by a query; reproduced 3 of 3 times, plus 3 more with other
tools):

| Tool, first call after restart | Result | Time |
|--------------------------------|--------|------|
| `get_hover` on `add` | `{"contents": "No hover information available"}` | 0 ms |
| `get_references` (the symbol has two references) | `{"locations": []}` | 0.00 s |
| `get_definition` | `{"locations": []}` | 0.00 s |
| `get_completions` | `{"items": []}` | 0.00 s |

- The restart response itself reports `"indexing_state": "unknown"`.
- The next gated call waits about 2 s (the gate engages once the `quiescent: false` notification
  arrives) and returns correct results.
- `workspace_symbol_search` immediately after a restart returned data only on later calls.
- On a cold start the same tools wait (first `-32051` while the server starts, then the gate) and
  never return a premature empty result; verified, including under CPU load (a hover waited 44 s).
- Not verified: the automatic respawn after a crash (the server could not be killed reliably). The
  code path is shared with restart, so the same gap is likely.

The spec [[mcp/008-manual-lsp-server-restart/spec|mcp/008]] and its playbook expect the opposite: a
restarted server "behaves like a freshly started one" and a read tool right after a restart "returns
the existing `WorkspaceIndexing` error rather than empty results while the new server loads".

**Why it matters.** `restart_server` is the documented recovery step for a stuck or stale server,
and it is called exactly when the agent is uncertain about the server's answers. A false empty
result right after it is the worst case of the bug class that bridge/006 was created to close: an
agent concludes a function is dead code, or that a symbol does not exist, and acts on it.

### Goal

After a restart, or an automatic respawn, the replaced server is treated as still loading until it
reports readiness or the gate's bounded timeout elapses, so whole-workspace read tools either
wait or return the retryable indexing error instead of an unqualified empty result. The restart
result does not report `unknown` for a server that is known to be mid-load.

### Out of Scope

- Generic `$/progress`-based detection for servers that never report a signal (tracked in #422).
- Gating `prepare_call_hierarchy`, `prepare_type_hierarchy` and `workspace_symbol_search`
  ([[bridge/013-indexing-state-on-ungated-prepare-tools/spec|bridge/013]] covers the prepare tools;
  `workspace_symbol_search` stays as decided by bridge/006).
- Changing the bounded gate timeout value (30 s).
- Re-arming the gate on re-indexing after the initial load.
- Changing the restart sequence (termination order, cooldown, process-group reaping).
- Technical design: recorded in a plan after this spec is approved.

## 2. User Stories

### US-001: Query right after a restart

AS AN AI coding agent that has just called `restart_server`
I WANT `get_references` and the other whole-workspace read tools to wait or return the retryable
indexing error
SO THAT I never mistake an empty index for a symbol without references.

**Acceptance criteria:**
```
GIVEN a rust-analyzer server restarted a moment ago on a project where `add` has two references
WHEN get_references is called for `add` immediately after restart_server returns
THEN the call waits for the server to be ready, or fails with the retryable indexing error
  AND it does not return {"locations": []}
```

### US-002: Same behavior for every gated tool

AS AN AI coding agent
I WANT the same protection on hover, definition, completions and code actions
SO THAT the whole read surface is consistent after a restart.

**Acceptance criteria:**
```
GIVEN a server restarted a moment ago
WHEN get_hover, get_definition, get_completions or get_code_actions is called at once
THEN none of them returns an unqualified empty result computed from an unloaded index
```

### US-003: The restart result is truthful

AS AN AI coding agent reading the result of `restart_server`
I WANT the reported indexing state to say that the replacement is loading when it is
SO THAT I know to expect a wait.

**Acceptance criteria:**
```
GIVEN a restart of a server whose kind reports readiness signals
WHEN restart_server returns
THEN the per-server indexing_state is not "unknown" while the replacement has not yet reported ready
```

### US-004: Servers without a readiness signal are not delayed

AS A user of a server that never reports readiness (no recognized signal)
I WANT a restart to add no artificial delay to my first call
SO THAT the fix does not regress the common case.

**Acceptance criteria:**
```
GIVEN a server that has never emitted a recognized readiness signal, restarted
WHEN a gated read tool is called at once
THEN it is answered without a fixed delay beyond what a cold start of that server costs
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN a server is replaced by `restart_server` or by an automatic respawn THE SYSTEM SHALL treat the replacement as loading until it reports readiness or the bounded gate timeout elapses, for servers that report readiness signals | must |
| FR-002 | WHEN a whole-workspace read tool (`IndexingGate::Required`) is called while the replacement is in that state THE SYSTEM SHALL wait within the gate's bounded timeout, or return the retryable `WorkspaceIndexing` error, and SHALL NOT return an unqualified empty result | must |
| FR-003 | WHEN the replacement reports readiness THE SYSTEM SHALL stop gating without further delay (no added latency in the ready case) | must |
| FR-004 | WHEN the bounded timeout elapses without a readiness signal THE SYSTEM SHALL end the gate, so a server that stops reporting is never blocked indefinitely | must |
| FR-005 | THE per-server `indexing_state` in the `restart_server` result SHALL NOT be `unknown` for a replacement that is known to be mid-load | must |
| FR-006 | THE SYSTEM SHALL decide per server whether the state after replacement is "loading until signal" or "unknown": a server that has reported a recognized readiness signal in its earlier life SHALL be treated as one that will report again; a server that never has SHALL keep today's fail-open behavior | must |
| FR-007 | WHEN a server is replaced THE SYSTEM SHALL apply FR-001 to both replacement paths with one shared mechanism, not two copies | must |
| FR-008 | WHEN a server is replaced THE SYSTEM SHALL keep the other servers' tracked state untouched ([[mcp/008-manual-lsp-server-restart/spec\|mcp/008]] NFR-002) | must |
| FR-009 | THE existing indexing-readiness behavior of a cold start SHALL NOT change | must |
| FR-010 | THE diagnostics tools SHALL keep reporting `indexing_in_progress` and SHALL report it as true for a replacement in the FR-001 state | should |
| FR-011 | THE playbook and the coverage status for restart under `.local/testing/` SHALL gain the case "query immediately after restart" and be reset | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | The pre-signal condition is an explicit typed state or typed provenance, not a boolean flag and not an inferred combination of timestamps, per [[constitution]] |
| NFR-002 | Latency | A server that never reports readiness pays no new fixed delay after a restart (US-004); a server that reports readiness is delayed only until it does, and never beyond the existing 30 s bound |
| NFR-003 | Reliability | No path leaves a gate armed forever; the state always ends in ready, timed out, or reset by another replacement |
| NFR-004 | Concurrency | Two replacements of one server, or a replacement during a gated call, leave one consistent state; a gated call in flight on the old process fails with the existing retryable restart error, not an empty result |
| NFR-005 | Consistency | A cold start and a restart of the same server on the same project produce the same observable sequence for a first query |
| NFR-006 | Observability | The state transition into and out of the pre-signal condition is logged at debug level with the server id |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Indexing state (existing) | Per-server `Unknown`, `Loading`, `Ready` | tracked in the notification cache |
| Pre-signal condition (new) | The replacement has not yet reported; the gate treats it as loading | server id, started-at instant, whether the server's kind has reported before |
| Signalling history (new or derived) | Whether a server id has ever reported a recognized signal | per server id; survives replacement |
| Restart outcome (existing) | Per-server result of `restart_server` | `indexing_state` among other fields |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Restart, then a hover within milliseconds (rust-analyzer) | Waits or returns `WorkspaceIndexing`; never an unloaded-index empty (FR-002) |
| Restart of a server that never reports readiness (for example pyright) | No new delay (FR-006, US-004) |
| Replacement never reports ready | The gate ends at the bounded timeout (FR-004), after which the tool answers as today |
| Replacement reports `quiescent: true` before any `quiescent: false` | Treated as ready at once (FR-003) |
| `restart_server {"all": true}` with one signalling and one non-signalling server | Each server follows its own rule (FR-006); the result shows per-server states |
| A gated call in flight when the restart happens | Fails with the existing retryable `server_restarted` error ([[mcp/008-manual-lsp-server-restart/spec\|mcp/008]]) |
| Automatic respawn after a crash | Same mechanism (FR-007); live verification is pending (the crash could not be induced reliably) |
| Two restarts in a row within the cooldown | The second is throttled by the existing rule; state is unchanged by a throttled call |
| Restart fails (replacement does not initialize) | The server is not registered; the existing failure outcome stands; no gate is armed |
| First restart of a server that has never produced a signal because nobody queried it yet | Signalling history is empty, so the server is not gated; `[NEEDS CLARIFICATION: see section 9, history source]` |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | `restart_server {"all": true}` then at once `get_references` on a symbol with two references (rust-analyzer, tiny cargo project), 10 repetitions | 0 of 10 return `{"locations": []}`; each waits and returns both locations, or fails with the retryable indexing error |
| SC-002 | Same sequence for `get_hover`, `get_definition`, `get_completions` | 0 premature empty results out of 10 each |
| SC-003 | Restart result `indexing_state` for rust-analyzer | Not `unknown` while mid-load |
| SC-004 | Restart of a non-signalling server (pyright or clangd) followed by a query | Latency within the cold-start latency of that server plus noise; no fixed added delay |
| SC-005 | Cold-start behavior of the same tools | Unchanged |
| SC-006 | Unit test: replacement state, readiness arrival, timeout, and a second replacement | Pass |
| SC-007 | Live check of the automatic respawn path (kill the server process) when a reliable method exists | Same as SC-001 |

## 8. Agent Boundaries

### Always (without asking)
- Reproduce with a release binary and a tiny cargo project, and keep the repro as a regression case under `.local/testing/`.
- Share one mechanism between the restart path and the respawn path.
- Model the pre-signal condition as a typed value (NFR-001).
- Run the full pre-commit suite and update `CHANGELOG.md` with the PR link.

### Ask First
- Where the "this server reports signals" knowledge comes from (history of the server id, the server kind, or configuration).
- Whether the `restart_server` result gains a new `indexing_state` variant or reports `loading`.
- Any change to the bounded gate timeout.

### Never
- Delay a server that has never reported a signal by a fixed time.
- Weaken or bypass the gate for any tool to "fix" this.
- Return an unqualified empty result as the answer to a gated call on a replaced server.
- Leave a gate armed without a bound.

## 9. Resolutions

- **Root cause (confirmed).** The reset to `Unknown` on replacement opens the gate until the first `quiescent: false`: the live trace logged the restart result 0.8 ms before the first `quiescent: false`. A cold start is not affected because its first status notification precedes server registration.
- **Source of the signalling knowledge (FR-006): option (a).** The tracker keeps, per server id, the signal source it has ever reported; the record survives a reset like the indexing policy. A server without that history is not gated.
- **Restart result.** The pre-signal condition reads `loading`; no new `indexing_state` value.
- **Duration.** The configured indexing-ready timeout (`workspace.indexing_ready_timeout_seconds`, default 30 s) counted from the replacement; a signal ends it at once.
- **Shape.** The tracked state is a typed `TrackedIndexing { AwaitingFirstSignal { since, within }, Signalled(entry) }`; `IndexingReset::AwaitReplacement { within }` seeds it in `respawn_locked` before the replacement's notification consumer starts, for both the restart and the automatic respawn (the consumer of an automatic respawn forwards the lifecycle lane too, so signals arrive). While awaiting, `$/progress` frames of a server whose history is `experimental/serverStatus` do not end the wait.
- **Automatic respawn, live check.** Still pending (no reliable way to kill the server process without the watchdog); the unit tests cover the shared path.
- **Playbook.** Whether the mcp/008 playbook was exercised live with an immediate query is recorded in the coverage status under `.local/testing/`.

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]] — the gate, its three states and its bounded wait
- [[mcp/008-manual-lsp-server-restart/spec|mcp/008]] — `restart_server`, its state reset and the expectation this spec restores
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] — automatic respawn
- [[bridge/013-indexing-state-on-ungated-prepare-tools/spec|bridge/013]] — related gap: ungated tools mid-index
- Code: `crates/mcpls-core/src/bridge/indexing.rs` (`IndexingTracker::state`, `reset`), `crates/mcpls-core/src/bridge/translator/respawn.rs` (`respawn_locked`), `crates/mcpls-core/src/bridge/translator/restart.rs`, `crates/mcpls-core/src/bridge/translator/routing.rs` (`IndexingGate`, `wait_for_indexing_ready`)
