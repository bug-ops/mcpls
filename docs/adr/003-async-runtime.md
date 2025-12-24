# ADR-003: Async Runtime Selection

**Status**: ACCEPTED

**Date**: 2025-12-24

## Context

The project requires an async runtime for:
- Concurrent MCP tool handling
- LSP server communication (stdio)
- Child process management
- Timeout and cancellation support

## Decision

Use Tokio as the exclusive async runtime.

```toml
[dependencies]
tokio = { version = "1.48", features = ["full"] }
```

## Consequences

### Positive

- Direct compatibility with rmcp (official MCP SDK uses Tokio)
- Mature ecosystem for process management (`tokio::process`)
- Excellent debugging tools (tokio-console)
- Wide adoption and community support
- LTS releases available (1.38.x until July 2025)

### Negative

- Heavier runtime than async-std or smol
- Not suitable for WASM targets (not a requirement)
- Requires feature flags for minimal builds

## Alternatives Considered

### async-std

Rejected because:
- rmcp is built on Tokio, would require compatibility layer
- Less mature process management
- Smaller ecosystem

### Synchronous design

Rejected because:
- Blocking on LSP responses prevents concurrent tool calls
- Poor user experience for multiple files
- Cannot handle server-initiated notifications

### Runtime-agnostic with async-trait

Rejected because:
- Added complexity for no benefit (rmcp requires Tokio anyway)
- Performance overhead from dynamic dispatch

## References

- [Tokio](https://tokio.rs/)
- [rmcp](https://github.com/modelcontextprotocol/rust-sdk)
- [Tokio LTS Policy](https://tokio.rs/blog/2023-10-announcing-tokio-1-34-lts)
