//! LSP client implementation.
//!
//! This module provides the LSP client for communicating with language servers
//! over JSON-RPC 2.0.

mod client;
mod file_watcher;
mod lifecycle;
mod transport;
mod types;

pub use client::{LspClient, ServerRequest};
pub use file_watcher::FileWatcher;
pub use lifecycle::{LspServer, ServerInitConfig, ServerInitResult, ServerState};
pub use transport::LspTransport;
pub use types::{
    InboundMessage, JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    LspNotification, RequestId,
};
