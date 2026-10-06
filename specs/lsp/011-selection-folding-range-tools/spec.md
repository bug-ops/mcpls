---
aliases:
  - Selection range and folding range tools
  - textDocument/selectionRange and textDocument/foldingRange coverage
tags:
  - sdd
  - spec
  - research
  - competitive-parity
  - lsp
  - mcp
created: 2026-10-05
status: implemented
related:
  - "[[constitution]]"
  - "[[lsp/008-lsp317-method-coverage-gaps/spec|lsp317-method-coverage-gaps]]"
  - "[[lsp/002-lsp317-missing-tools/spec|lsp317-missing-tools]]"
  - "[[mcp/001-mcp-tool-surface-and-routing/spec|mcp-tool-surface-and-routing]]"
  - "[[mcp/005-tool-capability-discoverability/spec|tool-capability-discoverability]]"
  - "[[bridge/001-position-encoding-layer/spec|position-encoding-layer]]"
  - "[[bridge/006-lsp-indexing-readiness-gate/spec|lsp-indexing-readiness-gate]]"
  - "[[bridge/007-enclosing-symbol-context/spec|enclosing-symbol-context]]"
  - "[[runtime/004-server-text-hygiene/spec|server-text-hygiene]]"
---

# Feature: Expose Selection Range and Folding Range as MCP Tools

> [!important] Decision (implemented, #616)
> - **Tools:** `get_selection_ranges` (`file_path`, `line`, `character`) and `get_folding_ranges` (`file_path`, `kind`: `all` | `comment` | `imports` | `region`), routed by the new `handles` values `selection_range` and `folding_range`; both listed in `get_tool_support`. Tool count 29 -> 31.
> - **Bounds:** a selection chain is cut to its 32 innermost ranges (`MAX_SELECTION_CHAIN`); folding regions use the shared 10,000-item `ItemBudget`, after the kind filter, ordered by start line then longest first. `truncated` reports either cut.
> - **Collapsed text:** returned, escaped, redacted first and then cut to 256 bytes (`MAX_COLLAPSED_TEXT_BYTES`) so a half-cut secret cannot leak.
> - **Range values:** returned in the existing 1-based shape, so their values can be passed as the range of `get_code_actions` and `format_range` (the round trip is a playbook check, SC-003).
> - **No batch positions, no source-text context line;** `unspecified` regions match only `all`; both tools are `IndexingGate::NotRequired`.
> - **Client capabilities:** `selectionRange` and `foldingRange` (columns on, `lineFoldingOnly: false`, the three standard kinds, `collapsedText: true`) are advertised at `initialize`.
> - **Deep chains:** a response nested past the JSON recursion limit (about 125 levels) fails only its own request with `undecodable response`; the connection stays up (#642).
> - **Undecodable server messages (#643):** a server request that cannot be decoded is answered with a `-32600` error and a notification or unrecognized frame is dropped, with at most one WARN a minute (carrying the count dropped since the last one; #681), and a frame whose bytes are not UTF-8 fails only its own request the same way, never decoded lossily; the connection stays up until `MAX_CONSECUTIVE_UNDECODABLE_FRAMES` (64) undecodable frames arrive in a row, a bound that stops a server emitting only garbage from being kept forever (it is torn down and respawned like any lost connection).
> - **Live matrix (SC-002 to SC-004)** is recorded in the testing playbook, not here.

> [!info] Metadata
> **Type**: enhancement / competitor-gap
> **Priority**: P4 (one comparable bridge ships both)
> **Related issues**: #616
> **Observed at**: 2026-10-05, 29 tools in `crates/mcpls-core/src/mcp/tool_surface.json`

> [!abstract]
> mcpls implements neither `textDocument/selectionRange` nor `textDocument/foldingRange`: a search of
> `crates/mcpls-core/src` finds no request for either, and the client capabilities mcpls advertises
> in `lsp/lifecycle.rs` do not mention them. [[lsp/008-lsp317-method-coverage-gaps/spec|lsp/008]]
> deferred both because the one comparable bridge examined then did not ship them. A second,
> actively maintained bridge examined on 2026-09-29 ships both. This spec states what an agent
> should be able to do with each, with the typing, gating, bounding and position guarantees of the
> existing tools. It says nothing about implementation shape.

## 1. Overview

### Problem Statement

Today an agent that needs to pick a text range must guess it. Three existing tools take a range
as input and none of them can say which ranges are meaningful:

- `get_code_actions` takes a start and end position; refactorings such as "extract function" only
  appear for a range that covers a whole expression or statement.
- `format_range` (added with the range-formatting work in lsp/008) formats exactly the range given;
  a range that cuts through an expression gives a useless or empty result.
- `get_inlay_hints` takes a range and is cheaper the tighter it is.

The only inputs an agent has to choose such a range are `get_document_symbols` (item granularity:
functions, classes, fields, never an expression or a statement) and `get_hover` (type text, not
extents). Statement and expression extents must be guessed from source text.

`textDocument/selectionRange` answers exactly this: for a position, the chain of ranges that the
server considers syntactically meaningful, innermost to outermost (identifier, expression,
statement, block, item, file).

`textDocument/foldingRange` returns the foldable regions of a file: blocks, functions, import
groups, comment blocks, `#region` markers. It overlaps with `get_document_symbols` for structure
(a function body is both a symbol range and a fold) but adds what symbols lack: import groups,
multi-line comment blocks, region markers and nested blocks that are not symbols, in a payload
that carries no names and is therefore cheap for a first look at a large file.

Prior art (verified 2026-09-29, one comparable bridge): it ships a selection-range tool
(position in, chain of ranges out, each with the first source line of the range as context) and
a folding-range tool (file in, regions out with start and end line, optional columns, kind and
collapsed text, with a kind filter of all, comment, imports or region). It states the same
purpose for selection range as above: choosing the range for code actions and range formatting.
Only one comparable bridge ships either method, hence P4.

**Re-examining the lsp/008 non-goal.** lsp/008 recorded that the value of selection range and
folding range "is editor interaction (expand selection, collapse, run buttons), not agent
navigation". That holds for folding and for code lens. It does not hold for selection range once
`format_range` and `get_code_actions` exist: the chain is the missing input for choosing their
range, which makes it an agent-facing capability and not an editor affordance. Folding range stays
a low-urgency convenience: its value over `get_document_symbols` is the import, comment and region
coverage plus payload size, and it is the weaker of the two. Both are specified so they can ship
together or separately (FR-040).

### Goal

An agent using mcpls can ask a configured language server for the chain of semantic ranges
enclosing a position and for the foldable regions of a file, through typed MCP tools, and can
tell beforehand whether the server for its language supports each, with no weaker position,
capability, readiness, boundary or size guarantees than the existing tools.

### Out of Scope

> [!danger] Non-goals
> Each can be revisited with its own finding.

- **`textDocument/codeLens`** (and `codeLens/resolve`). Remains deferred: lenses are editor
  buttons whose action is a server command, and executing commands is governed by
  [[runtime/003-workspace-supplied-code-execution/spec|runtime/003]].
- **`textDocument/semanticTokens`** (full, range, delta). Remains deferred for the reasons in
  lsp/008 (legend-dependent integer encoding, large payload, no concrete agent task).
- **Batch selection range.** The LSP request takes a list of positions. These tools take one
  position per call, like the other position-based tools (see Open Questions).
- **Applying or merging ranges.** The tools report ranges only. Feeding a range to
  `get_code_actions` or `format_range` is the agent's decision; mcpls does not couple the tools.
- **Computing ranges mcpls does not get from a server.** No fallback to a parser, a heuristic or
  document symbols when a server lacks the capability (the capability error is the answer).
- **Client-side folding presentation:** collapsing, `collapsedText` rendering policy, and
  fold-state tracking.
- **Name-based addressing** ([[mcp/007-symbol-name-addressing/spec|mcp/007]]): the selection tool
  takes a position, like the other position-based tools.
- **Technical design.** Deferred to a plan.

## 2. User Stories

### US-001: Agent picks a syntactically valid range for an edit-shaped tool (P4)

AS AN AI coding agent that wants code actions or formatting for one expression or statement
I WANT the chain of ranges enclosing a position, innermost first
SO THAT I can pass a range that covers a whole expression or statement to `get_code_actions`
or `format_range` instead of guessing its extents.

**Acceptance criteria:**
```
GIVEN a file with a call expression `f(a + b)` inside a statement inside a block
  AND the routed server advertises selection range support
WHEN the agent asks for the selection range at the position of `a`
THEN the result lists ranges from innermost to outermost, each containing the previous one,
  the first at or around the token and the last the outermost range the server reports
  AND every range is 1-based and can be passed unchanged as the range input of get_code_actions
  or format_range, designating the same text span
```

### US-002: Agent gets a cheap structural overview of a large file (P4)

AS AN AI coding agent opening a large file
I WANT the foldable regions of the file without names or bodies
SO THAT I can decide which section to read, including import groups and comment blocks that
`get_document_symbols` does not list.

**Acceptance criteria:**
```
GIVEN a file with an import group, two functions and a multi-line comment block
  AND the routed server advertises folding range support
WHEN the agent asks for folding ranges
THEN the result contains one region per fold with 1-based start and end line, and a typed kind
  that marks the import group as imports and the comment block as comment
```

### US-003: Agent narrows folding ranges by kind (P4)

AS AN AI coding agent that only wants imports or comment blocks
I WANT to filter the folding result by kind
SO THAT I do not receive hundreds of block regions I will discard.

**Acceptance criteria:**
```
GIVEN a file whose folding result mixes imports, comment and unkinded block regions
WHEN the agent asks for folding ranges with kind filter imports
THEN only imports regions are returned
  AND `truncated` reflects the filtered list, not the unfiltered one
```

### US-004: Agent learns per language whether the tools work (P4)

AS AN AI coding agent in a multi-server session
I WANT both tools to appear in the per-server support report
SO THAT I do not need a failing call to learn that the server for my language lacks them.

**Acceptance criteria:**
```
GIVEN a session where one server advertises selection range and one does not
WHEN the agent queries tool support
THEN the selection range tool is supported for the first server's language and reported as
  capability not advertised for the second, using the existing three-way distinction
```

## 3. Functional Requirements

EARS notation. Tool names are decided in the plan (see Open Questions); "the selection tool" and
"the folding tool" below name the two MCP tools.

### Selection range

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN an agent requests the selection range at a 1-based position in a file THE SYSTEM SHALL return the chain of ranges the routed server reports for `textDocument/selectionRange`, ordered innermost to outermost | must |
| FR-002 | THE SYSTEM SHALL send exactly one position per LSP request and return exactly the chain for that position | must |
| FR-003 | THE SYSTEM SHALL return each range in the same 1-based range shape the existing tools use, so a returned range is usable unchanged as the range input of `get_code_actions`, `format_range` and `get_inlay_hints` and designates the same text span there | must |
| FR-004 | THE SYSTEM SHALL return the chain as the server reported it: no reordering, no merging of equal consecutive ranges, no invented outermost "whole file" range | must |
| FR-005 | WHEN the server returns no selection range for the position (empty list or `null`) THE SYSTEM SHALL return an empty chain, not an error | must |
| FR-006 | THE SYSTEM SHALL bound the number of ranges in one returned chain and report when the server's chain was longer, keeping the innermost ranges | must |
| FR-007 | THE SYSTEM SHALL reject a position that is zero or beyond the shared position maximum with an invalid-parameters error and issue no LSP request | must |
| FR-008 | WHEN the routed server does not advertise `selectionRangeProvider` THE SYSTEM SHALL return the typed capability-not-supported error naming the missing capability | must |

### Folding range

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-010 | WHEN an agent requests the folding ranges of a file THE SYSTEM SHALL return the regions the routed server reports for `textDocument/foldingRange` | must |
| FR-011 | THE SYSTEM SHALL return each region with a 1-based start line and end line, optional 1-based start and end character when the server supplies them, a typed kind, and the collapsed text when the server supplies it | must |
| FR-012 | THE SYSTEM SHALL represent the kind as a closed set of variants: comment, imports, region, and one variant for a region with no kind or a server-defined kind (as highlights map an omitted or custom kind to the LSP default) | must |
| FR-013 | WHEN an agent supplies a kind filter THE SYSTEM SHALL return only regions of that kind, applying the filter before the result bound of FR-015; with no filter THE SYSTEM SHALL return all regions | must |
| FR-014 | THE SYSTEM SHALL return regions in a deterministic order: ascending start line, then descending end line, whatever order the server used | should |
| FR-015 | THE SYSTEM SHALL bound the number of regions returned and report when more exist than are returned, as existing list tools do, keeping the first regions in the FR-014 order | must |
| FR-016 | WHEN the server returns no regions THE SYSTEM SHALL return an empty result, not an error | must |
| FR-017 | THE SYSTEM SHALL treat the collapsed text as server-controlled text and pass it through the same escaping and redaction that other tool results apply ([[runtime/004-server-text-hygiene/spec|runtime/004]]) | must |
| FR-018 | WHEN a region's end line precedes its start line THE SYSTEM SHALL drop that region and log it, not pass it to the agent | should |
| FR-019 | WHEN the routed server does not advertise `foldingRangeProvider` THE SYSTEM SHALL return the typed capability-not-supported error naming the missing capability | must |

### Cross-cutting

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-030 | THE SYSTEM SHALL make each tool routable per language through the existing per-tool routing configuration, as its own routable kind (selection range and folding range separately, since servers claim them independently) | must |
| FR-031 | WHEN an existing configuration lists servers with explicit tool lists THE SYSTEM SHALL treat the new tools like any other unclaimed tool (the existing uncovered-tool warning applies) and SHALL NOT silently route them to a server that did not claim them | must |
| FR-032 | THE SYSTEM SHALL include each new tool in the per-server tool-support report ([[mcp/005-tool-capability-discoverability/spec|mcp/005]]) with the existing three-way distinction and with the same decision logic as the per-call gate | must |
| FR-033 | THE SYSTEM SHALL gate each tool on its advertised provider whichever of the three LSP forms the server uses (boolean, options object, registration options), and SHALL treat a provider of `false` or an absent provider as not advertised | must |
| FR-034 | WHEN a position or character is converted for a server that negotiated a non-UTF-16 position encoding THE SYSTEM SHALL convert through the shared position layer ([[bridge/001-position-encoding-layer/spec|bridge/001]]) and report `positions_degraded` with the existing meanings: `request` for the selection tool's input position, `response` for returned characters of either tool | must |
| FR-035 | THE SYSTEM SHALL state the indexing-gate decision per tool explicitly ([[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]]): both tools are NotRequired, because the servers examined derive both answers from the file's syntax tree and not from workspace analysis; the live test (SC-002) SHALL confirm this per server | must |
| FR-036 | THE SYSTEM SHALL return all positions in results 1-based, including optional characters | must |
| FR-037 | THE SYSTEM SHALL describe each tool in its MCP metadata, including the output shape, ordering, truncation and degradation fields, in the manner of existing tools | must |
| FR-038 | THE SYSTEM SHALL reject a file outside the workspace with the existing boundary error and issue no LSP request | must |
| FR-040 | THE SYSTEM SHALL allow the two tools to ship independently | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety (constitution) | Folding kind, kind filter, the selection chain and each region SHALL be concrete types: a closed enum for kind and filter, not strings; a region SHALL be one struct whose optional characters are either both present or both absent where representable. The LSP's open-string `FoldingRangeKind` is converted to the closed set at the boundary only |
| NFR-002 | Capability gating | Each new capability SHALL be checked through the shared typed gating path; adding a routable kind without its route and its capability mapping SHALL fail to compile or fail an existing exhaustiveness test |
| NFR-003 | Fail-open parity | Servers whose capabilities are not yet known are treated as the existing tools treat them (assumed supported) |
| NFR-004 | Graceful degradation | A missing capability, or an LSP error from one server, SHALL NOT affect other servers or tools |
| NFR-005 | Output bounds | Both results SHALL be bounded with an explicit truncation indicator, using the shared bounded-result mechanism of the list tools. A server that returns a chain or region list of any size, including a deeply nested chain, SHALL cost at most the bounded amount of conversion work |
| NFR-006 | No added round trips | One LSP request per call, no background requests |
| NFR-007 | Client capabilities | The `initialize` request SHALL advertise client capability for `textDocument.selectionRange` and `textDocument.foldingRange` if a server in the live matrix gates its provider on it (see Open Questions), with no change to the behavior of the existing tools |
| NFR-008 | Testing | Unit tests for parameter and response conversion, including a non-UTF-16 case for both tools, kind mapping, filter-before-bound, malformed region drop and chain bound; live tests per SC-002 |
| NFR-009 | Security | Path validation exactly as existing tools; no server commands executed |
| NFR-010 | Documentation | Doc comments and doc-tests per the constitution; the user-facing tool list, tool-count statements in the project docs, and the `.local/testing/` playbooks updated for each shipped tool |

## 5. Data Model

No persistent state.

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| Selection range chain | Ranges enclosing one position, innermost first | Ordered list of 1-based ranges, truncation indicator, position degradation indicator |
| Folding region | One foldable region of a file | 1-based start line and end line, optional 1-based start and end character, kind, optional collapsed text |
| Folding kind | Closed set | comment, imports, region, unspecified-or-custom |
| Folding kind filter | Closed set | all, comment, imports, region |
| Folding result | Regions of one file | Ordered list of regions, truncation indicator, position degradation indicator (`response` only) |
| Per-server tool support entry | Existing entity from mcp/005, extended with two entries | Tool, per-server supported flag, three-way aggregate |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Selection position on whitespace, comment or between tokens | Whatever the server returns; an empty chain is valid (FR-005) |
| Position beyond the end of the document | Server decides: empty chain or server error surfaced as it is for other tools. mcpls does not bound-check against document content, a known deviation shared with the existing position tools (#607) |
| Server chain begins with an empty range at the position | Returned as is (FR-004); the agent sees what the server said |
| Server chain contains equal consecutive ranges | Returned as is (FR-004) |
| Pathologically deep chain from the server | Truncated to the bound keeping the innermost ranges; `truncated` set (FR-006, NFR-005) |
| Selection range where a server returns a chain that does not contain the queried position | Returned as is; mcpls does not validate semantic correctness of server output |
| Folding file with thousands of regions | Ordered per FR-014, bounded per FR-015, `truncated` set |
| Kind filter matches nothing | Empty result, not an error (FR-016) |
| Server omits kind for a block region | Typed as the unspecified variant (FR-012); it matches no kind filter except `all` |
| Server returns a custom kind string | Typed as the unspecified variant; the raw string is not exposed |
| Region with end line before start line, or line out of the document | Malformed order dropped (FR-018); a line beyond the document is passed as is, like positions above |
| Region with a character on one end only | Passed as the server sent it; an absent character stays absent, never zero |
| Collapsed text containing control characters or a configured secret | Escaped and redacted (FR-017) |
| Non-UTF-16 server, queried column lands mid-character | `positions_degraded: request` on the selection tool, result flagged untrustworthy (FR-034) |
| Server not yet initialized | Same as existing tools (NFR-003) |
| Server advertises only `foldingRangeProvider` as an options object with no fields | Supported (FR-033) |
| File type no server handles | The existing no-server error |
| Existing `handles` lists that never mention the new tools | Unclaimed, warning at startup, no silent routing (FR-031) |
| Server restarted between calls | No state is carried between calls, so there is no stale-handle case |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Each shipped method has a typed MCP tool and appears in the per-server support report | 100% of shipped methods |
| SC-002 | Live verification, per server class below, recorded in the playbooks: (a) rust-analyzer; (b) typescript-language-server; (c) pyright; (d) gopls. For each: the advertised-or-not state of each provider is recorded; where advertised, the selection tool returns a chain whose first range lies inside the queried token or expression and whose last range spans the file or the outermost block, and the folding tool returns at least the function and import regions of a fixture file; where not advertised, the tool returns the capability-not-supported error and the support report agrees. Servers other than these four are optional | Every server of the matrix passes its applicable branch |
| SC-003 | Round trip: a range returned by the selection tool, passed as the range of `format_range` and of `get_code_actions`, designates the same text span, on one UTF-16 server and one non-UTF-16 fixture | Pass |
| SC-004 | Indexing: a selection and a folding call issued while the server is still indexing return the same result as after indexing, on at least one server that indexes (rust-analyzer) | Pass, or FR-035 revised |
| SC-005 | Support report decisions equal per-call gate decisions for every capability form (absent, false, true, options, registration options) | 0 mismatches |
| SC-006 | Non-UTF-16 conversion tests pass for both tools, covering both degradation meanings for selection range and `response` for folding | All pass |
| SC-007 | Bounds: a synthetic server answer with more regions and a deeper chain than the bounds returns exactly the bounded counts with `truncated` set, and with no panic or unbounded allocation | Pass |
| SC-008 | Adding a routable kind without a capability mapping does not compile or fails a guard test | Verified once |
| SC-009 | No existing tool changes behavior (including the new client capabilities of NFR-007) | 0 regressions in the existing suite |
| SC-010 | The competitor-gap row for these methods is marked closed in the next competitor comparison cycle | Closed |

## 8. Agent Boundaries

### Always (without asking)
- Reuse the existing range, position, degradation and truncation shapes and the shared bounded-result mechanism.
- Keep the per-call gate and the support report on one shared decision path.
- State the indexing-gate choice for each tool explicitly.
- Update the testing documents under `.local/testing/` for each shipped tool, not the continuous-improvement rules file.

### Ask First
- Merging the two methods into one tool, or adding a batch-of-positions input.
- Adding optional source text (the "context line") to selection results (see Open Questions).
- Changing the client capabilities advertised at `initialize` beyond NFR-007.
- Making any existing tool consult these tools (for example `get_code_actions` defaulting its range).
- Raising codeLens or semanticTokens out of Out of Scope.
- Adding a dependency.

### Never
- Weaken or bypass `Translator::require_capability`.
- Implement position conversion outside the shared position layer.
- Invent, merge, dedupe or reorder server-reported selection ranges.
- Represent kind or filter as strings or untyped JSON.
- Fall back to document symbols or a parser when a server lacks the capability.

## 9. Resolved Questions

All questions raised in the draft were settled when the tools shipped (#616); see the Decision callout at the top.

- **Tool names and count:** two tools, `get_selection_ranges` and `get_folding_ranges`.
- **Provider support:** rust-analyzer and typescript-language-server advertise both; pyright advertises neither (`capability_not_advertised`); gopls is recorded in the playbook when available.
- **Client capabilities:** `selectionRange` and `foldingRange` are advertised with columns on (`lineFoldingOnly: false`), the three standard kinds and `collapsedText`.
- **Bounds:** 32 selection ranges; folding uses the shared 10,000-item budget.
- **Source text in selection results:** none; no opt-in `context`.
- **Collapsed text:** returned, escaped, redacted, cut to 256 bytes.
- **Kind filter:** `all`, `comment`, `imports`, `region`; `unspecified` regions match only `all`.
- **Batch positions:** not offered.
- **Indexing gate:** `NotRequired` for both tools.
- **Overlap with `get_document_symbols`:** both tools ship (FR-040).
- **Default `handles`:** warning only; the new routing values are unclaimed under explicit `handles`.
- **lsp/008 text:** annotated.
- **Deliberate deviation from NFR-001:** a region's start and end characters are independently optional, as in LSP, not "both or neither"; an absent character stays absent.

## 10. See Also

- [[constitution]] — project principles, type safety and testing rules
- [[MOC-specs]] — all specifications
- [[lsp/008-lsp317-method-coverage-gaps/spec|lsp/008]] — deferred these methods; its `format_range` tool is the main consumer of selection ranges
- [[lsp/002-lsp317-missing-tools/spec|lsp/002]] — first LSP 3.17 gap set
- [[mcp/001-mcp-tool-surface-and-routing/spec|mcp/001]] — tool surface and per-tool routing
- [[mcp/005-tool-capability-discoverability/spec|mcp/005]] — support report both tools join
- [[bridge/001-position-encoding-layer/spec|bridge/001]] — 1-based layer and degradation signalling
- [[bridge/006-lsp-indexing-readiness-gate/spec|bridge/006]] — readiness gate
- [[bridge/007-enclosing-symbol-context/spec|bridge/007]] — opt-in enrichment pattern relevant to the source-text question
- [[runtime/004-server-text-hygiene/spec|runtime/004]] — server-text escaping and redaction applied to collapsed text
- Code references: `crates/mcpls-core/src/config/routing.rs` (`ToolKind`, `ToolKind::ALL`), `crates/mcpls-core/src/bridge/translator/routing.rs` (`Capability`, `IndexingGate`, `validate_position`), `crates/mcpls-core/src/bridge/translator/navigation.rs` (`ItemBudget`), `crates/mcpls-core/src/bridge/translator/highlights.rs` (closest existing handler shape), `crates/mcpls-core/src/lsp/lifecycle.rs` (client capabilities), `crates/mcpls-core/src/mcp/tool_surface.json`
