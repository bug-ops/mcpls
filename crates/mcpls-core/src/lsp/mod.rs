//! LSP client implementation.
//!
//! This module provides the LSP client for communicating with language servers
//! over JSON-RPC 2.0.

mod client;
pub(crate) mod command_path;
mod env;
pub(crate) mod launcher;
mod lifecycle;
mod process;
mod publish_mailbox;
mod stderr;
mod transport;
pub(crate) mod tsserver_pin;
pub(crate) mod types;

pub(crate) use client::{
    CONTENT_MODIFIED_RETRY_METHODS, ConnectionId, MAX_ERROR_MESSAGE_CALLER_BYTES, UnclassifiedError,
};
pub use client::{LspClient, MAX_CONSECUTIVE_UNDECODABLE_FRAMES, SHUTDOWN_TIMEOUT};
pub use command_path::HostOs;
pub(crate) use env::{ManagedEnvVar, ParentEnv, process_env};
#[cfg(test)]
#[cfg(unix)]
pub(crate) use lifecycle::fake_lsp_server_with_dead_loop_and_live_child;
pub use lifecycle::{ChildWorkingDir, LspServer, ServerInitConfig, ServerState};
pub(crate) use lifecycle::{
    ExitGrace, SUPPORTED_SYMBOL_KINDS, ServerStartOutcome, child_env_var, current_environment,
};
#[cfg(test)]
pub(crate) use lifecycle::{fake_lsp_server, fake_lsp_server_with_config};
pub use process::LIFELINE_SWEEP_BUDGET;
pub use publish_mailbox::{
    BoundedPublish, LostFiles, NotificationInbox, PublishDelivery, PublishReader, ServerMessage,
};
pub(crate) use publish_mailbox::{DropCounter, DropLog, Lane, NotificationSink};
#[cfg(test)]
pub(crate) use publish_mailbox::{MailboxLimits, PublishWriter, mailbox};
pub use transport::{LspTransport, LspTransportReader};
pub use types::{
    InboundMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, LspNotification,
    RequestId,
};
