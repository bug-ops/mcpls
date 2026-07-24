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
/// Holds the translator and subscription state. The MCP peer handle is not
/// stored here because resource-update notifications are sent by the pump
/// tasks in `lib.rs`, which own their own `Arc<OnceCell<Peer<RoleServer>>>`.
pub struct HandlerContext {
    /// Translator for converting MCP calls to LSP requests.
    pub translator: Arc<Mutex<Translator>>,
    /// Cache of pushed LSP notifications (diagnostics, logs, messages).
    ///
    /// Locked independently of `translator` so the `diagnostics_pump` task
    /// never contends with a tool call holding the translator lock across an
    /// in-flight LSP round-trip.
    pub notification_cache: Arc<Mutex<NotificationCache>>,
    /// Workspace roots, fixed at startup and immutable thereafter.
    ///
    /// Shared as a lock-free snapshot so cache-only handlers (e.g.
    /// `get_cached_diagnostics`, `read_resource`) can validate a path without
    /// locking `translator`, which may be held elsewhere across a slow
    /// in-flight LSP round-trip.
    pub workspace_roots: Arc<[PathBuf]>,
    /// Set of resource URIs the MCP client has subscribed to.
    pub subscriptions: Arc<ResourceSubscriptions>,
}

impl HandlerContext {
    /// Create a new handler context.
    #[must_use]
    pub const fn new(
        translator: Arc<Mutex<Translator>>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        workspace_roots: Arc<[PathBuf]>,
        subscriptions: Arc<ResourceSubscriptions>,
    ) -> Self {
        Self {
            translator,
            notification_cache,
            workspace_roots,
            subscriptions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Translator;

    #[test]
    fn test_handler_context_creation() {
        let translator = Arc::new(Mutex::new(Translator::new()));
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let workspace_roots: Arc<[PathBuf]> = Arc::from(Vec::new());
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let context = HandlerContext::new(
            translator,
            notification_cache,
            workspace_roots,
            subscriptions,
        );
        assert_eq!(Arc::strong_count(&context.translator), 1);
    }
}
