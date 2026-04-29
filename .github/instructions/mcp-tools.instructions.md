---
applyTo: "crates/mcpls-core/src/mcp/**"
---

## MCP layer review checklist

### Tool parameter structs (`tools.rs`)

- Every position parameter (`line`, `character`) must be documented as 1-based in both
  the `///` doc comment and the `#[schemars(description)]` annotation. LSP is 0-based;
  the conversion happens in `bridge/encoding.rs` — not here.
- `file_path` parameters must accept absolute paths only. Doc comments must state this
  explicitly so AI clients don't pass relative paths.
- New tool structs require `#[derive(JsonSchema)]` — omitting it silently removes the
  tool from the MCP schema exposed to clients.

### Tool dispatch (`handlers.rs`, `server.rs`)

- Every tool handler must call `ensure_open` (via `Translator`) before issuing any LSP
  request. Skipping it produces stale results when the file has changed on disk since
  the last access.
- Tool errors must propagate as MCP error responses, not panics. `unwrap()` and
  `expect()` are not acceptable in handler code.
- New tools must be registered in both the tool list and the dispatch match arm.
  A tool missing from either will either never appear to clients or panic at runtime.
