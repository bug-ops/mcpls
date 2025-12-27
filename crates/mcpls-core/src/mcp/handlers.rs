//! MCP tool handlers.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::bridge::Translator;
use crate::error::{Error, Result};
use crate::mcp::tools::{
    CompletionsParams, DefinitionParams, DiagnosticsParams, DocumentSymbolsParams,
    FormatDocumentParams, HoverParams, ReferencesParams, RenameParams, WorkspaceSymbolParams,
};

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

/// Shared context for all tool handlers.
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

/// Handler for the `get_hover` tool.
pub struct HoverHandler {
    context: Arc<HandlerContext>,
}

impl HoverHandler {
    /// Create a new hover handler.
    #[must_use]
    pub const fn new(context: Arc<HandlerContext>) -> Self {
        Self { context }
    }
}

#[async_trait]
impl ToolHandler for HoverHandler {
    async fn handle(&self, params: Value) -> Result<Value> {
        let params: HoverParams = serde_json::from_value(params)
            .map_err(|e| Error::InvalidToolParams(format!("Invalid hover params: {e}")))?;

        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_hover(params.file_path, params.line, params.character)
                .await?
        };

        Ok(serde_json::to_value(result)?)
    }

    fn name(&self) -> &'static str {
        "get_hover"
    }

    fn description(&self) -> &'static str {
        "Get hover information (type, documentation) at a position in a file"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Line number (1-based)"
                },
                "character": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Column/character number (1-based)"
                }
            },
            "required": ["file_path", "line", "character"]
        })
    }
}

/// Handler for the `get_definition` tool.
pub struct DefinitionHandler {
    context: Arc<HandlerContext>,
}

impl DefinitionHandler {
    /// Create a new definition handler.
    #[must_use]
    pub const fn new(context: Arc<HandlerContext>) -> Self {
        Self { context }
    }
}

#[async_trait]
impl ToolHandler for DefinitionHandler {
    async fn handle(&self, params: Value) -> Result<Value> {
        let params: DefinitionParams = serde_json::from_value(params)
            .map_err(|e| Error::InvalidToolParams(format!("Invalid definition params: {e}")))?;

        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_definition(params.file_path, params.line, params.character)
                .await?
        };

        Ok(serde_json::to_value(result)?)
    }

    fn name(&self) -> &'static str {
        "get_definition"
    }

    fn description(&self) -> &'static str {
        "Get the definition location of a symbol at the specified position"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Line number (1-based)"
                },
                "character": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Column/character number (1-based)"
                }
            },
            "required": ["file_path", "line", "character"]
        })
    }
}

/// Handler for the `get_references` tool.
pub struct ReferencesHandler {
    context: Arc<HandlerContext>,
}

impl ReferencesHandler {
    /// Create a new references handler.
    #[must_use]
    pub const fn new(context: Arc<HandlerContext>) -> Self {
        Self { context }
    }
}

#[async_trait]
impl ToolHandler for ReferencesHandler {
    async fn handle(&self, params: Value) -> Result<Value> {
        let params: ReferencesParams = serde_json::from_value(params)
            .map_err(|e| Error::InvalidToolParams(format!("Invalid references params: {e}")))?;

        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_references(
                    params.file_path,
                    params.line,
                    params.character,
                    params.include_declaration,
                )
                .await?
        };

        Ok(serde_json::to_value(result)?)
    }

    fn name(&self) -> &'static str {
        "get_references"
    }

    fn description(&self) -> &'static str {
        "Find all references to a symbol at the specified position"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Line number (1-based)"
                },
                "character": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Column/character number (1-based)"
                },
                "include_declaration": {
                    "type": "boolean",
                    "description": "Whether to include the declaration in the results",
                    "default": false
                }
            },
            "required": ["file_path", "line", "character"]
        })
    }
}

/// Handler for the `get_diagnostics` tool.
pub struct DiagnosticsHandler {
    context: Arc<HandlerContext>,
}

impl DiagnosticsHandler {
    /// Create a new diagnostics handler.
    #[must_use]
    pub const fn new(context: Arc<HandlerContext>) -> Self {
        Self { context }
    }
}

#[async_trait]
impl ToolHandler for DiagnosticsHandler {
    async fn handle(&self, params: Value) -> Result<Value> {
        let params: DiagnosticsParams = serde_json::from_value(params)
            .map_err(|e| Error::InvalidToolParams(format!("Invalid diagnostics params: {e}")))?;

        let result = {
            let mut translator = self.context.translator.lock().await;
            translator.handle_diagnostics(params.file_path).await?
        };

        Ok(serde_json::to_value(result)?)
    }

    fn name(&self) -> &'static str {
        "get_diagnostics"
    }

    fn description(&self) -> &'static str {
        "Get diagnostics (errors, warnings) for a file"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                }
            },
            "required": ["file_path"]
        })
    }
}

/// Handler for the `rename_symbol` tool.
pub struct RenameHandler {
    context: Arc<HandlerContext>,
}

impl RenameHandler {
    /// Create a new rename handler.
    #[must_use]
    pub const fn new(context: Arc<HandlerContext>) -> Self {
        Self { context }
    }
}

#[async_trait]
impl ToolHandler for RenameHandler {
    async fn handle(&self, params: Value) -> Result<Value> {
        let params: RenameParams = serde_json::from_value(params)
            .map_err(|e| Error::InvalidToolParams(format!("Invalid rename params: {e}")))?;

        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_rename(
                    params.file_path,
                    params.line,
                    params.character,
                    params.new_name,
                )
                .await?
        };

        Ok(serde_json::to_value(result)?)
    }

    fn name(&self) -> &'static str {
        "rename_symbol"
    }

    fn description(&self) -> &'static str {
        "Rename a symbol across the workspace"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Line number (1-based)"
                },
                "character": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Column/character number (1-based)"
                },
                "new_name": {
                    "type": "string",
                    "description": "New name for the symbol"
                }
            },
            "required": ["file_path", "line", "character", "new_name"]
        })
    }
}

/// Handler for the `get_completions` tool.
pub struct CompletionsHandler {
    context: Arc<HandlerContext>,
}

impl CompletionsHandler {
    /// Create a new completions handler.
    #[must_use]
    pub const fn new(context: Arc<HandlerContext>) -> Self {
        Self { context }
    }
}

#[async_trait]
impl ToolHandler for CompletionsHandler {
    async fn handle(&self, params: Value) -> Result<Value> {
        let params: CompletionsParams = serde_json::from_value(params)
            .map_err(|e| Error::InvalidToolParams(format!("Invalid completions params: {e}")))?;

        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_completions(
                    params.file_path,
                    params.line,
                    params.character,
                    params.trigger,
                )
                .await?
        };

        Ok(serde_json::to_value(result)?)
    }

    fn name(&self) -> &'static str {
        "get_completions"
    }

    fn description(&self) -> &'static str {
        "Get code completion suggestions at a position in a file"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Line number (1-based)"
                },
                "character": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Column/character number (1-based)"
                },
                "trigger": {
                    "type": "string",
                    "description": "Optional trigger character that invoked completion"
                }
            },
            "required": ["file_path", "line", "character"]
        })
    }
}

/// Handler for the `get_document_symbols` tool.
pub struct DocumentSymbolsHandler {
    context: Arc<HandlerContext>,
}

impl DocumentSymbolsHandler {
    /// Create a new document symbols handler.
    #[must_use]
    pub const fn new(context: Arc<HandlerContext>) -> Self {
        Self { context }
    }
}

#[async_trait]
impl ToolHandler for DocumentSymbolsHandler {
    async fn handle(&self, params: Value) -> Result<Value> {
        let params: DocumentSymbolsParams = serde_json::from_value(params).map_err(|e| {
            Error::InvalidToolParams(format!("Invalid document symbols params: {e}"))
        })?;

        let result = {
            let mut translator = self.context.translator.lock().await;
            translator.handle_document_symbols(params.file_path).await?
        };

        Ok(serde_json::to_value(result)?)
    }

    fn name(&self) -> &'static str {
        "get_document_symbols"
    }

    fn description(&self) -> &'static str {
        "Get all symbols (functions, classes, variables) in a document"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                }
            },
            "required": ["file_path"]
        })
    }
}

/// Handler for the `format_document` tool.
pub struct FormatDocumentHandler {
    context: Arc<HandlerContext>,
}

impl FormatDocumentHandler {
    /// Create a new format document handler.
    #[must_use]
    pub const fn new(context: Arc<HandlerContext>) -> Self {
        Self { context }
    }
}

#[async_trait]
impl ToolHandler for FormatDocumentHandler {
    async fn handle(&self, params: Value) -> Result<Value> {
        let params: FormatDocumentParams = serde_json::from_value(params).map_err(|e| {
            Error::InvalidToolParams(format!("Invalid format document params: {e}"))
        })?;

        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_format_document(params.file_path, params.tab_size, params.insert_spaces)
                .await?
        };

        Ok(serde_json::to_value(result)?)
    }

    fn name(&self) -> &'static str {
        "format_document"
    }

    fn description(&self) -> &'static str {
        "Format a document according to the language server's formatting rules"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file"
                },
                "tab_size": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Number of spaces per tab",
                    "default": 4
                },
                "insert_spaces": {
                    "type": "boolean",
                    "description": "Use spaces instead of tabs",
                    "default": true
                }
            },
            "required": ["file_path"]
        })
    }
}

/// Handler for the `workspace_symbol_search` tool.
pub struct WorkspaceSymbolHandler {
    context: Arc<HandlerContext>,
}

impl WorkspaceSymbolHandler {
    /// Create a new workspace symbol handler.
    #[must_use]
    pub const fn new(context: Arc<HandlerContext>) -> Self {
        Self { context }
    }
}

#[async_trait]
impl ToolHandler for WorkspaceSymbolHandler {
    async fn handle(&self, params: Value) -> Result<Value> {
        let params: WorkspaceSymbolParams = serde_json::from_value(params).map_err(|e| {
            Error::InvalidToolParams(format!("Invalid workspace symbol params: {e}"))
        })?;

        let result = {
            let mut translator = self.context.translator.lock().await;
            translator
                .handle_workspace_symbol(params.query, params.kind_filter, params.limit)
                .await?
        };

        Ok(serde_json::to_value(result)?)
    }

    fn name(&self) -> &'static str {
        "workspace_symbol_search"
    }

    fn description(&self) -> &'static str {
        "Search for symbols across the entire workspace by name or pattern"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query (fuzzy, case-insensitive)"
                },
                "kind_filter": {
                    "type": "string",
                    "description": "Filter by symbol kind (function, class, struct, enum, etc.)",
                    "enum": [
                        "Function", "Method", "Class", "Interface", "Struct",
                        "Enum", "Variable", "Constant", "Module", "Namespace"
                    ]
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 500,
                    "default": 100,
                    "description": "Maximum number of results to return"
                }
            },
            "required": ["query"]
        })
    }
}

/// Factory for creating all tool handlers.
pub struct ToolHandlers {
    handlers: Vec<Box<dyn ToolHandler>>,
}

impl ToolHandlers {
    /// Create all tool handlers with the given translator.
    #[must_use]
    pub fn new(translator: Arc<Mutex<Translator>>) -> Self {
        let context = Arc::new(HandlerContext::new(translator));

        let handlers: Vec<Box<dyn ToolHandler>> = vec![
            Box::new(HoverHandler::new(Arc::clone(&context))),
            Box::new(DefinitionHandler::new(Arc::clone(&context))),
            Box::new(ReferencesHandler::new(Arc::clone(&context))),
            Box::new(DiagnosticsHandler::new(Arc::clone(&context))),
            Box::new(RenameHandler::new(Arc::clone(&context))),
            Box::new(CompletionsHandler::new(Arc::clone(&context))),
            Box::new(DocumentSymbolsHandler::new(Arc::clone(&context))),
            Box::new(FormatDocumentHandler::new(Arc::clone(&context))),
            Box::new(WorkspaceSymbolHandler::new(Arc::clone(&context))),
        ];

        Self { handlers }
    }

    /// Get all registered handlers.
    #[must_use]
    pub fn handlers(&self) -> &[Box<dyn ToolHandler>] {
        &self.handlers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_context() -> Arc<HandlerContext> {
        let translator = Translator::new();
        Arc::new(HandlerContext::new(Arc::new(Mutex::new(translator))))
    }

    #[tokio::test]
    async fn test_hover_handler_invalid_params() {
        let handler = HoverHandler::new(create_test_context());
        let invalid_params = json!({"invalid": "params"});
        let result = handler.handle(invalid_params).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_definition_handler_invalid_params() {
        let handler = DefinitionHandler::new(create_test_context());
        let invalid_params = json!({"file_path": "test.rs"});
        let result = handler.handle(invalid_params).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_references_handler_invalid_params() {
        let handler = ReferencesHandler::new(create_test_context());
        let invalid_params = json!({"file_path": "test.rs", "line": 1});
        let result = handler.handle(invalid_params).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_diagnostics_handler_invalid_params() {
        let handler = DiagnosticsHandler::new(create_test_context());
        let invalid_params = json!({"wrong_field": "value"});
        let result = handler.handle(invalid_params).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_rename_handler_invalid_params() {
        let handler = RenameHandler::new(create_test_context());
        let invalid_params = json!({"file_path": "test.rs", "line": 1, "character": 1});
        let result = handler.handle(invalid_params).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_completions_handler_invalid_params() {
        let handler = CompletionsHandler::new(create_test_context());
        let invalid_params = json!({"file_path": "test.rs"});
        let result = handler.handle(invalid_params).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_document_symbols_handler_invalid_params() {
        let handler = DocumentSymbolsHandler::new(create_test_context());
        let invalid_params = json!({});
        let result = handler.handle(invalid_params).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_format_document_handler_invalid_params() {
        let handler = FormatDocumentHandler::new(create_test_context());
        let invalid_params = json!({"tab_size": 4});
        let result = handler.handle(invalid_params).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_workspace_symbol_handler_invalid_params() {
        let handler = WorkspaceSymbolHandler::new(create_test_context());
        let invalid_params = json!({});
        let result = handler.handle(invalid_params).await;
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[test]
    fn test_handler_metadata() {
        let context = create_test_context();

        let hover = HoverHandler::new(Arc::clone(&context));
        assert_eq!(hover.name(), "get_hover");
        assert!(!hover.description().is_empty());
        assert!(hover.schema().is_object());

        let definition = DefinitionHandler::new(Arc::clone(&context));
        assert_eq!(definition.name(), "get_definition");

        let references = ReferencesHandler::new(Arc::clone(&context));
        assert_eq!(references.name(), "get_references");

        let diagnostics = DiagnosticsHandler::new(Arc::clone(&context));
        assert_eq!(diagnostics.name(), "get_diagnostics");

        let rename = RenameHandler::new(Arc::clone(&context));
        assert_eq!(rename.name(), "rename_symbol");

        let completions = CompletionsHandler::new(Arc::clone(&context));
        assert_eq!(completions.name(), "get_completions");

        let symbols = DocumentSymbolsHandler::new(Arc::clone(&context));
        assert_eq!(symbols.name(), "get_document_symbols");

        let format = FormatDocumentHandler::new(Arc::clone(&context));
        assert_eq!(format.name(), "format_document");

        let workspace_symbol = WorkspaceSymbolHandler::new(Arc::clone(&context));
        assert_eq!(workspace_symbol.name(), "workspace_symbol_search");
    }
}
