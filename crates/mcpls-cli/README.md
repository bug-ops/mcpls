# mcpls

[![Crates.io](https://img.shields.io/crates/v/mcpls)](https://crates.io/crates/mcpls)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](../../LICENSE-MIT)

**Give your AI agent a compiler's eye.**

The mcpls CLI exposes language server intelligence through MCP. One binary, any language, zero runtime dependencies.

> [!TIP]
> Graceful degradation means you don't need every language server installed. If one fails, mcpls continues with available servers.

## Installation

```bash
cargo install mcpls
```

## Usage

```bash
mcpls                                      # stdio transport (default)
mcpls --log-level debug                    # verbose output
mcpls --config ./mcpls.toml               # custom config
mcpls --listen 127.0.0.1:3000             # HTTP transport (transport-http feature)
```

## Configuration

> [!NOTE]
> Configuration auto-discovery order: `$MCPLS_CONFIG` → `./mcpls.toml` → platform config dir
> Auto-creates default config with 30 language mappings on first run.

Create or edit `mcpls.toml` in the appropriate location:
- **Linux/macOS:** `~/.config/mcpls/mcpls.toml`
- **macOS (alternative):** `~/Library/Application Support/mcpls/mcpls.toml`
- **Windows:** `%APPDATA%\mcpls\mcpls.toml`

See the main [README](../../README.md) for configuration examples and custom extension mapping.

## Options

| Flag | Env | Description |
|------|-----|-------------|
| `-c, --config <PATH>` | `MCPLS_CONFIG` | Configuration file path |
| `-l, --log-level <LEVEL>` | `MCPLS_LOG` | trace, debug, info, warn, error (default: info) |
| `--log-json` | `MCPLS_LOG_JSON` | JSON-formatted logs for tooling |
| `--listen <ADDR>` | `MCPLS_LISTEN` | Bind address for HTTP transport (`transport-http` feature) |
| `--http-path <PATH>` | `MCPLS_HTTP_PATH` | URL prefix for HTTP transport (default: `/mcp`) |

## Claude Code Integration

Add to your Claude Code configuration (`~/.claude/claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "mcpls": { "command": "mcpls", "args": [] }
  }
}
```

See the main [README](../../README.md) for full documentation.

## License

Dual-licensed under [Apache 2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT).
