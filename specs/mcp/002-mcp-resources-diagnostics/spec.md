---
aliases:
  - MCP resources diagnostics
  - Diagnostics subscriptions
tags:
  - sdd
  - spec
  - enhancement
  - mcp
  - resources
  - diagnostics
created: 2026-07-30
status: implemented
related:
  - "[[constitution]]"
  - "[[bridge/004-get-diagnostics-flycheck-gap/spec]]"
---

# Feature: Expose LSP Diagnostics as MCP Resources with Subscriptions

> [!info] Metadata
> **Type**: enhancement
> **Priority**: P3

## 1. Overview

### Problem Statement

mcpls currently exposes LSP diagnostics only as polling tools (`get_diagnostics`,
`get_cached_diagnostics`). The MCP 2025-11-25 specification introduces first-class
**Resources** with **subscriptions** — clients can subscribe to a resource URI and receive
`notifications/resources/updated` push events when the resource changes.

Competing implementations already use this pattern:
- **Tritlo/lsp-mcp** (Haskell) exposes `lsp-diagnostics://` as a subscribable resource with
  real-time updates when files change.

mcpls's current diagnostics model is a pull-based cache (`NotificationCache`) populated by
the LSP push pump. This is already the right data structure — the gap is surfacing it through
the MCP resource/subscription interface instead of (or in addition to) tool calls.

## 2. User Stories

### US-001: Push notifications for diagnostics changes

AS AN AI agent editing a file,
I WANT to receive a notification when new diagnostics arrive rather than polling `get_diagnostics` repeatedly,
SO THAT I can respond to errors and warnings in real time.

### US-002: Subscribe to diagnostics resources

AS AN MCP client,
I WANT to subscribe to `lsp-diagnostics://path/to/file` and receive updates without issuing repeated tool calls,
SO THAT my implementation can be event-driven rather than polling-based.

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | mcpls must declare `resources` capability with `subscribe: true` in its `ServerInfo` | must |
| FR-002 | mcpls must implement `resources/list` returning URIs of the form `lsp-diagnostics://<absolute-path>` for each open document | must |
| FR-003 | mcpls must implement `resources/read` for `lsp-diagnostics://` URIs, returning the current `NotificationCache` contents for that path as JSON | must |
| FR-004 | mcpls must implement `resources/subscribe` — when a client subscribes, the notification pump must emit `notifications/resources/updated` whenever the diagnostics cache for that path changes. Every accepted push notifies; a `get_diagnostics` pull that changes the file's merged diagnostics also notifies, and an identical pull does not ([[bridge/009-diagnostics-subscription-staleness/spec\|bridge/009]], #574). A push or pull write that evicts another file's entry also notifies that file's subscribers once ([[bridge/009-diagnostics-subscription-staleness/spec\|bridge/009]] FR-013, #649) | must |
| FR-005 | Existing `get_cached_diagnostics` tool must remain for clients that do not support resources | must |
| FR-006 | `notifications/resources/updated` must be delivered per session: a session receives it only for URIs that session itself subscribed to, and a stalled session must not delay or lose updates for any other session (#468) | must |
| FR-007 | Over HTTP, `notifications/resources/updated` must be sent on the session's standalone GET (SSE) stream; subscriptions require a session established via the `initialize` handshake (#468, #482) | must |
| FR-008 | Over HTTP, a session is closed (freeing its session-cap slot) once it has had no inbound client request and no open response stream for 5 minutes; outbound `resources/updated` does not count as activity. The clock starts at the later of the last request and the last stream close; a cleanly closed stream is noticed on the next write (at most one 15 s SSE keep-alive later), while a silently vanished peer (half-open TCP) holds its stream only until the MCP liveness probe ends it (default interval 60 s plus deadline 30 s, see [[mcp/006-http-stream-liveness/spec]]); with probing switched off an open GET stream is no proof of life, so it does not hold its session: the session expires after 5 minutes without an inbound request even while the stream receives notifications (Breaking, mcp/010 FR-008), and the stream itself is held until the OS drops the connection: on Linux and Android accepted sockets carry `TCP_USER_TIMEOUT` of 60 s, so about 75 s after the peer vanishes, while macOS and Windows keep the kernel default (about 15-30 minutes) and behind a reverse proxy the proxy's timeouts govern (#531, #543). A client with an open GET stream that answers probes is never closed by this rule, and rmcp's own `keep_alive` is switched off so this reaper is the only expiry owner (#521, #573). A POST response stream whose peer vanished mid-write holds its session until TCP gives up (no longer 5 minutes) | must |
| FR-009 | `get_cached_diagnostics` and `resources/read` must resolve the file's diagnostics-route server with the same registered / expected / failed-to-start classification as the pull tools: while the server is still starting they return the retryable `ServerInitializing` error, and once it failed to start they return `ServerFailedToStart`, never an empty list. The failed-to-start case is #552's (#535); the still-starting case is #545. A file whose language has no configured server still reads as an empty cache | must |
| FR-010 | `resources/subscribe` for a file whose server failed to start must return `ServerFailedToStart` and leave the session's subscriptions unchanged (a subscription this call added is rolled back; a re-subscribe keeps the earlier one). Subscribing while the server is still starting succeeds (#544) | must |
| FR-011 | `subscriptions/listen` treats each URI like a subscribe: when every URI's server failed to start the stream is refused with `ServerFailedToStart`; otherwise each failed URI is published once so the client re-reads and gets the error (#544) | must |
| FR-012 | When initialization settles (including after a contained init-task panic), mcpls must publish `resources/updated` once to each session subscribed to a URI whose server failed to start, so a client waiting on a subscribe made during startup learns of the failure. Subscribe records before it resolves the route and settle mutates before it publishes, so one side always observes the other: a client never misses both. It sees the subscribe error, one such update, or both (a stray update for a rolled-back URI is harmless; the client re-reads and gets the error either way) (#544) | must |
| FR-013 | The diagnostics pump maps each published URI to its canonical form before caching it, through a pump-local resolver (`bridge/published_uri.rs`): resolved paths are memoized for 60 s (at most 8192 entries; a full memo drops expired entries, then the oldest half), and cache misses are canonicalized on the blocking pool with at most 8 in flight. The pump drains up to 64 queued notifications per round, resolves their paths together and applies them strictly in arrival order, so a cold burst of thousands of first publishes neither serialises on a slow filesystem nor reorders or drops a clear (#550) | must |
| FR-014 | Only definitive canonicalizations are memoized: a path whose file or directories are missing (`NotFound`, `NotADirectory`) resolves through its nearest existing ancestor and is cached (a path with a `..` component is refused, since joining the missing tail lexically could step around a symlink); any other filesystem error (`ESTALE`, `EIO`, `EACCES`) is transient, is not cached and is retried in place after 50 ms and 200 ms. If it persists, a clear (a publish with no diagnostics) is placed under the path's last known canonical form (even an expired memo entry), or failing that under its own lexically normalized spelling, which is never reported as canonical (so it cannot pass for the real key in the alias index); either way the client does not keep stale errors for a file the server already cleared. Like every empty publish it empties only its own source's entry, not the other spellings of the file. A path that failed transiently is left alone for 2 s, so a broken ancestor costs one 250 ms retry round rather than one per publish, while a publish carrying diagnostics is dropped until the next one. The server's diagnostics role is read again after the batch resolves, immediately before each cache write, so a demotion to secondary during a slow resolve stops that batch from being cached or notified (#174 section 8). While a batch resolves the pump keeps servicing the lifecycle lane (indexing and progress signals) and cancellation; the diagnostics lane waits, preserving order. Resolution happens before the batch is applied, so a clear can neither be lost nor reordered behind a later publish for the same URI. The workspace containment check is the unified one from #580 (a lexical `admits_uri` pre-filter, then `contains_canonical` on the resolved path) and runs after the memo on every publish, so a memo entry that outlives a symlink retarget (up to 60 s) can only mis-key a file already inside the workspace (#550) | must |
| FR-015 | Resolving a batch races pump cancellation: a canonicalization stuck on a hung filesystem cannot hold the diagnostics task past shutdown. The blocking call itself cannot be interrupted and is abandoned, so each pump generation can leave up to 8 blocking threads behind, and a respawn loop against a permanently hung filesystem grows toward tokio's blocking-pool limit. The first single canonicalization slower than 100 ms logs one warning per pump generation (#550) | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Zero extra LSP round-trips: use the existing notification pump output |
| NFR-002 | Compatibility | rmcp crate must expose resource registration and `notifications/resources/updated` send API — verify before implementation (rmcp 3.5.1 currently used) |
| NFR-003 | Performance | Steady-state publishes for already-seen files cost no filesystem call (memo hit); a cold burst is bounded by 8 parallel canonicalizations rather than one per publish in sequence (#550) |

## 5. See Also

- [[constitution]] — project principles
- [[bridge/004-get-diagnostics-flycheck-gap/spec]] — uses and extends the `NotificationCache` design from this spec
- MCP 2025-11-25 resources spec: https://modelcontextprotocol.io/specification/2025-11-25/server/resources
- Tritlo/lsp-mcp: https://github.com/Tritlo/lsp-mcp
- mcpls NotificationCache: `crates/mcpls-core/src/bridge/notifications.rs`
