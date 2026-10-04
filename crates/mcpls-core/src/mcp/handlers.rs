//! MCP handler context.
//!
//! This module provides the shared context for MCP tool handlers.
//! The actual tool implementations use the `#[tool]` macro from rmcp
//! and are defined in the `server` module.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::session::{SessionHandle, SubscriptionRegistry};
use crate::bridge::{NotificationCache, Translator};
use crate::config::McpConfig;

/// Shared context for all tool handlers.
///
/// Holds the translator and the session's subscription handle. `Translator` uses interior
/// mutability (each field locks independently, only for the short section
/// that touches it) so it is shared as a plain `Arc` with no outer lock —
/// this is what lets concurrent tool calls run their LSP round trips without
/// serializing behind a single mutex.
///
/// Per-instance mutable state must live in [`SessionHandle`]; every other
/// field is `Arc`-shared or immutable, so a new field forces an explicit
/// decision in `McplsServer::for_new_session`.
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
    /// This instance's subscription state and delivery handle.
    ///
    /// Scoped to one `McplsServer` instance, which coincides with "per
    /// session" only on rmcp's legacy session path -- on its stateless HTTP
    /// path a fresh instance is built per *request* but can never subscribe
    /// (see [`SessionHandle::require_stateful`]).
    pub session: SessionHandle,
    /// Whether a CWD-discovered `./mcpls.toml` was ignored as untrusted when
    /// the active [`ServerConfig`](crate::config::ServerConfig) was loaded.
    ///
    /// Surfaced in-band via `McplsServer::get_info`'s `RmcpServerConfig.instructions`
    /// (stderr's `tracing::warn!` at load time is typically invisible to an
    /// MCP client).
    pub project_config_ignored: bool,
    /// Configured `serverInfo`/`instructions` presentation overrides, read by
    /// `McplsServer::get_info`.
    pub mcp: McpConfig,
}

impl BridgeContext {
    /// Create a new bridge context with fresh, unregistered session state
    /// that registers into `subscription_registry` on its first subscribe.
    #[must_use]
    pub fn new(
        translator: Arc<Translator>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        workspace_roots: Arc<[PathBuf]>,
        subscription_registry: SubscriptionRegistry,
        project_config_ignored: bool,
        mcp: McpConfig,
    ) -> Self {
        Self {
            translator,
            notification_cache,
            workspace_roots,
            session: SessionHandle::new(subscription_registry),
            project_config_ignored,
            mcp,
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
        let subscription_registry = SubscriptionRegistry::new();
        let context = BridgeContext::new(
            translator,
            notification_cache,
            workspace_roots,
            subscription_registry,
            false,
            McpConfig::default(),
        );
        assert_eq!(Arc::strong_count(&context.translator), 1);
    }
}
