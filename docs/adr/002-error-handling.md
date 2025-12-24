# ADR-002: Error Handling Strategy

**Status**: ACCEPTED

**Date**: 2025-12-24

## Context

The project needs a consistent error handling strategy that:
- Provides typed, matchable errors for library consumers
- Offers rich context for CLI debugging
- Follows Rust ecosystem conventions

## Decision

Use a split strategy:

- **mcpls-core**: Use `thiserror` for typed, canonical errors
- **mcpls-cli**: Use `anyhow` for context-rich application errors

```rust
// mcpls-core/src/error.rs
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("LSP server initialization failed: {0}")]
    LspInitFailed(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

// mcpls-cli/src/main.rs
use anyhow::{Context, Result};

async fn run() -> Result<()> {
    mcpls_core::serve(config)
        .await
        .context("failed to start MCPLS server")?;
    Ok(())
}
```

## Consequences

### Positive

- Library errors are matchable and well-documented
- CLI gets rich error context chains for debugging
- Follows Microsoft Rust Guidelines (M-ERRORS-CANONICAL-STRUCTS)
- Clear API contract for library consumers

### Negative

- Two error handling styles in the same workspace
- Need to ensure proper error conversion at boundaries

## Alternatives Considered

### anyhow everywhere

Rejected because:
- Violates library guidelines (M-ERRORS-CANONICAL-STRUCTS)
- Library consumers can't match on error types
- Loses type information at API boundary

### thiserror everywhere

Rejected because:
- CLI doesn't need typed errors
- More boilerplate for application-level error handling
- Context chaining is more verbose

## References

- [Microsoft Rust Guidelines - Errors](https://microsoft.github.io/rust-guidelines/)
- [thiserror](https://docs.rs/thiserror)
- [anyhow](https://docs.rs/anyhow)
