---
name: mcpls
description: >-
  Install, configure, and run the mcpls CLI — a Rust binary bridging MCP to LSP that gives
  an agent compiler-grade code intelligence (hover, definitions, references, diagnostics,
  rename). Use when setting up or registering mcpls as an MCP server, writing or debugging
  an mcpls.toml, choosing CLI flags or MCPLS_* environment variables, or diagnosing why
  mcpls or one of its language servers fails to start.
license: MIT OR Apache-2.0
compatibility: >-
  Wraps the `mcpls` Rust binary (Rust 1.88+ / edition 2024 to build from source). Requires at
  least one LSP server on PATH (rust-analyzer, pyright, gopls, clangd, …). HTTP transport
  requires building with the non-default `transport-http` feature.
metadata:
  repository: "https://github.com/bug-ops/mcpls"
  docs: "https://github.com/bug-ops/mcpls/tree/main/docs/user-guide"
---

# mcpls

## Purpose

mcpls is a single Rust binary that bridges the Model Context Protocol (MCP) to the
Language Server Protocol (LSP). It spawns and speaks LSP to real language servers
(rust-analyzer, pyright, gopls, clangd, …) and exposes their capabilities to an AI
agent as 20 MCP tools — hover, go-to-definition, references, diagnostics, rename,
completions, symbols, formatting, call hierarchy, and more.

This skill covers operating the **binary**: installing it, choosing CLI flags and
environment variables, registering it with an MCP client, and writing `mcpls.toml`.
It does not enumerate the 20 MCP tools themselves or their parameters — for that, see
[Tools Reference](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/tools-reference.md).

## Prerequisites

mcpls does not implement language analysis itself — it forwards to a real LSP server
that must already be installed and on `PATH` (e.g. `rust-analyzer`, `pyright-langserver`,
`gopls`, `clangd`, `typescript-language-server`). mcpls runs multiple LSP servers
concurrently, and one failing to start does not affect the others (graceful
degradation) — but a language with no server configured, or whose server isn't on
`PATH`, simply has no code intelligence available for it.

See [Language Server Setup](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/installation.md#language-server-setup)
for per-language install commands.

## Installation

**From crates.io (recommended):**

```bash
cargo install mcpls
```

**Pre-built binaries:** download the archive for your platform from
[GitHub Releases](https://github.com/bug-ops/mcpls/releases), extract it, and move
the `mcpls` binary onto your `PATH`. See
[Pre-Built Binaries](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/installation.md#method-2-pre-built-binaries-from-github-releases)
for per-platform archive names. Each archive ships with a `.sha256` sidecar; verify
before extracting:

```bash
curl -LO https://github.com/bug-ops/mcpls/releases/latest/download/mcpls-<target>.tar.gz
curl -LO https://github.com/bug-ops/mcpls/releases/latest/download/mcpls-<target>.tar.gz.sha256
shasum -a 256 -c mcpls-<target>.tar.gz.sha256
```

**From source (this repository):**

```bash
cargo install --path crates/mcpls-cli
```

This builds only the default feature set — no HTTP transport (see below). To build
with HTTP transport support, add `--features transport-http`.

**Verify:**

```bash
mcpls --version
```

## CLI Reference

Each mcpls-specific option below accepts an equivalent `MCPLS_*` environment
variable; the flag takes precedence when both are set. `--version`/`--help` have
no environment variable equivalent.

| Flag | Short | Env var | Default | Notes |
|---|---|---|---|---|
| `--config <FILE>` | `-c` | `MCPLS_CONFIG` | auto-detect | Always trusted, even a *relative* path set via the env var — naming a path is treated as consent, so this bypasses the project-config trust gate entirely (see [Config trust model](#config-trust-model)). Hard-errors at startup if the file doesn't exist — unlike auto-detection, it never falls back to defaults. |
| `--trust-project-config` | — | `MCPLS_TRUST_PROJECT_CONFIG` | `false` | See [Config trust model](#config-trust-model) below. The env var accepts `1`/`0`, `true`/`false`, `yes`/`no`, `y`/`n`, and `on`/`off` (case-insensitive); any other value is a startup parse error. |
| `--log-level <LEVEL>` | `-l` | `MCPLS_LOG` | `info` | Any `tracing-subscriber` `EnvFilter` directive works, e.g. `mcpls=debug,info`. An invalid value does **not** error — it silently falls back to `info`. |
| `--log-json` | — | `MCPLS_LOG_JSON` | `false` | Output logs in JSON format for structured logging. The env var accepts `1`/`0`, `true`/`false`, `yes`/`no`, `y`/`n`, and `on`/`off` (case-insensitive). |
| `--listen <ADDR>` | — | `MCPLS_LISTEN` | unset | HTTP transport bind address (e.g. `127.0.0.1:3000`). Only exists when built with `--features transport-http` — see [HTTP transport caveats](#registering-with-an-mcp-client). |
| `--http-path <PATH>` | — | `MCPLS_HTTP_PATH` | `/mcp` | URL path the MCP service mounts at. Only meaningful with `--listen`; same `transport-http` feature gate. |

Plus standard `--version` / `--help`. No subcommands.

## Registering with an MCP client

**stdio (default, works with any build):**

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

**Per-project, trusting that project's `mcpls.toml`:**

```json
{
  "mcpServers": {
    "mcpls": {
      "command": "mcpls",
      "args": ["--trust-project-config"]
    }
  }
}
```

Only pass `--trust-project-config` for repositories you trust — see
[Config trust model](#config-trust-model). Prefer the `args` form above (scoped to
this one client config entry) over `MCPLS_TRUST_PROJECT_CONFIG=true` in a shell
profile or `.envrc` — the env var is a blanket grant for every `mcpls` process
launched in that shell, including future untrusted checkouts.

**HTTP transport:** requires a binary built with `--features transport-http`
(`cargo install mcpls --features transport-http`, or the equivalent `cargo install
--path` form). This asymmetry matters when diagnosing startup failures:

- On a build **without** the feature, passing `--listen` on the command line is a
  startup **parse error** (`unexpected argument '--listen'`) — clap doesn't know the
  flag exists.
- On that same build, setting `MCPLS_LISTEN`/`MCPLS_HTTP_PATH` as environment
  variables produces **no error at all** — the fields they'd bind to are compiled
  out, so mcpls silently ignores them and serves stdio as usual.

If mcpls appears to ignore `MCPLS_LISTEN`, or `--listen` errors as unrecognized, the
binary was built without `transport-http` — reinstall with the feature enabled.

## Configuration

### Search order and paths

mcpls resolves configuration in this order; the first match wins:

1. `--config <FILE>` / `$MCPLS_CONFIG` — an explicit path is always trusted and
   loaded via a strict path that hard-errors if the file is missing (it never falls
   back or creates a default).
2. `./mcpls.toml` in the current directory — only loaded when trusted (see below).
3. The platform user-config path (table below).
4. Built-in defaults (covers 30 languages out of the box).

| Platform | Path |
|---|---|
| Linux | `$XDG_CONFIG_HOME/mcpls/mcpls.toml`, else `~/.config/mcpls/mcpls.toml` |
| macOS | `~/Library/Application Support/mcpls/mcpls.toml` |
| Windows | `%APPDATA%\mcpls\mcpls.toml` |

`~/.config/mcpls/mcpls.toml` is **not** read on macOS — only
`~/Library/Application Support/mcpls/mcpls.toml` is.

**First run writes a default config.** If auto-detection (tiers 2–4 above) finds no
existing file, mcpls writes one to the platform path in the table, populated with all
30 default language mappings, and continues running. If the write fails (e.g.
read-only filesystem), it degrades gracefully to in-memory defaults with a warning —
it does not crash. This auto-create behavior only applies to auto-detection; passing
`--config <path>` explicitly never creates a file, it only reads one.

### Config trust model

A project-local `./mcpls.toml` can set the `command`/`args` mcpls spawns as an LSP
server — so honoring one automatically from an unfamiliar checkout would be arbitrary
code execution. mcpls therefore **ignores** a discovered `./mcpls.toml` by default.
Pass `--trust-project-config` (or `MCPLS_TRUST_PROJECT_CONFIG=true`) only for
repositories you trust. Note that the env var grants trust process-wide, not
per-project — setting it in a shell profile trusts every `mcpls` invocation that
shell ever launches, not just the one project you meant to trust.

When a project config is found but ignored, mcpls does not just log a `tracing::warn!`
to stderr (which a stdio-based agent typically can't see) — it also appends a NOTE to
the `instructions` field of the MCP `initialize` response (`ServerInfo.instructions`,
populated by `McplsServer::get_info`). **Check the server instructions surfaced at
connection time**: if you see a note about an ignored project config, the file wasn't
malicious-by-default — it just needs an explicit trust decision.

### Starter config

A minimal `mcpls.toml` — expand per-language via
[Configuration Reference](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/configuration.md):

A `[workspace]` table is deliberately omitted here: `roots` defaults to `[]`, which
already auto-resolves to the current directory, and adding `[workspace]` for that
alone would zero out the 30 built-in `language_extensions` mappings (see
[`language_extensions`](references/configuration.md#workspace-fields)) unless you
list them all back explicitly.

```toml
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
name = "rust-analyzer"
timeout_seconds = 30
request_timeout_seconds = 30

[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]
name = "pyright"
timeout_seconds = 30
request_timeout_seconds = 30
```

For the full field reference (`handles` routing, `initialization_options`, `env`
allowlist, heuristics), see
[references/configuration.md](references/configuration.md).

## Task recipes

| Goal | How |
|---|---|
| Add support for a new language | Add a `[[lsp_servers]]` entry with `language_id`, `command`, `file_patterns`. See [Multi-Language Configuration](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/installation.md#multi-language-configuration). |
| Route specific tools to a specialized server | Set `handles` on each `[[lsp_servers]]` entry to claim only the tools that server should serve; see `handles` in [references/configuration.md](references/configuration.md). |
| Fix a slow/timing-out LSP request | Raise that server's `request_timeout_seconds` (per-request) and/or `timeout_seconds` (handshake) in `mcpls.toml`. |
| Handle a "still initializing" error on a large project | This is `Error::ServerInitializing` — the server is up but hasn't finished its `initialize` handshake (common with rust-analyzer on a large repo). It returns immediately, it does not time out, so raising `request_timeout_seconds` does nothing here. Wait and retry the call instead. |
| Debug why mcpls won't start | Run with `--log-level debug` (or `trace`) and inspect stderr; see [Troubleshooting](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/troubleshooting.md#advanced-debugging). |
| Use a trusted project's own `mcpls.toml` | Pass `--trust-project-config` — see [Config trust model](#config-trust-model). |

## Troubleshooting

For symptom-driven debugging, see
[Troubleshooting Guide](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/troubleshooting.md):

- ["command not found: mcpls"](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/troubleshooting.md#command-not-found-mcpls) — `PATH` doesn't include Cargo's bin directory.
- [mcpls not showing up in an MCP client](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/troubleshooting.md#claude-code-integration) — verify the client config points at an absolute binary path.
- ["LSP server not available for file type"](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/troubleshooting.md#lsp-server-issues) — no `[[lsp_servers]]` entry matches the file's extension.
- ["Configuration file not found" / unexpected config used](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/troubleshooting.md#configuration-issues) — check the [search order and paths](#search-order-and-paths) above first.
- [High memory or CPU usage](https://github.com/bug-ops/mcpls/blob/main/docs/user-guide/troubleshooting.md#performance-issues) — usually an over-broad `workspace.roots` or too many configured servers.

## Further reference

[references/configuration.md](references/configuration.md) — full `mcpls.toml` schema
tables (`[workspace]` fields, `[[lsp_servers]]` fields, the `handles` routing map, the
`env` allowlist), for cases the starter config above doesn't cover.
