# Copilot Instructions for mcpls

mcpls is a bridge between MCP (Model Context Protocol) and LSP (Language Server Protocol).
AI clients speak MCP to mcpls; mcpls spawns and communicates with language servers over LSP.

```
AI Client ←→ [MCP/stdio] ←→ mcpls ←→ [LSP/stdio] ←→ language server
```

Key crates: `mcpls-core/src/bridge/`, `lsp/`, `mcp/`, `config/`.

## Correctness

**Position encoding** — MCP is 1-based, LSP is 0-based. All line/column conversions must
go through the dedicated encoding module. Direct arithmetic on position values outside
that module is a bug.

**LSP JSON-RPC message classification** — four shapes exist:
- `method` + `id` → server-to-client request (must be replied to)
- `method` only → notification (fire-and-forget)
- `id` + `result` or `error` → response
- anything else → protocol error

A message with `id` but no `method`, `result`, or `error` is invalid and must not be
silently treated as a successful `null` response.

**Document version monotonicity** — `textDocument/didOpen` version numbers must strictly
increase per URI across the document's lifetime. On counter overflow, reset to 1 rather
than saturating — after `didClose`/`didOpen` the server treats the document as fresh
regardless of the version number.

**Filesystem signature freshness** — when caching file state with `(mtime, size)`, stat
and read must be bound to the same moment. A stat before the read creates a TOCTOU
window: on filesystems with low mtime resolution a same-size rewrite within a single
clock tick leaves the signature unchanged while content has changed.

## Concurrency

**Lock granularity** — a mutex that guards a large struct must not be held across async
I/O. If a background task and a request handler share the same lock, and the request
handler awaits a network round-trip while holding it, the background task stalls. When
the background channel fills, the network response can no longer be delivered —
head-of-line deadlock. Prefer fine-grained locks scoped to the data they protect.

**Channel drop-on-full semantics** — `std::sync::mpsc::SyncSender::send` blocks when
the channel is full; it does not drop. Use `try_send` when the intent is to discard
events under backpressure and never block the sender thread.

## Dependencies

When a new crate is introduced, verify its license is in the `cargo deny` allow list.
A missing license will fail `cargo deny check licenses` in CI on the first run.

## Style

- All `pub` items must have `///` doc comments explaining *what* and *why*.
- No `unsafe` code — `deny(unsafe_code)` is enforced workspace-wide.
- Public enums that may gain variants must be marked `#[non_exhaustive]`, and breaking
  additions must be documented under `### Changed` in CHANGELOG.
