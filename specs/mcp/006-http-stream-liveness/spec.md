---
aliases:
  - HTTP stream liveness
  - SSE liveness probe
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
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp-resources-diagnostics]]"
---

# Feature: Transport-Independent Liveness of HTTP Streams

> [!info] Metadata
> **Type**: bug / hardening
> **Priority**: P2
> **Related issues**: #543; complements #552 (`TCP_USER_TIMEOUT`, which addressed #531 at the kernel level); #551 (stateless `subscriptions/listen` streams, section 5)

## 1. Overview

### Problem Statement

A peer that vanishes without closing its TCP connection (sleeping laptop, dropped NAT mapping,
proxy that kept the upstream connection open) leaves its session's standalone GET (SSE) stream
open until the OS gives up retransmitting (roughly 15-30 minutes on Linux). Until then the
stream holds a connection permit and a session slot, and `rmcp`'s worker can stay parked on a
full common channel. The kernel-level `TCP_USER_TIMEOUT` set by #552 is Linux/Android-only and does not see through a reverse proxy; this feature is its portable complement, not a replacement.

### Goal

Detect a dead GET-stream peer at the MCP layer, portably, within a bounded and configurable
time, without ending the session.

### Out of Scope

- Probing stateless `subscriptions/listen` streams: no session exists to acknowledge a probe, so they use the lease of section 5 (#551).
- Request-wise (POST response) streams and their resumes.
- Closing the session itself; the idle reaper bounds vanished clients (mcp/002, #521) and is the only expiry owner: rmcp's own `keep_alive` is off, so a client that answers probes on an open GET stream keeps its session (mcp/010, #573). With probing switched off (FR-009) an open GET stream no longer holds its session.

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHILE probing is enabled THE SYSTEM SHALL send an MCP `ping` request on each session's standalone GET stream every probe interval (default 60 s) | must |
| FR-002 | WHEN a client does not answer a probe within the probe deadline (default 30 s) THE SYSTEM SHALL end that GET stream, drop its `rmcp` receiver and release its stream guard, even if the HTTP layer no longer polls the stream | must |
| FR-003 | WHEN a client answers a probe, by a JSON-RPC response or an error with an `id` that identifies the probe, THE SYSTEM SHALL treat the stream as alive and arm the next probe, and SHALL NOT forward that reply to the MCP service | must |
| FR-004 | THE SYSTEM SHALL scope pending probes to the session that sent them, so an answer posted in one session never keeps another session's stream alive | must |
| FR-005 | THE SYSTEM SHALL forward an error response without an `id`, and every other client message, unchanged | must |
| FR-006 | THE forwarding task SHALL never wait on an unbounded send to the HTTP layer. A probe is queued ahead of buffered messages and delivered when the outbound buffer has room; the deadline runs from the probe tick, so a client that stalls and drains within the deadline keeps its stream | must |
| FR-007 | WHEN the stream that mirrors `rmcp`'s primary common channel times out THE SYSTEM SHALL, after dropping its receiver, close the session's standalone stream in `rmcp` on a detached, time-bounded task so stale shadow streams are cleared and the client's reconnected GET becomes the primary; a shadow timeout SHALL drop only its own stream | must |
| FR-008 | THE SYSTEM SHALL probe a resume only when its `Last-Event-ID` names the common channel (`<index>` form, not `<index>/<request>`) | must |
| FR-009 | THE embedder and the CLI SHALL be able to switch probing off (`StreamLiveness::Disabled`, `--http-stream-liveness off`, `MCPLS_HTTP_STREAM_LIVENESS`) for clients that ignore server `ping` requests; interval and deadline are non-zero typed values configurable only through `HttpConfig` | must |

## 3. Design Notes

- Probing is unconditional per stream (not only when idle): it detects a dead peer even when
  notifications flow into a proxy that keeps accepting them.
- Under `Probe`, a GET on a session whose slot has been removed fails instead of returning an
  unprobed stream.
- Implementation: `crates/mcpls-core/src/transport/liveness.rs`; wiring in
  `CappedSessionManager` (`transport.rs`).
- Probe ids are `mcpls-liveness-<n>` (per-session counter); `rmcp`'s own server ids are numeric.
- Primary/shadow tracking mirrors `rmcp`'s `resume_or_shadow_common`. A task clears its primary
  marker before dropping the inner receiver, and the role is decided after the inner call
  returns, so a sequential reconnect cannot leave a promoted primary tracked as a shadow.
  Two concurrent GETs on one session can invert the mirror; a chained inversion can leave a live
  shadow stream that is never kicked. Both are rare and end when the client reconnects.
- `EventId` classification relies on `rmcp`'s `Display` rendering; a unit test pins the format.

## 4. Success Criteria

| ID | Criterion |
|----|-----------|
| SC-001 | A GET stream whose client never answers ends within interval + deadline, even when nothing polls the outer stream |
| SC-002 | A client that answers keeps its stream open across several intervals (real-socket test) |
| SC-003 | `--http-stream-liveness off` disables probing and the flag parses with its env var |

## 5. Stateless Listen Lease (#551)

### Problem

A stateless 2026-07-28 `subscriptions/listen` stream has no session, and a client cannot answer a
server `ping` (no server-to-client requests, no session to POST to), so section 2's probe cannot
apply. A peer that vanished leaves the stream, its listen slot (`MAX_LISTEN_STREAMS`) and its
registration until the OS gives up (macOS and Windows: kernel default; behind a proxy: the proxy
hop is what the kernel sees).

### Design

The stream gets a lease. When it elapses the HTTP response body ends abruptly: a normal end of the
SSE body with no JSON-RPC response. Per the 2026-07-28 spec a final `SubscriptionsListenResult`
means a clean close, and only a transport close without it may trigger a reconnect (`rmcp`:
`SubscriptionEnd::Abrupt` means listen again). A live client therefore re-listens at once; a
vanished one never does and its slot is free after one lease.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-010 | WHEN `listen()` runs over HTTP with the lease on THE SYSTEM SHALL end that stream's HTTP body after a lease drawn uniformly from the lease window (default 15-30 minutes), without a final JSON-RPC result | must |
| FR-011 | THE lease SHALL be flagged by `listen()` through a request-extension slot (`ListenLeaseSlot`) and SHALL NOT depend on matching the `Mcp-Method` header; a POST whose response is not a `subscriptions/listen` stream, and stdio, SHALL be unaffected | must |
| FR-012 | THE lease SHALL be on while `StreamLiveness::Probe` is configured and off with `StreamLiveness::Disabled` (`--http-stream-liveness off`); `HttpConfig::with_listen_lease` overrides either | must |
| FR-013 | THE stateless SSE response SHALL NOT carry SSE `id:` fields (no event store), because a resumable stream would make the abrupt end resumable | must |
| FR-014 | WHEN a listen registers THE SYSTEM SHALL replay `resources/updated` for each requested URI that has cached diagnostics or whose empty entry was evicted within the last 2 minutes (ring of at most 256 entries, recording every removal of an empty entry, capacity and alias evictions alike, written only by the cache's removal path), so a clear published in the lease gap is not lost | must |

### Notes

- The jitter keeps streams opened together from expiring together.
- Replaying cached URIs plus recent evictions, rather than every requested URI, keeps a replay
  within `rmcp`'s default 64-slot client subscription buffer. Replaying evicted non-empty
  entries is deliberately not done: clients would re-read "not published" and drop valid errors.
- A client that never re-listens stops receiving push updates after one lease but can still read
  resources (`resources/read`, cached diagnostics). The remedy is `--http-stream-liveness off`.
- Known leftover (dates from #522): a slow consumer with more than 64 cached URIs can still
  overflow its buffer; tracked as a follow-up.
- A ring of 256 entries can overflow within seconds under eviction churn; a clear evicted before
  the re-listen and pushed out of the ring is not replayed.
- Implementation: `crates/mcpls-core/src/transport/lease.rs` (`LeaseWindow`, `ListenLease`,
  `ListenLeaseSlot`, `LeasedBody`, `attach_listen_lease`), `McplsServer::listen`
  (`mcp/server.rs`), eviction ring in `bridge/notifications.rs`.

### Success Criteria

| ID | Criterion |
|----|-----------|
| SC-004 | A real-socket listen with a short lease ends with no `result` and no `id:` field, frees its slot, and a re-listen succeeds |
| SC-005 | A real `rmcp` client at the default 64-slot buffer, listening on 200 URIs with 10 cached and one evicted clear, receives exactly the 11 replayed updates, sees `SubscriptionEnd::Abrupt`, and gets the same replay on re-listen |
| SC-006 | The lease is off with `StreamLiveness::Disabled`; lease draws stay within the window; the eviction ring is bounded, expires and ignores non-empty evictions |
