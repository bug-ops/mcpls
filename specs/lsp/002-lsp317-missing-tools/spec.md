# Spec lsp/002: Add Missing LSP 3.17 Tools (Inlay Hints, Type Hierarchy, Signature Help)

**Type**: enhancement
**Priority**: P3
**Status**: draft

## Problem Statement

mcpls exposes 16 MCP tools covering the core LSP surface. However, LSP 3.17 introduced several
capabilities that are absent from mcpls and present in reference/competing implementations:

| LSP Feature | LSP spec version | In mcpls? | In isaacphi/mcp-language-server? | In Tritlo/lsp-mcp? |
|------------|----------------|-----------|----------------------------------|---------------------|
| Inlay hints (`textDocument/inlayHint`) | 3.17 | No | No | No |
| Type hierarchy (`textDocument/prepareTypeHierarchy`) | 3.17 | No | No | No |
| Signature help (`textDocument/signatureHelp`) | 3.15 | No | No | No |
| Go to implementation (`textDocument/implementation`) | 3.6 | No | No | No |
| Go to type definition (`textDocument/typeDefinition`) | 3.6 | No | No | No |
| Selection range (`textDocument/selectionRange`) | 3.15 | No | No | No |

The highest-value missing capabilities for AI code navigation are:
1. **Signature help** — method parameter hints; highly useful when AI is generating call sites
2. **Go to implementation** — navigate from interface/trait to concrete implementations
3. **Type hierarchy** — navigate supertype/subtype chains
4. **Inlay hints** — inline type annotations; improves AI's ability to understand code without
   asking for hover on every symbol

These are not mere niceties: `get_signature_help` and `go_to_implementation` are explicitly
requested by users of similar tools (see isaacphi/mcp-language-server issues).

## User Stories

- As an AI agent writing Rust code, I want `get_signature_help` to understand function parameter
  types without hovering each symbol individually.
- As an AI agent exploring a Rust codebase, I want `go_to_implementation` to find which structs
  implement a given trait.
- As an AI agent reviewing code, I want `get_inlay_hints` to see inferred types inline.

## Functional Requirements

1. Add `get_signature_help` tool — takes file_path, line, character; returns parameter labels,
   documentation, and active parameter index.
2. Add `go_to_implementation` tool — takes file_path, line, character; returns list of
   implementation locations.
3. Add `get_inlay_hints` tool — takes file_path, start_line, end_line; returns list of hint
   labels with positions.
4. Add `go_to_type_definition` tool — takes file_path, line, character; returns type definition
   location.
5. `lsp-types` 0.97 already has all required request/response types for these.

## Non-Functional Requirements

- Tools must follow existing 1-based position convention.
- Each new tool needs unit tests (params serialization) and integration test stubs.
- Graceful degradation: if the LSP server doesn't support a capability, return empty result.

## See Also

- LSP 3.17 spec: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/
- lsp-types 0.97: https://docs.rs/lsp-types/0.97.0/lsp_types/
- Existing tool pattern: `crates/mcpls-core/src/mcp/server.rs`, `crates/mcpls-core/src/bridge/translator.rs`
