//! MCP server implementation using rmcp.
//!
//! This module provides the MCP server that exposes LSP capabilities
//! as MCP tools using the rmcp SDK.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use super::handlers::HandlerContext;
use super::tools::{
    CachedDiagnosticsParams, CallHierarchyCallsParams, CallHierarchyPrepareParams,
    CodeActionsParams, CompletionsParams, DefinitionParams, DiagnosticsParams,
    DocumentSymbolsParams, FormatDocumentParams, HoverParams, ReferencesParams, RenameParams,
    ServerLogsParams, ServerMessagesParams, WorkspaceSymbolParams,
};
use crate::bridge::Translator;

/// MCP server that exposes LSP capabilities as tools.
#[derive(Clone)]
pub struct McplsServer {
    context: Arc<HandlerContext>,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

#[tool_router]
impl McplsServer {
    /// Create a new MCP server with the given translator.
    #[must_use]
    pub fn new(translator: Arc<Mutex<Translator>>) -> Self {
        let context = Arc::new(HandlerContext::new(translator));
        Self {
            context,
            tool_router: Self::tool_router(),
        }
    }

    /// Get hover information at a position in a file.
    #[tool(description = "Get hover information (type, documentation) at a position in a file")]
    async fn get_hover(&self, params: Parameters<HoverParams>) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_hover(params.0.file_path, params.0.line, params.0.character)
                .await
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Get the definition location of a symbol.
    #[tool(description = "Get the definition location of a symbol at the specified position")]
    async fn get_definition(
        &self,
        params: Parameters<DefinitionParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_definition(params.0.file_path, params.0.line, params.0.character)
                .await
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Find all references to a symbol.
    #[tool(description = "Find all references to a symbol at the specified position")]
    async fn get_references(
        &self,
        params: Parameters<ReferencesParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_references(
                    params.0.file_path,
                    params.0.line,
                    params.0.character,
                    params.0.include_declaration,
                )
                .await
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Get diagnostics for a file.
    #[tool(description = "Get diagnostics (errors, warnings) for a file")]
    async fn get_diagnostics(
        &self,
        params: Parameters<DiagnosticsParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator.handle_diagnostics(params.0.file_path).await
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Rename a symbol across the workspace.
    #[tool(description = "Rename a symbol across the workspace")]
    async fn rename_symbol(&self, params: Parameters<RenameParams>) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_rename(
                    params.0.file_path,
                    params.0.line,
                    params.0.character,
                    params.0.new_name,
                )
                .await
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Get code completion suggestions.
    #[tool(description = "Get code completion suggestions at a position in a file")]
    async fn get_completions(
        &self,
        params: Parameters<CompletionsParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_completions(
                    params.0.file_path,
                    params.0.line,
                    params.0.character,
                    params.0.trigger,
                )
                .await
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Get all symbols in a document.
    #[tool(description = "Get all symbols (functions, classes, variables) in a document")]
    async fn get_document_symbols(
        &self,
        params: Parameters<DocumentSymbolsParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator.handle_document_symbols(params.0.file_path).await
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Format a document according to language server rules.
    #[tool(description = "Format a document according to the language server's formatting rules")]
    async fn format_document(
        &self,
        params: Parameters<FormatDocumentParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_format_document(
                    params.0.file_path,
                    params.0.tab_size,
                    params.0.insert_spaces,
                )
                .await
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Search for symbols across the workspace.
    #[tool(description = "Search for symbols across the entire workspace by name or pattern")]
    async fn workspace_symbol_search(
        &self,
        params: Parameters<WorkspaceSymbolParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_workspace_symbol(params.0.query, params.0.kind_filter, params.0.limit)
                .await
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Get code actions for a range.
    #[tool(
        description = "Get available code actions (quick fixes, refactorings) for a range in a file"
    )]
    async fn get_code_actions(
        &self,
        params: Parameters<CodeActionsParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_code_actions(
                    params.0.file_path,
                    params.0.start_line,
                    params.0.start_character,
                    params.0.end_line,
                    params.0.end_character,
                    params.0.kind_filter,
                )
                .await
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Prepare call hierarchy at a position.
    #[tool(description = "Prepare call hierarchy at a position, returns callable items")]
    async fn prepare_call_hierarchy(
        &self,
        params: Parameters<CallHierarchyPrepareParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_call_hierarchy_prepare(
                    params.0.file_path,
                    params.0.line,
                    params.0.character,
                )
                .await
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Get incoming calls (callers).
    #[tool(description = "Get functions that call the specified item (callers)")]
    async fn get_incoming_calls(
        &self,
        params: Parameters<CallHierarchyCallsParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator.handle_incoming_calls(params.0.item).await
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Get outgoing calls (callees).
    #[tool(description = "Get functions called by the specified item (callees)")]
    async fn get_outgoing_calls(
        &self,
        params: Parameters<CallHierarchyCallsParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator.handle_outgoing_calls(params.0.item).await
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Get cached diagnostics for a file.
    #[tool(description = "Get cached diagnostics for a file from LSP server notifications")]
    async fn get_cached_diagnostics(
        &self,
        params: Parameters<CachedDiagnosticsParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator.handle_cached_diagnostics(&params.0.file_path)
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Get recent LSP server log messages.
    #[tool(description = "Get recent LSP server log messages with optional level filtering")]
    async fn get_server_logs(
        &self,
        params: Parameters<ServerLogsParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator.handle_server_logs(params.0.limit, params.0.min_level)
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Get recent LSP server messages.
    #[tool(description = "Get recent LSP server messages (showMessage notifications)")]
    async fn get_server_messages(
        &self,
        params: Parameters<ServerMessagesParams>,
    ) -> Result<String, McpError> {
        let result = {
            let mut translator = self.context.translator.lock().await;
            translator.handle_server_messages(params.0.limit)
        };

        match result {
            Ok(value) => serde_json::to_string(&value)
                .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }
}

#[tool_handler]
impl ServerHandler for McplsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "mcpls".to_string(),
                title: Some("MCPLS - MCP to LSP Bridge".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
                icons: None,
                website_url: Some("https://github.com/bug-ops/mcpls".to_string()),
            },
            instructions: Some(
                concat!(
                    "Universal MCP to LSP bridge. Exposes Language Server Protocol ",
                    "capabilities as MCP tools for semantic code intelligence. ",
                    "Supports hover, definition, references, diagnostics, rename, ",
                    "completions, symbols, and formatting."
                )
                .to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_server() -> McplsServer {
        let translator = Translator::new();
        McplsServer::new(Arc::new(Mutex::new(translator)))
    }

    #[tokio::test]
    async fn test_server_info() {
        let server = create_test_server();
        let info = server.get_info();

        assert_eq!(info.protocol_version, ProtocolVersion::V_2024_11_05);
        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, "mcpls");
        assert!(info.instructions.is_some());
    }

    #[tokio::test]
    async fn test_hover_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(HoverParams {
            file_path: "/nonexistent/file.rs".to_string(),
            line: 1,
            character: 1,
        });

        // This should return an error (no LSP server configured)
        let result = server.get_hover(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_definition_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(DefinitionParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
        });

        let result = server.get_definition(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_references_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(ReferencesParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
            include_declaration: false,
        });

        let result = server.get_references(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_diagnostics_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(DiagnosticsParams {
            file_path: "/test/file.rs".to_string(),
        });

        let result = server.get_diagnostics(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rename_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(RenameParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
            new_name: "new_name".to_string(),
        });

        let result = server.rename_symbol(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_completions_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(CompletionsParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
            trigger: None,
        });

        let result = server.get_completions(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_document_symbols_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(DocumentSymbolsParams {
            file_path: "/test/file.rs".to_string(),
        });

        let result = server.get_document_symbols(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_format_document_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(FormatDocumentParams {
            file_path: "/test/file.rs".to_string(),
            tab_size: 4,
            insert_spaces: true,
        });

        let result = server.format_document(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_workspace_symbol_search_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(WorkspaceSymbolParams {
            query: "User".to_string(),
            kind_filter: None,
            limit: 100,
        });
        let result = server.workspace_symbol_search(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_code_actions_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(CodeActionsParams {
            file_path: "/test/file.rs".to_string(),
            start_line: 10,
            start_character: 5,
            end_line: 10,
            end_character: 15,
            kind_filter: None,
        });
        let result = server.get_code_actions(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_prepare_call_hierarchy_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(CallHierarchyPrepareParams {
            file_path: "/test/file.rs".to_string(),
            line: 10,
            character: 5,
        });
        let result = server.prepare_call_hierarchy(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_incoming_calls_tool_with_params() {
        let server = create_test_server();
        let item = serde_json::json!({
            "name": "test_function",
            "kind": 12,
            "uri": "file:///test/file.rs",
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            },
            "selectionRange": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            }
        });
        let params = Parameters(CallHierarchyCallsParams { item });
        let result = server.get_incoming_calls(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_outgoing_calls_tool_with_params() {
        let server = create_test_server();
        let item = serde_json::json!({
            "name": "test_function",
            "kind": 12,
            "uri": "file:///test/file.rs",
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            },
            "selectionRange": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            }
        });
        let params = Parameters(CallHierarchyCallsParams { item });
        let result = server.get_outgoing_calls(params).await;
        assert!(result.is_err());
    }
}
