# ADR-001: Workspace Structure

**Status**: ACCEPTED

**Date**: 2025-12-24

## Context

The mcpls project needs to provide both a reusable library for protocol translation and a CLI application for end users. We need to decide on the crate organization.

## Decision

Use a single Cargo workspace with two crates:

- `mcpls-core`: Library crate containing all protocol translation logic
- `mcpls` (mcpls-cli): Binary crate providing the CLI interface

```
mcpls/
├── Cargo.toml          # Workspace manifest
├── crates/
│   ├── mcpls-core/     # Library crate
│   └── mcpls-cli/      # Binary crate
```

## Consequences

### Positive

- Clean separation between library and application code
- Library can be embedded in other Rust projects (IDE plugins, custom MCP servers)
- Faster incremental compilation (changes to CLI don't rebuild core)
- Independent versioning possible in the future
- Follows Microsoft Rust Guidelines (M-SMALLER-CRATES)

### Negative

- Slightly more boilerplate (two Cargo.toml files)
- Need to maintain consistent versions across crates

## Alternatives Considered

### Single crate with lib + bin

Rejected because:
- Harder to use as a library dependency
- No clear separation of concerns
- CLI dependencies pulled into library consumers

### Three crates (core + lsp + mcp)

Rejected because:
- Over-engineering for initial scope (~3,000-5,000 lines)
- Can be refactored later if needed
- Adds maintenance overhead

## References

- [Microsoft Rust Guidelines - Smaller Crates](https://microsoft.github.io/rust-guidelines/)
- [Cargo Workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html)
