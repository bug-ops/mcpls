//! MCP handler context.
//!
//! This module provides the shared context for MCP tool handlers.
//! The actual tool implementations use the `#[tool]` macro from rmcp
//! and are defined in the `server` module.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use rmcp::task_manager::OperationProcessor;
use tokio::sync::Mutex;

use crate::bridge::{ResourceSubscriptions, Translator};

/// Shared context for all tool handlers.
///
/// Holds the translator and subscription state. The MCP peer handle is not
/// stored here because resource-update notifications are sent by the pump
/// tasks in `lib.rs`, which own their own `Arc<OnceCell<Peer<RoleServer>>>`.
pub struct HandlerContext {
    /// Translator for converting MCP calls to LSP requests.
    pub translator: Arc<Mutex<Translator>>,
    /// Processor for MCP task-augmented tool calls.
    pub task_processor: Arc<Mutex<OperationProcessor>>,
    /// Monotonic task ID source.
    pub task_counter: Arc<AtomicU64>,
    /// Set of resource URIs the MCP client has subscribed to.
    pub subscriptions: Arc<ResourceSubscriptions>,
}

impl HandlerContext {
    /// Create a new handler context.
    #[must_use]
    pub fn new(
        translator: Arc<Mutex<Translator>>,
        subscriptions: Arc<ResourceSubscriptions>,
    ) -> Self {
        Self {
            translator,
            task_processor: Arc::new(Mutex::new(OperationProcessor::new())),
            task_counter: Arc::new(AtomicU64::new(1)),
            subscriptions,
        }
    }

    /// Generate a new server-side task identifier.
    pub fn next_task_id(&self) -> String {
        let id = self.task_counter.fetch_add(1, Ordering::Relaxed);
        format!("mcpls-task-{id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Translator;

    #[test]
    fn test_handler_context_creation() {
        let translator = Arc::new(Mutex::new(Translator::new()));
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let context = HandlerContext::new(translator, subscriptions);
        assert!(Arc::strong_count(&context.translator) == 1);
        assert!(Arc::strong_count(&context.task_processor) == 1);
        assert_eq!(context.next_task_id(), "mcpls-task-1");
        assert_eq!(context.next_task_id(), "mcpls-task-2");
    }
}
