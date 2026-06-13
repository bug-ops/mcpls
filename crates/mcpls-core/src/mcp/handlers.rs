//! MCP handler context.
//!
//! This module provides the shared context for MCP tool handlers.
//! The actual tool implementations use the `#[tool]` macro from rmcp
//! and are defined in the `server` module.

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
    /// Shared cache for diagnostics, logs, and server messages.
    pub notification_cache: Arc<Mutex<NotificationCache>>,
    /// Set of resource URIs the MCP client has subscribed to.
    pub subscriptions: Arc<ResourceSubscriptions>,
}

impl HandlerContext {
    /// Create a new handler context.
    #[must_use]
    pub const fn new(
        translator: Arc<Mutex<Translator>>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        subscriptions: Arc<ResourceSubscriptions>,
    ) -> Self {
        Self {
            translator,
            notification_cache,
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
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let context = HandlerContext::new(translator, notification_cache, subscriptions);
        assert!(Arc::strong_count(&context.translator) == 1);
        assert!(Arc::strong_count(&context.notification_cache) == 1);
    }
}
