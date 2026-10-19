---
aliases:
  - Sweep stall on unanswered pull
  - Per-file sweep isolation
tags:
  - sdd
  - spec
  - bug
  - bridge
  - diagnostics
  - subscriptions
  - reliability
created: 2026-10-19
status: implemented
related:
  - "[[constitution]]"
  - "[[MOC-specs]]"
  - "[[bridge/009-diagnostics-subscription-staleness/spec|bridge/009-diagnostics-subscription-staleness]]"
  - "[[bridge/002-document-tracker-synchronization/spec|bridge/002-document-tracker-synchronization]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp/002-mcp-resources-diagnostics]]"
---

# Feature: A pull that never answers delays only its own file in the disk sweep

> [!info] Metadata
> **Author**: Andrei G.
> **Type**: bug
> **Priority**: P2
> **Source**: continuous-improvement cycle 043, live test of the disk sweep
> **Issue**: #806
> **Related issues**: #798 (sweep resync has no deadline and holds the path lock; shutdown does not wait for sweep jobs; stamps read under two locks), #791 (`didSave` depends on content), #802 (sweep windowing, shared constants); sweep history #784, #790, #794
> **Reproduced on**: release build, HEAD d2a9533, stdio, scripted stdio language server (no real server needed)

> [!abstract]
> The disk sweep ([[bridge/009-diagnostics-subscription-staleness/spec|bridge/009]] FR-017..FR-023)
> resyncs subscribed and companion files every 2 s and notifies subscribers of on-disk edits. It
> visits the files of one server one after another. When the `textDocument/diagnostic` pull of one
> subscribed file never answers, every other file of that server waits behind it: no `didChange`,
> no `didSave` and no `resources/updated` for them until the stuck file has used up its bounded
> retries. This spec requires the delay to stay with the stuck file.

## 1. Overview

### Problem Statement

[[bridge/009-diagnostics-subscription-staleness/spec|bridge/009]] FR-020 says a server that stops
answering "stalls only its own files". That holds across servers, but not across the files of one
server. The sweep runs one job per server, and the job handles that server's files in sequence.
For a subscribed file the visit includes a pull, and a pull that gets no answer is bounded only
by the LSP request timeout (`request_timeout_seconds`, default 30 s). A failed pull is retried on
later ticks at most 3 more times for the same document state (FR-017), each time waiting out the
full timeout. The job is busy for roughly `(1 + 3) x (request timeout + 2 s tick)` and visits no
other file of the server meanwhile.

Observed with a scripted server that answers pulls for `b.fk` and `c.fk` at once and never
answers a pull for `a.fk` (`request_timeout_seconds = 4`; all three files subscribed; `a.fk`
edited on disk, then `b.fk` 2.5 s later):

| Time (s) | Server-side event |
|----------|-------------------|
| 976.06 | pull `a.fk` |
| 980.06 | cancel (timeout) |
| 982.06 | `didChange` and `didSave` `a.fk`, pull `a.fk` |
| 986.06 | cancel |
| 988.06 | pull `a.fk` |
| 992.06 | cancel |
| 994.06 | pull `a.fk` |
| 998.06 | cancel |
| 1000.06 | `didChange`, `didSave` and pull `b.fk` |

The edit of `b.fk` at about 978 s reached the server, and so its subscribers, 21 s late. With the
default 30 s timeout the same sequence takes about 128 s. Control: with a second server owning
`b.f2`, an edit of `b.f2` was delivered 0.6 s after the edit while the first server's pull hung,
so per-server isolation holds. Tool calls are unaffected (a hover answered in 0 ms throughout).

Effect: one slow or wedged document delays on-disk-edit delivery for every other subscribed or
companion file of its server for minutes. A server that is merely slow on one large file causes a
proportionally shorter stall. A subscriber waiting to learn whether an edit broke the build gets
the answer minutes late, with no indication that anything is wrong.

### Goal

A pull or resync that does not answer for one file delays at most that file; edits to the other
subscribed and companion files of the same server are resynced and reported within about one sweep
interval, as when every pull answers.

### Out of Scope

- Shortening or changing the meaning of `request_timeout_seconds`, or any tool call's timeout.
- The retry cap per document state (FR-017) and the stuck file's own delay: both stay as they are.
- Shutdown ordering of sweep jobs, atomic reading of a document's stamp, and a deadline on the
  resync step: tracked in #798 and delivered with this change (see Design); the observable
  behaviour is stated here.
- Servers that never answer anything (dead, wedged initialization): FR-019 and FR-020 of
  bridge/009 already cover them, including the per-server isolation that this spec preserves.
- Changes to the notification payload, tool schemas or `tool_surface.json`.
- How a stuck file is reported to a client that reads diagnostics by tool call: unchanged.

## 2. User Stories

### US-001: My edit is reported although another file of the server hangs

AS A coding agent subscribed to `lsp-diagnostics://` URIs of several files served by one language
server
I WANT an on-disk edit of one file reported within a few seconds even while the server never
answers a pull for a different subscribed file
SO THAT one wedged document does not blind me to the rest of the project.

**Acceptance criteria:**
```
GIVEN a server that never answers the diagnostics pull for a.fk and answers it at once for b.fk,
      and a session subscribed to the diagnostics URIs of a.fk, b.fk and c.fk
WHEN a.fk is edited on disk, and b.fk is edited on disk 2.5 s later
THEN the server receives didChange (and didSave, when advertised) for b.fk within 5 s of b.fk's edit
AND the session receives resources/updated for b.fk within 5 s of the edit when b.fk's merged
    diagnostics changed
AND the bound does not depend on request_timeout_seconds
```

### US-002: Companion files are resynced although a subscribed file hangs

AS A coding agent subscribed to a file whose server also tracks other, unsubscribed files
I WANT those companion files to be resynced (and saved when the server asks for it) within one
sweep interval even while one subscribed file's pull is stuck
SO THAT the cross-file refresh of bridge/009 FR-022 keeps working.

**Acceptance criteria:**
```
GIVEN the setup of US-001 plus a tracked companion file d.fk that nobody subscribed to
WHEN d.fk is edited on disk while the pull for a.fk is hung
THEN the server receives didChange (and didSave, when advertised) for d.fk within 5 s of the edit
```

### US-003: A second server is not affected

AS A user with two language servers
I WANT the per-server isolation of bridge/009 FR-020 to keep holding
SO THAT a hung pull on one server never delays the other.

**Acceptance criteria:**
```
GIVEN servers fake (hung pull for a.fk) and fake2 (owning b.f2), both files subscribed
WHEN b.f2 is edited on disk while the pull for a.fk is hung
THEN fake2 receives didChange for b.f2 and the session is notified within 5 s of the edit
```

### US-004: The default timeout behaves like a short one

AS A user who runs with the default `request_timeout_seconds` (30)
I WANT the delay of other files to stay bounded in the same way
SO THAT the fix is not visible only in tests that shorten the timeout.

**Acceptance criteria:**
```
GIVEN the setup of US-001 with request_timeout_seconds left at its default
WHEN b.fk is edited on disk while the pull for a.fk is hung
THEN b.fk is delivered within 5 s of the edit, not after roughly 128 s
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHILE a `textDocument/diagnostic` pull for one tracked file is outstanding, THE SYSTEM SHALL resync, save (per bridge/009 FR-023) and, for subscribed files, pull the other tracked files of the same server at the same cadence as when no pull is outstanding (bridge/009 FR-017, FR-022) | must |
| FR-002 | WHEN an on-disk edit of a subscribed or companion file is made while a pull for another file of its server is outstanding, THE SYSTEM SHALL send that file's `didChange` (and `didSave` when FR-023 allows it) and, when its merged diagnostics change, notify its subscribers, no later than 5 s after the edit with the default sweep interval, for any `request_timeout_seconds` | must |
| FR-003 | WHEN a pull for a file does not answer, THE SYSTEM SHALL keep applying the existing retry behaviour to that file only: the sweep sends at most `MAX_FAILED_PULLS_PER_STATE` (3) pulls per document state, whatever the epoch (bridge/009 FR-017), each bounded by the request timeout, and SHALL treat the pull as failed the way a timed-out pull is treated elsewhere (bridge/009 FR-006: cache and subscribers unchanged). Together with the pull of the tool call that tracked the state, at most 4 requests reach the server | must |
| FR-004 | THE SYSTEM SHALL keep at most one outstanding sweep pull per file and per document state, so a stuck file's retries neither multiply nor accumulate while the file waits for its next attempt | must |
| FR-005 | WHILE a pull for one file is outstanding, THE SYSTEM SHALL keep the other bridge/009 guarantees for that file's server: no more than 8 epoch-driven pulls per server per 2 s window (FR-022), no more than 64 subscribed and 32 companion visits per tick (FR-018, FR-022), and no probing, respawning or waiting on a dead or initializing server (FR-019) | must |
| FR-006 | THE SYSTEM SHALL keep a hung pull for a file on one server from delaying any file of another server (bridge/009 FR-020, unchanged) | must |
| FR-007 | WHEN the stuck file's pull finally answers after the other files have moved on, THE SYSTEM SHALL apply it under the existing version and ordering rules (bridge/009 FR-005) and SHALL NOT notify twice for one change (bridge/009 NFR-001) | must |
| FR-008 | WHEN the sweep stops (server shutdown, session end), THE SYSTEM SHALL abandon the outstanding pulls of every file together with the rest of that server's job, with no pull or resync sent after the server shuts down (bridge/009 FR-020) | must |
| FR-009 | WHILE a file's pull is outstanding, THE SYSTEM SHALL keep tool calls for any file answered as before (a hover answers without waiting for the sweep) | must |

> [!note] Resolved: order within a pull pass
> Files whose own state is new to the sweep are pulled first, then files made stale by another
> file's change (epoch), then retries of failed pulls. Retries and epoch pulls are the ones a pass
> defers when it met a full background cap (see Design).

### Design

- **Two passes.** A per-server job resyncs every file it was given (subscribed and companion), each
  step bounded by the server's request timeout; a step that times out ends the job. The job then
  hands the subscribed files that are due to a pull pass that runs in the background, one pass per
  server at a time. The schedule skips a server only while its resync runs, so a pull never delays
  the next tick's `didChange` or `didSave` (FR-001, FR-002).
- **Patience, then background.** The pass waits 2 s (one sweep interval) for a pull. A pull that has
  not answered moves to a background task when its server has fewer than 2 pulls waiting there;
  the pass continues with the next file. When the cap is full the pull is cancelled and counted as
  a timeout, the pass defers its retries and the files whose background slot is taken, and ends
  after at most that one extra patience.
- **One claim per file.** A pull holds a claim on its file (FR-004). A claim dropped unfinished
  restores the earlier attempt; a claim that outlived its memo entry cannot touch a later one.
- **Counted failures.** Timeouts and errors are counted per document state: timeouts whatever the
  epoch, so a hung file is bounded (FR-003); errors per epoch, so a file that failed fast is pulled
  again when another file changed.
- **Shutdown.** Stopping the sweep ends the jobs and the passes, drops the background pulls, which
  cancels their LSP requests, and returns only when all of them are gone (FR-008).
- **Content, not versions, decides `didSave`.** The sweep keeps a digest of the text it last saw of
  each file; `didSave` and the server's epoch move only when the text differs (#791).

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Latency | With every other pull answering at once, the time from an on-disk edit of a subscribed or companion file to its `didChange` at the server is at most one sweep interval (2 s) plus a margin of 3 s, regardless of how many other files have unanswered pulls |
| NFR-002 | Stuck-file delay | A file whose pulls never answer is visited and retried no more often and no more times than today: the sweep sends `MAX_FAILED_PULLS_PER_STATE` (3) pulls per document state, one outstanding at a time and a request timeout apart (FR-003, FR-004). The fix SHALL NOT make it retry faster or more often to make up for the wait |
| NFR-003 | LSP load | At most 3 sweep pulls per server are outstanding at once: the one the pull pass waits for and `MAX_DETACHED_PULLS_PER_SERVER` (2) in the background. The existing windows (64 subscribed files per tick, 8 epoch-driven pulls per 2 s window) stay, so a server with many hung files receives at most the same number of requests as one with answering files |
| NFR-004 | Resource use | Outstanding pulls hold no lock that a tool call or another file's visit needs (the cache lock is never held across an LSP round trip, bridge/009 NFR-005), and a hung pull holds no per-path lock that a tool call for the same path could wait on. A resync and a `didSave` are bounded by the server's request timeout, so a wedged queue releases the path lock and ends the job (#798) |
| NFR-005 | Compatibility | No change to configuration, CLI flags, tool schemas, resource schemas or notification payloads; `request_timeout_seconds` keeps its meaning |
| NFR-006 | Type safety | Per-file sweep progress (idle, pull outstanding, failed N times for a state) stays a closed typed state, not booleans or paired options, as in the existing sweep memo |

## 5. Data Model

| Entity | Description | Key attributes |
|--------|-------------|----------------|
| Sweep job | The per-server unit of sweep work that bridge/009 FR-020 runs one at a time | server id, subscribed and companion windows |
| File sweep state | What the sweep knows about one file's current document state | state stamp, failed pull count (0..3), whether a pull is outstanding |
| Document state | Opening and synced version a pull answers | existing `DocumentStamp` |
| Epoch | Per-server counter that makes subscribed files due again when another file changed | existing, unchanged |

## 6. Edge Cases and Error Handling

| Scenario | Expected behavior |
|----------|-------------------|
| Pull for `a.fk` never answers, `b.fk` edited | `b.fk` resynced, saved and pulled within the FR-002 bound; `a.fk` keeps its bounded retries (FR-003) |
| Every subscribed file of the server hangs | No file is delayed by another's wait; each file is retried at most 4 times per document state; subscribers get nothing for any of them (bridge/009 FR-006) |
| The server is slow on one large file (answers after 20 s) | Other files are delivered at the normal cadence; when the large file's pull answers it is stored and notifies once, if the merged view changed (FR-007) |
| `a.fk` is edited again while its pull is outstanding | The older pull is discarded under bridge/009 FR-005 when it answers; it is not cancelled early. The new state is pulled once, after the outstanding pull ended (at most one pull of a file is outstanding, FR-004) |
| The stuck file is unsubscribed while its pull is outstanding | The pull ends within the request timeout at the latest; no further pull for that file is issued by the sweep |
| The stuck file's pull answers after the retry cap was reached | The report is stored under the usual rules; the file's state is current, so no further pull is made |
| Server hangs on `didChange` or `didSave` rather than on the pull | Out of scope here; the bounded resync is tracked in #798 |
| Two files of one server hang at once | Both are retried independently; neither delays the other or any third file (FR-001) |
| More hung files than the background cap holds | The pull pass pays one more patience for the first pull that finds the cap full (it is cancelled and counted as a timeout) and leaves the retries and the files that are due for a later tick; the next tick's resync is not held up (Design) |
| A slow but answering file (about 5 s) meets a full cap three times | Its pulls are cancelled and counted as timeouts, which settle a state whatever the epoch, so it stays stale until its content changes or a tool call pulls it. Accepted: it takes three consecutive full-cap ticks, and a server with more than two unanswered pulls is already degraded |
| A file changes between the resync and the pull of one pass | The pass opens the file again, brings the server up to date, and skips the pull: the change is observed (`didSave`, epoch) and pulled by the next tick's visit, never observed twice |
| A file resynced by a tool call after the sweep stopped visiting it | The sweep compares it with the state it last saw (a bounded set of 256 tombstones): `didSave` goes out when the text differs, and not when an evicted file is opened again with the same text (#791). Beyond 256 other files visited since, the path counts as new and gets no `didSave` for a change made before its next visit |
| Shutdown while a pull is outstanding | Pull abandoned with the job (FR-008); no request reaches the server after shutdown |
| Untrusted workspace | `didSave` stays suppressed (bridge/009 FR-023); pull and resync isolation applies unchanged |

## 7. Success Criteria and Acceptance Tests

| ID | Criterion | Verification |
|----|-----------|--------------|
| SC-001 | Hung pull for `a.fk`, `request_timeout_seconds = 4`: `b.fk` edited 2.5 s after `a.fk` reaches the server (`didChange`, `didSave`, pull) within 5 s of its edit; observed today: 21 s | Live test with a scripted stdio server (`--hang-pull-file a.fk`, `--pullcontent`, `--save`); unit test on virtual time |
| SC-002 | The same session notifies the `b.fk` subscriber (`resources/updated`) within 5 s of the edit when `b.fk`'s diagnostics changed, and exactly once | Live test, MCP client log |
| SC-003 | Companion `d.fk` (tracked, unsubscribed) edited while `a.fk` hangs: `didChange` within 5 s | Live test |
| SC-004 | Two servers: with `fake` hanging on `a.fk`, an edit of `b.f2` on `fake2` is delivered within 5 s (observed 0.6 s today) | Live test, control scenario |
| SC-005 | Default `request_timeout_seconds` (30): `b.fk` delivered within 5 s of its edit, not after about 128 s | Live test, default config, wall-clock timestamps from the server log |
| SC-006 | The stuck file receives `MAX_FAILED_PULLS_PER_STATE` (3) sweep pulls for its document state, one at a time and a request timeout apart, with the same spacing as before the change | Server-side request log in SC-001; `runtime::disk_sweep` unit test on virtual time |
| SC-007 | A hover on `a.fk` and `b.fk` answers within its usual latency throughout the hang | Live test, tool call timing |
| SC-008 | When `a.fk`'s pull answers late (after `b.fk` and `c.fk` were delivered), it notifies its subscriber once and never twice for one change | Live test with a delayed (not hung) pull |
| SC-009 | Regression: with all pulls answering, the sweep's latency, request counts and notification counts match the pre-change baseline (bridge/009 SC-010, SC-011, SC-014 still pass) | Existing `runtime::disk_sweep` and `diagnostics::sweep_tests` unit tests plus live run |

## 8. Agent Boundaries

### Always (without asking)
- Run the pre-commit gate (fmt, clippy `-D warnings`, nextest, rustdoc, locked checks) and add a test for the hung-pull scenario (a pull that never answers for one file, an edit to another, a latency bound).
- Keep bridge/009's sweep guarantees (windows, caps, epoch, per-server job, `didSave` gate) when changing how files are visited.
- Update `.local/testing/` playbooks, `coverage-status.md` and regression documents for the changed behavior; do not edit `continuous-improvement.md`.
- Add a `CHANGELOG.md` entry under `[Unreleased]`, one line, ending with the PR link.

### Ask first
- Changing the default sweep interval, the retry cap per document state, or `request_timeout_seconds` semantics.
- Adding a configuration option or CLI flag for the sweep.
- Adding a dependency.
- Folding this work together with #798's shutdown and stamp changes in one PR.

### Never
- Weaken per-server isolation, the untrusted-workspace `didSave` suppression, or the "never probe a dead server" rule (bridge/009 FR-019, FR-020, FR-023).
- Raise the number of pulls sent to a hung file or to a server to compensate for the wait (NFR-002, NFR-003).
- Hold the notification cache lock across an LSP round trip.
- Modify `notifications/resources/updated` payloads or tool schemas.
- Write specs under `.local/specs/`.

## 9. Open Questions

- Resolved: the finding is #806 and is closed with #798, #791 and #802 in one change.
- Resolved: retries are visited after files whose state is new (section 3).
- Resolved: the unit tests run on virtual time; the live test keeps the 5 s bound and may widen it under CI load without changing the requirement.
- Open: should a stuck file be reported to the subscriber as unavailable (for example through the read-side flags `get_cached_diagnostics` already carries) after its retries are used up? The requirements keep today's behaviour, which reports nothing.
- Resolved: a hung `didChange`/`didSave` enqueue is bounded by the request timeout (#798); the job ends and the remaining files wait for a later tick.

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[bridge/009-diagnostics-subscription-staleness/spec|bridge/009]] — the sweep's contract (FR-017..FR-023, NFR-005..NFR-007, SC-010..SC-015)
- [[bridge/002-document-tracker-synchronization/spec|bridge/002]] — `didChange` on disk change
- [[mcp/002-mcp-resources-diagnostics/spec|mcp/002]] — subscription and delivery model
- Issue #798 — sweep resync has no deadline and holds the path lock; shutdown does not wait for sweep jobs
- Code: `runtime/disk_sweep.rs` (per-server job), `bridge/translator/diagnostics/sweep.rs` (`sweep_file`, `MAX_FAILED_PULLS_PER_STATE`)
- Live-test kit: `.local/testing/scratch/ci043/t_sweep_hang.py` and `fake_lsp.py --hang-pull-file a.fk --pullcontent --save`
