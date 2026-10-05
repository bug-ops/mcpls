# Configuration Reference

Complete reference for configuring mcpls.

## Configuration File

mcpls uses TOML format for configuration. The file can be placed in several locations (searched in order):

1. Path specified by `--config` flag
2. `$MCPLS_CONFIG` environment variable
3. `./mcpls.toml` (current directory) — **only loaded with `--trust-project-config`** (or
   `MCPLS_TRUST_PROJECT_CONFIG=true`); see [Trusting a Project-Local Config](#trusting-a-project-local-config)
4. Platform user-config directory:
   - Linux: `$XDG_CONFIG_HOME/mcpls/mcpls.toml`, else `~/.config/mcpls/mcpls.toml`
   - macOS: `~/Library/Application Support/mcpls/mcpls.toml`
   - Windows: `%APPDATA%\mcpls\mcpls.toml`

### Trusting a Project-Local Config

A `mcpls.toml` discovered in the current directory controls which command mcpls
spawns as an LSP server (and other workspace settings), so mcpls does not load it
automatically. Running `mcpls` inside an untrusted checkout must not execute
commands from that checkout without explicit consent. Trusting it also confers
authorship of the agent-facing `serverInfo.title`/`description`/`instructions`
text (see [MCP Section](#mcp-section)) — a trusted config can replace that text
wholesale, with no in-band marker distinguishing it from mcpls's own built-in
wording.

To load a project-local `mcpls.toml`, opt in explicitly:

```bash
mcpls --trust-project-config
# or
MCPLS_TRUST_PROJECT_CONFIG=true mcpls
```

Without this flag, a `./mcpls.toml` in the current directory is ignored (a warning
is logged naming the ignored path) and mcpls falls through to the user config
directory or built-in defaults — including built-in project-marker heuristics, so
e.g. a `Cargo.toml` in the workspace still spawns rust-analyzer. An explicit
`--config <path>` or `$MCPLS_CONFIG` is always trusted, since naming a path is
itself the user's consent.

#### Trust model

`--trust-project-config` governs only the mcpls config. It does not make the workspace itself safe
to analyze: language servers execute workspace-supplied code, which is inherent to LSP and the same
for every LSP bridge, and mcpls does not sandbox them. See
[SECURITY.md](https://github.com/bug-ops/mcpls/blob/main/SECURITY.md) for the full policy.

| Server | Workspace-supplied code it can run |
|--------|-------------------------------------|
| typescript-language-server | The workspace's `node_modules/typescript/lib/tsserver.js` unless pinned (below); tsconfig plugins; automatic type acquisition may fetch packages over the network |
| rust-analyzer | Cargo build scripts and procedural macros |
| pyright / pylsp | The project's Python environment, plugins and interpreters named in config |
| gopls | The Go toolchain, including toolchain downloads requested by `go.mod` |
| clangd | Commands from `compile_commands.json` and `.clangd` configuration |

Only the typescript-language-server row has been verified live; the others describe the well-known
behavior of those servers. Run mcpls against untrusted code only inside an environment you are
willing to have that code execute in (a container or a disposable VM).

**tsserver pin (TypeScript).** By default mcpls passes the tsserver bundled next to
`typescript-language-server` as `initializationOptions.tsserver.path`, so a workspace's own
`node_modules` tsserver is not selected. Covered: `npm -g` style symlink installs (verified with
Homebrew's node) that have a global `typescript` package with a `package.json` `version` next to
the server. Not covered (#604), with a warning logged: Windows `.cmd` shims, script launchers (pnpm,
Volta, asdf, mise), `npx`/`bunx`/`node cli.mjs` wrappers, and installs with no valid `typescript`
package. A server installed inside the workspace is pinned with a warning, but that narrows
nothing because the server itself is workspace code. A
`tsserver.path` in your own `initialization_options` always wins; set it to a workspace path to opt
back in to the workspace's TypeScript. Other `initialization_options` that do not set
`tsserver.path` disable the pin (with a warning). Automatic type acquisition network fetches are not
prevented. The pin narrows one vector; it does not make an untrusted workspace safe.

> [!WARNING]
> `--trust-project-config` (and `MCPLS_TRUST_PROJECT_CONFIG=true`) is a **global**
> trust grant for the whole mcpls process — it is not scoped to a single project.
> Prefer setting it on a per-project MCP client config entry (the `args`/`env` for
> that project's `mcpls` server registration) rather than in your shell profile or
> a user-global MCP client config, so it doesn't silently apply the next time
> mcpls is launched against a different, untrusted checkout.
>
> `$MCPLS_CONFIG` is a second, by-design door past this gate: it is always
> trusted regardless of this flag, including when set to a relative path. A
> repository's own `.envrc` (or similar) exporting `MCPLS_CONFIG=./mcpls.toml`
> would make direnv-style tooling load it automatically — not a bug (an
> explicitly named path is consent, per the design above), but worth knowing if
> you audit a checkout for auto-executing config before running mcpls in it.

## Configuration Structure

```toml
# Optional: MCP serverInfo/initialize presentation overrides
[mcp]
title = "My Custom Bridge"
description = "Internal LSP bridge for Acme Corp"
instructions = "Use get_hover before get_definition."

# Workspace configuration
[workspace]
roots = ["/path/to/project1", "/path/to/project2"]
position_encodings = ["utf-8", "utf-16"]

# LSP server definitions (can have multiple)
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
timeout_seconds = 30
request_timeout_seconds = 30

# Optional: LSP server initialization options
[lsp_servers.initialization_options]
cargo.features = "all"
```

## MCP Section

Overrides the text mcpls reports about itself over MCP. Every field is
optional and independent; omitting one keeps mcpls's built-in text for it.
`serverInfo.name`, `version`, and `website_url` are not configurable —
`name` is the MCP-spec machine identifier (asserted by clients that key off
it), and `version`/`website_url` are project metadata rather than
presentation text.

| Field | Overrides | Default | Max size |
|-------|-----------|---------|----------|
| `mcp.title` | `serverInfo.title` | `"MCPLS - MCP to LSP Bridge"` | 128 bytes |
| `mcp.description` | `serverInfo.description` | the crate's `Cargo.toml` description | 1024 bytes |
| `mcp.instructions` | `ServerInfo.instructions` | built-in capability blurb | 4096 bytes |
| `mcp.tool_prefix` | every tool name (`{tool_prefix}_{tool}`) | unprefixed tool names | 32 bytes |

```toml
[mcp]
title = "My Custom Bridge"
description = "Internal LSP bridge for Acme Corp"
instructions = "Use get_hover before get_definition."
tool_prefix = "optics"
```

> [!IMPORTANT]
> A configured `mcp.instructions` **replaces** the built-in capability
> blurb entirely rather than appending to it — an AI agent that reads
> `instructions` at connection time (per mcpls's own Agent Skill) will see
> only your configured text. The untrusted-project-config NOTE (see
> [Trusting a Project-Local Config](#trusting-a-project-local-config)) is
> unrelated and is still appended after it when applicable.

Every field's size limit is enforced in UTF-8 bytes, not characters. A
whitespace-only value (e.g. `title = "   "`) is rejected as empty, the same
as an actually-empty string — omit the field entirely to use the built-in
default instead.

`mcp.tool_prefix` prefixes every MCP tool name with `{tool_prefix}_`, useful
when an MCP client runs multiple mcpls bridges concurrently (one per
project) and needs to tell their tools apart. The prefix must contain only
ASCII letters, digits, `_`, and `-`, and must start and end with a letter or
digit — a trailing `_`/`-` is rejected rather than stripped, since the `_`
separator before each tool name is inserted automatically by mcpls.

## Workspace Section

### `workspace.roots`

**Type**: Array of strings
**Default**: `[]` (auto-detect from current directory)

Workspace root directories for LSP servers.

```toml
[workspace]
# Single workspace
roots = ["/Users/username/projects/myproject"]

# Multiple workspaces
roots = [
    "/Users/username/projects/frontend",
    "/Users/username/projects/backend"
]

# Auto-detect (empty array)
roots = []
```

Where a relative root resolves against depends on which config file it comes
from:

- **An explicitly named config file** -- a project-local `mcpls.toml`, or a
  config named via `--config`/`$MCPLS_CONFIG` -- resolves each relative root
  against the directory containing that TOML file, not against mcpls's
  process working directory. For example, a repository-owned config at
  `<repo>/.agents/mcpls.toml` can target the repository root portably with:

  ```toml
  [workspace]
  roots = [".."]
  ```

  In that same file, `roots = ["."]` selects `<repo>/.agents`. This keeps a
  committed config's meaning stable regardless of launcher cwd.
- **The auto-discovered global/user config**
  (`~/.config/mcpls/mcpls.toml`, or the platform equivalent), loaded only
  when no project-local or explicitly named config applies, resolves each
  relative root against the process working directory instead. It isn't
  tied to any particular project, so a relative root there is more
  intuitively read as "relative to wherever mcpls was launched."

The resolved path must exist and is canonicalized before any LSP server is
initialized. A file path is also accepted under a root-level system symlink spelling of a root
(for example `/tmp/proj` on macOS for a root at `/private/tmp/proj`) once that spelling is
verified to resolve to the root; links deeper in the tree are not admitted. See
[`file_path`](tools-reference.md#file_path). A `ServerConfig` built programmatically has no config-file
location either; its relative roots are likewise resolved against the
process cwd when `serve`/`serve_with` starts.

On Windows, a `workspace.roots` entry that is rooted-without-a-drive (e.g.
`\workspace`) or drive-relative (e.g. `C:workspace`) is joined under the base
directory above, the same as any other relative root -- this deliberately
does not replicate native Windows path semantics (where those forms resolve
against the current drive's root or that drive's own current directory,
respectively). This differs from `--config`/`$MCPLS_CONFIG` itself, whose
path is still resolved with native semantics.

The empty default, `roots = []`, remains distinct: it selects the process cwd
at startup. Absolute roots continue to work unchanged. An empty `workspace.roots`
entry (e.g., `roots = [""]`) is rejected as invalid configuration with a clear
error. A missing relative root is also rejected instead of reaching LSP
initialization as an invalid `file://` URI.

### `workspace.position_encodings`

**Type**: Array of strings
**Default**: `["utf-8", "utf-16"]`
**Options**: `"utf-8"`, `"utf-16"`, `"utf-32"`

Preferred position encodings for LSP communication, offered to each spawned server during the `initialize` handshake in the listed order. The list must be non-empty and contain only the options above; mcpls rejects the config at load time otherwise.

```toml
[workspace]
position_encodings = ["utf-8", "utf-16", "utf-32"]
```

This is a preference, not a restriction: per the LSP spec, UTF-16 is a mandatory fallback encoding, so a server may still reply with UTF-16 even if it's omitted from this list. Most language servers negotiate UTF-16 by default.

### `workspace.language_extensions`

**Type**: Array of `LanguageExtensionMapping` objects
**Default**: 30 built-in language mappings (see below)

Custom file extension to language ID mappings. Allows you to:
- Add support for specialized file types
- Override default extension associations
- Reduce memory usage by including only languages you need

```toml
[workspace]

# Add Nushell support
[[language_extensions]]
extensions = ["nu"]
language_id = "nushell"

# Override Rust to use custom language ID
[[language_extensions]]
extensions = ["rs"]
language_id = "custom-rust"

# Add multiple extensions for Python
[[language_extensions]]
extensions = ["py", "pyi", "pyw"]
language_id = "python"
```

#### Default Language Mappings

mcpls includes 30 language mappings by default:

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

These defaults are automatically included when you don't specify custom `language_extensions`. If you provide any custom mappings, you must include all languages you want to use.

#### Minimal Configuration Strategy

For better performance, configure only the languages you actually use:

```toml
[workspace]

# Only Rust and Python
[[language_extensions]]
extensions = ["rs"]
language_id = "rust"

[[language_extensions]]
extensions = ["py", "pyi"]
language_id = "python"
```

This reduces memory usage compared to loading all 30 default mappings.

### `workspace.max_documents`

**Type**: Integer
**Default**: `100`

Maximum number of documents mcpls will keep open simultaneously. Once the ceiling is reached, opening a new document evicts the least-recently-used, unlocked document (sending `textDocument/didClose` to its LSP server) to make room; if every tracked document is locked (in active use), the tool call fails with a "document limit exceeded" error instead. Set to `0` to disable the limit.

```toml
[workspace]
max_documents = 500
```

A call that opts into `context: "enclosing_symbol"` opens at most `max_documents / 4` distinct files (at least 1, at most 16; 16 when the limit is `0`) for symbol lookup, so enrichment cannot evict most of the open documents.

Raising this limit increases mcpls's steady-state memory usage, since each open document's full content is held in memory. This is most useful for long-running agent sessions or broad-scope work (large monorepo audits, repo-wide refactors) that touch more than 100 distinct files.

### `workspace.max_file_size`

**Type**: Integer (bytes)
**Default**: `10485760` (10MB)

Maximum size, in bytes, of a single file mcpls will open. A file larger than this fails with a "file size limit exceeded" error. Set to `0` to disable the limit. Values above 1 GiB (`1073741824`) are rejected at startup with an `InvalidConfig` error; use a lower value, or `0`.

The limit also derives the per-response disk-read budget used for position conversion with non-UTF-16 servers: 4 times the limit (4 times the default when `0`), at most 256 MiB.

```toml
[workspace]
max_file_size = 0  # unlimited
```

Useful when a project contains files larger than 10MB (e.g. generated code, data fixtures) that still need LSP-backed tools to work against them.

### `workspace.max_concurrent_server_starts`

**Type**: Integer
**Default**: `8`

Maximum number of language servers started at the same time. Servers beyond the limit start as earlier ones finish `initialize`, in configuration order, and each server is usable as soon as its own startup completes. `0` is rejected at startup. Lower it on a small machine when many heavy servers (for example `OmniSharp` plus `jdtls`) are configured; mcpls logs an info line when more servers are configured than the limit.

```toml
[workspace]
max_concurrent_server_starts = 2
```

### `workspace.indexing_ready_timeout_seconds`

**Type**: Integer (seconds)
**Default**: `30`

Maximum time a whole-workspace query (hover, definition, references, rename, completions, code actions, call hierarchy incoming/outgoing calls) waits for its routed LSP server to report it has finished its initial workspace indexing, once a readiness signal has actually shown indexing is in progress. If the server is still indexing when this elapses, the tool call fails with a "still indexing" error instead of silently answering from a partial index. A server that never reports a readiness signal is never delayed.

```toml
[workspace]
indexing_ready_timeout_seconds = 45
```

Must be greater than 3 and less than 60 seconds; mcpls rejects the config otherwise. Raise it for large monorepos where the initial workspace load routinely takes longer than the default.

## LSP Server Configuration

### `indexing`

**Type**: String
**Default**: unset (readiness gating enabled)

Set to `"disabled"` to opt this server out of workspace-indexing readiness
gating entirely — whole-workspace queries routed to it never wait for an
indexing-readiness signal, even if the server emits one.

```toml
[[lsp_servers]]
language_id = "go"
command = "gopls"
indexing = "disabled"
```

Use this for a server whose indexing-progress signal is unreliable or too
slow to be useful as a gate; every other server config field still applies.

Each `[[lsp_servers]]` section defines a language server.

### `language_id`

**Type**: String
**Required**: Yes

Language identifier for this server.

```toml
[[lsp_servers]]
language_id = "rust"  # Standard: rust, python, typescript, javascript, go, etc.
```

### `command`

**Type**: String
**Required**: Yes

Command to execute the language server.

```toml
[[lsp_servers]]
command = "rust-analyzer"  # Must be in PATH or absolute path
```

For absolute paths:
```toml
[[lsp_servers]]
command = "/usr/local/bin/rust-analyzer"
```

### `args`

**Type**: Array of strings
**Default**: `[]`

Command-line arguments for the language server.

```toml
[[lsp_servers]]
command = "pyright-langserver"
args = ["--stdio"]  # Many servers require --stdio flag
```

### `file_patterns`

**Type**: Array of strings (glob patterns)
**Required**: No (defaults to empty array)

File patterns to associate with this language server.

```toml
[[lsp_servers]]
file_patterns = ["**/*.rs"]  # Rust files

[[lsp_servers]]
file_patterns = ["**/*.py", "**/*.pyi"]  # Python files

[[lsp_servers]]
file_patterns = ["**/*.ts", "**/*.tsx", "**/*.js", "**/*.jsx"]  # TS/JS files
```

Glob pattern syntax:
- `**` - Match any number of directories
- `*` - Match any characters except `/`
- `?` - Match single character
- `[abc]` - Match any character in brackets

### `timeout_seconds`

**Type**: Integer (1 to 900)
**Default**: `30`

Timeout in seconds for the `initialize` handshake during server startup. Values outside 1 to 900 are rejected at load time.
Servers that load a large project before answering `initialize` (e.g.
OmniSharp on a big Unity/C# solution) need this raised - the default 30 s can
otherwise cut the server off mid-initialization.

This does **not** bound individual tool-call requests (hover, definition,
references, etc.) sent after initialization - see `request_timeout_seconds`
below for that. The LSP server's `shutdown` request during teardown uses a
separate, fixed 5 s timeout that is not configurable.

```toml
[[lsp_servers]]
timeout_seconds = 60  # Increase for servers slow to complete `initialize`
```

### `request_timeout_seconds`

**Type**: Integer (1 to 900)
**Default**: `30`

Timeout in seconds (1 to 900, rejected at load time otherwise) applied to each individual LSP request issued while
translating an MCP tool call (hover, definition, references, diagnostics,
rename, etc.). Independent of `timeout_seconds`, which only bounds the
`initialize` handshake.

This bounds a single request **attempt**, not a whole tool call: when the LSP
server responds with `-32802` (content modified), mcpls retries up to 4
attempts total with exponential backoff (0.5 s + 1 s + 2 s = 3.5 s of total
sleep). So the worst-case latency for one tool call is:

```
4 * request_timeout_seconds + 3.5 seconds
```

If a tool call also triggers a server respawn (because the previous server
process had died), add `timeout_seconds` on top of that, since
`initialize` runs again before the request is retried.

Completion requests (`textDocument/completion`) are further capped at 10
seconds regardless of this setting - completions are latency-sensitive
enough that a slower result isn't useful, and this cap cannot currently be
raised. If completions specifically need a higher ceiling, file an issue
requesting a dedicated `completion_timeout_seconds` field rather than raising
`request_timeout_seconds`, which would not affect completions above 10 s.

A value of `0` is rejected at config load time; the effective timeout is
always at least 1 second.

```toml
[[lsp_servers]]
request_timeout_seconds = 60  # Increase for a slow LSP server (e.g. large monorepo indexing)
```

### `initialization_options`

**Type**: Table (key-value pairs)
**Default**: `{}`

Server-specific initialization options passed during LSP initialization.

```toml
[lsp_servers.initialization_options]
# rust-analyzer specific options
cargo.features = "all"
checkOnSave.command = "clippy"

# pyright specific options
python.analysis.typeCheckingMode = "strict"
```

See your language server documentation for available options.

For `typescript-language-server`, mcpls adds `tsserver.path` automatically unless you set it or set other options without it; see [Trust model](#trust-model).

### `env`

**Type**: Table (key-value pairs)
**Default**: `{}`

Environment variables to set for the LSP server process.

The spawned server does **not** inherit mcpls's full environment. Its
environment is cleared, then a minimal allowlist is passed through from
mcpls's own process — `PATH`, `HOME`, `USERPROFILE`, `TMPDIR`/`TEMP`/`TMP` on
every platform, plus Windows essentials (`SystemRoot`, `APPDATA`,
`LOCALAPPDATA`, and others the process loader and Node-based servers need) —
and only then is `env` applied on top, so entries here can override any
passthrough value. Use `env` to restore anything your server needs beyond
that allowlist: proxy settings, `VIRTUAL_ENV`/`PYTHONPATH`, toolchain
variables a `build.rs` reads (`DATABASE_URL`, `LIBCLANG_PATH`, …), or
session-specific values like `SSH_AUTH_SOCK`.

```toml
[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]

[lsp_servers.env]
PYTHONPATH = "/custom/path"
VIRTUAL_ENV = "/path/to/venv"
```

**Caution:** setting `PATH` here *replaces* the passthrough value rather than
prepending to it, and the two platforms then diverge — Unix searches your
explicit `PATH` first, so a bare `command` (no directory component) becomes
unresolvable unless your `PATH` entry still contains it; Windows still falls
back to searching the parent's `PATH` afterward. If you only need to add a
directory, prefer an absolute path in `command` over overriding `PATH`.

### `name`

**Type**: String
**Default**: the server's `language_id`

Explicit routing identity for this server. Two servers may share one
`language_id` (e.g. two Python servers), but each must have a distinct
identity — set `name` on at least one of them so they don't collide.

```toml
[[lsp_servers]]
name = "pyright"
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]

[[lsp_servers]]
name = "pylsp"
language_id = "python"
command = "pylsp"
handles = ["diagnostics"]
```

### `handles`

**Type**: Array of tool names
**Default**: unset (catch-all — serves every tool no other server for this language explicitly claims)

Restricts a server to exactly the listed routing values. Valid values:
`hover`, `definition`, `type_definition`, `declaration`, `implementation`, `references`,
`diagnostics`, `rename`, `completions`, `signature_help`,
`document_symbols`, `workspace_symbols`, `format_document`, `format_range`, `code_actions`,
`call_hierarchy`, `type_hierarchy`, `document_highlights`, `inlay_hints`. These are routing identifiers, not MCP tool
names — several MCP tools map to a shorter routing value:

| `handles` value | MCP tool(s) it governs |
|---|---|
| `rename` | `rename_symbol`, `prepare_rename` |
| `workspace_symbols` | `workspace_symbol_search` |
| `implementation` | `go_to_implementation` |
| `document_highlights` | `get_document_highlights` |
| `format_range` | `format_range` |
| `type_definition` | `go_to_type_definition` |
| `declaration` | `go_to_declaration` |
| `call_hierarchy` | `prepare_call_hierarchy`, `get_incoming_calls`, `get_outgoing_calls` (one route: the item `prepare_call_hierarchy` returns is only meaningful to the server that produced it) |
| `type_hierarchy` | `prepare_type_hierarchy`, `get_supertypes`, `get_subtypes` (one route, for the same reason) |
| `diagnostics` | `get_diagnostics` (pull) **and** `get_cached_diagnostics` (the push-notification cache is filtered by the same route, so both are always served by the same server) |

Every other value matches its MCP tool name directly (`hover` → `hover`, etc.).

When a language has no catch-all server, mcpls logs a warning naming the routing values no server claims; those tools then have no server for that language. After upgrading, add `type_hierarchy`, `document_highlights` and `format_range` to a server's list to enable the newer tools there.

At most one server per language may omit `handles` (the catch-all). A tool
may be claimed by only one server per language. In the example above,
`pylsp` handles only diagnostics; `pyright` (the catch-all) handles
everything else for `python`, including `hover`, `definition`, etc.

**Ambiguous configs fail at startup, not silently.** If two servers for one
language are *both applicable in the same workspace* (see
`heuristics` below) and either share a routing identity, both omit
`handles`, or both claim the same tool, mcpls refuses to start and prints an
error naming the conflicting `[[lsp_servers]]` entries. A config with
mutually exclusive `heuristics.project_markers` — where only one of the two
servers is ever applicable in a given workspace — is not ambiguous and
starts normally.

**If the server a tool is routed to fails to spawn**, that tool's requests
move to the language's catch-all server, if one is running; otherwise they
report no server available for that tool rather than silently falling back
to a server that explicitly declined it via `handles`. While the catch-all is
still starting, the tool reports the failed server's startup failure; it is
served by the catch-all once that registers.

Servers start concurrently, and each one is registered, and its languages are
usable, as soon as its own `initialize` completes. A slow or failing server
delays only its own languages, and the order of `[[lsp_servers]]` entries does
not affect when a server becomes usable.

**Exception: `workspace_symbol_search`.** This tool has no document, so it
has no language to route on. It resolves, across all configured servers, to
the first one that explicitly claims `workspace_symbols`, else the first
catch-all. Unlike every document-scoped tool above, there is no per-language
fallback to try (`handles` is per-language, and this tool has no language) —
if neither an explicit claimer nor a catch-all exists anywhere in the
workspace, the request fails naming the tool rather than being forwarded to
an arbitrary server that declined it via `handles`. Add `workspace_symbols`
to a server's `handles` list, or configure a catch-all, to enable this tool.

## Environment Variables

### `MCPLS_CONFIG`

Path to configuration file.

```bash
export MCPLS_CONFIG=/custom/path/to/mcpls.toml
mcpls
```

### `MCPLS_LOG`

Log level for mcpls output.

**Values**: `trace`, `debug`, `info`, `warn`, `error`, `off` (any case), or comma-separated `target=level` directives such as `info,mcpls_core=debug`
**Default**: `info`

An unknown level (for example `debgu`) is rejected at startup instead of silently disabling logging. A bare word must be a level: `mcpls_core` alone is rejected, write `mcpls_core=trace`.

The HTTP session id is a bearer secret, so mcpls caps the rmcp log targets that
print it (`rmcp::transport::streamable_http_server::session` at `warn`,
`rmcp::transport::worker` at `debug`) regardless of this level, and logs only a
short hash of the id itself. A more specific directive (for example
`MCPLS_LOG=info,rmcp::transport::streamable_http_server::session::local=info`)
overrides the cap and puts session ids back into the logs.

```bash
export MCPLS_LOG=debug
mcpls
```

### `MCPLS_LOG_JSON`

Output logs in JSON format.

**Values**: `1`/`0`, `true`/`false`, `yes`/`no`, `y`/`n`, `on`/`off` (case-insensitive)
**Default**: `false`

```bash
export MCPLS_LOG_JSON=true
mcpls
```

### `MCPLS_LISTEN` (transport-http feature)

Bind address for Streamable HTTP transport. When set, mcpls binds this address
instead of using stdio.

```bash
export MCPLS_LISTEN=127.0.0.1:3000
mcpls
```

> [!WARNING]
> **Authentication requirement**: mcpls performs **no authentication** on any transport, including HTTP. When binding to a non-loopback address (e.g., `0.0.0.0:3000`), you **must** place mcpls behind a reverse proxy that enforces authentication before forwarding requests.
>
> The `Host` header must be `localhost`, `127.0.0.1`, `::1` (any port), the bound IP address when it is a specific non-loopback address, or one of the hosts listed with `--http-allowed-host` (`MCPLS_HTTP_ALLOWED_HOSTS`); there is no wildcard, so a `0.0.0.0` bind needs the names clients use listed. Alternatively the reverse proxy can rewrite the `Host` header.
>
> A request carrying an `Origin` header is accepted only when it names `localhost`, `127.0.0.1` or `[::1]` on the bound port, or one of the origins listed with `--http-allowed-origin` (`MCPLS_HTTP_ALLOWED_ORIGINS`); anything else, including `Origin: null`, is answered with `403`. Requests without `Origin` (every non-browser client) are unaffected. Allowed origins do not relax the `Host` check, which runs first, so a browser page reaching mcpls by a non-loopback name needs that name in `--http-allowed-host` as well.
>
> **Example (nginx):**
> ```nginx
> location /mcp/ {
>     auth_request /auth;
>     proxy_pass http://localhost:3000;
>     proxy_set_header Host localhost;
> }
> ```

### `MCPLS_HTTP_PATH` (transport-http feature)

URL prefix the MCP service is mounted at.

The value must start with `/`, must not be `/` (the service already answers at
the root path), must not contain an empty segment (`//` or a trailing `/`) or a
`.`/`..` segment, and may use only ASCII letters, digits and `-._~` in each
segment. An invalid value is rejected with a usage error (exit code 2) before
any language server starts, even when `MCPLS_LISTEN` is not set.

**Default**: `/mcp`

```bash
export MCPLS_HTTP_PATH=/api/mcp
mcpls
```

### `MCPLS_HTTP_ALLOWED_ORIGINS` (transport-http feature)

Extra browser origins accepted by the HTTP transport besides the loopback origins on the bound port; repeat `--http-allowed-origin` or separate values with commas (spaces around a comma are ignored). Each value must be `http://` or `https://` followed by a host and an optional port; a missing port means the scheme default (80 or 443). A path, query, user information, wildcard, `null`, a non-numeric or out-of-range port or an unbracketed IPv6 host is a usage error (exit code 2). The host is matched case-insensitively. The `Host` header check is not affected; see `MCPLS_HTTP_ALLOWED_HOSTS`.

**Default**: none

```bash
export MCPLS_HTTP_ALLOWED_ORIGINS="https://app.example.com,http://[::1]:8080"
mcpls --listen 127.0.0.1:3000
```

### `MCPLS_HTTP_ALLOWED_HOSTS` (transport-http feature)

Extra `Host` header values accepted by the HTTP transport besides `localhost`, `127.0.0.1`, `::1` and the bound IP address when it is a specific non-loopback address; repeat `--http-allowed-host` or separate values with commas (spaces around a comma are ignored). Each value is a host name or IP address with an optional port (`mcp.example.com`, `mcp.example.com:8443`, `[2001:db8::1]:8443`); without a port any port matches, with one only that port does and a request that omits the port does not match. Clients and proxies omit `:80` and `:443`, so a pin to either (or to port 0) is a usage error: list the host without a port. A wildcard, user information, scheme, path, empty port (`host:`), out-of-range port, trailing dot, non-ASCII character (write an internationalized name in punycode, `xn--...`) or unbracketed IPv6 address is a usage error (exit code 2). The host is matched case-insensitively.

**Default**: none

```bash
export MCPLS_HTTP_ALLOWED_HOSTS="mcp.example.com:8443"
mcpls --listen 0.0.0.0:8443
```

### `MCPLS_HTTP_STREAM_LIVENESS` (transport-http feature)

Liveness probing of each session's HTTP GET (SSE) stream: `probe` or `off`.
Only meaningful when `MCPLS_LISTEN` is set.

With `probe`, mcpls sends an MCP `ping` request on the GET stream every 60 s and
closes the stream when the client does not answer (by POSTing the JSON-RPC
response) within 30 s. This frees the stream of a vanished peer and works behind
a reverse proxy, complementing the kernel-level `TCP_USER_TIMEOUT` mcpls sets on
accepted sockets on Linux and Android. A client that ignores server `ping` requests would be
disconnected every 90 s; use `off` for it. The probe interval and deadline are
configurable only when embedding `mcpls-core` (`StreamLiveness`).

A session whose client answers probes on an open GET stream stays alive until
the client closes the stream or sends `DELETE`. With `off` there is no proof of
life, so an open GET stream does not hold its session: the session expires
after 5 minutes without an inbound request, even while the stream receives
notifications. Clients using `off` must send a request (for example `ping`)
more often than that. A POST or request-wise resume response stream is cut
after 1 hour (`ResponseStreamDeadline`, configurable only when embedding),
whatever it is still sending: a stream lifetime bound that lets the session
expire, so a vanished peer no longer pins it. While the server's write to a
peer that stopped reading a response is stuck, the session slot and connection
permit are held until the connection fails (`TCP_USER_TIMEOUT` on Linux and
Android, the OS default elsewhere, or the reverse proxy timeout; #600).

Stateless `subscriptions/listen` streams (MCP 2026-07-28) have no session and a
client cannot answer a server `ping`, so they are bounded by a lease instead:
after a random 15 to 30 minutes the HTTP response ends abruptly, without a final
result, which a client reads as a dropped connection and answers by listening
again. mcpls replays the cached diagnostics URIs plus any clear evicted from the
cache in the last two minutes (at most 256), so nothing is lost across the gap.
A client that never listens again stops receiving push updates after one lease
but can still read resources. `off` disables both the probe and the lease.

**Default**: `probe`

```bash
export MCPLS_HTTP_STREAM_LIVENESS=off
mcpls
```

## Secret Redaction

mcpls hides the values of secret-named environment variables, secret-named
`--flag=value` / `--flag value` arguments and secret-keyed
`initialization_options` strings (names containing `TOKEN`, `KEY`, `SECRET`,
`PASSW`, `CRED` or `AUTH`, case-insensitive; values under 8 bytes are not
redacted) from the text of a server that reaches logs or MCP clients: server
log and show messages, startup and request errors (including malformed-frame
protocol errors), diagnostics (message, `source`, string `code`, related
information and every string value of `data`, in the cache, the diagnostics
resource and `get_diagnostics` pull results), `$/progress` text, trace-level
wire logs, and the spawn argument list (only the count is logged at `info`;
values appear redacted at `debug`). Replacements read `[redacted:NAME]`.

Not redacted: URIs (diagnostic related-location URIs and `codeDescription`
links stay intact because they are cache keys and links), the keys of a
diagnostic's `data` object, and tool results such as hover text, symbol names
and code action titles (tracked in #599).

Matching is by exact value, plus its JSON-escaped and `Debug`-escaped
spellings. A server that re-encodes a secret, for example as a `\uXXXX`
escape for non-ASCII text, as `\/` for a `/`, or as URL or base64 text, can
slip past the redaction in trace-level wire logs; typical ASCII tokens without
a `/` are unaffected.

## Complete Examples

### Rust Project (Zero Config)

mcpls works without configuration for Rust:

```bash
# No configuration needed!
mcpls
```

### Python Project

```toml
[workspace]
roots = ["/Users/username/projects/myapp"]

[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]
timeout_seconds = 45

[lsp_servers.initialization_options]
python.analysis.typeCheckingMode = "basic"
python.analysis.autoSearchPaths = true
```

To use [ty](https://docs.astral.sh/ty/) instead of the default Pyright server:

```toml
[[lsp_servers]]
language_id = "python"
command = "ty"
args = ["server"]
file_patterns = ["**/*.py", "**/*.pyi"]

[lsp_servers.heuristics]
project_markers = ["pyproject.toml", "ty.toml"]
```

To run pyright for everything except diagnostics, and a second server
(`pylsp`) for diagnostics only:

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
args = []
file_patterns = ["**/*.py"]
handles = ["diagnostics"]
```

### TypeScript/JavaScript Project

```toml
[workspace]
roots = ["/Users/username/projects/webapp"]

[[lsp_servers]]
language_id = "typescript"
command = "typescript-language-server"
args = ["--stdio"]
file_patterns = ["**/*.ts", "**/*.tsx", "**/*.js", "**/*.jsx"]

[lsp_servers.initialization_options]
preferences.quotePreference = "single"
preferences.importModuleSpecifierPreference = "relative"
```

### TypeScript 7 (native server)

TypeScript 7 is the native port of the compiler. Its npm package ships no `lib/tsserver.js`, so
`typescript-language-server` cannot use it. You have two options:

1. Keep `typescript-language-server` and install a JavaScript-based TypeScript next to it
   (`npm install -g typescript-language-server typescript@6`). Installing `typescript@6` globally replaces a global TypeScript 7 `tsc`.
2. Run the TypeScript 7 native server, `tsc --lsp --stdio`. Edit the existing `typescript` entry
   of your config file (or remove it first) so that its `command` and `args` are:

```toml
[[lsp_servers]]
language_id = "typescript"
command = "/home/me/ts7/node_modules/.bin/tsc"
args = ["--lsp", "--stdio"]
file_patterns = ["**/*.ts", "**/*.tsx"]
```

Do not add this as a second `typescript` entry next to the default one: two entries for one
language that both omit `handles` are rejected at startup, with a "duplicate server id" error
when both are unnamed and with a "two catch-all servers" error even when the new one has its own
`name`.

Use the absolute path of a TypeScript 7 install **outside the workspace**: `<npm prefix>/bin/tsc`
(`<npm prefix>\tsc.cmd` on Windows). To keep TypeScript 7 next to a TypeScript 6 install, put it
in a separate prefix, for example `npm install --prefix ~/ts7 typescript@7`. A `tsc` from the
workspace's `node_modules`, or a bare `tsc` that `PATH` may resolve into the workspace, is workspace-supplied code that mcpls
would run: see the [Trust model](#trust-model). The native server reports diagnostics through
pull requests only and does not support type hierarchy.

### Go Project

```toml
[workspace]
roots = ["/Users/username/go/src/myproject"]

[[lsp_servers]]
language_id = "go"
command = "gopls"
args = []
file_patterns = ["**/*.go"]

[lsp_servers.initialization_options]
analyses.unusedparams = true
staticcheck = true
```

### Multi-Language Monorepo

```toml
[workspace]
roots = [
    "/Users/username/projects/monorepo/frontend",
    "/Users/username/projects/monorepo/backend",
    "/Users/username/projects/monorepo/cli"
]

# Language extensions (optional - defaults will be used if not specified)
[[language_extensions]]
extensions = ["rs"]
language_id = "rust"

[[language_extensions]]
extensions = ["ts", "tsx"]
language_id = "typescript"

[[language_extensions]]
extensions = ["py", "pyi"]
language_id = "python"

# Rust backend
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/backend/**/*.rs", "**/cli/**/*.rs"]

# TypeScript frontend
[[lsp_servers]]
language_id = "typescript"
command = "typescript-language-server"
args = ["--stdio"]
file_patterns = ["**/frontend/**/*.ts", "**/frontend/**/*.tsx"]

# Python scripts
[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/scripts/**/*.py"]
```

### C/C++ Project

```toml
[workspace]
roots = ["/Users/username/projects/cppproject"]

[[lsp_servers]]
language_id = "cpp"
command = "clangd"
args = ["--background-index", "--clang-tidy"]
file_patterns = ["**/*.cpp", "**/*.cc", "**/*.cxx", "**/*.h", "**/*.hpp"]

[lsp_servers.initialization_options]
compilationDatabasePath = "build"
```

### Custom Language Support (Nushell Example)

```toml
[workspace]
roots = ["/Users/username/projects/scripts"]

# Add Nushell language support
[[language_extensions]]
extensions = ["nu"]
language_id = "nushell"

# Keep Rust support for other scripts
[[language_extensions]]
extensions = ["rs"]
language_id = "rust"

# Shell scripts
[[language_extensions]]
extensions = ["sh", "bash"]
language_id = "shellscript"

# Configure Nushell LSP server
[[lsp_servers]]
language_id = "nushell"
command = "nu"
args = ["--lsp"]
file_patterns = ["**/*.nu"]
timeout_seconds = 30

# rust-analyzer for Rust scripts
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
```

## Command-Line Flags

mcpls supports configuration via command-line flags:

```bash
# Specify config file
mcpls --config /path/to/mcpls.toml

# Set log level
mcpls --log-level debug

# Enable JSON logging
mcpls --log-json

# HTTP transport (requires transport-http feature)
mcpls --listen 127.0.0.1:3000
mcpls --listen 127.0.0.1:3000 --http-path /api/mcp   # must start with "/", not "/" itself
mcpls --listen 127.0.0.1:3000 --http-stream-liveness off

# Show version
mcpls --version

# Show help
mcpls --help
```

## Configuration Validation

Test your configuration:

```bash
# mcpls will validate config on startup
mcpls --log-level debug

# Check for errors in logs
# Valid config will show: "Configuration loaded successfully"
```

Common validation errors:
- Missing required fields (`language_id`, `command`, `file_patterns`)
- Invalid TOML syntax
- Command not found in PATH
- Invalid glob patterns

## Performance Tuning

### Large Projects

For large codebases, increase timeouts:

```toml
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
timeout_seconds = 120         # 2 minutes for initial indexing
request_timeout_seconds = 60  # slower tool-call responses (see the field's docs above for the retry-ceiling math)
```

### Multiple Workspaces

Limit workspace roots to active projects:

```toml
[workspace]
# Don't include entire home directory!
roots = [
    "/Users/username/active-project",
    "/Users/username/dependency-project"
]
```

### Server-Specific Optimizations

#### rust-analyzer

```toml
[lsp_servers.initialization_options]
cargo.features = "all"
checkOnSave.enable = true
checkOnSave.command = "clippy"
files.excludeDirs = ["target", ".git"]  # Skip build artifacts
```

#### pyright

```toml
[lsp_servers.initialization_options]
python.analysis.typeCheckingMode = "basic"  # "strict" is slower
python.analysis.diagnosticMode = "openFilesOnly"  # Faster
```

#### typescript-language-server

```toml
[lsp_servers.initialization_options]
diagnostics.ignoredCodes = [6133, 6192]  # Disable some slow checks
```

## Troubleshooting Configuration

See [Troubleshooting Guide](troubleshooting.md) for common configuration issues.

## Next Steps

- [Getting Started](getting-started.md) - Quick start guide
- [Tools Reference](tools-reference.md) - Available MCP tools
- [Troubleshooting](troubleshooting.md) - Common issues
