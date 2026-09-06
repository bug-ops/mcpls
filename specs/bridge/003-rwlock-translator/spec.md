---
aliases:
  - RwLock Translator
  - Concurrent read locks
tags:
  - sdd
  - spec
  - enhancement
  - bridge
  - concurrency
created: 2026-08-01
status: draft
related:
  - "[[constitution]]"
  - "[[bridge/001-position-encoding-layer/spec]]"
  - "[[bridge/002-document-tracker-synchronization/spec]]"
---

# Feature: Replace `Arc<Mutex<Translator>>` with `Arc<RwLock<Translator>>`

> [!info] Metadata
> **Type**: enhancement
> **Priority**: P2

## 1. Overview

### Problem Statement

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

## 2. User Stories

### US-001: Concurrent read operations

AS AN AI agent issuing multiple simultaneous tool calls,
I WANT hover and definition calls to run concurrently rather than queued behind each other,
SO THAT latency for batched requests is not cumulative.

### US-002: Non-blocking notification pump

AS A developer,
I WANT the notification pump to receive push diagnostics without waiting for a pending hover call to release the lock,
SO THAT diagnostics updates are not stalled by unrelated read operations.

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | `Arc<Mutex<Translator>>` must be replaced with `Arc<RwLock<Translator>>` | must |
| FR-002 | Read-only tool handlers (hover, definition, references, document_symbols, completions, call_hierarchy, workspace_symbol_search, code_actions, get_cached_diagnostics, get_server_logs, get_server_messages) must acquire a read lock (`read().await`) | must |
| FR-003 | Mutating handlers (rename_symbol, format_document, get_diagnostics which triggers didOpen) must acquire a write lock (`write().await`) | must |
| FR-004 | The `NotificationCache` must be separated from `Translator` into its own `Arc<RwLock<...>>` (per spec #104) so the pump can write diagnostics without waiting for a read lock on the full `Translator` | must |
| FR-005 | No deadlocks: no code path may hold a read lock and attempt to acquire a write lock | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Concurrent read throughput: ≥ 2x improvement on multi-tool batches (hover + definition) |
| NFR-002 | Performance | No regression on single-tool latency |
| NFR-003 | Compatibility | MSRV compatibility: `tokio::sync::RwLock` is available since tokio 1.0 |

## 5. Alternatives Considered

- **arc-swap**: Suitable for config-like data that is atomically replaced wholesale; does not fit `Translator` which is mutated in-place.
- **dashmap**: Only applicable to HashMap-shaped state; Translator has richer structure.
- **message-passing (channel)**: Would require restructuring all 16 handlers significantly; higher complexity than RwLock upgrade.

## 6. See Also

- [[constitution]] — project principles
- [[bridge/001-position-encoding-layer/spec]] — foundational protocol conversion layer
- [[bridge/002-document-tracker-synchronization/spec]] — document state management
- #104 — split NotificationCache out of Translator lock
- #108 — reduce Mutex hold time
- tokio docs: https://docs.rs/tokio/latest/tokio/sync/struct.RwLock.html
- tokio discussion on std vs tokio Mutex: https://github.com/tokio-rs/tokio/discussions/7627
