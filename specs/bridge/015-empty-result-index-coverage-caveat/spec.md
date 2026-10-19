---
aliases:
  - Empty result index coverage caveat
  - Open-document-only index disclosure
tags:
  - sdd
  - spec
  - enhancement
  - bridge
  - lsp
  - reliability
  - competitor-gap
created: 2026-10-19
status: draft
related:
  - "[[constitution]]"
  - "[[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006-lsp-indexing-readiness-gate]]"
  - "[[bridge/002-document-tracker-synchronization/spec|bridge/002-document-tracker-synchronization]]"
  - "[[bridge/005-expose-document-tracker-limits/spec|bridge/005-expose-document-tracker-limits]]"
  - "[[bridge/012-indexing-gate-after-restart/spec|bridge/012-indexing-gate-after-restart]]"
  - "[[bridge/013-indexing-state-on-ungated-prepare-tools/spec|bridge/013-indexing-state-on-ungated-prepare-tools]]"
  - "[[mcp/014-tools-list-payload-size/spec|mcp/014-tools-list-payload-size]]"
---

# Feature: Disclose index coverage on empty references and workspace-symbol results

> [!info] Metadata
> **Type**: enhancement (competitor-gap)
> **Priority**: P4
> **Author**: Andrei G.
> **Source**: continuous-improvement cycle, competitor gap analysis
> **Issue**: #796

> [!abstract]
> `get_references` and `workspace_symbol_search` return an empty list when the language server
> answers `[]` or `null`. The "workspace still indexing" cause of that answer is already disclosed
> ([[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]], [[bridge/013-indexing-state-on-ungated-prepare-tools/spec|bridge/013]]).
> A second cause is not: servers whose index covers only documents the client has sent with
> `textDocument/didOpen` answer empty for symbols in files the session never opened. mcpls opens
> documents lazily and forgets per-server sync state on restart, so this cause is reachable in
> normal use. This spec compares three ways to deal with it, recommends one, and keeps closing the
> item as a documented non-goal on the table.

## 1. Overview

### Problem Statement

`Translator::handle_references` (`crates/mcpls-core/src/bridge/translator/navigation.rs`) and the
workspace-symbol handler return the server's answer through `response.unwrap_or_default()`. An
empty `LocationsResult` or symbol list therefore has one wire shape for several different facts:

| Cause of an empty answer | Disclosed today |
|--------------------------|-----------------|
| The symbol has no references / no match | no (this is the genuine empty) |
| The routed server is still indexing | yes: `Error::WorkspaceIndexing` for `get_references` (gated), see [[bridge/006-lsp-indexing-readiness-gate/spec\|bridge/006]]; `workspace_symbol_search` bypasses the gate by design, see section 6 |
| The server's index is built only from documents the client opened, and the files holding the answer were never opened in this session | no |
| The server was restarted or respawned; mcpls forgot which documents it had sent, and re-sends a document only when a later tool call touches it | no |

The last two rows are the same class of server behavior seen from two angles. Some language
servers build their symbol table and reference index only from buffers delivered through
`textDocument/didOpen`, and answer `workspace/symbol` and `textDocument/references` with `[]` for
symbols in other files. The same servers may still answer `textDocument/definition` correctly,
because definition can resolve through the include or import graph on demand. The caller then sees
a working `get_definition` next to an empty `get_references` on the same symbol, and no hint that
the second answer is bounded by what the server has loaded.

mcpls makes this reachable in two ways:

- It opens documents lazily: only the queried file is sent with `didOpen` (`bridge/state.rs`,
  [[bridge/002-document-tracker-synchronization/spec|bridge/002]]). A reference in a file the
  session never touched is outside such a server's index.
- After `restart_server` or a respawn the per-server sync state is dropped
  (`DocumentTracker::forget_server`). Files the user considers "already open" are forgotten by the
  new server process until a position-based tool touches each one again.

An AI client that reads an empty list as "no references" or "symbol not defined" may delete or
rename code that is used elsewhere. This is the same false-negative hazard that
[[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]] closed for the indexing cause (#420).

**Evidence and its limits.** Found by a competitor gap analysis. One comparable LSP-to-MCP bridge
qualified its find-symbol and find-references empty results with an index-coverage caveat that
separates the empty causes, and re-sends `didOpen` for previously open documents after a server
restart; earlier it had replaced an unconditional "may be dead code" hint with a neutral one for
the same reason. One competitor and one niche server class is thin evidence, hence P4. The mcpls
side was confirmed by source inspection at HEAD `d2a9533`: no coverage hint exists anywhere in
`crates/mcpls-core/src` for the empty-references case. A live reproduction against a real
open-document-only server has not been run `[NEEDS CLARIFICATION: which language server in the
default 30-language mapping, if any, exhibits open-document-only indexing for references, so that
the reproduction in SC-001 can be run live rather than against a test double]`.

### Goal

When `get_references` or `workspace_symbol_search` returns an empty result and mcpls cannot rule
out that the routed server indexes only opened documents, the response carries a typed, bounded,
machine-readable statement of what the answer covers, so that a client can tell "nothing found in
the indexed scope" from "nothing exists", with unchanged output and no extra cost for non-empty
results.

### Options Compared

The finding names three candidate approaches. They answer different questions: (a) tells the
caller, (b) removes one cause, (c) tells the human.

| | (a) Empty-result coverage note | (b) Re-open previously open documents after restart/respawn | (c) Document as a non-goal, README guidance only |
|---|---|---|---|
| What it fixes | Caller can tell a scope-bounded empty from a genuine empty, on every server, for both causes | Only the restart/respawn cause; the "never opened" cause remains | Nothing in the product; sets expectations |
| LSP round-trips | None on the non-empty path; none added on the empty path if built from tracker state mcpls already holds | N `didOpen` per restart, N bounded by tracker limits ([[bridge/005-expose-document-tracker-limits/spec\|bridge/005]]); reads files from disk | None |
| Wire change | One optional field on two tools; description text in `tools/list` | None | None |
| Risk | False reassurance if the note is read as "complete" when absent; false alarm on servers that index the whole workspace | Contradicts lazy open ([[bridge/002-document-tracker-synchronization/spec\|bridge/002]]); mcpls would reopen documents the user closed or that changed on disk; races with the restart gate ([[bridge/012-indexing-gate-after-restart/spec\|bridge/012]]) | The hazard stays for every client that does not read the README |
| Fits the project's established pattern | Yes: typed disclosure fields (`indexing_in_progress`, `positions_degraded`, `availability`) | No existing precedent for eager re-sync | Yes for non-goals, no for false-negative hazards (#420, #244 were fixed, not documented) |
| Size budget ([[mcp/014-tools-list-payload-size/spec\|mcp/014]]) | Adds schema bytes to two tools; must fit | None | None |

**Recommendation: (a), plus the (c) documentation as its companion; (b) is not part of this
change.** (a) is the only option that removes the hazard for both causes without changing document
lifecycle behavior, and it is the established mcpls shape for "the answer may be incomplete". (b)
addresses a narrower cause at a real behavioral cost and should be evaluated only if (a) proves
insufficient `[NEEDS CLARIFICATION: is a bounded re-sync after restart wanted as a separate
follow-up, or rejected outright because it conflicts with lazy open?]`.

**Close-as-non-goal fallback.** If section 11's gate fails (the field cannot fit the
`tools/list` budget without trimming unrelated schema text, or no trigger rule below can avoid
flagging every empty result), the item closes as option (c) alone: a README and tool-description
statement that an empty result reflects only what the routed server has indexed, with a pointer to
`get_definition` and to touching the files of interest. P4 and one-competitor evidence make this a
legitimate outcome, not a failure.

### Out of Scope

- Eagerly opening every file in the workspace to widen a server's index.
- Option (b), re-sending `didOpen` for previously open documents after restart or respawn, as part
  of this change (see the open question above).
- Disclosing coverage on tools other than `get_references` and `workspace_symbol_search`
  (`go_to_implementation`, call hierarchy incoming calls, type hierarchy). They are candidates for
  a follow-up once the field shape is proven `[NEEDS CLARIFICATION: should the shared type be
  designed now so those tools can adopt it without a shape change?]`.
- The indexing-in-progress cause (already covered by bridge/006, bridge/012 and bridge/013) and any
  change to the 30 s bounded wait or the gating of `workspace_symbol_search`.
- A config key or environment variable that declares a server as open-document-only. Per-server
  behavior is not configured here.
- Detecting a server's indexing model from its capabilities or name. LSP defines no capability for
  it `[NEEDS CLARIFICATION: is any initialize-time signal usable to classify a server as
  open-document-only, or must the note apply to every server on an empty result?]`.
- Technical design: recorded in a plan after this spec is approved.

## 2. User Stories

### US-001: Tell "nothing indexed" from "no references"

AS AN AI coding agent deciding whether a symbol is unused
I WANT an empty `get_references` result to say what scope the answer covers
SO THAT I do not delete or rename a symbol whose references live in files the server never loaded.

**Acceptance criteria:**
```
GIVEN a routed server that indexes only documents sent with didOpen
  AND a symbol declared in file A whose references are in file B, which was never opened in this session
WHEN the agent calls get_references on the symbol in file A
THEN the result is empty
  AND it carries the coverage note, so it is distinguishable from an empty result without the note
```

### US-002: Same disclosure for workspace symbol search

AS AN AI coding agent searching for a symbol by name
I WANT an empty `workspace_symbol_search` result to carry the same coverage statement
SO THAT a missing symbol is not read as a nonexistent symbol.

**Acceptance criteria:**
```
GIVEN a routed server that indexes only opened documents
  AND a symbol defined in a file the session never opened
WHEN the agent calls workspace_symbol_search for that symbol
THEN the result is empty and carries the same coverage note as in US-001
```

### US-003: Restart does not silently shrink coverage

AS A user who restarted a language server and expects the previously touched files to still count
I WANT the note to reflect that the new server process has been sent fewer documents than before
SO THAT an empty answer right after `restart_server` is not read as authoritative.

**Acceptance criteria:**
```
GIVEN documents were synced to the routed server before restart_server
WHEN the agent calls workspace_symbol_search for a symbol in one of them after the restart,
     before any tool touches that file again, and the result is empty
THEN the coverage note reports that the current server process has been sent fewer documents
     than the session had synced before the restart
```

### US-004: Non-empty results and ready full-index servers pay nothing

AS A user of any server, including servers that index the whole workspace
I WANT non-empty results to be byte-identical to today and no extra LSP traffic
SO THAT the disclosure costs nothing where it is not needed.

**Acceptance criteria:**
```
GIVEN any routed server
WHEN get_references or workspace_symbol_search returns at least one item
THEN the serialized response is identical to the pre-change response
  AND no LSP request beyond the one the tool already sends is made
```

### US-005: A maintainer can reverse the decision cheaply

AS A maintainer
I WANT the choice between (a)+(c) and close-as-non-goal recorded with its reason
SO THAT the P4 trade-off is revisited on evidence, not re-derived.

**Acceptance criteria:**
```
GIVEN this spec is implemented or closed
WHEN a reader opens its Resolution section
THEN the chosen option, the measured payload delta and the rejected options with reasons are recorded
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN `get_references` or `workspace_symbol_search` returns an empty result AND the trigger rule (FR-003) holds THE SYSTEM SHALL attach a typed coverage note to the response | must |
| FR-002 | WHEN the result contains at least one item THE SYSTEM SHALL NOT attach the coverage note and SHALL serialize the response exactly as before | must |
| FR-003 | THE trigger rule SHALL be decided from state mcpls already holds (the document tracker's synced set for the routed server and its sync generation) and SHALL NOT require an additional LSP request `[NEEDS CLARIFICATION: exact rule. Candidates: (i) every empty result; (ii) an empty result when the routed server has been sent no document other than the queried one; (iii) an empty result when the number of synced documents is below a threshold. Rule (i) is the simplest and never under-warns but flags servers that index the whole workspace on every genuine empty]` | must |
| FR-004 | THE coverage note SHALL be a typed field on the typed response, defined once and shared by both tools, with a closed set of values and bounded size; it SHALL NOT be a free-form string | must |
| FR-005 | THE coverage note SHALL state the routed server's synced-document scope in machine-readable terms (at minimum: that the answer is limited to the server's own index, and the number of documents mcpls has sent to the current server process) `[NEEDS CLARIFICATION: exact members. Candidates: a closed enum scope value; a document count; a flag set when the sync generation changed since the session's first sync (restart or respawn seen)]` | must |
| FR-006 | WHEN the routed server's sync generation has changed since documents were last synced to it (restart or respawn) THE note SHALL say so, using the generation already tracked by `DocumentTracker`, without recording a new per-server history | should |
| FR-007 | THE note SHALL describe the routed server only; WHEN the request spans several servers (workspace symbol search routed to more than one) THE note SHALL be derived per routed server and merged by a rule stated in the plan `[NEEDS CLARIFICATION: merge rule for multi-server workspace_symbol_search: any-server-partial, or per-server entries bounded by a cap]` | must |
| FR-008 | WHEN the indexing state is `Loading` THE SYSTEM SHALL keep reporting indexing through the existing channel (`Error::WorkspaceIndexing` or `indexing_in_progress`) and SHALL NOT fold it into the coverage note | must |
| FR-009 | THE `tools/list` descriptions of the two tools SHALL state, in one canonical wording, that an empty result with the note reflects only what the routed server has indexed and is not proof of absence, and SHALL NOT state or imply that an empty result without the note is proof of absence | must |
| FR-010 | THE output schemas of the two tools SHALL document the field | must |
| FR-011 | THE user guide SHALL gain a short statement of the limitation (companion of option (c)): servers that index only opened documents, how `get_definition` can still resolve, and that touching the files of interest widens the answer | must |
| FR-012 | THE `.local/testing/` playbooks SHALL gain a case for an open-document-only server (empty references for an unopened file; empty workspace symbol after restart) and the coverage status SHALL be reset for the bridge subsystem | must |
| FR-013 | THE chosen option, the measured `tools/list` delta and the trigger rule SHALL be recorded in this spec's Resolution section | must |
| FR-014 | THE note SHALL NOT change the order, content or count of returned locations or symbols, nor the position conversion applied to them | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Output compatibility | Default output for non-empty results is byte-identical to the pre-change output; the field is omitted, not `null`, when absent. This deliberately differs from `indexing_in_progress`, which is always present, because the note applies only to empty results |
| NFR-002 | Latency | No extra LSP round-trip on the non-empty path and none on the empty path: the note is computed from tracker state held in memory ([[constitution]] section VI) |
| NFR-003 | Payload budget | The golden `tools/list` payload SHALL stay within the enforced budgets of [[mcp/014-tools-list-payload-size/spec\|mcp/014]] (total 135,500 B, per tool 9,000 B, schema descriptions 35,000 B as enforced in `mcp/schema_shape.rs` at HEAD). The field type SHALL be shared and its description kept to one canonical sentence per tool. `get_references` is the largest published tool in mcp/014's baseline (12,458 B before shaping), so its per-tool headroom SHALL be measured before design `[NEEDS CLARIFICATION: current measured headroom of the total and of get_references; mcp/014 recorded only 357 B of total headroom at the time of mcp/016, so the change may need an offsetting trim or a reviewed budget raise]` |
| NFR-004 | Type safety | The scope is a closed enum and the count a bounded integer on a typed response; no stringly-typed state and no untyped map, per [[constitution]] and the project's type-safety rule |
| NFR-005 | Consistency | The note is produced by one function shared by both tools, and its wording in descriptions is one canonical sentence; a third tool adopting it requires no new definition |
| NFR-006 | Concurrency | Reading tracker state for the note SHALL follow the existing lock-ordering discipline and SHALL NOT hold a tracker lock across an await |
| NFR-007 | Pre-1.0 | The response shape change is additive and is recorded in `CHANGELOG.md` with the PR link |
| NFR-008 | Documentation | New public items carry `///` docs with `# Examples` where non-trivial; `docs/user-guide/tools-reference.md` stays accurate |
| NFR-009 | Portability | Tests that fabricate servers or paths work on Windows and macOS (fake executables get an `.exe` name), per `CLAUDE.md` |
| NFR-010 | Server-agnostic | No server name or server-specific behavior appears in the bridge layer; the note is derived from tracker state, not from a list of known open-document-only servers |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Index coverage note (new) | Optional, typed statement attached to an empty references or workspace-symbol result | closed-enum scope; count of documents sent to the current server process; whether a restart or respawn was observed `[NEEDS CLARIFICATION: final members, see FR-005]` |
| Synced document set (existing) | Per-server set of documents mcpls has sent with `didOpen` | server id, paths, sync generation (`DocumentTracker`) |
| Sync generation (existing) | Per-server counter bumped by `forget_server` | integer, compared to detect a restart or respawn |
| Locations result / workspace symbol result (existing) | The tool outputs that gain the optional field | items, existing degradation fields |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Non-empty result | No note; response unchanged (FR-002, NFR-001) |
| Empty result, server indexes the whole workspace | With rule (i) the note appears although the empty is genuine: acceptable cost of never under-warning; with rules (ii) or (iii) the note may be suppressed (FR-003). The wording must not claim the server is open-document-only |
| Empty result, server `Loading` | `get_references` returns `Error::WorkspaceIndexing` as today; for `workspace_symbol_search` (ungated by bridge/006 and left so by bridge/013) the existing indexing disclosure is unchanged and the coverage note is independent of it (FR-008) `[NEEDS CLARIFICATION: workspace_symbol_search has no indexing flag today; does disclosing coverage here pull in the deferred question of gating or flagging it (#422)?]` |
| Empty result straight after `restart_server` or respawn | Note reports a changed sync generation (FR-006) |
| Server returns `null` instead of `[]` | Treated as empty, same as today (`unwrap_or_default`) |
| Server returns an error | Error path unchanged; no note |
| Several servers routed, one open-document-only | Note derived per routed server and merged by the plan's rule (FR-007) |
| `include_declaration = true` and the server returns only the declaration | Non-empty: no note, even though references in unopened files may be missing. Documented in the user guide (FR-011); out of scope for this field |
| Client ignores the note | Behavior equals today; the note is advisory, no gate is added |
| Document tracker at its configured limit ([[bridge/005-expose-document-tracker-limits/spec\|bridge/005]]) | The count is the tracker's real synced count; eviction is visible as a lower count, not hidden |
| Note present but server in fact indexed everything | The caller is told the answer is bounded by the server's index, which is true of every server answer; no claim of incompleteness beyond that |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Reproduction from the finding against a test double that answers `[]` for symbols outside opened documents: `get_references` on a symbol whose references are in an unopened file | Empty result carries the note; a caller reading only the typed fields can distinguish it from an empty result of a server that opened the file |
| SC-002 | `workspace_symbol_search` for a symbol in a file opened before `restart_server`, called right after the restart | Empty result carries the note with the restart indicator |
| SC-003 | Non-empty results of both tools, 10 repetitions each, compared byte for byte with the pre-change build | Identical |
| SC-004 | LSP requests counted per call on the non-empty and empty paths | Equal to the pre-change count |
| SC-005 | Golden `tools/list` payload | Within all three enforced budgets, with the measured delta recorded in the Resolution section |
| SC-006 | Rule coverage by tests | A unit test per trigger-rule branch and a golden-snapshot update for the two tools' schemas |
| SC-007 | Decision record | Resolution section states option chosen, delta and rejected options (US-005) |

## 8. Agent Boundaries

### Always (without asking)
- Derive the note from existing tracker state in one shared function; do not add a second definition per tool.
- Keep the field optional and omitted when absent so non-empty output stays byte-identical.
- Measure the `tools/list` delta against the enforced budgets before and after, and regenerate `tool_surface.json` through the existing mechanism.
- Run the full pre-commit suite (fmt, clippy, nextest, rustdoc, `cargo check -p mcpls --locked`, the `fuzz` lockfile check) and update `CHANGELOG.md` with the PR link.
- Update the `.local/testing/` playbooks and reset the coverage status for the affected subsystem.

### Ask First
- Raising any `mcp/014` budget constant or trimming unrelated schema text to make room.
- Choosing trigger rule (i) over (ii) or (iii), since it decides how often the note fires.
- Extending the note to tools beyond the two named.
- Adopting option (b), eager re-sync, instead of or in addition to (a).
- Any new dependency.

### Never
- Open documents the client did not ask about in order to widen a server's index.
- Add an extra LSP round-trip on the non-empty path, or a new timeout or poll loop.
- Emit the note on a non-empty result, or serialize it as `null`.
- Present the absence of the note as proof that an empty result is genuine.
- Add a config key, environment variable or server-name list that classifies servers as open-document-only.
- Change the gate or flag behavior of `get_references`, `workspace_symbol_search`, `get_diagnostics` or the `indexing_in_progress` channel.

## 9. Open Questions

> [!question] To resolve before the plan
> - [NEEDS CLARIFICATION: trigger rule, FR-003 (i), (ii) or (iii)]
> - [NEEDS CLARIFICATION: exact members of the note, FR-005]
> - [NEEDS CLARIFICATION: multi-server merge rule for `workspace_symbol_search`, FR-007]
> - [NEEDS CLARIFICATION: a live reproduction server for the open-document-only behavior, section 1]
> - [NEEDS CLARIFICATION: an initialize-time signal to classify servers, if any, Out of Scope]
> - [NEEDS CLARIFICATION: measured `tools/list` headroom, NFR-003]
> - [NEEDS CLARIFICATION: whether option (b) is a wanted follow-up or rejected]
> - [NEEDS CLARIFICATION: whether the shared type should be designed now for adoption by `go_to_implementation` and the hierarchy tools]
> - [NEEDS CLARIFICATION: interaction with the deferred gating or flagging of `workspace_symbol_search` (#422)]

## 10. See Also

- [[constitution]] — project principles
- [[MOC-specs]] — all specifications
- [[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]] — the gate and the false-negative precedent (#420)
- [[bridge/013-indexing-state-on-ungated-prepare-tools/spec|bridge/013]] — the `indexing_in_progress` disclosure pattern this spec parallels
- [[bridge/012-indexing-gate-after-restart/spec|bridge/012]] — state of a replaced server
- [[bridge/002-document-tracker-synchronization/spec|bridge/002]] — lazy `didOpen` and sync generation
- [[bridge/005-expose-document-tracker-limits/spec|bridge/005]] — tracker limits that bound option (b)
- [[mcp/014-tools-list-payload-size/spec|mcp/014]] — `tools/list` size budget
- Code: `crates/mcpls-core/src/bridge/translator/navigation.rs` (`handle_references`), `crates/mcpls-core/src/bridge/state.rs` (`DocumentTracker`, `forget_server`), `crates/mcpls-core/src/bridge/translator/restart.rs`, `crates/mcpls-core/src/bridge/translator/respawn.rs`, `crates/mcpls-core/src/mcp/schema_shape.rs` (budget constants), `crates/mcpls-core/src/mcp/tool_surface.json`
