# Configuration

In this chapter you build a configuration file step by step: first the workspace, then language servers, then the settings you tune on large projects. Each step is optional; add a section only when you need it. Every key is listed in the [Configuration Reference](../reference/config.md).

## Prerequisites

- You know where the file lives ([Minimal Configuration](../getting-started/minimal-config.md)).

## The shape of the file

A configuration has three top-level parts, all optional:

```text
[mcp]
# how mcpls presents itself to the client

[workspace]
# which directories to analyze, and resource limits

[[lsp_servers]]
# one entry per language server (repeat the table)
```

Unknown keys are rejected at startup with an error that names the key, so a typo never fails silently.

## Choose the workspace

`workspace.roots` lists the directories the language servers analyze. An empty list (the default) means the directory mcpls was launched from.

```toml
[workspace]
roots = ["/Users/you/projects/frontend", "/Users/you/projects/backend"]
```

Roots must exist. A relative root resolves against the directory that holds the config file when you named it explicitly (`--config`, `MCPLS_CONFIG`, or a trusted project-local file), and against the launch directory when it comes from the user config directory. Keep roots narrow: never list your home directory.

Tools accept only files under a root. A path outside every root is rejected.

## Define a language server

An `[[lsp_servers]]` entry says which program serves which files:

```toml
[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py", "**/*.pyi"]
```

- `command` is looked up on `PATH`; an absolute path also works.
- `args` carries flags. Many servers need `--stdio`.
- `file_patterns` maps files to this server. mcpls routes by extension (or by bare name for files such as `Makefile`), so only the final `*.EXT` part matters and the directory prefix is ignored. Write one pattern per extension; brace expansion such as `**/*.{ts,tsx}` is rejected.

### Start a server only where it applies

Add project markers so the server starts only in workspaces that contain one of them:

```toml
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
file_patterns = ["**/*.rs"]

[lsp_servers.heuristics]
project_markers = ["Cargo.toml", "rust-toolchain.toml"]
```

A server starts when any marker exists in the workspace tree. With no `heuristics` table the server always starts.

## Pass options to the server

`initialization_options` are sent once, during the LSP handshake. `settings` are pushed after it and answer the server's own configuration requests. Both use the server's own option names:

```toml
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
file_patterns = ["**/*.rs"]

[lsp_servers.initialization_options]
cargo.features = "all"
checkOnSave.command = "clippy"
```

Top-level dotted keys expand into nested objects, so `cargo.features = "all"` becomes `{"cargo": {"features": "all"}}`. Check your server's documentation for the available options.

## Give the server environment variables

A language server does not inherit your full environment. mcpls passes only `PATH`, `HOME`, `USERPROFILE`, the temporary-directory variables and the Windows system variables. Restore anything else the server needs with `env`:

```toml
[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]

[lsp_servers.env]
VIRTUAL_ENV = "/path/to/venv"
```

Setting `PATH` here replaces the inherited value instead of extending it. To add one directory, give `command` as an absolute path instead.

## Tune timeouts and limits for large projects

When a server is slow to start or a project is large, raise the timeouts:

```toml
[workspace]
indexing_ready_timeout_seconds = 45
max_documents = 500

[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
file_patterns = ["**/*.rs"]
timeout_seconds = 120
request_timeout_seconds = 60
```

| Key | Controls | Default |
|-----|----------|---------|
| `timeout_seconds` | The `initialize` handshake at startup (1 to 900) | 30 |
| `request_timeout_seconds` | Each LSP request behind a tool call (1 to 900) | 30 |
| `workspace.indexing_ready_timeout_seconds` | How long whole-workspace queries wait for the server to finish indexing (above 3, below 60) | 30 |
| `workspace.max_documents` | Files held open at once; the least recently used is closed first (`0` = unlimited) | 100 |
| `workspace.max_file_size` | Largest file mcpls opens, in bytes (`0` = unlimited, at most 1 GiB) | 10 MiB |
| `workspace.max_concurrent_server_starts` | Servers starting at the same time | 8 |

## Use more than one server for a language

Two servers can share a language when each has a distinct `name` and at most one of them handles everything. `handles` restricts a server to the listed routing values:

```toml
[[lsp_servers]]
name = "pyright"
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]

[[lsp_servers]]
name = "pylsp"
language_id = "python"
command = "pylsp"
file_patterns = ["**/*.py"]
handles = ["diagnostics"]
```

Here `pylsp` answers `get_diagnostics` and `get_cached_diagnostics`, and `pyright` (the catch-all, with no `handles`) answers everything else. Conflicting claims are rejected at startup with an error naming the entries. The routing values are listed in [`handles`](../reference/config.md#handles).

## Customize how mcpls introduces itself

The `[mcp]` section changes the text and tool names clients see. Use `tool_prefix` when you run several mcpls instances in one client:

```toml
[mcp]
title = "Billing service bridge"
instructions = "Prefer get_hover before get_definition."
tool_prefix = "billing"
```

With this prefix the tools are named `billing_get_hover`, `billing_get_references`, and so on. A configured `instructions` replaces the built-in guidance entirely.

## What's Next

Next, learn how to [install and configure the language servers](language-servers.md) for each language.
