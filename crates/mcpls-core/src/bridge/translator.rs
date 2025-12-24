//! MCP to LSP translation layer.

use crate::lsp::LspClient;
use std::collections::HashMap;

use super::DocumentTracker;

/// Translator handles MCP tool calls by converting them to LSP requests.
#[derive(Debug)]
pub struct Translator {
    /// LSP clients indexed by language ID.
    lsp_clients: HashMap<String, LspClient>,
    /// Document state tracker.
    document_tracker: DocumentTracker,
}

impl Translator {
    /// Create a new translator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lsp_clients: HashMap::new(),
            document_tracker: DocumentTracker::new(),
        }
    }

    /// Register an LSP client for a language.
    pub fn register_client(&mut self, language_id: String, client: LspClient) {
        self.lsp_clients.insert(language_id, client);
    }

    /// Get the document tracker.
    #[must_use]
    pub fn document_tracker(&self) -> &DocumentTracker {
        &self.document_tracker
    }

    /// Get a mutable reference to the document tracker.
    pub fn document_tracker_mut(&mut self) -> &mut DocumentTracker {
        &mut self.document_tracker
    }

    // TODO: These methods will be implemented in Phase 3-5
    // Initialize and shutdown are now handled by LspServer in lifecycle.rs

    // Future implementation will use LspServer instead of LspClient directly
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}
