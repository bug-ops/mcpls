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

# Feature: Transport-Independent Liveness of HTTP GET Streams

> [!info] Metadata
> **Type**: bug / hardening
> **Priority**: P2
> **Related issues**: #543; complements #552 (`TCP_USER_TIMEOUT`, which addressed #531 at the kernel level); follow-up #551 (stateless `subscriptions/listen` streams)

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

- Stateless `subscriptions/listen` streams: no session exists to acknowledge a probe (#551).
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
