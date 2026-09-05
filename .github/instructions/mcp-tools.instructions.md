---
applyTo: "crates/mcpls-core/src/mcp/**"
---

## Type safety

Tool parameter structs must use domain types, not raw primitives, for values with
protocol-specific conventions. Position fields (line, column) must be documented as
1-based in both the `///` doc comment and the `#[schemars(description)]` — the MCP↔LSP
coordinate conversion happens in the bridge layer, not here.

Path parameters must accept absolute paths. State this in the schema description; AI
clients use it to form requests correctly.

New tool structs require `#[derive(JsonSchema)]`. Omitting it silently removes the tool
from the schema exposed to clients — they cannot discover or invoke it.

## Error handling

Tool handlers must return structured MCP error responses on failure, never panic.
`unwrap()` and `expect()` in handler code kill the entire server process and drop all
active client sessions. Use `?` with proper error mapping instead.

All error variants must carry enough context for the caller to understand what went
wrong. A bare `"internal error"` string is not actionable. Include the file path,
method name, or other relevant detail.

## Idiomatic dispatch

When adding a new tool, register it in both the capability advertisement (tool list)
and the dispatch match arm. A tool missing from the list is undiscoverable; a tool
missing from dispatch panics at runtime. A compile-time check (e.g. a const assertion
or a test that calls every listed tool name through dispatch) is preferable to relying
on manual review.

Prefer exhaustive match arms over wildcard catch-alls in dispatch. A wildcard silently
swallows unhandled tool names; exhaustive matching makes unregistered names a compile
error.
