---
aliases:
  - SIGTERM stdio shutdown hang
  - tokio blocking-pool stdin hang
tags:
  - sdd
  - spec
  - transport
  - lsp
  - bug
created: 2026-08-05
status: implemented
related:
  - "[[constitution]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec]]"
---

# Feature: mcpls process actually exits on SIGTERM/SIGINT while a stdio client is still connected

> [!info] Metadata
> **Author**: rust-live-tester (filed from live-testing evidence, cycle 013)
> **Related PR**: #270 (commit ec67fa4) — added `wait_for_shutdown_signal` / `run_stdio` signal handling, closed #241
> **Related issue**: #241 (original "no SIGINT/SIGTERM handling, orphans LSP processes")

> [!success] Resolution
> Implemented by commit `8c3e172` (PR #321), closing #308: `crates/mcpls-cli/src/main.rs` now
> calls `std::process::exit` as its final step instead of returning normally, resolving Open
> Question 1 (Section 9) in favor of the `std::process::exit(0)` approach — scoped to the CLI
> binary only, so `mcpls-core`'s `serve`/`serve_with`/`shutdown`/`run_stdio` keep their normal
> `Result`-returning semantics for library embedders (satisfying FR-002/NFR-002). The same PR
> also fixed `await_lsp_init_handle`'s timeout branch, which called `JoinHandle::abort()` without
> re-awaiting it — a SIGTERM arriving mid-spawn could otherwise let `process::exit` run before the
> aborted task's not-yet-registered LSP `Child` handles dropped, orphaning a process before
> `kill_on_drop` could fire.
>
> Commit `9642857` (PR #328) followed up with a related hardening fix: `serve_with` previously
> ran config validation, workspace-root heuristics, and background LSP spawning *before*
> `run_stdio` ever registered a signal handler, and `run_stdio` itself only registered one after
> the MCP `initialize` handshake resolved — a signal in that window fell through to the OS
> default disposition (immediate termination, skipping `Translator::shutdown_servers`). A
> `ShutdownSignal` is now constructed once, before any startup work, moved into whichever
> transport runs, and reused for the whole process lifetime (previously `SIGINT` was
> re-registered via a fresh `tokio::signal::ctrl_c()` listener on every wait, silently losing a
> signal delivered while a different `select!` branch was being polled).
>
> Open Question 2 (cross-platform verification beyond macOS `sample`(1)) was not explicitly
> addressed by either PR; the fix itself (`std::process::exit`) is platform-independent, but no
> Linux-specific verification evidence was recorded in either commit.

## 1. Overview

### Problem Statement

`run_stdio` (`crates/mcpls-core/src/transport.rs:189-211`) and `shutdown` (`crates/mcpls-core/src/lib.rs:646-659`) log a complete, correct shutdown sequence on `SIGTERM`/`SIGINT` — "shutdown signal received", "Shutting down LSP servers...", "LSP server shut down successfully", "MCPLS server shutting down", "mcpls shutdown complete" — and the spawned LSP child process (e.g. `rust-analyzer`) genuinely is killed. `run()` (`crates/mcpls-cli/src/main.rs:36-81`) then returns `Ok(())`, and `main()` returns `ExitCode::SUCCESS`.

But if an MCP client is still connected over stdio (has not closed its write end of mcpls's stdin — the normal state for any live session, e.g. Claude Desktop/Code, a long-running agent), **the OS process itself does not exit**. It was confirmed still running and consuming a PID more than 30 minutes after its own log said shutdown was complete, and only a `SIGKILL` (or the client itself eventually closing stdin) ends it.

Root cause, confirmed via `sample`(1) stack trace on macOS: the main thread is parked in `tokio::runtime::blocking::pool::BlockingPool::shutdown` → `Receiver::wait`, which is the `#[tokio::main]`-generated wrapper blocking on `Runtime::drop` after `main()`'s body returned. That drop waits for every outstanding `spawn_blocking` task to finish. One `tokio-rt-worker` thread is still executing `tokio::io::blocking::Blocking<std::io::Stdin>::poll_read` → `std::io::Stdin::read` → the raw `read()` syscall on the process's real stdin fd — a genuine blocking OS thread that `tokio::io::stdin()` uses internally (a well-documented tokio caveat: this thread cannot be cancelled and only returns when the fd sees more data or EOF). Since the client's write end is still open, that `read()` never returns, `BlockingPool::shutdown` never completes, and the process hangs indefinitely past the point where its own logs claim it already shut down.

This directly contradicts the doc comment on `run_stdio` (`transport.rs:183-188`): "Returns as soon as either the stdio transport closes ... or a `SIGTERM`/`SIGINT` is received, so callers can run orderly cleanup ... before the process exits ... which is acceptable here since the process exits shortly after." The process does not exit shortly after — it exits only when the client disconnects, an event decoupled from the signal that was supposed to trigger shutdown.

It also undermines the stated purpose of #270/#241: `wait_for_shutdown_signal`'s own doc comment says `SIGTERM` is "sent by containers and systemd." In that exact scenario — an orchestrator restarting/stopping the mcpls container while its MCP client is still attached — mcpls will not actually terminate within the orchestrator's grace period (commonly 10s for Docker/Kubernetes) and gets forcibly `SIGKILL`ed, which is precisely the "orphans processes / unclean shutdown" outcome #270 was written to prevent. In this specific hang, LSP child cleanup already completed correctly before the hang (no orphaned `rust-analyzer`), so the practical blast radius is a hung/unkillable-by-SIGTERM parent process and delayed container/pod termination, not orphaned LSP subprocesses — a narrower but still real regression of #270's goal.

The HTTP transport (`run_http`) is not affected by this specific mechanism — it does not use `tokio::io::stdin()`.

### Goal

`mcpls` running the stdio transport actually terminates the OS process promptly after `SIGTERM`/`SIGINT`, regardless of whether the connected client has closed its end of stdin.

### Out of Scope

- HTTP transport shutdown (already bounded/verified correct — not affected by this mechanism)
- The `panic = "abort"` cleanup-bypass limitation already documented as a known limitation on `Translator::shutdown_servers` (separate, already-acknowledged gap)
- Redesigning the LSP-server graceful-shutdown handshake itself (`Translator::shutdown_servers` is confirmed working correctly)

## 2. User Stories

### US-001: Container orchestrator stops mcpls while a client is attached
AS A platform operator running mcpls in Docker/Kubernetes with a long-lived MCP client connected
I WANT `SIGTERM` to actually terminate the mcpls process within its normal shutdown logging window
SO THAT rolling restarts/deploys don't rely on the orchestrator's `SIGKILL` fallback and don't delay pod/container termination

**Acceptance criteria:**
```
GIVEN mcpls is running the stdio transport with an MCP client connected (client has not closed its stdin write end)
WHEN mcpls receives SIGTERM
THEN the OS process exits within a bounded, short time (comparable to today's logged shutdown duration, well under typical orchestrator grace periods)
```

### US-002: Existing SIGTERM log semantics stay truthful
AS A developer relying on the shutdown log lines to know the process has stopped
I WANT "mcpls shutdown complete" to mean the process is actually about to exit
SO THAT monitoring/log-based automation isn't misled into thinking shutdown finished when the process is still alive

**Acceptance criteria:**
```
GIVEN the "mcpls shutdown complete" log line has been emitted
WHEN a caller checks process liveness immediately after
THEN the process is gone or exits within a small fixed bound (not indefinitely blocked)
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `SIGTERM`/`SIGINT` is received on the stdio transport THE SYSTEM SHALL terminate the OS process without waiting for the connected client to close its end of stdin | must |
| FR-002 | WHEN the process terminates via FR-001 THE SYSTEM SHALL still have completed `Translator::shutdown_servers()` first (no orphaned LSP subprocesses — do not regress #241) | must |
| FR-003 | WHERE the fix bypasses normal `Runtime::drop`/destructor semantics (e.g. via `std::process::exit`) THE SYSTEM SHALL flush all buffered log output before terminating | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Compatibility | HTTP transport shutdown behavior is unchanged |
| NFR-002 | Compatibility | Normal stdio-EOF shutdown (client disconnects first) continues to work exactly as today |
| NFR-003 | Testability | A regression test can simulate "client stdin stays open across signal delivery" without relying on real process-level signals/timing (e.g. by exercising the `shutdown()` function directly and asserting it doesn't depend on stdin state) |

## 5. Data Model

No new domain entities — this is a process-lifecycle/shutdown-sequencing fix.

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Client closes stdin before SIGTERM arrives (today's already-working path) | Unaffected — process exits via existing stdio-EOF path |
| Client stays connected, SIGTERM arrives | Process exits promptly anyway (this fix) |
| `SIGKILL` sent instead of `SIGTERM` | Unaffected (already an immediate hard kill, out of scope) |
| HTTP transport, any signal | Unaffected — does not use `tokio::io::stdin()` |
| Panic under `panic = "abort"` | Unaffected — already a documented, separate known limitation |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Live repro: spawn mcpls, complete MCP `initialize`/`initialized` handshake, keep the client's stdin pipe open, send `SIGTERM` | Process (not just the logical shutdown sequence) exits within a few seconds |
| SC-002 | No LSP subprocess left orphaned by the fix | `pgrep -P <mcpls_pid>` empty after shutdown, matching current (already-correct) behavior |
| SC-003 | Existing shutdown/signal-handling unit and integration tests continue to pass | `cargo nextest run --workspace --all-features` green |

## 8. Agent Boundaries

### Always (without asking)
- Verify the fix live: real `SIGTERM` sent to a running binary with an open client stdin pipe held by the test harness, confirming OS-level process exit (not just log output)
- Preserve the already-correct LSP-server graceful shutdown (`Translator::shutdown_servers`) ordering — it must still run to completion before the process terminates
- Run `cargo +nightly fmt --all -- --check`, `cargo clippy --all-targets --all-features --workspace -- -D warnings`, `cargo nextest run --workspace --all-features --lib --bins`
- Update `CHANGELOG.md` under `[Unreleased]`

### Ask First
- Whether to call `std::process::exit()` directly in `main.rs` after the existing shutdown sequence (simplest, most common fix for this exact tokio caveat, but skips normal Rust destructors for anything constructed in `main`/`run` — confirm nothing relies on those running) vs. a more invasive restructuring (e.g. avoiding `tokio::io::stdin()` entirely in favor of a cancellable reader)

### Never
- Silently swallow the LSP-shutdown step to "speed up" exit — FR-002 requires it still runs
- Change HTTP transport shutdown semantics as a side effect

## 9. Open Questions

- [NEEDS CLARIFICATION: Is `std::process::exit(0)` immediately after the existing "mcpls shutdown complete" log (with an explicit flush of the tracing writer first) an acceptable fix, or does the project want a more general solution that keeps `main`'s normal `ExitCode` return path (e.g. spawning `run_stdio`'s read loop in a way that's actually cancellable, or accepting the tokio `stdin()` limitation and documenting it instead of masking it)?]
- [NEEDS CLARIFICATION: Should this be verified across platforms (Linux is the primary container/systemd deployment target; this was reproduced and stack-traced on macOS) before considering it fixed, given the root cause — tokio's internal stdin blocking-thread pool — is platform-independent but the `sample`(1) confirmation tooling used here is macOS-only? `lldb`/`gdb` + `/proc/<pid>/task/*/stack` would be the Linux equivalent.]

## 10. See Also

- [[constitution]] — project principles (not yet created for this project)
- [[MOC-specs]] — all specifications
- PR #270 (commit ec67fa4) — added the signal handling this bug affects, closed #241
- Issue #241 — original "no SIGINT/SIGTERM handling" issue
- `crates/mcpls-core/src/transport.rs:148-211` — `wait_for_shutdown_signal`, `run_stdio`
- `crates/mcpls-core/src/lib.rs:633-659` — `shutdown()`
- `crates/mcpls-cli/src/main.rs:15-34` — `#[tokio::main]` entry point whose generated `Runtime::drop` is where the hang occurs
- tokio `io::stdin()` docs — documents the internal blocking-thread implementation this bug is rooted in
