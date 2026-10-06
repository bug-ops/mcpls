---
aliases:
  - Stale push entry merged with fresh pull
  - get_diagnostics returns a fixed error after an edit
tags:
  - sdd
  - spec
  - bug
  - bridge
  - diagnostics
created: 2026-10-06
status: draft
related:
  - "[[constitution]]"
  - "[[MOC-specs]]"
  - "[[bridge/004-get-diagnostics-flycheck-gap/spec|bridge/004-get-diagnostics-flycheck-gap]]"
  - "[[bridge/009-diagnostics-subscription-staleness/spec|bridge/009-diagnostics-subscription-staleness]]"
  - "[[bridge/011-push-only-server-diagnostics/spec|bridge/011-push-only-server-diagnostics]]"
  - "[[bridge/002-document-tracker-synchronization/spec|bridge/002-document-tracker-synchronization]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp/002-mcp-resources-diagnostics]]"
---

# Feature: A push entry for an older document version is not merged into the answer for the current one

> [!info] Metadata
> **Type**: bug
> **Priority**: P2
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle 040, live-test finding
> **Issue**: #703
> **Reproduced on**: release build, HEAD 12ee3a1, pyright 1.1.408, stdio

> [!abstract]
> `get_diagnostics` merges the server's `textDocument/diagnostic` pull report with the push cache.
> After an edit, mcpls re-syncs the document with `didChange` (version N+1) and the server's pull
> answers for the new content, but a push entry the server published for version N is still in the
> cache and is merged into the answer. The caller sees an error the server's own pull reported as
> fixed, for as long as the server takes to publish for N+1 (about 250-350 ms with pyright).
> `publishDiagnostics` carries `version` and the document tracker knows the synced version, so the
> stale entry is detectable. This spec makes a push entry for an older version never contradict an
> answer for the current content, on every surface that reads the cache, and documents the guarantee.

## 1. Overview

### Problem Statement

`Translator::handle_validated_diagnostics` (`crates/mcpls-core/src/bridge/translator/diagnostics.rs`)
re-syncs the tracked document (`prepare_document_for_path`, which sends `didChange` when the file
changed on disk), stamps the pull with the synced version (`begin_pull`), sends the pull, stores the
report as the file's pulled slot (`settle_pull`) and merges the pulled slot with the pushed slots
(`DiagnosticSources::merge`, `bridge/notifications.rs`). The merge keeps every pushed diagnostic
that is not a duplicate of a pulled one. It never looks at the pushed entry's `version`. The doc
comment on `handle_validated_diagnostics` accepts this: "a pushed entry may reflect a slightly older
document version than the fresh pull result". The user guide (`docs/user-guide/tools-reference.md`,
"Verifying an Edit", step 3) warns only about a stale **empty** result, not about a stale **error**.

**Observed** (pyright 1.1.408, one `[[lsp_servers]]` with `language_id = "python"`,
`command = "pyright-langserver"`, `args = ["--stdio"]`, `file_patterns = ["**/*.py"]`):

1. Write `x: int = "s"` to a file, call `get_hover`, wait 1.5 s, call `get_diagnostics` until it
   returns the `reportAssignmentType` error (`availability: published`, `origin: pull`, 1 item).
2. Overwrite the file with `x: int = 1`.
3. Call `get_diagnostics` six times, 150 ms apart. Counts returned: 1, 1, 0, 0, 0, 0, identical in
   4 rounds.

Trace of one round: mcpls sends `textDocument/didChange` version 2 at T+0.000; the pulls (T+0.005 and
T+0.161) are both answered `{"kind":"full","resultId":"6","items":[]}`; the server's
`publishDiagnostics` for version 2 arrives at T+0.252. Both calls before that returned the version 1
error from the push cache. A scratch driver is `.local/testing/scratch/ci040/t_stale2.py`.

**Why it matters.** The documented "Verifying an Edit" loop is: apply an edit, call `get_diagnostics`,
revert if new errors appear. The mirror failure (a fixed error still listed) makes an agent revert or
re-edit a correct change, or loop on an error that no longer exists. The server answered
authoritatively for the current content and mcpls contradicted it from its own cache.

**Why the entry is detectable.** A push entry carries `version: Option<i32>` (`DiagnosticInfo`). The
`DocumentTracker` knows the version the owning server was last told (`synced_version`). A pushed entry
with `version = Some(v)` and a synced version greater than `v` describes content the server has since
been told to replace. [[bridge/009-diagnostics-subscription-staleness/spec|bridge/009]] (FR-005 and
the version-supersession rules for the pulled slot, #670) already uses the tracker this way for the
pulled slot; the pushed slots have no equivalent.

### Goal

A pushed diagnostics entry whose document version is older than the version the owning server was last
synced to is excluded from the merged view that `get_diagnostics`, `get_cached_diagnostics` and the
`lsp-diagnostics://` resource return, so an answer for the current content is never contradicted by an
older publish; the guarantee is documented.

### Out of Scope

- Waiting for the server's publish for the new version, or retrying the pull, to make the answer
  complete. Reporting what is known is in scope; blocking is not.
- Notifying subscribers of a change that no tool call observes (a resync with no pull); that stays
  with #648 ([[bridge/009-diagnostics-subscription-staleness/spec|bridge/009]]).
- Rejecting a stale publish at store time in the diagnostics pump (see Open Questions); this spec
  works at read and merge time.
- Staleness of the **empty** result after an edit on servers whose diagnostics come from a build step
  (rust-analyzer flycheck). That is a different mechanism (the server has not produced the new result
  yet), already described in the user guide; this spec changes only the stale-non-empty direction.
- Servers that publish versions that do not correspond to the versions mcpls sent (see Open
  Questions); detecting that is not attempted.
- Technical design: recorded in a plan after this spec is approved.

## 2. User Stories

### US-001: A fixed error is not reported after the fix

AS AN AI coding agent verifying an edit
I WANT `get_diagnostics` right after I fix an error to not list that error
SO THAT I do not revert or redo a correct edit.

**Acceptance criteria:**
```
GIVEN a pull-capable server that published one diagnostic for document version N
  AND the file was edited so the server is now synced to version N+1
  AND the server's pull answer for version N+1 is empty
  AND the server has not yet published for version N+1
WHEN get_diagnostics is called for the file
THEN the response contains no diagnostics
  AND it does not contain the version N diagnostic
```

### US-002: All read paths agree

AS A client that reads diagnostics through tools and resources
I WANT `get_diagnostics`, `get_cached_diagnostics` and `resources/read` to return the same
diagnostics for the same file at the same moment
SO THAT the answer does not depend on which surface I used.

**Acceptance criteria:**
```
GIVEN the state of US-001 after get_diagnostics returned
WHEN get_cached_diagnostics is called and lsp-diagnostics://<file> is read
THEN both exclude the version N diagnostic, as get_diagnostics did
```

### US-003: A publish for the current version still shows up

AS AN AI coding agent
I WANT a diagnostic the server publishes for the current document version to be returned
SO THAT excluding stale entries never hides a real error.

**Acceptance criteria:**
```
GIVEN the file is synced to version N+1
  AND the server publishes one diagnostic for version N+1
WHEN get_diagnostics is called
THEN the response contains that diagnostic, whatever the pull answered for it by the merge rules
     of bridge/004
```

### US-004: Entries without a version are not guessed at

AS A user of a server that publishes without `version`
I WANT its pushed diagnostics to keep appearing as before
SO THAT flycheck-style results are not dropped on a guess.

**Acceptance criteria:**
```
GIVEN a server whose publishDiagnostics carries no version
WHEN the document is edited and get_diagnostics is called
THEN the merge result is identical to the previous build
```

### US-005: The documentation states the guarantee

AS AN AI coding agent reading the user guide
I WANT the guide to say when a returned diagnostic can be older than the current content
SO THAT I know what a diagnostics answer right after an edit does and does not promise.

**Acceptance criteria:**
```
GIVEN docs/user-guide/tools-reference.md
WHEN the get_diagnostics section and "Verifying an Edit" are read
THEN they state that a pushed entry for an older document version is not returned
  AND they state which cases are still not covered (versionless pushes, build-step results)
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE SYSTEM SHALL classify each pushed source entry of a file as superseded or current: superseded when the entry has `version = Some(v)` and its `(opening, version)` differs from the tracker's synced `(opening, version)` for the entry's owning server (lower or higher), current otherwise, including every unversioned entry and every entry of a file the tracker does not hold open | must |
| FR-002 | WHEN the merged view of a file is built for any read path, THE SYSTEM SHALL drop from a superseded pushed entry only the diagnostics a pull of the same server already answers for: items recorded as covered, and items whose `source` the server's pull reports carry. No other diagnostic of the entry is dropped | must |
| FR-003 | THE SYSTEM SHALL apply FR-002 on `get_diagnostics`, `get_cached_diagnostics` and the `lsp-diagnostics://` resource read through one shared predicate, so the three surfaces agree at the same point in time | must |
| FR-004 | THE SYSTEM SHALL NOT exclude an unversioned pushed entry, a current pushed entry, or the pulled slot on account of this classification (the pulled slot keeps the supersession rules of [[bridge/009-diagnostics-subscription-staleness/spec\|bridge/009]]) | must |
| FR-005 | WHEN the owning server's pulled slot exists at the synced `(opening, version)` and a pushed entry of that server is superseded, THE SYSTEM SHALL return the pulled diagnostics merged with that entry minus its covered items, so an empty pull report plus a superseded push entry whose items the earlier pull reported yields an empty result. Without that slot (failed, partial or `unchanged` pull, push-only server) nothing is dropped | must |
| FR-006 | WHEN a push is accepted for the file, THE SYSTEM SHALL keep its current behavior: the entry replaces the owner's previous entry for that source, and one `resources/updated` is sent per accepted publish. Exclusion at read time does not suppress, delay or add notifications for pushes | must |
| FR-007 | WHEN no pulled slot of the entry's owner exists at the synced state (push-only server, pull failure, `unchanged` or partial report), THE SYSTEM SHALL keep the latest push as the answer; `get_diagnostics` reports a failed pull with `origin: cache_after_failed_pull`, and an error only when the merged result is empty | must |
| FR-008 | WHEN a pull write changes the merged view of a subscribed file, including only because covered items drop out, THE SYSTEM SHALL notify once: the view before and after the write are built under the same rule, and a pull write that only changes coverage or the slot's `(opening, version)` is compared as a replacement, so a later identical pull compares as unchanged | must |
| FR-009 | THE SYSTEM SHALL read the synced version for the entry's owning server, so entries of one server are never compared with the synced version of another server's copy of the document | must |
| FR-010 | THE SYSTEM SHALL apply the classification per entry, including entries stored under a symlink alias URI, using the version stored with that entry | must |
| FR-011 | THE merged view's `version` SHALL be computed from the remaining (non-excluded) entries, per the existing rule (canonical pushed entry's version, else the pulled slot's, else none) | should |
| FR-012 | WHEN the document was closed or evicted and reopened, or the owning server was respawned, THE SYSTEM SHALL NOT treat a leftover entry as current merely because its `version` equals the restarted synced version: the entry's opening must match. Coverage is recorded only for a pull of the entry's own opening, and a learned source never covers an entry of another opening. A publish of an earlier opening that arrives after the reopen is stored as of the current opening and stays visible until the server republishes | must |
| FR-013 | THE SYSTEM SHALL add no LSP request, wait or retry for FR-001 to FR-012 | must |
| FR-014 | WHEN a stale pushed entry is excluded, THE SYSTEM SHALL log it at DEBUG with the URI, the entry version and the synced version, and SHALL NOT log it at WARN or ERROR | should |
| FR-015 | THE user guide SHALL describe the guarantee and its limits in the `get_diagnostics` section and in "Verifying an Edit" (US-005), and `CHANGELOG.md` SHALL record the behavior change | must |
| FR-016 | THE tool descriptions and `tool_surface.json` SHALL stay unchanged unless the plan decides a new field is needed; if so, the `tools/list` size budget applies | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | The classification is a closed enum (current, stale, unversioned) computed in one place and matched exhaustively by the merge; no `Option<i32>` comparison scattered across read paths and no boolean flag, per [[constitution]] |
| NFR-002 | Consistency | The same predicate and the same tracker read serve all three read paths (FR-003); a second implementation of "is this entry stale" is not acceptable (DRY, as [[bridge/010-workspace-containment-single-predicate/spec\|bridge/010]] did for containment) |
| NFR-003 | Concurrency | The tracker read follows the existing lock discipline: no cache lock held across an `await` on an LSP request; `synced_version` is a synchronous read already used under the cache lock by `settle_pull` |
| NFR-004 | Performance | The classification adds at most one tracker lookup per pushed entry of the requested file (at most 8 pushed slots per file) and no allocation proportional to the diagnostics count |
| NFR-005 | Compatibility | The output schema is unchanged by default (FR-016); pre-1.0, a field addition is allowed but needs the plan to justify it. Behavior change recorded in `CHANGELOG.md` |
| NFR-006 | Observability | A normal edit-then-read produces no warn or error log line for a stale entry (FR-014) |
| NFR-007 | Windows and macOS | The fixture that reproduces the race uses a scripted fake server with no timing sleeps in assertions; fake executables follow the `.exe` rule on Windows |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Pushed source entry | One server's last published diagnostics for one URI | `uri`, `version: Option<i32>`, `diagnostics`, owner `ServerId`, spelling (canonical or alias) |
| Entry freshness | Result of comparing an entry with the owner's synced version | closed enum: `Current`, `Stale`, `Unversioned` |
| Synced version | The document version the owning server was last told, read from the `DocumentTracker` | per (document, `ServerId`); restarts at a lower value after respawn or close and reopen |
| Pulled slot | The file's last stored pull report, stamped with the version it answered | unchanged by this spec |
| Merged view | What every read returns (`DiagnosticSources::merge`) | built from the pulled slot plus the non-stale pushed entries |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Reproduction: push v1 error, edit to v2, pull clean at v2, no v2 publish yet | Empty result (FR-002, FR-005); no v1 error |
| Server publishes v2 (clean) 250 ms later | Entry replaced; one notification per accepted publish (FR-006); result stays empty |
| Server publishes v2 with a new error, pull already answered clean | The v2 push entry is current and is merged by the bridge/004 rules (FR-004); the new error is reported |
| Server publishes without `version` (flycheck-style) | Unversioned: never excluded (FR-001, FR-004); behavior identical to today |
| Push entry `version` equal to the synced version | Current |
| Push entry `version` greater than the synced version | Not stale by FR-001; see FR-012 and the Open Questions (reopen or respawn restarts the tracker version) |
| Several edits before any publish (v1 push, synced v4) | Stale; excluded |
| Server publishes late for an intermediate version (v2) while synced is v3 | Stored as published, classified stale at read, excluded until the v3 publish replaces it |
| Push-only server (no pull), edit, no publish yet | No pull report to override the entry; the stale entry is not presented as current (FR-007); wording in Open Questions |
| Pull fails (timeout, `ContentModified`, `unchanged`, partial) and the cache holds only a stale entry | FR-007: not presented as current |
| Pull fails and the cache holds a current entry | Cache-only answer as today |
| Two servers route the same file (multiple pushed sources) | Each entry is compared with its own owner's synced version (FR-009); a stale entry of one does not affect the other |
| Symlink alias URI entry | Classified by the version stored on that entry (FR-010) |
| Server respawned | Cached diagnostics of the replaced process are already dropped ([[bridge/011-push-only-server-diagnostics/spec\|bridge/011]]); the file reads as pending until the replacement publishes; no leftover entry is classified against the restarted versions (FR-012) |
| Document closed or evicted and reopened (tracker version restarts at 1) | An entry published for the earlier opening must not read as current only because its version is not below the new synced version (FR-012) |
| File changed on disk but no tool call resynced it | Nothing is stale yet: the tracker has not moved; unchanged behavior (the lazy sync model of [[bridge/002-document-tracker-synchronization/spec\|bridge/002]]) |
| Subscriber on the file | A pull that makes the stale entry drop out of the view is a pull write; FR-008 governs whether it notifies |
| `get_cached_diagnostics` on a never-pulled file after an edit made by another session | The same classification applies (FR-003); the tracker is shared across sessions |
| `workspace/diagnostic` or `workspaceDiagnostic` results | Not involved; document-pull and push only |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Scripted fake server: publishes a v1 error on open, answers the v2 pull clean, delays its v2 publish | `get_diagnostics` returns an empty list on every call between the `didChange` and the v2 publish; 0 calls return the v1 error |
| SC-002 | Live repro on pyright (the observed steps above, 4 rounds) | Counts 0, 0, 0, 0, 0, 0 in every round; no call returns the reportAssignmentType error after the overwrite |
| SC-003 | Fake server publishes a v2 error after the clean pull | `get_diagnostics` returns the v2 error from the first call that sees the cached publish |
| SC-004 | Fake versionless server | Output byte-identical to the previous build on the same script |
| SC-005 | `get_cached_diagnostics` and `resources/read` after the SC-001 state | Same diagnostics as `get_diagnostics` at the same point (US-002, NFR-002) |
| SC-006 | rust-analyzer live check on a flycheck-sourced diagnostic published at the pre-edit version | Behavior recorded in the live-test report and decided per the Open Questions; no regression of the [[bridge/004-get-diagnostics-flycheck-gap/spec\|bridge/004]] cases in the existing suite |
| SC-007 | Respawn and reopen cases (FR-012) | Unit tests: a leftover entry of the earlier process or opening is never returned as current |
| SC-008 | Existing diagnostics tests (pump, cache, merge, subscriptions) | All pass unchanged except those that encoded the stale merge |
| SC-009 | User guide | The `get_diagnostics` section and "Verifying an Edit" state the guarantee and its limits (US-005) |

## 8. Agent Boundaries

### Always (without asking)
- Reproduce with a scripted fake server and keep it as a regression fixture; record the pyright repro as a playbook case under `.local/testing/` (playbooks, `coverage-status.md`, regression documents), without editing `continuous-improvement.md`.
- Express freshness as an enum matched exhaustively (NFR-001) and read it through one predicate (NFR-002).
- Keep the pull plus push merge rules of [[bridge/004-get-diagnostics-flycheck-gap/spec|bridge/004]] unchanged for current and unversioned entries.
- Run the full pre-commit suite and add a one-line `CHANGELOG.md` entry with the PR link.

### Ask First
- Dropping a stale entry at store time in the pump instead of at read time.
- Adding a response field or an `availability` value for "stale" (FR-007, FR-016).
- Treating versions that exceed the synced version as stale (FR-012).
- Changing flycheck-sourced behavior for rust-analyzer if the live check shows they carry versions.

### Never
- Exclude an unversioned entry on a guess.
- Add an LSP request, a sleep or a wait to hide the window.
- Special-case a server by name.
- Return an unqualified empty list for a file whose only entry was excluded and for which no pull answered (FR-007; see [[bridge/011-push-only-server-diagnostics/spec|bridge/011]]).
- Hold the `NotificationCache` lock across an LSP round trip.
- Write specs under `.local/specs/`.

## 9. Decisions

> [!success] Resolved for #703
> - **Coverage, not blanket exclusion.** A superseded push is never hidden wholesale: flycheck and clippy items that no pull reports stay visible (bridge/004). An item is covered when a same-problem item was in the same server's pulled slot at the entry's own state, recorded at the moment that slot leaves the cache (replacement, a superseding push from any server, eviction, clear), or when its `source` is one the server's non-empty full pull reports carried (up to 32 sources per server, forgotten with the server's diagnostics).
> - **Store-time computation, read-time exclusion.** Coverage is computed when the pulled slot is retired and stored on the pushed entry; the exclusion itself is decided at read, under the cache lock, from one predicate shared by `get_diagnostics`, `get_cached_diagnostics` and the resource. A new publish for a source replaces the entry and resets its coverage.
> - **Higher versions.** An entry whose version is above the synced one is superseded as well as a lower one (reopen restarts the tracker's numbering).
> - **Late publishes.** A publish for an older version arriving after the pull for the newer one is stored and excluded at read when its source was learned; otherwise it stays until the server's next publish (documented limit, also for a first-ever pull that came back empty and for items without a `source`).
> - **Pull failure.** A failed pull leaves no pulled slot at the synced state, so nothing is excluded and the cache answers with `origin: cache_after_failed_pull`.
> - **Learned sources across files.** A source learned from one file's pull can change another file's merged view (its superseded push items now count as covered) without a `resources/updated` for that file; the next publish or pull of it notifies. Accepted: it only hides items the server's own pulls report.
> - **Version of the merged view** comes from a current canonical pushed entry, else the pulled slot: a superseded push never lends its version to the newer pull's content.
> - **Servers with their own version counter** stay out of scope; the LSP contract ties `version` to the document version the client sent.

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[bridge/004-get-diagnostics-flycheck-gap/spec|bridge/004]] — pull plus push merge and the flycheck guarantee this spec must not regress
- [[bridge/009-diagnostics-subscription-staleness/spec|bridge/009]] — pulled slot, version supersession, change detection and the three-read-path agreement
- [[bridge/011-push-only-server-diagnostics/spec|bridge/011]] — `availability` and `origin`, respawn dropping the cache
- [[bridge/002-document-tracker-synchronization/spec|bridge/002]] — lazy resync and synced versions
- [[mcp/002-mcp-resources-diagnostics/spec|mcp/002]] — cache-backed diagnostics tool and resources
- Docs: `docs/user-guide/tools-reference.md` (`get_diagnostics`, "Verifying an Edit")
- Code: `crates/mcpls-core/src/bridge/translator/diagnostics.rs` (`handle_validated_diagnostics`, `settle_pull`), `crates/mcpls-core/src/bridge/notifications.rs` (`DiagnosticSources::merge`, `DiagnosticInfo.version`, `NotificationCache::attach_documents`), `crates/mcpls-core/src/bridge/state.rs` (`DocumentTracker::synced_version`)
