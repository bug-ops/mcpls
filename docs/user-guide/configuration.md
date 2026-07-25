# Configuration Reference

Complete reference for configuring mcpls.

## Configuration File

mcpls uses TOML format for configuration. The file can be placed in several locations (searched in order):

1. Path specified by `--config` flag
2. `$MCPLS_CONFIG` environment variable
3. `./mcpls.toml` (current directory) — **only loaded with `--trust-project-config`** (or
   `MCPLS_TRUST_PROJECT_CONFIG=true`); see [Trusting a Project-Local Config](#trusting-a-project-local-config)
4. `~/.config/mcpls/mcpls.toml` (user config directory)

### Trusting a Project-Local Config

A `mcpls.toml` discovered in the current directory controls which command mcpls
spawns as an LSP server (and other workspace settings), so mcpls does not load it
automatically. Running `mcpls` inside an untrusted checkout must not execute
commands from that checkout without explicit consent.

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

# Optional: LSP server initialization options
[lsp_servers.initialization_options]
cargo.features = "all"
```

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

### `workspace.position_encodings`

**Type**: Array of strings
**Default**: `["utf-8", "utf-16"]`
**Options**: `"utf-8"`, `"utf-16"`, `"utf-32"`

Preferred position encodings for LSP communication.

```toml
[workspace]
position_encodings = ["utf-8", "utf-16", "utf-32"]
```

Most language servers use UTF-16 encoding. mcpls automatically converts between MCP (UTF-8) and LSP encodings.

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

## LSP Server Configuration

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

**Type**: Integer
**Default**: `30`

Timeout in seconds for LSP server operations, including the initial `initialize`
handshake. Servers that load a large project before answering `initialize`
(e.g. OmniSharp on a big Unity/C# solution) need this raised - the default 30 s
can otherwise cut the server off mid-initialization.

```toml
[[lsp_servers]]
timeout_seconds = 60  # Increase for slow servers or large projects
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

### `env`

**Type**: Table (key-value pairs)
**Default**: `{}`

Environment variables to set for the LSP server process.

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
`hover`, `definition`, `type_definition`, `implementation`, `references`,
`diagnostics`, `rename`, `completions`, `signature_help`,
`document_symbols`, `workspace_symbols`, `format_document`, `code_actions`,
`call_hierarchy`, `inlay_hints`. These are routing identifiers, not MCP tool
names — several MCP tools map to a shorter routing value:

| `handles` value | MCP tool(s) it governs |
|---|---|
| `rename` | `rename_symbol` |
| `workspace_symbols` | `workspace_symbol_search` |
| `implementation` | `go_to_implementation` |
| `type_definition` | `go_to_type_definition` |
| `call_hierarchy` | `prepare_call_hierarchy`, `get_incoming_calls`, `get_outgoing_calls` (one route: the item `prepare_call_hierarchy` returns is only meaningful to the server that produced it) |
| `diagnostics` | `get_diagnostics` (pull) **and** `get_cached_diagnostics` (the push-notification cache is filtered by the same route, so both are always served by the same server) |

Every other value matches its MCP tool name directly (`hover` → `hover`, etc.).

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
to a server that explicitly declined it via `handles`.

**Exception: `workspace_symbol_search`.** This tool has no document, so it
has no language to route on. It resolves, across all configured servers, to
the first one that explicitly claims `workspace_symbols`, else the first
catch-all, else — only as a last resort, and only if neither of those exist
— the first live server at all, even one that declined `workspace_symbols`
via `handles`. This last-resort step is intentional (workspace search is
better served imperfectly than not at all) but means `handles` is not an
absolute guarantee for this one tool the way it is for every
document-scoped tool above.

## Environment Variables

### `MCPLS_CONFIG`

Path to configuration file.

```bash
export MCPLS_CONFIG=/custom/path/to/mcpls.toml
mcpls
```

### `MCPLS_LOG`

Log level for mcpls output.

**Values**: `trace`, `debug`, `info`, `warn`, `error`
**Default**: `info`

```bash
export MCPLS_LOG=debug
mcpls
```

### `MCPLS_LOG_JSON`

Output logs in JSON format.

**Values**: `true`, `false`
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

### `MCPLS_HTTP_PATH` (transport-http feature)

URL prefix the MCP service is mounted at.

**Default**: `/mcp`

```bash
export MCPLS_HTTP_PATH=/api/mcp
mcpls
```

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
mcpls --listen 127.0.0.1:3000 --http-path /api/mcp

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
timeout_seconds = 120  # 2 minutes for initial indexing
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
