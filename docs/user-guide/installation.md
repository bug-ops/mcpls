# Installation Guide

Complete installation guide for mcpls - the universal MCP to LSP bridge.

## Prerequisites

- Rust 1.85 or later (for building from source)
- At least one Language Server installed (see [Language Server Setup](#language-server-setup))

## Installation Methods

### Method 1: Cargo Install from crates.io (Recommended)

The easiest way to install mcpls is via cargo:

```bash
cargo install mcpls
```

Verify installation:

```bash
mcpls --version
# Should output: mcpls 0.1.0
```

### Method 2: Pre-Built Binaries from GitHub Releases

Download pre-built binaries for your platform from [GitHub Releases](https://github.com/bug-ops/mcpls/releases).

#### Linux (x86_64)

```bash
# Download latest release
curl -LO https://github.com/bug-ops/mcpls/releases/latest/download/mcpls-v0.1.0-linux-x86_64.tar.gz

# Extract archive
tar xzf mcpls-v0.1.0-linux-x86_64.tar.gz

# Move to system path
sudo mv mcpls /usr/local/bin/

# Verify installation
mcpls --version
```

#### macOS (x86_64 Intel)

```bash
# Download latest release
curl -LO https://github.com/bug-ops/mcpls/releases/latest/download/mcpls-v0.1.0-macos-x86_64.tar.gz

# Extract archive
tar xzf mcpls-v0.1.0-macos-x86_64.tar.gz

# Move to system path
sudo mv mcpls /usr/local/bin/

# Verify installation
mcpls --version
```

#### macOS (Apple Silicon / M1/M2/M3)

```bash
# Download latest release
curl -LO https://github.com/bug-ops/mcpls/releases/latest/download/mcpls-v0.1.0-macos-aarch64.tar.gz

# Extract archive
tar xzf mcpls-v0.1.0-macos-aarch64.tar.gz

# Move to system path
sudo mv mcpls /usr/local/bin/

# Verify installation
mcpls --version
```

#### Windows (x86_64)

1. Download `mcpls-v0.1.0-windows-x86_64.zip` from [GitHub Releases](https://github.com/bug-ops/mcpls/releases)
2. Extract the archive to a directory
3. Add the directory to your PATH environment variable
4. Open a new terminal and verify:

```powershell
mcpls --version
```

### Method 3: Building from Source

For the latest development version or custom builds:

```bash
# Clone repository
git clone https://github.com/bug-ops/mcpls
cd mcpls

# Build and install
cargo install --path crates/mcpls

# Verify installation
mcpls --version
```

### Method 4: Docker

Run mcpls in a Docker container:

```bash
# Pull image from GitHub Container Registry
docker pull ghcr.io/bug-ops/mcpls:latest

# Run mcpls (stdio mode)
docker run -i ghcr.io/bug-ops/mcpls:latest
```

For production use with configuration:

```bash
# Create config file
cat > mcpls.toml <<EOF
[workspace]
roots = ["/workspace"]

[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
EOF

# Run with volume mount
docker run -i \
  -v $(pwd)/mcpls.toml:/etc/mcpls/mcpls.toml:ro \
  -v $(pwd):/workspace:ro \
  ghcr.io/bug-ops/mcpls:latest
```

## Language Server Setup

mcpls requires language servers to be installed separately. Install the language servers for the languages you work with.

### Rust - rust-analyzer

**Installation:**

```bash
# Install via rustup (recommended)
rustup component add rust-analyzer

# Verify installation
rust-analyzer --version
```

**Configuration:** Zero-config for Rust projects. rust-analyzer is the default LSP server.

### Python - pyright

**Installation:**

```bash
# Install via npm
npm install -g pyright

# Verify installation
pyright --version
```

**Configuration:**

Create `~/.config/mcpls/mcpls.toml`:

```toml
[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]
```

### TypeScript/JavaScript - typescript-language-server

**Installation:**

```bash
# Install typescript and language server
npm install -g typescript typescript-language-server

# Verify installation
typescript-language-server --version
```

**Configuration:**

```toml
[[lsp_servers]]
language_id = "typescript"
command = "typescript-language-server"
args = ["--stdio"]
file_patterns = ["**/*.ts", "**/*.tsx", "**/*.js", "**/*.jsx"]
```

### Go - gopls

**Installation:**

```bash
# Install via go
go install golang.org/x/tools/gopls@latest

# Verify installation (ensure $GOPATH/bin is in PATH)
gopls version
```

**Configuration:**

```toml
[[lsp_servers]]
language_id = "go"
command = "gopls"
args = []
file_patterns = ["**/*.go"]
```

### C/C++ - clangd

**Installation:**

```bash
# Ubuntu/Debian
sudo apt install clangd

# Fedora/RHEL
sudo dnf install clangd

# macOS (via Homebrew)
brew install llvm

# Verify installation
clangd --version
```

**Configuration:**

```toml
[[lsp_servers]]
language_id = "cpp"
command = "clangd"
args = []
file_patterns = ["**/*.c", "**/*.cpp", "**/*.h", "**/*.hpp"]
```

### Java - jdtls (Eclipse JDT Language Server)

**Installation:**

1. Download from [Eclipse JDT LS releases](https://download.eclipse.org/jdtls/milestones/)
2. Extract to a directory (e.g., `~/jdtls`)

**Configuration:**

```toml
[[lsp_servers]]
language_id = "java"
command = "/path/to/jdtls/bin/jdtls"
args = []
file_patterns = ["**/*.java"]
```

### Bash - bash-language-server

**Installation:**

```bash
npm install -g bash-language-server

# Verify installation
bash-language-server --version
```

**Configuration:**

```toml
[[lsp_servers]]
language_id = "bash"
command = "bash-language-server"
args = ["start"]
file_patterns = ["**/*.sh", "**/*.bash"]
```

## Configuration

mcpls searches for configuration files in the following order:

1. Path specified by `--config` flag
2. `$MCPLS_CONFIG` environment variable
3. `./mcpls.toml` (current directory)
4. `~/.config/mcpls/mcpls.toml` (user config directory)

### Minimal Configuration

For Rust projects only (uses built-in defaults):

```toml
[workspace]
roots = []  # Auto-detect from current directory
```

### Multi-Language Configuration

Example configuration for a polyglot project:

```toml
[workspace]
roots = [
    "/home/user/projects/myapp"
]

# Rust
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]

# Python
[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]

# TypeScript/JavaScript
[[lsp_servers]]
language_id = "typescript"
command = "typescript-language-server"
args = ["--stdio"]
file_patterns = ["**/*.ts", "**/*.tsx", "**/*.js"]

# Go
[[lsp_servers]]
language_id = "go"
command = "gopls"
args = []
file_patterns = ["**/*.go"]
```

## PATH Configuration

After installation, ensure the installation directory is in your PATH.

### Linux/macOS

If `mcpls --version` doesn't work, add to your shell profile:

```bash
# For Bash (~/.bashrc or ~/.bash_profile)
export PATH="$HOME/.cargo/bin:$PATH"

# For Zsh (~/.zshrc)
export PATH="$HOME/.cargo/bin:$PATH"

# Reload shell configuration
source ~/.bashrc  # or ~/.zshrc
```

### Windows

1. Open System Properties > Advanced > Environment Variables
2. Edit the `Path` variable for your user
3. Add the directory containing `mcpls.exe` (e.g., `C:\Users\YourName\.cargo\bin`)
4. Click OK and restart your terminal

## Upgrading

### Cargo Install

```bash
cargo install mcpls --force
```

### Pre-Built Binaries

Download the latest release and replace the existing binary:

```bash
# Backup current version
sudo mv /usr/local/bin/mcpls /usr/local/bin/mcpls.backup

# Download and install new version
curl -LO https://github.com/bug-ops/mcpls/releases/latest/download/mcpls-v0.2.0-linux-x86_64.tar.gz
tar xzf mcpls-v0.2.0-linux-x86_64.tar.gz
sudo mv mcpls /usr/local/bin/

# Verify upgrade
mcpls --version
```

### Docker

```bash
docker pull ghcr.io/bug-ops/mcpls:latest
```

## Uninstalling

### Cargo Install

```bash
cargo uninstall mcpls
```

### Manual Installation

```bash
# Remove binary
sudo rm /usr/local/bin/mcpls

# Remove configuration (optional)
rm -rf ~/.config/mcpls
```

### Docker

```bash
docker rmi ghcr.io/bug-ops/mcpls:latest
```

## Verifying Installation

After installation, verify mcpls is working correctly:

```bash
# Check version
mcpls --version

# Check help
mcpls --help

# Test with initialize request (manual test)
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}' | mcpls
```

Expected output should be a JSON response with `"method":"initialize"` result.

## Troubleshooting

### "command not found: mcpls"

**Problem:** mcpls binary not in PATH

**Solution:**
1. Check installation directory: `which mcpls` or `where mcpls` (Windows)
2. Add to PATH (see [PATH Configuration](#path-configuration))
3. Try absolute path: `~/.cargo/bin/mcpls --version`

### "failed to compile mcpls"

**Problem:** Rust version too old

**Solution:**
```bash
# Update Rust to 1.85+
rustup update stable
rustup default stable

# Verify version
rustc --version
```

### macOS: "cannot be opened because the developer cannot be verified"

**Problem:** Security restriction on unsigned binaries

**Solution:**
```bash
# Remove quarantine attribute
xattr -d com.apple.quarantine /usr/local/bin/mcpls

# Or right-click > Open in Finder
```

### Windows: "mcpls.exe is not recognized"

**Problem:** Binary not in PATH

**Solution:**
1. Find installation directory: `where mcpls.exe`
2. Add to PATH via System Properties
3. Restart terminal

## Next Steps

After installation:

- [Getting Started Guide](getting-started.md) - Quick start with Claude Code
- [Configuration Reference](configuration.md) - Detailed configuration options
- [Tools Reference](tools-reference.md) - Documentation for all MCP tools
- [Troubleshooting](troubleshooting.md) - Common issues and solutions

## Platform-Specific Notes

### Linux

- Ensure `~/.cargo/bin` is in PATH
- May need `build-essential` for compiling from source
- Some LSP servers require additional dependencies

### macOS

- Both Intel and Apple Silicon binaries available
- May need to allow binary execution in Security & Privacy settings
- Homebrew recommended for installing LSP servers

### Windows

- Use PowerShell or Command Prompt
- May need Visual Studio Build Tools for compiling from source
- WSL2 recommended for best LSP server compatibility

## Support

For installation issues:

1. Check [Troubleshooting Guide](troubleshooting.md)
2. Search [GitHub Issues](https://github.com/bug-ops/mcpls/issues)
3. Open a new issue with:
   - Operating system and version
   - Installation method used
   - Complete error message
   - Output of `mcpls --version` (if partially working)
