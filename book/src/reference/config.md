# Configuration Reference

In this chapter you find every key of `mcpls.toml`, with type, default and rules. For a guided introduction read [Configuration](../guide/configuration.md) first. Unknown keys are rejected at startup.

## File structure

```text
[mcp]                   # optional presentation overrides
[workspace]             # optional workspace settings and limits
[[workspace.language_extensions]]   # optional, repeatable
[[lsp_servers]]         # one table per language server
[lsp_servers.heuristics]
[lsp_servers.initialization_options]
[lsp_servers.settings]
[lsp_servers.env]
```

Where the file is found is described in [Command Line and Environment](cli.md#--config).

## `[mcp]`

Overrides the text mcpls reports about itself. Every field is optional; omitting one keeps the built-in text. `serverInfo.name`, `version` and `website_url` are not configurable.

| Key | Overrides | Default | Maximum |
|-----|-----------|---------|---------|
| `title` | `serverInfo.title` | `MCPLS - MCP to LSP Bridge` | 128 bytes |
| `description` | `serverInfo.description` | The crate description | 1024 bytes |
| `instructions` | `ServerInfo.instructions` | Built-in capability text | 4096 bytes |
| `tool_prefix` | Every tool name, as `{tool_prefix}_{tool}` | No prefix | 32 bytes |

Limits are in UTF-8 bytes. A whitespace-only value is rejected as empty; omit the key instead. `instructions` replaces the built-in text rather than appending to it. `tool_prefix` may contain only ASCII letters, digits, `_` and `-`, and must start and end with a letter or digit.

## `[workspace]`

### `roots`

Array of strings. Default `[]`, meaning the process working directory.

Directories the servers analyze. Each must exist and is canonicalized before any server starts. An empty string is rejected. A relative root resolves against the directory of the config file when the file was named explicitly (`--config`, `MCPLS_CONFIG`, or a trusted project-local file), and against the process working directory when the file came from the user config directory. A file path is also accepted under a root-level system symlink spelling of a root (for example `/tmp/proj` for a root at `/private/tmp/proj` on macOS); links deeper in the tree are not admitted.

### `position_encodings`

Array of `"utf-8"`, `"utf-16"`, `"utf-32"`. Default `["utf-8", "utf-16"]`.

Encodings offered to each server during `initialize`, in order. Must be non-empty. See [Positions and Encodings](../advanced/positions.md).

### `heuristics_max_depth`

Integer, at most 64. Default `10`.

How deep the recursive project-marker search goes.

### `max_documents`

Integer. Default `100`; `0` disables the limit.

Documents kept open at once. At the limit the least recently used unlocked document is closed; if every document is in use, the call fails with a "document limit exceeded" error.

### `max_file_size`

Integer, bytes. Default `10485760` (10 MiB); `0` disables the limit. Values above `1073741824` (1 GiB) are rejected.

Largest file mcpls opens; larger files fail with a "file size limit exceeded" error.

### `max_concurrent_server_starts`

Integer, at least 1. Default `8`.

Servers starting at the same time. Others start as earlier ones finish `initialize`, in configuration order.

### `indexing_ready_timeout_seconds`

Integer, above 3 and below 60. Default `30`.

How long whole-workspace queries (hover, definition, references, rename, completions, code actions, call hierarchy) wait for the routed server to finish indexing, once a readiness signal has shown that indexing is in progress.

### `language_extensions`

Array of tables with `extensions` (array of strings, no leading dot, case-sensitive) and `language_id`. Default: the 30 mappings below.

```toml
[[workspace.language_extensions]]
extensions = ["py", "pyi", "pyw"]
language_id = "python"
```

Supplying any mapping replaces the defaults, so include every language you use.

#### Default language mappings

| Language | Extensions | Language ID |
|----------|-----------|-------------|
| Rust | rs | rust |
| Python | py, pyw, pyi | python |
| JavaScript | js, mjs, cjs | javascript |
| TypeScript | ts, mts, cts | typescript |
| TypeScript React | tsx | typescriptreact |
| JavaScript React | jsx | javascriptreact |
| Go | go | go |
| C | c, h | c |
| C++ | cpp, cc, cxx, hpp, hh, hxx | cpp |
| Java | java | java |
| Ruby | rb | ruby |
| PHP | php | php |
| Swift | swift | swift |
| Kotlin | kt, kts | kotlin |
| Scala | scala, sc | scala |
| Zig | zig | zig |
| Lua | lua | lua |
| Shell | sh, bash, zsh | shellscript |
| JSON | json | json |
| TOML | toml | toml |
| YAML | yaml, yml | yaml |
| XML | xml | xml |
| HTML | html, htm | html |
| CSS | css | css |
| SCSS | scss | scss |
| Less | less | less |
| Markdown | md, markdown | markdown |
| C# | cs | csharp |
| F# | fs, fsi, fsx | fsharp |
| R | r, R | r |

## `[[lsp_servers]]`

One table per language server.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `language_id` | string | required | Language identifier sent to the server |
| `command` | string | required | Executable, on `PATH` or absolute |
| `args` | array of strings | `[]` | Command-line arguments |
| `file_patterns` | array of strings | `[]` | Files this server serves ([forms](#file_patterns)) |
| `timeout_seconds` | integer, 1 to 900 | `30` | The `initialize` handshake |
| `request_timeout_seconds` | integer, 1 to 900 | `30` | Each LSP request behind a tool call |
| `initialization_options` | table | none | Sent in `initialize` |
| `settings` | table | none | Pushed after `initialized` |
| `env` | table of strings | `{}` | Environment variables for the server |
| `heuristics` | table | none | Project markers |
| `name` | string | the `language_id` | Routing identity |
| `handles` | array of strings | unset (catch-all) | Routing values this server serves |
| `indexing` | `"disabled"` | unset | Opt out of indexing gating |
| `selection` | `"explicit"` or `"auto"` | `"explicit"` | Native TypeScript 7 selection |

### `file_patterns`

mcpls routes a file by its extension, or by its name when it has none, so a pattern only maps one extension or one extensionless name to this server's `language_id`.

- A final segment `*.EXT`, where `EXT` is letters, digits, `_`, `-` or `+`, for example `**/*.rs`. The directory part (`**/`, `src/**/`) is accepted and ignored.
- One pattern per extension: `["**/*.cpp", "**/*.h"]`. Brace expansion is not supported.
- An extensionless file by bare name, `NAME` or `**/NAME`, such as `Makefile`. The name is letters, digits, `_`, `-` or `+`, case-sensitive, with no directory part other than `**/`.
- Everything else is rejected at startup with an error naming the entry and pattern: character classes, `?`, `src/**`, `**/*`, dotfiles and dotted names (`.eslintrc`, `Makefile.am`), single files (`src/main.rs`) and multi-part extensions (`**/*.tar.gz`).

A file with no mapping fails with `no LSP server configured for language: plaintext`, followed by the file's extension or name and the configured patterns.

### `timeout_seconds` and `request_timeout_seconds`

`timeout_seconds` bounds the `initialize` handshake only. Raise it for servers that load a large project before answering, such as OmniSharp on a big solution.

`request_timeout_seconds` bounds one request attempt. When a server answers "content modified" (`-32802`), mcpls retries up to 4 attempts in total with 0.5 s, 1 s and 2 s of backoff, so the worst-case latency of one tool call is `4 * request_timeout_seconds + 3.5` seconds, plus `timeout_seconds` if a respawn is needed. Completions are further capped at 10 seconds. The shutdown request during teardown uses a fixed 5 seconds.

### `initialization_options` and `settings`

Free-form tables in the server's own vocabulary. Top-level dotted keys expand into nested objects, while keys inside values are left untouched. An empty `settings` table is rejected, and so is a TOML datetime.

```toml
[lsp_servers.initialization_options]
cargo.features = "all"

[lsp_servers.settings]
"python.analysis.typeCheckingMode" = "strict"
```

For `typescript-language-server`, mcpls adds `tsserver.path` automatically unless you set it, or set other options without it ([TypeScript](../advanced/typescript.md)).

### `env`

The server does not inherit mcpls's environment. It is cleared, then `PATH`, `HOME`, `USERPROFILE`, `TMPDIR`, `TEMP`, `TMP` and the Windows system variables are passed on, and then `env` is applied and can override any of them. Setting `PATH` replaces the passthrough value. On Unix, your `PATH` is searched first, so a bare `command` becomes unresolvable unless your `PATH` contains it; Windows falls back to the parent's `PATH`. To add a directory, give an absolute `command` instead.

### `heuristics`

```toml
[lsp_servers.heuristics]
project_markers = ["Cargo.toml", "rust-toolchain.toml"]
```

The server starts if any marker exists in the workspace tree, searched to `heuristics_max_depth` and skipping directories such as `node_modules`, `target` and `.git`. An empty list, or no table, means the server always starts.

### `name`

The routing identity, used by `--allow-server`, `restart_server` and error messages. Two servers may share a `language_id`, but each needs a distinct identity.

### `handles`

Restricts a server to exactly the listed routing values; unset means catch-all, serving every tool no other server claims for that language. The values are routing identifiers, not tool names:

| `handles` value | Tools it governs |
|-----------------|------------------|
| `hover` | `get_hover` |
| `definition` | `get_definition` |
| `type_definition` | `go_to_type_definition` |
| `declaration` | `go_to_declaration` |
| `implementation` | `go_to_implementation` |
| `references` | `get_references` |
| `diagnostics` | `get_diagnostics` and `get_cached_diagnostics` |
| `rename` | `rename_symbol`, `prepare_rename` |
| `completions` | `get_completions` |
| `signature_help` | `get_signature_help` |
| `document_symbols` | `get_document_symbols` |
| `workspace_symbols` | `workspace_symbol_search` |
| `format_document` | `format_document` |
| `format_range` | `format_range` |
| `code_actions` | `get_code_actions` |
| `call_hierarchy` | `prepare_call_hierarchy`, `get_incoming_calls`, `get_outgoing_calls` |
| `type_hierarchy` | `prepare_type_hierarchy`, `get_supertypes`, `get_subtypes` |
| `document_highlights` | `get_document_highlights` |
| `inlay_hints` | `get_inlay_hints` |
| `selection_range` | `get_selection_ranges` |
| `folding_range` | `get_folding_ranges` |

The hierarchy values each govern one route, because an item is meaningful only to the server that produced it. Both diagnostics tools share a route so they are always answered by the same server.

Rules:

- At most one server per language may omit `handles`, and a tool may be claimed by one server per language.
- If two servers for one language are both applicable in the same workspace and share an identity, both omit `handles`, or both claim a tool, mcpls refuses to start and names the entries. Mutually exclusive `heuristics` make a pair unambiguous.
- If the server a tool is routed to fails to spawn, the tool falls back to the language's catch-all, if one is running. Otherwise it reports that no server is available, and never falls back to a server that declined it through `handles`.
- When a language has no catch-all, mcpls logs a warning naming the routing values no server claims.
- `workspace_symbol_search` has no language. It goes to the first server that claims `workspace_symbols`, else the first catch-all, and fails if neither exists.

### `indexing`

Set to `"disabled"` to exempt this server from indexing-readiness gating, for a server whose readiness signal is unreliable or too slow to use as a gate.

### `selection`

Valid only on a `typescript-language-server` command, and written on the generated default `typescript` entry. `"auto"` lets mcpls replace `command` and `args` with the native TypeScript 7 server. See [TypeScript](../advanced/typescript.md#option-2-let-mcpls-choose).

## Complete examples

### Python, Go and TypeScript

```toml
[workspace]
roots = ["/Users/you/projects/myapp"]

[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]
timeout_seconds = 45

[lsp_servers.initialization_options]
python.analysis.typeCheckingMode = "basic"
python.analysis.autoSearchPaths = true

[[lsp_servers]]
language_id = "go"
command = "gopls"
file_patterns = ["**/*.go"]

[lsp_servers.initialization_options]
analyses.unusedparams = true
staticcheck = true

[[lsp_servers]]
language_id = "typescript"
command = "typescript-language-server"
args = ["--stdio"]
file_patterns = ["**/*.ts", "**/*.tsx", "**/*.js", "**/*.jsx"]

[lsp_servers.initialization_options]
preferences.quotePreference = "single"
preferences.importModuleSpecifierPreference = "relative"
```

### Everything at once

```toml
[mcp]
title = "My Custom Bridge"
description = "Internal LSP bridge for Acme Corp"
instructions = "Use get_hover before get_definition."
tool_prefix = "optics"

[workspace]
roots = ["/path/to/project"]
position_encodings = ["utf-8", "utf-16"]
heuristics_max_depth = 10
max_documents = 100
max_file_size = 10485760
max_concurrent_server_starts = 8
indexing_ready_timeout_seconds = 30

[[workspace.language_extensions]]
extensions = ["nu"]
language_id = "nushell"

[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
timeout_seconds = 30
request_timeout_seconds = 30

[lsp_servers.heuristics]
project_markers = ["Cargo.toml", "rust-toolchain.toml"]

[lsp_servers.initialization_options]
cargo.features = "all"
checkOnSave.command = "clippy"
```

## What's Next

If a configuration does not behave as you expect, see [Troubleshooting](../guide/troubleshooting.md).
