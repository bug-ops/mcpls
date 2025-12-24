//! LSP client implementation.
//!
//! This module provides the LSP client for communicating with language servers
//! over JSON-RPC 2.0.

mod client;
mod lifecycle;
mod transport;

pub use client::LspClient;
pub use lifecycle::ServerState;
pub use transport::StdioTransport;
