---
aliases:
  - Speculative edit diagnostic preview
  - Edit impact preview
  - Speculative edit parity
tags:
  - sdd
  - spec
  - research
  - mcp
  - diagnostics
  - competitor-gap
created: 2026-10-04
status: draft
related:
  - "[[constitution]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp-resources-diagnostics]]"
  - "[[mcp/003-mcp-2026-stateless-adoption/spec|mcp-2026-stateless-adoption]]"
  - "[[mcp/005-tool-capability-discoverability/spec|tool-capability-discoverability]]"
  - "[[bridge/002-document-tracker-synchronization/spec|document-tracker-synchronization]]"
  - "[[bridge/004-get-diagnostics-flycheck-gap/spec|get-diagnostics-flycheck-gap]]"
  - "[[bridge/006-lsp-indexing-readiness-gate/spec|lsp-indexing-readiness-gate]]"
  - "[[bridge/007-enclosing-symbol-context/spec|enclosing-symbol-context]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp-server-lifecycle-and-respawn]]"
---

# Feature: Read-Only Preview of the Diagnostic Impact of a Proposed Edit

> [!info] Metadata
> **Type**: research (competitor gap, scope decision required)
> **Priority**: P4
> **Related issues**: #570; the optional blast-radius group (section 3, group C) relates to #565
> (see [[bridge/007-enclosing-symbol-context/spec|bridge/007]])

## 1. Overview

### Problem Statement

mcpls is a read-only bridge. `rename_symbol` and `format_document` return text edits without
applying them (`crates/mcpls-core/src/bridge/translator/edits.rs`), `get_diagnostics` reports the
current diagnostics of a file as the server sees it now, and the project deliberately stays out of
file editing (the competitor-gap table lists file-editing tools as out of scope).

An AI agent that has a proposed change in hand (a rename edit returned by mcpls, a formatter
result, or a patch it wrote itself) therefore cannot ask the question it most wants answered before
committing to the change: **"if I apply this edit, which new errors appear?"** Today the only way is
to write the change to disk, wait for the server to re-analyze, poll `get_diagnostics`, and revert
if the result is bad. That loop has costs the agent cannot avoid:

- it mutates the working tree (file watchers, build tools, formatters-on-save, git status noise)
  just to ask a question;
- it needs an undo step the agent must get right, and a crash between apply and revert leaves the
  tree dirty;
- it cannot be done at all by a client that has no write access, or in a host that requires user
  approval for every write.

**Prior art.** One comparable code-intelligence bridge markets "speculative execution" as a
distinguishing feature:

- a single-edit preview applies a proposed edit to an in-memory document version
  (`textDocument/didChange` without touching disk), waits for diagnostics, returns the diagnostic
  delta, then discards the in-memory change;
- a chain variant evaluates a sequence of dependent edits and reports the first step that
  introduces an error;
- an explicit multi-call session (create / evaluate / commit / discard) provides the same
  capability across several calls;
- a separate composite operation returns in one call all exports of a file plus all of their
  callers, partitioned into test and non-test.

Only that one bridge was found to offer it, which is why the finding is P4: a niche capability
that makes an agent's edit-verify loop cheaper and safer, not a correctness defect. No mcpls result
is wrong or silently incomplete today.

**Why this is a scope decision and not a simple port.** The capability is cheap to describe and
expensive to host, because it is the first place mcpls would *send content to a language server that
is not the content on disk*. That touches several established guarantees:

- **The read-only stance.** Every tool is classified read-only at the router level
  ([[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]] FR-009). A preview never writes a file, but
  it does change language-server session state for a time. Whether that still counts as read-only,
  and what annotation hints it may carry, must be decided, not assumed.
- **The document tracker.** `DocumentTracker` ([[bridge/002-document-tracker-synchronization/spec|bridge/002]])
  derives a document's content from disk, bumps one monotonic version per path, tracks the last
  version synced to each server, serializes `ensure_open` per path, and evicts by LRU with in-flight
  pinning. It has no concept of a content overlay that differs from disk. A preview must not corrupt
  that state, must not be evicted mid-evaluation, and must not leave a server holding a divergent
  in-memory document if the preview is never discarded (client cancel, disconnect, timeout, server
  respawn).
- **Concurrent calls on the same path.** While a preview overlay exists, any other tool call
  (hover, definition, references) touching the same file would see the speculative content unless
  the overlay is isolated or the path is held exclusively for the duration, which blocks those calls.
- **Diagnostics caching.** Diagnostics are push-based in LSP and cached in `NotificationCache`, which
  also feeds `get_cached_diagnostics` and the MCP resource subscriptions
  ([[mcp/002-mcp-resources-diagnostics/spec|mcp/002]]). Diagnostics published for a speculative
  document must never be mistaken for real diagnostics by those consumers, and must not trigger
  `notifications/resources/updated` for subscribers.
- **What a server can see without saving.** Servers analyze open-document content on `didChange`, but
  some diagnostics only exist after a build triggered by save (rust-analyzer's `cargo check`
  "flycheck", see [[bridge/004-get-diagnostics-flycheck-gap/spec|bridge/004]]). A preview could
  therefore under-report compiler errors that the write-to-disk loop would surface.
  `[NEEDS CLARIFICATION: verify empirically per server class which diagnostics appear on didChange alone; this bounds how trustworthy "no new errors" can be.]`
- **Indexing readiness.** Per [[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]], an empty
  result during indexing is indistinguishable from "no problems". A preview that reports "no new
  errors" from a server still indexing would be exactly the silent-misleading-result class that
  spec exists to prevent.
- **Edit staleness.** An edit computed against content X applied to current content Y is a different
  edit; the preview must say what it was applied to.
- **Statelessness.** Multi-call sessions (`create` / `evaluate` / `commit` / `discard`) are server-held
  state across MCP calls, which cuts against the direction tracked in
  [[mcp/003-mcp-2026-stateless-adoption/spec|mcp/003]].

### Options

The spec deliberately lists the options rather than choosing one; the choice is the first
deliverable (FR-001).

| Option | Description | What it costs | What it gives up |
|--------|-------------|---------------|------------------|
| A. Remain read-only, document the loop | No new capability. Document the recommended agent loop for verifying an edit (apply the edit with the host's own editor, poll `get_diagnostics`, revert on failure), plus its caveats, in user docs and tool descriptions. Record the non-goal here and in the competitor-gap table | Documentation only | The agent keeps paying the dirty-working-tree cost; no capability parity. Gains: full compiler (flycheck) diagnostics, because the file really is saved |
| B. Stateless single-call preview | One new read-only-class tool: the caller supplies a typed edit proposal; mcpls applies it to an in-memory overlay on the routed server, waits for diagnostics to settle, returns a typed diagnostic delta, and restores the server's view before returning, all within one MCP call | New hosting logic in the tracker for a call-scoped overlay with guaranteed restore; new typed DTOs; a "settled" definition per server | No dependent-edit chains or commit; limited to diagnostics visible without save |
| C. Chain and session | Option B plus either a multi-edit "chain" input evaluated in order (reporting the first step that introduces an error) or an explicit multi-call session (create / evaluate / commit / discard) | Everything in B plus cross-call server-held state, expiry, and cleanup on disconnect; `commit` would be the first write path in a read-only bridge | Conflicts with the read-only stance (commit) and the stateless direction (sessions). A single-call multi-step chain avoids both |
| D. Compose onto existing edit-producing tools | An opt-in input on `rename_symbol` / `format_document` that, in the same call, previews the diagnostic delta of the edit the tool itself just produced | Smallest typed surface: the edit never crosses the MCP boundary again | Covers only mcpls-generated edits, not patches the agent wrote; couples edit generation with verification |

> [!tip] Suggested direction (non-binding, needs maintainer approval)
> Option A is the baseline regardless of the outcome, because the documented loop is correct today
> and its caveats (flycheck diagnostics, file-watcher side effects) are worth stating. If a
> capability is adopted at all, B (optionally with a single-call chain from C) is the smallest shape
> that keeps mcpls stateless and write-free; sessions with `commit` are recommended against.

### Goal

Either (a) the project has made and recorded an explicit decision to remain read-only with the
recommended agent loop documented, or (b) an AI agent can obtain, from one MCP call and without any
write to disk, a typed, honest report of how the diagnostics of the affected files would change if a
proposed edit were applied, with the language server's view of every touched document guaranteed to
return to the on-disk truth afterwards. In both cases an optional single-call composite gives an
agent a file's exported symbols with their callers (group C).

### Out of Scope

- Applying, writing, or persisting any edit to disk (file-editing tools remain out of
  scope; Option C's `commit` is listed only to be evaluated and rejected or deferred).
- Generating the proposed edit: rename and format edits already come from `rename_symbol` and
  `format_document`; code-action edits from `get_code_actions`.
- Changing `get_diagnostics`, `get_cached_diagnostics`, or the diagnostics resources for existing,
  non-preview calls.
- Forcing a language server to run a build (flycheck) on speculative content, or any
  server-specific build integration.
- Running tests, linters, or build tools outside the language server.
- Technical design (plan phase).

> [!note] Constitution and type safety
> Section VI (no additional LSP round trips on existing flows) is satisfied only if every new LSP
> traffic path is reachable solely through a new, explicit entry point (FR-021). Section VII
> (simplicity, one pattern per problem) argues for a single preview mechanism, not three variants.
> The constitution has no explicit type-safety clause; the maintainer's standing rule (illegal states
> unrepresentable, no stringly-typed data, closed enums for outcomes and reasons) is expressed in
> NFR-006 to NFR-008 and in the data model.

## 2. User Stories

### US-001: Agent checks a rename before committing to it

AS AN AI coding agent that just received a `rename_symbol` edit
I WANT to know which diagnostics the edit would introduce or resolve across the touched files
SO THAT I apply it only if it does not break the build, without dirtying the working tree to find out.

**Acceptance criteria:**
```
GIVEN a rename edit touching three files, one of which would gain an unresolved-name error
WHEN the agent requests a diagnostic preview of that edit
THEN the response lists the introduced diagnostic with its file and range
  AND no file on disk has changed
  AND get_diagnostics for the same files afterwards matches its output from before the preview
```

### US-002: Agent validates a hand-written patch

AS AN AI coding agent that wrote its own multi-file patch
I WANT the same preview for an edit I authored
SO THAT the check is not limited to edits mcpls itself generated.

**Acceptance criteria:**
```
GIVEN an edit proposal not produced by any mcpls tool, expressed in the preview's typed input
WHEN the agent requests a preview
THEN the same delta report is returned as for an mcpls-generated edit
```
(Applies only if Option B or C is adopted; Option D does not satisfy this story.)

### US-003: Agent gets an honest answer when a preview cannot be trusted

AS AN AI coding agent
I WANT the response to say when the verdict is incomplete (server still indexing, diagnostics did not
settle, server lacks the capability, edit was stale, file outside the workspace)
SO THAT "no new errors" is never an artifact of the preview not having been computed.

**Acceptance criteria:**
```
GIVEN a server still indexing, or one whose diagnostics did not settle within the bound
WHEN the agent requests a preview
THEN the response carries an explicit typed state for the affected files naming the reason
  AND it is structurally distinct from a settled result with an empty delta
```

### US-004: Operator is certain a preview leaves no trace

AS A mcpls operator
I WANT every preview to leave the language server, the document tracker, the diagnostics cache, and
resource subscribers exactly as before, even if the client cancels, disconnects, or the server crashes
mid-preview
SO THAT a speculative feature can never leave a server holding a document that exists nowhere on
disk.

**Acceptance criteria:**
```
GIVEN a preview in progress
WHEN the client cancels the call, the call times out, or the routed server is respawned
THEN the overlay is discarded, the server's view of every touched document returns to disk content,
  and a subsequent tool call on those files behaves as if the preview never happened
```

### US-005: Existing clients are unaffected

AS A maintainer of an existing MCP client
I WANT output and LSP traffic of all existing tool calls to be unchanged
SO THAT this feature cannot regress anything that works today.

**Acceptance criteria:**
```
GIVEN any existing tool call
WHEN made before and after this feature ships
THEN the response is byte-identical and the LSP request count is unchanged
```

### US-006: Agent assesses the blast radius of changing a file in one call (group C)

AS AN AI coding agent about to change a file
I WANT its exported symbols together with their callers, with test and non-test callers separated, in
one call
SO THAT I do not chain `get_document_symbols`, `prepare_call_hierarchy`, and `get_incoming_calls`
per symbol myself.

**Acceptance criteria:**
```
GIVEN a file exporting four symbols with callers in production and test code
WHEN the agent requests the blast radius of that file
THEN each exported symbol is returned with its callers partitioned into test and non-test, bounded
  and with explicit truncation and unavailability states
```

## 3. Functional Requirements

Use EARS notation. Prefix with FR-NNN. Group A is unconditional. Group B applies only if the
FR-001 decision adopts a capability (Option B, C's single-call chain, or D). Group C is a separate,
smaller, independently optional group.

### Group A: Decision and baseline (unconditional)

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM's maintainers SHALL record, before any implementation, which option of section 1 is adopted (A, B, C, or D), with the rationale, in the plan for this spec and in the competitor-gap table | must |
| FR-002 | WHEN Option A is adopted (or in addition to any other option) THE SYSTEM SHALL document the recommended write-then-poll-then-revert agent loop for verifying an edit, including its caveats (working-tree side effects, flycheck diagnostics requiring save, indexing readiness), in the user documentation | must |
| FR-003 | WHEN any option other than A is adopted THE SYSTEM SHALL keep Option A's documented loop valid and not describe the preview as a replacement for it where flycheck-class diagnostics matter | should |

### Group B: Speculative preview (conditional on FR-001)

| ID | Requirement | Priority |
|----|------------|----------|
| FR-010 | WHEN a caller submits an edit proposal for preview THE SYSTEM SHALL return, per affected file, the diagnostics introduced by the edit, the diagnostics resolved by it, and the count of unchanged diagnostics, computed against a baseline taken from the same source and mode as the post-edit result | must |
| FR-011 | WHEN a preview runs THE SYSTEM SHALL NOT write, create, rename, or delete any file on disk | must |
| FR-012 | WHEN a preview completes, fails, times out, is cancelled by the client, or its routed server respawns THE SYSTEM SHALL restore the server's view of every touched document to the on-disk content before the call returns or, for cancellation and respawn, before any later call can observe the overlay | must |
| FR-013 | WHEN a preview runs THE SYSTEM SHALL ensure no other tool call can observe the speculative content of a touched document, either by isolating it or by holding the document exclusively for a bounded time, `[NEEDS CLARIFICATION: isolation versus exclusive hold; exclusive hold makes concurrent hover/definition calls on that file wait, and the trade-off is a plan decision]` | must |
| FR-014 | WHEN a preview runs THE SYSTEM SHALL keep speculative diagnostics out of the diagnostics cache that serves `get_cached_diagnostics` and the MCP diagnostics resources, and SHALL NOT emit resource-updated notifications caused by them | must |
| FR-015 | WHEN the edit proposal touches a file outside every configured workspace root, or a file the tool's path validation would refuse THE SYSTEM SHALL refuse that file with a typed reason and SHALL NOT open it | must |
| FR-016 | WHEN the edit proposal references a document version or content the server's current view no longer matches (stale edit) THE SYSTEM SHALL refuse or report the staleness with a typed reason rather than silently applying the edit to different content | must |
| FR-017 | WHEN the routed server is still indexing, lacks the capability needed to produce diagnostics for the document, or its diagnostics do not settle within the bound THE SYSTEM SHALL report that file's verdict as not computed or unavailable with a typed reason, never as an empty delta | must |
| FR-018 | WHEN the proposal touches more files than the document-tracker limits allow, or more than a fixed per-call file cap THE SYSTEM SHALL refuse or truncate explicitly with a typed reason, and SHALL NOT evict documents with in-flight handlers `[NEEDS CLARIFICATION: cap value]` | must |
| FR-019 | WHEN an edit's ranges are expressed in MCP coordinates THE SYSTEM SHALL convert them through `bridge/encoding.rs` and propagate the existing `positions_degraded` signal where conversion is inexact | must |
| FR-020 | WHEN a preview of an edit introduces diagnostics in files other than those the edit touches (dependents) THE SYSTEM SHALL either report them for dependents the server already tracks as open, or state in the result which scope was observed `[NEEDS CLARIFICATION: dependents the server has not opened are invisible to a didChange-only preview; is the observed scope reported or must the preview open dependents, which costs more LSP traffic?]` | should |
| FR-021 | WHEN a preview is not requested THE SYSTEM SHALL issue no overlay `didChange`, no extra document open, and no new output field on any existing tool | must |
| FR-022 | WHEN a chain of dependent edits is submitted in one call (Option C, single-call form) THE SYSTEM SHALL evaluate the steps in order and report the first step whose cumulative effect introduces an error diagnostic, without server-held state surviving the call | could |
| FR-023 | WHEN the capability is adopted THE SYSTEM SHALL describe its input and output in the published `inputSchema` and `outputSchema` (`crates/mcpls-core/src/mcp/tool_surface.json`), cover the new entry point in `get_tool_support` ([[mcp/005-tool-capability-discoverability/spec\|mcp/005]]), and classify its annotations explicitly | must |
| FR-024 | WHEN the capability is adopted as a new tool THE SYSTEM SHALL accept as input the same typed edit shape that `rename_symbol` and `format_document` return, so an edit produced by mcpls can be previewed without reshaping | should |
| FR-025 | WHEN diagnostics are compared before and after THE SYSTEM SHALL decide that two diagnostics are "the same" by a defined identity that tolerates the position shift the edit itself causes `[NEEDS CLARIFICATION: identity definition, for example code plus message plus range translated through the edit; without it every diagnostic below an insertion reads as both resolved and introduced]` | must |
| FR-026 | WHEN Option C's explicit multi-call session (create / evaluate / commit / discard) is proposed THE SYSTEM SHALL treat it as out of scope for this spec unless an amendment resolves the statelessness and `commit` write-path conflicts of section 1 | must |

### Group C: Blast-radius composite (separate, optional)

Independent of groups A and B; may be adopted, deferred, or rejected on its own.

| ID | Requirement | Priority |
|----|------------|----------|
| FR-030 | WHEN a caller requests the blast radius of a file THE SYSTEM SHALL return the file's exported (top-level, externally visible) symbols, and for each the callers found through call hierarchy, in one MCP call | could |
| FR-031 | WHEN callers are returned THE SYSTEM SHALL partition them into test and non-test, with the classification rule explicit and typed `[NEEDS CLARIFICATION: how a caller is classified as test code, since LSP carries no such signal; candidates are path conventions per language or a configured pattern list]` | could |
| FR-032 | WHEN the routed server lacks call-hierarchy or document-symbol support, or a per-symbol request fails THE SYSTEM SHALL return the remaining symbols and mark the affected ones unavailable with a typed reason, never failing the whole call | could |
| FR-033 | WHEN the number of exported symbols or callers exceeds the call's bounds THE SYSTEM SHALL truncate with an explicit result-level state, reusing the shared item budget and `truncated` conventions | could |
| FR-034 | WHEN the composite computes callers THE SYSTEM SHALL reuse the existing call-hierarchy and document-symbol translation paths and symbol-containment logic of [[bridge/007-enclosing-symbol-context/spec\|bridge/007]] rather than a parallel implementation | could |
| FR-035 | WHEN the composite is not requested THE SYSTEM SHALL add no LSP traffic to any existing flow; the fan-out per call is bounded by a fixed cap of symbols and requests `[NEEDS CLARIFICATION: caps]` | could |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Safety | A preview SHALL be unable to leave a language server holding a document whose content differs from disk after the call (and any cancellation or respawn cleanup) has finished. This holds under client cancel, disconnect, timeout, panic in the handler, and server crash (FR-012) |
| NFR-002 | Constitution VI | The only LSP traffic added is that of an explicit preview or blast-radius call, bounded per call by fixed caps; no existing flow gains a round trip (FR-021, FR-035) |
| NFR-003 | Performance | A preview SHALL have an explicit, bounded settle deadline; one slow server SHALL NOT multiply call latency by the file count `[NEEDS CLARIFICATION: deadline value and whether it is per call or per file]` |
| NFR-004 | Robustness | One failing server or file SHALL NOT affect other files, servers, or the document tracker's state for unrelated paths (constitution I, graceful degradation); no `unwrap`/`expect` on preview paths |
| NFR-005 | Security | A preview SHALL NOT widen the set of files mcpls opens or reads beyond what existing path validation and workspace-root rules permit (FR-015); server-supplied URIs are not trusted for file reads |
| NFR-006 | Type safety | The edit proposal SHALL be a typed structure (file, range, replacement text), never a free-form patch string parsed at runtime, and ranges SHALL reuse the existing MCP position type |
| NFR-007 | Type safety | The preview outcome per file SHALL be a closed sum: settled with a typed delta, not computed with a reason enum, unavailable with a reason enum, refused with a reason enum. "No new errors" SHALL be representable only by a settled outcome with an empty introduced set, never by `null`, an empty list, or an absent field standing for several meanings. Reasons are enums, not free text |
| NFR-008 | Type safety | The delta SHALL be three typed collections (introduced, resolved) plus an unchanged count, using the existing `Diagnostic` DTO and severity type; the diagnostic identity of FR-025 SHALL be a named type, not an ad hoc string key |
| NFR-009 | Consistency | A single preview path SHALL serve every edit source (constitution VII); position conversion goes through `bridge/encoding.rs` only (constitution IV) |
| NFR-010 | Determinism | Identical inputs against an unchanged workspace and server state SHALL produce identical delta ordering, independent of server response ordering |
| NFR-011 | Protocol compatibility | Output remains schema-valid, structured output validated against the published `outputSchema`; existing tools' schemas are unchanged |
| NFR-012 | Testability | Overlay isolation, restore-on-cancel, restore-on-respawn, stale edit, over-cap, out-of-workspace, indexing, and not-settled cases SHALL each be coverable with the in-process fake-server harness without a real language server; at least one live check per server class is recorded in the testing documents under `.local/testing/` |
| NFR-013 | Honesty about coverage | The tool description and documentation SHALL state that diagnostics requiring a save or build (flycheck class) may be absent from a preview, so a clean preview is not a build guarantee |

## 5. Data Model

Conceptual entities only; concrete types belong to the plan.

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Edit Proposal | A set of text edits across files, to be evaluated but never written | per file: path, ordered text edits (MCP range, replacement text); optional precondition on the document content or version it was computed against |
| Preview Baseline | The diagnostics of each touched file before the edit, taken from the same source and mode as the post-edit result | per file: diagnostics, source (pull or push), indexing state at capture |
| Diagnostic Identity | The notion of "same diagnostic" across the edit | severity, code, message, range translated through the edit (FR-025) |
| Diagnostic Delta | Outcome of comparison for one file | introduced (diagnostics), resolved (diagnostics), unchanged count |
| Preview Outcome (per file) | Closed set describing what is known for one file | settled(delta); not computed(reason: indexing, did not settle, deadline); unavailable(reason: capability absent, request failed, server respawned); refused(reason: out of workspace, stale proposal, tracker limit, file cap, invalid edit) |
| Preview Summary | Result-level statement about completeness | files previewed versus skipped, whether positions were degraded, observed scope (FR-020), whether flycheck-class diagnostics were out of reach |
| Speculative Overlay | Call-scoped, in-memory replacement of a document's content on one server | path, server, overlay content, owning call; lifetime strictly within one call or its cleanup (never survives the call) |
| Blast Radius Entry (group C) | One exported symbol with its callers | symbol identity (name path, kind, range), callers partitioned into test and non-test, per-symbol state (resolved, truncated, unavailable with reason) |

Existing entities reused unchanged: `WorkspaceEditDescription` / `TextEdit` (edit shape), `Diagnostic`,
`Position`, `PositionDegradation`, `IndexingState`, the normalized-item budget, `DocumentState`
(version and per-server sync history), and `NotificationCache` (which must not receive speculative
diagnostics).

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Server is still indexing | File outcome "not computed: indexing"; never an empty delta (FR-017, spec bridge/006) |
| Diagnostics do not settle within the bound | "not computed: did not settle" with the deadline reported; overlay already discarded (FR-012) |
| Server answers diagnostics only by push, not pull | The comparison uses the same mode for baseline and post-edit result; if the mode cannot produce a trustworthy post-edit result, "unavailable" with reason |
| Compiler-class (flycheck) diagnostics exist only after save | Preview may omit them; the summary and description say so (NFR-013); Option A loop remains the answer for that class |
| Edit inserts lines above existing diagnostics | Existing diagnostics shift; with a defined identity they are "unchanged", not "resolved plus introduced" (FR-025) |
| Edit proposal computed against an older document state | Refused or reported stale with a typed reason (FR-016) |
| Edit touches a file outside the workspace roots | That file refused with a typed reason; no file opened (FR-015) |
| Edit touches more files than tracker limits or per-call cap | Explicit refusal or truncation; documents with in-flight handlers are never evicted (FR-018, bridge/002 FR-011) |
| Edit makes the document unparsable (syntax errors galore) | Valid outcome: introduced diagnostics may be many; bounded by the existing item budget and `truncated` signal |
| Client cancels or disconnects mid-preview | Overlay discarded before any later call can observe it; server view returns to disk (FR-012, NFR-001) |
| Routed server crashes or respawns mid-preview | Respawn already forgets that server's sync history (bridge/002 `forget_server`); preview outcome "unavailable: server respawned"; no stale overlay on the new process |
| Another tool call touches the same file during a preview | Not served speculative content (FR-013); either isolated or waits for a bounded time |
| File changes on disk during the preview (concurrent external edit) | Preview reports against the content it started from, and the outcome says the disk changed `[NEEDS CLARIFICATION: detect and refuse, or tolerate]` |
| Two concurrent previews on the same file | Serialized or refused with a typed reason; they never interleave overlays |
| Empty edit proposal | Settled outcome with empty delta for zero files, or input validation error; never a silent no-op `[NEEDS CLARIFICATION: choose]` |
| Edit is a no-op (replacement equals existing text) | Settled, empty delta |
| Routed server does not support `didChange` incremental or full sync as expected | "unavailable: capability absent" |
| Subscribers to diagnostics resources during a preview | No resource-updated notification caused by speculative diagnostics (FR-014) |
| Blast radius: server lacks call hierarchy | Symbols returned with callers "unavailable: capability absent" (FR-032) |
| Blast radius: file with hundreds of exports | Truncated at the cap with an explicit state (FR-033, FR-035) |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | The scope decision was made with maintainer approval and recorded in the plan and competitor-gap table before implementation | Recorded (FR-001) |
| SC-002 | Calls and writes an agent needs to learn "what errors would this edit introduce" (if B/C/D adopted) | 1 call, 0 disk writes, versus apply + wait + poll + revert today |
| SC-003 | After any preview (including cancelled, timed-out, and respawn cases) the server's document view equals disk content | 100% in the fault-injection tests (NFR-001) |
| SC-004 | Default output and LSP traffic of every existing tool call | Unchanged: byte-identical response and identical request count versus the pre-change baseline (golden tests) |
| SC-005 | Files with a determinate typed outcome across settled, indexing, did-not-settle, capability-absent, stale, out-of-workspace, and over-cap cases | 100%: no file carries an implicit or null outcome |
| SC-006 | Agreement between a settled preview and the real write-then-poll result for the same edit, on the server-native diagnostic class | `[NEEDS CLARIFICATION: target and corpus; measured live against rust-analyzer and one non-flycheck server]` |
| SC-007 | Documentation of the Option A loop and its caveats published (FR-002) | Present in user docs |
| SC-008 | Blast radius (if adopted): calls to learn the callers of a file's exports | 1, versus 1 + S + S per-symbol calls for S symbols today |
| SC-009 | Live verification recorded in `.local/testing/` playbooks and coverage status | One per server class, for each adopted group |

## 8. Agent Boundaries

### Always (without asking)
- Keep every existing tool's output and LSP traffic unchanged (FR-021, SC-004).
- Reuse the existing edit DTOs, diagnostic DTOs, position-encoding layer, document tracker methods,
  and item budget; add no parallel implementation (constitution VII).
- Model outcomes, reasons, and the edit proposal as typed values and closed enums (NFR-006 to
  NFR-008); update `tool_surface.json` and its consistency tests together with the DTOs.
- Run the full pre-commit check suite from the constitution before any commit.

### Ask First
- Choosing among options A to D (FR-001), including adopting any capability at all.
- Introducing a content overlay concept into `DocumentTracker`, or changing its eviction or pinning
  rules.
- Choosing the settle deadline, file caps, the diagnostic identity, and the isolation versus
  exclusive-hold model (FR-013, FR-025, NFR-003).
- Choosing the annotation hints for a preview tool, given the router-level read-only classification.
- Adding the blast-radius composite (group C), a new dependency, or any multi-call session.

### Never
- Write, create, rename, or delete a file on disk as part of a preview.
- Add a `commit` or any apply path to mcpls through this feature.
- Leave a server's view of a document different from disk after a call has finished.
- Let speculative diagnostics reach `NotificationCache`, `get_cached_diagnostics`, or resource
  subscribers.
- Report a not-computed, unavailable, or refused file as an empty delta.
- Open or read a file outside what existing path validation allows.
- Weaken or bypass the indexing-readiness gate or `Translator::require_capability`.

## 9. Open Questions

- [NEEDS CLARIFICATION: which option (A, B, C single-call chain, D) is adopted; FR-001. Suggested
  direction in section 1: A as baseline, B if any capability.]
- [NEEDS CLARIFICATION: does a preview still qualify for the router-level read-only classification
  (mcp/001 FR-009) given it transiently changes language-server session state, and which
  `ToolAnnotations` hints apply (read-only, idempotent, open-world)?]
- [NEEDS CLARIFICATION: empirical coverage; which diagnostics appear on `didChange` alone for
  rust-analyzer, pyright, typescript-language-server, gopls, and how much does a preview under-report
  relative to write-to-disk (bounds NFR-013 and SC-006)?]
- [NEEDS CLARIFICATION: isolation versus exclusive hold for concurrent calls on a touched file
  (FR-013).]
- [NEEDS CLARIFICATION: diagnostic identity (FR-025) and the settle definition: how does mcpls know a
  server has finished re-analyzing, given push diagnostics have no completion marker and pull
  diagnostics may be unsupported?]
- [NEEDS CLARIFICATION: scope of observed diagnostics for dependents the server has not opened
  (FR-020).]
- [NEEDS CLARIFICATION: settle deadline, per-call file cap, and chain step cap values.]
- [NEEDS CLARIFICATION: behavior when the file changes on disk or the proposal is stale; refuse versus
  tolerate (FR-016, section 6).]
- [NEEDS CLARIFICATION: Option D coupling; if edit-producing tools gain a preview input, does it share
  the closed-enum opt-in pattern recommended in bridge/007, so later levels are additive?]
- [NEEDS CLARIFICATION: group C test versus non-test classification rule (FR-031), and whether group C
  should be filed as its own issue and spec if adopted.]
- [NEEDS CLARIFICATION: whether the competitor-gap table should record "explicit non-goal" for
  sessions (`create` / `commit` / `discard`) now, independent of the chosen option.]

## 10. See Also

- [[constitution]] — project principles (VI: no extra LSP round trips on existing flows; VII:
  one pattern per problem)
- [[MOC-specs]] — all specifications
- [[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]] — read-only
  classification (FR-009), `tool_surface.json` and `outputSchema` conventions
- [[mcp/002-mcp-resources-diagnostics/spec|mcp-resources-diagnostics]] — diagnostics cache and
  subscriptions speculative diagnostics must not reach
- [[mcp/003-mcp-2026-stateless-adoption/spec|mcp-2026-stateless-adoption]] — statelessness direction
  that argues against multi-call sessions
- [[mcp/005-tool-capability-discoverability/spec|tool-capability-discoverability]] — `get_tool_support`
  must cover any new entry point
- [[bridge/002-document-tracker-synchronization/spec|document-tracker-synchronization]] — per-path
  locking, version and per-server sync tracking, eviction and pinning an overlay must respect
- [[bridge/004-get-diagnostics-flycheck-gap/spec|get-diagnostics-flycheck-gap]] — why save-triggered
  compiler diagnostics may be invisible to a preview
- [[bridge/006-lsp-indexing-readiness-gate/spec|lsp-indexing-readiness-gate]] — empty-during-indexing
  hazard a preview must not reproduce
- [[bridge/007-enclosing-symbol-context/spec|enclosing-symbol-context]] — containment logic and typed
  outcome pattern the blast-radius group reuses (#565)
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp-server-lifecycle-and-respawn]] — respawn
  behavior an in-flight overlay must survive
- `crates/mcpls-core/src/bridge/translator/edits.rs`, `crates/mcpls-core/src/bridge/state.rs`,
  `crates/mcpls-core/src/bridge/notifications.rs` — current edit-producing handlers, document
  tracker, and diagnostics cache
