# ADR 0001 — Handling external file changes

## Status

Accepted (2026-04-27).

## Context

`DocumentTracker::ensure_open` reads each file from disk exactly once per
session and never re-syncs, so any modification made outside mcpls — git
operations, the MCP host's own edit tools, formatters, code generators —
is invisible to both mcpls and the underlying LSP server until the
process is restarted. This produces stale answers from every per-file
tool (`get_hover`, `get_definition`, `get_references`,
`get_document_symbols`, `get_diagnostics`, `get_completions`,
`get_code_actions`, `format_document`, `rename_symbol`, the call
hierarchy tools) and from `workspace_symbol_search`. See issue #102 for
the verified repro.

## Decision

Two complementary changes:

1. **Stat-on-access in `ensure_open`** (mcpls-side, applies to every
   server). On every call we stat the file and compare `(mtime, size)`
   against the tracked `DocumentState`. On mismatch we send
   `textDocument/didClose` followed by `textDocument/didOpen` with a
   bumped version, replacing the cached state. This fixes every per-file
   tool against every LSP server, including those that do not register
   `workspace/didChangeWatchedFiles` (e.g. zls).

2. **`workspace/didChangeWatchedFiles`** (LSP-side, eager). For LSP
   servers that dynamically register file watchers via
   `client/registerCapability`, mcpls now declares
   `workspace.didChangeWatchedFiles.dynamic_registration: true`,
   handles the inbound registration request, runs a `notify`-based
   filesystem watcher per server, and forwards matching events as
   `workspace/didChangeWatchedFiles` notifications. The watcher also
   invalidates the `DocumentTracker` entry for any affected path so
   that the next `ensure_open` re-syncs (composes cleanly with #1).

Manual `reload_workspace` is intentionally out of scope here.

## Consequences

- Every per-file MCP tool now reflects on-disk truth without restart.
- For watcher-registering servers (rust-analyzer, gopls, pyright,
  typescript-language-server, clangd) the LSP's workspace index also
  stays live, fixing `workspace_symbol_search` staleness.
- The transport gains an `InboundMessage::Request` variant and the
  client gains a small server-to-client request dispatcher, which is
  also useful for future protocol features (`workspace/configuration`,
  work-done progress, etc.).
- New runtime dependency: `notify`. Watchers run in a blocking thread
  bridged to tokio via `std::sync::mpsc` → `tokio::sync::mpsc`.
- Watcher failure (e.g. inotify exhaustion) is logged and the server
  continues; #1 still covers per-file freshness in that case.

## Alternatives considered

- **Synthesise `textDocument/didChange` with full-content edits** instead
  of close+reopen on resync. Rejected: more complex, requires accurate
  range/version tracking we do not otherwise need, and the close+reopen
  pair is what rust-analyzer's own VS Code client does on external
  changes when it cannot prove edits are local.
- **`notify-debouncer-full`** instead of raw `notify`. Rejected for
  simplicity; we run a small in-process coalescer and do not need
  rename correlation.
- **A `reload_workspace` MCP tool** as a manual escape hatch. Useful but
  orthogonal; not implemented here.
