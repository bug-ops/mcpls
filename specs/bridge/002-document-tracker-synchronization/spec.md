---
aliases:
  - DocumentTracker synchronization
  - Document staleness detection
tags:
  - sdd
  - spec
  - bridge
  - state
created: 2026-08-05
status: implemented
related:
  - "[[constitution]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec]]"
---

# Feature: Document State Tracking, Disk-Staleness Detection, and Per-Server Sync

> [!info] Metadata
> **Author**: retroactive spec, authored during the `.local/specs/` → `specs/` migration and gap
> analysis (no single originating commit/PR — this documents already-shipped, working
> functionality)
> **Type**: core subsystem
> **Priority**: P1 (retroactive — reflects centrality, not an open defect)

> [!success] Resolution
> This is a retroactive spec: `crates/mcpls-core/src/bridge/state.rs`'s `DocumentTracker` and
> `DocumentState` already implement everything described below. Resource-limit enforcement
> (`max_documents`/`max_file_size`) is already covered by
> [[bridge/005-expose-document-tracker-limits/spec|spec bridge/005]]; this spec covers the *rest* of
> `DocumentTracker`'s responsibilities: content caching, disk-staleness detection, per-path
> concurrency control, and per-server sync-version tracking, none of which any existing spec
> documents.

## 1. Overview

### Problem Statement

Every MCP tool that needs a language server to have analyzed a file (hover, definition,
diagnostics, etc.) must first ensure that server has received the file's current content via LSP's
`textDocument/didOpen` (first time) or `textDocument/didChange` (subsequent edits). Naively
re-reading the file from disk on every single tool call and always sending `didChange` would be
both slow (a disk read per call) and wrong (a file can have unsaved, in-memory-only edits — e.g.
an MCP host's own edit buffer — that must not be silently overwritten by a stale disk read).

`DocumentTracker` solves three distinct problems at once:

1. **Content caching with disk-staleness detection**: trust a cached in-memory copy of a file's
   content when a disk stat proves it hasn't changed, but re-read and re-verify by content compare
   when the stat is ambiguous (an mtime too recent to trust, given filesystem mtime granularity
   varies from 1s (APFS/ext3) to 2s (FAT/exFAT)) or has changed outright.
2. **Per-path concurrency control**: two tool calls touching the *same* file must not race each
   other's read-modify-sync sequence, but calls touching *different* files must never block on each
   other.
3. **Per-server sync-version tracking**: a single document can be routed to different servers for
   different tools (see [[mcp/001-mcp-tool-surface-and-routing/spec|spec mcp/001]]'s `ToolRouter`), and
   each server needs its own `didOpen`/`didChange` history — one server having seen a document does
   not mean another one has.

### Goal

Every tool call that touches a file gets that file's true current content synced to the correct
LSP server(s), using the cheapest correct path available (trust the in-memory cache when a disk
stat proves nothing has changed; otherwise re-verify), without two concurrent calls for the same
path corrupting each other's view of the document's version, and without one server's sync history
being confused with another's.

### Out of Scope

- Resource-limit enforcement (`max_documents`/`max_file_size`) — already
  [[bridge/005-expose-document-tracker-limits/spec|spec bridge/005]].
- What happens to a server's sync history when that server crashes and is respawned — already
  covered by [[lsp/001-lsp-server-lifecycle-and-respawn/spec|spec lsp/001]]'s FR-009
  (`DocumentTracker::forget_server`), which this spec's data structures support but does not
  itself specify the trigger for.
- Position/range conversion of any content this tracker holds — [[bridge/001-position-encoding-layer/spec|spec bridge/001]].

## 2. User Stories

### US-001: Repeated tool calls against an unchanged file don't re-read it from disk

AS AN AI agent issuing several tool calls (hover, then definition, then references) against the
same file within a short window, with no edits in between
I WANT the file's content read from disk only once, not once per tool call
SO THAT a burst of tool calls against the same file stays fast

**Acceptance criteria:**
```
GIVEN a file whose (mtime, size) has not changed since it was last read
WHEN a second tool call ensures the file is open shortly after the first
THEN the cached in-memory content is trusted without a second disk read, once the mtime is
     old enough (past MTIME_GRANULARITY) to be trusted as settled
```

### US-002: An externally-edited file is detected even when mtime granularity can't be trusted

AS A developer whose editor, a formatter, or `git checkout`/`stash` rewrites a file between two
mcpls tool calls happening within the same filesystem mtime tick
I WANT the tracker to still detect the change rather than serving stale cached content
SO THAT a rapid external edit isn't silently missed just because the OS's mtime resolution wasn't
fine enough to distinguish before/after

**Acceptance criteria:**
```
GIVEN a file rewritten externally within the same mtime granularity window as the tracker's last
     read
WHEN the next tool call ensures the file is open
THEN the tracker re-reads and content-compares rather than trusting the stat alone, and picks up
     the new content if it actually changed
```

### US-003: Two servers routed to the same document each get correct didOpen/didChange history

AS A developer using two servers for the same language (e.g. pyright for hover, a linter LSP for
diagnostics, routed per [[mcp/001-mcp-tool-surface-and-routing/spec|spec mcp/001]])
I WANT each server to receive its own correct `didOpen` (first contact) vs. `didChange`
(subsequent edit) sequence for a shared document
SO THAT one server having already seen a document doesn't cause the other to incorrectly skip its
own required `didOpen`

**Acceptance criteria:**
```
GIVEN a document already synced to server A but never sent to server B
WHEN a tool call routes to server B for the same document
THEN server B receives didOpen (not didChange), independent of server A's sync history
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHALL track, per open document, its URI, language ID, monotonically-increasing version, in-memory content, and a per-server map of the last version synced to each server | must |
| FR-002 | WHEN a document's on-disk `(mtime, size)` matches the last-observed snapshot AND that mtime is old enough (past a filesystem mtime-granularity margin) to be trusted as settled THE SYSTEM SHALL trust the cached in-memory content without re-reading the file | must |
| FR-003 | WHEN a document's on-disk `(mtime, size)` does not match the last-observed snapshot, OR the mtime is not yet settled THE SYSTEM SHALL re-read the file and compare content directly rather than trusting the stat alone | must |
| FR-004 | WHEN a not-yet-settled stat is observed repeatedly with an unchanged `(mtime, size)` THE SYSTEM SHALL debounce the (comparatively expensive) content re-read within a bounded window, while the disk stat itself is never debounced | must |
| FR-005 | WHEN a document's content is updated via a local (non-disk) edit (`update`) THE SYSTEM SHALL bump its version, replace its content, and clear its disk provenance (so the next `ensure_open` always re-verifies by content compare rather than trusting a stale stat) | must |
| FR-006 | THE SYSTEM SHALL serialize `ensure_open`/`update` calls for the *same* path via a per-path lock, while calls for *different* paths never block on each other | must |
| FR-007 | THE SYSTEM SHALL track, per (document, server) pair, the last version synced to that server, so a server that has never seen a document receives `didOpen` and one that has already seen an earlier version receives `didChange` | must |
| FR-008 | THE SYSTEM SHALL provide `forget_server(server_id)` to clear one server's entire sync history across all tracked documents, without affecting any other server's sync history for the same documents | must |
| FR-009 | THE SYSTEM SHALL provide `line_text(path, line)`, reading from in-memory tracked content (not disk), for range/position conversion consumers ([[bridge/001-position-encoding-layer/spec|spec bridge/001]]) that need a document's current line text without an extra disk read | must |
| FR-010 | WHEN `DocumentTracker::open` is called for an already-tracked path THE SYSTEM SHALL unconditionally replace the entry, resetting version to 1 and clearing all servers' sync history for that path | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | The common "file unchanged" path must not re-read file content — a stat comparison alone must suffice once the mtime is settled |
| NFR-002 | Correctness | `DocumentState.version` must be monotonically non-decreasing for the lifetime of a single tracked entry — every mutation goes through a dedicated method, never a partial field write |
| NFR-003 | Concurrency | `ensure_open`'s per-path lock is not reentrant; no code path may attempt to acquire it recursively (this must be documented plainly, since violating it self-deadlocks with no panic and no timeout to signal it) |
| NFR-004 | Portability | The mtime-granularity margin must cover the coarsest common filesystem granularity in realistic deployment targets (FAT/exFAT at 2s), not just the finest (APFS/ext3/HFS+ at ~1s or better) |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|-----------------|
| `DocumentState` | Tracked state for one open document | `uri`, `language_id`, `version: i32` (monotonic), `content: String`, `disk: Option<DiskSync>`, `synced: HashMap<ServerId, i32>` |
| `DiskSync` | Snapshot of a document's on-disk filesystem state as of its last verified read | `mtime: Option<SystemTime>`, `size: u64`, `mtime_settled: bool`, `content_checked_at: Instant` (excluded from equality — a debounce timer, not logical state) |
| `DocumentTracker` | Workspace-wide tracker, shareable behind a plain `Arc` with no outer lock | `documents: StdMutex<HashMap<PathBuf, DocumentState>>`, `path_locks: StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>`, `generations: StdMutex<HashMap<ServerId, u64>>` |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|--------------------|
| File's `(mtime, size)` unchanged and mtime is settled | Trust cache, no re-read (FR-002) |
| File's `(mtime, size)` unchanged but mtime is *not yet* settled (rewritten within the granularity window) | Re-read and content-compare, debounced within `DISK_CHECK_DEBOUNCE` (250ms) for repeated calls against the same unsettled snapshot |
| File's `(mtime, size)` changed on every stat (rapid external rewrites) | Never debounced — each such call already disagrees with the cached snapshot, so it always takes the immediate re-read path |
| Filesystem does not report mtime at all | Entry is never treated as settled; forces a content re-read outside the debounce window every time |
| Local edit applied via `update` | Version bumps, disk provenance cleared — next `ensure_open` always re-verifies by content compare, since the new content's disk provenance is unknown |
| `update` called from within a task that already holds the same path's lock | Self-deadlock (no panic, no timeout) — documented as a hard invariant, not handled defensively |
| A document reopened via `open` while already tracked | Entry unconditionally replaced: version resets to 1, all servers' sync history for that path cleared |
| Server A has synced a document, server B has not | Server B's next `ensure_open` for that document sends `didOpen`, independent of server A's already-synced state |
| A server is forgotten via `forget_server` | Only that server's sync history is cleared across every tracked document; other servers' sync history is untouched |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | `cargo nextest run -E 'package(mcpls-core) and test(state)'` | All existing `DocumentTracker`/`DocumentState` unit tests pass |
| SC-002 | Repeated `ensure_open` calls against an unchanged file within one process lifetime | Disk content read at most once per genuine change, verified by stat-only fast path being taken on unchanged subsequent calls |
| SC-003 | Concurrent `ensure_open` calls for two different paths | Neither blocks on the other (per-path locking, not a single tracker-wide lock) |

## 8. Agent Boundaries

### Always (without asking)
- Route every document-content mutation through `DocumentState`'s dedicated methods
  (`apply_local_edit`, `commit_reload`, `set_disk`, `mark_synced`, `forget_server`) rather than a
  partial field write, preserving the monotonic-version invariant (NFR-002)
- Preserve the per-path (not global) locking granularity in `ensure_open`/`update`

### Ask First
- Changing `MTIME_GRANULARITY` or `DISK_CHECK_DEBOUNCE` — both are tuned against real filesystem
  behavior (FAT/exFAT vs. APFS/ext3), not arbitrary constants
- Adding an eviction/LRU policy for tracked documents — explicitly out of scope per
  [[bridge/005-expose-document-tracker-limits/spec|spec bridge/005]]'s Out of Scope section, which this spec's
  `DocumentTracker` also respects

### Never
- Acquire a path's `lock_path` guard reentrantly from within a call that already holds it — this
  self-deadlocks with no panic and no timeout (NFR-003)
- Trust a disk stat match without checking `mtime_settled` — this is exactly the racy-rewrite gap
  US-002 exists to close

## 9. Open Questions

None — this is a retroactive spec documenting stable, already-shipped, well-tested behavior.

## 10. See Also

- [[constitution]] — project principles (not yet created for this project)
- [[MOC-specs]] — all specifications
- [[bridge/005-expose-document-tracker-limits/spec|spec bridge/005]] — `ResourceLimits`
  (`max_documents`/`max_file_size`) enforcement, the sibling concern this spec does not duplicate
- [[bridge/001-position-encoding-layer/spec|spec bridge/001]] — consumer of `DocumentTracker::line_text`
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|spec lsp/001]] — `forget_server`'s trigger (a respawned
  server has no memory of previously-open documents)
- [[mcp/001-mcp-tool-surface-and-routing/spec|spec mcp/001]] — `ToolRouter`, the mechanism that can route
  one document to different servers for different tools, motivating per-server sync tracking
- `crates/mcpls-core/src/bridge/state.rs` — `DocumentTracker`, `DocumentState`, `DiskSync`
