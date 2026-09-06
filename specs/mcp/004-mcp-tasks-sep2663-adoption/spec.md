---
aliases:
  - MCP Tasks (SEP-2663) adoption tracking
tags:
  - sdd
  - spec
  - research
  - mcp
created: 2026-09-06
status: draft
related:
  - "[[constitution]]"
  - "[[mcp/003-mcp-2026-stateless-adoption/spec|mcp-2026-stateless-adoption]]"
---

# Feature: Track MCP Tasks (SEP-2663) Adoption for Long-Running Tool Requests

> [!info] Metadata
> **Author**: rust-architect + rust-critic (team-develop architecture review for #119/#137)
> **Type**: research / deferred design
> **Priority**: P4
> **Related issues**: #119 (this finding), #137 (structured tool output — implemented and shipped
> separately in PR #397, unaffected by this deferral)

> [!warning] Implemented, reviewed, and explicitly descoped — not merely unstarted
> This is not a "hasn't been looked at yet" backlog item. A full architecture design for #119 was
> produced and adversarially critiqued during the #119+#137 team-develop cycle (2026-09-06). The
> design was **workable** (rmcp 3.2.0 fully supports the required primitives — see §1) but
> critique found it **not safe to ship as designed**: useless in its safe (off-by-default)
> configuration and non-conformant with the SEP-2663 spec in its enabled configuration. The
> team-lead descoped it from PR #397 rather than ship either horn of that dilemma. This spec
> preserves the completed research so a future implementation pass starts from verified facts,
> not from zero.

## 1. Overview

### Problem Statement

The Model Context Protocol's Tasks primitive (SEP-2663, part of the 2025-11-25 spec revision)
lets a tool call be augmented with an asynchronous task ID: the client polls `tasks/result`
instead of blocking on the call. For mcpls, an LSP bridge, this matters because some LSP
round-trips can legitimately run long against a large workspace — `find_references` and
`rename_symbol` against rust-analyzer on a big monorepo, for instance.

### What was verified (rmcp 3.2.0, not the issue's original rmcp 1.5.0-era assumption)

Issue #119 was filed against rmcp 1.5.0-era type names (`TasksCapability`, `ToolsTaskCapability`,
`GetTaskResultRequest`, `CancelTaskResult`). The workspace now pins `rmcp = "3.2.0"`. Verified
directly against the vendored registry source
(`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/rmcp-3.2.0/`), rmcp 3.2.0 **fully
supports** the server-side Tasks primitive — this is not a "wait for upstream" blocker:

| Item | Location |
|---|---|
| `rmcp::task_manager` module (unconditional, not feature-gated) | `src/lib.rs:30` |
| `TaskManager` (`Clone`, `Arc<Mutex<_>>` inner) — `spawn`, `get_task`, `update_task`, `cancel_task`, `running_task_count`, `shutdown` | `src/task_manager.rs:299-505` |
| `TaskOptions { ttl_ms, poll_interval_ms, status_message }`, `DEFAULT_TASK_TTL_MS = 300_000` | `src/task_manager.rs:39,42,229` |
| `TaskContext::cancelled()` / `is_cancel_requested()` | `src/task_manager.rs:100-146` |
| `CallToolResponse::{Complete, InputRequired, Task}` — `call_tool` return type | `src/model/mrtr.rs:104-114` |
| `CreateTaskResult::new(task)`, `Task`, `TaskStatus`, `DetailedTask`, `GetTaskResult::new` | `src/model/task.rs:282,48,21,165,362` |
| `ServerCapabilities::builder().enable_tasks()` | `src/model/capabilities.rs:458` |
| `ServerHandler::{get_task, update_task, cancel_task}` — default `method_not_found`, override to enable | `src/handler/server.rs:561,572,583` |
| `#[tool_handler]` skips generating `call_tool` when the impl already defines one | `rmcp-macros-3.2.0/src/tool_handler.rs:44` |
| Canonical server integration pattern | `rmcp-3.2.0/tests/test_task.rs:40-120` |

### The actual blocker: a protocol asymmetry, not a missing primitive

rmcp 3.2.0 has two real gaps that make adoption unsafe, not merely inconvenient:

- `CallToolRequestParams` (`src/model.rs:4054-4071`) has **no `task` field** — a client cannot
  opt into task-mode execution *per call*, which is what the SEP-2663 spec's opt-in model
  requires.
- `Tool` (`src/model/tool.rs:17-40`) has **no `execution.taskSupport`** — the server cannot
  advertise task eligibility *per tool* either.

Consequence: task-vs-sync dispatch can only be decided **server-side**, keyed on nothing more
than whether the client declared the `tasks` capability at all. There is no way to ask "does this
specific client, for this specific call, want a task?" — only "does this client support tasks at
all?" This is the crux the reviewed design could not resolve without either being useless or
non-conformant (see §3).

## 2. What the reviewed (and rejected-for-now) design looked like

For a future implementation to build on real prior work rather than re-deriving it, the
adversarially-reviewed design is preserved here:

- **Scope**: 3 tools — `get_references`, `rename_symbol`, `get_diagnostics`. `get_completions`
  was explicitly excluded (latency-critical, already capped at a 10s timeout; a poll round-trip
  would cost more than it saves).
- **Gate**: a config flag `mcp.tasks.enabled` (default `false`) plus a fixed, non-configurable
  eligible-tool list (no per-call or per-tool opt-in exists in rmcp 3.2.0 to build a finer gate
  on — see §1).
- **New module**: `crates/mcpls-core/src/mcp/tasks.rs` — `TaskManager` held on `McplsServer`,
  `call_tool`/`get_task`/`update_task`/`cancel_task` overridden inside the existing
  `#[tool_handler]` impl block.
- **DRY seam**: rejected as unnecessary once #137 (structured output) was ready — no `_impl`
  extraction is needed unless a future design actually revives dual sync/task dispatch for the
  same handler bodies.
- **HTTP session isolation**: `TaskManager` clones share one `Arc<Mutex<_>>` state; the HTTP
  transport hands every session a clone of the same `McplsServer` (`transport.rs:437`), so a
  `new_session()` constructor recreating `TaskManager` per session would be needed. (Critique
  downgraded the "cross-tenant state leak" framing of this — `Arc<BridgeContext>` is already
  global per process by design, and task IDs are UUIDv4 — but keeping per-session isolation is
  still correct practice if Tasks ships at all.)
- **Cancellation — honest limitation**: `tasks/cancel` can only settle the task's status as
  `cancelled`; `lsp/client.rs` has no `$/cancelRequest`, so the underlying LSP request keeps
  running until its own `request_timeout()` elapses. Any future implementation must document this
  rather than imply real request cancellation.

## 3. Why this was descoped (critique findings)

Independent architecture critique (2026-09-06) returned a `significant` verdict on this design,
not because any cited API was wrong, but because of three design/operational gaps:

- **TTL default was inverted, not a safety margin.** A naive default `ttl_seconds = 300` is
  *below* the workspace's own configurable worst-case LSP path
  (`MAX_TIMEOUT_SECONDS = 900` in `crates/mcpls-core/src/config/server.rs` →
  `4 * 900 + 3.5 ≈ 3603s` worst case). An operator who raises `request_timeout_seconds` would get
  tasks killed by the TTL sweep while the underlying LSP call is still legitimately in flight. Any
  future design must either validate `ttl_seconds` against the configured `request_timeout_seconds`
  at config load, or derive the default from it.
- **Session-isolation fix and shutdown wiring were mutually inconsistent as specified** — a
  `new_session()`-per-HTTP-session `TaskManager` construction makes any shutdown hook on the
  factory-held `McplsServer` a no-op, since the factory prototype's `TaskManager` is never the one
  handling real requests. A future design must wire shutdown per real session, or drop the
  shutdown call entirely (SIGTERM ends the process regardless, so this is close to cosmetic).
- **The core finding**: with `mcp.tasks.enabled = true`, mcpls would not be SEP-2663 conformant —
  merely "opinionated," as the reviewed design characterized it, understates the gap. The spec's
  intended opt-in is per-call (`params.task`); since rmcp 3.2.0 has no such field, a client that
  never asked for task execution would receive one anyway whenever it declares the `tasks`
  capability at all. **With the flag off (the safe default), the feature ships zero user value.**
  With the flag on, the feature answers non-conformantly to every call from any tasks-capable
  client, whether or not that specific call wanted async execution.

Team-lead decision: rather than ship a feature that is either inert or protocol-incorrect, #119
stays unimplemented and this spec records the deferral condition.

## 4. Goal

mcpls adopts MCP Tasks (SEP-2663) for long-running tool calls once one of the following becomes
true, at which point this spec should be promoted into an implementation spec following the
pattern in [[lsp/002-lsp317-missing-tools/spec|spec lsp/002]]:

- **Path A — upstream protocol fix.** rmcp (or the underlying MCP spec) adds a per-call opt-in
  (`CallToolRequestParams.task`) and/or per-tool advertisement (`Tool.execution.taskSupport`),
  closing the asymmetry described in §1. This is the conformant path and should be preferred if
  it lands.
- **Path B — latency-triggered hybrid dispatch**, designed but explicitly *not built* during the
  #119/#137 cycle (deferred, not rejected): run every eligible call synchronously under a short
  deadline (e.g. ~1.5s); only if that deadline is exceeded, move the *already-running* future into
  `TaskManager::spawn` and return a task handle for the client to poll. This is the only dispatch
  mode identified so far that could ever be safe to default on — it never returns a task handle to
  a client uninterested in one for a call that would have completed quickly anyway, and it costs a
  fast-path client (e.g. a cache-hit `get_diagnostics`) nothing. `TaskFuture` in rmcp 3.2.0 takes
  an owned boxed future, so moving an in-flight future into it post-hoc is technically feasible,
  but "meaningfully more code" than the fixed-gate design — no implementation attempt has been
  made yet.

### Out of Scope (for this tracking spec)

- Implementing either Path A or Path B now — this spec exists to record verified facts and the
  deferral rationale, not to commit to a timeline.
- Re-opening the #137 (structured output) implementation — that shipped independently in PR #397
  and is unaffected by this deferral.
- The MCP 2026-07-28 stateless revision tracked separately in
  [[mcp/003-mcp-2026-stateless-adoption/spec|spec mcp/003]] — that spec already notes it excludes
  "implementing the redesigned Tasks extension — tracked separately in issue #119," i.e. here.

## 5. Functional Requirements (candidate, gated)

| ID | Candidate Requirement (speculative) | Gating condition |
|----|------|------|
| FR-001 | WHEN rmcp (or the MCP spec) adds a per-call task opt-in field to `CallToolRequestParams` THE SYSTEM MAY implement Path A: gate task dispatch on that field rather than only on client capability | rmcp/spec adds the field (Path A) |
| FR-002 | WHEN a latency-triggered hybrid dispatch design is produced and reviewed THE SYSTEM MAY implement Path B for the 3 previously-scoped tools (`get_references`, `rename_symbol`, `get_diagnostics`) | A hybrid design passes architecture + critic review |
| FR-003 | IF Path B is implemented THEN `ttl_seconds` SHALL be validated at config load against the configured `request_timeout_seconds` (or derived from it), per the critique's TTL finding in §3 | Path B implementation |
| FR-004 | IF per-session `TaskManager` isolation is implemented for the HTTP transport THEN any `TaskManager::shutdown()` wiring SHALL target the actual per-session instance, not the factory prototype | Any implementation touching HTTP transport |
| FR-005 | WHEN either path ships, `$/cancelRequest` support SHOULD be added to `lsp/client.rs` so `tasks/cancel` can abort the underlying LSP request rather than only marking the task record cancelled | Either path's implementation (documented as a known limitation until then) |

## 6. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Conformance | mcpls SHALL NOT ship an `mcp.tasks.enabled` (or equivalent) flag whose "on" state answers non-conformantly to SEP-2663 for any client that did not request task execution for a given call, unless clearly and prominently documented as experimental/non-conformant |
| NFR-002 | No premature action | No source code under `crates/` SHALL be modified as a result of this spec alone — promotion to an implementation spec is required first, per [[lsp/004-lsp-318-draft-gaps/spec\|spec lsp/004]]'s Agent Boundaries pattern |
| NFR-003 | Traceability | This spec SHALL be discoverable from `specs/MOC-specs.md` and cross-linked with issue #119 |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | This spec's premises (rmcp's per-call opt-in gap) are re-checked | At least once per rmcp major/minor version bump that touches `task_manager` or `model.rs`'s `CallToolRequestParams` |
| SC-002 | This spec is promoted to an implementation spec | Only once Path A or Path B's gating condition (§4) is met, following the escalation pattern used for #298/#299 |

## 8. Agent Boundaries

### Always (without asking)
- Re-check rmcp's changelog for `CallToolRequestParams.task` or `Tool.execution.taskSupport` additions during future dependency-update or continuous-improvement cycles.

### Ask First
- Promoting this spec into an implementation spec (new `specs/mcp/00N-*` directory) — even once a gating condition appears met, confirm scope with the user first, per the #298/#299 precedent.

### Never
- Ship an `mcp.tasks.enabled = true` path that silently returns task handles to non-tasks-requesting calls without the NFR-001 disclosure.
- Reintroduce the TTL-vs-`request_timeout_seconds` mismatch described in §3 without the FR-003 validation.

## 9. Open Questions

- [NEEDS CLARIFICATION: will the MCP spec or rmcp add a per-call task opt-in (Path A), or is a latency-triggered hybrid (Path B) the more realistic path? No upstream signal either way as of 2026-09-06.]
