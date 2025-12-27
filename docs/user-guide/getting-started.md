# Getting Started with mcpls

This guide will help you get up and running with mcpls in 5 minutes.

## What is mcpls?

mcpls is a universal bridge that exposes Language Server Protocol (LSP) capabilities as Model Context Protocol (MCP) tools, enabling AI assistants like Claude Code to access semantic code intelligence.

Instead of treating code as plain text, AI agents can now:
- Get type information and documentation
- Navigate to symbol definitions
- Find all references across the codebase
- Access compiler diagnostics
- Perform workspace-wide refactoring
- Get code completion suggestions

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

### Verify installation

```bash
mcpls --version
# Should output: mcpls 0.1.0
```

## Quick Start with Claude Code

### 1. Configure Claude Code

Add mcpls to your Claude Code MCP configuration file:

**macOS/Linux**: `~/.claude/mcp.json`
**Windows**: `%APPDATA%\Claude\mcp.json`

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

### 2. Restart Claude Code

After adding the configuration, restart Claude Code to load mcpls.

### 3. Verify Tools

Ask Claude: "What tools are available?"

You should see 8 mcpls tools:
- get_hover
- get_definition
- get_references
- get_diagnostics
- rename_symbol
- get_completions
- get_document_symbols
- format_document

### 4. Try It Out

Open a Rust project and ask Claude:

> "What type is this variable on line 42?"

Claude will use the `get_hover` tool to retrieve type information from rust-analyzer.

## Configuration (Optional)

mcpls works zero-config for Rust projects (uses rust-analyzer by default). For other languages, create a configuration file.

### Configuration File Location

mcpls searches for configuration in:
1. Path specified by `--config` flag
2. `$MCPLS_CONFIG` environment variable
3. `./mcpls.toml` (current directory)
4. `~/.config/mcpls/mcpls.toml`

### Example Configuration

Create `~/.config/mcpls/mcpls.toml`:

```toml
[workspace]
roots = []  # Auto-detect from current directory

# Rust - rust-analyzer (built-in)
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]

# Python - pyright
[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]

# TypeScript - typescript-language-server
[[lsp_servers]]
language_id = "typescript"
command = "typescript-language-server"
args = ["--stdio"]
file_patterns = ["**/*.ts", "**/*.tsx"]
```

## Example Usage

### Get Type Information

```
User: What's the type of the user variable on line 15 in src/main.rs?
Claude: [Uses get_hover] The variable `user` has type `User`, which is a struct
        defined in this module with fields: id (u64), name (String), email (String).
```

### Find References

```
User: Where is the calculate_total function used?
Claude: [Uses get_references] The function `calculate_total` is referenced in 5 locations:
        1. src/billing.rs:42
        2. src/invoice.rs:18
        3. src/report.rs:67
        4. tests/billing_tests.rs:25
        5. tests/integration_tests.rs:103
```

### Get Diagnostics

```
User: Are there any errors in this file?
Claude: [Uses get_diagnostics] Found 2 errors:
        1. Line 23: cannot find value `undefined_variable` in this scope
        2. Line 45: mismatched types: expected `i32`, found `String`
```

### Rename Symbol

```
User: Rename the process_data function to handle_data everywhere
Claude: [Uses rename_symbol] Successfully prepared rename across 12 files
        with 28 edits. Would you like me to apply these changes?
```

## Installing Language Servers

mcpls requires language servers to be installed separately:

### Rust (rust-analyzer)
```bash
rustup component add rust-analyzer
```

### Python (pyright)
```bash
npm install -g pyright
```

### TypeScript (typescript-language-server)
```bash
npm install -g typescript-language-server
```

### Go (gopls)
```bash
go install golang.org/x/tools/gopls@latest
```

### C/C++ (clangd)
```bash
# Ubuntu/Debian
sudo apt install clangd

# macOS
brew install llvm
```

## Next Steps

- [Configuration Guide](configuration.md) - Detailed configuration options
- [Tools Reference](tools-reference.md) - Documentation for each MCP tool
- [Troubleshooting](troubleshooting.md) - Common issues and solutions

## Common Questions

### Do I need to configure mcpls for Rust?

No! mcpls has built-in support for rust-analyzer and works zero-config for Rust projects.

### Can I use multiple language servers?

Yes! Configure as many language servers as needed in `mcpls.toml`. mcpls will route requests to the appropriate server based on file patterns.

### Does mcpls modify my code?

Only when you explicitly ask for changes (like rename_symbol or format_document). All other tools are read-only and provide information without modifying files.

### Can I use mcpls with other MCP clients?

Yes! mcpls implements the standard MCP protocol and works with any MCP-compliant client, not just Claude Code.
