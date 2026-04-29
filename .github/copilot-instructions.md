# Copilot Instructions for mcpls

mcpls is a bridge between MCP (Model Context Protocol) and LSP (Language Server Protocol).
AI clients speak MCP to mcpls; mcpls spawns and communicates with language servers over LSP.

## Architecture

```
AI Client ←→ [MCP/stdio] ←→ mcpls ←→ [LSP/stdio] ←→ rust-analyzer / tsgo / pyright / ...
```

Key crates:
- `mcpls-core/src/bridge/` — MCP→LSP translation, document state, diagnostics cache
- `mcpls-core/src/lsp/` — LSP client, JSON-RPC 2.0 lifecycle, file watcher
- `mcpls-core/src/mcp/` — MCP server, tool definitions and dispatch
- `mcpls-core/src/config/` — TOML config, LSP server discovery heuristics

## Code Review Priorities

### Correctness

- **Position encoding**: MCP is 1-based, LSP is 0-based. All conversions must go through
  `bridge/encoding.rs`. Flag any direct arithmetic on line/column values outside that module.

- **LSP protocol compliance**: Inbound JSON-RPC messages fall into four shapes:
  `method`+`id` → server request, `method` only → notification,
  `id`+(`result`|`error`) → response, anything else → protocol error.
  Responses without `result` or `error` must not be silently treated as `null`.

- **Document versioning**: `textDocument/didOpen` version numbers must be strictly
  monotone per document URI. After `didClose`/`didOpen` resync cycles, reset to 1
  rather than saturating at `i32::MAX`.

- **Glob patterns**: `globset` matches against the full absolute path. Bare patterns
  like `*.rs` only match filenames with no directory component. Patterns without a
  leading `/` or `**/` prefix must be anchored with `**/` before passing to `GlobSetBuilder`.

### Concurrency

- **Lock scope**: `Arc<Mutex<Translator>>` must never be held across async LSP I/O
  (notify calls, request round-trips). If a lock guard is held while awaiting a network
  operation, flag it — this creates head-of-line blocking between request handlers and
  the `notification_pump`.

- **Channel semantics**: `std::sync::mpsc::SyncSender::send` blocks when the channel is
  full; it does not drop. Use `try_send` when the intent is drop-on-full.

### Dependencies

- Check `deny.toml` allow list when new crates are introduced. Licenses not in the list
  will fail `cargo deny check licenses` in CI. Common gap: `CC0-1.0` (used by `notify`).

## Style

- All `pub` types, traits, functions, and methods must have `///` doc comments explaining
  *what* and *why*, not just restating the name.
- No `unsafe` code — `deny(unsafe_code)` is enforced workspace-wide.
- `#[non_exhaustive]` is required on any public enum that may gain variants in future releases.
