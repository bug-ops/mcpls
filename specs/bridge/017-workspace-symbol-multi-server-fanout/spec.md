---
aliases:
  - Workspace symbol multi-server fan-out
  - Polyglot workspace symbol search
tags:
  - sdd
  - spec
  - enhancement
  - bridge
  - routing
  - multi-server
  - competitor-gap
created: 2026-11-01
status: implemented
related:
  - "[[constitution]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001-mcp-tool-surface-and-routing]]"
  - "[[mcp/005-tool-capability-discoverability/spec|mcp/005-tool-capability-discoverability]]"
  - "[[mcp/014-tools-list-payload-size/spec|mcp/014-tools-list-payload-size]]"
  - "[[mcp/015-request-cancellation-propagation/spec|mcp/015-request-cancellation-propagation]]"
  - "[[mcp/018-bounded-tool-inputs-and-results/spec|mcp/018-bounded-tool-inputs-and-results]]"
  - "[[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006-lsp-indexing-readiness-gate]]"
  - "[[bridge/012-indexing-gate-after-restart/spec|bridge/012-indexing-gate-after-restart]]"
  - "[[bridge/013-indexing-state-on-ungated-prepare-tools/spec|bridge/013-indexing-state-on-ungated-prepare-tools]]"
  - "[[bridge/015-empty-result-index-coverage-caveat/spec|bridge/015-empty-result-index-coverage-caveat]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001-lsp-server-lifecycle-and-respawn]]"
  - "[[runtime/003-workspace-supplied-code-execution/spec|runtime/003-workspace-supplied-code-execution]]"
---

# Feature: Fan workspace symbol search out over every capable server

> [!info] Metadata
> **Type**: enhancement (competitor-gap)
> **Priority**: P3
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle, competitor gap analysis
> **Issue**: #844

> [!abstract]
> `workspace_symbol_search` has no file argument to route by, so it resolves exactly one server
> (`ToolRouter::resolve_any`): the first server in config order that explicitly claims the tool, else
> the first catch-all. In a polyglot workspace with several live servers, symbols owned by the others
> are silently missing, and the response gives no hint that those servers were not asked. This spec
> defines a fan-out: query every eligible, capable server concurrently with a per-server deadline,
> merge the results tagged by producing server, report per-server outcomes (including failures), and
> leave single-server output byte-identical.

## 1. Overview

### Problem Statement

`Translator::handle_workspace_symbol` (`crates/mcpls-core/src/bridge/translator/symbols.rs`)
resolves its server through `workspace_symbol_server`, which calls
`ToolRouter::resolve_any(ToolKind::WorkspaceSymbols)` (`crates/mcpls-core/src/config/routing.rs`).
That function walks the servers in config declaration order and returns the first one that
explicitly claims the tool, else the first catch-all, and nothing else
([[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]], FR-005). The handler then sends one
`workspace/symbol` request to that server and returns its answer.

Per-file tools do not have this problem: the file's language selects the server. A workspace-wide
query has no such selector, so with two or more running servers the answer comes from whichever one
config order favors:

| Situation | What the caller sees today |
|-----------|----------------------------|
| Polyglot workspace (for example a Rust server and a TypeScript server), query for a TypeScript symbol, Rust server resolved | Empty or unrelated result, indistinguishable from "no such symbol" |
| Polyglot workspace, query matching symbols in both languages | Only the resolved server's matches, no hint that another server exists |
| Resolved server is empty, another server could answer | Empty result; `index_coverage` ([[bridge/015-empty-result-index-coverage-caveat/spec|bridge/015]]) describes only the resolved server |
| Capability reporting (`get_tool_support`) for the tool | Reports the one resolved server, so a client cannot tell that other servers also hold workspace symbols |

An AI client that reads the empty or partial answer as "this symbol does not exist" may re-create
code that exists, or decide a rename or deletion is safe. This is the same false-negative hazard
that [[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]] (#420) and
[[bridge/015-empty-result-index-coverage-caveat/spec|bridge/015]] (#796) closed for the indexing and
index-coverage causes; routing is a third cause of the same empty-means-nothing misreading, and the
only one that is silent in every case.

Two properties of the current code constrain the design and are inputs to it:

- **Routing declarations are meaningful.** A server with a `handles` list has explicitly declined
  every tool not on it. `resolve_any` deliberately never falls back to "the first server at all"
  for that reason ([[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]], NFR-004 and edge case on
  `NoClaimant`). The fan-out must keep that guarantee.
- **The result is already bounded.** `limit` is clamped to `MAX_NORMALIZED_LOCATIONS` (10,000) before
  any range normalization, because normalization can read from disk (#474, #487;
  [[mcp/018-bounded-tool-inputs-and-results/spec|mcp/018]]). Merging several answers must not
  reopen that amplification.

**Evidence and its limits.** Found by a competitor gap analysis: several comparable bridges now send
workspace-level queries (those with no file argument to route by) to every connected server, merge
the results tagged by the server that produced them, and report per-server failures. The mcpls side
was confirmed by source inspection at HEAD `d041526`: exactly one server is resolved and asked, and
no response field names which servers were or were not asked. A live reproduction against a real
polyglot configuration has not been run; SC-001 runs against two test doubles.

### Goal

When more than one eligible server could answer `workspace_symbol_search`, the call queries all of
the capable ones concurrently within bounded time and resources, returns one deterministic merged
result in which each symbol names its producing server, and states per server whether it answered,
was not ready, failed, or lacks the capability, so that a missing symbol is never silently
attributable to an unasked server; with exactly one eligible server the output is byte-identical to
today.

### Options Compared

| | (a) Fan out to all eligible capable servers, merged and tagged | (b) Optional input parameter to select servers, default unchanged | (c) Status quo, document the single-server behavior and point to `get_tool_support` |
|---|---|---|---|
| What it fixes | The silent omission, for every client, with no client change | Lets an informed client ask each server; the default call stays silently partial | Nothing in the product |
| LSP round-trips | One `workspace/symbol` per eligible capable server, concurrent | One per selected server | One |
| Wire change | Optional per-symbol `server` field and optional per-server outcome list, both absent in single-server setups | New input parameter plus the same output need | None |
| Risk | Latency bounded by the slowest server; larger responses; merge and truncation semantics to define | Clients must discover and use it; the false-negative hazard stays by default | The hazard stays for every client that does not read the docs |
| Fits the project's pattern | Yes: typed optional disclosure fields (`index_coverage`, `positions_degraded`), graceful degradation per server | Partly: `restart_server` takes a server selector, but that tool is not a query | No for false-negative hazards (#420, #796 were fixed, not documented) |
| Size budget ([[mcp/014-tools-list-payload-size/spec\|mcp/014]]) | Adds schema bytes to one tool; see NFR-003 | Adds more | None |

**Recommendation: (a).** It is the only option that removes the hazard on the default call. (b) is
not excluded as a later refinement (see Out of Scope) but does not address the silent default.

### Out of Scope

- Fan-out for any other tool. `workspace_symbol_search` is the only tool that resolves a server
  through `resolve_any` today; per-file tools keep language-based routing.
- An input parameter for choosing servers (option (b)), and any per-call override of the participating set.
- Changing `resolve_any` semantics for callers other than this tool, or the per-file route tiers.
- Gating `workspace_symbol_search` on indexing readiness (#422/#423 stay deferred) or changing the
  `Error::WorkspaceIndexing` behavior of other tools.
- Cross-server result ranking or relevance scoring; the merge preserves each server's own order.
- Deduplication beyond exact-identity duplicates (FR-012), and any symbol-equivalence heuristic across languages.
- A config key or environment variable controlling the fan-out. Participation is derived from
  existing routing declarations and capabilities.
- Technical design: recorded in a plan after this spec is approved.

## 2. User Stories

### US-001: Find a symbol owned by any running server

AS AN AI coding agent searching a polyglot workspace by symbol name
I WANT one `workspace_symbol_search` call to cover every running language server that can answer
SO THAT a symbol in the second language is found without my knowing which server holds it.

**Acceptance criteria:**
```
GIVEN two running servers, both advertising workspaceSymbolProvider and both routable for the tool
  AND a symbol "Widget" known only to the server that config order ranks second
WHEN the agent calls workspace_symbol_search with query "Widget"
THEN the result contains the symbol
  AND the symbol names the server that produced it
```

### US-002: Know which servers were asked

AS AN AI coding agent reading an empty or partial result
I WANT the response to say, per server, whether it answered, was not ready, timed out, failed, or lacks the capability
SO THAT I distinguish "no such symbol" from "a server was not able to say".

**Acceptance criteria:**
```
GIVEN two eligible servers, one of which is still initializing
WHEN the agent calls workspace_symbol_search
THEN the call returns without waiting for the initializing server
  AND the response lists that server with an "initializing" outcome
  AND the symbols of the other server are returned
```

### US-003: One bad server does not hide the others

AS A user whose workspace runs several servers
I WANT a timed-out, crashed or restarting server to be reported without discarding the others' symbols or stalling the call
SO THAT graceful degradation (a documented mcpls property) holds for workspace search too.

**Acceptance criteria:**
```
GIVEN three eligible servers, one of which never replies
WHEN the agent calls workspace_symbol_search
THEN the call completes within that server's request deadline, not later
  AND the two answering servers' symbols are present
  AND the silent server is reported as timed out
```

### US-004: Single-server users pay nothing

AS A user with one eligible server (the common case)
I WANT the response to be byte-identical to today and the request count unchanged
SO THAT the fan-out costs nothing where it is not needed.

**Acceptance criteria:**
```
GIVEN exactly one server eligible for workspace symbols
WHEN the agent calls workspace_symbol_search
THEN the serialized response equals the pre-change response for the same server answer
  AND exactly one workspace/symbol request is sent
```

### US-005: Routing declarations and trust boundaries still bind

AS A maintainer and as an operator of an untrusted workspace
I WANT a server that declared it does not handle workspace symbols, or that untrusted mode refused, never to be contacted
SO THAT fan-out widens coverage without widening what mcpls is allowed to run or ask.

**Acceptance criteria:**
```
GIVEN a server configured with a handles list that omits the workspace symbols tool
  AND a second server that is a catch-all
WHEN the agent calls workspace_symbol_search
THEN only the catch-all receives workspace/symbol
  AND the response does not attribute any symbol to the declining server
```

### US-006: Discover multi-server coverage up front

AS AN AI coding agent
I WANT `get_tool_support` for `workspace_symbol_search` to reflect every eligible server
SO THAT I can see before calling that more than one server will be asked, and which of them lacks the capability.

**Acceptance criteria:**
```
GIVEN two eligible servers, one lacking workspaceSymbolProvider
WHEN the agent calls get_tool_support
THEN the entry for workspace_symbol_search reports both servers and the capability gap of the second
```

## 3. Functional Requirements

Terms: the **eligible set** is the servers the router permits for the tool (FR-001); the
**queried set** is the eligible servers that are live and advertise `workspaceSymbolProvider`.

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE eligible set SHALL be derived from the router's declarations: a server is eligible WHEN it explicitly claims the workspace symbols tool OR it is a catch-all; a server whose `handles` list omits the tool SHALL NOT be eligible. Decided: the eligible set is the union of explicit claimers and catch-alls of every language, in config order, minus every server untrusted-workspace mode refused; a claimer no longer hides a catch-all for this tool (a behavior change, recorded in `CHANGELOG.md`). | must |
| FR-002 | WHEN the eligible set holds at least two servers THE SYSTEM SHALL send the same `workspace/symbol` query to every member of the queried set, concurrently | must |
| FR-003 | WHEN the eligible set holds exactly one server THE SYSTEM SHALL behave and serialize exactly as before this change, including every existing error | must |
| FR-004 | THE eligible set SHALL be computed from the configured router, not only from the live one, so a configured server that failed to start or is not yet registered appears in the outcome list (FR-009) instead of vanishing and turning a polyglot setup into an apparent single-server setup | must |
| FR-005 | WHEN an eligible server is not registered yet but is expected THE SYSTEM SHALL NOT wait for it, SHALL report it as `initializing`, and SHALL return the other servers' results | must |
| FR-006 | WHEN an eligible server is dead, failed to start or is being respawned THE SYSTEM SHALL NOT delay any other server's request on it and SHALL report it as `unavailable`. Decided: a fan-out never respawns; a dead registered server is reported `unavailable` at once (a branch that ends in `ServerTerminated` is `unavailable` too), and every queried branch is bounded by its own request deadline. A single eligible server keeps today's respawn. | must |
| FR-007 | WHEN an eligible live server does not advertise `workspaceSymbolProvider` THE SYSTEM SHALL NOT send it the request and SHALL report it as `capability_not_advertised` | must |
| FR-008 | WHEN no server of the eligible set produced an answer THE SYSTEM SHALL return an error, never a success-shaped empty result; the error SHALL be the one the single-server path returns for the same condition when the eligible set has one member, and a documented most-specific aggregate (all initializing, all failed to start, none capable) otherwise | must |
| FR-009 | WHEN the eligible set holds at least two servers THE response SHALL carry a typed outcome list with one entry per eligible server: server id, a closed outcome enum (`answered`, `initializing`, `unavailable`, `timed_out`, `failed`, `capability_not_advertised`), and for `answered` the count of symbols the server returned before filtering | must |
| FR-010 | WHEN the eligible set holds at least two servers THE SYSTEM SHALL tag every returned symbol with its producing server; WHEN it holds one server THE tag SHALL be absent, not `null` | must |
| FR-011 | THE merged order SHALL be deterministic: servers in config declaration order, and within a server the order the server returned; it SHALL NOT depend on which server replied first. Decided: concatenation; the per-server counts of FR-009 and `truncated` make the cut visible. | must |
| FR-012 | WHEN two answers hold a symbol with identical name, kind, container, URI and normalized range THE SYSTEM SHALL keep the first in merged order and drop the rest. Decided: duplicates are dropped per server after `kind_filter` and before the symbol counts towards the collection cap, so duplicates never push a later server's symbols out; the range compared is the raw LSP range, so the same symbol reported in two position encodings is kept twice, each with its own `server` tag. | should |
| FR-013 | THE `kind_filter` SHALL apply to every server's answer and `limit` SHALL apply once to the merged, filtered, deduplicated set, clamped to `MAX_NORMALIZED_LOCATIONS`; `truncated` SHALL be set WHEN any merged symbol was dropped by `limit` or the clamp | must |
| FR-014 | THE range normalization budget SHALL be bounded by `MAX_NORMALIZED_LOCATIONS` for the whole call, not per server | must |
| FR-015 | EACH server's symbols SHALL be normalized with that server's own position-encoding context, and `out_of_workspace` SHALL be computed per server; `positions_degraded` of the merged result SHALL report degradation WHEN any contributing server's answer was degraded. Decided: a single merged value (the maximum over the answering servers), per-symbol attribution through the `server` tag. | must |
| FR-016 | WHEN a server answered with no symbols itself THE outcome entry for that server SHALL carry the `index_coverage` note of [[bridge/015-empty-result-index-coverage-caveat/spec\|bridge/015]], and the top-level `index_coverage` SHALL be omitted whenever the outcome list is present | must |
| FR-017 | WHEN a queried server is still loading its workspace index THE outcome entry for that server SHOULD say so, using the indexing state mcpls already holds, without an extra LSP request. Decided: not added in this change (follow-up issue); the `index_coverage` note of FR-016 is the only coverage disclosure. | should |
| FR-018 | EACH server's request SHALL carry that server's existing request deadline (the `request_timeout_seconds` mechanism used by `budgeted`), so total call time is bounded by the slowest server's deadline, not their sum. Decided: no grace deadline, no new timeout and no new config key. | must |
| FR-019 | THE number of concurrently outstanding requests of one call SHALL be bounded by a constant not smaller than the number of eligible servers permitted by configuration. Decided: concurrency equals the eligible-set size, which the configuration bounds. | must |
| FR-020 | WHEN the call is cancelled THE SYSTEM SHALL cancel each outstanding request on the server that holds it, send nothing to other servers, produce no partial result, and start no further request, per [[mcp/015-request-cancellation-propagation/spec\|mcp/015]] FR-017, FR-018 | must |
| FR-021 | WHEN one server fails, times out or is restarted mid-call THE SYSTEM SHALL keep the other servers' requests and results unaffected | must |
| FR-022 | THE queried set SHALL contain only servers present in the live registry; WHEN untrusted-workspace mode refused a server it SHALL NOT be started, contacted or listed as a symbol producer by this tool. Decided: a refused server is excluded from the eligible set (so it is not listed) by the one eligibility function the handler and `get_tool_support` share; with one admitted server left the output is byte-identical to a single-server setup. | must |
| FR-023 | `get_tool_support` for `workspace_symbol_search` SHALL aggregate over the eligible set WHEN it holds at least two servers (`supported` naming the first supporting server in config order, else `initializing`, else the first capability gap, else `no_server`) with no change to the response shape, and SHALL serialize as today WHEN it holds one; per-server detail is a follow-up; the `get_tool_support` parity tests SHALL keep `workspace_support` and the handler on the same decision functions | should |
| FR-024 | THE `tools/list` description and output schema of `workspace_symbol_search` SHALL document the `server` tag and the outcome list in one canonical sentence each | must |
| FR-025 | THE user guide SHALL state that the search covers every capable eligible server, how the outcome list reads, and that a missing server in the list means its `handles` declaration excludes the tool | must |
| FR-026 | THE `.local/testing/` playbooks SHALL gain a polyglot case (two real servers, query matching both; one server stopped; one server lacking the capability) and the coverage status SHALL be reset for the bridge subsystem | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Output compatibility | With one eligible server, the serialized response and the sequence of LSP messages are byte-identical to the pre-change build; new fields are omitted, never `null` |
| NFR-002 | Latency | With N queried servers each answering in d ms, total call time is at most d plus a bounded overhead, not N times d; a hung server adds at most its own request deadline |
| NFR-003 | Payload budget | The golden `tools/list` payload SHALL stay within the enforced budgets of [[mcp/014-tools-list-payload-size/spec\|mcp/014]] (total 136,000 B, per tool 9,000 B, schema descriptions 35,000 B). At the start of the change the total was 135,990 B, leaving 10 B of headroom, and `workspace_symbol_search` was 4,289 B (measured after: total 138,583 B with the `get_completions` cap text, `workspace_symbol_search` 6,629 B, descriptions 30,229 B; the total budget is now 139,000 B); the addition therefore needs either trimmed unrelated schema text or an approved budget raise. Decided: the total budget is raised to the measured total rounded up to the next 500 B and recorded in [[mcp/014-tools-list-payload-size/spec|mcp/014]]; the per-tool and description budgets stay unchanged |
| NFR-004 | Result bounds | The merged result never holds more than `MAX_NORMALIZED_LOCATIONS` symbols and never triggers more range normalizations than that across all servers; the outcome list holds at most one entry per eligible server and each entry has bounded size |
| NFR-005 | Resource bounds | Collecting the raw answers of N servers stays bounded; a hostile or oversized answer from one server cannot exceed what a single-server call may already consume per server. Decided: collection stops once `limit + 1` unique symbols are accepted overall, so one server cannot make the merge hold more than that; the raw answers themselves are bounded as for a single server. |
| NFR-006 | Type safety | Outcomes are a closed enum, the producing server is a typed server id bounded by `MAX_SERVER_ID_BYTES`, counts are bounded integers; no stringly-typed state and no untyped map, per [[constitution]] and the project's type-safety rule |
| NFR-007 | Determinism | Identical server answers yield an identical response regardless of reply timing, verified with reversed reply latencies |
| NFR-008 | Concurrency | Registry and tracker locks follow the existing lock-ordering discipline and are never held across an await; one translator snapshot of router and registry is used for the whole call so a concurrent restart cannot split the eligible set |
| NFR-009 | Security | Fan-out contacts only servers the existing startup and trust logic admitted ([[runtime/003-workspace-supplied-code-execution/spec\|runtime/003]]); it adds no spawn path and no new trust input |
| NFR-010 | Server-agnostic | No server name or language list appears in the bridge layer; eligibility and capability come from router declarations and `initialize` capabilities |
| NFR-011 | Pre-1.0 | The response change is additive for multi-server setups and is recorded in `CHANGELOG.md` with the PR link; the shadowing change of FR-001, if adopted, is recorded as a behavior change |
| NFR-012 | Documentation | New public items carry `///` docs with `# Examples` where non-trivial; the tools reference stays accurate |
| NFR-013 | Portability | Tests that fabricate servers or paths work on Windows and macOS (fake executables get an `.exe` name), per `CLAUDE.md` |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Eligible set (derived) | Servers the configured router permits for the workspace symbols tool | server id, explicit claim or catch-all, config order |
| Queried set (derived) | Eligible servers that are live and advertise `workspaceSymbolProvider` | server id, capability set |
| Server outcome (new) | Typed per-server statement about this call, present in multi-server setups only | server id; outcome (`answered`, `initializing`, `unavailable`, `timed_out`, `failed`, `capability_not_advertised`); symbols returned before filtering; optional index coverage note; optional indexing flag |
| Producing-server tag (new) | Optional per-symbol field naming the server whose answer contained the symbol | server id; absent when the eligible set has one member |
| Workspace symbol result (existing) | The tool output | symbols, `truncated`, `positions_degraded`, `index_coverage` (single-server shape only), plus the outcome list |
| Index coverage note (existing) | [[bridge/015-empty-result-index-coverage-caveat/spec\|bridge/015]] statement for an empty answer | synced document count, restart flag |
| Tool support report (existing) | `get_tool_support` entry per tool | route support; gains one entry per eligible server for this tool |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Eligible set of one server | Byte-identical to today (FR-003, NFR-001) |
| Two eligible servers, one still initializing | Call does not wait; that server reported `initializing`; the other's symbols returned (FR-005) |
| One server dead, respawn in progress or backing off | Reported `unavailable`, never respawned by this call; others unaffected (FR-006, FR-021) |
| One server restarted by `restart_server` mid-call | Its pending request fails with the existing restart error; reported `unavailable`; others unaffected |
| Server returns an LSP error | Reported `failed`; no retry beyond the existing content-modified retry ([[lsp/001-lsp-server-lifecycle-and-respawn/spec\|lsp/001]]); others unaffected |
| Server never replies | Reported `timed_out` at its deadline; `$/cancelRequest` sent once per [[mcp/015-request-cancellation-propagation/spec\|mcp/015]] FR-006 |
| Eligible server lacks `workspaceSymbolProvider` | Not asked; reported `capability_not_advertised` (FR-007) |
| Every eligible server lacks the capability | Error, same classification as single-server `CapabilityNotSupported` generalized (FR-008) |
| Every eligible server initializing | Existing all-initializing error (FR-008) |
| Some servers answered empty, one failed | Empty merged list plus the outcome list; the failure is visible so the empty is not read as authoritative (FR-009) |
| Server declared `handles` without the tool | Not eligible, not asked, not listed (FR-001, US-005) |
| Configured server failed to start while a second runs | Multi-server shape with the failed server listed, not an apparent single-server setup (FR-004) |
| Server was refused by untrusted-workspace mode | Never started or contacted; not a symbol producer (FR-022) |
| Same symbol reported by two servers | First in merged order kept (FR-012) |
| Servers disagree on position encoding (UTF-8 and UTF-16) | Each answer normalized with its own context; `positions_degraded` merged by FR-015 |
| Merged set exceeds `limit` or the cap | Cut after filtering and deduplication; `truncated: true` (FR-013) |
| `kind_filter` removes everything from one server | That server is `answered` with its pre-filter count; no `index_coverage` (the server did not answer empty) |
| Client cancels mid-fan-out | Each outstanding request cancelled on its own server; no result (FR-020) |
| Replies arrive in reverse of config order | Same merged order and response (FR-011, NFR-007) |
| Two servers share a language catch-all slot | Rejected at config load today (`from_configs` rejects two catch-alls per language); not a fan-out case |
| Client ignores the new fields | Behavior equals today's for the symbols field; the fields are advisory |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Two test-double servers, each holding a distinct symbol, query matching both | Both symbols returned, each tagged with its own server, in config order |
| SC-002 | Single eligible server, 10 repetitions, response compared byte for byte with the pre-change build; LSP messages counted | Identical bytes; exactly one `workspace/symbol` per call |
| SC-003 | Three servers answering after 200 ms each | Call completes in at most 400 ms |
| SC-004 | Three servers, one that never replies, request deadline T | Call completes within T plus 100 ms; two servers' symbols present; third reported `timed_out` |
| SC-005 | One server initializing, one ready | Call completes without waiting on the initializing server; outcome `initializing` listed |
| SC-006 | Server whose `handles` omits the tool plus a catch-all | Zero `workspace/symbol` requests to the declining server |
| SC-007 | Server lacking the capability | Zero requests to it; outcome `capability_not_advertised` |
| SC-008 | Untrusted mode with one refused and one admitted server | Refused server never spawned or contacted; response built from the admitted server |
| SC-009 | Repeat SC-001 with reply latencies reversed | Byte-identical response |
| SC-010 | Two servers each returning more than `MAX_NORMALIZED_LOCATIONS` symbols | At most 10,000 symbols returned and at most 10,000 range normalizations; `truncated: true` |
| SC-011 | Cancel mid-call with two outstanding requests | One `$/cancelRequest` per server, none to others, no result |
| SC-012 | All eligible servers failing, initializing or incapable | Error, never an empty success |
| SC-013 | Golden `tools/list` payload | Within the three enforced budgets, with the measured delta and any budget decision recorded |
| SC-014 | `get_tool_support` parity matrix with two eligible servers | `workspace_support` and the handler agree on the eligible set and capability gaps |
| SC-015 | Rule coverage by tests | A unit test per eligibility, outcome, merge, dedup and truncation branch; golden snapshot updated for the tool's schema |

## 8. Agent Boundaries

### Always (without asking)
- Derive eligibility and capability from the router declarations and `initialize` capabilities through shared decision functions, so the handler and `get_tool_support` cannot drift.
- Take one router and registry snapshot per call and avoid holding any lock across an await.
- Keep every new response field optional and omitted when absent so single-server output stays byte-identical.
- Measure the `tools/list` payload against the enforced budgets before and after, and regenerate `tool_surface.json` through the existing mechanism.
- Run the full pre-commit suite (fmt, clippy, nextest, rustdoc, `cargo check -p mcpls --locked`, the `fuzz` lockfile check) and update `CHANGELOG.md` with the PR link.
- Update the `.local/testing/` playbooks and reset the coverage status for the affected subsystem.

### Ask First
- Raising any `mcp/014` budget constant or trimming unrelated schema text to make room (NFR-003 leaves 10 B).
- Letting a catch-all server join an explicit claimer for this tool (FR-001 changes today's tier shadowing).
- Choosing concatenation over round-robin merge, and the duplicate rule (FR-011, FR-012).
- Adding any new timeout, concurrency constant or config key.
- Adding indexing-state disclosure to the multi-server shape (FR-017) or to the single-server shape.
- Extending the fan-out to any other tool, or adding a server-selector input (option (b)).
- Any new dependency.

### Never
- Send `workspace/symbol` to a server whose `handles` omit the tool, that lacks the capability, or that untrusted-workspace mode refused.
- Spawn, respawn or register a server on behalf of this tool beyond the existing respawn rule.
- Return a success-shaped empty result when no server answered.
- Wait on an initializing server, or let one slow server delay another's request.
- Serialize new fields as `null`, or change the output of a single-eligible-server setup.
- Exceed `MAX_NORMALIZED_LOCATIONS` for the merged set, or normalize ranges per server without a shared budget.
- Add a server-name or language list to the bridge layer.
- Change the gate or flag behavior of other tools or the `indexing_in_progress` channel.

## 9. Decisions

Open questions of the draft, resolved on 2026-11-01 (the user decided FR-001, FR-006, FR-011, FR-012 and NFR-003; the rest follow the defaults stated in the requirements):

- FR-001: union of explicit claimers and catch-alls, minus refused servers.
- FR-006: no respawn in a fan-out; a dead server is `unavailable`.
- FR-011: concatenation in config order.
- FR-012: exact duplicates dropped before the per-server cut; collection stops at `limit + 1` unique symbols.
- FR-015: one merged `positions_degraded`.
- FR-017: not added; follow-up issue.
- FR-018, FR-019: no new timeout or cap.
- FR-022: refused servers are not listed.
- NFR-003: raise the total tools/list budget.
- NFR-005: bounded by the `limit + 1` stop.
- `get_tool_support`: aggregate with no shape change; per-server detail is a follow-up.
- The outcome list is present whenever the eligible set has two or more members.
- No server answered: a server still `initializing` wins because it is the retryable answer (all of them give `WorkspaceServersInitializing`, a mix gives that server's `ServerInitializing`); else, when every member failed to start, `AllServersFailedToInit` lists every failure (#527); else the first breakage (`unavailable`, `timed_out`, `failed`) in config order gives its own error, else the first server's capability error.

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]] — `resolve_any` two-tier resolution and determinism (FR-005, NFR-004)
- [[mcp/005-tool-capability-discoverability/spec|mcp/005]] — `get_tool_support` and its parity matrix
- [[mcp/014-tools-list-payload-size/spec|mcp/014]] — `tools/list` size budget
- [[mcp/015-request-cancellation-propagation/spec|mcp/015]] — multi-server cancellation (FR-017, FR-018, FR-019)
- [[mcp/018-bounded-tool-inputs-and-results/spec|mcp/018]] — `MAX_NORMALIZED_LOCATIONS` and `truncated`
- [[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]], [[bridge/012-indexing-gate-after-restart/spec|bridge/012]], [[bridge/013-indexing-state-on-ungated-prepare-tools/spec|bridge/013]] — indexing disclosure patterns
- [[bridge/015-empty-result-index-coverage-caveat/spec|bridge/015]] — the `index_coverage` note carried per server here
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] — respawn and backoff
- [[runtime/003-workspace-supplied-code-execution/spec|runtime/003]] — untrusted-workspace mode and the server allowlist
- Code: `crates/mcpls-core/src/bridge/translator/symbols.rs` (`handle_workspace_symbol`, `workspace_symbol_server`), `crates/mcpls-core/src/config/routing.rs` (`ToolRouter::resolve_any`), `crates/mcpls-core/src/bridge/translator/support.rs` (`workspace_support`), `crates/mcpls-core/src/bridge/translator/navigation.rs` (`MAX_NORMALIZED_LOCATIONS`), `crates/mcpls-core/src/bridge/translator/dto.rs` (`WorkspaceSymbolResult`), `crates/mcpls-core/src/mcp/schema_shape.rs` (budget constants), `crates/mcpls-core/src/mcp/tool_surface.json`
