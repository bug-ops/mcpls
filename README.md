# mcpls

[![Crates.io](https://img.shields.io/crates/v/mcpls)](https://crates.io/crates/mcpls)
[![docs.rs](https://img.shields.io/docsrs/mcpls-core)](https://docs.rs/mcpls-core)
[![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/mcpls/ci.yml?branch=main)](https://github.com/bug-ops/mcpls/actions)
[![codecov](https://codecov.io/gh/bug-ops/mcpls/graph/badge.svg?token=FQEDLNF2GS)](https://codecov.io/gh/bug-ops/mcpls)
[![MSRV](https://img.shields.io/badge/MSRV-1.85-blue)](https://blog.rust-lang.org/2025/02/20/Rust-1.85.0.html)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](LICENSE-MIT)

**Stop treating code as text. Give your AI agent a compiler's understanding.**

mcpls is a universal bridge between AI coding assistants and language servers. It exposes the full power of LSP — type inference, cross-reference analysis, semantic navigation — through the Model Context Protocol, enabling AI agents to reason about code the way IDEs do.

## Why mcpls?

AI coding assistants are remarkably capable, but they're working blind. They see code as text, not as the structured, typed, interconnected system it actually is.

**mcpls changes that.** By bridging MCP and LSP, it gives AI agents access to:

- **Type information** — Know exactly what a variable is, not what it might be
- **Cross-references** — Find every usage of a symbol across your entire codebase
- **Semantic navigation** — Jump to definitions, implementations, type declarations
- **Real diagnostics** — See actual compiler errors, not hallucinated ones
- **Safe refactoring** — Rename symbols with confidence, workspace-wide

> [!TIP]
> Zero configuration for Rust projects. Just install mcpls and rust-analyzer — ready to go.

## Installation

```bash
cargo install mcpls
```

<details>
<summary><strong>Pre-built binaries & other methods</strong></summary>

Download from [GitHub Releases](https://github.com/bug-ops/mcpls/releases/latest):

| Platform | Architecture | Download |
|----------|--------------|----------|
| Linux | x86_64 | [mcpls-linux-x86_64.tar.gz](https://github.com/bug-ops/mcpls/releases/latest) |
| Linux | x86_64 (static) | [mcpls-linux-x86_64-musl.tar.gz](https://github.com/bug-ops/mcpls/releases/latest) |
| macOS | Intel | [mcpls-macos-x86_64.tar.gz](https://github.com/bug-ops/mcpls/releases/latest) |
| macOS | Apple Silicon | [mcpls-macos-aarch64.tar.gz](https://github.com/bug-ops/mcpls/releases/latest) |
| Windows | x86_64 | [mcpls-windows-x86_64.zip](https://github.com/bug-ops/mcpls/releases/latest) |

**From source:**

```bash
git clone https://github.com/bug-ops/mcpls
cd mcpls
cargo install --path crates/mcpls-cli
```

</details>

<details>
<summary><strong>Prerequisites (language servers)</strong></summary>

mcpls uses graceful degradation — if one language server fails, it continues with available servers.

**Rust (rust-analyzer):**
```bash
rustup component add rust-analyzer
# Or: brew install rust-analyzer (macOS)
```

**Python (pyright):**
```bash
npm install -g pyright
```

**TypeScript:**
```bash
npm install -g typescript-language-server typescript
```

**Go (gopls):**
```bash
go install golang.org/x/tools/gopls@latest
```

> [!IMPORTANT]
> At least one language server must be available.

</details>

## Quick Start

**1. Configure Claude Code** (`~/.claude/claude_desktop_config.json`):

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

**2. Experience the difference:**

```
You: What's the return type of process_request on line 47?

Claude: [get_hover] It returns Result<Response, ApiError> where:
        - Response is defined in src/types.rs:23
        - ApiError is an enum with variants: Network, Parse, Timeout

You: Find everywhere ApiError::Timeout is handled

Claude: [get_references] Found 4 matches:
        - src/handlers/api.rs:89 — retry logic
        - src/handlers/api.rs:156 — logging
        - src/middleware/timeout.rs:34 — wrapper
        - tests/api_tests.rs:201 — test case
```

## MCP Tools

<details>
<summary><strong>Code Intelligence</strong></summary>

| Tool | What it does |
|------|--------------|
| `get_hover` | Type signatures, documentation, inferred types at any position |
| `get_definition` | Jump to where a symbol is defined — across files, across crates |
| `get_references` | Every usage of a symbol in your workspace |
| `get_completions` | Context-aware suggestions that respect types and scope |
| `get_document_symbols` | Structured outline — functions, types, constants, imports |
| `workspace_symbol_search` | Find symbols by name across the entire workspace |

</details>

<details>
<summary><strong>Diagnostics & Analysis</strong></summary>

| Tool | What it does |
|------|--------------|
| `get_diagnostics` | Real compiler errors and warnings, not guesses |
| `get_cached_diagnostics` | Fast access to push-based diagnostics from LSP server |
| `get_code_actions` | Quick fixes, refactorings, and source actions at a position |

</details>

<details>
<summary><strong>Refactoring & Call Hierarchy</strong></summary>

| Tool | What it does |
|------|--------------|
| `rename_symbol` | Workspace-wide rename with full reference tracking |
| `format_document` | Apply language-specific formatting rules |
| `prepare_call_hierarchy` | Get callable items at a position for call hierarchy |
| `get_incoming_calls` | Find all callers of a function (who calls this?) |
| `get_outgoing_calls` | Find all callees of a function (what does this call?) |

</details>

<details>
<summary><strong>Server Monitoring</strong></summary>

| Tool | What it does |
|------|--------------|
| `get_server_status` | Show registered LSP servers and their status |
| `get_server_logs` | Debug LSP issues with internal log messages |
| `get_server_messages` | User-facing messages from the language server |

</details>

## Configuration

<details>
<summary><strong>Server Heuristics</strong></summary>

mcpls uses smart heuristics to spawn only relevant language servers. Each server checks for project markers before starting.

| Language | Server | Project Markers |
|----------|--------|-----------------|
| Rust | rust-analyzer | `Cargo.toml`, `rust-toolchain.toml` |
| Python | pyright | `pyproject.toml`, `setup.py`, `requirements.txt` |
| TypeScript | typescript-language-server | `package.json`, `tsconfig.json` |
| Go | gopls | `go.mod`, `go.sum` |
| C/C++ | clangd | `CMakeLists.txt`, `compile_commands.json`, `Makefile` |
| Zig | zls | `build.zig`, `build.zig.zon` |

> [!TIP]
> Heuristics use OR logic — if ANY marker exists, the server spawns.

**Custom heuristics:**

```toml
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"

[lsp_servers.heuristics]
project_markers = ["Cargo.toml", "rust-toolchain.toml", ".rust-version"]
```

</details>

<details>
<summary><strong>Environment Variables</strong></summary>

| Variable | Description | Default |
|----------|-------------|---------|
| `MCPLS_CONFIG` | Path to configuration file | Auto-detected |
| `MCPLS_LOG` | Log level (trace, debug, info, warn, error) | `info` |
| `MCPLS_LOG_JSON` | Output logs as JSON | `false` |

**Config file locations:**

| Platform | Default Location |
|----------|-----------------|
| Linux | `~/.config/mcpls/mcpls.toml` |
| macOS | `~/.config/mcpls/mcpls.toml` or `~/Library/Application Support/mcpls/` |
| Windows | `%APPDATA%\mcpls\mcpls.toml` |

</details>

<details>
<summary><strong>Full Configuration Example</strong></summary>

```toml
[workspace]
roots = ["/path/to/project"]
heuristics_max_depth = 10

[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
timeout_seconds = 30

[lsp_servers.heuristics]
project_markers = ["Cargo.toml", "rust-toolchain.toml"]

[lsp_servers.initialization_options]
cargo.features = "all"
checkOnSave.command = "clippy"

[[language_extensions]]
extensions = ["nu"]
language_id = "nushell"
```

See [Configuration Reference](docs/user-guide/configuration.md) for all options.

</details>

## Supported Language Servers

mcpls works with any LSP 3.17 compliant server. Battle-tested with:

<details>
<summary><strong>View supported servers</strong></summary>

| Language | Server | Notes |
|----------|--------|-------|
| Rust | rust-analyzer | Zero-config, built-in support |
| Python | pyright | Full type inference |
| TypeScript/JS | typescript-language-server | JSX/TSX support |
| Go | gopls | Modules and workspaces |
| C/C++ | clangd | compile_commands.json |
| Java | jdtls | Maven/Gradle projects |
| Zig | zls | build.zig support |
| And 24+ others | Any LSP 3.17 server | See [docs](docs/user-guide/configuration.md) |

</details>

## Architecture

<details>
<summary><strong>View architecture diagram</strong></summary>

```mermaid
flowchart TB
    subgraph AI["AI Agent (Claude)"]
    end

    subgraph mcpls["mcpls Server"]
        MCP["MCP Server<br/>(rmcp)"]
        Trans["Translation Layer"]
        LSP["LSP Clients<br/>Manager"]
        MCP --> Trans --> LSP
    end

    subgraph Servers["Language Servers"]
        RA["rust-analyzer"]
        PY["pyright"]
        TS["tsserver"]
        Other["..."]
    end

    AI <-->|"MCP Protocol<br/>(JSON-RPC 2.0)"| mcpls
    mcpls <-->|"LSP Protocol<br/>(JSON-RPC 2.0)"| Servers
```

**Key design decisions:**
- **Single binary** — No Node.js, Python, or other runtime dependencies
- **Async-first** — Tokio-based, handles multiple LSP servers concurrently
- **Memory-safe** — Pure Rust, zero `unsafe` blocks
- **Resource-bounded** — Configurable limits on documents and file sizes

</details>

## Documentation

- [Getting Started](docs/user-guide/getting-started.md)
- [Configuration Reference](docs/user-guide/configuration.md)
- [Tools Reference](docs/user-guide/tools-reference.md)
- [Troubleshooting](docs/user-guide/troubleshooting.md)

## Development

```bash
cargo build              # Build
cargo nextest run        # Test
cargo run -- --log-level debug  # Run locally
```

**Requirements:** Rust 1.85+ (Edition 2024)

## Contributing

Contributions welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

Dual-licensed under [Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.

---

**mcpls** — Because AI deserves to understand code, not just read it.
