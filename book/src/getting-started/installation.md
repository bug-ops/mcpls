# Installation

In this chapter you install the `mcpls` binary and at least one language server. Both are required: mcpls is the bridge, and the language server does the analysis.

## Prerequisites

- A project in a language that has a language server, such as Rust, Python, TypeScript, Go or C/C++.
- A terminal. Building from source needs Rust 1.99 or later.

## Install mcpls

The installer scripts detect your platform, download the matching release archive, verify its SHA256 checksum and install `mcpls` into a per-user directory. They need no `sudo` or administrator rights.

**Linux and macOS** (installs to `~/.local/bin`):

```bash
curl -fsSL https://raw.githubusercontent.com/bug-ops/mcpls/main/scripts/install.sh | sh
```

**Windows (PowerShell)** (installs to `$HOME\.local\bin`):

```powershell
irm https://raw.githubusercontent.com/bug-ops/mcpls/main/scripts/install.ps1 | iex
```

On Linux and macOS, `MCPLS_INSTALL_DIR` changes the target directory and `MCPLS_VERSION` selects a release tag such as `v0.7.0` instead of the latest.

Verify the install:

```bash
mcpls --version
```

The command prints the version. If your shell reports `command not found`, add the install directory to `PATH` (see [Troubleshooting](../guide/troubleshooting.md#command-not-found-mcpls)).

## Other installation methods

**Cargo**:

```bash
cargo install mcpls
```

**From source**:

```bash
git clone https://github.com/bug-ops/mcpls
cd mcpls
cargo install --path crates/mcpls-cli
```

**Manual download.** Download the archive for your platform from [GitHub Releases](https://github.com/bug-ops/mcpls/releases/latest). Each archive has a `.sha256` file next to it; verify it when you download by hand.

| Platform | Architecture | Archive |
|----------|--------------|---------|
| Linux | x86_64 | `mcpls-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | aarch64 | `mcpls-aarch64-unknown-linux-gnu.tar.gz` |
| macOS | Intel | `mcpls-x86_64-apple-darwin.tar.gz` |
| macOS | Apple Silicon | `mcpls-aarch64-apple-darwin.tar.gz` |
| Windows | x86_64 | `mcpls-x86_64-pc-windows-msvc.zip` |
| Windows | ARM64 | `mcpls-aarch64-pc-windows-msvc.zip` |

**Docker.** The image runs mcpls over stdio and reads its configuration from `/etc/mcpls/mcpls.toml`:

```bash
docker run -i \
  -v "$(pwd)/mcpls.toml:/etc/mcpls/mcpls.toml:ro" \
  -v "$(pwd):/workspace:ro" \
  ghcr.io/bug-ops/mcpls:latest
```

The image contains no language servers, so extend it with the servers you need and list `/workspace` in `workspace.roots`.

> **Note:** The HTTP transport is an optional build feature, not part of the prebuilt binaries or the Docker image. See [Transports](../advanced/transports.md#enabling-the-http-transport) to build it.

## Install a language server

mcpls starts language servers; it does not ship them. Install the one for your language, and make sure its executable is on your `PATH`:

| Language | Install | Executable |
|----------|---------|------------|
| Rust | `rustup component add rust-analyzer` | `rust-analyzer` |
| Python | `npm install -g pyright` | `pyright-langserver` |
| TypeScript, JavaScript | `npm install -g typescript-language-server typescript@6` | `typescript-language-server` |
| Go | `go install golang.org/x/tools/gopls@latest` | `gopls` |
| C, C++ | `apt install clangd` or `brew install llvm` | `clangd` |
| Zig | install `zls` from your package manager | `zls` |

At least one language server must be available. If one fails to start, mcpls keeps running with the others. More languages and details are in [Language Servers](../guide/language-servers.md).

## What's Next

Next, [connect mcpls to an AI client](connect-client.md) so the assistant can call its tools.
