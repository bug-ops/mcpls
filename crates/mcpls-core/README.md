# mcpls-core

[![Crates.io](https://img.shields.io/crates/v/mcpls-core)](https://crates.io/crates/mcpls-core)
[![docs.rs](https://img.shields.io/docsrs/mcpls-core)](https://docs.rs/mcpls-core)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](https://github.com/bug-ops/mcpls/blob/main/LICENSE-MIT)

**The translation layer that lets AI agents understand code semantically.**

mcpls-core is the library behind [mcpls](https://crates.io/crates/mcpls). It turns MCP tool calls into language server requests, manages the language server processes, and translates the answers back. Use it to embed the bridge in your own application; to just use mcpls, install the [`mcpls`](https://crates.io/crates/mcpls) binary.

User documentation: <https://bug-ops.github.io/mcpls/>. API documentation: <https://docs.rs/mcpls-core>.

## Installation

```toml
[dependencies]
mcpls-core = "0.7"
```

Enable the `transport-http` feature to serve over Streamable HTTP:

```toml
[dependencies]
mcpls-core = { version = "0.7", features = ["transport-http"] }
```

## Usage

```rust,ignore
use mcpls_core::{ServerConfig, Transport};

#[tokio::main]
async fn main() {
    let config = ServerConfig::load().expect("failed to load config");
    let result = mcpls_core::serve_with(config, Transport::Stdio).await;
    // Exit explicitly: the stdio reader parks an uncancellable thread, so
    // returning from `main` can hang on SIGTERM. See `serve_with`'s docs.
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}
```

## Features

- Protocol translation between MCP tools and LSP requests, with 1-based to 0-based position and encoding conversion
- Language server lifecycle management: spawn, initialize, restart, shutdown
- Non-blocking startup: the MCP server accepts connections while language servers initialize
- Lazy document tracking and a diagnostics cache for push-based notifications
- TOML configuration, server discovery by project markers, and trust controls
- stdio and optional HTTP transports

## Learn more

- [Architecture](https://bug-ops.github.io/mcpls/advanced/architecture.html)
- [Configuration reference](https://bug-ops.github.io/mcpls/reference/config.html)
- [Transports](https://bug-ops.github.io/mcpls/advanced/transports.html)
- [Security and trust](https://bug-ops.github.io/mcpls/advanced/security.html)

## License

Dual-licensed under [Apache 2.0](https://github.com/bug-ops/mcpls/blob/main/LICENSE-APACHE) or [MIT](https://github.com/bug-ops/mcpls/blob/main/LICENSE-MIT).
