# Spec bridge/003: Replace `Arc<Mutex<Translator>>` with `Arc<RwLock<Translator>>`

**Type**: enhancement
**Priority**: P2
**Status**: draft

## Problem Statement

Every one of the 16 MCP tool handlers in `mcpls-core/src/mcp/server.rs` acquires a full exclusive
`Mutex` lock on the `Translator` before dispatching an LSP request:

```rust
let mut translator = self.context.translator.lock().await;
translator.handle_hover(...).await
```

Most LSP calls are read operations (hover, definition, references, document symbols, completions,
call hierarchy, workspace symbol search, code actions, get_diagnostics). Only a small subset
mutate `Translator` state (rename_symbol, format_document, and the document-open bookkeeping
inside `DocumentTracker`).

Holding an exclusive lock across the full async round-trip to the LSP server (which may take
hundreds of milliseconds for a cold rust-analyzer query) serialises all concurrent tool calls.
This is the root cause described in #104 (notification pump starvation) and the motivation for
#108 (reduce lock hold time).

## User Stories

- As an AI agent issuing multiple simultaneous tool calls, I want hover and definition calls to
  run concurrently rather than queued behind each other.
- As a developer, I want the notification pump to receive push diagnostics without waiting for
  a pending hover call to release the lock.

## Functional Requirements

1. `Arc<Mutex<Translator>>` must be replaced with `Arc<RwLock<Translator>>`.
2. Read-only tool handlers (hover, definition, references, document_symbols, completions,
   call_hierarchy, workspace_symbol_search, code_actions, get_cached_diagnostics, get_server_logs,
   get_server_messages) must acquire a read lock (`read().await`).
3. Mutating handlers (rename_symbol, format_document, get_diagnostics which triggers didOpen)
   must acquire a write lock (`write().await`).
4. The `NotificationCache` must be separated from `Translator` into its own `Arc<RwLock<...>>`
   (per spec for #104) so the pump can write diagnostics without waiting for a read lock on the
   full `Translator`.
5. No deadlocks: no code path may hold a read lock and attempt to acquire a write lock.

## Non-Functional Requirements

- Concurrent read throughput: ≥ 2x improvement on multi-tool batches (hover + definition).
- No regression on single-tool latency.
- MSRV compatibility: `tokio::sync::RwLock` is available since tokio 1.0.

## Alternatives Considered

- **arc-swap**: Suitable for config-like data that is atomically replaced wholesale; does not
  fit `Translator` which is mutated in-place.
- **dashmap**: Only applicable to HashMap-shaped state; Translator has richer structure.
- **message-passing (channel)**: Would require restructuring all 16 handlers significantly;
  higher complexity than RwLock upgrade.

## See Also

- #104 — split NotificationCache out of Translator lock
- #108 — reduce Mutex hold time
- tokio docs: https://docs.rs/tokio/latest/tokio/sync/struct.RwLock.html
- tokio discussion on std vs tokio Mutex: https://github.com/tokio-rs/tokio/discussions/7627
