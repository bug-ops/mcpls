# ADR-007: LSP Server Notification Handling

**Status**: ACCEPTED

**Date**: 2025-12-27

## Context

LSP servers send asynchronous notifications to clients for important events:

- `textDocument/publishDiagnostics` - Diagnostic messages (errors, warnings, hints)
- `window/logMessage` - Log messages from the server
- `window/showMessage` - User-facing messages from the server
- `$/progress` - Progress reporting for long-running operations

Currently, these notifications are logged and ignored (`client.rs:322`). This prevents MCP tools from accessing valuable information like diagnostics that LSP servers compute asynchronously after document changes.

## Decision

Implement **notification caching with MCP tool exposure**:

### 1. Notification Storage Strategy

Store notifications in a shared state structure managed by the Translator:

```rust
struct NotificationCache {
    /// Diagnostics by file URI
    diagnostics: HashMap<Uri, Vec<Diagnostic>>,
    /// Recent server log messages (ring buffer)
    log_messages: VecDeque<LogMessage>,
    /// Recent server messages to user (ring buffer)
    server_messages: VecDeque<ServerMessage>,
    /// Resource limits
    limits: CacheLimits,
}
```

### 2. Cache Management

- **Diagnostics**: Map-based, keyed by URI, replaced on each notification
- **Log messages**: Ring buffer with configurable size (default: 100)
- **Server messages**: Ring buffer with configurable size (default: 50)
- **Progress notifications**: Not cached (Phase 1), may be added later

### 3. Architecture Integration

```
LSP Server → LspClient::message_loop → NotificationCache → MCP Tools
                                             ↑
                                             │
                                      Arc<Mutex<_>>
                                             │
                                       HandlerContext
```

The notification cache will be:
- Owned by `Translator` (similar to `DocumentTracker`)
- Accessed via `Arc<Mutex<Translator>>` in handlers
- Updated by `LspClient::message_loop` via callback/channel

### 4. New MCP Tools

Expose cached data through three new MCP tools:

- `get_cached_diagnostics(file_path)` - Get diagnostics for a specific file
- `get_server_logs(limit?, min_level?)` - Get recent server log messages
- `get_server_messages(limit?)` - Get recent server messages

### 5. Communication Pattern

Use a notification channel to decouple LSP client from Translator:

```rust
// In Translator
pub(crate) fn notification_sender(&self) -> mpsc::Sender<LspNotification>

// In LspClient::message_loop
match InboundMessage::Notification(notification) => {
    // Send to notification handler
    notification_tx.send(notification).await?;
}

// Background task in Translator processes notifications
```

## Consequences

### Positive

- MCP tools can access diagnostics without polling LSP server
- Diagnostics appear immediately after server computes them
- Server logs available for debugging
- No breaking changes to existing API
- Follows existing patterns (`Arc<Mutex<>>`, similar to `DocumentTracker`)
- Bounded memory usage via ring buffers

### Negative

- Increased memory usage (diagnostics for all files + message buffers)
- Cache can become stale if files change externally
- Additional complexity in message routing
- Diagnostics only available after server sends notification

### Mitigation

- Implement resource limits (max diagnostics cache size, buffer sizes)
- Clear diagnostics on `textDocument/didClose`
- Document cache behavior in tool descriptions
- Add metrics for cache size monitoring

## Alternatives Considered

### Polling LSP Server

Request diagnostics on-demand using `textDocument/diagnostic`:

**Rejected because:**
- Not all LSP servers support pull diagnostics
- Push diagnostics are the standard LSP pattern
- Would require periodic polling or manual refresh
- Less efficient than caching push notifications

### Stateless Pass-Through

Forward notifications directly to MCP without caching:

**Rejected because:**
- MCP tools are request-response, cannot receive push notifications
- Would lose diagnostic information between MCP calls
- Doesn't match MCP tool model

### Store in LspClient

Store notifications directly in `LspClient`:

**Rejected because:**
- `LspClient` should remain focused on protocol handling
- `Translator` is the bridge layer, owns MCP-visible state
- Would duplicate state across multiple clients

## Implementation Phases

### Phase 1: Notification Storage (Week 1)

1. Create `NotificationCache` in `bridge/notifications.rs`
2. Add cache ownership to `Translator`
3. Create notification channel infrastructure

### Phase 2: Message Loop Integration (Week 1)

1. Update `LspClient::message_loop` to handle notifications
2. Deserialize notification types (`PublishDiagnosticsParams`, etc.)
3. Send notifications through channel to cache

### Phase 3: MCP Tool Handlers (Week 2)

1. Implement `get_cached_diagnostics` handler
2. Implement `get_server_logs` handler
3. Implement `get_server_messages` handler
4. Add tool registration in `ToolHandlers::new`

### Phase 4: Testing & Documentation (Week 2)

1. Unit tests for notification cache
2. Integration tests with rust-analyzer
3. Update README with new tools
4. Add examples

## References

- [LSP - PublishDiagnostics](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_publishDiagnostics)
- [LSP - Window Notifications](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#window_logMessage)
- [LSP - Progress](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#progress)
