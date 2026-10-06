# Command Line and Environment

In this chapter you find every command-line flag of the `mcpls` binary and its environment variable. The flags are the same ones `mcpls --help` prints. Options marked HTTP exist only in builds with the `transport-http` feature ([enabling it](../advanced/transports.md#enabling-the-http-transport)).

## Usage

```bash
mcpls [OPTIONS]
```

mcpls takes no subcommands. With no options it serves MCP over stdio using the discovered configuration.

## Options

| Flag | Environment variable | Default | Description |
|------|----------------------|---------|-------------|
| `-c`, `--config <FILE>` | `MCPLS_CONFIG` | discovered | Path to the configuration file. A named path is always trusted |
| `--trust-project-config` | `MCPLS_TRUST_PROJECT_CONFIG` | off | Load a `./mcpls.toml` found in the current directory |
| `--workspace-trust <MODE>` | none | `trusted` | `trusted` or `untrusted`; command line only |
| `--allow-server <ID>` | none | none | Server to start in untrusted mode; repeatable; command line only |
| `-l`, `--log-level <LEVEL>` | `MCPLS_LOG` | `info` | A level or comma-separated `target=level` directives |
| `--log-json` | `MCPLS_LOG_JSON` | off | Output logs as JSON lines |
| `--listen <ADDR>` (HTTP) | `MCPLS_LISTEN` | none | Serve Streamable HTTP on this address instead of stdio |
| `--http-path <PATH>` (HTTP) | `MCPLS_HTTP_PATH` | `/mcp` | URL path the service is mounted at |
| `--http-stream-liveness <MODE>` (HTTP) | `MCPLS_HTTP_STREAM_LIVENESS` | `probe` | `probe` or `off` |
| `--http-allowed-origin <ORIGIN>` (HTTP) | `MCPLS_HTTP_ALLOWED_ORIGINS` | none | Extra browser origin; repeatable or comma-separated |
| `--http-allowed-host <HOST>` (HTTP) | `MCPLS_HTTP_ALLOWED_HOSTS` | none | Extra `Host` value; repeatable or comma-separated |
| `-h`, `--help` | none | | Print help |
| `-V`, `--version` | none | | Print the version |

Boolean values (`MCPLS_TRUST_PROJECT_CONFIG`, `MCPLS_LOG_JSON`) accept `1`/`0`, `true`/`false`, `yes`/`no`, `y`/`n` and `on`/`off`, in any case. Any other value, including an empty string, is a startup error.

## Details

### `--config`

Without it, mcpls searches in order for `$MCPLS_CONFIG`, a trusted `./mcpls.toml`, and the platform user config:

| Platform | Location |
|----------|----------|
| Linux | `$XDG_CONFIG_HOME/mcpls/mcpls.toml`, else `~/.config/mcpls/mcpls.toml` |
| macOS | `~/Library/Application Support/mcpls/mcpls.toml` |
| Windows | `%APPDATA%\mcpls\mcpls.toml` |

If none exists, mcpls writes a default file in the user config directory (not in untrusted mode) and continues with the built-in servers.

### `--trust-project-config`

A `mcpls.toml` in the current directory can name the program mcpls runs, so it is ignored unless you pass this flag. It is a grant for the whole process. See [Security and Trust](../advanced/security.md).

### `--workspace-trust` and `--allow-server`

With `untrusted`, only the servers named by `--allow-server` start, and the executable, configuration and environment checks apply. `--allow-server` without `untrusted` is a usage error, as is combining `untrusted` with `--trust-project-config`. See [Untrusted-Workspace Mode](../advanced/untrusted-mode.md).

### `--log-level`

Accepts `trace`, `debug`, `info`, `warn`, `error` and `off`, in any case, or directives such as `info,mcpls_core=debug`. A bare word must be a level: `mcpls_core` alone is rejected, write `mcpls_core=trace`. An unknown level is rejected at startup. Logs go to standard error.

### `--listen` and the HTTP options

See [Transports](../advanced/transports.md) for the behavior, limits and the Host and Origin rules. Every invalid value of an HTTP option is a usage error with exit code 2. `--http-path` is validated even when `--listen` is not given.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Clean shutdown |
| `1` | A startup or runtime error, logged before exit |
| `2` | A command-line usage error |
| `101` | mcpls panicked; language servers are still reaped |

## What's Next

For the file format, see the [Configuration Reference](config.md).
