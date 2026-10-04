---
aliases:
  - LSP Child Process Lifetime Binding
  - Orphaned LSP Servers
tags:
  - sdd
  - spec
  - enhancement
  - lsp-bridge
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec]]"
---

# Feature: Bind LSP Child Process Trees to mcpls's Lifetime

> [!info] Metadata
> **Author**: Andrei G.
> **Issue**: #526 (residual scope of #470)

## 1. Overview

### Problem Statement

Spawned LSP servers are reaped only when Rust code runs: `kill_on_drop`, `Translator::shutdown_servers`
(bounded by `lsp::SHUTDOWN_TIMEOUT`, #530), and the CLI's guarded runtime shutdown after a panic
(release builds unwind since #530). None of these run when mcpls is `SIGKILL`ed or OOM-killed,
exits via the forced `exit(1)` on a second signal during shutdown, or panics inside a `Drop`
while unwinding (the cases listed under "Limitations" on `Translator::shutdown_servers`), so
every configured server is orphaned. Grandchildren of a server (`cargo check`,
`proc-macro-srv`, `node` behind an npm `.cmd` shim) are outside `kill_on_drop` even on a clean
exit, because it signals only the direct child.

Many servers exit on their own when stdin reaches EOF or when the `processId` mcpls already sends
in `initialize` disappears, but that is per-server behaviour, does not cover a hung server, and
does not cover grandchildren.

### Constraints

- **No `unsafe` anywhere in mcpls crates**: this batch also sets `unsafe_code = "forbid"`
  workspace-wide (#419); the Windows file-type check in `bridge/state.rs` uses `winapi-util` instead of raw FFI. `unsafe` inside third-party dependencies is acceptable.
- No `pre_exec` hook (it requires `unsafe`), so Linux `PR_SET_PDEATHSIG` is unavailable. It
  would be wrong anyway: it fires when the *forking thread* exits, and tokio retires idle
  blocking-pool threads.
- `mcpls-core` is a library, so the mechanism must not depend on the `mcpls` binary
  re-executing itself.

### Design

**Unix (Linux and macOS, identical): lifeline watchdog process group.**
On every LSP spawn, mcpls starts one watchdog per server: `/bin/sh -c <script>` with
`process_group(0)` (it leads a new process group), `env_clear()`, `current_dir("/")`,
stdin = a pipe whose write end only mcpls holds, stdout/stderr = null. The script ignores
`HUP INT TERM QUIT USR1 USR2 ALRM PIPE`, blocks on `read` until stdin reaches EOF, then runs `kill -s KILL 0`
(SIGKILL to its own process group). Every LSP server is spawned with
`process_group(<its watchdog's pgid>)`, so the server and every descendant that does not create
its own group or session join that server's group. Dropping or terminating one `ServerProcess`
closes only its own pipe and kills only its own group.

The kernel closes mcpls's end of the pipe on **any** exit (clean, panic, `exit(1)`, `SIGKILL`,
OOM), so the watchdog observes EOF and kills the whole group. mcpls never writes to the pipe.
There is no registration protocol, and therefore no PGID-reuse hazard: the group id stays
valid for as long as its leader, the watchdog, lives. (A single shared watchdog signalling a
list of server-led groups was rejected: a recycled pgid could SIGKILL an unrelated group.)

**Windows: Job Object.** Each server is spawned via `process-wrap` (`JobObject` + `KillOnDrop`
wrappers). It creates the process suspended, assigns it to a job with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, then resumes it. mcpls owns the job handle. The kernel
closes it on any mcpls exit and terminates every process in the job. Dropping the server handle
(shutdown, respawn) also kills that server's whole tree. The job is created **without**
`JOB_OBJECT_LIMIT_BREAKAWAY_OK`.

### Per-platform behaviour

| Exit path | Linux / macOS | Windows |
|-----------|---------------|---------|
| Clean exit of the mcpls *process* | in-process shutdown, then the watchdog sweeps leftover descendants | job close kills the tree |
| Panic, `exit(1)` on second signal, panic in `Drop` | watchdog (pipe EOF) | job close |
| `SIGKILL` / OOM kill / forced exit | watchdog (pipe EOF) | kernel closes the job handle |
| One server respawned, restarted or shut down while mcpls runs | the server's whole group is killed (descendants that left the group excepted) | whole tree killed |
| `serve()` returns inside a longer-lived embedding host | trees already killed when each server is shut down or dropped | trees already killed by handle drop |

## 2. User Stories

- **US-001**: As a user whose MCP client force-kills mcpls, I want no `rust-analyzer` (multi-GB)
  or its children left running afterwards. On Unix this holds for descendants that stay in the
  lifeline group; descendants that call `setsid()`/`setpgid()`, notably the `cargo check` run by
  rust-analyzer's flycheck, escape and survive (follow-up #541). On Windows the whole tree is killed.
- **US-002**: As a library embedder calling `mcpls_core::serve`, I want the same guarantee without
  shipping an extra binary.

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN the mcpls process exits by any means on Unix THE SYSTEM SHALL SIGKILL every LSP server process and every descendant still in its server's lifeline process group | must |
| FR-002 | WHEN mcpls exits by any means on Windows THE SYSTEM SHALL terminate every process in each LSP server's job object | must |
| FR-003 | WHEN an individual LSP server is shut down, restarted or respawned THE SYSTEM SHALL terminate that server's whole process tree (Windows: its job; Unix: its process group, which also kills shared daemons such as Gradle or Bloop that the server started) | must |
| FR-004 | WHEN the watchdog cannot be started THE SYSTEM SHALL log one warning and spawn the server without lifetime binding in a fresh separate process group (`process_group(0)`, so Ctrl-C does not reach it), never failing the spawn because of the binding | must |
| FR-005 | THE SYSTEM SHALL start a fresh watchdog for every LSP spawn, so a respawn never joins a group whose watchdog is gone | should |
| FR-006 | THE SYSTEM SHALL serialize every watchdog creation together with its server spawn under one process-wide lock, so no LSP child can inherit any lifeline pipe's write end through the non-atomic `pipe()` + `FD_CLOEXEC` sequence std uses on macOS | must |
| FR-007 | WHEN a spawn into the lifeline group fails with `PermissionDenied` AND the watchdog has already exited (`try_wait` is not `Ok(None)`; EPERM: the group vanished because something other than mcpls reaped it) THE SYSTEM SHALL warn and retry the spawn exactly once with a fresh watchdog (or unbound, per FR-004). On any other spawn error THE SYSTEM SHALL drop the idle watchdog, so repeated failing spawns never leak a process | must |
| FR-008 | THE SYSTEM SHALL provide a bounded tree termination for one server (`ServerProcess::terminate_tree`) that is idempotent and used by shutdown and manual restart | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Safety | No `unsafe` in mcpls crates; workspace lint `unsafe_code = "forbid"` |
| NFR-002 | Footprint | One watchdog process per LSP server (a respawn replaces it); each uses only shell builtins |
| NFR-003 | Latency | Group kill completes promptly after mcpls death; tests poll with a 5 s ceiling |

## 5. Data Model

| Entity | Description |
|--------|-------------|
| `lsp::process::ServerProcess` | Owns one spawned server and replaces `LspServer.child: Option<tokio::process::Child>` with `Option<ServerProcess>`; `None` stays the in-memory test-fixture case. Unix wraps `tokio::process::Child`; Windows wraps `Box<dyn process_wrap::tokio::ChildWrapper>`. API: `spawn(Command)`, `take_stdin`/`take_stdout`, `try_wait`, `wait` (leader only, still awaited under `LspServer::shutdown`'s `deadline.min(CHILD_EXIT_GRACE)`), and a `Drop` that kills. Test-only `from_unbound(tokio::process::Child)` (unix) for `fake_lsp_server_with_dead_loop_and_live_child`. |
| `ProcessGroupId` (unix) | Newtype over `i32`, constructible only from a live watchdog's pid |
| `Lifeline` (unix) | The watchdog's `tokio::process::Child`, the write end of its pipe and its `ProcessGroupId`. Owned by the `ServerProcess` it leads the group of; dropping or closing it kills that group. A process-wide `static Mutex<()>` only serializes spawns (FR-006). |

## 6. Edge Cases and Error Handling

| Scenario | Behaviour |
|----------|-----------|
| **Intentional daemon (Windows)**: a descendant meant to outlive its spawner (shared build server, file watcher, telemetry agent) | Killed on server respawn, per-server shutdown and mcpls exit. Because the job lacks `BREAKAWAY_OK`, a descendant that passes `CREATE_BREAKAWAY_FROM_JOB` gets `ERROR_ACCESS_DENIED` from `CreateProcess` and its own spawn fails. **Intended**: children dying with the parent is the point of #526. Documented in README and CHANGELOG; no mitigation. |
| **Intentional daemon (Unix)** that does not call `setsid()`/`setpgid()` (e.g. Gradle daemon started by jdtls, Bloop/sbt for metals) | Killed when the server is respawned, restarted or shut down, and when mcpls exits, even if other clients use it. Intended, documented in README and CHANGELOG. |
| A descendant calls `setsid()`/`setpgid()` (Unix) | It leaves the group and is **not** killed: best effort only. Verified: rust-analyzer's flycheck `cargo check` runs in its own session and survives `kill -9` of mcpls (Windows is unaffected). Tracked in #541. |
| Watchdog start fails (Unix) vs. job creation or assignment fails (Windows) | Unix: warn once and spawn the server unbound in a fresh process group (it dies only by the in-process paths). Windows: the server spawn fails with the error. |
| A descendant opens `/dev/tty` (ssh/git credential prompt during `cargo fetch`, pinentry) while mcpls runs in an interactive terminal | The lifeline group is a background group of that terminal, so the descendant gets `SIGTTIN`/`SIGTTOU` and stops; that flycheck run hangs. Not a concern when an MCP client spawns mcpls without a controlling terminal (the normal case). Documented. |
| Ctrl-C (SIGINT) in the controlling terminal | Servers are in the lifeline group, not the terminal's foreground group, so they no longer receive it directly; mcpls handles the signal and shuts them down gracefully |
| Watchdog is itself `SIGKILL`ed while mcpls lives | It kills only its own server's group and binding is lost for that server; the next spawn of any server gets a fresh watchdog |
| Watchdog dies between its start and the server spawn (zombie-led group) | FR-007 applies; if it dies after the spawn the server is unbound. Accepted residual race. |
| Watchdog reaped by another party (embedder with `SIGCHLD = SIG_IGN` or a `waitpid(-1)` reaper) | The spawn gets EPERM and the watchdog is dead; FR-007 applies |
| mcpls dies between `spawn()` and the child's `setpgid` | Not possible: `setpgid` runs in the child before `exec` |
| A server crashes and is respawned (Unix) | Its surviving grandchildren in the group are killed with the old `ServerProcess` (#542) |
| Host process (library embedder) forks concurrently on macOS | Its children may inherit the pipe write end and delay EOF. Documented; outside mcpls's control. |
| `/bin/sh` missing | FR-004 |

## 7. Success Criteria

| ID | Criterion |
|----|-----------|
| SC-001 | Unix e2e (`#[cfg(unix)]`, `#[ignore = "Requires mcpls binary built"]` so the CI e2e job runs it): an LSP "server" script forks a grandchild and never answers `initialize`. After `SIGKILL` of the mcpls binary, both pids are gone within the 5 s polling ceiling. |
| SC-002 | Unix unit test: closing a test-local lifeline's pipe kills a process spawned into its group |
| SC-003 | Unix unit tests: joining a vanished group fails with `PermissionDenied` and a fresh spawn starts a new watchdog; a `PermissionDenied` with a live watchdog does not retry; a failed spawn leaves no idle watchdog |
| SC-006 | Unix unit tests: `terminate_tree` and drop kill a server's grandchild; terminating one server leaves another server's grandchild alive; a respawn kills the crashed server's grandchild |
| SC-004 | Windows unit test: a server started via PowerShell `Start-Process -PassThru` writes its grandchild's pid to a file; after `ServerProcess` is dropped, `tasklist /FI "PID eq <pid>"` no longer lists it |
| SC-005 | `rg 'unsafe' crates/ --type rust` finds no `unsafe` block or `allow(unsafe_code)`; the workspace builds with `unsafe_code = "forbid"` |

## 8. Agent Boundaries

- **Always**: keep `kill_on_drop(true)` on the leader as the in-process fast path.
- **Ask first**: any registration protocol with the watchdog; any breakaway allowance on the
  Windows job. (Per-server process groups were approved for #542.)
- **Never**: `pre_exec`, any `unsafe` or `allow(unsafe_code)`, or a `mcpls` re-exec supervisor.

## 8a. Changes

- #542: the process-wide watchdog is replaced by one watchdog per server so that respawn,
  restart and shutdown kill the server's whole group; NFR-002 grows from one to N `sh`
  processes, and group kill on respawn also kills shared daemons (Gradle, Bloop).

## 9. See Also

- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|spec lsp/001]] — graceful, deadline-bounded
  shutdown (FR-011) and respawn, which this spec complements rather than replaces. Without this
  spec, its US-004 "no orphans" story holds only on the in-process paths.
