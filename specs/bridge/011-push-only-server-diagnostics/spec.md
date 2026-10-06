---
aliases:
  - Push-only server diagnostics
  - get_diagnostics on a server without diagnosticProvider
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
  - "[[bridge/004-get-diagnostics-flycheck-gap/spec|bridge/004-get-diagnostics-flycheck-gap]]"
  - "[[bridge/009-diagnostics-subscription-staleness/spec|bridge/009-diagnostics-subscription-staleness]]"
  - "[[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006-lsp-indexing-readiness-gate]]"
  - "[[lsp/012-client-publish-diagnostics-capability/spec|lsp/012-client-publish-diagnostics-capability]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp/002-mcp-resources-diagnostics]]"
  - "[[mcp/005-tool-capability-discoverability/spec|mcp/005-tool-capability-discoverability]]"
---

# Feature: `get_diagnostics` answers from the push cache when the server has no pull provider

> [!info] Metadata
> **Type**: bug
> **Priority**: P2
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle, live-test finding
> **Issue**: #666

> [!abstract]
> `get_diagnostics` always sends `textDocument/diagnostic`. A server that publishes diagnostics but
> has no pull provider answers `-32601`, and mcpls turns that into an error whenever the push cache
> has nothing to show, including for a clean file the server correctly reported as clean. This spec
> makes `get_diagnostics` honest on push-only servers: it does not send a request the server never
> advertised, answers from the push cache, says that the answer is push-derived, and tells "clean"
> apart from "nothing published yet".

## 1. Overview

### Problem Statement

`Translator::handle_validated_diagnostics`
(`crates/mcpls-core/src/bridge/translator/diagnostics.rs`) sends `textDocument/diagnostic` for
every call and merges the response with the push cache. When the pull request fails it returns the
cache-only result only if that result is non-empty; otherwise it propagates the error. The request
is sent without checking `diagnosticProvider`, and the tool is not capability-gated.

**Observed with a fake push-only server** (no `diagnosticProvider`; on `didOpen` it publishes `[]`
for a clean file and one diagnostic for another; it answers `textDocument/diagnostic` with
`-32601`):

- `get_diagnostics` on the clean file returns the JSON-RPC error `-32603`
  `LSP server error: -32601 - Unhandled method textDocument/diagnostic` on every call, although
  `get_cached_diagnostics` returns `{"diagnostics": []}`, because the server did publish an empty
  list.
- On the file with a diagnostic, the first call right after open also errors (the push has not been
  cached yet); the second call succeeds from the cache.
- Every call logs an `ERROR` line `LSP error response ... textDocument/diagnostic`, so ordinary use
  floods the error log.
- `get_tool_support` reports coverage `all` for `get_diagnostics` although the server advertises no
  `diagnosticProvider`.

typescript-language-server (5.1.3 and 6.0.1) behaves the same way: it has no pull provider. Its
diagnostics are currently absent altogether because the client does not declare the capability that
makes it publish ([[lsp/012-client-publish-diagnostics-capability/spec|lsp/012]]); once that is
fixed, this spec is what makes `get_diagnostics` usable against it.

**Why it matters.** An agent that calls `get_diagnostics` on a clean TypeScript file receives an
error instead of "no problems". A well-behaved agent retries or switches tools; a less careful one
treats the error as noise and the file as fine. The cache already holds the right answer, and the
empty list is a real answer: the server said the file is clean. Today the code cannot tell that
apart from "nothing arrived yet", and both are turned into a failure or an empty success.

### Goal

For a routed server that does not advertise `diagnosticProvider`, `get_diagnostics` sends no pull
request, answers from the push cache, flags the answer as push-derived, logs no error, and
distinguishes a published-empty (clean) file from a file for which no publish has arrived yet.

### Out of Scope

- Making the server publish diagnostics (the client capability is
  [[lsp/012-client-publish-diagnostics-capability/spec|lsp/012]]).
- Merge semantics for servers that have both pull and push (settled in
  [[bridge/004-get-diagnostics-flycheck-gap/spec|bridge/004]]).
- Staleness of cached entries after edits
  ([[bridge/009-diagnostics-subscription-staleness/spec|bridge/009]]).
- Waiting for a publish (a poll loop or a timed wait) as the way to resolve "pending"; reporting
  the state is in scope, blocking on it is a plan-time decision.
- Changing `get_cached_diagnostics` semantics beyond what the new "pending" state requires.
- Technical design: recorded in a plan after this spec is approved.

## 2. User Stories

### US-001: Clean file on a push-only server is clean

AS AN AI coding agent
I WANT `get_diagnostics` on a file that a push-only server reported as clean to say the file is
clean
SO THAT I do not treat an error as the answer or retry forever.

**Acceptance criteria:**
```
GIVEN a routed server without diagnosticProvider that has published an empty list for a file
WHEN get_diagnostics is called for that file
THEN the response is a successful empty diagnostics list
  AND no textDocument/diagnostic request was sent
  AND no ERROR line was logged
```

### US-002: A file with a diagnostic is reported on the first call after the publish

AS AN AI coding agent
I WANT a diagnostic the server has published to be returned
SO THAT I see the error on the first call that can know about it.

**Acceptance criteria:**
```
GIVEN a push-only server that has published one diagnostic for a file
WHEN get_diagnostics is called for that file
THEN the response contains that diagnostic
```

### US-003: "Nothing published yet" is not "clean"

AS AN AI coding agent
I WANT to know when the server has not yet published for a file I just opened
SO THAT I do not read the absence of results as the absence of errors.

**Acceptance criteria:**
```
GIVEN a push-only server that has not yet published for a file
WHEN get_diagnostics is called for that file
THEN the response states that diagnostics are pending or unknown for that file
  AND it is neither an error nor an unqualified empty list
```

### US-004: The caller can tell the answer is push-derived

AS AN AI coding agent
I WANT the result or `get_tool_support` to say that `get_diagnostics` on this server reads the
push cache
SO THAT I understand its freshness and do not assume a fresh analysis ran.

**Acceptance criteria:**
```
GIVEN a routed server without diagnosticProvider
WHEN get_tool_support is queried, or get_diagnostics is called
THEN the output says the answer is push-derived
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN the routed server does not pull-answer, THE SYSTEM SHALL NOT send `textDocument/diagnostic` for `get_diagnostics`. Pull support is learned per server process, because `diagnosticProvider` is only the advertisement (pyright advertises `null` and answers pulls): a server that advertises a provider is pulled; one that does not is probed with its first pull, `-32601` marks it unsupported (no further pull until the process is replaced), an answered pull marks it as answering and it keeps being pulled | must |
| FR-002 | WHEN the routed server is unsupported (FR-001) THE SYSTEM SHALL answer `get_diagnostics` from the push cache | must |
| FR-003 | THE SYSTEM SHALL record whether a publish has ever been received for a file, so a published empty list (clean) is distinct from no publish yet | must |
| FR-004 | WHEN a publish with an empty list has been received for the file THE SYSTEM SHALL answer with a successful empty result | must |
| FR-005 | WHEN no publish has been received for the file THE SYSTEM SHALL report diagnostics as pending or unknown through a distinct, structured state, not as an error and not as an unqualified empty list | must |
| FR-006 | WHEN `get_diagnostics` is answered from the push cache because the server has no pull provider THE SYSTEM SHALL say so in the result, or in `get_tool_support` for that server, or in both | must |
| FR-007 | WHEN the pull request is not sent (FR-001), or the probing pull is refused with `-32601`, THE SYSTEM SHALL NOT log an `ERROR` for it (the refusal is logged at DEBUG); a server that advertises a provider and fails a pull keeps the ERROR log | must |
| FR-008 | WHEN the routed server advertises `diagnosticProvider` THE SYSTEM SHALL keep the current pull plus push merge, unchanged | must |
| FR-009 | WHEN the routed server advertises `diagnosticProvider` and the pull request fails THE SYSTEM SHALL keep the current error behavior unless the plan decides otherwise; this spec does not change it | should |
| FR-010 | THE existing `indexing_in_progress` and `push_notifications_degraded` flags SHALL keep their meaning and SHALL be reported alongside the new state | must |
| FR-011 | THE `get_tool_support` coverage for `get_diagnostics` SHALL NOT claim full coverage for a server without `diagnosticProvider` without qualification | should |
| FR-012 | THE new "pending or unknown" state SHALL also be available on `get_cached_diagnostics` and on the diagnostics resource read, so the three surfaces agree | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | The distinction among "published diagnostics", "published empty" and "nothing published" is a closed type (enum), not an empty collection used as a sentinel and not a boolean pair, per [[constitution]] |
| NFR-002 | Observability | A normal call against a push-only server produces no error-level log line |
| NFR-003 | Latency | The push-only path adds no LSP round-trip; it is a cache read |
| NFR-004 | Concurrency | Cache access follows the existing lock discipline (no cache lock across an `await` on an LSP request), per [[bridge/003-rwlock-translator/spec\|bridge/003]] |
| NFR-005 | Compatibility | The response shape change is additive and recorded in `CHANGELOG.md`; pre-1.0 compatibility is not a constraint |
| NFR-006 | Consistency | The state reported for a file is the same on every surface that reads the cache (FR-012) at the same point in time |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Diagnostics availability | Per-file answer state from the push cache | `Published(diagnostics)`, `PublishedEmpty` (or `Published` with an empty list), `NotYetPublished` |
| Pull capability | Whether the routed server advertises `diagnosticProvider` | present or absent, read from the server capabilities |
| Diagnostics provenance | Where a `get_diagnostics` answer came from | pull plus push, or push only |
| Existing cached entry | Per-URI push entry | `uri`, `version`, `diagnostics` |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Push-only server, clean file, publish received | Successful empty result (FR-004) |
| Push-only server, file just opened, no publish yet | Pending or unknown state (FR-005), not an error |
| Push-only server, publish received for a different file only | Pending or unknown for the requested file |
| Server publishes `[]` after previously publishing a diagnostic | The latest publish wins; empty means clean (FR-004) |
| Server restarted or respawned | Cached entries for that server are cleared today; the state returns to not-yet-published until the next publish |
| Server with `diagnosticProvider` but pull fails | Unchanged (FR-009) |
| Server declares `diagnosticProvider` with `workspaceDiagnostics` only | Treated as a pull provider for documents only if it advertises the document pull; plan decides the exact predicate |
| Push never arrives (server does not publish for this file type) | The file stays pending or unknown; the result must not read as clean. A bounded wait is a plan-time option |
| Server publishes only after the document version changes | Same as not-yet-published until the first publish |
| Native TypeScript server (pull provider present) | Unchanged pull plus push merge (FR-008) |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Fake push-only server, clean file | `get_diagnostics` returns a successful empty result; 0 `textDocument/diagnostic` requests on the wire; 0 `ERROR` log lines |
| SC-002 | Fake push-only server, file with one diagnostic | `get_diagnostics` returns the diagnostic once the publish is cached |
| SC-003 | Fake push-only server, call before any publish | Pending or unknown state; not an error; not an unqualified empty list |
| SC-004 | `get_tool_support` for the push-only route | Does not report unqualified full coverage for `get_diagnostics`, and says the answer is push-derived |
| SC-005 | Live check on typescript-language-server after [[lsp/012-client-publish-diagnostics-capability/spec\|lsp/012]] | `get_diagnostics` returns the type error and a clean file returns a clean result, with no `-32601` and no `ERROR` log |
| SC-006 | Server with `diagnosticProvider` (rust-analyzer) | Output identical to the previous build |
| SC-007 | The three cache-reading surfaces on the same file | Report the same state (NFR-006) |

## 8. Agent Boundaries

### Always (without asking)
- Reproduce with the fake push-only server and keep it as a regression fixture under `.local/testing/`.
- Express the availability state as an enum (NFR-001).
- Keep the pull plus push merge byte-for-byte unchanged for servers with `diagnosticProvider`.
- Run the full pre-commit suite and update `CHANGELOG.md` with the PR link.

### Ask First
- The exact response shape for pending or unknown (a field, a distinct status, or an error variant that is retryable).
- Whether `get_diagnostics` may wait for a first publish for a bounded time.
- Whether to extend `get_cached_diagnostics` and resources in this change (FR-012) or split it.
- Whether the `get_tool_support` coverage value gains a new qualified variant.

### Never
- Return an unqualified empty list for a file that has not been published.
- Send `textDocument/diagnostic` to a server that does not advertise `diagnosticProvider`.
- Change merge or dedup behavior for servers that have a pull provider.
- Special-case a server by name.

## 9. Resolutions

- **Response shape.** A structured `availability` field (`published`, `pending`, `evicted`) on `get_diagnostics`, `get_cached_diagnostics` and the resource read, and `origin` (`pull`, `push_cache`) on `get_diagnostics`; the call succeeds and the caller branches on the field. `pending` and `evicted` carry no answer, so an empty list next to them is not clean.
- **No wait.** `get_diagnostics` returns `pending` at once; a bounded wait is a follow-up.
- **How the cache records a clean file.** A published empty list is an ordinary entry (`published`). Capacity eviction of the last entry of a file leaves a mark: only clean entries evicted reads `published` (clean), a lost non-empty entry reads `evicted`. The marks are bounded; once a server lost the oldest of them, a file without entry or mark reads `evicted` for that server rather than `pending`. A server's marks are dropped with its diagnostics.
- **Capability predicate.** `diagnosticProvider` present (any form) is the advertisement; FR-001 adds what a probe learned. `get_tool_support` reports `push_only` for a server that advertises nothing and has not answered a pull, so a push-only route is never reported as plain `supported`.
- **Respawn.** The learned pull support, the eviction marks and the cached diagnostics of the replaced process are dropped; the file reads `pending` until the replacement publishes. Subscribers are notified through the existing respawn invalidation.

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[bridge/004-get-diagnostics-flycheck-gap/spec|bridge/004]] — pull plus push merge this spec leaves unchanged
- [[bridge/009-diagnostics-subscription-staleness/spec|bridge/009]] — staleness of cached diagnostics and subscriptions
- [[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]] — `indexing_in_progress` flag precedent on diagnostics responses
- [[lsp/012-client-publish-diagnostics-capability/spec|lsp/012]] — makes typescript-language-server publish at all
- [[mcp/002-mcp-resources-diagnostics/spec|mcp/002]] — cache-backed diagnostics tool and resources
- [[mcp/005-tool-capability-discoverability/spec|mcp/005]] — `get_tool_support` coverage semantics
- Code: `crates/mcpls-core/src/bridge/translator/diagnostics.rs` (`handle_validated_diagnostics`), `crates/mcpls-core/src/bridge/notifications.rs` (diagnostics cache), `crates/mcpls-core/src/mcp/server.rs` (`get_diagnostics`, `DiagnosticsResponse`)
