---
aliases:
  - HTTP typed limits, allowed origins and stream deadline
tags:
  - sdd
  - spec
  - enhancement
  - http
  - security
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[mcp/006-http-stream-liveness/spec|http-stream-liveness]]"
  - "[[mcp/010-http-session-keepalive-with-live-stream/spec|http-session-keepalive-with-live-stream]]"
---

# Feature: Typed HTTP body limit, extra allowed origins and a response stream deadline

> [!info] Metadata
> **Type**: enhancement
> **Priority**: P3
> **Author**: Andrei G.
> **Issues**: #584, #585, #587
> **Observed at**: `e3d9c66`

## 1. Overview

### Problem Statement

- `HttpConfig::max_request_body_bytes` was a bare `usize` that accepted `0` (rejecting every POST)
  and any large value, unlike its sibling limits `ConnectionLimit` and `SessionLimit` (#585).
- The only browser origins accepted were loopback ones on the bound port. A page served from
  another origin, reaching mcpls through a tunnel or a proxy that rewrites `Host`, got `403` with
  no way to allow it (#584).
- A POST or request-wise resume response stream had no lifetime bound once rmcp's `keep_alive` was
  switched off, so a peer that vanished mid-write held its session slot until TCP gave up (#587).

### Goal

The body cap is a validated type, extra origins are a validated allowlist, and response streams end
after a bounded time, without changing any default behavior for existing deployments.

### Out of Scope

- A `Host` allowlist (`--http-allowed-host`): rmcp validates `Host` against a loopback-only list
  before `Origin`, so direct access with a non-loopback `Host` stays `403` (follow-up #597).
- Freeing the session slot and connection permit of a peer that stopped reading a response (follow-up #600).

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE body cap SHALL be `RequestBodyLimit`, built by `RequestBodyLimit::new`: `None` for `0`, values above 64 MiB clamped to 64 MiB, default 4 MiB; `HttpConfig::max_request_body` and `with_max_request_body` replace `max_request_body_bytes`, `with_max_request_body_bytes` and `DEFAULT_MAX_REQUEST_BODY_BYTES` | must |
| FR-002 | THE clamp policy SHALL match `ConnectionLimit` and `SessionLimit` (all three come from one macro, which asserts at compile time that the default does not exceed the maximum). There is no TOML or CLI surface, so a clamp cannot hide an operator typo, and the 64 MiB maximum bounds rmcp's in-memory body buffering | must |
| FR-003 | `AllowedOrigin` SHALL be built from `http://` or `https://` strings only and SHALL reject: other or missing schemes, userinfo, a path other than `/`, a query, `null`, wildcards (`*`, `:*`) and unbracketed IPv6 hosts, with a typed `InvalidAllowedOrigin`; a port suffix that is not 1-5 ASCII digits within `u16` is `Malformed` rather than silently replaced by the scheme default; surrounding whitespace is trimmed, so `MCPLS_HTTP_ALLOWED_ORIGINS="https://a.com, https://b.com"` works | must |
| FR-004 | `AllowedOrigin` SHALL store a lowercase host and an explicit port (the scheme default when absent) and display as `scheme://host:port`, IPv6 hosts bracketed | must |
| FR-005 | `HttpConfig::allowed_origins` and `with_allowed_origins` SHALL add origins to the loopback origins of the bound port (`AllowedOrigin::loopback`); the CLI exposes them as `--http-allowed-origin` (repeatable, `MCPLS_HTTP_ALLOWED_ORIGINS`, comma-separated) | must |
| FR-006 | EVERY configured origin SHALL be accepted by rmcp's own matcher against the `Origin` a browser sends: rmcp drops allowlist entries it cannot parse, so an end-to-end test covers a portless `Origin` against a stored `:443`, bracketed IPv6, an uppercase host and a non-default port, while a foreign origin stays `403` | must |
| FR-007 | THE documentation SHALL state that allowed origins serve pages whose requests reach mcpls with a loopback `Host`, and do not relax the `Host` check | must |
| FR-008 | POST and non-common resume response streams SHALL end `ResponseStreamDeadline` after they open (default 1 hour, non-zero), in both liveness modes; the cut logs the session fingerprint at `debug` | must |
| FR-009 | THE default deadline SHALL satisfy `DEFAULT >= 2 * MAX_TIMEOUT_SECONDS + INDEXING_STALENESS_BOUND + 300 s`, checked at compile time: a single tool call can spend a respawn of up to 900 s, an indexing wait under 60 s and a request of up to 900 s | must |
| FR-010 | THE deadline SHALL be a total lifetime bound counted from when the stream opens, not an idle timeout, so the cut ends the stream whatever it is still sending, after which the stream no longer holds the session open and the reaper expires the session after the idle timeout; WHILE hyper's write of a response to a non-reading peer is stuck the body is not polled, so the cut cannot fire and both the session slot and the connection permit wait for TCP (#600); the GET probe has the same limit | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | Body cap, origin and deadline are validated newtypes; no raw `usize` or `String` crosses the config boundary |
| NFR-002 | Compatibility | Defaults keep today's behavior: 4 MiB body cap, loopback-only origins; the only behavior change is the 1 hour stream cut |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `RequestBodyLimit::new(usize::MAX)` | Clamped to 64 MiB |
| `Origin: https://app.example.com` against a stored `https://app.example.com:443` | Accepted |
| `Origin: http://app.example.com` against a stored `https` entry | `403` |
| Request with a non-loopback `Host` and an allowed `Origin` | `403` from the `Host` check (out of scope, #597) |
| Long tool call near the worst case | Finishes inside the 1 hour deadline (FR-009); a client that lost the stream re-requests |
| Server answers `ServerCancelled` or `ContentModified` up to three times, each with a full request timeout | Pathological; not covered by the derived bound |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Paused-time test of a response stream with a 20 s deadline | ends at 20 s; the session is reaped 10 s later (idle timeout 10 s) |
| SC-002 | End-to-end `Origin` matrix through `spawn_http_server` | every configured form accepted, unlisted forms `403` |
| SC-003 | `--http-allowed-origin` parsing | repeats and splits on commas; each invalid shape rejected with a validation error |

## 10. See Also

- [[constitution]] -- project principles
- [[MOC-specs]] -- all specifications
- [[mcp/006-http-stream-liveness/spec|mcp/006]] -- GET stream probing
- [[mcp/010-http-session-keepalive-with-live-stream/spec|mcp/010]] -- session expiry rules (FR-008)
- Code: `crates/mcpls-core/src/transport.rs`, `crates/mcpls-cli/src/args.rs`
