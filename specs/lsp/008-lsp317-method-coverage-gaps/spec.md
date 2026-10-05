---
aliases:
  - LSP 3.17 method coverage gaps
  - Declaration, type hierarchy, prepareRename, documentHighlight, rangeFormatting
tags:
  - sdd
  - spec
  - research
  - competitive-parity
  - lsp
  - mcp
created: 2026-10-04
status: draft
related:
  - "[[constitution]]"
  - "[[lsp/002-lsp317-missing-tools/spec|lsp317-missing-tools]]"
  - "[[lsp/004-lsp-318-draft-gaps/spec|lsp-318-draft-gaps]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]]"
  - "[[mcp/005-tool-capability-discoverability/spec|tool-capability-discoverability]]"
  - "[[mcp/007-symbol-name-addressing/spec|symbol-name-addressing]]"
  - "[[bridge/001-position-encoding-layer/spec|position-encoding-layer]]"
  - "[[bridge/006-lsp-indexing-readiness-gate/spec|lsp-indexing-readiness-gate]]"
  - "[[runtime/003-workspace-supplied-code-execution/spec|workspace-supplied-code-execution]]"
---

# Feature: Close the Remaining LSP 3.17 Request-Method Gaps in the MCP Tool Surface

> [!info] Metadata
> **Type**: enhancement / competitor-gap
> **Priority**: P3 (declaration), P4 (type hierarchy, prepare rename, document highlight, range formatting)
> **Implemented**: group A (declaration) with #567 as the `go_to_declaration` tool, routed through the `declaration` `handles` value, gated on `declarationProvider` and on indexing readiness like `get_definition`, taking a position only; link results are flattened like definition links. Groups B and C remain draft.
> **Related issues**: #567 (declaration, P3), #568 (type hierarchy, P4), #569 (minor methods: prepare rename, document highlight, range formatting, P4)

## Decision (#568, #569): Groups B and C implemented, declaration untouched

> [!important] Resolved
> Scope: Group B (FR-010 to FR-017) and Group C (FR-020 to FR-028) with the cross-cutting
> requirements for them. Group A (declaration, #567) is not part of this change and stays open.
> `semanticTokens` stays a documented non-goal (Out of Scope). The tool surface grows from 21 to 27.

- **Tools (open question 1).** Six tools, one per concern: `prepare_type_hierarchy`,
  `get_supertypes`, `get_subtypes`, `prepare_rename`, `get_document_highlights`, `format_range`.
  The supertypes and subtypes `item` input is the typed `HierarchyItem`, not untyped
  JSON (NFR-002). Call hierarchy shares the same `HierarchyItem` for output and for the `item` input
  of incoming and outgoing calls (#618); a malformed item fails at deserialization.
- **Routing (question 2).** One `ToolKind::TypeHierarchy` covers prepare, supertypes and subtypes
  (FR-014: the item is producer-bound). `prepare_rename` routes through `ToolKind::Rename`, so the
  verdict comes from the server that performs the rename. New kinds `TypeHierarchy`,
  `DocumentHighlights`, `FormatRange` bring `ToolKind::ALL` to 18. `Capability` gains
  `TypeHierarchy`, `PrepareRename`, `DocumentHighlights`, `FormatRange` (18 total, `CapabilitySet`
  widened to `u32`). `PrepareRename` is the one non-primary capability: it is gated per tool via
  `McpTool::capability()`, and the support report and the per-call gate read the same value.
- **Capability (FR-023, FR-028).** `PrepareRename` is supported only when `renameProvider` is a
  `RenameOptions` with `prepareProvider: true`. mcpls now advertises
  `textDocument.rename.prepareSupport` (with `prepareSupportDefaultBehavior: identifier`), without
  which clangd and typescript-language-server never advertise it.
- **Live servers (question 3).** Verified live on 2026-10-04: clangd advertises and answers all four
  groups' methods (type hierarchy, prepare rename, highlights, range formatting);
  typescript-language-server answers prepare rename, highlights and range formatting and does not
  advertise type hierarchy. rust-analyzer advertises prepare rename and highlights, and neither
  type hierarchy nor range formatting.
- **Indexing gate (question 4, FR-035).** `prepare_type_hierarchy` NotRequired (mirrors
  `prepare_call_hierarchy`), `get_supertypes` and `get_subtypes` Required, `prepare_rename`
  Required (a mid-index "not renameable" would mislead), `get_document_highlights` NotRequired,
  `format_range` NotRequired.
- **Rename flow (question 5).** `rename_symbol` stays independent of `prepare_rename`.
- **Range formatting (question 6).** A new `format_range` tool returning `FormatDocumentResult`.
  Like `format_document` it does not cap the edit list, and mcpls neither filters nor invents edits.
- **Default `handles` (question 8).** Warning only. The uncovered-tool warning now also names
  `type_hierarchy`, `document_highlights` and `format_range`; `prepare_rename` rides on `rename`.
- **Respawn (question 9).** A walk is always dispatched to the route of the item's file, never to
  another server (FR-014). A respawned server may reject or empty-answer a stale item; mcpls adds no
  special handling.
- **Prepare rename outcomes (FR-021, FR-022).** `status` is `renameable` (range, optional
  placeholder), `default_behavior` (no range invented) or `not_renameable` (optional
  `server_message`). A `null` answer, `defaultBehavior: false`, or a JSON-RPC `-32602` or `-32001`
  (`UnknownErrorCode`, what clangd answers for "no symbol here") error reads as `not_renameable`; a `-32602` that mcpls classifies as an out-of-range position
  (rust-analyzer's "Invalid offset" text) is checked first and stays a caller-fault error.
- **Highlights (FR-025).** `kind` is `text`, `read` or `write`; an omitted or custom kind maps to
  `text`.
- **Known deviations.** FR-027 rejects zero, oversized and reversed ranges, and a line beyond the end
  of the tracked document (`Error::PositionBeyondDocument`, invalid params, checked after the
  capability and indexing gates and skipped for an untracked document). A character past the end of
  its line is not rejected: LSP 3.17 has the server clamp it to the line length, so a
  `format_range` end of `(n, 999)` keeps meaning "through the end of line n". `get_code_actions` and
  `get_inlay_hints` still do not check ranges against the document. `-32001` is LSP's catch-all
  `UnknownErrorCode`, so a genuine server failure reported with it also reads as `not_renameable`
  (its `server_message` keeps the server's text, and mcpls logs it at WARN), and a server that splits
  lines on `\n` only can consider a line mcpls admits (mcpls follows the LSP 3.17 line model, where a
  lone `\r` also ends a line) out of range and answer `-32001`, which reads the same way.

> [!abstract]
> mcpls exposed 21 MCP tools when this was written (29 now, including `go_to_declaration` and the tools this spec added), covering most LSP 3.17 navigation and editing requests. Six
> request methods remain unexposed while competing bridges already ship them:
> `textDocument/declaration`, `textDocument/prepareTypeHierarchy` with
> `typeHierarchy/supertypes` and `typeHierarchy/subtypes`, `textDocument/prepareRename`,
> `textDocument/documentHighlight`, `textDocument/rangeFormatting`, and
> `textDocument/semanticTokens`. This spec states what an AI agent should be able to do with
> each, grouped by priority so each group can ship independently. It deliberately says nothing
> about implementation shape.

## 1. Overview

### Problem Statement

[[lsp/002-lsp317-missing-tools/spec|lsp/002]] closed the first LSP 3.17 gap set (signature help,
implementation, type definition, inlay hints) under #116 and recorded that type hierarchy was
descoped as low priority because no reference project exposed it. That premise has changed: a
competing bridge now ships it. A fresh scan on 2026-10-04 of `crates/mcpls-core/src` and the
current tool list in `crates/mcpls-core/src/mcp/tool_surface.json` confirms these LSP request
methods are absent from mcpls, which has `get_definition`, `go_to_implementation`,
`go_to_type_definition`, `rename_symbol`, `format_document`, and call hierarchy (prepare,
incoming, outgoing) but nothing for the methods below.

Prior art (verified 2026-10-04): two comparable code-intelligence bridges were compared.

- One broad bridge exposes tools for declaration, prepare rename, type hierarchy (prepare,
  supertypes, and subtypes behind a single tool), document highlights, range formatting,
  semantic tokens, and server command execution. It does not implement selection range, folding
  range, or code lens.
- A second bridge exposes declaration and implementation lookup over LSP, and documents that
  declaration lookup generally does not work for symbols in external dependencies. It offers
  type hierarchy only through a separate non-LSP backend, not over LSP.

Evidence strength differs per method, which is why the priorities differ:

| Method | Comparable bridges shipping it over LSP | Priority |
|--------|-----------------------------------------|----------|
| declaration | 2 comparable bridges | P3 |
| type hierarchy | 1 comparable bridge | P4 |
| prepareRename | 1 comparable bridge | P4 |
| documentHighlight | 1 comparable bridge | P4 |
| rangeFormatting | 1 comparable bridge | P4 |
| semanticTokens | 1 comparable bridge | P4, evaluated and deferred (see Out of Scope) |

**Why declaration matters most.** `get_definition` answers "where is this symbol defined", but
for languages that separate declaration from definition the two answers differ and an agent
needs both. The clearest case is C and C++: a header declares, a source file defines. Go
interface methods are the other: the declaration lives on the interface, the definition on
each implementing type. Without declaration the agent can reach the implementation but not the
contract it fulfils.

**Why the rest are P4.** Each is a refinement of something mcpls already does: type hierarchy
extends type navigation next to `go_to_implementation`; prepareRename lets an agent learn
whether a rename is possible before proposing one; documentHighlight is a file-local subset of
`get_references`; rangeFormatting is a narrower `format_document`. None unblocks a task an
agent cannot already complete another way. They are worth specifying because competitors ship
them and because the shared concerns (capability gating, discoverability, position handling)
are cheaper to settle once.

### Goal

An AI agent using mcpls can invoke each LSP 3.17 request method listed above on a configured
language server through a typed MCP tool, and learns whether it is supported for a given
language server before calling it, with no weaker position, capability, or readiness guarantees
than the existing tools.

### Out of Scope

> [!danger] Non-goals
> The following were evaluated against the same competitor evidence and are not part of this
> spec. Each can be revisited with its own finding.

- **`textDocument/semanticTokens` (full, range, delta) and `workspace/semanticTokens/refresh`.**
  Evaluated and deferred. The wire format is a delta-encoded integer array that is meaningless
  without the server's legend, so a usable tool must decode it into named token types and
  modifiers, and a whole-file result is large for an agent's context. Hover, document symbols,
  and inlay hints already answer the questions agents ask about a symbol, and only one
  comparable bridge ships it with no evidence of use. Revisit when a concrete agent task is shown to
  need token-level classification (see Open Questions).
- **`textDocument/selectionRange`, `textDocument/foldingRange`, `textDocument/codeLens`.** Not
  implemented by the comparable bridge that ships the methods above, so no parity pressure.
  Their value is editor interaction (expand selection, collapse, run buttons), not
  agent navigation.
- **`workspace/executeCommand` and the equivalent command-execution tool of a comparable bridge.** Executes
  server-defined commands that can run workspace-supplied code. Governed by
  [[runtime/003-workspace-supplied-code-execution/spec|runtime/003]]; not a coverage gap to
  close here.
- **Applying edits.** Range formatting and rename-related tools return proposed edits only,
  consistent with the existing `format_document` and `rename_symbol`. mcpls remains a bridge,
  not an editor.
- **Name-based addressing.** Whether the new tools accept symbol names instead of positions
  belongs to [[mcp/007-symbol-name-addressing/spec|mcp/007]]. These tools take positions, like
  the existing position-based tools.
- **LSP 3.18 additions.** Tracked in [[lsp/004-lsp-318-draft-gaps/spec|lsp/004]].
- **Technical design.** Deferred to a plan.

## 2. User Stories

### US-001: Agent finds the declaration of a symbol (P3)

AS AN AI coding agent working in a C/C++ or Go codebase
I WANT to jump from a usage or definition to the symbol's declaration
SO THAT I can read the contract (header prototype, interface method) separately from the
implementation, without grepping for it.

**Acceptance criteria:**
```
GIVEN a C++ source file whose function is declared in a header and defined in the .cpp file
  AND the routed server advertises declaration support
WHEN the agent asks for the declaration at a call site of that function
THEN the result is the header location, distinct from the location get_definition returns
```

### US-002: Agent navigates a type hierarchy (P4)

AS AN AI coding agent exploring an object-oriented codebase
I WANT to start from a type and list its supertypes and its subtypes, and keep walking
SO THAT I can understand inheritance structure without reading every file.

**Acceptance criteria:**
```
GIVEN a class with at least one base class and one derived class
  AND the routed server advertises type hierarchy support
WHEN the agent prepares a type hierarchy at the class, then requests its supertypes and subtypes
THEN the supertypes result contains the base class and the subtypes result contains the derived class
  AND each returned item can be passed back to continue the walk in either direction
```

### US-003: Agent checks that a rename is possible before proposing it (P4)

AS AN AI coding agent
I WANT to ask whether the symbol at a position can be renamed, and what range and current name
the server would replace
SO THAT I can choose a valid position and avoid proposing a rename the server will reject.

**Acceptance criteria:**
```
GIVEN a position on a renameable identifier and a server that advertises rename preparation
WHEN the agent asks whether it can be renamed
THEN the result carries the range of the identifier (and the server's placeholder text when given)

GIVEN a position on a keyword or literal
WHEN the agent asks whether it can be renamed
THEN the result states that the position is not renameable, which is distinct from a failure
  and distinct from "server does not support the capability"
```

### US-004: Agent finds all occurrences of a symbol within one file (P4)

AS AN AI coding agent reading one file
I WANT every occurrence of the symbol at a position within that file, marked read, write, or
text
SO THAT I can trace local data flow without the cost of a workspace-wide references query.

**Acceptance criteria:**
```
GIVEN a local variable that is assigned once and read twice in a file
WHEN the agent asks for highlights at the variable
THEN three ranges are returned, one marked write and two marked read
```

### US-005: Agent formats only a selected range (P4)

AS AN AI coding agent that edited part of a file
I WANT formatting edits for only the lines I changed
SO THAT I do not produce a diff full of unrelated formatting changes.

**Acceptance criteria:**
```
GIVEN a file with unformatted code both inside and outside a chosen line range
  AND a server that advertises range formatting
WHEN the agent requests formatting for that range
THEN every returned edit lies within or adjacent to the range, per the server's response, and
  no edit is invented or filtered by mcpls
```

### US-006: Agent learns per language which of the new tools work (P3)

AS AN AI coding agent in a multi-server session
I WANT the new tools to appear in the per-server support report like every existing tool
SO THAT I never need a failing call to learn that the server for my language lacks, for
example, type hierarchy.

**Acceptance criteria:**
```
GIVEN a session with one server that supports type hierarchy and one that does not
WHEN the agent queries tool support
THEN type hierarchy is reported supported for the first server's language and not supported for
  the second, using the same three-way distinction as the existing tools
```

## 3. Functional Requirements

EARS notation. Requirements are grouped by priority so each group can ship alone. Within a
group, "the tool" means the MCP tool or tools that expose the method; whether one tool or
several is decided in the plan (see Open Questions).

### Group A: Declaration (P3, #567)

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN an agent requests the declaration of the symbol at a 1-based position in a file THE SYSTEM SHALL return the declaration location or locations the routed server reports for `textDocument/declaration` | must |
| FR-002 | THE SYSTEM SHALL return declaration results in the same shape as the existing definition-style navigation results (location, and link results where the server supplies them), so an agent handles both with one parser | must |
| FR-003 | WHEN the server returns no declaration THE SYSTEM SHALL return an empty result, not an error | must |
| FR-004 | WHEN the routed server does not advertise `declarationProvider` THE SYSTEM SHALL return the same typed capability-not-supported error the existing tools return, naming the missing capability | must |

### Group B: Type hierarchy (P4, #568)

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-010 | WHEN an agent requests a type hierarchy at a 1-based position THE SYSTEM SHALL return the type hierarchy items the routed server reports for `textDocument/prepareTypeHierarchy` | must |
| FR-011 | WHEN an agent supplies a type hierarchy item obtained from a previous prepare result THE SYSTEM SHALL return its supertypes (`typeHierarchy/supertypes`) | must |
| FR-012 | WHEN an agent supplies a type hierarchy item obtained from a previous prepare result THE SYSTEM SHALL return its subtypes (`typeHierarchy/subtypes`) | must |
| FR-013 | THE SYSTEM SHALL accept, as the item input of FR-011 and FR-012, exactly the item shape it returns from FR-010 and from FR-011/FR-012 results, so a walk can continue in either direction without client-side reshaping | must |
| FR-014 | THE SYSTEM SHALL route a type hierarchy walk to the server that produced the item, never to a different server for the same language, because an item is only meaningful to its producer | must |
| FR-015 | THE SYSTEM SHALL bound the number of items returned from supertypes and subtypes and report when more exist than are returned, as incoming and outgoing calls already do | must |
| FR-016 | WHEN the routed server does not advertise `typeHierarchyProvider` THE SYSTEM SHALL return the typed capability-not-supported error | must |
| FR-017 | WHEN an item is malformed or its document is outside the workspace THE SYSTEM SHALL reject it with an invalid-parameters error and issue no LSP request | must |

### Group C: Minor methods (P4, #569)

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-020 | WHEN an agent asks whether the symbol at a 1-based position can be renamed THE SYSTEM SHALL return the range of the renameable identifier and the placeholder text when the server supplies one (`textDocument/prepareRename`) | should |
| FR-021 | WHEN the server answers that the position is not renameable THE SYSTEM SHALL return a result distinguishable from a transport failure and from a missing capability | should |
| FR-022 | WHEN the server answers prepareRename with the "use default behavior" form (no range) THE SYSTEM SHALL return a result that states this explicitly rather than inventing a range | should |
| FR-023 | WHEN the routed server advertises `renameProvider` without preparation support THE SYSTEM SHALL report prepare rename as not supported while `rename_symbol` stays supported | should |
| FR-024 | WHEN an agent requests highlights at a 1-based position THE SYSTEM SHALL return every range the server reports for `textDocument/documentHighlight`, each with its kind (text, read, or write) | should |
| FR-025 | WHEN the server omits a highlight kind THE SYSTEM SHALL surface it as the LSP default (text), not as an absent or unknown value | should |
| FR-026 | WHEN an agent requests formatting for a 1-based line and column range, plus tab size (1 to 32, `TabSize`; any other value is rejected as invalid params, #606) and insert-spaces options THE SYSTEM SHALL return the text edits the server reports for `textDocument/rangeFormatting`, under the same options semantics as `format_document` | should |
| FR-027 | WHEN the start of a formatting range is after its end, or either lies outside the document THE SYSTEM SHALL reject the request with an invalid-parameters error and issue no LSP request; a line beyond the end of the document is rejected, a character past the end of its line is forwarded (the server clamps it) | should |
| FR-028 | WHEN the routed server does not advertise the capability a Group C tool needs (`documentHighlightProvider`, `documentRangeFormattingProvider`, or rename preparation) THE SYSTEM SHALL return the typed capability-not-supported error | should |
| FR-029 | WHEN a not-renameable answer is returned as a normal result THE SYSTEM SHALL log the underlying server error response below ERROR (an expected outcome), while a position rejected as invalid stays an ERROR-logged tool error | should |

### Cross-cutting (all groups)

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-030 | THE SYSTEM SHALL make each new tool routable per language through the existing per-tool routing configuration, so a server that handles a subset of tools can be assigned the new tools explicitly | must |
| FR-031 | WHEN an existing configuration lists servers with explicit tool lists THE SYSTEM SHALL treat the new tools like any other unclaimed tool (the existing uncovered-tool warning applies) and SHALL NOT silently route them to a server that did not claim them | must |
| FR-032 | THE SYSTEM SHALL include each new tool in the per-server tool-support report ([[mcp/005-tool-capability-discoverability/spec|mcp/005]]) with the same three-way distinction (not supported anywhere, supported for some, supported for all) and with the same decision logic the per-call gate uses | must |
| FR-033 | WHEN a position-taking new tool is called on a server that negotiated a non-UTF-16 position encoding THE SYSTEM SHALL convert positions through the shared position layer ([[bridge/001-position-encoding-layer/spec|bridge/001]]) and SHALL report position degradation in the result as the existing tools do, with the same `request` and `response` meanings | must |
| FR-034 | THE SYSTEM SHALL return all new tool positions 1-based, including ranges inside returned items | must |
| FR-035 | WHEN a new tool's answer depends on whole-workspace analysis and the server is still indexing THE SYSTEM SHALL apply the readiness behavior of [[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]]; the gate decision per tool (required or not required) SHALL be stated explicitly, not left to a default | must |
| FR-036 | THE SYSTEM SHALL describe each new tool in its MCP metadata, including output shape and the degradation and truncation fields, in the same manner as existing tools | must |
| FR-037 | THE SYSTEM SHALL ship the groups independently: Group A without B or C, and so on | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety (constitution) | Every new tool input and output SHALL be expressed as concrete types: highlight kind, rename-preparation outcome (renameable, not renameable, default behavior), and type hierarchy item SHALL be closed sets of variants or structs, not strings, untyped JSON, or bare integers. Illegal states (for example a "renameable" outcome with no range) SHALL be unrepresentable. Conversion to primitives happens only at the MCP boundary |
| NFR-002 | Type safety of item input | The item a client passes back for the type hierarchy walk SHALL be a typed input. The existing call hierarchy item input is declared with no type in its published input schema (verified in `tool_surface.json`, 2026-10-04); this spec does not require fixing that, but new tools SHALL NOT copy it |
| NFR-003 | Capability gating | Each new capability SHALL be checked through the same typed gating path as the existing tools, so the per-call check and the support report cannot disagree. Adding a new tool kind SHALL fail to compile, or fail an existing exhaustiveness test, until its route and its capability mapping are both defined |
| NFR-004 | Fail-open parity | Servers whose capabilities are not yet known SHALL be treated as the existing tools treat them (assumed supported), so the new tools introduce no new failure mode in the startup window |
| NFR-005 | Graceful degradation | A missing capability, or an LSP error from one server, SHALL NOT affect other servers or other tools, as for every existing tool |
| NFR-006 | Output bounds | Result size SHALL be bounded for every tool that returns lists (type hierarchy, highlights, range formatting edits), with an explicit truncation indicator, so one request cannot flood an agent's context |
| NFR-007 | No added round trips | A new tool SHALL cost one LSP request (two where the LSP protocol requires prepare then query), and SHALL NOT add background requests to existing flows |
| NFR-008 | Testing | Each new tool SHALL have unit tests for parameter conversion and response conversion, including a non-UTF-16 encoding case, and at least one live test against a real server that supports the method |
| NFR-009 | Security | New tools SHALL validate file paths against the workspace boundary exactly as existing tools do, and SHALL NOT execute server commands |
| NFR-010 | Documentation | Doc comments, doc-tests, and the tool descriptions SHALL meet the constitution's documentation rules; the user-facing tool list and the testing playbooks SHALL be updated for each shipped group |

## 5. Data Model

No persistent state. New or extended concepts:

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Declaration result | Same family as the definition-style location results | Locations or location links, 1-based ranges, position degradation indicator |
| Type hierarchy item | Opaque-to-the-agent handle for one type, returned by prepare and by walks, accepted back as input | Name, symbol kind, detail, URI, full range, selection range (all 1-based), producer-owned opaque data |
| Type hierarchy walk result | Supertypes or subtypes of one item | List of items, truncation indicator, position degradation indicator |
| Rename preparation outcome | One of: renameable (range and optional placeholder), renameable with server default range, not renameable | Range, placeholder |
| Document highlight | One occurrence of a symbol in one file | 1-based range, kind (text, read, write) |
| Range formatting request | Formatting scoped to a range | File, 1-based range, tab size, insert-spaces flag |
| Per-server tool support entry | Existing entity from [[mcp/005-tool-capability-discoverability/spec|mcp/005]], extended with one entry per new tool | Tool, per-server supported flag, three-way aggregate |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Declaration requested where a server returns the same location as definition (languages without a declaration concept) | Return it; mcpls does not deduplicate against definition, so the agent sees what the server said |
| Declaration of a symbol from an external dependency | Return whatever the server returns; an empty result is valid and not an error. A comparable bridge documents the same limitation (FR-003) |
| Type hierarchy prepare at a position that is not a type | Empty result, not an error |
| Type hierarchy item from server A passed after the document was closed or the server respawned | The walk fails with a clear error, or yields an empty result, per the server; it is never routed to another server (FR-014). [NEEDS CLARIFICATION: behavior across a respawn, see lsp/001] |
| Cyclic or very deep hierarchy (interfaces, diamond inheritance) | Each call returns one level; the agent decides how deep to walk. Bounded by FR-015 |
| Server advertises `renameProvider` as plain `true` | Prepare rename reported unsupported (FR-023); `rename_symbol` unaffected |
| prepareRename returns `null` | "Not renameable" outcome (FR-021), not an error |
| Highlight request on whitespace or comment | Empty result |
| Range formatting range spans zero characters | Valid; server decides, edits may be empty |
| Range formatting range in a file larger than the tracked-document limit | Same behavior as `format_document` today |
| Non-UTF-16 server, queried column lands mid-character | Request degradation is reported (FR-033); the result is flagged untrustworthy per the existing meaning |
| Server still indexing | Behavior per FR-035 |
| Existing `handles` lists that never mention the new tools | New tools are unclaimed for that language; warning at startup; no silent routing (FR-031) |
| Tool called while the server has not finished `initialize` | Same as existing tools (NFR-004) |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Every shipped method has a typed MCP tool and appears in the per-server support report | 100% of shipped methods |
| SC-002 | Each shipped method is exercised against at least one real server that advertises it, in a live test | 1 server per method; the specific servers are chosen in the plan |
| SC-003 | Support-report decisions equal per-call gating decisions for every new tool, across every server capability combination in the parity test | 0 mismatches |
| SC-004 | Non-UTF-16 conversion tests pass for each position-taking new tool, covering both degradation meanings | All pass |
| SC-005 | Adding a routable tool without a capability mapping does not compile or fails an existing guard test | Verified once per group |
| SC-006 | No existing tool changes behavior as a result of shipping a group | 0 regressions in the existing suite |
| SC-007 | The competitor-gap rows for each shipped method are marked closed in the next competitor comparison cycle | All shipped rows closed |

## 8. Agent Boundaries

### Always (without asking)
- Reuse existing result, position, degradation, and truncation shapes rather than defining parallel ones.
- Keep the per-call capability gate and the support report on one shared decision path.
- State each tool's indexing-gate choice explicitly.
- Update the testing playbooks under `.local/testing/` for each shipped group, not the continuous-improvement rules file.

### Ask First
- Merging several methods into one MCP tool, or splitting one method across tools (see Open Questions).
- Any change to the existing call hierarchy tool shapes, including typing its item input.
- Raising semanticTokens, selectionRange, foldingRange, or codeLens out of the Non-Goals.
- Changing default routing so that unclaimed tools fall back to a catch-all in a new way.
- Adding a dependency.

### Never
- Weaken or bypass `Translator::require_capability`.
- Implement position conversion outside the shared position layer.
- Execute server commands or apply edits.
- Represent highlight kind, rename outcome, or hierarchy items as strings or untyped JSON.
- Spawn, route to, or fall back to a server other than the one that produced a hierarchy item.

## 9. Open Questions

> [!question] Items for the plan, or for the caller
> - [NEEDS CLARIFICATION: **One tool per concern or merged?** One comparable bridge merges call hierarchy and type hierarchy into one tool taking a direction. mcpls uses a prepare tool plus separate incoming and outgoing tools, with an item as the handle. Keeping the mcpls pattern for type hierarchy (prepare, supertypes, subtypes) is consistent and keeps each tool's schema simple but adds three tools to a 21-tool surface; a merged tool saves surface but needs a tagged input variant. Which does the maintainer prefer? This spec requires only the behaviors in FR-010 to FR-013.]
> - [NEEDS CLARIFICATION: **Routing kind granularity.** Does type hierarchy share one routing kind across prepare, supertypes, and subtypes (as call hierarchy does, because the item is producer-bound), or are they separate? FR-014 requires the producer-bound outcome either way. Likewise whether prepare rename routes with rename or separately.]
> - [NEEDS CLARIFICATION: **Which servers in the default language mapping advertise each method?** Needed to choose live-test servers (SC-002) and to size real-world value. Not verified in this spec; clangd is a candidate for declaration and type hierarchy, but this was not checked.]
> - [NEEDS CLARIFICATION: **Indexing gate per tool** (FR-035). Declaration and type hierarchy prepare resemble definition (position-based name resolution); prepare rename and document highlight may be single-file. Decide per tool using the same reasoning as bridge/006.]
> - [NEEDS CLARIFICATION: **Rename flow.** Should `rename_symbol` itself consult prepare rename first when supported, or stay independent? This spec treats them as independent tools. A coupled flow would change an existing tool and falls under Ask First.]
> - [NEEDS CLARIFICATION: **Range formatting vs `format_document`.** Is range a new tool or an optional range on the existing tool? Either satisfies FR-026; the tradeoff is surface size against schema clarity.]
> - [NEEDS CLARIFICATION: **semanticTokens scope in #569.** This spec defers it (Out of Scope). Confirm that #569 should list it as deferred with the revisit trigger, rather than as an open requirement.]
> - [NEEDS CLARIFICATION: **Default `handles` behavior for existing configs** (FR-031). Is warning-only acceptable for upgrade, or should the 30-language default mapping be extended to claim the new tools where the default server supports them?]
> - [NEEDS CLARIFICATION: **Type hierarchy walk across a server respawn** (Edge Cases).]

## 10. See Also

- [[constitution]] — project principles, type safety and testing rules
- [[MOC-specs]] — all specifications
- [[lsp/002-lsp317-missing-tools/spec|lsp/002]] — first LSP 3.17 gap set; records the type hierarchy descope whose premise has changed
- [[lsp/004-lsp-318-draft-gaps/spec|lsp/004]] — LSP 3.18 tracking
- [[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]] — tool surface and per-tool routing
- [[mcp/005-tool-capability-discoverability/spec|mcp/005]] — per-server support report that each new tool joins
- [[mcp/007-symbol-name-addressing/spec|mcp/007]] — name-based addressing, orthogonal to these tools
- [[bridge/001-position-encoding-layer/spec|bridge/001]] — 1-based layer and degradation signalling
- [[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]] — readiness gate
- [[runtime/003-workspace-supplied-code-execution/spec|runtime/003]] — why `executeCommand` is excluded
- Code references: `crates/mcpls-core/src/config/routing.rs` (`ToolKind::ALL`, asserted at 15 entries), `crates/mcpls-core/src/bridge/translator/routing.rs` (`Capability`, `IndexingGate`), `crates/mcpls-core/src/mcp/tool_surface.json`
