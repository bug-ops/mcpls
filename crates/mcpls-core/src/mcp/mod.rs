//! MCP tool definitions and handlers.
//!
//! This module defines the MCP tools that expose LSP capabilities
//! to AI agents.

mod handlers;
mod server;
mod tools;

pub use server::McplsServer;

// `mcp::tools`'s param structs (e.g. `PositionParams`, `ReferencesParams`)
// are intentionally not re-exported here: every tool handler in `server.rs`
// extracts them via `Parameters<T>` from `super::tools` directly, and no
// in-tree caller ever names one through this module. A caller that needs to
// construct MCP tool arguments should send JSON matching each tool's
// published schema rather than depend on these internal Rust types -- don't
// reintroduce a partial `pub use` list here without a documented reason (see
// #320).
