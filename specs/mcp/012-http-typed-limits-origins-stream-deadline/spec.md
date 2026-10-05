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
> **Issues**: #584, #585, #587, #597, #600, #602
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

- The `Host` check was loopback-only and ran before `Origin`, so a deployment behind a name (a reverse
  proxy, a tunnel) or bound to `0.0.0.0` had no way to be reached directly (#597).
- A peer that stopped reading a response held its connection permit, and its session slot until the
  idle reaper expired it, for as long as TCP kept the connection (#600).
- A response sent while the request body was still unread (an early `403` or `413`) could be lost to
  a connection reset on close (#602).

### Out of Scope

- A peer that trickles one read or one request-body chunk per timeout window still holds its
  permit; that needs a minimum-rate bound (#613).

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE body cap SHALL be `RequestBodyLimit`, built by `RequestBodyLimit::new`: `None` for `0`, values above 64 MiB clamped to 64 MiB, default 4 MiB; `HttpConfig::max_request_body` and `with_max_request_body` replace `max_request_body_bytes`, `with_max_request_body_bytes` and `DEFAULT_MAX_REQUEST_BODY_BYTES` | must |
| FR-002 | THE clamp policy SHALL match `ConnectionLimit` and `SessionLimit` (all three come from one macro, which asserts at compile time that the default does not exceed the maximum). There is no TOML or CLI surface, so a clamp cannot hide an operator typo, and the 64 MiB maximum bounds rmcp's in-memory body buffering | must |
| FR-003 | `AllowedOrigin` SHALL be built from `http://` or `https://` strings only and SHALL reject: other or missing schemes, userinfo, a path other than `/`, a query, `null`, wildcards (`*`, `:*`) and unbracketed IPv6 hosts, with a typed `InvalidAllowedOrigin`; a port suffix that is not 1-5 ASCII digits within `u16` is `Malformed` rather than silently replaced by the scheme default; surrounding whitespace is trimmed, so `MCPLS_HTTP_ALLOWED_ORIGINS="https://a.com, https://b.com"` works | must |
| FR-004 | `AllowedOrigin` SHALL store a lowercase host and an explicit port (the scheme default when absent) and display as `scheme://host:port`, IPv6 hosts bracketed | must |
| FR-005 | `HttpConfig::allowed_origins` and `with_allowed_origins` SHALL add origins to the loopback origins of the bound port (`AllowedOrigin::loopback`); the CLI exposes them as `--http-allowed-origin` (repeatable, `MCPLS_HTTP_ALLOWED_ORIGINS`, comma-separated) | must |
| FR-006 | EVERY configured origin SHALL be accepted by rmcp's own matcher against the `Origin` a browser sends: rmcp drops allowlist entries it cannot parse, so an end-to-end test covers a portless `Origin` against a stored `:443`, bracketed IPv6, an uppercase host and a non-default port, while a foreign origin stays `403` | must |
| FR-007 | THE documentation SHALL state that allowed origins do not relax the `Host` check, which runs first | must |
| FR-008 | POST and non-common resume response streams SHALL end `ResponseStreamDeadline` after they open (default 1 hour, non-zero), in both liveness modes; the cut logs the session fingerprint at `debug` | must |
| FR-009 | THE default deadline SHALL satisfy `DEFAULT >= 2 * MAX_TIMEOUT_SECONDS + INDEXING_STALENESS_BOUND + 300 s`, checked at compile time: a single tool call can spend a respawn of up to 900 s, an indexing wait under 60 s and a request of up to 900 s | must |
| FR-010 | THE deadline SHALL be a total lifetime bound counted from when the stream opens, not an idle timeout, so the cut ends the stream whatever it is still sending, after which the stream no longer holds the session open and the reaper expires the session after the idle timeout; WHILE hyper's write of a response to a non-reading peer is stuck the body is not polled, so the cut cannot fire; the write-stall deadline (FR-015) frees the connection permit and the session slot is freed after the idle timeout; the GET probe has the same limit | must |
| FR-011 | `AllowedHost` SHALL be a host name or IP literal with an optional port and SHALL reject, with a typed `InvalidAllowedHost`: wildcards (`*`), userinfo (`@`), an empty host, an empty port (`host:`), a port that is not a number up to 65535, a trailing dot and non-ASCII input (an internationalized name is written in punycode), because rmcp compares hosts as plain lowercase strings, and anything `http::uri::Authority` does not parse (schemes, paths, unbracketed IPv6); surrounding whitespace is trimmed | must |
| FR-012 | `AllowedHost` SHALL store a lowercase host with IPv6 brackets and an optional port, and display as `host` or `host:port` so the value re-parses as an authority in rmcp's allowlist, which falls back to a raw string for entries it cannot parse; no port matches any port, a port matches only that port | must |
| FR-013 | THE accepted `Host` values SHALL be the loopback names (`localhost`, `127.0.0.1`, `[::1]`, any port), the bound IP literal on any port WHEN the bind address is neither unspecified nor loopback (a client reaching a `:80` or `:443` bind sends no port, and rmcp requires a pinned port to be present), and `HttpConfig::allowed_hosts` (`with_allowed_hosts`); there is no wildcard, so a `0.0.0.0` or `[::]` bind accepts only the loopback names and the configured hosts | must |
| FR-014 | THE CLI SHALL expose `--http-allowed-host` (repeatable, `MCPLS_HTTP_ALLOWED_HOSTS`, comma-separated, blank segments ignored) mirroring `--http-allowed-origin`, and the non-loopback bind warning and the DNS-rebinding documentation SHALL describe the real `Host` rules | must |
| FR-015 | EVERY accepted connection SHALL fail a write that makes no progress for `WriteStallTimeout` (default 30 s, non-zero, `HttpConfig::write_stall_timeout`, no CLI flag) with `TimedOut`, covering `poll_write`, `poll_write_vectored` and `poll_shutdown`; a write with progress disarms the timer and the next stall starts a fresh deadline; a peer that drains one send buffer per window is not detected | must |
| FR-016 | A CLEAN connection close SHALL send the FIN and then discard incoming bytes until EOF, a read error, 2 s without data, 1 MiB discarded or `min(header_read_timeout, 30 s)` in total, reading at most 16 buffers per poll, resetting the idle timer once per batch and sharing one timer with the stall deadline, so an early `403` or `413` reaches a peer that is still sending a body of up to 1 MiB (a longer body is reset after the cap); closes after a header timeout, a stall or an error do not linger, and no linger starts (or a running one ends) once the server is shutting down | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | Body cap, origin and deadline are validated newtypes; no raw `usize` or `String` crosses the config boundary |
| NFR-002 | Compatibility | Defaults keep today's accepted requests: 4 MiB body cap, loopback-only origins and hosts; behavior changes are the 1 hour stream cut, the 30 s write-stall cut and the close linger (at most 30 s) |
| NFR-003 | Type safety | Allowed host and write-stall timeout are validated newtypes |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `RequestBodyLimit::new(usize::MAX)` | Clamped to 64 MiB |
| `Origin: https://app.example.com` against a stored `https://app.example.com:443` | Accepted |
| `Origin: http://app.example.com` against a stored `https` entry | `403` |
| Request with an unlisted `Host` and an allowed `Origin` | `403` from the `Host` check |
| `Host: pinned.example` against a stored `pinned.example:8443` | `403` (a pinned port must match and be present) |
| `Host: mcp.example.com` (default port omitted) against a stored `mcp.example.com` | Accepted; a pin to `:80` or `:443` is rejected at parse time ([[mcp/013-http-allowed-host-default-port-rejection/spec\|mcp/013]]) |
| `--http-allowed-host example.com.` or `bücher.example` | Rejected (trailing dot; use punycode) |
| `--http-allowed-host example.com:` | Rejected with a validation error |
| Peer stops reading a streamed response | Connection closed after the write-stall timeout; permit freed |
| Peer keeps sending after an early `403` | Status delivered, then closed once the linger budget is spent |
| Long tool call near the worst case | Finishes inside the 1 hour deadline (FR-009); a client that lost the stream re-requests |
| Server answers `ServerCancelled` or `ContentModified` up to three times, each with a full request timeout | Pathological; not covered by the derived bound |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Paused-time test of a response stream with a 20 s deadline | ends at 20 s; the session is reaped 10 s later (idle timeout 10 s) |
| SC-002 | End-to-end `Origin` matrix through `spawn_http_server` | every configured form accepted, unlisted forms `403` |
| SC-003 | `--http-allowed-origin` and `--http-allowed-host` parsing | repeat and split on commas; each invalid shape rejected with a validation error |
| SC-004 | End-to-end `Host` matrix through `spawn_http_server` | configured hosts accepted (pinned port exact), unlisted hosts `403` |
| SC-005 | Real-socket early `403` and `413` with a 512 KiB unread body | status line delivered, then a clean EOF |
| SC-006 | Real-socket non-reading peer on a one-connection cap | next client served within the write-stall timeout plus slack |

## 10. See Also

- [[constitution]] -- project principles
- [[MOC-specs]] -- all specifications
- [[mcp/006-http-stream-liveness/spec|mcp/006]] -- GET stream probing
- [[mcp/010-http-session-keepalive-with-live-stream/spec|mcp/010]] -- session expiry rules (FR-008)
- Code: `crates/mcpls-core/src/transport.rs`, `crates/mcpls-cli/src/args.rs`
