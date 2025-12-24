//! Translation layer between MCP and LSP protocols.
//!
//! This module handles the bidirectional conversion between
//! MCP tool calls and LSP requests/responses.

mod encoding;
mod state;
mod translator;

pub use encoding::{PositionEncoding, lsp_to_mcp_position, mcp_to_lsp_position};
pub use state::{DocumentState, DocumentTracker};
pub use translator::Translator;
