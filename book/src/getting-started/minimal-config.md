# Minimal Configuration

In this chapter you learn what mcpls does with no configuration, and the smallest file that adds a language or a project root. Most users need only a few lines.

## Prerequisites

- A [connected client](connect-client.md) and an installed language server.

## Zero configuration

On first run mcpls writes a default configuration file to your user config directory and starts the language servers whose project markers it finds in the workspace:

| Language | Server | Starts when the workspace contains |
|----------|--------|------------------------------------|
| Rust | rust-analyzer | `Cargo.toml`, `rust-toolchain.toml` |
| Python | pyright | `pyproject.toml`, `setup.py`, `requirements.txt`, `pyrightconfig.json` |
| TypeScript | typescript-language-server | `package.json`, `tsconfig.json`, `jsconfig.json` |
| Go | gopls | `go.mod`, `go.sum` |
| C/C++ | clangd | `CMakeLists.txt`, `compile_commands.json`, `Makefile`, `.clangd` |
| Zig | zls | `build.zig`, `build.zig.zon` |

A Rust project therefore works as soon as `rust-analyzer` is installed. A server whose markers are absent is not started, so unused servers cost nothing.

The workspace is the directory the client launched mcpls from.

## Where the configuration file lives

mcpls looks for `mcpls.toml` in this order and uses the first that applies:

1. The path given with `--config` (or the `MCPLS_CONFIG` environment variable).
2. `./mcpls.toml` in the current directory, only when you pass `--trust-project-config`.
3. The user config directory:

| Platform | Location |
|----------|----------|
| Linux | `$XDG_CONFIG_HOME/mcpls/mcpls.toml`, else `~/.config/mcpls/mcpls.toml` |
| macOS | `~/Library/Application Support/mcpls/mcpls.toml` |
| Windows | `%APPDATA%\mcpls\mcpls.toml` |

A `mcpls.toml` inside a repository is ignored by default because it can name a program to run. See [Security and Trust](../advanced/security.md).

## Add a language

A configuration file replaces the built-in server list, so list every server you want. A file with no `[[lsp_servers]]` entries starts no language servers at all. This file runs rust-analyzer and pyright:

```toml
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
file_patterns = ["**/*.rs"]

[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]
```

Each `[[lsp_servers]]` entry needs a `language_id`, a `command` that is on `PATH` (or an absolute path), and `file_patterns` that say which files it serves. Use one pattern per extension, such as `**/*.ts`.

Save the file in the user config directory, or anywhere and pass it explicitly:

```bash
mcpls --config /path/to/mcpls.toml
```

To make the client do that, put the flag in its `args`:

```json
{
  "mcpServers": {
    "mcpls": {
      "command": "mcpls",
      "args": ["--config", "/path/to/mcpls.toml"]
    }
  }
}
```

## Pin the project root

By default the workspace is the launch directory. To set it yourself, add a `[workspace]` section with absolute paths that exist:

```toml
[workspace]
roots = ["/Users/you/projects/myapp"]
```

See the [Example Configuration](../reference/example-config.md) for a complete annotated file.

## What's Next

You now have a working setup. Part 2 starts with [Configuration](../guide/configuration.md), which covers every section you will touch in daily use.
