---
aliases:
  - Dropped publishDiagnostics frame
  - Stale diagnostics after undecodable publish
tags:
  - sdd
  - spec
  - bug
  - lsp-bridge
  - diagnostics
  - framing
created: 2026-11-01
status: approved
related:
  - "[[constitution]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001-lsp-server-lifecycle-and-respawn]]"
  - "[[lsp/011-selection-folding-range-tools/spec|lsp/011-selection-folding-range-tools]]"
  - "[[lsp/013-diagnostics-publish-burst-drops/spec|lsp/013-diagnostics-publish-burst-drops]]"
  - "[[bridge/009-diagnostics-subscription-staleness/spec|bridge/009-diagnostics-subscription-staleness]]"
  - "[[bridge/011-push-only-server-diagnostics/spec|bridge/011-push-only-server-diagnostics]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp/002-mcp-resources-diagnostics]]"
---

# Feature: An oversized or undecodable `publishDiagnostics` frame is not silently dropped while stale diagnostics read as current

> [!info] Metadata
> **Type**: bug
> **Priority**: P2
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle 047, live-test finding
> **Issue**: #863

> [!abstract]
> Since #859 an LSP frame between 10 MiB and 256 MiB no longer closes the server connection.
> A notification of that size, and any undecodable notification, is discarded by the client
> message loop with no effect on the diagnostics cache. When the discarded frame is a
> `textDocument/publishDiagnostics`, the previous cache entry for the file stays in place and is
> still reported as `availability: published` with `push_notifications_degraded: false`, so an
> agent reads stale errors (or a stale clean result) as the current state and no
> `resources/updated` is sent. This spec requires that a lost publish is recorded and surfaced
> through the existing availability and degradation fields, consistently on all three
> diagnostics surfaces.

## 1. Overview

### Problem Statement

`crates/mcpls-core/src/lsp/transport.rs` (`skip_content`, `oversized_message`) turns a frame over
the 10 MiB buffering limit into `InboundMessage::UndecodableNotification` when the frame is a
notification, instead of closing the connection. The client message loop
(`crates/mcpls-core/src/lsp/client.rs`) handles
`InboundMessage::UndecodableNotification | InboundMessage::UndecodableFrame` with an empty arm:
the frame is counted for a rate-limited WARN and nothing else happens.

Before #859 the same frame was fatal: the connection closed, the server was respawned and the
cache honestly read `availability: pending` with `push_notifications_degraded: true`. After #859
the previous entry for the file survives untouched. The same silent loss already existed for a
small malformed (undecodable, under 10 MiB) `publishDiagnostics` frame; #859 widens it to any
publish over 10 MiB, which a file with tens of thousands of diagnostics produces.

The consequences are the silent-incompleteness class settled as a correctness issue in
[[lsp/013-diagnostics-publish-burst-drops/spec|lsp/013]],
[[bridge/009-diagnostics-subscription-staleness/spec|bridge/009]] and
[[bridge/011-push-only-server-diagnostics/spec|bridge/011]]:

- The agent reads the previous diagnostics (for example "first error" after the error was fixed,
  or a clean result after new errors appeared) as current.
- A server publishes a file again only when the file changes, so the stale state can last for the
  rest of the session.
- No `resources/updated` is sent, so a subscriber never learns that the file's state changed.
- `push_notifications_degraded` stays false, so no structured signal marks the loss. Only two
  rate-limited WARN lines exist.

**Reproduced live (release binary built from `f13ba47`).** Scratch stdio server with `--pull none`
and `--bigpub 11534336` (one fake server for `**/*.fk`):

| Step | Observation on `f13ba47` | Observation on `a0dcedb` (before #859) |
|------|--------------------------|----------------------------------------|
| Open `bp.fk` (hover); server publishes one error "first error" | `get_diagnostics`: `published`, ["first error"], `push_notifications_degraded: false` | same |
| Edit the file on disk, hover again; server answers with an 11 MiB valid-JSON `publishDiagnostics` frame | `get_diagnostics`: still `published`, ["first error"], `push_notifications_degraded: false`, `origin: push_cache`; WARN logs "Skipped an oversized LSP frame of N bytes" and "Dropped an undecodable LSP message" only | `pending`, [], `push_notifications_degraded: true` (connection torn down and respawned) |
| Small `publishDiagnostics` with a JSON syntax error | stale entry kept, no flag | stale entry kept, no flag |

### Goal

When a `textDocument/publishDiagnostics` frame cannot be decoded or is too large to buffer, the
loss is recorded for its server (and for its file when identifiable) and surfaced through the
existing structured fields, so a stale entry is never presented as the current state.

### Out of Scope

- Raising the 10 MiB buffering limit or buffering and decoding oversized frames: the limit
  protects memory ([[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]]).
- Restoring the pre-#859 behavior of closing the connection for a 10 MiB to 256 MiB notification.
- Non-diagnostics notifications (`window/logMessage`, `window/showMessage`, `$/progress`): their
  loss is not a state-correctness issue and stays rate-limited log noise.
- Undecodable responses and server requests: handled by their own per-request error paths.
- The publish burst and channel overflow path (settled in
  [[lsp/013-diagnostics-publish-burst-drops/spec|lsp/013]]) and capacity eviction.
- Technical design: recorded in a plan after this spec is approved.

## 2. User Stories

### US-001: A lost publish does not leave stale diagnostics reading as current

AS AN AI coding agent reading diagnostics after an edit
I WANT a file whose latest publish was lost to read as degraded, not as the previous result
SO THAT I do not act on errors that were fixed or miss errors that appeared.

**Acceptance criteria:**
```
GIVEN a cached publish for bp.fk with the diagnostic "first error"
  AND the server then sends an 11 MiB valid publishDiagnostics frame for bp.fk
WHEN get_diagnostics, get_cached_diagnostics or the diagnostics resource read targets bp.fk
THEN the response does not present "first error" as the current state with push_notifications_degraded false
  AND it carries the structured loss signal decided in Section 9
```

### US-002: A small malformed publish is treated the same way

AS AN AI coding agent
I WANT an undecodable `publishDiagnostics` under the size limit to be surfaced like an oversized one
SO THAT the loss signal does not depend on how large the broken frame was.

**Acceptance criteria:**
```
GIVEN a cached publish for a file
  AND the server sends a publishDiagnostics frame with a JSON syntax error
WHEN a diagnostics surface is read
THEN the response carries the same loss signal as for an oversized frame
```

### US-003: Subscribers learn that the state changed

AS A client subscribed to a file's diagnostics resource
I WANT a `resources/updated` when a publish for it was lost
SO THAT I re-read the resource and see the degraded state.

**Acceptance criteria:**
```
GIVEN a subscription to the diagnostics resource of a file
WHEN a publishDiagnostics frame for it is dropped as oversized or undecodable
THEN the subscriber receives a resources/updated notification when the merged view changed
```

### US-004: The three surfaces agree

AS AN AI coding agent
I WANT `get_diagnostics`, `get_cached_diagnostics` and the resource read to report the loss alike
SO THAT the answer does not depend on which surface I use.

**Acceptance criteria:**
```
GIVEN a recorded loss for a server
WHEN each of the three surfaces is read at the same point in time
THEN availability and the degradation flag are identical on all three
```

### US-005: Dropped frames do not flood the log

AS AN operator reading the mcpls log
I WANT log output for dropped frames to stay rate limited
SO THAT a server repeating the failure cannot bury other warnings.

**Acceptance criteria:**
```
GIVEN a server that repeatedly sends oversized publishDiagnostics frames
WHEN the log is read
THEN each source of the loss logs at most one WARN per rate-limit interval, with a count
  AND an oversized publish may log up to three lines in one interval, one from each source
      (the transport's skipped-frame line, the undecodable-run line, the lost-publish line)
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN a `textDocument/publishDiagnostics` frame is dropped because it exceeds the buffering limit or cannot be decoded THE SYSTEM SHALL record the loss for the sending server in state readable by every diagnostics surface | must |
| FR-002 | WHEN the dropped frame's `params.uri` can be identified THE SYSTEM SHALL additionally record the loss for that file and SHALL NOT serve that file's previous entry as current | should |
| FR-003 | WHEN the dropped frame's `params.uri` cannot be identified THE SYSTEM SHALL record the loss per server and every cached file of that server SHALL be reported as possibly stale rather than as current. This covers every unidentified loss: the server's pushed entries are evicted as lost, its capacity-eviction `clean` marks become `lost` (so a file evicted while clean no longer reads published-clean), and its overflow mark is set | must |
| FR-004 | THE loss SHALL be reported through the existing structured `availability` field as `evicted`, through the lsp/013 pipeline, and `push_notifications_degraded` SHALL stay untouched (a dropped frame does not stop push notifications); no new free-form field is added | must |
| FR-005 | THE `get_diagnostics`, `get_cached_diagnostics` and diagnostics resource read SHALL report the same availability and degradation state at the same point in time | must |
| FR-006 | THE loss state SHALL end by the rules of [[lsp/013-diagnostics-publish-burst-drops/spec|lsp/013]]: a file's mark ends with its next accepted publish, the server's overflow mark ends at the server's next accepted write (a pull included), and both are cleared on server respawn or restart together with the server's diagnostics. Trade-off: a file with no cached entry flips from `pending` to `evicted` without a `resources/updated`, since notifying subscribers of files with no entry would need the router in the pump. A delivery queued in the old mailbox of a respawned server never evicts the replacement's entries; the abort that guarantees this is best effort, so one queued delivery can in rare timing apply once, and it ends at the replacement's first write. Known trade-off: when a mailbox overflow (lsp/013) is followed by an unidentified loss, the merged loss is delivered before newer pending publishes, whose writes then end the overflow; only files with no cached entry are affected (they read `pending` instead of `evicted`), never a stale entry | must |
| FR-007 | WHEN a loss is recorded and the merged view of a subscribed file changed THE SYSTEM SHALL send `resources/updated` for it | must |
| FR-008 | THE dropped-frame log lines SHALL stay rate limited through the existing mechanism, with one WARN per source per interval carrying a dropped count (the transport's skipped-frame line, the undecodable-run line and the lost-publish line), and SHALL NOT include server-controlled frame content | must |
| FR-009 | WHEN a dropped notification is not a `publishDiagnostics` frame THE SYSTEM SHALL NOT record a diagnostics loss | must |
| FR-010 | WHEN a loss is recorded for one server THE SYSTEM SHALL NOT mark another server's diagnostics, and the bound on capacity-eviction marks and their overflow mark SHALL apply per server, so one server's marks never displace another's (a file's mark belongs to the server that recorded it last) | must |
| FR-011 | A later valid `publishDiagnostics` for an affected file SHALL replace the entry normally and clear the file-level loss | must |
| FR-012 | THE identification of a dropped frame as a diagnostics publish SHALL NOT require buffering the full oversized frame, and memory per frame SHALL stay within the existing limits | must |
| FR-013 | Regression tests SHALL cover an oversized and a small malformed publish after an accepted publish, asserting the loss signal on all three surfaces and the subscriber notification | must |
| FR-015 | WHEN a `publishDiagnostics` frame is valid JSON but its params fail the typed decode THE SYSTEM SHALL record the loss as for an undecodable frame: for the file when `params.uri` is a string, else unidentified | must |
| FR-016 | WHEN a `publishDiagnostics` has no `params`, or `params` is not an object, THE SYSTEM SHALL record an unidentified loss, as for every unreadable publish | must |
| FR-017 | WHEN the list of lost files is full and another named loss arrives THE SYSTEM SHALL escalate to an unidentified loss (every pushed entry of the server is evicted, and its capacity-eviction `clean` marks become `lost`, so no file keeps reading published-clean) instead of only setting the overflow mark | must |
| FR-018 | EVERY source of a lost publish (oversized, malformed, typed-decode failure) SHALL reach the mailbox through one entry point that owns the lost-publish WARN, which carries no URI and no frame content | must |
| FR-019 | A `uri` longer than 16 KiB SHALL be treated as unidentified, whether it names a frame that could not be decoded, a decoded publish, or a publish the mailbox could not hold; no URI longer than 16 KiB is cached or remembered as lost | must |
| FR-014 | THE testing playbooks under `.local/testing/` SHALL gain the oversized-publish case (fake server `--bigpub`, driver) and the coverage status for diagnostics SHALL be reset | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | The loss is a variant of the existing closed availability and degradation types, not a new boolean beside them and not an empty list used as a sentinel, per [[constitution]] |
| NFR-002 | Memory | No oversized frame is buffered to classify it; scanner state per frame stays constant (FR-012) |
| NFR-003 | Observability | Log output for repeated drops is bounded and carries no server-controlled content (FR-008) |
| NFR-004 | Compatibility | Any response change is additive and recorded in `CHANGELOG.md`; pre-1.0 compatibility is not a constraint |
| NFR-005 | Throughput | The valid-frame path adds no extra copy or hop; tool output for well-formed publishes is identical |
| NFR-006 | Concurrency | Cache access follows the existing lock discipline (no cache lock across an `await` on an LSP request) |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Dropped notification | Notification the transport could not deliver as a typed message | reason (oversized or malformed), size, method if identifiable, uri if identifiable |
| Server loss marker | Per-server record that a publish was lost: the overflow mark, plus eviction of every pushed entry for an unidentified loss | owner, set or clear, cleared by the rules of FR-006 |
| File loss marker | Per-file record of a lost publish | owner, canonical URI, cleared by a later valid publish |
| Diagnostics availability | Existing per-file answer state | `published`, `pending`, `evicted`; a lost publish reads `evicted` |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Oversized publish, no prior entry for the file | The file does not read `pending` as if nothing was published; it reads as lost or degraded (FR-001, FR-002) |
| Oversized publish with a prior entry | The prior entry is not served as current (FR-002, FR-003) |
| Oversized publish that would have been a clean (empty) result | Stale errors are not served as current; the agent sees the degradation, not the old errors |
| Frame method cannot be determined (truncated or garbage) | Treated as an unknown notification: no diagnostics loss is recorded (FR-009); only the rate-limited log applies |
| `uri` appears after the large `diagnostics` array in the frame | Resolved: the whole frame is scanned in constant memory, so member order does not matter |
| A key written with escapes (`"\u006dethod"`, `"\u0075ri"`) | The scanner never matches it, while a decoded value does. An oversized publish whose `method` key is escaped records no loss (pinned limitation); an escaped `uri` key leaves the file unidentified |
| `uri` or `params` repeated, or `uri` not a string | Unusable: the loss is unidentified |
| `uri` longer than 16 KiB | Unidentified (FR-019) |
| Several oversized publishes for different files | Each recorded; the per-server marker covers all |
| Same file, valid publish after the loss | Entry replaced, file-level loss cleared (FR-011) |
| Server sends only oversized publishes repeatedly | State stays degraded; log stays rate limited (FR-008) |
| Server respawned or restarted | Loss state dropped with the old diagnostics (FR-006) |
| Two servers, one dropping | The other is untouched (FR-010) |
| Pull-diagnostics server | Pull results are stored as separate `Pulled` slots; a pull result is not marked by a lost push, and the merged view follows the existing rules of [[bridge/011-push-only-server-diagnostics/spec|bridge/011]] |
| Oversized frame is a request or response | Out of scope; handled by existing per-request error paths |
| Windows and macOS | Tests must not depend on wall-clock timing or on a real 11 MiB frame beyond what the fixture needs |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Live repro (fake server `--bigpub 11534336`, `--pull none`), step 4 on all three surfaces | Not `published` with ["first error"] and `push_notifications_degraded: false`; carries the loss signal |
| SC-002 | Live repro with a small malformed publish | Same loss signal as SC-001 |
| SC-003 | Subscription on the file during the repro | One `resources/updated` when the merged view changed |
| SC-004 | Surfaces compared at the same time | Identical availability and degradation state |
| SC-005 | Repeated oversized publishes | At most one WARN per source per interval with a count (up to three lines for an oversized publish) |
| SC-006 | Non-diagnostics oversized notification | No diagnostics loss recorded |
| SC-007 | Regression test (FR-013) | Fails on `f13ba47`, passes after |
| SC-008 | Valid publishes from rust-analyzer, pyright, clangd, gopls, typescript-language-server | Output unchanged (NFR-005) |

## 8. Agent Boundaries

### Always (without asking)
- Reproduce with the fake server and driver (`--bigpub`, `t_bigpub2.py`) before and after the change; keep them under `.local/testing/`.
- Reuse the existing availability and degradation types and the existing rate-limit mechanism.
- Run the full pre-commit suite and update `CHANGELOG.md` with the PR link.

### Ask First
- The granularity of the loss marker and the shape of the outcome in responses (Section 9).
- Whether the transport scanner may extract the `uri` of an oversized frame.
- Any public response shape change beyond an additive field or value.

### Never
- Buffer an oversized frame in full to classify it.
- Restore connection teardown for oversized notifications as the fix.
- Record a diagnostics loss for a non-diagnostics notification or against another server.
- Silence the loss by lowering the log level without recording it in state.
- Include server-controlled frame content in log lines.

## 9. Decisions

- Granularity: per file when `params.uri` is recovered, else per server (an unidentified loss evicts every pushed entry of the server).
- Outcome: `availability: evicted` through the lsp/013 pipeline; `push_notifications_degraded` is untouched.
- End of the loss: as lsp/013 (FR-006).
- Identification: the scanner reads `method` and `params.uri` in one pass over the frame in constant memory, in any member order (FR-012).
- A typed-decode failure of valid JSON is a loss too (FR-015).
- Frames whose method is not `publishDiagnostics`, including `UndecodableFrame`, record no diagnostics loss (FR-009).
- Issue: #863. The drop count is not exposed on another surface.

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] — message loop, lifecycle and respawn
- [[lsp/011-selection-folding-range-tools/spec|lsp/011]] — rate-limited WARN precedent for undecodable frames
- [[lsp/013-diagnostics-publish-burst-drops/spec|lsp/013]] — settled policy: a lost publish is recorded and surfaced
- [[bridge/009-diagnostics-subscription-staleness/spec|bridge/009]] — subscription notification on cache change
- [[bridge/011-push-only-server-diagnostics/spec|bridge/011]] — availability states and degradation rules
- [[mcp/002-mcp-resources-diagnostics/spec|mcp/002]] — diagnostics resources and `resources/updated`
- Code: `crates/mcpls-core/src/lsp/transport.rs` (`skip_content`, `oversized_message`), `crates/mcpls-core/src/lsp/client.rs` (`InboundMessage::UndecodableNotification`), `crates/mcpls-core/src/json_envelope.rs`, `crates/mcpls-core/src/bridge/notifications.rs`
