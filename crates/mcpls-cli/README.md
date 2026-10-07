# mcpls

[![Crates.io](https://img.shields.io/crates/v/mcpls)](https://crates.io/crates/mcpls)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](https://github.com/bug-ops/mcpls/blob/main/LICENSE-MIT)

**Give your AI agent a compiler's eye.**

The mcpls command-line tool bridges the Model Context Protocol (MCP) and the Language Server Protocol (LSP). It runs the language servers for your project and exposes their type information, references, diagnostics and refactoring to an AI client as 31 MCP tools. One binary, any language, no runtime dependencies.

Documentation: <https://bug-ops.github.io/mcpls/>

## Installation

```bash
cargo install mcpls
```

Prebuilt binaries and installer scripts are described in the [installation chapter](https://bug-ops.github.io/mcpls/getting-started/installation.html). You also need at least one language server, such as rust-analyzer.

## Usage

Register mcpls with your client, which launches it over stdio. For Claude Code:

```bash
claude mcp add --scope user mcpls -- mcpls
```

Other invocations:

```bash
mcpls --config ./mcpls.toml     # use a specific configuration file
mcpls --log-level debug         # verbose logs on standard error
mcpls --help                    # every option
```

Rust projects work with no configuration. For other languages, see [Minimal Configuration](https://bug-ops.github.io/mcpls/getting-started/minimal-config.html).

## Features

- 31 tools: hover, definition, references, diagnostics, rename, code actions, call and type hierarchies, selection and folding ranges, and more
- Symbol-name addressing, `get_tool_support` discovery and `restart_server`
- Several language servers at once, started concurrently and stopped with mcpls, with graceful degradation
- stdio transport by default; optional HTTP transport behind the `transport-http` feature
- Untrusted-workspace mode (`--workspace-trust untrusted`) for code you do not trust

## Learn more

- [Command line and environment reference](https://bug-ops.github.io/mcpls/reference/cli.html)
- [Configuration reference](https://bug-ops.github.io/mcpls/reference/config.html)
- [Tools overview](https://bug-ops.github.io/mcpls/tools/overview.html)
- [Security and trust](https://bug-ops.github.io/mcpls/advanced/security.html)
- [Troubleshooting](https://bug-ops.github.io/mcpls/guide/troubleshooting.html)

## License

Dual-licensed under [Apache 2.0](https://github.com/bug-ops/mcpls/blob/main/LICENSE-APACHE) or [MIT](https://github.com/bug-ops/mcpls/blob/main/LICENSE-MIT).
