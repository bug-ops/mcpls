# mcpls

[![CI](https://github.com/bug-ops/mcpls/actions/workflows/ci.yml/badge.svg)](https://github.com/bug-ops/mcpls/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/mcpls.svg)](https://crates.io/crates/mcpls)
[![Documentation](https://docs.rs/mcpls-core/badge.svg)](https://docs.rs/mcpls-core)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE-MIT)

Universal MCP to LSP bridge - expose Language Server Protocol capabilities as MCP tools for AI agents.

## Overview

**mcpls** bridges the gap between AI coding assistants and language servers, enabling semantic code intelligence through the Model Context Protocol (MCP). Instead of treating code as plain text, AI agents can now access:

- Type information and documentation via hover
- Go-to-definition for symbol navigation
- Find all references across the codebase
- Compiler diagnostics and errors
- Code completion suggestions
- Symbol renaming with workspace-wide changes
- Document formatting

## Features

- **Single binary** - No runtime dependencies (Node.js, Python, etc.)
- **Multi-language support** - Works with any LSP-compliant language server
- **Zero-config for Rust** - Built-in rust-analyzer support
- **TOML configuration** - Familiar format for Rust developers
- **Memory safe** - Built in Rust for reliability in long-running processes

## Installation

### From crates.io

```bash
cargo install mcpls
```

### From source

```bash
git clone https://github.com/bug-ops/mcpls
cd mcpls
cargo install --path crates/mcpls-cli
```

## Quick Start

### 1. Configure Claude Code

Add mcpls to your Claude Code MCP configuration:

```json
{
  "mcpServers": {
    "mcpls": {
      "command": "mcpls",
      "args": []
    }
  }
}
```

### 2. Create configuration (optional)

For custom LSP servers, create `~/.config/mcpls/mcpls.toml`:

```toml
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]

[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]

[[lsp_servers]]
language_id = "typescript"
command = "typescript-language-server"
args = ["--stdio"]
file_patterns = ["**/*.ts", "**/*.tsx"]
```

### 3. Use with Claude

Claude can now use semantic code intelligence:

```
User: What type is the variable on line 42?
Claude: [Uses get_hover tool] This is a `Vec<String>` - a growable array of strings.

User: Find all usages of the `process` function
Claude: [Uses get_references tool] Found 5 references across 3 files...
```

## Available MCP Tools

| Tool | Description |
|------|-------------|
| `get_hover` | Get type information and documentation at a position |
| `get_definition` | Jump to symbol definition |
| `get_references` | Find all references to a symbol |
| `get_diagnostics` | Get compiler errors and warnings |
| `get_completions` | Get code completion suggestions |
| `rename_symbol` | Rename a symbol across the workspace |
| `get_document_symbols` | List all symbols in a document |
| `format_document` | Format a document |

## Configuration Reference

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `MCPLS_CONFIG` | Path to configuration file | Auto-detected |
| `MCPLS_LOG` | Log level (trace, debug, info, warn, error) | `info` |
| `MCPLS_LOG_JSON` | Output logs as JSON | `false` |

### Configuration File

```toml
# Workspace settings
[workspace]
roots = ["/path/to/project"]
position_encodings = ["utf-8", "utf-16"]

# LSP server definitions
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
timeout_seconds = 30

[lsp_servers.initialization_options]
# Server-specific initialization options
cargo.features = "all"
```

## Supported Language Servers

mcpls works with any LSP-compliant language server. Tested with:

- **Rust**: rust-analyzer
- **Python**: pyright, pylsp
- **TypeScript/JavaScript**: typescript-language-server
- **Go**: gopls
- **C/C++**: clangd

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    AI Agent (Claude)                     │
└─────────────────────────┬───────────────────────────────┘
                          │ MCP Protocol (JSON-RPC 2.0)
┌─────────────────────────▼───────────────────────────────┐
│                     mcpls Server                         │
│  ┌────────────┐  ┌─────────────────┐  ┌──────────────┐  │
│  │ MCP Server │→ │Translation Layer│→ │ LSP Clients  │  │
│  │   (rmcp)   │  │                 │  │   Manager    │  │
│  └────────────┘  └─────────────────┘  └──────────────┘  │
└─────────────────────────────────────────────────────────┘
                          │ LSP Protocol (JSON-RPC 2.0)
┌─────────────────────────▼───────────────────────────────┐
│         rust-analyzer, pyright, tsserver, ...            │
└─────────────────────────────────────────────────────────┘
```

## Development

### Prerequisites

- Rust 1.85+ (Edition 2024)
- cargo

### Building

```bash
cargo build
```

### Testing

```bash
cargo test
```

### Running locally

```bash
cargo run -- --log-level debug
```

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

## Acknowledgments

- [Model Context Protocol](https://modelcontextprotocol.io/) by Anthropic
- [Language Server Protocol](https://microsoft.github.io/language-server-protocol/) by Microsoft
- [rmcp](https://github.com/modelcontextprotocol/rust-sdk) - Official MCP Rust SDK
- [lsp-types](https://github.com/gluon-lang/lsp-types) - LSP type definitions for Rust
