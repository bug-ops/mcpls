# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Initial project structure with workspace layout
- Core library (`mcpls-core`) with protocol translation foundation
- CLI application (`mcpls`) with argument parsing and logging
- LSP client module structure
- MCP tool definitions for 8 core tools:
  - `get_hover` - Get type information and documentation
  - `get_definition` - Jump to symbol definition
  - `get_references` - Find all references
  - `get_diagnostics` - Get compiler diagnostics
  - `get_completions` - Get code completions
  - `rename_symbol` - Rename symbols
  - `get_document_symbols` - List document symbols
  - `format_document` - Format document
- Document state tracking for LSP synchronization
- Position encoding conversion (MCP 1-based to LSP 0-based)
- TOML configuration support
- Built-in rust-analyzer default configuration
- Dual MIT/Apache-2.0 licensing

### Changed

- N/A

### Deprecated

- N/A

### Removed

- N/A

### Fixed

- N/A

### Security

- N/A

[Unreleased]: https://github.com/bug-ops/mcpls/compare/v0.1.0...HEAD
