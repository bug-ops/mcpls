---
applyTo: "crates/mcpls-core/src/mcp/**"
---

## Tool parameter types

Every position parameter must be documented as 1-based in both the doc comment and the
schema description. The MCP↔LSP coordinate conversion happens in the bridge layer, not
in the tool definition — the doc comment is the contract visible to AI clients.

Path parameters must accept absolute paths. State this explicitly in the description so
clients do not pass relative paths that silently resolve to wrong locations.

New tool structs require `#[derive(JsonSchema)]`. Omitting it silently removes the tool
from the schema exposed to clients — callers cannot discover or invoke it.

## Tool dispatch

Every tool handler must ensure the target document is open and in sync with disk before
issuing an LSP request. Skipping the open step returns stale results when the file was
modified externally since the last access.

Errors must propagate as structured MCP error responses. `unwrap()` and `expect()` are
not acceptable in handler code — a panic kills the entire server process and drops all
active sessions.

When adding a new tool, register it in both the tool list (capability advertisement) and
the dispatch match arm. A tool present in only one of the two will either never be
discoverable by clients or panic at runtime when called.
