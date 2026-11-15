---
aliases:
  - Symbol source context
  - symbol_source location context
tags:
  - sdd
  - spec
  - enhancement
  - bridge
  - token-efficiency
  - competitor-gap
created: 2026-11-15
status: implemented
related:
  - "[[constitution]]"
  - "[[bridge/001-position-encoding-layer/spec|bridge/001-position-encoding-layer]]"
  - "[[bridge/002-document-tracker-synchronization/spec|bridge/002-document-tracker-synchronization]]"
  - "[[bridge/007-enclosing-symbol-context/spec|bridge/007-enclosing-symbol-context]]"
  - "[[mcp/014-tools-list-payload-size/spec|mcp/014-tools-list-payload-size]]"
  - "[[mcp/018-bounded-tool-inputs-and-results/spec|mcp/018-bounded-tool-inputs-and-results]]"
---

# Feature: Symbol Source Text on Location Results

> [!info] Metadata
> **Type**: enhancement (competitor-gap, token-efficiency)
> **Priority**: P4
> **Issue**: #909

> [!abstract]
> The five location tools (`get_definition`, `get_references`, `go_to_implementation`, `go_to_type_definition`, `go_to_declaration`) gain a third `context` level, `symbol_source`. Each result item then carries the bounded source text of the symbol it names (a definition) or sits in (a reference), cut out of the document text the tracker already holds. It adds no LSP request and no disk read beyond the `enclosing_symbol` level, and the default output is unchanged. This supersedes the "bodies are out of scope" decision of [[bridge/007-enclosing-symbol-context/spec|bridge/007]] and the Out of Scope note of [[mcp/007-symbol-name-addressing/spec|mcp/007]] for this opt-in level only.

## 1. Overview

### Problem Statement

After `enclosing_symbol` an agent knows which symbol each location belongs to, but still needs one read per symbol to see its code. For a reference list or a definition that is the next round trip, and the text is already in memory: the tracker holds the document the server was told about.

### Goal

An agent can ask for the source of each result item in the same call, with explicit per-item and per-call bounds, exact with respect to the document version the server answered for, and with no change to any default output.

### Out of Scope

- Symbol-level editing tools, fuzzy or content search, cross-call caching of text.
- `get_diagnostics`: it keeps the two-level `context` (`none`, `enclosing_symbol`); `source` is in neither its type nor its schema.
- Reading from disk: only text of documents opened through the workspace containment gate is sliced.
- Bodies for hierarchy tools and `workspace_symbol_search`.

## 2. User Stories

### US-001: See the code of every result in one call

AS AN AI coding agent
I WANT each definition or reference to carry its symbol's source text
SO THAT I need no follow-up file read per result

```
GIVEN a get_references call with context symbol_source
WHEN the server answers and the files are inside the workspace
THEN each item carries source: included{text, truncated} or a typed reason it has none
```

### US-002: A reference list does not repeat one body

AS A client paying per token
I WANT items that share one span to share one copy of the text
SO THAT 30 references inside one function cost one body

```
GIVEN 30 references inside one 150-line function
THEN exactly one item is included and the other 29 are same_as that item
```

### US-003: Never stale, never unbounded

AS A client
I WANT text that matches the version the server's ranges refer to, and a hard output bound
SO THAT a concurrent edit or a huge symbol cannot mislead or flood me

### US-004: Existing clients are unaffected

AS AN existing client
I WANT `none` and `enclosing_symbol` to behave as before
SO THAT no output or LSP traffic changes unless I opt in

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE SYSTEM SHALL accept `context` = `none` (default) \| `enclosing_symbol` \| `symbol_source` on the five location tools (`LocationContext`). `none` and `enclosing_symbol` produce output and LSP traffic identical to before this spec | must |
| FR-002 | WHEN `symbol_source` is requested THE SYSTEM SHALL send the same requests as `enclosing_symbol` (same `documentSymbol` lookups, file cap, deadline and `parse_file_uri` containment) and SHALL add no LSP request. The excerpt text comes only from the tracker's pinned text. Converting a link's `targetRange` to MCP columns reuses the same budgeted line reader that converts the location's own range, and reads at most the two lines of that range, only for a non-UTF-16 server, an untracked target file and `symbol_source`; it adds no new file and no new trust boundary | must |
| FR-003 | WHEN `symbol_source` is requested THE SYSTEM SHALL give every location item a `source`, a closed sum with status `included`, `same_as` or `not_computed`. The field lives on a location-only item type, so `get_diagnostics` output has no `source` in its type or schema | must |
| FR-004 | THE SYSTEM SHALL pick the span of an item by its tool: for the four go-to tools the **definition rule**, for `get_references` the **enclosing-body rule** (section 5) | must |
| FR-005 | THE SYSTEM SHALL slice `included.text` from the tracker's pinned text of the file, `[start, end)` of the span, lines joined with `\n` (CRLF normalized), only while the document stamp equals the stamp taken before the `documentSymbol` request (FR-006) | must |
| FR-006 | THE SYSTEM SHALL treat the text as current only when, in one tracker lock acquisition, the document is open, `state.opened == expected.opening`, `state.version == expected.version` and the server's synced version equals `expected.version` (`SweepState::Current` with an equal stamp). Any other state gives `not_computed{document_changed}`; a closed document gives `not_computed{document_changed}` as well. A text reload that moved `state.version` without the server being re-synced therefore also fails the check | must |
| FR-007 | THE SYSTEM SHALL bound one item to 200 lines and 16 KiB, cutting on a line boundary else on a character boundary, and SHALL set `truncated` iff it cut. It SHALL never copy more than the byte cap while slicing (a 4 MiB single-line document yields at most 16 KiB) | must |
| FR-008 | THE SYSTEM SHALL bound one call to 64 KiB of `included` text in total (call-budget rules, section 5) | must |
| FR-009 | THE SYSTEM SHALL slice each distinct (uri, span) once per call. A later item with an equal span is `same_as{item}`, where `item` is the index in the same `locations` list of an earlier `included` item with an equal uri and span | must |
| FR-010 | THE SYSTEM SHALL report a skipped item with a typed reason: `no_span`, `call_budget`, `document_changed` or `symbol_lookup_skipped`, never an empty text, `null` or free text | must |
| FR-011 | THE SYSTEM SHALL treat the sliced text as untrusted server-influenced content: it is emitted verbatim as a payload (as workspace-edit payloads are) and is not part of server-text sanitation of notes | must |
| FR-013 | THE SYSTEM SHALL cut at most 256 distinct spans per call (`MAX_SOURCE_CUTS_PER_CALL`) and SHALL report every later new span as `call_budget` without cutting it. Once the call budget is spent, every later new span is `call_budget` without a cut. The order stays file first-seen, then item order | must |
| FR-012 | THE SYSTEM SHALL NOT make the slice exact when `positions_degraded` is set, nor for a link `target_range` in a file that changed on disk between the go-to answer and its open; both are documented as undefined | should |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Output size | Σ `included` bytes per call <= 64 KiB; one item <= 200 lines and 16 KiB |
| NFR-002 | Cost | No allocation proportional to a document or a line beyond the 16 KiB item cap while slicing. At most `MAX_SCANNED_BYTES_PER_CALL` (16 MiB) of document text is examined per call, plus the one cut that crosses it; examined means the bytes read up to each line terminator plus the line-index walk to the first line |
| NFR-003 | Type safety | `SourceOutcome`, `SourceSkipReason` and `LocationContext` are `#[non_exhaustive]` enums; `same_as.item` is an index newtype that in mcpls output names an `included` item; spans are typed (`SpanRule`), not flags |
| NFR-004 | Tool surface | The addition fits the `tools/list` budget after the raise recorded in [[mcp/014-tools-list-payload-size/spec|mcp/014]] (about 800 B per location tool) |
| NFR-005 | Time | The pass reuses the `enclosing_symbol` deadline; slicing is in-memory, and the only file reads are the budgeted line reads of FR-002. Slicing is synchronous but bounded by the per-call scan budget of NFR-002 (milliseconds), so it fits inside the deadline without cancellation points |

## 5. Rules

### Definition rule (the four go-to tools), in priority order

1. The innermost **hierarchical** `documentSymbol` whose `selectionRange` contains the location's range gives that symbol's `range`. The descent uses the enclosing-symbol sibling rule: among overlapping siblings that contain the range, the innermost wins (latest start, earliest end, then name and kind). A flat (`SymbolInformation`) tree never matches.
2. Otherwise the raw `LocationLink.targetRange` of the answer, normalized with the go-to request's encoding context, covering locals, parameters and closure bindings. It applies only when the file's `documentSymbol` lookup succeeded, because the pin and the stamp come from that lookup's open. A failed or unsupported lookup gives `symbol_lookup_skipped` even when a link carried a `targetRange`. mcpls advertises `linkSupport` only for definition and declaration, so for implementation and type definition only rule 1 applies.
3. Otherwise `no_span`.

### Enclosing-body rule (`get_references`)

The `range` of the item's resolved innermost enclosing symbol. A top-level reference gives `no_span`.

### Call-budget rules

- The budget is consumed in file first-seen order, then item order within the file.
- Each distinct span is sliced once with the per-item cap. If the resulting text is longer than the remaining call budget the item is **skipped** (`call_budget`); it is never cut to fit.
- Once the per-call scan budget (NFR-002) is spent, every later new span is `call_budget` without a cut.
- The pass **continues** after a skip: a later, smaller distinct span can still be `included` (greedy skip).
- A repeat of a span that was skipped for budget is `call_budget` too, without slicing it again, and never `same_as`. So `same_as.item` always points to an `included` item.

### Slice boundaries

- A start column inside a character rounds **down** to that character; an end column inside a character rounds **up**.
- Only an end past the end of the line text or `u32::MAX` means the line end.

### Reasons

| Reason | Meaning |
|--------|---------|
| `no_span` | The rule found no span for the item |
| `call_budget` | The span is over the remaining call budget, or the per-call scan budget is spent (or it repeats such a span) |
| `document_changed` | The stamp check of FR-006 failed or the document was no longer open |
| `symbol_lookup_skipped` | The file's `documentSymbol` lookup was skipped or failed; the item's `enclosing_symbol` (`not_computed` or `unavailable`) states why, so file-level reasons are not mirrored. An out-of-workspace item reads `enclosing_symbol: not_computed{out_of_workspace}` and `source: not_computed{symbol_lookup_skipped}` |

## 6. Data Model

| Entity | Description |
|--------|-------------|
| `LocationContext` | `none` \| `enclosing_symbol` \| `symbol_source`, input of the five location tools |
| `NavigationLocation` | The LSP `Location` fields flattened, plus an optional `source`; the item type of the location tools' `Contextual` wrapper (no `Deref`; callers use `inner.location`) |
| `SourceOutcome` | Tagged by `status`: `included{text, truncated}`, `same_as{item}`, `not_computed{reason}` |
| `ItemIndex` | Transparent index that in mcpls output names an earlier `included` item in the same list |
| `SourceSkipReason` | The four reasons above |
| `DocumentTracker::excerpt` | Crate-private tracker accessor performing FR-005..FR-007 in one lock, returning `Changed` or `Untracked` instead of text when the stamp no longer matches or the document is gone |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | 30 references in one 150-line function | 1 `included`, 29 `same_as` |
| SC-002 | A reload between the `documentSymbol` answer and the cut without a re-sync | `document_changed`, no `included` |
| SC-003 | Items A1, B1, A2 with a budget for exactly one file | File first-seen order decides |
| SC-004 | 4 MiB single-line document | `included.text` <= 16 KiB, `truncated` |
| SC-005 | `get_diagnostics` schema | No `source`, no `SourceOutcome` |
| SC-006 | `none` and `enclosing_symbol` | No `source` key; same LSP request count as before |

## 8. Agent Boundaries

### Always
- Slice only text the tracker pinned for the stamp the server answered for.
- Keep `get_diagnostics` free of `source`.

### Ask First
- A larger per-item or per-call cap, or a fourth `context` level.
- Slicing for tools outside the five location tools.

### Never
- Read a file from disk to produce `source`, or open a file outside the workspace containment gate.
- Cut an over-budget span to fit the call budget.
- Report `same_as` for a span whose first occurrence was not `included`.

## 9. Decisions

- Slicing lives under the pin held by the `enclosing_symbol` pass, so no new open or request exists.
- Dedup keys are (uri, span); `Position2D` and `Range` are `Hash` for this.
- A per-call scan budget (`MAX_SCANNED_BYTES_PER_CALL`, 16 MiB, #927) bounds the work of many cuts deep into one long line; `excerpt` reports the bytes it examined and the pass charges them. Cutting all spans of a file in one forward walk, or moving slicing to the blocking pool, was not needed at that bound.
- `symbol_lookup_skipped` reuses the sibling `enclosing_symbol` state instead of mirroring each file-level reason, to stay inside the `get_references` size budget.

## 10. See Also

- [[constitution]] — section VI, no extra round trips on existing flows
- [[MOC-specs]] — all specifications
- [[bridge/001-position-encoding-layer/spec|bridge/001]] — typed columns used for slicing
- [[bridge/002-document-tracker-synchronization/spec|bridge/002]] — tracker, pin and sync state
- [[bridge/007-enclosing-symbol-context/spec|bridge/007]] — the pass this level extends
- [[mcp/014-tools-list-payload-size/spec|mcp/014]] — size budget
- [[mcp/018-bounded-tool-inputs-and-results/spec|mcp/018]] — pin and bounded results
- Code: `crates/mcpls-core/src/bridge/translator/enclosing.rs`, `crates/mcpls-core/src/bridge/state.rs` (`DocumentTracker::excerpt`)
