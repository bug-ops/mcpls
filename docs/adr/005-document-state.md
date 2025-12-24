# ADR-005: Document State Management

**Status**: ACCEPTED

**Date**: 2025-12-24

## Context

LSP requires document lifecycle management:
- `textDocument/didOpen` before any feature request
- `textDocument/didChange` when content changes
- `textDocument/didClose` when done with document

MCP tools are stateless from the LLM's perspective.

We need to bridge stateless MCP calls to stateful LSP sessions.

## Decision

Implement **lazy opening with optimistic caching**:

1. **Lazy Opening**: Open documents on first MCP tool call for that file
2. **Optimistic Caching**: Keep documents open until server shutdown
3. **Version Tracking**: Increment version on each change notification

```rust
struct DocumentTracker {
    documents: HashMap<PathBuf, DocumentState>,
}

impl DocumentTracker {
    async fn ensure_open(&mut self, path: &Path, client: &LspClient) -> Result<()> {
        if self.documents.contains_key(path) {
            return Ok(()); // Already open
        }

        let content = tokio::fs::read_to_string(path).await?;
        client.did_open(path, content.clone()).await?;

        self.documents.insert(path.to_path_buf(), DocumentState {
            uri: path_to_uri(path),
            language_id: detect_language(path),
            version: 1,
            content,
        });

        Ok(())
    }
}
```

## Consequences

### Positive

- Minimizes `didOpen` calls (improves performance)
- Simplifies MCP tool handlers (no state management)
- LSP server retains context between calls
- Predictable behavior

### Negative

- Memory usage grows with unique files accessed
- No automatic cleanup (documents stay open)
- Stale content if file changes externally

### Mitigation

- Future: Implement LRU eviction for large workspaces
- Future: Add file watcher for external changes

## Alternatives Considered

### Open/close per MCP call

Rejected because:
- Inefficient (repeated file reads)
- LSP server loses compilation context
- Slower responses

### External file watcher

Rejected because:
- Complexity out of scope for v1.0
- Platform-specific implementations
- Can be added later

## References

- [LSP - Text Document Synchronization](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_synchronization)
