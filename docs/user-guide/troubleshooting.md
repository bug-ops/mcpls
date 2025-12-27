# Troubleshooting Guide

Common issues and solutions when using mcpls.

## Table of Contents

- [Installation Issues](#installation-issues)
- [Claude Code Integration](#claude-code-integration)
- [LSP Server Issues](#lsp-server-issues)
- [Configuration Issues](#configuration-issues)
- [Performance Issues](#performance-issues)
- [Common Error Messages](#common-error-messages)
- [Getting Help](#getting-help)

---

## Installation Issues

### "command not found: mcpls"

**Problem**: mcpls binary not in PATH after `cargo install`

**Solution**:
```bash
# Add Cargo bin directory to PATH
export PATH="$HOME/.cargo/bin:$PATH"

# For permanent fix, add to ~/.bashrc or ~/.zshrc
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

**Verify**:
```bash
which mcpls
# Should output: /Users/username/.cargo/bin/mcpls
```

### "failed to compile mcpls"

**Problem**: Rust version too old

**Solution**:
```bash
# Update to Rust 1.85 or later
rustup update stable
rustc --version
# Should output: rustc 1.85.0 or higher
```

**Problem**: Missing build dependencies

**Solution** (Linux):
```bash
# Ubuntu/Debian
sudo apt update
sudo apt install build-essential pkg-config libssl-dev

# Fedora/RHEL
sudo dnf install gcc pkg-config openssl-devel
```

### "error: could not find Cargo.toml"

**Problem**: Not in the project directory

**Solution**:
```bash
# Clone the repository first
git clone https://github.com/bug-ops/mcpls
cd mcpls

# Then install
cargo install --path crates/mcpls-cli
```

---

## Claude Code Integration

### mcpls not showing up in Claude Code

**Checklist**:
1. Verify mcpls is installed: `mcpls --version`
2. Check MCP configuration file exists
   - macOS/Linux: `~/.claude/mcp.json`
   - Windows: `%APPDATA%\Claude\mcp.json`
3. Verify JSON syntax is valid (no trailing commas)
4. Restart Claude Code completely (quit and reopen)
5. Check Claude Code logs for errors

**Example valid configuration**:
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

**Invalid configurations**:
```json
{
  "mcpServers": {
    "mcpls": {
      "command": "mcpls",
      "args": [],  // ❌ Trailing comma!
    },  // ❌ Trailing comma!
  }
}
```

### "Failed to start MCP server"

**Problem**: mcpls binary not found or not executable

**Solution**:
```bash
# Find the mcpls binary
which mcpls

# If found, use absolute path in config
{
  "mcpServers": {
    "mcpls": {
      "command": "/Users/username/.cargo/bin/mcpls",
      "args": []
    }
  }
}
```

**Test manually**:
```bash
# Test mcpls stdio communication
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}}' | mcpls
```

Expected output should include:
```json
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05",...}}
```

### Tools available but not working

**Problem**: LSP server not configured or not installed

**Symptoms**:
- Claude sees tools in the list
- Tool calls return "LSP server not available for file type"

**Solution**:

1. Install required language server:
```bash
# For Rust
rustup component add rust-analyzer

# For Python
npm install -g pyright

# For TypeScript
npm install -g typescript-language-server
```

2. Verify language server works:
```bash
rust-analyzer --version
pyright --version
typescript-language-server --version
```

3. Configure in `~/.config/mcpls/mcpls.toml` if needed (Rust works zero-config)

---

## LSP Server Issues

### "LSP server not available for file type"

**Problem**: No LSP server configured for the file extension

**Solution**:

Create `~/.config/mcpls/mcpls.toml`:
```toml
[[lsp_servers]]
language_id = "python"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]
```

**Verify configuration**:
```bash
mcpls --log-level debug
# Check logs for "Registered LSP server for language: python"
```

### "LSP server timeout"

**Problem**: Language server taking too long to respond

**Symptoms**:
- First requests are slow
- Large projects time out
- Tools return timeout errors

**Solution 1**: Increase timeout in configuration:
```toml
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
timeout_seconds = 120  # Increase from default 30
```

**Solution 2**: Wait for initial indexing to complete:
```bash
# rust-analyzer needs time to index on first run
# Monitor with debug logging
mcpls --log-level debug
```

**Solution 3**: Reduce workspace size:
```toml
[workspace]
# Limit to active project only
roots = ["/Users/username/current-project"]
```

### "rust-analyzer indexing takes forever"

**Problem**: Large codebase with many dependencies

**Symptoms**:
- High CPU usage on first run
- Slow response times
- Timeout errors

**Solutions**:

1. **Wait for initial indexing** (one-time cost):
```bash
# Tail logs to monitor progress
mcpls --log-level info 2>&1 | grep "rust-analyzer"
```

2. **Exclude build artifacts**:
```toml
[lsp_servers.initialization_options]
files.excludeDirs = ["target", ".git", "node_modules"]
```

3. **Disable on-save checking temporarily**:
```toml
[lsp_servers.initialization_options]
checkOnSave.enable = false
```

4. **Close unnecessary workspaces**:
```toml
[workspace]
# Don't include entire home directory!
roots = ["/Users/username/active-project"]
```

### "LSP server crashed"

**Problem**: Language server process died unexpectedly

**Symptoms**:
- Tools suddenly stop working
- "Server connection closed" errors
- Need to restart mcpls

**Debug steps**:

1. Check server logs:
```bash
mcpls --log-level debug 2>&1 | tee mcpls-debug.log
```

2. Test server manually:
```bash
# For rust-analyzer
rust-analyzer --help

# For pyright
pyright-langserver --help
```

3. Update language server:
```bash
# rust-analyzer
rustup update
rustup component add rust-analyzer

# pyright
npm update -g pyright
```

4. Report bug to language server maintainers if reproducible

---

## Configuration Issues

### "Configuration file not found"

**Problem**: mcpls not finding `mcpls.toml`

**Debug**:
```bash
# Check searched locations
mcpls --log-level debug 2>&1 | grep "config"
```

**Solution 1**: Specify config explicitly:
```bash
mcpls --config /path/to/mcpls.toml
```

**Solution 2**: Set environment variable:
```bash
export MCPLS_CONFIG=/path/to/mcpls.toml
mcpls
```

**Solution 3**: Place in default location:
```bash
mkdir -p ~/.config/mcpls
cp mcpls.toml ~/.config/mcpls/
```

### "Invalid configuration: missing field"

**Problem**: TOML syntax error or missing required field

**Common mistakes**:
```toml
# ❌ Missing required fields
[[lsp_servers]]
command = "rust-analyzer"
# Missing: language_id, file_patterns

# ✅ Correct
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
```

**Solution**: Validate TOML syntax:
```bash
# Use online TOML validator
# Or check with mcpls debug mode
mcpls --config mcpls.toml --log-level debug
```

### "Command not found: rust-analyzer"

**Problem**: Language server not in PATH

**Solution 1**: Install language server:
```bash
rustup component add rust-analyzer
```

**Solution 2**: Use absolute path:
```toml
[[lsp_servers]]
command = "/Users/username/.rustup/toolchains/stable-x86_64-apple-darwin/bin/rust-analyzer"
```

**Solution 3**: Add to PATH:
```bash
export PATH="$HOME/.rustup/toolchains/stable-x86_64-apple-darwin/bin:$PATH"
```

---

## Performance Issues

### mcpls using too much memory

**Problem**: Multiple LSP servers or large workspace

**Symptoms**:
- High memory usage (>500MB)
- System slowdown
- Out of memory errors

**Solutions**:

1. **Configure only needed language servers**:
```toml
# Don't configure servers you don't use
[[lsp_servers]]
language_id = "rust"  # Only if working with Rust
# ...
```

2. **Limit workspace roots**:
```toml
[workspace]
# Only active projects
roots = ["/Users/username/current-project"]
```

3. **Restart mcpls periodically**:
```bash
# If using with Claude, restart Claude Code
# Or restart mcpls if running standalone
```

4. **Exclude large directories**:
```toml
[lsp_servers.initialization_options]
files.excludeDirs = ["target", "node_modules", ".git", "dist"]
```

### Slow response times

**Problem**: Cold start or large files

**Symptoms**:
- First request takes >5 seconds
- Subsequent requests fast
- Tools time out

**Solutions**:

1. **Increase timeout**:
```toml
[[lsp_servers]]
timeout_seconds = 60
```

2. **Pre-warm LSP server**:
```bash
# Keep mcpls running between requests
# Don't restart for every interaction
```

3. **Enable debug logging** to identify bottleneck:
```bash
mcpls --log-level debug 2>&1 | grep "duration\|took\|elapsed"
```

4. **Check system resources**:
```bash
# Monitor CPU and memory
top -pid $(pgrep mcpls)
```

### High CPU usage

**Problem**: Language server indexing or checking

**Temporary solutions**:
```toml
[lsp_servers.initialization_options]
# For rust-analyzer
checkOnSave.enable = false  # Disable cargo check on save

# For pyright
python.analysis.diagnosticMode = "openFilesOnly"
```

**Long-term solution**: Wait for indexing to complete (one-time)

---

## Common Error Messages

### "Document not found"

**Cause**: File path not in workspace or doesn't exist

**Fix**:
1. Ensure file exists: `ls -la /path/to/file`
2. Verify file is in workspace roots
3. Use absolute path, not relative path

### "No client available for language"

**Cause**: No LSP server configured for file extension

**Fix**: Add LSP server configuration for that language

**Example**:
```toml
[[lsp_servers]]
language_id = "go"
command = "gopls"
args = []
file_patterns = ["**/*.go"]
```

### "Position out of bounds"

**Cause**: Line/character position exceeds file content

**Fix**:
1. Verify line number is valid (1-based indexing)
2. Verify character is within line length
3. Remember: character is UTF-8 code points, not bytes

**Example**:
```rust
// File with 10 lines
get_hover(file, line: 15, ...)  // ❌ Line 15 doesn't exist
get_hover(file, line: 5, ...)   // ✅ Valid
```

### "Internal error: failed to parse LSP response"

**Cause**: LSP server returned invalid JSON or unexpected format

**Debug**:
```bash
# Enable trace logging
mcpls --log-level trace 2>&1 | tee mcpls-trace.log
# Look for malformed JSON in logs
```

**Solutions**:
1. Update language server to latest version
2. Check for server bugs or incompatibilities
3. Report issue to mcpls maintainers with trace logs

### "Failed to initialize LSP server"

**Cause**: Server startup failed or initialization timeout

**Debug**:
```bash
# Test server manually
rust-analyzer --help  # Should show help message

# Check initialization options
mcpls --log-level debug 2>&1 | grep "initialization"
```

**Solutions**:
1. Verify server is installed and executable
2. Check initialization_options in config
3. Increase timeout
4. Remove invalid initialization options

---

## Getting Help

### Before asking for help

1. **Enable debug logging**:
```bash
mcpls --log-level debug 2>&1 | tee mcpls-debug.log
```

2. **Collect system information**:
```bash
mcpls --version
rust-analyzer --version  # or other LSP server
rustc --version
uname -a  # OS info
```

3. **Verify configuration**:
```bash
cat ~/.config/mcpls/mcpls.toml
```

4. **Test minimal example**:
```bash
# Create minimal config
cat > test-mcpls.toml <<EOF
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
EOF

mcpls --config test-mcpls.toml --log-level debug
```

### Where to get help

1. **GitHub Issues**: https://github.com/bug-ops/mcpls/issues
   - Search existing issues first
   - Include debug logs and configuration
   - Provide minimal reproduction steps

2. **GitHub Discussions**: https://github.com/bug-ops/mcpls/discussions
   - For questions and general help
   - Community support

3. **Documentation**:
   - [Getting Started](getting-started.md)
   - [Configuration Reference](configuration.md)
   - [Tools Reference](tools-reference.md)

### Reporting bugs

When reporting bugs, include:

```bash
# System information
mcpls --version
rust-analyzer --version  # or other LSP server
rustc --version
uname -a

# Configuration
cat ~/.config/mcpls/mcpls.toml

# Debug logs (run command that fails)
mcpls --log-level trace 2>&1 | tee bug-report.log

# Minimal reproduction steps
echo "1. Create file test.rs with content: ..."
echo "2. Run: mcpls ..."
echo "3. Expected: ..."
echo "4. Actual: ..."
```

### Feature requests

For feature requests, include:
- **Use case**: What problem are you trying to solve?
- **Proposed solution**: How should it work?
- **Alternatives**: What workarounds exist?
- **Examples**: Show example configuration or usage

---

## Advanced Debugging

### Enable trace logging

Maximum verbosity for debugging:
```bash
export MCPLS_LOG=trace
mcpls 2>&1 | tee trace.log
```

### Test LSP server directly

Bypass mcpls to test LSP server:
```bash
# Start rust-analyzer
rust-analyzer

# Send initialize request (JSON-RPC)
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"processId":null,"rootUri":"file:///path/to/project","capabilities":{}}}
```

### Test MCP protocol

Test mcpls MCP implementation:
```bash
# Send MCP initialize
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}}' | mcpls

# List tools
echo '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' | mcpls
```

### Monitor file changes

Watch configuration file:
```bash
# macOS
fswatch ~/.config/mcpls/mcpls.toml | xargs -n1 echo "Config changed:"

# Linux
inotifywait -m ~/.config/mcpls/mcpls.toml
```

### Network debugging

If using TCP transport (future feature):
```bash
# Monitor network traffic
tcpdump -i lo0 -A port 8080

# Test with netcat
nc localhost 8080
```

---

## Quick Reference

### Restart everything

```bash
# 1. Kill any running mcpls processes
pkill mcpls

# 2. Clear any cached state (if applicable)
rm -rf ~/.cache/mcpls  # Future feature

# 3. Restart Claude Code
# Quit and reopen Claude Code application

# 4. Verify clean start
mcpls --version
```

### Reset configuration

```bash
# Backup existing config
cp ~/.config/mcpls/mcpls.toml ~/.config/mcpls/mcpls.toml.backup

# Start with minimal config
cat > ~/.config/mcpls/mcpls.toml <<EOF
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["**/*.rs"]
EOF
```

### Check logs

```bash
# Recent errors
mcpls --log-level error 2>&1 | tail -20

# All debug output
mcpls --log-level debug 2>&1 | less

# JSON logs for parsing
mcpls --log-json 2>&1 | jq
```

---

## Next Steps

- [Getting Started](getting-started.md) - Quick start guide
- [Configuration](configuration.md) - Detailed configuration
- [Tools Reference](tools-reference.md) - MCP tools documentation
