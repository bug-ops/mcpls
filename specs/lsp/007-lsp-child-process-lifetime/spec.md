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
> **Issue**: #526 (residual scope of #470); #541 (descendants that `setsid`); #542 (respawn leaves the previous server's descendants); #591 (restart_server and respawn reap the tree)

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

Many servers exit on their own when stdin reaches EOF or when the `processId` mcpls sends
in `initialize` disappears, but that is per-server behaviour, does not cover a hung server, and
does not cover grandchildren.

A first Unix design (#546) put every server in one watchdog-led process group. It did not cover
descendants that call `setsid()`/`setpgid()`, notably rust-analyzer's flycheck `cargo check` (#541),
nor the helpers a crashed server leaves in the shared group until mcpls exits (#542).

### Constraints

- **No `unsafe` anywhere in mcpls crates**: this batch also sets `unsafe_code = "forbid"`
  workspace-wide (#419); the Windows file-type check in `bridge/state.rs` uses `winapi-util` instead of raw FFI. `unsafe` inside third-party dependencies is acceptable. Signals are sent through `rustix` (safe API).
- No `pre_exec` hook (it requires `unsafe`), so Linux `PR_SET_PDEATHSIG` is unavailable. It
  would be wrong anyway: it fires when the *forking thread* exits, and tokio retires idle
  blocking-pool threads.
- `mcpls-core` is a library, so the mechanism must not depend on the `mcpls` binary
  re-executing itself.

### Design

**Unix (Linux and macOS, identical): one lifeline per server.** Under one process-wide spawn lock
(FR-006) mcpls starts, per server:

1. **Anchor**: `/bin/sh` with `process_group(0)`; its pid is the server's group `G`. It ignores the
   usual termination signals, reads its own pipe (mcpls holds the write end) until EOF, then
   `exec sleep 60`. Its only job is to keep `G` reserved while the group is signalled; it also holds
   a copy of the read end of the server's stderr pipe (mcpls creates that pipe itself with
   `std::io::pipe`).
2. **The server**, with `process_group(G)` and `kill_on_drop(false)`.
3. **Watchdog**: `/bin/sh` with `process_group(0)` (outside `G`), `env_clear()` (plus a fixed `PATH`),
   `current_dir("/")`. Its stdin is one end of a `UnixStream::pair()` (mcpls keeps the other end), its
   stdout is a dup of the server's stdin write end and its stderr a dup of the server's stdout read
   end; the script moves those to fds 3 and 4 and points stdout/stderr at `/dev/null`. The argv is
   `G`, the leader pid, the `ps` program (a test seam) and the awk program.

Because the watchdog holds the server's stdin and stdout and the anchor holds its stderr, the server
**cannot learn that mcpls died** (EOF on stdin, `EPIPE`/`SIGPIPE` on stdout or stderr) before the
watchdog has frozen the tree: writes block instead of failing. `initialize` additionally sends
`processId: null` while a watchdog is bound (LSP allows it), so a server that watches its parent pid
cannot race the freeze either.

The watchdog reads line commands from the socket. On **EOF** (the kernel closes mcpls's end on *any*
exit: clean, panic, `exit(1)`, `SIGKILL`, OOM; mcpls also half-closes it on purpose to start a sweep) it
runs the **sweep**:

1. `SIGSTOP` `G` (the first action; the watchdog is outside `G`, so it keeps running).
2. Up to 3 passes: snapshot `ps -A -o pid= -o ppid= -o pgid=` into a temp file (the `ps` job runs in
   the background and is polled for at most 2 s, never waited on, so a hung `ps` cannot hang the
   sweep), then `awk` computes the closure **D** of the roots (members of `G` plus the leader pid, minus
   the watchdog's own tree) through `ppid`. Every member of D outside `G` is an **escapee**: a
   `group <g>` line when its group leader `g > 1` is itself in D (so the group was created by our
   tree), otherwise a `pid <p>` line. Each new escapee is printed on the socket and **then**
   `SIGSTOP`ped; passes repeat until nothing new appears.
3. `SIGKILL` every recorded escapee, the leader pid (if still registered) and finally `G`; remove the
   temp directory and exit.

The kernel therefore delivers the kill to everything frozen by the same process that froze it.

**Graceful shutdown** (`LspServer::shutdown`): after the LSP `shutdown` response mcpls sends `mark`; the
watchdog freezes and records the escapees as in step 2, prints each `target` line before stopping it,
`SIGCONT`s `G` and prints `marked`. mcpls then sends `exit`. Once the leader has exited (or the grace
expired) mcpls half-closes the socket, the watchdog sweeps (so the recorded escapees die together with
anything new), and `ServerProcess::terminate` waits for the watchdog, up to
`lsp::LIFELINE_SWEEP_BUDGET`. If `marked` does not arrive within 3 s, or the socket ends first, mcpls
skips `exit` and terminates the tree immediately; a watchdog that is already dead is replaced by killing
from Rust (below).

**Leader reaping.** When Rust reaps the leader (`try_wait`/`wait` returning a status) it sends
`forget-leader`, so the watchdog never targets a pid the kernel may have handed to an unrelated process
(a dead `ServerProcess` can sit in `lsp_servers` for minutes during respawn backoff). A reaped leader
found by `try_wait` (a crash) also releases its lifeline at once: the sweep kills the helpers it left in
`G` (#542) without waiting for the respawn. The leader pid is a root unconditionally, so a leader that
called `setsid`/`setpgid` itself is still killed.

**Rust-side fallback.** If the watchdog failed to start, or is dead when the lifeline is released, Rust
kills the recorded escapees, `G` (with rustix, only while the anchor is unreaped, i.e. while `G` is still
reserved), the leader and the helpers, so `Drop` never leaves the group behind. A detached task awaits the
watchdog for at most `LIFELINE_SWEEP_BUDGET` and falls back the same way if it overruns.

**Windows: Job Object.** Each server is spawned via `process-wrap` (`JobObject` + `KillOnDrop`
wrappers). It creates the process suspended, assigns it to a job with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, then resumes it. mcpls owns the job handle. The kernel
closes it on any mcpls exit and terminates every process in the job. Dropping the server handle
(shutdown, respawn) also kills that server's whole tree. The job is created **without**
`JOB_OBJECT_LIMIT_BREAKAWAY_OK`. `processId` stays the real mcpls pid.

**No PGID-reuse hazard.** Every group the watchdog signals is reserved for the whole sweep. `G` is held by
its anchor, which lives until the final group kill or 60 s after mcpls's end of the anchor pipe closes,
longer than `LIFELINE_SWEEP_BUDGET`. An escaped group's leader belongs to the frozen closure and its
parent is frozen, so even as a zombie it keeps the id. Frozen pids are killed by pid inside the same
sweep that stopped them. Residuals: the anchor is killed externally and the pid space wraps during the
sweep; the leader is reaped by Rust and its pid reused before `forget-leader` is processed.

### Per-platform behaviour

| Exit path | Linux / macOS | Windows |
|-----------|---------------|---------|
| Clean exit of the mcpls *process* | `shutdown` marks escapees, then `terminate` sweeps the tree and waits | job close kills the tree |
| Panic, `exit(1)` on second signal, panic in `Drop` | watchdog (socket EOF) sweeps the tree | job close |
| `SIGKILL` / OOM kill / forced exit | watchdog (socket EOF) sweeps the tree | kernel closes the job handle |
| One server respawned or shut down while mcpls runs | that server's lifeline sweeps its whole tree, including `setsid` descendants | whole tree killed |
| A server crashes and respawn backs off | the crash is noticed by `try_wait`, its lifeline is released and swept immediately | job handle dropped with the server |
| `serve()` returns inside a longer-lived embedding host | `terminate` waits for each tree to be gone (up to `LIFELINE_SWEEP_BUDGET`) | trees already killed by handle drop |

## 2. User Stories

- **US-001**: As a user whose MCP client force-kills mcpls, I want no `rust-analyzer` (multi-GB)
  or its children left running afterwards, including the `cargo check` run by rust-analyzer's flycheck, which runs in its own session. On Unix this holds for every descendant whose ancestry is still attributable at sweep time (see Edge Cases); on Windows the whole tree is killed.
- **US-002**: As a library embedder calling `mcpls_core::serve`, I want the same guarantee without
  shipping an extra binary, and I want `serve()` to return only once the trees are gone.
- **US-003**: As a user whose language server crashes, I want the helpers it left behind killed when mcpls notices, not when mcpls exits.

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN the mcpls process exits by any means on Unix THE SYSTEM SHALL SIGKILL every LSP server process and every descendant, including descendants that left the server's process group through `setsid`/`setpgid` while their ancestry was still attributable | must |
| FR-002 | WHEN mcpls exits by any means on Windows THE SYSTEM SHALL terminate every process in each LSP server's job object | must |
| FR-003 | WHEN an individual LSP server is shut down, restarted (`restart_server`) or respawned THE SYSTEM SHALL terminate that server's whole process tree (Windows: its job; Unix: its lifeline sweep) | must |
| FR-004 | WHEN the anchor cannot be started THE SYSTEM SHALL log one warning and spawn the server unbound in a fresh separate process group (`process_group(0)`, so Ctrl-C does not reach it; killed on drop), never failing the spawn because of the binding. WHEN only the watchdog cannot be started THE SYSTEM SHALL keep the server in the anchor's group, log one warning, and kill that group (and the leader) from Rust when the server is dropped; `processId` then carries the real pid | must |
| FR-005 | THE SYSTEM SHALL start a fresh anchor and watchdog for every server spawn, so a dead helper of one server never affects another | must |
| FR-006 | THE SYSTEM SHALL create the lifeline fds (anchor pipe, stderr pipe, socketpair) and spawn the anchor, the server and the watchdog under one lock, so no LSP child can inherit another server's lifeline fds through the non-atomic `FD_CLOEXEC` sequence std uses on macOS | must |
| FR-007 | WHEN a spawn into the anchor's group fails with `PermissionDenied` AND the anchor has already been reaped (EPERM from `setpgid`: the group vanished because something other than mcpls reaped it) THE SYSTEM SHALL discard it, warn, and retry the spawn exactly once with a fresh anchor. WHEN the anchor is still alive THE SYSTEM SHALL return the error unchanged, because `PermissionDenied` can also be exec EACCES | must |
| FR-008 | WHEN a watchdog is bound THE SYSTEM SHALL send `processId: null` in `initialize` and hold the server's stdin, stdout and stderr ends outside the server's reach until the sweep, so the server cannot exit before its tree is frozen | must |
| FR-009 | WHEN the leader is reaped THE SYSTEM SHALL tell the watchdog to forget its pid (`forget-leader`); WHEN `try_wait` observes the leader gone THE SYSTEM SHALL release the lifeline immediately | must |
| FR-010 | WHEN `LspServer::shutdown` runs THE SYSTEM SHALL freeze and record the escapees before sending `exit`, skip `exit` if that fails within 3 s, and not return before the tree is gone or `LIFELINE_SWEEP_BUDGET` has elapsed | must |
| FR-011 | THE SYSTEM SHALL never signal pid or pgid <= 1, mcpls's own pid or process group, the anchor's group or the watchdog's own group as an escapee, SHALL ignore `ps` rows that are not exactly three numeric fields with pid > 1, SHALL seed the closure from the leader only while it is registered (never from an empty value), SHALL NOT propagate through ppid <= 1, and SHALL signal a group only when its leader belongs to the frozen closure. The final group signals are sent only while the anchor is alive | must |
| FR-012 | THE SYSTEM SHALL provide an idempotent, bounded tree termination for one server (`ServerProcess::terminate_tree(within)`) used by shutdown and manual restart (a crashed server is instead swept when `try_wait` notices the crash, and again by the final drop): it sweeps the lifeline including escapees, keeps the reaped leader reportable through `try_wait` (so `is_dead` is true afterwards), and for an unbound server kills the leader | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Safety | No `unsafe` in mcpls crates; workspace lint `unsafe_code = "forbid"` |
| NFR-002 | Footprint | Two `sh` processes per live server (about 1 MB each), plus one transient `ps`/`awk` pair per sweep pass. Windows: none |
| NFR-003 | Latency | A sweep completes well inside `lsp::LIFELINE_SWEEP_BUDGET` (8 s: 3 passes of at most 2 s each plus the kills); tests poll with a 5 s ceiling for the normal case |
| NFR-004 | Robustness | A hung `ps` (D-state, NFS stall) never blocks the sweep: the group is killed after the 2 s poll gives up, without attribution |

## 5. Data Model

| Entity | Description |
|--------|-------------|
| `lsp::process::ServerProcess` | Owns one spawned server: the leader `tokio::process::Child`, the stderr `Receiver`, and the `Lifeline` (Unix). API: `spawn(Command)`, `take_stdin`/`take_stdout`/`take_stderr`, `try_wait`, `wait` (leader only), `mark_escapees`, `terminate`, `is_lifeline_bound`, and a `Drop` that releases the lifeline. Test-only `from_unbound` for fixtures. Windows wraps `Box<dyn ChildWrapper>` and has no lifeline. |
| `Lifeline` (unix) | The `Anchor` (child, write end of its pipe, `G`), an optional `Watchdog` (child plus `Channel`), and whether the leader was forgotten. Dropping the server hands it to a detached `Sweep`. |
| `Channel` (unix) | mcpls's end of the watchdog socketpair: a tokio reader and a std writer; closing the writer half-closes the socket. Parses `target group N` / `target pid N` / `marked` lines into `Escapee`s. |
| `Escapee` (unix) | `Group(Pid)` or `Process(Pid)`, constructible only for pids > 1 that are neither the anchor's nor the watchdog's id |
| `Sweep` (unix) | The helpers and leader still to be waited for after the lifeline is closed; its fallback kill is the Rust-side replacement for a dead watchdog |
| `lsp::LIFELINE_SWEEP_BUDGET` | `pub const` (8 s): upper bound for a sweep; also the grace other tools (the benchmark harness) allow before reporting survivors |
| `lsp::process::ServerStderr` | The server's stderr read end: a tokio pipe `Receiver` on Unix (mcpls creates the pipe), `ChildStderr` on Windows |

## 6. Edge Cases and Error Handling

| Scenario | Behaviour |
|----------|-----------|
| **Intentional daemon (Windows)**: a descendant meant to outlive its spawner (shared build server, file watcher, telemetry agent) | Killed on server respawn, per-server shutdown and mcpls exit. Because the job lacks `BREAKAWAY_OK`, a descendant that passes `CREATE_BREAKAWAY_FROM_JOB` gets `ERROR_ACCESS_DENIED` from `CreateProcess` and its own spawn fails. **Intended**: children dying with the parent is the point of #526. Documented in README and CHANGELOG; no mitigation. |
| **Intentional daemon (Unix)**: a descendant that deliberately outlives its spawner (shared gradle, sccache or MSBuild nodes, a build server) and calls `setsid()` | Now killed too, when its ancestry is attributable at sweep time. Intended, documented in README and CHANGELOG. |
| A descendant's parent exited before any sweep (a crashed leader's `setsid` child that was reparented to init, or to a Linux subreaper) | **Not attributable**: only members of `G` and the leader are roots. Only a subreaper could fix it, which is out of scope. Helpers that stayed in `G` are killed at crash detection (FR-009). |
| A server that SIGTERMs its stopped escapees during `exit` and waits for them | Hangs until `CHILD_EXIT_GRACE`, then the sweep kills it. Escapees stay `SIGSTOP`ped on purpose: once the leader exits they are reparented to init and could no longer be attributed, and a continued escapee could spawn unattributed children. |
| The watchdog dies mid-session (killed externally, OOM) | `try_wait` health checks notice it, log one warning and `is_lifeline_bound` turns false for later checks; `processId` already sent stays null. If mcpls is then `SIGKILL`ed nothing sweeps that server's tree (the server still exits on stdin EOF, which dies with mcpls; group members and `setsid` escapees may linger). `Drop`/`terminate` still kill by hand. Accepted residual: rebinding a live server is not possible. |
| The `ps` snapshot **times out** during `mark` (transient stall) | The watchdog prints `scan-timeout`; mcpls withholds `exit` and terminates at once, so the retry in the sweep sees the still-live leader's tree. A snapshot that **fails fast** (no `ps`) keeps the graceful `exit` and only logs the warning, since attribution cannot succeed anyway. |
| Worst-case shutdown time per server | `SHUTDOWN_TIMEOUT` (10 s, including `mark`) plus the sweep allowance, which is the time left before that deadline clamped to 3 s..`LIFELINE_SWEEP_BUDGET` (8 s); after the allowance the tree is killed from Rust. So at most ~13 s plus reaping, with servers shut down in parallel. |
| A server that exits only on stdin EOF and ignores `exit` | The watchdog holds the stdin write end, so on graceful shutdown the server waits out `CHILD_EXIT_GRACE` and is SIGKILLed by the sweep instead of exiting on its own. |
| `ps` is missing or unusable (slim container images without procps, busybox without `-A`/`-o`) | Attribution is lost: the watchdog prints `scan-failed`, mcpls logs one warning, and the sweep still kills `G` and the leader. Escaped descendants survive. |
| A stopped `setpgid` escapee group in the server's session when the leader exits after `mark` | POSIX orphaned-process-group rule: the kernel sends it SIGHUP and SIGCONT, so it may run again until the sweep, which still kills every recorded target. `setsid` escapees (rust-analyzer's flycheck) are unaffected. Not fixable without a kernel-level hold. |
| An escapee created by an in-group survivor between `mark` and the sweep | The sweep always scans again after `STOP`ping `G`, so it is attributed. |
| `pkill -9 -f mcpls` (or any pattern matching the helper command lines) | Helper argv0 is `lsp-lifeline-watchdog`, so a plain `mcpls` pattern spares the watchdog; a pattern matching it kills the watchdog and no sweep runs for that server. |
| The anchor is killed or reaped externally while mcpls runs | `G` is no longer reserved, so neither the watchdog nor Rust signals it (pgid-reuse safety): the server's group members linger until they exit, and only the leader pid and recorded escapees are killed. |
| The server stops reading its stdin or mcpls stops draining its stdout/stderr while both are alive (the stderr drain gives up after 5 read errors; an embedder's runtime shuts down) | The server blocks on its next write instead of dying from `EPIPE`, until the sweep kills it, because the helpers hold the other ends. |
| Watchdog start fails (Unix) | FR-004: warn once; the server stays in the anchor's group and is killed from Rust on drop; escaped descendants are not swept. Anchor start fails: spawned unbound in its own group. Windows: the server spawn fails with the error. |
| Watchdog is `SIGKILL`ed while mcpls lives | `try_wait` finds it dead at drop/terminate: Rust kills the recorded escapees, `G`, the leader and the anchor. Binding is lost for that server only. |
| Anchor leaks (partial spawn, watchdog death) | Cleaned explicitly: a failed spawn kills it; `Drop`/fallback kill it; otherwise it exits 60 s after its pipe closes. |
| Reaped by another party (embedder with `SIGCHLD = SIG_IGN` or a `waitpid(-1)` reaper) | The spawn gets EPERM and the anchor is dead; FR-007 applies |
| Pid reuse between the leader's reap and `forget-leader` | Residual: the pid space must wrap in that window. |
| A descendant opens `/dev/tty` (ssh/git credential prompt during `cargo fetch`, pinentry) while mcpls runs in an interactive terminal | `G` is a background group of that terminal, so the descendant gets `SIGTTIN`/`SIGTTOU` and stops; that flycheck run hangs. Not a concern when an MCP client spawns mcpls without a controlling terminal. Documented. |
| Ctrl-C (SIGINT) in the controlling terminal | Servers are in their own groups, not the terminal's foreground group, so they no longer receive it directly; mcpls handles the signal and shuts them down gracefully |
| mcpls dies between `spawn()` and the child's `setpgid` | Not possible: `setpgid` runs in the child before `exec` |
| mcpls dies between `mark` and `exit` | The watchdog owns the recorded targets: EOF triggers the sweep, which kills them. Nothing stays `SIGSTOP`ped. |
| `mktemp` or the temp directory is unavailable, or `ps` is missing | The sweep falls back to killing `G` and the leader, without attribution |
| `mktemp -d` location | Uses the platform default (`/tmp` on Linux, the per-user `/var/folders/...` directory on macOS) because the watchdog's environment is cleared |
| Host process (library embedder) forks concurrently on macOS | Its children may inherit a lifeline fd and delay EOF. Documented; outside mcpls's control. |
| `/bin/sh` missing | FR-004 |
| Which shells run the helpers | POSIX `sh` features only (`dash`, bash, macOS `/bin/sh`; the unit tests run the sweep under `dash` and `bash` when present). The scan poll uses fractional `sleep 0.02`, probed once at startup; where unsupported it falls back to 1 s polls (the 2 s snapshot timeout then rounds up). `busybox sh` is expected to work but is not exercised in CI. |

## 7. Success Criteria

| ID | Criterion |
|----|-----------|
| SC-001 | Unix e2e (`#[cfg(unix)]`, `#[ignore = "Requires mcpls binary built"]` so the CI e2e job runs it): a perl "server" that exits on stdin EOF, streams `window/logMessage` frames to stdout and text to stderr every millisecond, never answers `initialize` and leaves a `setsid` grandchild. After `SIGKILL` of the mcpls binary, both pids are gone within the 5 s polling ceiling. |
| SC-002 | Unix unit tests: dropping a `ServerProcess` and `terminate` kill an EOF-exiting server's `setsid` child; a leader that itself called `setsid` is killed on drop; `terminate` returns inside `LIFELINE_SWEEP_BUDGET` |
| SC-003 | Unix unit tests: a spawn into a vanished group retries per FR-007; a watchdog or anchor start failure follows FR-004 and `Drop` still kills the group |
| SC-004 | Windows unit test: a server started via PowerShell `Start-Process -PassThru` writes its grandchild's pid to a file; after `ServerProcess` is dropped, `tasklist /FI "PID eq <pid>"` no longer lists it |
| SC-005 | `rg 'unsafe' crates/ --type rust` finds no `unsafe` block or `allow(unsafe_code)`; the workspace builds with `unsafe_code = "forbid"` |
| SC-006 | Unix e2e: a graceful `SIGTERM` shutdown with a server that answers `shutdown`/`exit` and leaves a `setsid` grandchild leaves no process behind |
| SC-007 | Unix e2e (#542): a server that crashes and is respawned has the helper it left in its group killed |
| SC-008 | Unix unit tests: `mark` freezes the escapee and lets the server run; mcpls dying after `mark` leaves nothing stopped; a `mark` timeout reports failure; a hung `ps` still kills the group within the budget; a killed watchdog falls back to killing by hand; reaping the leader sends `forget-leader` and a forgotten pid survives the sweep; the awk program's fixtures (escaped group, foreign group, own tree, pids <= 1, reparented orphans, leader root) hold |
| SC-009 | Unit test: `initialize` carries `processId: null` while a watchdog is bound |
| SC-010 | Unix unit tests: `terminate_tree` kills the whole tree, is idempotent and keeps `try_wait` reporting; terminating one server leaves another server's tree alive; an unbound server's leader is killed; a respawn kills the crashed server's helper (e2e SC-007) |

## 8. Agent Boundaries

- **Always**: keep `kill_on_drop(true)` on an unbound leader; kill the leader of a bound server on every teardown path (the watchdog does it, Rust is the fallback).
- **Ask first** (approved by the user for #541/#542, 2026-10-04): the per-server watchdog and anchor, and the `rustix` dependency. Any further registration protocol with the watchdog, any change to which processes the sweep may signal, or any breakaway allowance on the Windows job still needs approval.
- **Never**: `pre_exec`, any `unsafe` or `allow(unsafe_code)`, a `mcpls` re-exec supervisor, signalling a group whose leader is outside the frozen closure, or signalling pid or pgid <= 1.

## 9. See Also

- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|spec lsp/001]] — graceful, deadline-bounded
  shutdown (FR-011) and respawn, which this spec complements rather than replaces. Without this
  spec, its US-004 "no orphans" story holds only on the in-process paths.
