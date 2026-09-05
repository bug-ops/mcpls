---
aliases:
  - Position encoding layer
  - MCP/LSP position conversion
tags:
  - sdd
  - spec
  - bridge
  - encoding
created: 2026-08-05
status: implemented
related:
  - "[[constitution]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec]]"
  - "[[config/001-config-discovery-and-heuristics/spec]]"
---

# Feature: Position Encoding Conversion Between MCP and LSP

> [!info] Metadata
> **Author**: retroactive spec, authored during the `.local/specs/` → `specs/` migration and gap
> analysis (no single originating commit/PR — this documents already-shipped, working
> functionality accreted across multiple PRs)
> **Type**: core subsystem (critical path)
> **Priority**: P1 (retroactive — reflects the criticality of a subsystem every tool call depends
> on, not an open defect)

> [!success] Resolution
> This is a retroactive spec: `crates/mcpls-core/src/bridge/encoding.rs` already implements
> everything described below. There is no single originating PR to cite — the module has been
> extended incrementally (negotiated-encoding support via PR #289/#291 referenced from
> [[lsp/004-lsp-318-draft-gaps/spec|spec lsp/004]]'s Resolution callout, char-boundary panic fixes, astral
> character / surrogate-pair edge cases) rather than designed once. Representative evidence: the
> module's 20+ unit tests (`crates/mcpls-core/src/bridge/encoding.rs`, `mod tests`) cover UTF-8,
> UTF-16, and UTF-32 round trips, ASCII-identical-across-encodings, out-of-bounds fallback,
> mid-character-boundary rejection, astral (non-BMP) characters, and CRLF line-ending handling.

## 1. Overview

### Problem Statement

MCP and LSP disagree on two axes for every position mcpls translates between a client and a
language server:

1. **Line/column base**: MCP positions are 1-based (line 1, character 1 is the start of a file);
   LSP positions are 0-based.
2. **Character unit**: LSP's `PositionEncodingKind` allows a server to negotiate UTF-8, UTF-16, or
   UTF-32 code units for its `character` field (`initialize`'s
   `capabilities.general.positionEncodings`, negotiated in
   [[lsp/001-lsp-server-lifecycle-and-respawn/spec|spec lsp/001]]'s `LspServer::spawn`). MCP's own
   character columns are always UTF-16 code units (the pre-negotiation, universally-supported
   default). A server that negotiates UTF-8 or UTF-32 requires re-deriving the column from the
   line's actual text, not a fixed arithmetic offset — a naive 1:1 column mapping silently
   corrupts every position on a line containing any multi-byte character (accented letters,
   CJK text, emoji, or any astral-plane code point).

Getting this wrong is not a cosmetic bug: every navigation tool (hover, definition, references,
rename, call hierarchy, diagnostics ranges) depends on exact position round-tripping. A one-column
drift on a line with a preceding multi-byte character silently points at the wrong symbol, with no
error raised — the request "succeeds" against the wrong location.

### Goal

Every MCP↔LSP position conversion (a) correctly maps MCP's 1-based/UTF-16 convention to and from
LSP's 0-based/negotiated-encoding convention, for `Utf8`/`Utf16`/`Utf32`; (b) never panics on
untrusted input (a server-reported byte offset that lands mid-character); and (c) degrades to a
safe fallback (the raw, unconverted value) rather than silently producing a plausible-looking but
wrong position when an exact conversion is not possible.

### Out of Scope

- Negotiating which encoding a server uses — that is `LspServer::spawn`'s responsibility
  ([[lsp/001-lsp-server-lifecycle-and-respawn/spec|spec lsp/001]]); this spec covers only the conversion
  math once an encoding is known.
- Line-ending normalization — `line_text` is always sourced without its terminator (via
  `str::lines()` or equivalent), so CRLF vs. LF is not this module's concern.
- Range conversion orchestration (which end of a range needs which line's text) — that lives in
  `bridge/translator/encoding_ctx.rs`, a consumer of this module, not part of it.

## 2. User Stories

### US-001: Hover/definition/rename land on the correct symbol on a non-ASCII line

AS AN AI coding agent operating on a file containing non-ASCII characters (accented letters, CJK
comments, emoji in string literals)
I WANT every position-based tool call to resolve to the exact character the client meant, even
when the negotiated LSP server encoding differs from MCP's UTF-16 convention
SO THAT hover/definition/rename/etc. never silently target the wrong symbol on such a line

**Acceptance criteria:**
```
GIVEN a line of text containing a multi-byte UTF-8 character before the target column
  AND the LSP server has negotiated UTF-8 position encoding
WHEN mcpls converts an MCP (1-based, UTF-16) position to an LSP (0-based, UTF-8) position
THEN the resulting LSP character offset correctly points at the same character the MCP caller
     specified, re-derived from the line's actual text rather than a fixed arithmetic offset
```

### US-002: A malformed or out-of-bounds position never crashes the process

AS A mcpls operator
I WANT a server-reported byte offset that lands mid-character, or a client-reported column past
the end of a line, to be handled gracefully
SO THAT a single malformed position cannot panic the process (`panic = "abort"` is configured
project-wide, so a panic here would kill every in-flight request, not just one)

**Acceptance criteria:**
```
GIVEN a byte offset that lands inside a multi-byte character (not on a UTF-8 char boundary)
WHEN byte_offset_to_character/character_to_byte_offset is called with that offset
THEN the function returns an Err, never panics

GIVEN an MCP character column past the end of the target line's text
WHEN mcp_to_lsp_position converts it
THEN the raw (unconverted) value is used as a fallback rather than erroring the whole request
```

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | THE SYSTEM SHALL convert an MCP position (1-based line/character) to an LSP position (0-based) via `mcp_to_lsp_position`, and the inverse via `lsp_to_mcp_position` | must |
| FR-002 | WHEN the negotiated encoding is `Utf16` THE SYSTEM SHALL perform a pure line/column offset with no `line_text` lookup — byte-for-byte identical to fixed-encoding (pre-negotiation) behavior, since MCP's own columns are already UTF-16 | must |
| FR-003 | WHEN the negotiated encoding is `Utf8` or `Utf32` AND `line_text` is available THE SYSTEM SHALL re-derive the column in the target encoding's units from the line's actual text, not a fixed arithmetic offset | must |
| FR-004 | WHEN `line_text` is unavailable (e.g. the file could not be read) THE SYSTEM SHALL fall back to the raw, unconverted MCP/LSP character rather than failing the request | must |
| FR-005 | WHEN a re-derived character offset does not round-trip exactly (lands inside a multi-unit character, e.g. a UTF-16 surrogate pair or a multi-byte UTF-8 sequence) THE SYSTEM SHALL fall back to the raw value rather than silently rounding forward to the next character boundary | must |
| FR-006 | WHEN `byte_offset_to_character`/`character_to_byte_offset` is given a byte offset that is not on a UTF-8 character boundary THE SYSTEM SHALL return an `Err`, never panic | must |
| FR-007 | THE SYSTEM SHALL support parsing (`PositionEncoding::from_lsp`) and serializing (`PositionEncoding::to_lsp`) the three LSP-defined encoding kind strings: `"utf-8"`, `"utf-16"`, `"utf-32"` | must |
| FR-008 | WHEN `line.saturating_sub(1)` or `character.saturating_sub(1)` would underflow (an MCP position of `0`) THE SYSTEM SHALL clamp to `0` rather than wrapping or panicking | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Safety | No conversion path may panic on untrusted input (a server-reported or client-reported offset), consistent with the workspace's `panic = "abort"` configuration where a single panic kills the whole process, not just one request |
| NFR-002 | Correctness | ASCII-only text must convert identically regardless of negotiated encoding (`Utf8`==`Utf16`==`Utf32` for pure-ASCII lines), since ASCII characters are exactly 1 byte/1 UTF-16 unit/1 code point in every encoding |
| NFR-003 | Performance | Conversion for the common case (`Utf16`, the default and most-negotiated encoding per `config/mod.rs`'s `default_position_encodings` doc comment) must be O(1) — no `line_text` scan at all |
| NFR-004 | Testability | Every fallback path (missing `line_text`, out-of-bounds, mid-character-boundary, astral/surrogate-pair) must have a dedicated regression test, since these are exactly the cases a naive implementation gets wrong silently |

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|-----------------|
| `PositionEncoding` | The three LSP-negotiable character-counting conventions | `Utf8`, `Utf16` (default), `Utf32` |
| `EncodingConverter` | Stateless converter bound to one `PositionEncoding`, converting between byte offsets and character offsets in a given line of text | `byte_offset_to_character(text, byte_offset) -> Result<u32, String>`, `character_to_byte_offset(text, character_offset) -> Result<usize, String>` |
| MCP position | 1-based `(line, character)`, character always in UTF-16 units | `line: u32`, `character: u32` |
| LSP `Position` (`lsp_types::Position`) | 0-based `(line, character)`, character in the negotiated encoding's units | `line: u32`, `character: u32` |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|--------------------|
| Negotiated encoding is `Utf16` | Pure arithmetic offset; `line_text` never consulted even if supplied (locked in by `test_utf16_negotiated_ignores_line_text`) |
| Negotiated encoding is `Utf8`/`Utf32`, ASCII-only line | All three encodings agree (`test_mcp_to_lsp_position_ascii_identical_across_encodings`) |
| Negotiated encoding is `Utf8`, line contains a 2-byte character (e.g. `é`) before the target column | Column re-derived in bytes, one further than a naive UTF-16-based offset (`test_mcp_to_lsp_position_utf8_negotiated_multibyte`) |
| Negotiated encoding is `Utf8`, line contains an astral character (e.g. `𝄞`, 4 UTF-8 bytes / 2 UTF-16 units) | Column correctly re-derived across the surrogate pair (`test_mcp_to_lsp_position_utf8_negotiated_astral_char`) |
| MCP character offset lands mid-surrogate-pair (client miscounted an astral character) | Falls back to the raw offset rather than rounding forward past the whole character (`test_mcp_to_lsp_position_mid_surrogate_falls_back`) |
| Byte offset lands mid-character (not a char boundary) | `Err`, not a panic (`test_byte_offset_to_character_mid_char_boundary_does_not_panic`) — a documented regression fix, since `text[..byte_offset]` would otherwise panic and, under `panic = "abort"`, kill the whole process |
| MCP character far past the end of a short line | Falls back to the raw (unconverted) value (`test_mcp_to_lsp_position_out_of_bounds_falls_back`) |
| `line_text` is `None` (file unreadable) | Falls back to the raw value (`test_mcp_to_lsp_position_missing_line_text_falls_back`) |
| MCP position `(0, 0)` | Clamped to `(0, 0)` via `saturating_sub`, no underflow (`test_saturating_sub_zero`) |
| CRLF-terminated source line | No column shift — `line_text` never includes the terminator (`test_mcp_to_lsp_position_utf8_negotiated_crlf_line_text`) |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | `cargo nextest run -E 'package(mcpls-core) and test(encoding)'` | All existing unit tests in `bridge/encoding.rs` pass |
| SC-002 | Round-trip property: `lsp_to_mcp_position(mcp_to_lsp_position(l, c, ...), ...) == (l, c)` for the `Utf16` fast path | Holds for `line`/`char` in `1..100` (`test_roundtrip`) |
| SC-003 | No panic on any fuzzed/adversarial byte offset within `0..=text.len()` | Every offset either succeeds or returns `Err`, never panics |

## 8. Agent Boundaries

### Always (without asking)
- Add a dedicated unit test for any new fallback/edge case discovered (astral characters,
  surrogate pairs, out-of-bounds, mid-character boundaries follow this pattern already)
- Keep the `Utf16` fast path free of any `line_text` scan — this is a deliberate performance/
  correctness invariant (NFR-003), not an accidental optimization

### Ask First
- Changing the fallback behavior (raw/unconverted value) to instead error the whole request —
  this is a deliberate design choice (graceful degradation over hard failure) that several
  existing tests lock in

### Never
- Slice `text` at a byte offset without first checking `text.is_char_boundary(offset)` — this is
  exactly the panic class `byte_offset_to_character`'s guard exists to prevent, and under
  `panic = "abort"` such a panic kills the entire process, not just one request

## 9. Open Questions

None — this is a retroactive spec documenting stable, already-shipped, well-tested behavior.

## 10. See Also

- [[constitution]] — project principles (not yet created for this project)
- [[MOC-specs]] — all specifications
- [[lsp/003-lsp-types-unmaintained-migration/spec|spec lsp/003]] — names `bridge/encoding.rs` as the
  critical-path code most sensitive to any `lsp-types`/`ls-types` type-level divergence
- [[lsp/004-lsp-318-draft-gaps/spec|spec lsp/004]] — Resolution callout documents the PRs (#289/#291)
  that wired negotiated `PositionEncodingKind` into this module and `DocumentTracker`-backed line
  lookups
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|spec lsp/001]] — where `PositionEncodingKind` is
  negotiated during `LspServer::spawn`'s `initialize` handshake
- `crates/mcpls-core/src/bridge/encoding.rs` — the module this spec documents
- `crates/mcpls-core/src/bridge/translator/encoding_ctx.rs` — consumer that orchestrates range
  conversion using this module's single-position primitives
