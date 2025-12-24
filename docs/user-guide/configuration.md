# Configuration Reference

Complete reference for configuring mcpls.

## Configuration File

mcpls uses TOML format for configuration. The file can be placed in several locations (searched in order):

1. Path specified by `--config` flag
2. `$MCPLS_CONFIG` environment variable
3. `./mcpls.toml` (current directory)
4. `~/.config/mcpls/mcpls.toml` (user config directory)

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

Timeout in seconds for LSP server operations.

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

## Command-Line Flags

mcpls supports configuration via command-line flags:

```bash
# Specify config file
mcpls --config /path/to/mcpls.toml

# Set log level
mcpls --log-level debug

# Enable JSON logging
mcpls --log-json

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
