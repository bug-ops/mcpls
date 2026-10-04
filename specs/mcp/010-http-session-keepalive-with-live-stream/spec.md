---
aliases:
  - HTTP session keep-alive
  - rmcp keep_alive vs live GET stream
tags:
  - sdd
  - spec
  - mcp
  - http
  - transport
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[mcp/006-http-stream-liveness/spec|http-stream-liveness]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp-resources-diagnostics]]"
---

# Feature: A Live GET Stream Keeps Its HTTP Session Alive

> [!info] Metadata
> **Type**: bug / hardening
> **Priority**: P3
> **Related issues**: #573; follows #543/#553 ([[mcp/006-http-stream-liveness/spec|mcp/006]]) and #521 ([[mcp/002-mcp-resources-diagnostics/spec|mcp/002]] FR-008)
> **Verified at**: HEAD `ad90190`, release build with `--features transport-http`, rmcp 3.5.0

## 1. Overview

### Problem Statement

mcpls promises that an HTTP session with an open response stream is never expired for inactivity: its own idle reaper (`IdleTimeout`, `SessionActivity`, `run_idle_reaper` in `crates/mcpls-core/src/transport.rs`) skips any session with an open stream, and the GET-stream liveness probe (mcp/006) closes the stream of a client that stops answering. A client that keeps a standalone GET stream open, answers every `ping` probe and holds `resources/subscribe` subscriptions is therefore demonstrably alive.

rmcp's own session timer contradicts that promise. `SessionConfig.keep_alive` (default `SessionConfig::DEFAULT_KEEP_ALIVE`, 300 s) is a sleep re-armed on each event in the session worker loop (`LocalSessionWorker`, rmcp 3.5.0 `session/local.rs`): an inbound client message, a stream (re)establishment or resume, a close, or an outbound message from the service handler. It ends the session with `IdleTimeout` when nothing happens for 5 minutes. Neither input that keeps such a client alive reaches that loop:

- probe replies are consumed in `CappedSessionManager::accept_message` and never forwarded (mcp/006 FR-003);
- the probes themselves are produced by mcpls inside the forwarding task, not by the rmcp service, and SSE keep-alive pings (`sse_keep_alive`, 15 s) are written by the HTTP layer, not the worker.

Observed (reproduction in section 8): the client's GET stream closes at 300.0 s, the next POST returns `404 Not Found: Session not found`, all subscriptions are lost, and the client must re-initialize. The server log shows `rmcp::service: input stream terminated` 300.005 s after session creation and no mcpls "closing idle HTTP session" line. The doc comment of `serve_http` admits the gap ("rmcp's own 5-minute `keep_alive` still ends a session that sees no event at all in that time"), and mcp/002 FR-008 documents it as accepted. The unit test `test_healthy_get_only_listener_is_not_reaped` passes only because it keeps the rmcp timer fed with `resources/updated` notifications and a POST.

Control: a session that never answers probes loses its stream at 90 s and expires at 90 + 300 s through the mcpls reaper. That path works as designed and must not change.

### Goal

A session whose client keeps a standalone GET stream open and keeps answering liveness probes stays alive until the client closes the stream, sends `DELETE`, or stops answering; the mcpls idle reaper is the only component that expires an HTTP session for inactivity.

### Out of Scope

- Stateless request paths and `subscriptions/listen` streams: they own no session and no rmcp session worker (see section 5).
- Changing probe interval or deadline defaults, or the probe mechanism itself (mcp/006).
- Making `IdleTimeout` or rmcp `keep_alive` user-configurable from the CLI or config file. [NEEDS CLARIFICATION: is a CLI/config knob wanted, or does `HttpConfig::session_idle_timeout` stay the only (crate-internal) setting?]
- Authentication, session-id secrecy, and the session cap (`max_concurrent_sessions`).
- Persisting subscriptions across a session loss (re-initialization stays the client's job after any genuine expiry).

## 2. User Stories

### US-001: A subscribed, quiet client stays connected

AS A developer running an MCP client that keeps a GET stream open and subscribes to diagnostics resources
I WANT my session to survive any stretch without requests or notifications while my client answers liveness probes
SO THAT I do not lose subscriptions and have to re-initialize every 5 minutes

**Acceptance criteria:**
```
GIVEN an initialized session with an open GET stream, one resources/subscribe subscription,
      and a client that answers every liveness probe with 202
WHEN  the client sends no other request and no notification is produced for 6 minutes (> 300 s)
THEN  the GET stream is still open, a POST with the session id returns 200/202 (not 404),
      and the subscription still delivers the next resources/updated
```

### US-002: Operator keeps one expiry owner

AS AN operator running mcpls behind a reverse proxy
I WANT session expiry to follow a single, documented rule
SO THAT I can reason about how long a vanished client holds a session slot

**Acceptance criteria:**
```
GIVEN a session whose client vanished (stream dead, no more answers)
WHEN  time passes
THEN  the mcpls reaper closes the session within the bound documented for it,
      and no session slot outlives that bound because of this feature
```

### US-003: A dead client is still dropped

AS AN operator
I WANT a client that stops answering probes to lose its session as before
SO THAT dead peers do not hold permits

**Acceptance criteria:**
```
GIVEN a session with an open GET stream whose client never answers probes
WHEN  the probe deadline passes and the stream is closed
THEN  the session expires through the mcpls reaper IdleTimeout after the stream closed
      (90 s + 300 s with defaults), unchanged from today
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHILE a session has an open standalone GET stream that is not past its probe deadline, THE SYSTEM SHALL NOT end the session because of rmcp's `keep_alive` timer, regardless of inbound requests or outbound notifications | must |
| FR-002 | WHEN a client answers a liveness probe, THE SYSTEM SHALL count the answer as client activity for expiry purposes (it already re-arms the mcpls idle clock through `touch`), such that no session expiry timer can fire within one `IdleTimeout` of that answer while the stream is open | must |
| FR-003 | THE mcpls idle reaper SHALL be the only component that expires an HTTP session for inactivity: rmcp's `keep_alive` SHALL NOT end a session earlier than the reaper would under the same conditions, and SHALL NOT end a session the reaper would keep | must |
| FR-004 | WHEN a session has no open response stream and no inbound client activity for `IdleTimeout`, THE SYSTEM SHALL close it through the reaper, freeing its `max_concurrent_sessions` permit, with no more delay than the existing 1.2x sweep bound ([[mcp/002-mcp-resources-diagnostics/spec\|mcp/002]] FR-008) | must |
| FR-005 | WHEN a client with an open GET stream stops answering probes, THE SYSTEM SHALL close the stream per mcp/006 FR-002 and SHALL then expire the session after `IdleTimeout` measured from the stream close, unchanged | must |
| FR-006 | WHEN the client sends `DELETE` or closes its stream and sends nothing further, THE SYSTEM SHALL release the session as today (explicit `close_session`; idle expiry after stream close) | must |
| FR-007 | THE SYSTEM SHALL keep probe replies out of the MCP service (mcp/006 FR-003) unless the chosen design requires forwarding them; any forwarding SHALL NOT produce an outbound message on any stream or an `unknown request id` warning | must |
| FR-008 | WHILE probing is `StreamLiveness::Disabled`, THE SYSTEM SHALL NOT count an open GET stream as proof of life: the stream does not hold its session, which expires within `IdleTimeout` of the last inbound request (the open counts as activity at the moment it is established), so a vanished peer is bounded exactly as a quiet session is. Decision recorded for option (b) of the original question, accepted as **Breaking**: a healthy `--http-stream-liveness off` listener that only receives notifications is now cut 5 minutes after its last request, which rmcp's notification-fed timer used to prevent. A request-wise POST stream keeps its session while open; if its peer vanished mid-write it is bounded by TCP (`TCP_USER_TIMEOUT` of 60 s on Linux and Android, the OS default elsewhere, or the reverse proxy timeout), no longer by rmcp's 5-minute timer. Residual risk, deferred: POST and non-common resume streams are not probed and hold a `StreamGuard`, so a client that stops reading a large response, or a half-open peer off Linux, pins its session slot until TCP gives up; exposure is capped by `SessionLimit`/`ConnectionLimit` and a stream-age or write-progress deadline is tracked as a follow-up | must |
| FR-009 | THE SYSTEM SHALL update the `serve_http` doc comment, mcp/002 FR-008 and mcp/006 so that none of them still states that rmcp's `keep_alive` ends a session whose client holds an answering GET stream | must |
| FR-010 | THE test suite SHALL contain a case that fails on `ad90190`: a session kept alive only by an open GET stream and answered probes, with no notifications and no other requests, survives longer than rmcp's keep_alive using the same effective values (scaled down), through the real rmcp worker, not only through `SessionActivity` | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Reliability / resource safety | No leak of vanished-peer sessions. The mcpls reaper SHALL remain the single owner of expiry, so every session slot that rmcp's `keep_alive` bounded before this change is bounded afterwards by the reaper, probe, or an equally tight mechanism, in every configuration (`Probe`, `Disabled`) and for every stream kind (GET, request-wise POST) |
| NFR-002 | Reliability | A session with an open stream whose peer vanished SHALL be released within probe interval + probe deadline + `IdleTimeout` (+ one sweep interval) under `StreamLiveness::Probe`; the bound is not lengthened by this change |
| NFR-003 | Reliability | Expiry decisions SHALL NOT depend on outbound notification volume (preserves #521: a stream of `resources/updated` never keeps an abandoned session alive) |
| NFR-004 | Performance | No additional per-message locking on the hot path beyond one `touch` per probe reply; no new timer or task per session beyond what mcp/006 already spawns [NEEDS CLARIFICATION: confirm a design that changes how rmcp's timer is fed does not add per-session tasks] |
| NFR-005 | Type safety | Any new durations or modes are non-zero or enum-typed values (as `IdleTimeout`, `ProbeInterval`, `StreamLiveness`); no raw `Option<Duration>` leaks into `HttpConfig`. `deny(unsafe_code)` holds |
| NFR-006 | Compatibility | Wire behaviour of the Streamable HTTP endpoint is unchanged apart from the session no longer being dropped at 300 s; no new headers, status codes or event types. Before v1.0.0 no backwards-compatibility shim is added; the behaviour change goes in `CHANGELOG.md` |
| NFR-007 | Observability | The existing `closing idle HTTP session` debug line SHALL remain the log signal for reaper expiry; an expiry that still comes from rmcp (if any path remains) SHALL be distinguishable in the log [NEEDS CLARIFICATION: is a distinct log line for an rmcp-originated `IdleTimeout` wanted?] |
| NFR-008 | Testability | Time-dependent tests use `tokio::time` pause/advance or the existing `TEST_IDLE` scaling; no test sleeps for real minutes |

## 5. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Stateless request path (no `Mcp-Session-Id`, `server/discover`, one service instance per request) | No session, no rmcp session worker and no `keep_alive` apply; behaviour unchanged. Stateless `subscriptions/listen` streams stay governed by `MAX_LISTEN_STREAMS` (#551) |
| `--http-stream-liveness off` / `StreamLiveness::Disabled` | No probe means no answer signal. FR-008 governs; at minimum the change SHALL NOT make a vanished peer holding an open stream more durable than it is today |
| Client holds the GET stream open but never answers probes | Stream closed at interval + deadline (90 s); session expires 300 s later via reaper; unchanged (control case) |
| Client answers probes on a shadow GET stream while the primary is dead | Probe state is per stream and per session (mcp/006 FR-004, FR-007); the primary's timeout closes the standalone stream in rmcp and the session then follows FR-005. A live shadow SHALL NOT keep the session alive on its own [NEEDS CLARIFICATION: shadow streams receive no notifications; confirm `SessionActivity` open-stream counting already treats them as open and whether that is acceptable] |
| Open request-wise POST stream whose peer vanished (long tool call, no probe) | Today bounded by rmcp's `keep_alive` when no events flow; after the change it SHALL remain bounded (NFR-001). [NEEDS CLARIFICATION: the reaper currently never reaps a session with any open stream, POST included; a design that disables rmcp's timer must bound this case, e.g. by `TCP_USER_TIMEOUT` plus SSE pings only, or by a stream-age cap] |
| Long-running request (> 5 min) from a live client | Not expired by rmcp's timer during the wait (it counts as an open stream for the reaper); this is an intended side effect and is covered by the NFR-001 bound for vanished peers |
| Session never initialized | rmcp `init_timeout` (60 s) is a separate timer and stays as is |
| Client reconnects its GET with `Last-Event-ID` | Stream open and resume count as activity as today; no regression to the primary/shadow handling of mcp/006 |
| Probe reply arrives for a session already closed or reaped | Ignored without error (`session_liveness` returns `None`); the client's next POST gets 404 and re-initializes |
| Clock jump or paused runtime | Expiry uses `tokio::time::Instant`; behaviour as the existing reaper |
| `IdleTimeout` set lower than the probe interval (internal `HttpConfig::session_idle_timeout`) | Reaper still skips sessions with an open stream, so a short idle timeout does not reap an answering listener. [NEEDS CLARIFICATION: is a minimum ratio to the probe interval worth enforcing at the type level?] |

## 6. Design Notes (non-binding)

The requirements are outcome-based. Candidate shapes, to be decided in `/sdd plan`:

- **Neutralise rmcp's timer, reaper owns expiry (implemented).** `CappedSessionManager::new` builds `LocalSessionManager::default()`, whose `session_config` is a public field; setting `keep_alive` to `None` removes the second owner. That alone loosens bounds in `Disabled` mode and for POST-only streams (FR-008, NFR-001), so the reaper's open-stream rule must be tightened to compensate. Needs a decision on what counts as proof of life when probes are off.
- **Feed rmcp's timer.** Forwarding the probe reply (or a synthetic client `ping`) into the worker re-arms the timer, but reverses mcp/006 FR-003 and risks a stray response on the common channel; rejected unless the plan shows it can be done cleanly.
- **Make the reaper stream-aware.** Count only a probe-acknowledged GET stream as keeping the session alive. This gives `Disabled` mode a defined bound and is the likely companion of the first option.
- The `serve_http` doc comment and mcp/002 FR-008 name the gap explicitly; both change with this feature (FR-009).
- Regression anchor: `test_healthy_get_only_listener_is_not_reaped` passes today because its notification stream and POST feed rmcp's timer. A new test must not feed it (FR-010).

## 7. Success Criteria

| ID | Criterion |
|----|-----------|
| SC-001 | The reproduction in section 8 no longer ends the session at 300 s; the session survives at least 2x `SessionConfig::DEFAULT_KEEP_ALIVE` with the client answering probes |
| SC-002 | The control case (never answers) still loses its stream at 90 s and the session at 90 + 300 s (within one sweep interval) |
| SC-003 | A session whose peer vanished after a probe answer is released within NFR-002's bound; verified with a scaled-time test, no leaked permit (`max_concurrent_sessions` slot reusable) |
| SC-004 | With `--http-stream-liveness off`, behaviour matches the decision recorded for FR-008, covered by a test |
| SC-005 | Docs (`serve_http` comment, mcp/002 FR-008, mcp/006) no longer state the 5-minute caveat; `CHANGELOG.md` `[Unreleased]` has a one-line entry |

## 8. Reproduction

Release build with `--features transport-http`, HEAD `ad90190`.

1. `mcpls --config cfg.toml --listen 127.0.0.1:38803`.
2. POST `initialize` and `notifications/initialized`; keep the `Mcp-Session-Id`. Open `GET /mcp` with `Accept: text/event-stream` and that session id. Do nothing else.
3. Answer every `{"method":"ping","id":"mcpls-liveness-N"}` event with `POST {"jsonrpc":"2.0","id":"mcpls-liveness-N","result":{}}` (202 each time at 60, 120, 180, 240 s).
4. Observed: GET stream closed at 300.0 s; the next POST with the session id returns `404 Not Found: Session not found`; the log shows `rmcp::service: input stream terminated` 300.005 s after session creation and no mcpls "closing idle HTTP session" line.

## 9. Agent Boundaries

### Always (without asking)
- Run `cargo +nightly fmt --all -- --check`, clippy with `-D warnings`, `cargo nextest run --workspace --all-features --lib --bins` and the rustdoc gate before every commit
- Keep position, subscription and probe code paths of mcp/006 intact; reuse `SessionActivity`, `IdleTimeout`, `StreamGuard`
- Verify rmcp behaviour against the 3.5.0 source in `~/.cargo/registry`, not the issue text
- Update `.local/testing/` playbooks and coverage status for the HTTP transport so the next live cycle re-tests the long-quiet-session scenario

### Ask First
- Forwarding probe replies into rmcp, or any change to rmcp session or stream semantics beyond `SessionConfig`
- Adding a dependency, or exposing a new CLI flag or config key
- Changing probe interval/deadline defaults or the `IdleTimeout` default

### Never
- Weaken or remove the idle reaper's protection against vanished peers, or let outbound notification volume keep a session alive (#521)
- Leave a code path where both rmcp's timer and the reaper expire sessions on different rules
- Edit anything under `crates/` as part of this spec task; write specs under `.local/specs/`

## 10. Open Questions

> [!question] Open items
> - [ ] Issue number: #573
> - [x] FR-008: the `Disabled`-mode bound: option (b), recorded as a Breaking change
> - [x] Edge case: a request-wise POST stream held by a vanished peer is bounded by TCP, not by a stream-age cap (see FR-008)
> - [ ] Edge cases: shadow-stream accounting
> - [ ] NFR-007: distinct log line for an rmcp-originated expiry
> - [ ] Whether a CLI/config setting for the session idle timeout is wanted

## 11. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[mcp/006-http-stream-liveness/spec|mcp/006 http-stream-liveness]] — the probe this feature completes
- [[mcp/002-mcp-resources-diagnostics/spec|mcp/002 mcp-resources-diagnostics]] — FR-008, the reaper contract and the documented caveat
- [[mcp/003-mcp-2026-stateless-adoption/spec|mcp/003 mcp-2026-stateless-adoption]] — stateless paths excluded here
