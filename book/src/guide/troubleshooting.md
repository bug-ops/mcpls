# Troubleshooting

In this chapter you find the fix for the problems people hit most, ordered the way they appear: install, client connection, language servers, configuration, and unexpected results. Start by turning on logs, which answer most questions on their own.

## Prerequisites

- mcpls is installed ([Installation](../getting-started/installation.md)).

## Read the logs first

mcpls writes logs to standard error, so they never mix with the MCP stream on standard output. Raise the level with `--log-level` or `MCPLS_LOG`:

```bash
mcpls --log-level debug 2> mcpls-debug.log
```

`MCPLS_LOG` accepts `trace`, `debug`, `info`, `warn`, `error`, `off`, or comma-separated `target=level` directives such as `info,mcpls_core=debug`. An unknown level is rejected at startup. Add `--log-json` for JSON lines.

From inside the assistant, the `get_server_logs` tool shows what the language servers logged.

## Installation

### Command not found: mcpls

The install directory is not on `PATH`. The installer scripts use `~/.local/bin`; `cargo install` uses `~/.cargo/bin`.

```bash
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
```

Add the line to `~/.zshrc` or `~/.bashrc` to keep it. On Windows, add the directory containing `mcpls.exe` to your user `Path` in Environment Variables and open a new terminal.

### The binary is blocked on macOS

A binary downloaded by hand can carry a quarantine attribute. Remove it:

```bash
xattr -d com.apple.quarantine /path/to/mcpls
```

### Failed to compile

Building from source needs Rust 1.99 or later:

```bash
rustup update stable
```

## Client connection

### mcpls does not appear in the client

1. Run `mcpls --version` in a terminal.
2. Check the client's configuration file for a syntax error. JSON does not allow comments or trailing commas.
3. Restart the client completely.
4. If the client cannot find the binary, use its absolute path as `command`; `which mcpls` prints it.

For Claude Code, `claude mcp list` shows what is registered.

### Test the server by hand

Send an MCP `initialize` request on standard input:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}' | mcpls
```

A JSON response containing `serverInfo` means the server starts. Use the client's own diagnostics for anything beyond that.

## Language servers

### LSP server not available for file type

No `[[lsp_servers]]` entry maps the file's extension. The error names the extension and the `file_patterns` configured across servers. Add an entry:

```toml
[[lsp_servers]]
language_id = "go"
command = "gopls"
file_patterns = ["**/*.go"]
```

Or the server did not start because its project markers are missing from the workspace; check the startup log for the servers that were registered.

### Server failed to start

The error names the command and the reason. Check that the server runs by itself (`gopls version`), that `command` is on `PATH` or absolute, and that the `args` are right (many servers need `--stdio`). Remember that a language server sees a reduced environment; set what it needs in the entry's `env`.

### Server still initializing

`ServerInitializing` (`-32051`) is retryable. Large projects can take minutes to start. Raise `timeout_seconds` if the server is cut off during the handshake.

### Queries are slow or time out

The first query on a large project waits for indexing. Raise the limits for that server:

```toml
[workspace]
indexing_ready_timeout_seconds = 45

[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
file_patterns = ["**/*.rs"]
timeout_seconds = 120
request_timeout_seconds = 60
```

A single request is retried when the server answers "content modified" (`-32802`): up to 4 attempts with a 3.5 second total backoff, so the worst case for one call is `4 * request_timeout_seconds + 3.5` seconds. Keep `workspace.roots` narrow so the server does not index unrelated directories.

### Queries wait after restart_server

After a restart or an automatic respawn, mcpls treats a server that reported readiness before as loading again until its replacement reports ready, or until `indexing_ready_timeout_seconds` passes. This prevents answers from an empty index. Retry after the `-32050` or `-32051` error, lower the timeout, or exempt the server from gating:

```toml
[[lsp_servers]]
language_id = "go"
command = "gopls"
file_patterns = ["**/*.go"]
indexing = "disabled"
```

### A server crashed

mcpls respawns a crashed server on the next call that needs it. Push-only diagnostics are lost until you restart mcpls, and results say so with `push_notifications_degraded: true`. Call `restart_server` for a server that is wedged.

### Results look stale after a file changed

mcpls detects changes made on disk (a checkout, a formatter, another editor) and resynchronizes the server on the next call. Two cases are not detected: a file restored with the same size and the same modification time (`tar x`, `rsync -a`, `cp -p`), and files mcpls has never opened, which `workspace_symbol_search` reads from the server's own index. Run `touch` on the file, or restart the server.

## Configuration

### Configuration file not found or the wrong one is used

Run with `--log-level debug` and look for the config path in the output. Order of lookup: `--config` or `MCPLS_CONFIG`, then `./mcpls.toml` only with `--trust-project-config`, then the user config directory ([locations](../getting-started/minimal-config.md#where-the-configuration-file-lives)). A warning names a project-local file that was ignored for lack of trust.

### Invalid configuration

Unknown keys, bad values and unsupported patterns fail at startup with a message naming the entry. Typical causes:

- A TOML syntax error.
- A missing `language_id`, `command` or other required field.
- A `file_patterns` entry in an unsupported form, such as `**/*.{ts,tsx}`, `src/**` or `.eslintrc`.
- A `workspace.roots` entry that does not exist.
- Two servers for one language without distinct `name` values, or two catch-all servers.

### Position out of bounds or document not found

Lines and columns start at 1, and the file must exist under a workspace root. Use an absolute path.

## Unexpected results

### get_diagnostics returns an empty list

Check `availability`. `pending` means the server has not published for the file yet (servers without pull support, such as typescript-language-server, only push); call again after a moment. `evicted` means a publish was dropped; the server's next publish restores it. Only `published` with an empty list means a clean file.

### A tool reports capability_not_advertised

The routed server does not support that feature: for example, type hierarchy on rust-analyzer. Run `get_tool_support` to see coverage per language. If every server for a language lists `handles`, add the routing value the tool needs to one of them ([`handles`](../reference/config.md#handles)).

### enclosing_symbol is not_computed or unavailable

| `reason` | Fix |
|----------|-----|
| `capability_absent` | The server does not advertise `documentSymbolProvider` |
| `request_failed`, `timed_out` | Retry, or check `get_server_logs` |
| `file_cap` | Too many distinct files; narrow the query or raise `workspace.max_documents` |
| `tracker_limit` | The file could not be opened; raise `workspace.max_documents` |
| `out_of_workspace` | The file is outside every root and is never opened |
| `deadline` | The 30 second budget ran out before this file |

### Was not started: the workspace is untrusted

mcpls runs with `--workspace-trust untrusted` and the server was not allowed. Add `--allow-server <id>` for each server you accept. If the message says the executable lies inside the workspace, nothing overrides that; install it outside the workspace. See [Untrusted-Workspace Mode](../advanced/untrusted-mode.md).

### tsserver pin warnings

mcpls could not pin TypeScript's `tsserver` to the one next to `typescript-language-server`. See [TypeScript: tsserver Pinning and TypeScript 7](../advanced/typescript.md#when-the-pin-is-not-applied).

## Shutdown hangs

The first `SIGTERM` or `SIGINT` starts a graceful shutdown that closes the language servers. If it takes too long, send the signal again to force an immediate exit.

## Getting help

Collect the output of `mcpls --version`, the language server's version, your configuration file and a `--log-level debug` log, then search or open an issue at <https://github.com/bug-ops/mcpls/issues>. Report security problems privately, as described in `SECURITY.md` in the repository.

## What's Next

If you want to understand why mcpls behaves this way, continue with [Architecture](../advanced/architecture.md).
