# ADR-006: Configuration Format

**Status**: ACCEPTED

**Date**: 2025-12-24

## Context

The project needs a configuration format for:
- LSP server definitions (command, args, patterns)
- Workspace settings (roots, encoding preferences)
- Server-specific initialization options

The format should be familiar to Rust developers.

## Decision

Use **TOML** for configuration files.

```toml
# mcpls.toml

[workspace]
roots = ["/home/user/projects/myproject"]
position_encodings = ["utf-8", "utf-16"]

[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]

[lsp_servers.initialization_options]
cargo.features = "all"

[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]
```

### Configuration Discovery

1. `$MCPLS_CONFIG` environment variable
2. `./mcpls.toml` (current directory)
3. `~/.config/mcpls/mcpls.toml` (Linux/macOS)
4. `%APPDATA%\mcpls\mcpls.toml` (Windows)
5. Built-in defaults (rust-analyzer only)

## Consequences

### Positive

- Familiar to Rust developers (Cargo.toml)
- Supports comments for documentation
- Human-readable and writable
- Strong ecosystem support (toml crate)
- Hierarchical structure for nested options

### Negative

- Less structured than JSON Schema validation
- No IDE completion without language server
- Arrays of tables syntax (`[[section]]`) can be confusing

## Alternatives Considered

### JSON

Rejected because:
- No comments support
- Less readable for humans
- Verbose for nested structures

### YAML

Rejected because:
- Not Rust ecosystem convention
- Complex parsing (indentation-sensitive)
- Security concerns with some parsers

### Environment variables only

Rejected because:
- Hard to represent complex configuration
- No support for multiple LSP servers
- Poor discoverability

## References

- [TOML Specification](https://toml.io/)
- [toml crate](https://docs.rs/toml)
- [Cargo.toml reference](https://doc.rust-lang.org/cargo/reference/manifest.html)
