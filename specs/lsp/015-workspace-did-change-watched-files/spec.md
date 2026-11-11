---
aliases:
  - workspace/didChangeWatchedFiles
  - Watched file change notifications
  - Client-side file watching
tags:
  - sdd
  - spec
  - enhancement
  - lsp-bridge
  - file-watching
  - competitor-gap
created: 2026-11-06
status: draft
related:
  - "[[constitution]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001-lsp-server-lifecycle-and-respawn]]"
  - "[[lsp/010-workspace-configuration-push/spec|lsp/010-workspace-configuration-push]]"
  - "[[mcp/008-manual-lsp-server-restart/spec|mcp/008-manual-lsp-server-restart]]"
  - "[[mcp/014-tools-list-payload-size/spec|mcp/014-tools-list-payload-size]]"
  - "[[bridge/002-document-tracker-synchronization/spec|bridge/002-document-tracker-synchronization]]"
  - "[[bridge/010-workspace-containment-single-predicate/spec|bridge/010-workspace-containment-single-predicate]]"
  - "[[runtime/003-workspace-supplied-code-execution/spec|runtime/003-workspace-supplied-code-execution]]"
---

# Feature: Report on-disk file changes to language servers with `workspace/didChangeWatchedFiles`

> [!info] Metadata
> **Type**: enhancement (competitor gap)
> **Priority**: P3
> **Author**: Andrei G.
> **Source**: continuous-improvement competitor gap analysis
> **Issue**: #889

> [!abstract]
> mcpls re-syncs a document it has open when its `(mtime, size)` changes, but it never tells a
> language server about files created, deleted or changed on disk that mcpls has not opened:
> no `workspace/didChangeWatchedFiles` is ever sent, and the client capabilities disable dynamic
> registration, so servers are told they may not register file watchers. A server that relies on
> its client for file watching then serves stale cross-file results (new files invisible, deleted
> files still present, manifest changes unapplied) until it is restarted with `restart_server`.
> This spec requires that mcpls can report on-disk changes to the servers that asked for them,
> bounded in memory and event rate, contained to the workspace, per-server routed, and disabled
> or narrowed in untrusted-workspace mode. The product shape (explicit tool, automatic watcher or
> both) is an open decision (Section 9).

## 1. Overview

### Problem Statement

`crates/mcpls-core/src/bridge/state.rs` sends `textDocument/didOpen` lazily on the first access
to a file and, for a document it already holds open, compares `(mtime, size)` on the next access
and sends `textDocument/didChange` when they differ (with the mtime-settled rule of
[[bridge/002-document-tracker-synchronization/spec|bridge/002]]). That covers only documents mcpls
itself opened.

`workspace/didChangeWatchedFiles` appears nowhere in `crates/mcpls-core/src`. The client
capabilities built in `crates/mcpls-core/src/lsp/lifecycle.rs` set `workspace` to
`workspace_folders: Some(true)` and an optional `configuration`, and every `dynamic_registration`
is `Some(false)`. A server therefore sees a client that does not support
`workspace/didChangeWatchedFiles` registration, and mcpls answers an unsolicited
`client/registerCapability` with a null result and ignores its content
(`crates/mcpls-core/src/lsp/client.rs`, `server_request_result`).

The files an AI client changes through its own editing tools are exactly the files mcpls never
opens: new source files, deleted files, and build inputs such as `compile_commands.json`,
`tsconfig.json`, `go.mod` and `Cargo.toml`. For servers that depend on the client for file
watching, those changes are invisible. Other servers fall back to their own watchers and are
unaffected. Which servers fall in which group is not established here
(`[NEEDS CLARIFICATION: verify per server (gopls, clangd, pyright, rust-analyzer, typescript-language-server) which ones need client-provided watching]`).

The only current remedy is `restart_server`
([[mcp/008-manual-lsp-server-restart/spec|mcp/008]]), which discards the whole server state and
re-indexes. That spec left "notifying a server of a changed manifest without restarting it
(`workspace/didChangeWatchedFiles`, `workspace/didChangeConfiguration`)" out of scope as tracked
separately if wanted. `workspace/didChangeConfiguration` has since shipped
([[lsp/010-workspace-configuration-push/spec|lsp/010]]); the file-watching half has not.

Other LSP-to-MCP bridges forward file-change notifications, in at least one case through both an
explicit tool and an automatic debounced watcher of the workspace root. This is a parity gap, not
a conformance failure: dynamic registration is optional in the protocol.

> [!warning] Server-side staleness is not demonstrated
> A live reproduction against clangd (a `compile_commands.json` change, with and without a
> notification) was attempted, but clangd's background index did not answer `workspace/symbol`
> within the test window in either arm. The gap itself (no capability, no notification) is
> established from code; the user-visible impact per server is not. Section 7 carries a
> verification plan, and the priority should be re-assessed from its result (SC-001).

### Goal

When files change on disk outside mcpls, every connected server that asked to be told about such
changes receives a correct, bounded, workspace-contained `workspace/didChangeWatchedFiles`
notification, so its cross-file results do not stay stale until a restart.

### Out of Scope

- Replacing or changing the `(mtime, size)` re-sync of open documents
  ([[bridge/002-document-tracker-synchronization/spec|bridge/002]]).
- `workspace/willRenameFiles`, `workspace/didRenameFiles`, `workspace/didCreateFiles`,
  `workspace/didDeleteFiles` (file-operation notifications): a different capability negotiated by
  the server, tracked separately if wanted.
- Applying edits to disk: mcpls does not write files; this feature only reports changes made by
  others.
- Changing server configuration on a manifest change (`workspace/didChangeConfiguration` is
  settled in [[lsp/010-workspace-configuration-push/spec|lsp/010]]).
- Watching paths outside the workspace roots, including symlink targets that escape them.
- Automatic restart of a server when a notification is not honored; `restart_server` remains the
  fallback.
- Choosing a filesystem watcher crate or the tool name: decisions for the plan.

## 2. User Stories

### US-001: A new file becomes visible without a restart

AS AN AI coding agent that created a source file with an editing tool
I WANT the language server to learn about the new file
SO THAT `workspace_symbol_search`, `get_references` and diagnostics include it without a restart.

**Acceptance criteria:**
```
GIVEN a running server that registered a watcher matching **/*.ext
  AND an indexed workspace
WHEN a new matching file is created on disk by a process other than mcpls
THEN the server receives workspace/didChangeWatchedFiles with one Created change for that file
  AND a following cross-file query that depends on the file reflects it
```

### US-002: A deleted file stops appearing in results

AS AN AI coding agent that deleted a file
I WANT the server to drop it
SO THAT references and symbols do not point at a file that no longer exists.

**Acceptance criteria:**
```
GIVEN a server that registered a watcher matching the file
WHEN the file is deleted on disk
THEN the server receives one Deleted change for it
  AND a document mcpls holds open for it is closed with textDocument/didClose
```

### US-003: A manifest change is applied without restarting the server

AS AN AI coding agent that edited `Cargo.toml`, `go.mod`, `tsconfig.json` or `compile_commands.json`
I WANT the server to be told the build input changed
SO THAT it reloads the project model without losing its whole index.

**Acceptance criteria:**
```
GIVEN a server that registered a watcher matching the manifest
WHEN the manifest is modified on disk
THEN the server receives one Changed change for it, after the write has settled
```

### US-004: A server that does not want notifications receives none

AS AN operator
I WANT only servers that registered file watchers to be notified, and only about matching files
SO THAT servers with their own watchers, and unrelated servers in a multi-server workspace, are not
flooded or confused.

**Acceptance criteria:**
```
GIVEN two servers, one that registered a watcher for **/*.py and one that registered none
WHEN a .py file changes on disk
THEN only the first server receives a notification, and only for files its registration matches
```

### US-005: An untrusted workspace cannot use file watching to widen its reach

AS A user running mcpls with `--workspace-trust untrusted`
I WANT file-change reporting to stay inside the boundaries of that mode
SO THAT a hostile workspace gains no new way to influence a server or the host.

**Acceptance criteria:**
```
GIVEN mcpls started with --workspace-trust untrusted
WHEN files change on disk, including through a symlink that points outside the workspace
THEN no change outside the workspace roots is ever reported
  AND the behavior of the feature in this mode is the one decided in Section 9
```

### US-006: An event storm does not exhaust memory or flood a server

AS AN operator of a long session
I WANT a bulk change (a branch switch, a package install, a build writing thousands of files) to be
coalesced and bounded
SO THAT mcpls memory stays flat and the server receives a few bounded notifications.

**Acceptance criteria:**
```
GIVEN a burst of file-system events far larger than the configured bounds
WHEN the burst is processed
THEN memory held for pending events never exceeds the bound
  AND the server receives deduplicated notifications, each within the per-notification bound
  AND the loss of precision, if any, is recorded rather than silent
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE SYSTEM SHALL be able to send `workspace/didChangeWatchedFiles` to a server, carrying changes of a closed typed kind (created, changed, deleted) that is serialized to the LSP numeric value only at the protocol boundary | must |
| FR-002 | WHEN a server has not registered a `workspace/didChangeWatchedFiles` watcher THE SYSTEM SHALL NOT send it that notification | must |
| FR-003 | WHEN a server registers or unregisters watchers through `client/registerCapability` and `client/unregisterCapability` THE SYSTEM SHALL record the registration per server and SHALL answer the request as the protocol requires | must |
| FR-004 | THE SYSTEM SHALL advertise `workspace.didChangeWatchedFiles` with `dynamicRegistration` only when the feature is able to honor registrations for that server | must |
| FR-005 | WHEN a change matches a server's registered glob pattern and watch kind THE SYSTEM SHALL include it in that server's notification, and SHALL NOT include a change that matches no registration of that server | must |
| FR-006 | THE SYSTEM SHALL support both plain string glob patterns and relative patterns (base URI plus pattern) of LSP 3.17 in registrations | must |
| FR-007 | THE SYSTEM SHALL report only paths inside the workspace roots, using the single containment predicate of [[bridge/010-workspace-containment-single-predicate/spec\|bridge/010]], including its symlink and root-alias handling | must |
| FR-008 | WHEN several changes for one file arrive within the settle window THE SYSTEM SHALL coalesce them into one change using documented rules (create then delete cancels, delete then create is changed, repeated changes are one) | must |
| FR-009 | THE SYSTEM SHALL bound pending events per server, events per notification, and path length, and SHALL record any event dropped for those bounds in typed state rather than discard it silently | must |
| FR-010 | WHEN pending events exceed the bound THE SYSTEM SHALL apply the overflow policy decided in Section 9 (the protocol has no rescan event) | must |
| FR-011 | THE SYSTEM SHALL route each notification only to the servers whose registration matched, and a failure to deliver to one server SHALL NOT affect another | must |
| FR-012 | WHEN a server is restarted or respawned THE SYSTEM SHALL drop its recorded registrations and pending events, and SHALL send nothing to the replacement until it registers again | must |
| FR-013 | WHEN a deleted file is a document mcpls holds open THE SYSTEM SHALL close it in the document tracker, and WHEN a changed file is open THE SYSTEM SHALL follow the ordering decided in Section 9 relative to the existing `didChange` re-sync | must |
| FR-014 | WHEN `--workspace-trust untrusted` is active THE SYSTEM SHALL apply the untrusted-mode behavior decided in Section 9, and no environment variable or config key SHALL enable or relax it | must |
| FR-015 | THE feature SHALL work on Linux, macOS and Windows, including case-insensitive file systems, and SHALL NOT depend on event ordering or timing guarantees that differ between platform watcher backends | must |
| FR-016 | WHERE an explicit tool is part of the decided shape THE SYSTEM SHALL take typed, bounded, workspace-contained paths with a typed change kind, reject unknown arguments, and fit the `tools/list` size budget of [[mcp/014-tools-list-payload-size/spec\|mcp/014]] | should |
| FR-017 | WHERE an automatic watcher is part of the decided shape THE SYSTEM SHALL watch only registered roots, ignore the mcpls-internal and conventional build-output directories per the decision in Section 9, and stop it on shutdown without leaking threads or handles | should |
| FR-018 | THE log output for dropped, coalesced or rejected events SHALL be rate limited and SHALL NOT include server-controlled or file-content data | must |
| FR-019 | `get_tool_support` or an equivalent surface SHOULD report per server whether file-change notification is active, so an agent knows whether `restart_server` is still needed after a manifest change | should |
| FR-020 | Regression and integration tests SHALL cover registration, matching, coalescing, bounds, containment, respawn and the untrusted-mode behavior, using a fake server that registers watchers | must |
| FR-021 | THE testing playbooks under `.local/testing/` SHALL gain a file-watching case and the coverage status for the lifecycle subsystem SHALL be reset | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | Change kind, watch kind (a closed flag set, not a raw integer), registration, glob pattern, watched path and bounded counts are distinct types; raw `u8`, `i32` and `String` values appear only at the serde boundary, per [[constitution]] and the user type-safety rule |
| NFR-002 | Memory | Pending events, registrations per server, and glob patterns per registration have fixed upper bounds; memory for a burst is constant in burst size |
| NFR-003 | Event rate | Notifications per server are rate limited (settle window plus a maximum notifications per interval); the settle window default is a plan decision informed by the debounce used for open-document re-sync |
| NFR-004 | Security | No path outside the workspace roots is reported; no symlink is followed out of a root; path and pattern lengths are bounded; server-supplied glob patterns cannot cause unbounded matching time |
| NFR-005 | Untrusted mode | Behavior in `--workspace-trust untrusted` is explicit and never more permissive than trusted mode (Section 9) |
| NFR-006 | Portability | Tests do not rely on wall-clock timing, platform-specific event ordering or symlink availability on Windows; fake executables use `.exe` on Windows |
| NFR-007 | Isolation | A slow or failed server does not block event processing for another server (graceful degradation, [[constitution]] Section I) |
| NFR-008 | Concurrency | No registry or tracker lock is held across an `await` on an LSP write |
| NFR-009 | Compatibility | Any new tool or capability field is recorded in `CHANGELOG.md` with its PR link; pre-1.0 compatibility is not a constraint |
| NFR-010 | Dependencies | A new dependency requires justification in `CHANGELOG.md` and the PR; the `fuzz/Cargo.lock` refresh rule applies |
| NFR-011 | Idle cost | With no registered watcher and no explicit call, the feature adds no thread, handle or per-event work |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| File change | One reported on-disk change | typed kind (created, changed, deleted), workspace-contained path (bounded URI) |
| Watch kind set | Which change kinds a watcher wants | closed flag set (create, change, delete), default all |
| Watcher registration | One server-registered file watcher | server, registration id, glob pattern (plain or relative), watch kind set |
| Registration table | Per-server set of registrations | owner, bounded count, cleared on respawn |
| Pending change set | Coalesced, bounded events awaiting the settle window | server, path to kind, bound, dropped marker |
| Dropped-events marker | Typed record that events were lost to a bound | server, set or clear, policy outcome (Section 9) |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| File created then deleted within the settle window | No notification (FR-008) |
| File deleted then re-created within the window | One changed (FR-008) |
| Atomic save by an editor (write temp, rename over target) | One changed for the target; the temp file reported only if a registration matches it |
| Directory deleted or renamed | Per-file deletions are not enumerable after the fact: behavior per Section 9; a registration that watches directories sees the directory change as the protocol defines |
| Burst far above the bound (branch switch, install, build) | Coalesce, then apply the overflow policy; mark dropped events (FR-009, FR-010) |
| Path outside the workspace roots, or a symlink pointing out | Never reported (FR-007) |
| Case-only rename on a case-insensitive file system | Reported with the spelling the file system returns; delete plus create semantics per platform, normalized by FR-008 |
| Same file open in the document tracker and changed on disk | Ordering relative to `didChange` per Section 9; never two conflicting views of the file |
| Server registers a pattern with a base URI outside its workspace | Registration accepted but never matches an out-of-workspace path (FR-007) |
| Server registers a pathological or very long glob | Rejected or truncated per bound (NFR-004); the request is still answered |
| `client/unregisterCapability` for an unknown id | Answered per protocol, no state change |
| Server dies while events are pending | Pending events dropped with the server (FR-012); other servers unaffected |
| Server restarted with `restart_server` | Registrations cleared; nothing sent until it registers again (FR-012) |
| Watcher backend fails to start (inotify limit reached, permissions) | Feature degrades for that root with a structured, rate-limited signal; other roots and servers continue |
| Network or removable file systems that emit no events | Out of the feature's guarantee; `restart_server` remains the fallback |
| Untrusted workspace | Per Section 9 (FR-014) |
| Windows long or UNC paths | Handled through the existing path normalization of [[bridge/010-workspace-containment-single-predicate/spec\|bridge/010]] |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Verification plan: for each of gopls, clangd, pyright, rust-analyzer and typescript-language-server, run a created-file, deleted-file and manifest-change scenario with and without the notification and record whether the server's answer differs | A table of per-server results; the priority of this spec is re-assessed from it |
| SC-002 | Fake server that registers `**/*.fk` | Receives exactly one correctly typed notification per settled change and none for unmatched files |
| SC-003 | Fake server that registers nothing | Receives no notification in any scenario (FR-002) |
| SC-004 | Burst of N events, N far above the pending bound | Peak memory for pending events within the bound; notifications each within the per-notification bound |
| SC-005 | Files and symlinks outside the workspace roots | Zero reported |
| SC-006 | Respawn and `restart_server` | Registrations and pending events cleared; none sent to the replacement before it registers |
| SC-007 | Untrusted mode | Behavior matches the Section 9 decision, with a test |
| SC-008 | CI on Linux, macOS and Windows | Feature tests pass without wall-clock sleeps beyond the injected clock |
| SC-009 | Existing tools with no registered watcher | Output and `tools/list` payload unchanged (NFR-011), unless a tool is added within the budget |

## 8. Agent Boundaries

### Always (without asking)
- Reuse the single containment predicate, the bounded URI type, the existing rate-limited WARN mechanism and the injected clock used by the tracker.
- Reuse existing typed ids (`ServerId`) and routing for per-server delivery.
- Run the full pre-commit suite, refresh `fuzz/Cargo.lock` if `mcpls-core` dependencies change, and update `CHANGELOG.md` with the PR link.
- Update the `.local/testing/` playbooks and coverage status for any behavior change.

### Ask First
- Choosing explicit tool, automatic watcher, or both, and the tool name and arguments (Section 9).
- Adding a filesystem watcher dependency, or a new tool that changes the `tools/list` budget.
- Advertising `dynamicRegistration` for `workspace.didChangeWatchedFiles`.
- The overflow policy and the untrusted-mode behavior.
- Any config key or CLI flag for this feature.

### Never
- Send a notification to a server that did not register a watcher.
- Report or watch a path outside the workspace roots, or follow a symlink out of one.
- Add an environment variable or config key that relaxes untrusted-mode behavior.
- Hold unbounded events, registrations or patterns, or let a server-supplied glob run unbounded.
- Log server-controlled or file-content data.
- Edit source files, manifests or CI files as part of writing this spec.

## 9. Open Questions

- [NEEDS CLARIFICATION: Product shape. (a) An explicit MCP tool the agent calls after editing files (deterministic, no background thread, costs an agent step and a `tools/list` entry); (b) an automatic debounced watcher of the workspace roots (no agent step, adds a platform-dependent background component and a dependency); (c) both, with the tool as the fallback for platforms or file systems where watching fails. Which is the MVP?]
- [NEEDS CLARIFICATION: Dynamic registration. Advertise `workspace.didChangeWatchedFiles.dynamicRegistration: true` and honor `client/registerCapability` (precise, glob-aware), or send changes to every server without registration gating (simple, but contradicts FR-002 and floods servers that watch for themselves). How should a server that never registers but would benefit (static-watch servers) be handled?]
- [NEEDS CLARIFICATION: Which watcher crate, if any, and whether the explicit tool alone is enough for the MVP. A decision for the later plan, bounded by the constitution (new dependencies need justification) and the Windows, macOS and Linux requirement.]
- [NEEDS CLARIFICATION: Which language servers need client-provided watching and how stale their answers get (SC-001). The priority and the MVP shape depend on this.]
- [NEEDS CLARIFICATION: Untrusted-workspace mode. Options: disabled entirely; enabled only for allowlisted servers and only for the explicit tool; enabled like trusted mode but with a stricter containment and no automatic watcher. A hostile workspace can create many files, so an automatic watcher widens the event-driven attack surface.]
- [NEEDS CLARIFICATION: Overflow policy. The protocol has no rescan event. Options: send a bounded sample plus a typed dropped marker; send changed for every file mcpls tracks as open; restart the server after repeated overflow; degrade to the explicit tool only.]
- [NEEDS CLARIFICATION: Interaction with open documents. Servers commonly prefer didChange for open files; should a watched-file change for a document mcpls holds open be suppressed, sent alongside the `didChange` re-sync, or sent first?]
- [NEEDS CLARIFICATION: Directory events. A deleted or renamed directory cannot be enumerated afterward; report the directory, synthesize per-file deletions from the tracker's known files, or leave to the server.]
- [NEEDS CLARIFICATION: Ignore rules. Should the watcher and the explicit tool skip `.git`, `target`, `node_modules` and other build-output directories by default, honoring `.gitignore`, and is that list a typed config?]
- [NEEDS CLARIFICATION: Settle window and notification bounds (events per notification, pending events per server, notifications per interval). Defaults to be derived from measurement in the plan.]
- [NEEDS CLARIFICATION: Surface for FR-019: a field in `get_tool_support`, in server status, or none.]

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] — message loop, lifecycle and respawn
- [[lsp/010-workspace-configuration-push/spec|lsp/010]] — the configuration half of workspace change notification
- [[mcp/008-manual-lsp-server-restart/spec|mcp/008]] — the current remedy and the deferred scope this spec picks up
- [[mcp/014-tools-list-payload-size/spec|mcp/014]] — `tools/list` size budget for a new tool
- [[bridge/002-document-tracker-synchronization/spec|bridge/002]] — open-document `(mtime, size)` re-sync and settle rule
- [[bridge/010-workspace-containment-single-predicate/spec|bridge/010]] — the containment predicate
- [[runtime/003-workspace-supplied-code-execution/spec|runtime/003]] — untrusted-workspace mode
- Code: `crates/mcpls-core/src/lsp/lifecycle.rs` (client capabilities), `crates/mcpls-core/src/lsp/client.rs` (`server_request_result`), `crates/mcpls-core/src/bridge/state.rs`, `crates/mcpls-core/src/bridge/workspace_roots.rs`
- [LSP 3.17: workspace/didChangeWatchedFiles](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#workspace_didChangeWatchedFiles)
