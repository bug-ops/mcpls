# Language Servers

In this chapter you set up the language servers for the languages you use and learn what to expect from each. mcpls works with any LSP 3.17 server, and six are built in.

## Prerequisites

- mcpls installed and a [configuration file](configuration.md) if you go beyond the defaults.

## Built-in servers

These start with no configuration when their project markers are present:

| Language | Server | Install |
|----------|--------|---------|
| Rust | rust-analyzer | `rustup component add rust-analyzer` |
| Python | pyright | `npm install -g pyright` |
| TypeScript, JavaScript | typescript-language-server | `npm install -g typescript-language-server typescript@6` |
| Go | gopls | `go install golang.org/x/tools/gopls@latest` |
| C, C++ | clangd | `apt install clangd`, `dnf install clangd`, or `brew install llvm` |
| Zig | zls | your package manager |

Confirm each server runs by itself before you debug mcpls, for example `rust-analyzer --version` or `gopls version`.

### TypeScript needs a JavaScript TypeScript

`typescript-language-server` drives a JavaScript `tsserver`, and TypeScript 7 ships none. Install `typescript@6` next to it, as above. If you want the TypeScript 7 native server instead, see [TypeScript: tsserver Pinning and TypeScript 7](../advanced/typescript.md).

## Alternatives and extra servers

Replace or add servers with `[[lsp_servers]]` entries. Each example below is a complete entry.

**Python with ty instead of pyright:**

```toml
[[lsp_servers]]
language_id = "python"
command = "ty"
args = ["server"]
file_patterns = ["**/*.py", "**/*.pyi"]

[lsp_servers.heuristics]
project_markers = ["pyproject.toml", "ty.toml"]
```

**Java with jdtls:**

```toml
[[lsp_servers]]
language_id = "java"
command = "/path/to/jdtls/bin/jdtls"
file_patterns = ["**/*.java"]
```

**Shell scripts with bash-language-server:**

```toml
[[lsp_servers]]
language_id = "shellscript"
command = "bash-language-server"
args = ["start"]
file_patterns = ["**/*.sh", "**/*.bash"]
```

**C and C++ with clangd options:**

```toml
[[lsp_servers]]
language_id = "cpp"
command = "clangd"
args = ["--background-index", "--clang-tidy"]
file_patterns = ["**/*.c", "**/*.cpp", "**/*.cc", "**/*.h", "**/*.hpp"]

[lsp_servers.initialization_options]
compilationDatabasePath = "build"
```

Use the `language_id` from the [default language table](../reference/config.md#default-language-mappings); it is the identifier sent to the server when a file is opened.

## Files with unusual extensions

mcpls knows 30 languages by extension. To teach it another, add a mapping and a server for it:

```toml
[[workspace.language_extensions]]
extensions = ["nu"]
language_id = "nushell"

[[lsp_servers]]
language_id = "nushell"
command = "nu"
args = ["--lsp"]
file_patterns = ["**/*.nu"]
```

Extensions are written without the dot and are case-sensitive. If you supply any `language_extensions`, list every language you need, because your list replaces the defaults.

## Monorepos

`file_patterns` cannot confine a server to a subdirectory, because routing uses the extension only. Scope servers by project instead, with `workspace.roots` and `heuristics.project_markers`:

```toml
[workspace]
roots = ["/work/monorepo/backend", "/work/monorepo/frontend"]

[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
file_patterns = ["**/*.rs"]

[lsp_servers.heuristics]
project_markers = ["Cargo.toml"]

[[lsp_servers]]
language_id = "typescript"
command = "typescript-language-server"
args = ["--stdio"]
file_patterns = ["**/*.ts", "**/*.tsx"]

[lsp_servers.heuristics]
project_markers = ["package.json"]
```

## What servers can and cannot do

Not every server supports every tool. rust-analyzer advertises neither type hierarchy nor range formatting, and typescript-language-server does not advertise type hierarchy. The `get_tool_support` tool reports, per language, which tools are usable, so you can check before you call them.

## What's Next

Now tour [what the 31 tools can do](../tools/overview.md).
