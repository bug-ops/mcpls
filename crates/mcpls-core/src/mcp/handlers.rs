//! MCP tool handlers.

use crate::error::Result;
use async_trait::async_trait;
use serde_json::Value;

/// Trait for handling MCP tool calls.
#[async_trait]
pub trait ToolHandler: Send + Sync {
    /// Handle a tool call and return the result.
    async fn handle(&self, params: Value) -> Result<Value>;

    /// Get the tool name.
    fn name(&self) -> &'static str;

    /// Get the tool description.
    fn description(&self) -> &'static str;

    /// Get the JSON schema for the tool parameters.
    fn schema(&self) -> Value;
}
