//! MCP handler context.
//!
//! This module provides the shared context for MCP tool handlers.
//! The actual tool implementations use the `#[tool]` macro from rmcp
//! and are defined in the `server` module.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::bridge::Translator;

/// Shared context for all tool handlers.
///
/// This struct holds the translator that converts MCP tool calls
/// to LSP requests and is shared across all tool implementations.
pub struct HandlerContext {
    /// Translator for converting MCP calls to LSP requests.
    pub translator: Arc<Mutex<Translator>>,
}

impl HandlerContext {
    /// Create a new handler context.
    #[must_use]
    pub const fn new(translator: Arc<Mutex<Translator>>) -> Self {
        Self { translator }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Translator;

    #[test]
    fn test_handler_context_creation() {
        let translator = Translator::new();
        let context = HandlerContext::new(Arc::new(Mutex::new(translator)));
        // Context should be created successfully
        assert!(Arc::strong_count(&context.translator) == 1);
    }
}
