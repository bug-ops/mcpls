---
aliases:
  - Diagnostics subscription staleness
  - One-shot lsp-diagnostics subscriptions
tags:
  - sdd
  - spec
  - enhancement
  - bridge
  - diagnostics
  - subscriptions
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[MOC-specs]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp-resources-diagnostics]]"
  - "[[bridge/002-document-tracker-synchronization/spec|document-tracker-synchronization]]"
  - "[[bridge/004-get-diagnostics-flycheck-gap/spec|get-diagnostics-flycheck-gap]]"
---

# Feature: Notify `lsp-diagnostics://` Subscribers When a Pull Changes a Tracked File's Diagnostics

> [!info] Metadata
> **Author**: k05h31
> **Type**: enhancement
> **Priority**: P3
> **Related issues**: #574; feature under repair: #115 (resource subscriptions, [[mcp/002-mcp-resources-diagnostics/spec|mcp/002]]); follow-ups #648 (no-tool-call trigger), #649 (notify on push-triggered eviction)
> **Reproduced on**: release build, HEAD ad90190, stdio and HTTP (three sessions), rust-analyzer, tiny cargo project

## 1. Overview

### Problem Statement

`resources/subscribe` and `subscriptions/listen` on `lsp-diagnostics://<file>` fire only from one
source: a `textDocument/publishDiagnostics` push that the diagnostics pump
(`diagnostics_pump`, `crates/mcpls-core/src/lib.rs`) stores in `NotificationCache` and then offers
to each subscribed session. Nothing else in mcpls produces a notification. Three facts make that
source dry for a server such as rust-analyzer once the document has been published once:

1. **mcpls never tells the server a file was saved.** `textDocument/didSave` is never sent
   (no occurrence in `crates/`). rust-analyzer runs its flycheck (`cargo check`) on `didSave`, and
   does not re-push a changed document's diagnostics after the first publishes.
2. **mcpls does not watch the disk.** The older `file_watcher` is gone. A tracked file is
   re-read only lazily, when a tool call reaches `DocumentTracker::ensure_open`
   (disk-staleness check, [[bridge/002-document-tracker-synchronization/spec|bridge/002]]), which
   sends `didChange`. With no tool call, the server is not told the file changed.
3. **A pull result is a dead end.** `Translator::handle_diagnostics`
   (`bridge/translator/diagnostics.rs`) merges the `textDocument/diagnostic` response with the
   cache, returns it to the caller, and drops it: it is not written to `NotificationCache` and
   not published to subscribers. rust-analyzer supports pull diagnostics, so after a resync the
   server answers the pull with the new error while pushing nothing.

Observed (issue reproduction): after the initial replay plus first publish (2
`notifications/resources/updated`), appending `fn zz() { let q: i32 = "s"; }` to `main.rs` and
calling `get_diagnostics` returns `E0308 expected i32, found &'static str`, so mcpls resynced the
file and rust-analyzer analysed it. Yet no `resources/updated` arrives within 4 s,
`get_cached_diagnostics` does not contain E0308, and reverting the file and pulling again also
notifies nobody. Over HTTP with sessions A and B on `main.rs` and C on `util.rs`: zero
notifications for any of them. The debug log shows only the two initial `publishDiagnostics`,
then `didChange` and `textDocument/diagnostic` with no further push.

Effect: for a pull-capable server the subscription feature is one-shot. A subscriber (an agent
waiting to learn whether its edit broke the build) is never told, and the two diagnostic read
paths disagree: `get_diagnostics` has the fresh error, `get_cached_diagnostics` and
`resources/read` do not. [[bridge/004-get-diagnostics-flycheck-gap/spec|bridge/004]] fixed the
opposite direction (push-sourced flycheck diagnostics missing from the pull); this spec is the
mirror gap (pull-sourced diagnostics missing from the cache and the subscription channel).

### Goal

A subscriber to `lsp-diagnostics://<file>` is told, without a notification storm, when a tool
call that pulled the file observed that the diagnostics a read of it returns have changed, for
servers that deliver diagnostics by pull as well as by push, and the cache and the pull path
report the same state.

> [!success] Decision
> Option A only (section 3): pull results are written to the cache and published. The trigger is
> a tool call that pulled (`get_diagnostics`); mcpls sends no `didSave`, watches no files, polls
> nothing and does not act on `workspace/diagnostic/refresh`. An on-disk edit that no tool call
> observes still notifies nobody; that remainder is #648. Push behavior is unchanged (FR-007).

### Out of Scope

- Changing the push path for servers that already push after every change (pyright, gopls, etc.).
  Their behavior must not regress; they are the baseline for FR-007.
- Speculative or in-memory edits that never touch the disk
  ([[mcp/009-speculative-edit-diagnostic-preview/spec|mcp/009]]).
- Stateless-HTTP subscriptions (rejected by #482, [[mcp/003-mcp-2026-stateless-adoption/spec|mcp/003]]).
- Workspace-wide diagnostics (`workspace/diagnostic`) and subscriptions to files that are not
  tracked (`DocumentTracker`) by any session's activity.
- Retrying or extending the `ContentModified` handling ([[lsp/005-lsp-content-modified-retry/spec|lsp/005]]).
- A trigger that needs no tool call (`didSave`, a file watcher, re-pulling subscribed files): #648.
- Notification payload changes: `notifications/resources/updated` stays URI-only; the client
  re-reads via `resources/read`.

## 2. User Stories

### US-001: Learn that my edit broke the build

AS A coding agent subscribed to `lsp-diagnostics://<file>`
I WANT a `resources/updated` after a pull observes that the file's diagnostics changed
SO THAT I re-read diagnostics only when something changed, instead of polling `get_diagnostics`.

**Acceptance criteria:**
```
GIVEN a session subscribed to a tracked rust-analyzer file whose initial publishes have settled
WHEN the file is edited on disk so that it gains a new error, and a tool call pulls its diagnostics
THEN the session receives a resources/updated for that URI before the pull returns (FR-008)
AND a following resources/read and get_cached_diagnostics both contain the new error
```

### US-002: Hear about the fix, not only the breakage

AS A coding agent
I WANT a notification when an error disappears after I revert or repair the file
SO THAT I know the file is clean without a speculative pull.

**Acceptance criteria:**
```
GIVEN the state after US-001 (error present, subscriber notified)
WHEN the file is reverted on disk and a tool call pulls its diagnostics
THEN the subscriber receives a resources/updated and a read returns no E0308
```

### US-003: Another tool call refreshes my subscription

AS A client with two MCP sessions (A calls tools, B only subscribes)
I WANT B to be notified when A's `get_diagnostics` observes new diagnostics
SO THAT sessions sharing one workspace do not each need to poll.

**Acceptance criteria:**
```
GIVEN sessions A and B, B subscribed to main.rs, A not subscribed
WHEN A calls get_diagnostics on main.rs and the result differs from the cached state
THEN B receives exactly one resources/updated for main.rs and A receives none
AND a session C subscribed only to util.rs receives nothing
```

### US-004: Quiet when nothing changed

AS A subscriber
I WANT no notification when a resync or pull produced identical diagnostics
SO THAT the stream carries signal only.

**Acceptance criteria:**
```
GIVEN a subscribed file with settled diagnostics
WHEN the same file is pulled N times with no content change
THEN the subscriber receives no resources/updated from those pulls
```

## 3. Design Options

The triggers in the issue are not mutually exclusive. Each option is scored on what it fixes.

| Option | What it does | Fixes | Does not fix | Cost / risk | Status |
|--------|--------------|-------|--------------|-------------|--------|
| **A. Publish pull results** | After `handle_diagnostics` gets a pull report, write it to `NotificationCache` as a pull-sourced entry and publish to subscribers if the merged view changed | cache/pull disagreement; subscriber notified when any session pulls (US-003); no new LSP traffic | an edit nobody pulls after (no trigger without a tool call); flycheck/push-only diagnostics still depend on the server | A pull/push source split in the cache; `Translator` reaches the session registry through `NotificationWiring`; version-race handling | shipped (#574) |
| **B. Send `didSave` after resync** | After `didChange` for a content-changed file, send `textDocument/didSave` so the server runs flycheck and pushes | server-driven push (flycheck included) with no cache semantics change | still needs a tool call to trigger the resync; servers not advertising save support | Heavy side effect on the user's workspace (`cargo check` per save), must respect `TextDocumentSyncOptions.save` and `includeText` | #648 |
| **C. Watch tracked files** | A file watcher over tracked (open) documents resyncs on change and then triggers B and/or A | the only option that works with no tool call at all | none of the above is free | New dependency, event storms, debounce, cross-platform parity; the previous watcher was removed | #648 |
| **D. React to `workspace/diagnostic/refresh`** | Treat the request as "re-pull the tracked files that have subscribers" | servers that refresh by request | servers that never send it | Needs an accurate `refreshSupport` capability; re-pull fan-out can storm | not shipped |
| **E. Subscription-driven re-pull** | While a file has subscribers, mcpls pulls it periodically or on a resync trigger and publishes on change | works with no watcher; bounded by subscriber count | latency equals the poll period | Timer per subscribed file, cap interplay with FR-012 | #648 |

> [!success] Decisions
> - **Option set**: A only. The trigger is a tool call that pulled (US-001, US-003); an edit that no tool call observes is #648.
> - **FR-002 and pushes**: change detection applies to pull writes only. Every accepted push still notifies as before (FR-007).
> - **Freshness bound**: the notification is enqueued before the pull's tool call returns (FR-008).

## 4. Functional Requirements

The EARS requirements below hold for any chosen option. Option-specific requirements are marked.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN the diagnostics a read of a tracked file would return (the merged view `get_cached_diagnostics` and `resources/read` expose) change for any reason other than a `didClose` or server respawn, THE SYSTEM SHALL publish `resources/updated` for that file's URI to every session subscribed to it | must |
| FR-002 | THE SYSTEM SHALL decide "changed" for a pull write by comparing the merged view before and after the write, so a write that leaves the merged view identical (same diagnostics, same order-insensitive set) SHALL NOT notify | must |
| FR-003 | WHEN `get_diagnostics` obtains a full pull report for a file, THE SYSTEM SHALL make that report visible to `get_cached_diagnostics` and `resources/read` for the file, so the three read paths agree at the same point in time | must |
| FR-004 | WHEN a pull report is stored, THE SYSTEM SHALL keep it as a source distinct from pushed diagnostics, so a push does not erase pull-sourced diagnostics and a pull does not erase flycheck-sourced pushed diagnostics, and a read returns their union. Pulled diagnostics come first and are never collapsed among themselves; a pushed diagnostic is dropped when it is the same problem as a pulled one (both carry a `code`, equal severity and code, and the ranges overlap or start within 3 lines, per [[bridge/004-get-diagnostics-flycheck-gap/spec\|bridge/004]]; otherwise equal range, severity, message and code), or an exact duplicate of an earlier pushed one. Severity (none reads as information) and code (an integer reads as its decimal string) are compared as `get_diagnostics` reports them. The union is ordered by a stable sort on range. | must |
| FR-005 | WHEN a pull report arrives after the document was resynced to another version while the request was in flight, after a pull issued later already stored its report, or after the server's diagnostics were cleared (respawn), THE SYSTEM SHALL discard it for the cache, SHALL NOT publish, and SHALL still return it to the caller. The version comparison is best effort: the tracker is read under its own lock and a resync marks the new version after `didChange` is enqueued | must |
| FR-006 | WHEN a pull request fails (timeout, `-32601`, `ContentModified`) or answers with an `unchanged` or partial report, THE SYSTEM SHALL leave the cache and subscribers unchanged | must |
| FR-007 | WHILE a server pushes after every change, THE SYSTEM SHALL behave as before for that server: one `resources/updated` per accepted publish, including re-publishes with identical content. FR-002 applies to pull writes only; a push is never suppressed and never changes which URIs are notified for the pushed file itself; an eviction the push causes adds one notification for the evicted file (FR-013) | must |
| FR-008 | WHEN a pull changes the merged view of a subscribed file, THE SYSTEM SHALL enqueue the notification before the pull's tool call returns | must |
| FR-009 | Option B (`didSave` after resync) | not shipped, #648 |
| FR-010 | Option C (file watcher) | not shipped, #648 |
| FR-011 | Option D (`workspace/diagnostic/refresh`) | not shipped |
| FR-012 | THE SYSTEM SHALL store pull-sourced entries under the same global budgets as pushed ones (`MAX_DIAGNOSTIC_ENTRIES`, `MAX_DIAGNOSTICS_ENTRY_BYTES`, per-owner fair-share eviction, #266, #276, #284). The pulled slot (one per file, canonical URI) is not counted toward `MAX_SOURCES_PER_FILE`, so a file holds at most 8 pushed slots plus one pulled slot; a pull write never evicts a slot of the file it writes | must |
| FR-013 | WHEN a pull or push write evicts the entry of another file, THE SYSTEM SHALL publish `resources/updated` for the evicted file so its subscribers re-read (#649). The write returns the evicted files and the caller publishes after the cache lock is released | should |
| FR-014 | THE SYSTEM SHALL deliver every publication through the existing per-session `Delivery` (coalescing pending-URI set plus capacity-1 doorbell), never awaiting a peer from the publishing task | must |
| FR-015 | THE SYSTEM SHALL publish only to sessions that subscribed to the exact file URI (or a `subscriptions/listen` stream that listed it); a pull, resync or `didSave` caused by session A SHALL NOT notify, expose or delay any other session beyond the notification FR-001 prescribes for that session's own subscriptions | must |
| FR-016 | WHEN the routed diagnostics server for a file has failed to start, is initializing, or has been marked `push_degraded` and no pull is possible, THE SYSTEM SHALL keep the existing mcp/002 FR-009..FR-012 behavior and SHALL NOT fabricate notifications | must |

## 5. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Notification volume | A single content change to one file yields at most one `resources/updated` per subscribed session from this feature, regardless of how many pulls, resyncs, `didSave`s or publishes the change triggers (coalescing by FR-002 plus `Delivery`'s pending set). Pulling an unchanged file 100 times yields zero. |
| NFR-002 | Notification storm bound | A bulk change (e.g. `git checkout` touching 1,000 files with 100 tracked) produces at most one notification per subscribed tracked URI and at most `MAX_SUBSCRIPTIONS` (1,000) per session; trigger work (resyncs, pulls, `didSave`s) is bounded by the tracked-document limit, not by the event count. |
| NFR-003 | Per-session isolation | Notification delivery state is per session (mcp/002 FR-006). A stalled or slow session cannot delay or lose another's update; a session that did not subscribe never sees a URI. Sessions share one translator, notification cache and set of workspace roots, so a pulled report is readable by every session through the cache reads (FR-003); only the notification is URI-only and limited to the files a session subscribed to. |
| NFR-004 | Cache budget | Pull-sourced entries never raise the cache above its existing global bounds (1,000 entries, 1 MiB per entry, 8 sources per file). A pull-only server (one that never pushes) must not be able to evict another server's entries beyond its fair share. |
| NFR-005 | Lock discipline | Cache writes follow the pump's rule: the cache lock is never held across an LSP round trip, and a pull-sourced write takes it only for the store (as `handle_diagnostics` already does for its read). |
| NFR-006 | LSP load | The feature adds no LSP traffic: a pull is the caller's own request. (Options B and D, not shipped, would add at most one `didSave` / one re-pull per tracked file per content change.) |
| NFR-007 | Cost containment | Triggers that can start heavy server work (flycheck via `didSave`) are skipped when the file has no subscriber, unless the open question in FR-009 is resolved otherwise. |
| NFR-008 | Type safety | Pull-versus-push provenance is a closed typed enum on the cache entry, not a string or boolean flag; the changed/unchanged decision returns a typed outcome the pump and the tool path consume exhaustively. |
| NFR-009 | Compatibility | `notifications/resources/updated` payload, `resources/read` schema, `get_cached_diagnostics` and `get_diagnostics` tool schemas are unchanged (no `tool_surface.json` change). |
| NFR-010 | Merge cost | Merging a file's sources costs a lookup per pushed diagnostic (comparison grouped by normalized `(severity, code)`), not a scan of the pulled ones; a file at the entry cap merges in well under a second, since every read path runs the merge on a runtime worker |

## 6. Data Model

| Entity | Description | Key attributes |
|--------|-------------|----------------|
| Diagnostics entry | Cached diagnostics for one published URI, owned by one server | URI key, owner `ServerId`, document `version`, diagnostics list, order sequence |
| Diagnostics source | Provenance of an entry's content: pushed by the server or obtained by pull | closed enum `{Pushed, Pulled}`; `source` tags are server-specific and not modelled |
| Merged view | Union of one file's sources across its canonical and alias URIs (`DiagnosticSources::merge`), what every read returns | list, count, canonical file key |
| Change outcome | Result of a cache write | `Changed` / `Unchanged` (typed, drives publish) |
| Subscription | Per-session set of subscribed URIs (cap `MAX_SUBSCRIPTIONS`) with a coalescing `Delivery` | session id, URI set, pending set |
| Trigger (option-dependent) | What produced the write | pull observation, `didSave`-induced push, watcher resync, refresh re-pull |

## 7. Edge Cases and Error Handling

| Scenario | Expected behavior |
|----------|-------------------|
| File edited while a pull is in flight | The late pull report carries an older version than the tracker's; it is discarded (FR-005); the newer resync's own pull/push publishes |
| Pull returns `unchanged` (`RelatedUnchangedDocumentDiagnosticReport`) | Not stored and not published; the result is the cache view. `previous_result_id` stays `None`, so a server answers full reports, which is what change detection needs |
| Pull returns partial result (`DocumentDiagnosticReportPartialResult`) | Not stored, not published (current code treats it as empty) |
| Pull and push both report the same error | One entry in the merged view per FR-004's dedupe rule; no double notification |
| Pull says clean but flycheck-sourced push diagnostics exist | The merged view still contains the pushed warnings (the bridge/004 guarantee holds); only the pull-sourced slot is replaced |
| Reverting the file removes the only error | Merged view changes from one item to none: notify (US-002); the empty entry follows the existing empty-entry eviction preference (#284) |
| Server not pull-capable (push-only, `-32601`) | No pull-sourced write; behavior identical to today (FR-006, FR-007) |
| Server `push_degraded` (respawned, pump dark) | The cache cannot be fresh by push; a pull-sourced write is the only freshness path and is the case most helped by this feature. Pull writes proceed as usual and notifications carry no flag; the read-side `push_notifications_degraded` flag is unchanged |
| File outside the workspace or symlink alias | Same `PublishedDiagnosticsUri` canonicalization as push (mcp/002, #552); a pull for an alias writes under its canonical file key |
| Many subscribed files, one bulk resync | One resync/pull per tracked file; one notification per subscribed URI per session; trigger concurrency bounded (NFR-002) |
| Subscriber on a file the tracker has not opened | Subscribing does not open the document: a `didOpen` is a side effect that belongs to the no-tool-call trigger (#648). The first pull opens it and notifies |
| Session disconnects mid-delivery | Handled by existing `Target::send` / `TargetClosed` path; no change |
| `didSave` sent to a server that closes the document on save semantics | Not applicable: send only per FR-009's capability gate |
| Cache cap reached while many subscribed files are pulled | Eviction per FR-012; the evicted files' subscribers are notified (FR-013) |
| A push carries a document version newer than the pulled slot's | The pulled slot is dropped, since its content is older than what the server last said |
| A push has no version, or an equal or lower one | The pulled slot stays until the next pull: servers that push and pull the same version keep both slots, and a versionless flycheck push must not erase the pulled native diagnostics (bridge/004). A cached read can therefore show pulled content older than the file, and since pulled items win in the merge it can hide a fresher pushed duplicate, until the next pull (#670) |
| A pull is answered while the server is still indexing | Stored like any other; every read path already carries `indexing_in_progress` and the next pull corrects the slot with one notification |
| First pull after startup, eviction or respawn | Change detection compares merged views, so the first pull is a change only when the merged view differs from what a read returned before. Volatile `data` fields read as a change on every pull |
| Same code within 3 lines | The dedupe ignores the message, the bridge/004 heuristic: two distinct errors with one code starting within 3 lines are shown once, with the pulled message |
| Pulled `relatedInformation` and `data` | `resources/read` returns the stored LSP diagnostics as the server sent them (`data` redacted), so a pulled `relatedInformation` location can name a path outside the workspace roots, exactly as a pushed one can |
| A pull-only file | `has_diagnostics` and the `subscriptions/listen` replay hold after a pull alone: a subscriber attaching then gets exactly one replayed update |

### Differences in `get_diagnostics` output

Compared with merging the cache into the pull at read time, results differ in four documented ways: items are ordered by the cache merge's stable `(start, end)` sort, so items with an equal start can reorder; pull items are bounded like pushed ones (256 KiB per message, 1 MiB per entry, keeping the most severe items when cut); identical pull items are not collapsed; and a failed pull falls back to a view that includes the last stored pulled slot, which may come from an older document version.

## 8. Success Criteria and Acceptance Tests

| ID | Criterion | Verification |
|----|-----------|--------------|
| SC-001 | Reproduction from the issue: after an appended error and one `get_diagnostics`, the subscriber receives a `resources/updated` within FR-008's bound, and `get_cached_diagnostics` and `resources/read` contain E0308 | Live test, release build, rust-analyzer, stdio |
| SC-002 | Reverting the file and pulling again notifies the subscriber and removes E0308 from all read paths | Live test |
| SC-003 | Three HTTP sessions (A and B on `main.rs`, C on `util.rs`): a pull that changes `main.rs` notifies A and B exactly once each and C never | Real-socket integration test with a sentinel push for `util.rs` |
| SC-004 | 100 consecutive pulls of an unchanged file yield zero notifications | Unit test on the change-outcome and a live count |
| SC-005 | A push-every-change server (existing mock/pyright path) sees unchanged notification counts versus the pre-change baseline | Existing pump tests plus a regression case |
| SC-006 | A pull racing an edit never publishes a stale report | Unit test moving the tracker's synced version while the request is in flight |
| SC-007 | Writing pull-sourced entries for 1,100 distinct files stays within `MAX_DIAGNOSTIC_ENTRIES` and does not evict another server's entries below its fair share | Unit test on `NotificationCache` |
| SC-008 | A flycheck-sourced pushed warning survives a clean pull and a later pull-sourced error survives a push | Unit test |
| SC-009 | A stalled session does not delay another session's notification (mcp/002 FR-006 test shape extended to pull-triggered publishes) | Integration test |
| SC-010 | Not applicable: Options B and C are not shipped (#648) | none |

## 9. Agent Boundaries

### Always (without asking)
- Run the pre-commit gate (fmt, clippy `-D warnings`, nextest, rustdoc) and add tests with every behavior change.
- Reuse `PublishedDiagnosticsUri`, `DiagnosticSources::merge`, `Delivery` and the cache's eviction machinery rather than adding a parallel store.
- Update `.local/testing/` playbooks, `coverage-status.md` and regression documents for the changed behavior; do not edit `continuous-improvement.md`.
- Add a `CHANGELOG.md` entry under `[Unreleased]`, one line, ending with the PR link.

### Ask first
- Adding a dependency (`notify` for Option C) or a new CLI/config option (e.g. `--diagnostics-didsave`, watcher ignore list).
- Sending `didSave` at all (Option B): it triggers real `cargo check` runs in the user's workspace.
- Changing the notify-on-every-publish behavior for push servers (FR-007), which alters mcp/002 FR-004 and its test expectations.
- Changing `Translator`'s constructor or ownership to reach the session registry.

### Never
- Modify `notifications/resources/updated` payload or tool/resource schemas without a spec change.
- Hold the `NotificationCache` lock across an LSP round trip or a peer send.
- Let a pull write erase pushed flycheck diagnostics (regresses bridge/004).
- Notify a session for a URI it did not subscribe to, or let one session's stall block another.
- Write specs under `.local/specs/`.

## 10. Decisions

All questions of the draft are resolved:

- Option set: A only; the trigger is a tool call that pulled (#648 covers the rest).
- No notification is required without a tool call.
- Freshness bound: the notification is enqueued before the pull returns.
- FR-002's change detection applies to pull writes only; pushes keep notifying on every accepted publish.
- Provenance is the closed enum `{Pushed, Pulled}`; dedupe follows FR-004.
- `file_watcher` is not reintroduced; rust-analyzer's `workspace/diagnostic/refresh` behavior is moot while Option D is not shipped.
- Subscribing does not open the document.
- Eviction of a subscribed entry is announced for pull-triggered and push-triggered evictions alike (#649); protecting subscribed entries from eviction was rejected because it breaks the fair-share and aggregate bounds.
- `previous_result_id` stays `None`.

## 11. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[mcp/002-mcp-resources-diagnostics/spec|mcp/002]] — subscription and delivery model (FR-004, FR-006..FR-012)
- [[bridge/002-document-tracker-synchronization/spec|bridge/002]] — lazy disk-staleness sync, `didChange`
- [[bridge/004-get-diagnostics-flycheck-gap/spec|bridge/004]] — pull/push merge on read
- [[lsp/005-lsp-content-modified-retry/spec|lsp/005]] — `ContentModified` on pulls racing edits
- [[mcp/009-speculative-edit-diagnostic-preview/spec|mcp/009]] — in-memory edits, flycheck and watcher caveats
- Code: `runtime/pump.rs` (`diagnostics_pump`, `PumpWiring::publish_changed`), `bridge/notifications.rs` (`store_pulled_diagnostics`, `DiagnosticSources::merge`, budgets), `bridge/translator/diagnostics.rs` (`handle_diagnostics`), `mcp/session.rs` (`Delivery`)
