//! LSP client implementation.
//!
//! This module provides the LSP client for communicating with language servers
//! over JSON-RPC 2.0.

mod client;
mod lifecycle;
mod process;
mod stderr;
mod transport;
pub(crate) mod tsserver_pin;
pub(crate) mod types;

pub(crate) use client::{CONTENT_MODIFIED_RETRY_METHODS, MAX_ERROR_MESSAGE_CALLER_BYTES};
pub use client::{LspClient, SHUTDOWN_TIMEOUT};
#[cfg(all(test, unix))]
pub(crate) use lifecycle::fake_lsp_server_with_dead_loop_and_live_child;
pub(crate) use lifecycle::{ExitGrace, SUPPORTED_SYMBOL_KINDS, ServerStartOutcome, child_env_var};
pub use lifecycle::{LspServer, ServerInitConfig, ServerInitResult, ServerState};
#[cfg(test)]
pub(crate) use lifecycle::{fake_lsp_server, fake_lsp_server_with_config};
pub use process::LIFELINE_SWEEP_BUDGET;
pub use transport::{LspTransport, LspTransportReader};
pub use types::{
    InboundMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, LspNotification,
    RequestId,
};
