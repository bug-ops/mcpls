# Contributing to mcpls

Thank you for your interest in contributing to mcpls! This document provides guidelines and information for contributors.

## Code of Conduct

This project follows the [Rust Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct). Please be respectful and constructive in all interactions.

## Getting Started

### Prerequisites

- Rust 1.85+ (Edition 2024)
- A language server for testing (e.g., rust-analyzer)

### Setting Up Development Environment

1. Fork and clone the repository:
   ```bash
   git clone https://github.com/YOUR_USERNAME/mcpls
   cd mcpls
   ```

2. Build the project:
   ```bash
   cargo build
   ```

3. Run tests:
   ```bash
   cargo test
   ```

4. Run clippy:
   ```bash
   cargo clippy --all-targets --all-features
   ```

## Development Workflow

### Branch Naming

- `feature/description` - New features
- `fix/description` - Bug fixes
- `docs/description` - Documentation changes
- `refactor/description` - Code refactoring

### Commit Messages

Follow conventional commit format:

```
type(scope): description

[optional body]

[optional footer]
```

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `chore`

Examples:
```
feat(mcp): add get_workspace_symbols tool
fix(lsp): handle server crash gracefully
docs(readme): add pyright configuration example
```

### Pull Request Process

1. Create a feature branch from `main`
2. Make your changes with clear, focused commits
3. Ensure all tests pass: `cargo test`
4. Ensure clippy is happy: `cargo clippy --all-targets`
5. Ensure code is formatted: `cargo fmt --check`
6. Update documentation if needed
7. Submit a pull request

### PR Checklist

- [ ] Tests added/updated for new functionality
- [ ] Documentation updated
- [ ] CHANGELOG.md updated (for user-facing changes)
- [ ] No clippy warnings
- [ ] Code formatted with `cargo fmt`

## Project Structure

```
mcpls/
├── crates/
│   ├── mcpls-core/     # Core library (protocol translation)
│   │   ├── src/
│   │   │   ├── lsp/    # LSP client implementation
│   │   │   ├── mcp/    # MCP tool definitions
│   │   │   ├── bridge/ # Translation layer
│   │   │   └── config/ # Configuration types
│   │   └── tests/
│   └── mcpls-cli/      # CLI application
├── docs/
│   └── adr/            # Architecture Decision Records
├── examples/
└── tests/fixtures/
```

## Coding Guidelines

### Rust Style

- Follow [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Follow [Microsoft Rust Guidelines](https://microsoft.github.io/rust-guidelines/)
- Use `rustfmt` for formatting
- Address all clippy warnings

### Error Handling

- Use `thiserror` for library errors in `mcpls-core`
- Use `anyhow` for application errors in `mcpls-cli`
- Provide context with errors using `.context()` or custom messages

### Documentation

- Document all public APIs
- Include examples in doc comments where helpful
- Keep documentation up-to-date with code changes

### Testing

- Write unit tests for new functionality
- Write integration tests for LSP communication
- Use `rstest` for parameterized tests
- Aim for meaningful coverage, not just high percentages

## Architecture Decisions

Major architectural decisions are documented in `docs/adr/`. When proposing significant changes:

1. Create a new ADR document
2. Describe the context, decision, and consequences
3. Reference the ADR in your PR

## Getting Help

- Open an issue for bugs or feature requests
- Start a discussion for questions or ideas
- Check existing issues before creating new ones

## License

By contributing to mcpls, you agree that your contributions will be licensed under the MIT OR Apache-2.0 license.
