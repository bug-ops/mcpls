---
aliases:
  - Diagnostics publish burst drops
  - Notification channel overflow
tags:
  - sdd
  - spec
  - bug
  - lsp-bridge
  - diagnostics
  - backpressure
created: 2026-10-06
status: implemented
related:
  - "[[constitution]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001-lsp-server-lifecycle-and-respawn]]"
  - "[[lsp/012-client-publish-diagnostics-capability/spec|lsp/012-client-publish-diagnostics-capability]]"
  - "[[lsp/011-selection-folding-range-tools/spec|lsp/011-selection-folding-range-tools]]"
  - "[[bridge/009-diagnostics-subscription-staleness/spec|bridge/009-diagnostics-subscription-staleness]]"
  - "[[bridge/011-push-only-server-diagnostics/spec|bridge/011-push-only-server-diagnostics]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp/002-mcp-resources-diagnostics]]"
  - "[[runtime/004-server-text-hygiene/spec|runtime/004-server-text-hygiene]]"
---

# Feature: `publishDiagnostics` bursts above the notification channel capacity are not lost silently

> [!info] Metadata
> **Type**: bug
> **Priority**: P2
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle 040, live-test finding
> **Issue**: #704

> [!abstract]
> The LSP client message loop hands `textDocument/publishDiagnostics`, `window/logMessage` and
> `window/showMessage` to the bridge over one bounded channel (256 frames) with a non-blocking
> send. When a server publishes diagnostics for many files in one burst the channel fills, the
> excess frames are discarded with one WARN each, and nothing else changes: the dropped file reads
> `availability: pending`, no `resources/updated` fires, and no degradation flag is set, although
> the diagnostics cache (1000 entries) would have held them. This spec requires that a publish is
> either delivered or its loss is recorded and surfaced, and that the log noise of an overflow is
> rate limited.

## 1. Overview

### Problem Statement

`LspClient::message_loop_inner` (`crates/mcpls-core/src/lsp/client.rs`) routes each parsed
notification to a lane. `PublishDiagnostics`, `LogMessage` and `ShowMessage` share the
notification lane, a `tokio::sync::mpsc` channel of `NOTIFICATION_CHANNEL_CAPACITY = 256`
(`crates/mcpls-core/src/lsp/lifecycle.rs`). The frame is sent with `try_send`; on a full channel the
only effect is `warn!("Dropping notification: lane=..., method=... (channel full or closed)")`
and the frame is discarded.

The bridge drains the channel into the diagnostics cache, which is bounded separately by
`MAX_DIAGNOSTIC_ENTRIES = 1000`, shared across servers with per-owner fair-share eviction
(#266, #276, #284). The channel is therefore the narrower bound: a burst of 257 or more frames
that arrives faster than the bridge drains it loses frames even though the cache has room for
every one of them.

Servers legitimately publish such bursts: project-wide diagnostics after a build or flycheck,
a monorepo, background indexing, a workspace reload. The finding is not limited to servers that
publish for many files at once on first open; any burst above the channel capacity loses its tail.

**Reproduced live (release binary at `12ee3a1`).** A scratch stdio server that, on the first
`didOpen`, publishes one error-severity diagnostic for each of N distinct existing files, then a
driver that opens one file through `get_hover`, waits 4 s and reads `get_cached_diagnostics` for
every flooded file:

| Flooded files (N) | Files missing from the cache | Dropped-frame WARN lines |
|-------------------|------------------------------|--------------------------|
| 200 | 0 | 0 |
| 300 | 0 | 0 |
| 400 | 1 | 1 |
| 600 | 113 (the tail of the burst) | 113 |
| 1100 | 139 more than the cache bound alone explains | 139 |

Missing files read `availability: pending` with an empty list.

**Why it matters.**

- A dropped file reads `pending`, the same state as "the server has not published yet". An agent
  that waits and retries waits for a publish that already happened and was discarded.
- A server publishes a file's diagnostics again only when the file changes, so the loss can be
  permanent for the session.
- No `resources/updated` is sent for the lost publish, so a subscriber never learns of an error.
- `push_notifications_degraded` stays false and no other flag marks the loss, so no signal tells
  the caller the cache view is incomplete. This is the silent-incompleteness class settled as a
  correctness issue in [[bridge/004-get-diagnostics-flycheck-gap/spec|bridge/004]] and
  [[bridge/009-diagnostics-subscription-staleness/spec|bridge/009]].
- Each dropped frame writes its own WARN line, unthrottled: hundreds of lines per burst. Other
  server-controlled log noise is rate limited (undecodable frames, at most one WARN a minute with a
  dropped count, #681, see [[lsp/011-selection-folding-range-tools/spec|lsp/011]]).

### Goal

When a server publishes diagnostics faster than the bridge drains them, no publish is lost
without a trace: below the cache bound every publish reaches the cache, and any publish that
cannot be delivered is recorded and surfaced so that the affected files do not read as `pending`,
and the overflow produces bounded log output.

### Out of Scope

- Raising `NOTIFICATION_CHANNEL_CAPACITY` as the only fix: a constant increase moves the threshold
  without removing the silent loss and is a plan-time tuning option, not the requirement.
- The cache bound itself (`MAX_DIAGNOSTIC_ENTRIES`), its fair-share eviction and the `evicted`
  availability for capacity eviction (settled in
  [[bridge/011-push-only-server-diagnostics/spec|bridge/011]] and
  [[bridge/009-diagnostics-subscription-staleness/spec|bridge/009]]).
- Servers that publish more distinct files than the cache bound: loss there is the existing,
  already surfaced capacity eviction.
- Throttling or batching `resources/updated` delivery beyond what exists today.
- The lifecycle lane's capacity and its `$/progress` filtering, except to state that it is not
  affected (Section 6).
- Technical design: recorded in a plan after this spec is approved.

## 2. User Stories

### US-001: A burst below the cache bound is fully cached

AS AN AI coding agent reading diagnostics after a project-wide check
I WANT every file the server published for, up to the cache bound, to be in the cache
SO THAT a file with an error does not read as pending or clean.

**Acceptance criteria:**
```
GIVEN a server that publishes one diagnostic for each of 600 distinct files in one burst
WHEN the burst has been fully received
THEN get_cached_diagnostics returns availability published with the diagnostic for each of the 600 files
  AND no frame was discarded
```

### US-002: An undeliverable publish is not read as "not yet published"

AS AN AI coding agent
I WANT a file whose publish could not be delivered to be reported as lost, not as pending
SO THAT I do not wait for a publish that already happened.

**Acceptance criteria:**
```
GIVEN a burst whose delivery overflows the channel and the loss-handling path is the one that applies
WHEN get_cached_diagnostics, get_diagnostics or the diagnostics resource read targets an affected file
THEN the response does not report availability pending for that file
  AND it carries a structured signal that the push view of this server is incomplete
```

### US-003: Subscribers learn about the late-delivered diagnostics

AS A client subscribed to a file's diagnostics resource
I WANT a `resources/updated` once that file's diagnostics reach the cache
SO THAT I learn about an error from a burst without polling.

**Acceptance criteria:**
```
GIVEN a subscription to the diagnostics resource of a file inside a large burst
WHEN the bridge has cached that file's publish
THEN the subscriber receives a resources/updated notification for it
```

### US-004: An overflow does not flood the log

AS AN operator reading the mcpls log
I WANT a burst overflow to produce a bounded number of lines
SO THAT real warnings are not buried under hundreds of identical ones.

**Acceptance criteria:**
```
GIVEN a burst that overflows the channel by 500 frames
WHEN the log is read
THEN at most one WARN per rate-limit interval names the lane and carries the count dropped since the previous one
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN a server publishes diagnostics for up to `MAX_DIAGNOSTIC_ENTRIES` distinct files in one burst THE SYSTEM SHALL cache the publish for every one of them, regardless of the notification channel capacity | must |
| FR-002 | THE SYSTEM SHALL NOT discard a `textDocument/publishDiagnostics` frame without recording the loss in state that every diagnostics surface can read | must |
| FR-003 | WHEN two publishes for the same URI are pending delivery THE SYSTEM MAY coalesce them to the later one, because a later publish supersedes an earlier one; coalescing SHALL be per owner and per URI and SHALL preserve the order of publishes for different URIs only as far as the cache semantics require | may |
| FR-004 | WHEN a publish cannot be delivered under FR-001 and FR-003 THE SYSTEM SHALL record, per owner (server), that its push view lost frames, and every file of that owner that has no cache entry SHALL read as lost, not `pending` | must |
| FR-005 | THE lost state SHALL be reported through the same structured `availability` field and degradation flags the diagnostics surfaces already use (`get_cached_diagnostics`, `get_diagnostics`, the diagnostics resource read), so the three surfaces agree at the same point in time | must |
| FR-006 | THE lost state SHALL end by the same rules as other degraded push state: it is dropped with the server's diagnostics on respawn or restart, and ends when the server's next accepted write makes the loss moot, with a stated trade-off documented as for the `evicted` overflow in [[bridge/011-push-only-server-diagnostics/spec\|bridge/011]] | must |
| FR-007 | WHEN a dropped-frame loss is recorded THE SYSTEM SHALL log at most one WARN per rate-limit interval per lane, naming the lane and the number of frames dropped since the previous line; individual dropped frames SHALL be logged at DEBUG or not at all | must |
| FR-008 | THE rate-limited WARN SHALL reuse the existing rate-limit mechanism for server-controlled log noise instead of introducing a second one | must |
| FR-009 | WHEN `window/logMessage` or `window/showMessage` frames share the lane with `publishDiagnostics`, THE SYSTEM SHALL NOT let their volume cause a diagnostics publish to be lost: either diagnostics have their own delivery path or the log and message frames are the ones that yield under pressure | must |
| FR-010 | WHEN `window/logMessage` or `window/showMessage` frames are dropped THE SYSTEM SHALL account for them in the same rate-limited log line and SHALL NOT mark any file's diagnostics as lost because of them | must |
| FR-011 | WHEN a message-loop delivery blocks on capacity (if backpressure is the chosen mechanism) THE SYSTEM SHALL keep answering server requests and pending-request responses without deadlock, and SHALL keep honoring client commands and shutdown | must |
| FR-012 | THE total memory held by undelivered publishes SHALL be bounded by a constant independent of the server's behavior, and SHALL NOT exceed the bound that the diagnostics cache applies to the same data | must |
| FR-013 | WHEN a burst from one server saturates delivery THE SYSTEM SHALL NOT starve the diagnostics of another registered server; the per-owner fair-share eviction of the cache (#266, #276, #284) SHALL remain the only cross-server arbiter, and the lost state of one owner SHALL NOT be recorded against another | must |
| FR-014 | WHEN a server is respawned or restarted THE SYSTEM SHALL discard the replaced process's undelivered publishes and its lost state, and SHALL NOT apply a late publish of the replaced process to the replacement's cache | must |
| FR-015 | THE `$/progress` lifecycle lane SHALL be unaffected by diagnostics bursts: a burst on the notification lane SHALL NOT delay or drop `begin`/`end` frames, and the reverse | must |
| FR-016 | A regression test SHALL publish a burst larger than the channel capacity and smaller than the cache bound, and SHALL assert that every file is cached or marked lost, with no `pending` file and at most one WARN per interval | must |
| FR-017 | THE testing playbooks under `.local/testing/` SHALL gain a burst case (flooding fake server, driver, counts) and the coverage status for diagnostics SHALL be reset | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | The lost state is a variant of the existing closed availability and degradation types, not a boolean beside them and not an empty list used as a sentinel, per [[constitution]] |
| NFR-002 | Memory | Delivery buffers are bounded by constants; a hostile server that publishes an unbounded stream cannot grow memory (FR-012). Per-entry size bounds of the cache (`MAX_DIAGNOSTICS_ENTRY_BYTES`) apply to anything buffered |
| NFR-003 | Liveness | The message loop never blocks indefinitely on a full channel; a stalled consumer degrades to recorded loss, not to a hung connection (FR-011) |
| NFR-004 | Observability | An overflow produces at most one WARN per interval per lane; the per-frame detail is at DEBUG |
| NFR-005 | Concurrency | Cache access follows the existing lock discipline (no cache lock across an `await` on an LSP request), per [[bridge/003-rwlock-translator/spec\|bridge/003]] |
| NFR-006 | Compatibility | Any response change is additive and recorded in `CHANGELOG.md`; pre-1.0 compatibility is not a constraint |
| NFR-007 | Throughput | The normal (non-burst) publish path adds no extra hop or copy compared with the current build; steady-state latency is unchanged |

## 5. Data Model

No new persistent entity beyond a per-owner lost marker.

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Notification lane | Bounded channel from the client message loop to the bridge | capacity (256 today), frame kinds carried (diagnostics, log, show message) |
| Lifecycle lane | Separate bounded channel for `$/progress` `begin`/`end` and other frames | capacity (128 today); unaffected |
| Pending publish | Publish accepted by the message loop and not yet in the cache | owner, canonical URI, diagnostics; at most one per (owner, URI) if coalesced |
| Owner lost marker | Per-server record that at least one publish was not delivered | owner, whether set, cleared by the rules of FR-006 |
| Diagnostics availability | Existing per-file answer state | `published`, `pending`, `evicted`, plus the lost outcome decided in Section 9 |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Burst of N <= 256 frames | Unchanged: all delivered |
| Burst of 257 to 1000 distinct files | All cached (FR-001) or, where delivery cannot keep up, files marked lost (FR-004), never `pending` |
| Burst larger than `MAX_DIAGNOSTIC_ENTRIES` | Existing capacity eviction applies and stays surfaced; the lost marker is set only for frames lost before the cache |
| Many publishes for the same file in a burst | Coalescing to the latest is allowed (FR-003); the cache ends with the latest publish for that file |
| A burst of `window/logMessage` frames with few diagnostics | Log frames yield or are rate-limit-accounted; no file is marked lost because of them (FR-009, FR-010) |
| A burst of `window/showMessage` frames | Same as log frames (FR-010) |
| A server publishing diagnostics while also flooding `$/progress` | Progress `report` frames are filtered before any channel; `begin`/`end` use the lifecycle lane and are unaffected (FR-015) |
| Two servers, one in a burst | The other server's entries are untouched; fair-share eviction is the only cross-server effect (FR-013) |
| Server respawned during a burst | Undelivered publishes and the lost marker of the old process are dropped; the replacement starts from `pending` (FR-014) |
| Bridge consumer slow or stalled | Delivery degrades to recorded loss without blocking the message loop (NFR-003) |
| Channel closed (bridge shut down) | The frame is discarded without marking loss; shutdown is not a diagnostics loss |
| Late publish after the lost marker was set | The publish enters the cache normally; the marker ends by FR-006 |
| File closed or deleted while its publish is pending | The cache's existing handling of publishes for unknown or closed documents applies unchanged |
| Subscriber on a file whose publish was coalesced | One `resources/updated` for the final state is sufficient |
| Windows or macOS timing differences | The test must not depend on wall-clock scheduling; it drives the channel deterministically |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Live flood of N = 600 files through the fake stdio server (release binary) | 0 files missing from `get_cached_diagnostics`; before the change 113 were missing |
| SC-002 | Live flood of N = 1100 files | Cache holds `MAX_DIAGNOSTIC_ENTRIES` entries; every file outside the cache reads `evicted` or lost, none reads `pending`; before the change 139 more were dropped |
| SC-003 | Dropped-frame WARN lines for a 500-frame overflow | At most 1 per rate-limit interval, carrying the dropped count; before the change 1 per frame |
| SC-004 | A burst of log and `showMessage` frames mixed with diagnostics | 0 diagnostics files marked lost because of message frames |
| SC-005 | Two registered servers, one flooding | The quiet server's entries unchanged; no cross-owner lost marker |
| SC-006 | Subscription on a file inside a 600-file burst | One `resources/updated` after the file is cached |
| SC-007 | Unit or integration test for FR-016 | Fails against the previous build, passes after |
| SC-008 | Normal-load diagnostics latency and tool outputs for rust-analyzer, pyright, clangd, gopls, typescript-language-server | Identical to the previous build (NFR-007) |

## 8. Agent Boundaries

### Always (without asking)
- Reproduce with the flooding fake server and the driver before changing behavior; keep both as regression fixtures under `.local/testing/`.
- Use the existing rate-limit mechanism for the log line and the existing availability and degradation types for the lost state.
- Keep every buffer bounded and keep the message loop free of unbounded blocking.
- Run the full pre-commit suite (including the fuzz lockfile check if dependencies change) and update `CHANGELOG.md` with the PR link.

### Ask First
- The chosen mechanism: backpressure, per-URI coalescing, a dedicated diagnostics lane, or recorded loss only (Section 9).
- The exact shape of the lost outcome in the response (a new `availability` value, or an existing one plus a flag).
- Changing the channel capacity constants as part of the fix.
- Making log and `showMessage` frames lossy in favor of diagnostics (FR-009), or splitting them to their own lane.
- Any public response shape change beyond an additive field or value.

### Never
- Block the message loop unboundedly on a full channel, or hold a cache lock across an `await` on a channel send.
- Add an unbounded queue as the fix.
- Mark a file as lost because of a dropped log or `showMessage` frame.
- Record a lost marker against a server that did not lose frames.
- Special-case a server by name.
- Silence the loss by lowering the log level without recording it in state.

## 9. Decisions

> [!success] Resolved for #704
> - **Mechanism.** `publishDiagnostics` leaves the 256-slot channel. Each server's message loop writes it into a per-client mailbox (`lsp/publish_mailbox.rs`): one pending publish per file (a later publish replaces an earlier one), bounded to `MAX_DIAGNOSTIC_ENTRIES` files and 64 MiB per server, each entry bounded like the cache bounds one. Log and `showMessage` frames keep the channel and yield under pressure; they never mark a file.
> - **Lost outcome.** A publish the mailbox cannot hold is recorded per file. The pump hands the lost files to the cache (`NotificationCache::record_lost_publishes`), which removes the owner's older entries of those files and marks them lost, so the file reads `evicted` (the existing availability value; no schema change) until the owner publishes it again, and its subscribers get `resources/updated`. Only when more files are lost than the lost list names (`MAX_DIAGNOSTIC_ENTRIES`) does the owner's eviction overflow flag stand in, set after the burst's own writes.
> - **Replacement over the byte cap.** When a replacement for a pending file does not fit, both the pending and the new publish are dropped and the file is marked lost, so it never reads as published with content older than the server's last word.
> - **End of the lost state (FR-006).** A file's mark ends with the owner's next entry for it; the overflow flag ends with the owner's next accepted write (the same trade-off as capacity eviction: a file lost before that write may read `pending` again). Both are dropped with the server's diagnostics on respawn or restart.
> - **Capacity.** The channel capacity stays 256 for log/`showMessage`; the mailbox is as wide as the cache, so it is never the narrower bound.
> - **Respawn (FR-014).** The discard consumer of an automatically respawned server drops the mailbox reader, which discards what is pending and makes the writer stop buffering; a closed mailbox ends the pump.
> - **Known limits.** The 64 MiB mailbox bound is per server, not a shared budget across servers; the mailbox's own warning is limited per server while the channel lanes' warnings are limited per process; bytes count a cheap upper bound of the serialized size, not heap.
> - **Logging (FR-007/008).** Overflows warn through `WarnLimiter`: one line per minute per lane (mailbox, notification channel, lifecycle channel) with the dropped count; each dropped frame is logged at DEBUG.

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] — message loop, lifecycle and respawn
- [[lsp/011-selection-folding-range-tools/spec|lsp/011]] — rate-limited WARN precedent for server-controlled noise (#681, #685)
- [[lsp/012-client-publish-diagnostics-capability/spec|lsp/012]] — makes push-model servers publish, raising the number of publishes that can burst
- [[bridge/009-diagnostics-subscription-staleness/spec|bridge/009]] — subscription notification on cache change, shared bounds and fair-share eviction
- [[bridge/011-push-only-server-diagnostics/spec|bridge/011]] — `availability` states and the `evicted` overflow rules this spec extends
- [[mcp/002-mcp-resources-diagnostics/spec|mcp/002]] — diagnostics resources and `resources/updated` delivery
- [[runtime/004-server-text-hygiene/spec|runtime/004]] — server-controlled text reaching the cache and logs
- Code: `crates/mcpls-core/src/lsp/client.rs` (`message_loop_inner`, `notification_lane`), `crates/mcpls-core/src/lsp/lifecycle.rs` (`NOTIFICATION_CHANNEL_CAPACITY`, `LIFECYCLE_CHANNEL_CAPACITY`), `crates/mcpls-core/src/bridge/notifications.rs` (diagnostics cache, `MAX_DIAGNOSTIC_ENTRIES`)
