//! MCP handler context.
//!
//! This module provides the shared context for MCP tool handlers.
//! The actual tool implementations use the `#[tool]` macro from rmcp
//! and are defined in the `server` module.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::bridge::{NotificationCache, ResourceSubscriptions, Translator};

/// Shared context for all tool handlers.
///
/// Holds the translator and subscription state. `Translator` uses interior
/// mutability (each field locks independently, only for the short section
/// that touches it) so it is shared as a plain `Arc` with no outer lock —
/// this is what lets concurrent tool calls run their LSP round trips without
/// serializing behind a single mutex.
///
/// The MCP peer handle is not stored here because resource-update
/// notifications are sent by the pump tasks in `lib.rs`, which own their own
/// `Arc<OnceCell<Peer<RoleServer>>>`.
pub struct BridgeContext {
    /// Translator for converting MCP calls to LSP requests.
    pub translator: Arc<Translator>,
    /// Cache of pushed LSP notifications (diagnostics, logs, messages).
    ///
    /// Locked independently of `translator`, which itself holds no outer
    /// lock, so the `diagnostics_pump` task never contends with a tool call
    /// running an in-flight LSP round-trip.
    pub notification_cache: Arc<Mutex<NotificationCache>>,
    /// Workspace roots, fixed at startup and immutable thereafter.
    ///
    /// Shared as a lock-free snapshot so cache-only handlers (e.g.
    /// `get_cached_diagnostics`, `read_resource`) can validate a path without
    /// locking anything.
    pub workspace_roots: Arc<[PathBuf]>,
    /// Set of resource URIs the MCP client has subscribed to.
    pub subscriptions: Arc<ResourceSubscriptions>,
    /// Whether a CWD-discovered `./mcpls.toml` was ignored as untrusted when
    /// the active [`ServerConfig`](crate::config::ServerConfig) was loaded.
    ///
    /// Surfaced in-band via `McplsServer::get_info`'s `ServerInfo.instructions`
    /// (stderr's `tracing::warn!` at load time is typically invisible to an
    /// MCP client).
    pub project_config_ignored: bool,
}

impl BridgeContext {
    /// Create a new bridge context.
    #[must_use]
    pub const fn new(
        translator: Arc<Translator>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        workspace_roots: Arc<[PathBuf]>,
        subscriptions: Arc<ResourceSubscriptions>,
        project_config_ignored: bool,
    ) -> Self {
        Self {
            translator,
            notification_cache,
            workspace_roots,
            subscriptions,
            project_config_ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Translator;

    #[test]
    fn test_bridge_context_creation() {
        let translator = Arc::new(Translator::new());
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let workspace_roots: Arc<[PathBuf]> = Arc::from(Vec::new());
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let context = BridgeContext::new(
            translator,
            notification_cache,
            workspace_roots,
            subscriptions,
            false,
        );
        assert_eq!(Arc::strong_count(&context.translator), 1);
    }
}
