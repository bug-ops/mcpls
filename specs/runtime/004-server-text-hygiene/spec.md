---
aliases:
  - Server text hygiene
  - Redaction and escaping of server-supplied text
tags:
  - sdd
  - spec
  - bug
  - security
  - logging
  - redaction
created: 2026-10-04
status: implemented
related:
  - "[[constitution]]"
  - "[[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp-server-lifecycle-and-respawn]]"
  - "[[mcp/002-mcp-resources-diagnostics/spec|mcp-resources-diagnostics]]"
---

# Feature: Redact and escape text a language server controls

> [!info] Metadata
> **Type**: bug (hardening)
> **Priority**: P3
> **Author**: Andrei G.
> **Issues**: #581, #582, #583
> **Observed at**: `e3d9c66`

## 1. Overview

### Problem Statement

Three paths still let server-supplied text reach operators or MCP clients unfiltered:

1. JSON log mode (`--log-json`) escapes only the control characters serde writes as escapes (C0).
   C1 controls, line and paragraph separators and bidirectional marks stay raw, so a server-chosen
   method name or URI can forge or reorder a log line that text mode already neutralizes (#581).
2. `Error::LspProtocolError` carried a plain `String` built from serde errors, which echo the
   offending value, so a configured secret in a malformed frame reached the error `Display` and the
   MCP client (#582).
3. Diagnostics and `$/progress` text were not redacted. The diagnostics resource serializes the raw
   cached `Diagnostic`, including `data`, `relatedInformation` and `MarkupContent` messages, so a
   server echoing a secret there leaked it through `read_resource` (#583).

### Goal

Every string a server controls is escaped before it is logged and redacted before it is cached,
stored in an error or returned, using the same secret set already applied to logs and stderr.

### Out of Scope

- Redacting tool results (hover text, symbol names, action titles): follow-up #599.
- Redacting URIs: they are cache keys and links (FR-009).

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | THE `mcpls` binary SHALL escape every JSON string of a log line with the same rule as text mode (`escape_control`), covering event fields, span fields and keys | must |
| FR-002 | THE JSON rewrite SHALL decode, escape and re-encode only the string literals that contain a backslash or a character `needs_control_escape` flags, and copy every other byte as is, so key order is identical to a clean line; a literal whose escaping changes nothing keeps its original bytes | must |
| FR-003 | WHEN a literal cannot be decoded THE SYSTEM SHALL write the fixed line `{"level":"ERROR","fields":{"message":"log line suppressed: unescapable field"}}` plus a newline instead of the original | must |
| FR-004 | THE SYSTEM SHALL NOT parse a log line into a `serde_json::Value` or enable `preserve_order`: feature unification would change `mcpls-core` maps and the tool surface snapshot | must |
| FR-005 | `Error::LspProtocolError` SHALL hold a `RedactedText`, constructible only from `Redactions::redact` or a `&'static str`, so an unredacted `String` cannot be stored in it | must |
| FR-006 | THE transport reader and the client SHALL build protocol errors through their `Redactions` (`parse_inbound_message` takes it; `LspClient` keeps it) | must |
| FR-007 | WHEN a `publishDiagnostics` notification arrives THE SYSTEM SHALL redact, per diagnostic, the message (both the plain and the `MarkupContent` form), `source`, a string `code`, every `relatedInformation[].message` and every string leaf of `data` (values only, not keys) before the notification leaves the client; THE SYSTEM SHALL apply the same redaction to every item of a `textDocument/diagnostic` pull report before it is merged with the cache, so a pulled item and its cached twin still deduplicate | must |
| FR-008 | THE SYSTEM SHALL redact the `title` and `message` of `$/progress` payloads; they are never stored or served, so this is cache hygiene only | should |
| FR-009 | THE SYSTEM SHALL NOT redact URIs (`uri`, `relatedInformation[].location.uri`, `codeDescription.href`): they are cache keys and links, and a path-valued secret (an `SSH_AUTH_SOCK`-style value) would otherwise corrupt them | must |
| FR-010 | WHEN the redaction set is empty THE SYSTEM SHALL skip notification redaction, so the common case does not walk `data` on the reader task | should |
| FR-011 | WHEN notification parameters fail to deserialize THE SYSTEM SHALL log the error category only, not serde's message, which can echo a value | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Type safety | The "already redacted" property of a protocol error is carried by `RedactedText`, not by convention |
| NFR-002 | Performance | A clean JSON log line is checked with one scan and written unchanged |
| NFR-003 | Compatibility | JSON field names and order of a clean line are unchanged |

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Field holds `\n`, `\u{202E}`, a C1 control or a tag character | Escaped like text mode; the line stays one line |
| Field holds a quote or a Windows path | Decoded, found unchanged by `escape_control`, original bytes kept (FR-002) |
| Secret is a key of `data` | Not redacted: keys are not values (FR-007) |
| Secret appears in the URI of a related location | Kept (FR-009); the secret is still hidden everywhere else |
| Secret split across two `data` strings | Not matched; the same limit as every other redaction path |

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | JSON log with hostile event and span fields | one line per event, no raw C1/bidi/tag character, expected decoded values |
| SC-002 | Key order of an escaped line vs a clean line | identical |
| SC-003 | Secret in a malformed frame | absent from the `LspProtocolError` text |
| SC-004 | Secret in message, `MarkupContent`, nested `data` and related information read through `get_cached_diagnostics`, the diagnostics resource and a `get_diagnostics` pull | absent; URIs unchanged; no duplicate between a pulled and a cached item |

## 10. See Also

- [[constitution]] -- project principles
- [[MOC-specs]] -- all specifications
- [[lsp/001-lsp-server-lifecycle-and-respawn/spec|lsp/001]] -- server output handling
- Code: `crates/mcpls-cli/src/logging.rs`, `crates/mcpls-core/src/redaction.rs`, `crates/mcpls-core/src/lsp/{client,transport,types}.rs`
