//! MCP server implementation using rmcp.
//!
//! This module provides the MCP server that exposes LSP capabilities
//! as MCP tools using the rmcp SDK.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    Implementation, ListResourcesResult, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ResourceUpdatedNotificationParam,
    ServerCapabilities, ServerInfo, SubscribeRequestParams, UnsubscribeRequestParams,
};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use super::handlers::BridgeContext;
use super::tools::{
    CachedDiagnosticsParams, CallHierarchyCallsParams, CallHierarchyPrepareParams,
    CodeActionsParams, CompletionsParams, DefinitionParams, DiagnosticsParams,
    DocumentSymbolsParams, FormatDocumentParams, GoToImplementationParams,
    GoToTypeDefinitionParams, HoverParams, InlayHintsParams, PositionParams, RangeParams,
    ReferencesParams, RenameParams, ServerLogsParams, ServerMessagesParams, SignatureHelpParams,
    WorkspaceSymbolParams,
};
use crate::bridge::resources::{make_uri, parse_uri};
use crate::bridge::{
    NotificationCache, ResourceSubscriptions, Translator, validate_path_against_roots,
};

/// MCP server that exposes LSP capabilities as tools.
#[derive(Clone)]
pub struct McplsServer {
    context: Arc<BridgeContext>,
}

/// Map a bridge-layer result to the MCP tool response shape shared by every `#[tool]` handler.
fn to_tool_result<T: serde::Serialize>(
    result: crate::error::Result<T>,
) -> Result<String, McpError> {
    match result {
        Ok(value) => serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None)),
        Err(e) => Err(McpError::internal_error(e.to_string(), None)),
    }
}

#[tool_router]
impl McplsServer {
    /// Create a new MCP server with the given translator, notification cache,
    /// workspace roots, and subscriptions.
    ///
    /// `project_config_ignored` reports whether a CWD-discovered
    /// `./mcpls.toml` was skipped as untrusted when the active config was
    /// loaded (see [`ServerConfig::project_config_ignored`](crate::config::ServerConfig::project_config_ignored));
    /// `get_info` surfaces it in [`ServerInfo::instructions`].
    #[must_use]
    pub fn new(
        translator: Arc<Translator>,
        notification_cache: Arc<Mutex<NotificationCache>>,
        workspace_roots: Arc<[PathBuf]>,
        subscriptions: Arc<ResourceSubscriptions>,
        project_config_ignored: bool,
    ) -> Self {
        let context = Arc::new(BridgeContext::new(
            translator,
            notification_cache,
            workspace_roots,
            subscriptions,
            project_config_ignored,
        ));
        Self { context }
    }

    /// Get hover information at a position in a file.
    #[tool(
        description = "Type and documentation info at position. Returns signatures, docs, and inferred types for symbols.",
        title = "Hover",
        annotations(
            title = "Hover",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_hover(
        &self,
        Parameters(HoverParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
        }): Parameters<HoverParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_hover(file_path, line, character)
                .await
        };

        to_tool_result(result)
    }

    /// Get the definition location of a symbol.
    #[tool(
        description = "Definition location of symbol at position. Returns file path, line, and character where declared.",
        title = "Go to Definition",
        annotations(
            title = "Go to Definition",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_definition(
        &self,
        Parameters(DefinitionParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
        }): Parameters<DefinitionParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_definition(file_path, line, character)
                .await
        };

        to_tool_result(result)
    }

    /// Find all references to a symbol.
    #[tool(
        description = "All references to symbol at position. Returns locations across workspace where symbol is used.",
        title = "Find References",
        annotations(
            title = "Find References",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_references(
        &self,
        Parameters(ReferencesParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
            include_declaration,
        }): Parameters<ReferencesParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_references(file_path, line, character, include_declaration)
                .await
        };

        to_tool_result(result)
    }

    /// Get diagnostics for a file.
    #[tool(
        description = "Diagnostics for a file. Returns errors, warnings, and hints with severity and location.",
        title = "Diagnostics",
        annotations(
            title = "Diagnostics",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_diagnostics(
        &self,
        Parameters(DiagnosticsParams { file_path }): Parameters<DiagnosticsParams>,
    ) -> Result<String, McpError> {
        // Merging push-model (flycheck/clippy) diagnostics into the pull
        // result, including the pull-error-but-cache-has-data fallback, is
        // handled inside handle_diagnostics itself -- see its doc comment.
        let result = self
            .context
            .translator
            .handle_diagnostics(file_path, &self.context.notification_cache)
            .await;

        to_tool_result(result)
    }

    /// Rename a symbol across the workspace.
    // read-only: returns a proposed WorkspaceEdit, does not apply it -- mcpls
    // has no write-back path today; revisit if that changes.
    #[tool(
        description = "Rename symbol across workspace. Returns text edits for all files where symbol is used.",
        title = "Rename Symbol",
        annotations(
            title = "Rename Symbol",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn rename_symbol(
        &self,
        Parameters(RenameParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
            new_name,
        }): Parameters<RenameParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_rename(file_path, line, character, new_name)
                .await
        };

        to_tool_result(result)
    }

    /// Get code completion suggestions.
    #[tool(
        description = "Completion suggestions at position. Returns methods, functions, variables, types, and snippets.",
        title = "Completions",
        annotations(
            title = "Completions",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_completions(
        &self,
        Parameters(CompletionsParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
            trigger,
        }): Parameters<CompletionsParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_completions(file_path, line, character, trigger)
                .await
        };

        to_tool_result(result)
    }

    /// Get all symbols in a document.
    #[tool(
        description = "Symbols in a file. Returns hierarchical outline with functions, classes, structs, and locations.",
        title = "Document Symbols",
        annotations(
            title = "Document Symbols",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_document_symbols(
        &self,
        Parameters(DocumentSymbolsParams { file_path }): Parameters<DocumentSymbolsParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_document_symbols(file_path)
                .await
        };

        to_tool_result(result)
    }

    /// Format a document according to language server rules.
    // read-only: returns proposed text edits, does not apply them -- mcpls
    // has no write-back path today; revisit if that changes.
    #[tool(
        description = "Format document with language-specific rules. Returns text edits for indentation, spacing, and style.",
        title = "Format Document",
        annotations(
            title = "Format Document",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn format_document(
        &self,
        Parameters(FormatDocumentParams {
            file_path,
            tab_size,
            insert_spaces,
        }): Parameters<FormatDocumentParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_format_document(file_path, tab_size, insert_spaces)
                .await
        };

        to_tool_result(result)
    }

    /// Search for symbols across the workspace.
    #[tool(
        description = "Search workspace symbols by name. Supports partial matching and fuzzy search.",
        title = "Workspace Symbol Search",
        annotations(
            title = "Workspace Symbol Search",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn workspace_symbol_search(
        &self,
        Parameters(WorkspaceSymbolParams {
            query,
            kind_filter,
            limit,
        }): Parameters<WorkspaceSymbolParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_workspace_symbol(query, kind_filter, limit)
                .await
        };

        to_tool_result(result)
    }

    /// Get code actions for a range.
    // read-only: returns proposed CodeAction edits, does not apply them --
    // mcpls has no write-back path today; revisit if that changes.
    #[tool(
        description = "Code actions for range. Returns quick fixes, refactorings, and source actions with edits.",
        title = "Code Actions",
        annotations(
            title = "Code Actions",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_code_actions(
        &self,
        Parameters(CodeActionsParams {
            file_path,
            range:
                RangeParams {
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                },
            kind_filter,
        }): Parameters<CodeActionsParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_code_actions(
                    file_path,
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                    kind_filter,
                )
                .await
        };

        to_tool_result(result)
    }

    /// Prepare call hierarchy at a position.
    #[tool(
        description = "Prepare call hierarchy at position. Returns callable items for incoming/outgoing call analysis.",
        title = "Prepare Call Hierarchy",
        annotations(
            title = "Prepare Call Hierarchy",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn prepare_call_hierarchy(
        &self,
        Parameters(CallHierarchyPrepareParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
        }): Parameters<CallHierarchyPrepareParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_call_hierarchy_prepare(file_path, line, character)
                .await
        };

        to_tool_result(result)
    }

    /// Get incoming calls (callers).
    #[tool(
        description = "Functions calling the specified item. Takes call hierarchy item, returns all callers.",
        title = "Incoming Calls",
        annotations(
            title = "Incoming Calls",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_incoming_calls(
        &self,
        Parameters(CallHierarchyCallsParams { item }): Parameters<CallHierarchyCallsParams>,
    ) -> Result<String, McpError> {
        let result = { self.context.translator.handle_incoming_calls(item).await };

        to_tool_result(result)
    }

    /// Get outgoing calls (callees).
    #[tool(
        description = "Functions called by the specified item. Takes call hierarchy item, returns all callees.",
        title = "Outgoing Calls",
        annotations(
            title = "Outgoing Calls",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_outgoing_calls(
        &self,
        Parameters(CallHierarchyCallsParams { item }): Parameters<CallHierarchyCallsParams>,
    ) -> Result<String, McpError> {
        let result = { self.context.translator.handle_outgoing_calls(item).await };

        to_tool_result(result)
    }

    /// Get cached diagnostics for a file.
    #[tool(
        description = "Cached diagnostics from server notifications. Faster than get_diagnostics, no new analysis.",
        title = "Cached Diagnostics",
        annotations(
            title = "Cached Diagnostics",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_cached_diagnostics(
        &self,
        Parameters(CachedDiagnosticsParams { file_path }): Parameters<CachedDiagnosticsParams>,
    ) -> Result<String, McpError> {
        let result =
            match Translator::cached_diagnostics_uri(&self.context.workspace_roots, &file_path) {
                Ok(uri) => {
                    // Lock only long enough for the map lookup + clone: no
                    // canonicalize() or Vec mapping while `notification_cache`
                    // is held, since `diagnostics_pump` needs the same lock.
                    let diag_info = {
                        let cache = self.context.notification_cache.lock().await;
                        cache.get_diagnostics(&uri).cloned()
                    };
                    Ok(Translator::diagnostics_from_cache_entry(diag_info.as_ref()))
                }
                Err(e) => Err(e),
            };

        to_tool_result(result)
    }

    /// Get recent LSP server log messages.
    #[tool(
        description = "Recent server log messages. Filter by level (error, warning, info, debug) for debugging.",
        title = "Server Logs",
        annotations(
            title = "Server Logs",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_server_logs(
        &self,
        Parameters(ServerLogsParams { limit, min_level }): Parameters<ServerLogsParams>,
    ) -> Result<String, McpError> {
        let result = {
            let cache = self.context.notification_cache.lock().await;
            Translator::handle_server_logs(&cache, limit, min_level)
        };

        to_tool_result(result)
    }

    /// Get recent LSP server messages.
    #[tool(
        description = "Recent server messages (showMessage notifications). User-facing prompts and status updates.",
        title = "Server Messages",
        annotations(
            title = "Server Messages",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_server_messages(
        &self,
        Parameters(ServerMessagesParams { limit }): Parameters<ServerMessagesParams>,
    ) -> Result<String, McpError> {
        let result = {
            let cache = self.context.notification_cache.lock().await;
            Translator::handle_server_messages(&cache, limit)
        };

        to_tool_result(result)
    }

    /// Get signature help at a position.
    #[tool(
        description = "Signature help at position. Returns parameter info, active signature/parameter, and documentation while typing a call.",
        title = "Signature Help",
        annotations(
            title = "Signature Help",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_signature_help(
        &self,
        Parameters(SignatureHelpParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
        }): Parameters<SignatureHelpParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_signature_help(file_path, line, character)
                .await
        };

        to_tool_result(result)
    }

    /// Go to implementation locations.
    #[tool(
        description = "Implementation locations of trait method or interface member at position.",
        title = "Go to Implementation",
        annotations(
            title = "Go to Implementation",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn go_to_implementation(
        &self,
        Parameters(GoToImplementationParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
        }): Parameters<GoToImplementationParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_implementation(file_path, line, character)
                .await
        };

        to_tool_result(result)
    }

    /// Go to type definition location.
    #[tool(
        description = "Type definition location of expression at position. Distinct from go-to-definition for variable bindings.",
        title = "Go to Type Definition",
        annotations(
            title = "Go to Type Definition",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn go_to_type_definition(
        &self,
        Parameters(GoToTypeDefinitionParams {
            position:
                PositionParams {
                    file_path,
                    line,
                    character,
                },
        }): Parameters<GoToTypeDefinitionParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_type_definition(file_path, line, character)
                .await
        };

        to_tool_result(result)
    }

    /// Get inlay hints for a range.
    #[tool(
        description = "Inlay hints in range. Returns inferred type/parameter annotations the editor would render inline.",
        title = "Inlay Hints",
        annotations(
            title = "Inlay Hints",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn get_inlay_hints(
        &self,
        Parameters(InlayHintsParams {
            file_path,
            range:
                RangeParams {
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                },
        }): Parameters<InlayHintsParams>,
    ) -> Result<String, McpError> {
        let result = {
            self.context
                .translator
                .handle_inlay_hints(
                    file_path,
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                )
                .await
        };

        to_tool_result(result)
    }
}

#[tool_handler]
impl ServerHandler for McplsServer {
    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        // TODO(critic-S5): paginate when max_documents == 0 (unlimited mode can produce
        // very large single-page responses that may exceed transport buffers).
        let open_paths = self.context.translator.open_document_paths();
        let resources: Vec<_> = open_paths
            .iter()
            .filter_map(|path| {
                let uri = make_uri(path)
                    .inspect_err(|e| {
                        tracing::warn!(
                            "Skipping path in list_resources (make_uri failed): {}: {e}",
                            path.display()
                        );
                    })
                    .ok()?;
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                Some(
                    Resource::new(uri, name)
                        .with_mime_type("application/json")
                        .with_description("LSP diagnostics for this file"),
                )
            })
            .collect();

        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let path =
            parse_uri(&request.uri).map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Enforce workspace-root containment — mirrors the guard in every LSP tool.
        // Validated against a lock-free snapshot of workspace_roots (fixed at
        // startup) so this cache-only read never needs to touch `translator` at all.
        let validated_path = validate_path_against_roots(&path, &self.context.workspace_roots)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Build the URI from the canonicalized path (not the raw input path):
        // it must match what `diagnostics_pump` stores from LSP notifications,
        // which are always keyed by the canonical form.
        let lsp_uri = crate::bridge::path_to_uri(&validated_path)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // TODO(critic-S2): distinguish "file not tracked" from "file tracked but clean"
        // in the response shape. Currently both return `{"diagnostics":null}` which is
        // ambiguous for clients that need to know whether analysis has run yet.
        let diagnostics = {
            let cache = self.context.notification_cache.lock().await;
            cache.get_diagnostics(lsp_uri.as_str()).cloned()
        };

        let json = serde_json::to_string(&diagnostics)
            .map_err(|e| McpError::internal_error(format!("Serialization error: {e}"), None))?;

        Ok(ReadResourceResult::new(vec![ResourceContents::text(json, request.uri)]).into())
    }

    /// When cached diagnostics exist, the replay notification is flushed to the client
    /// before this call returns its own response; this is legal per JSON-RPC/MCP, which
    /// permits notifications to interleave with in-flight requests, so a conformant
    /// client must demultiplex by request `id` rather than assume response-before-notification ordering.
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let path =
            parse_uri(&request.uri).map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Enforce workspace-root containment (same invariant as every LSP tool).
        // Validated against a lock-free snapshot of workspace_roots so subscribing
        // never needs to touch `translator` at all (see `read_resource`).
        let validated_path = validate_path_against_roots(&path, &self.context.workspace_roots)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Track and reply under the canonical resource URI, not the client's raw
        // `request.uri`: `diagnostics_pump` derives `mcp_uri` from the canonical LSP
        // path (see below), so a subscription keyed by a non-canonical but equivalent
        // URI (symlink, macOS /var vs /private/var, ...) would never match its
        // `subs.contains` check and silently stop receiving pushes.
        let canonical_uri =
            make_uri(&validated_path).map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Record the subscription *before* checking the cache. This closes the race where
        // a PublishDiagnostics notification lands between the cache check and the
        // subscription being recorded: if diagnostics arrive before this point, the check
        // below catches them; if they arrive after, `diagnostics_pump`'s own
        // `subs.contains` check already sees this URI as subscribed and delivers the
        // update through the normal push path.
        self.context
            .subscriptions
            .subscribe(canonical_uri.clone())
            .await
            .map_err(|e| McpError::invalid_params(e, None))?;

        // Build the URI from the canonicalized path, matching `read_resource` and
        // what `diagnostics_pump` stores from LSP notifications.
        let lsp_uri = crate::bridge::path_to_uri(&validated_path)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let has_cached_diagnostics = {
            let cache = self.context.notification_cache.lock().await;
            cache.get_diagnostics(lsp_uri.as_str()).is_some()
        };

        if has_cached_diagnostics
            && let Err(e) = context
                .peer
                .notify_resource_updated(ResourceUpdatedNotificationParam::new(
                    canonical_uri.clone(),
                ))
                .await
        {
            tracing::warn!("Failed to replay cached diagnostics for {canonical_uri}: {e}");
        }

        Ok(())
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        // Parse the URI for consistency with subscribe validation.
        let path =
            parse_uri(&request.uri).map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Remove under the same canonical URI `subscribe` recorded under. Best-effort
        // fall back to the raw URI if canonicalization fails (e.g. the file was
        // deleted since subscribing) so unsubscribing a stale entry never errors.
        let key = validate_path_against_roots(&path, &self.context.workspace_roots)
            .ok()
            .and_then(|validated_path| make_uri(&validated_path).ok())
            .unwrap_or_else(|| request.uri.clone());

        self.context.subscriptions.unsubscribe(&key).await;
        Ok(())
    }

    fn get_info(&self) -> ServerInfo {
        let mut implementation = Implementation::new("mcpls", env!("CARGO_PKG_VERSION"));
        implementation.title = Some("MCPLS - MCP to LSP Bridge".to_string());
        implementation.description = Some(env!("CARGO_PKG_DESCRIPTION").to_string());
        implementation.website_url = Some("https://github.com/bug-ops/mcpls".to_string());

        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_resources_subscribe()
            .build();
        let mut server_info = ServerInfo::new(capabilities);
        server_info.server_info = implementation;
        let mut instructions = concat!(
            "Universal MCP to LSP bridge. Exposes Language Server Protocol ",
            "capabilities as MCP tools for semantic code intelligence. ",
            "Supports hover, definition, references, diagnostics, rename, ",
            "completions, symbols, and formatting."
        )
        .to_string();

        if self.context.project_config_ignored {
            instructions.push_str(
                " NOTE: a project-local mcpls.toml was found in the current directory but \
                 ignored as untrusted; the server is running on built-in defaults or a global \
                 config instead. If this repository is trusted, restart mcpls with \
                 --trust-project-config (or MCPLS_TRUST_PROJECT_CONFIG=true) to load it.",
            );
        }
        server_info.instructions = Some(instructions);

        server_info
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn create_test_server() -> McplsServer {
        create_test_server_with_ignored_flag(false)
    }

    fn create_test_server_with_ignored_flag(project_config_ignored: bool) -> McplsServer {
        let translator = Arc::new(Translator::new());
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let workspace_roots: Arc<[PathBuf]> = Arc::from(Vec::new());
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        McplsServer::new(
            translator,
            notification_cache,
            workspace_roots,
            subscriptions,
            project_config_ignored,
        )
    }

    #[tokio::test]
    async fn test_server_info() {
        let server = create_test_server();
        let info = server.get_info();

        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, "mcpls");
        assert!(info.instructions.is_some());
    }

    #[tokio::test]
    async fn test_server_info_omits_ignore_notice_when_not_ignored() {
        let server = create_test_server_with_ignored_flag(false);
        let info = server.get_info();

        assert!(!info.instructions.unwrap().contains("ignored as untrusted"));
    }

    #[tokio::test]
    async fn test_server_info_surfaces_ignored_project_config() {
        let server = create_test_server_with_ignored_flag(true);
        let info = server.get_info();

        let instructions = info.instructions.unwrap();
        assert!(instructions.contains("ignored as untrusted"));
        assert!(instructions.contains("--trust-project-config"));
    }

    #[tokio::test]
    async fn test_hover_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(HoverParams {
            position: PositionParams {
                file_path: "/nonexistent/file.rs".to_string(),
                line: 1,
                character: 1,
            },
        });

        // This should return an error (no LSP server configured)
        let result = server.get_hover(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_definition_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(DefinitionParams {
            position: PositionParams {
                file_path: "/test/file.rs".to_string(),
                line: 10,
                character: 5,
            },
        });

        let result = server.get_definition(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_references_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(ReferencesParams {
            position: PositionParams {
                file_path: "/test/file.rs".to_string(),
                line: 10,
                character: 5,
            },
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
            position: PositionParams {
                file_path: "/test/file.rs".to_string(),
                line: 10,
                character: 5,
            },
            new_name: "new_name".to_string(),
        });

        let result = server.rename_symbol(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_completions_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(CompletionsParams {
            position: PositionParams {
                file_path: "/test/file.rs".to_string(),
                line: 10,
                character: 5,
            },
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
            range: RangeParams {
                start_line: 10,
                start_character: 5,
                end_line: 10,
                end_character: 15,
            },
            kind_filter: None,
        });
        let result = server.get_code_actions(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_prepare_call_hierarchy_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(CallHierarchyPrepareParams {
            position: PositionParams {
                file_path: "/test/file.rs".to_string(),
                line: 10,
                character: 5,
            },
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

    #[tokio::test]
    async fn test_cached_diagnostics_tool_with_params() {
        use std::fs;

        use tempfile::TempDir;

        let server = create_test_server();

        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let params = Parameters(CachedDiagnosticsParams {
            file_path: test_file.to_str().unwrap().to_string(),
        });

        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.get("diagnostics").is_some());
    }

    /// `get_cached_diagnostics` end-to-end: a cache entry stored under the
    /// canonical URI (as `diagnostics_pump` would store it) must be found when
    /// requested via a textually non-canonical path -- proving `cached_diagnostics_uri`
    /// still canonicalizes correctly after the lock-scope split, and that
    /// `diagnostics_from_cache_entry` correctly maps a populated entry through
    /// the actual tool call (not just the unit-level helpers directly).
    #[tokio::test]
    async fn test_cached_diagnostics_tool_finds_entry_via_noncanonical_path() {
        use std::fs;

        use tempfile::TempDir;
        use url::Url;

        let server = create_test_server();

        let temp_dir = TempDir::new().unwrap();
        let subdir = temp_dir.path().join("sub");
        fs::create_dir(&subdir).unwrap();
        let test_file = subdir.join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let canonical_path = test_file.canonicalize().unwrap();
        let uri: lsp_types::Uri = Url::from_file_path(&canonical_path)
            .unwrap()
            .as_str()
            .parse()
            .unwrap();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: None,
            message: "cached error".to_string(),
            related_information: None,
            tags: None,
            data: None,
        };
        {
            let mut cache = server.context.notification_cache.lock().await;
            cache.store_diagnostics(&uri, Some(1), vec![diagnostic]);
        }

        // Textually distinct from `test_file`, but canonicalizes to the same path.
        let noncanonical = subdir.join("..").join("sub").join("test.rs");
        let params = Parameters(CachedDiagnosticsParams {
            file_path: noncanonical.to_str().unwrap().to_string(),
        });

        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let diagnostics = parsed.get("diagnostics").unwrap().as_array().unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].get("message").unwrap(), "cached error");
    }

    #[tokio::test]
    async fn test_cached_diagnostics_tool_nonexistent_file() {
        let server = create_test_server();
        let params = Parameters(CachedDiagnosticsParams {
            file_path: "/nonexistent/file.rs".to_string(),
        });

        let result = server.get_cached_diagnostics(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_default_params() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 50,
            min_level: None,
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.get("logs").is_some());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_error_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 10,
            min_level: Some("error".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let logs = parsed.get("logs").unwrap().as_array().unwrap();
        assert_eq!(logs.len(), 0);
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_warning_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 100,
            min_level: Some("warning".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_info_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 50,
            min_level: Some("info".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_debug_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 20,
            min_level: Some("debug".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_invalid_level() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 10,
            min_level: Some("invalid_level".to_string()),
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_server_logs_tool_with_zero_limit() {
        let server = create_test_server();
        let params = Parameters(ServerLogsParams {
            limit: 0,
            min_level: None,
        });

        let result = server.get_server_logs(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let logs = parsed.get("logs").unwrap().as_array().unwrap();
        assert_eq!(logs.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_default_params() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 20 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(parsed.get("messages").is_some());
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_custom_limit() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 5 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let messages = parsed.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_zero_limit() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 0 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());

        let json_str = result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let messages = parsed.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 0);
    }

    #[tokio::test]
    async fn test_server_messages_tool_with_large_limit() {
        let server = create_test_server();
        let params = Parameters(ServerMessagesParams { limit: 1000 });

        let result = server.get_server_messages(params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_get_signature_help_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(SignatureHelpParams {
            position: PositionParams {
                file_path: "/test/file.rs".to_string(),
                line: 10,
                character: 5,
            },
        });

        let result = server.get_signature_help(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_go_to_implementation_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(GoToImplementationParams {
            position: PositionParams {
                file_path: "/test/file.rs".to_string(),
                line: 10,
                character: 5,
            },
        });

        let result = server.go_to_implementation(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_go_to_type_definition_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(GoToTypeDefinitionParams {
            position: PositionParams {
                file_path: "/test/file.rs".to_string(),
                line: 10,
                character: 5,
            },
        });

        let result = server.go_to_type_definition(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_inlay_hints_tool_with_params() {
        let server = create_test_server();
        let params = Parameters(InlayHintsParams {
            file_path: "/test/file.rs".to_string(),
            range: RangeParams {
                start_line: 1,
                start_character: 1,
                end_line: 10,
                end_character: 1,
            },
        });

        let result = server.get_inlay_hints(params).await;
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // Tool annotation tests
    // ------------------------------------------------------------------

    /// Every registered tool must carry `ToolAnnotations` (plus the current-spec
    /// `Tool.title`) so MCP clients can decide when to skip confirmation dialogs
    /// (read-only tools) or must prompt the user (destructive tools) without
    /// invoking the tool first. Sourced from `tool_router().list_all()` (not a
    /// hand-written list of tool names) so a future tool added without
    /// annotations fails this test instead of silently passing.
    #[test]
    fn test_all_tools_carry_annotations() {
        let tools = McplsServer::tool_router().list_all();
        assert_eq!(tools.len(), 20, "unexpected number of registered tools");

        for tool in &tools {
            assert!(
                tool.title.is_some(),
                "tool `{}` is missing a top-level title",
                tool.name
            );
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("tool `{}` is missing annotations", tool.name));
            assert!(
                annotations.title.is_some(),
                "tool `{}` is missing an annotations title",
                tool.name
            );
            assert!(
                annotations.read_only_hint.is_some(),
                "tool `{}` is missing read_only_hint",
                tool.name
            );
            assert!(
                annotations.destructive_hint.is_some(),
                "tool `{}` is missing destructive_hint",
                tool.name
            );
            assert!(
                annotations.idempotent_hint.is_some(),
                "tool `{}` is missing idempotent_hint",
                tool.name
            );
        }
    }

    /// Value-level regression guard for every tool's `(read_only, destructive,
    /// idempotent)` classification, sourced from the live `tool_router` (not
    /// per-tool `*_tool_attr()` calls) so the expected-tool table itself is
    /// checked against the actual registered count.
    #[test]
    fn test_tool_annotation_classifications_match_intent() {
        let tools = McplsServer::tool_router().list_all();
        let by_name: std::collections::HashMap<&str, &rmcp::model::ToolAnnotations> = tools
            .iter()
            .map(|tool| {
                (
                    tool.name.as_ref(),
                    tool.annotations
                        .as_ref()
                        .unwrap_or_else(|| panic!("tool `{}` is missing annotations", tool.name)),
                )
            })
            .collect();

        // (tool name, read_only_hint, destructive_hint, idempotent_hint)
        let expected: &[(&str, bool, bool, bool)] = &[
            ("get_hover", true, false, true),
            ("get_definition", true, false, true),
            ("get_references", true, false, true),
            ("get_diagnostics", true, false, true),
            ("rename_symbol", true, false, true),
            ("get_completions", true, false, true),
            ("get_document_symbols", true, false, true),
            ("format_document", true, false, true),
            ("workspace_symbol_search", true, false, true),
            ("get_code_actions", true, false, true),
            ("prepare_call_hierarchy", true, false, true),
            ("get_incoming_calls", true, false, true),
            ("get_outgoing_calls", true, false, true),
            ("get_cached_diagnostics", true, false, true),
            ("get_server_logs", true, false, true),
            ("get_server_messages", true, false, true),
            ("get_signature_help", true, false, true),
            ("go_to_implementation", true, false, true),
            ("go_to_type_definition", true, false, true),
            ("get_inlay_hints", true, false, true),
        ];

        assert_eq!(
            expected.len(),
            tools.len(),
            "expected-classification table is out of sync with the registered tool count"
        );

        for (name, read_only, destructive, idempotent) in expected {
            let annotations = by_name
                .get(name)
                .unwrap_or_else(|| panic!("tool `{name}` not found in tool_router"));
            assert_eq!(
                annotations.read_only_hint,
                Some(*read_only),
                "tool `{name}` read_only_hint mismatch"
            );
            assert_eq!(
                annotations.destructive_hint,
                Some(*destructive),
                "tool `{name}` destructive_hint mismatch"
            );
            assert_eq!(
                annotations.idempotent_hint,
                Some(*idempotent),
                "tool `{name}` idempotent_hint mismatch"
            );
        }
    }

    // ------------------------------------------------------------------
    // Resource handler tests (logic-level, avoiding rmcp::service::RequestContext
    // which requires a live Peer with private fields)
    // ------------------------------------------------------------------

    /// `list_resources` returns an empty vec for a fresh translator with no open documents.
    #[tokio::test]
    async fn test_list_resources_returns_empty_when_no_open_documents() {
        let server = create_test_server();
        let empty = server.context.translator.open_document_paths().is_empty();
        assert!(empty);
    }

    /// `parse_uri` rejects `file://` scheme — ensures `read_resource` would return an error.
    #[test]
    fn test_read_resource_rejects_file_scheme() {
        let result = parse_uri("file:///some/file.rs");
        assert!(result.is_err());
    }

    /// `parse_uri` rejects `https://` scheme.
    #[test]
    fn test_subscribe_rejects_https_scheme() {
        let result = parse_uri("https://evil.com/file.rs");
        assert!(result.is_err());
    }

    /// Regression test for `read_resource`'s canonical-path fix: a path reached
    /// through a symlink must resolve, via `validate_path_against_roots`, to the
    /// same URI as its canonical (symlink-resolved) form -- matching what
    /// `diagnostics_pump` stores from LSP notifications. Building `lsp_uri` from
    /// the raw (symlinked) path (the pre-fix behavior) would produce a
    /// mismatched cache key and always miss.
    ///
    /// Uses a real symlink rather than `..` segments: `path_to_uri` re-parses
    /// the URI string through `url::Url::parse` (for RFC 3986 char encoding),
    /// which normalizes away `..` segments regardless of platform -- so a path
    /// differing only by `..` produces the same URI as its canonical form with
    /// or without the fix. Only an actual symlink resolution (which happens in
    /// `canonicalize()`, not in URI string normalization) creates a real
    /// raw-vs-canonical difference. Unix-only: creating symlinks on Windows CI
    /// runners typically requires elevated privileges / Developer Mode.
    #[test]
    #[cfg(unix)]
    fn test_read_resource_canonical_path_matches_pump_cache_key() {
        use std::fs;
        use std::os::unix::fs::symlink;

        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        // Canonicalize the base up front so any symlink-iness already present
        // in the OS temp directory itself (e.g. macOS's `/tmp` -> `/private/tmp`)
        // doesn't leak into the comparison -- the only symlink under test is
        // `link_dir`.
        let base = temp_dir.path().canonicalize().unwrap();
        let real_dir = base.join("real");
        fs::create_dir(&real_dir).unwrap();
        let test_file = real_dir.join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let link_dir = base.join("link");
        symlink(&real_dir, &link_dir).unwrap();
        let noncanonical = link_dir.join("test.rs");
        assert_ne!(noncanonical, test_file);

        let validated = validate_path_against_roots(&noncanonical, &[]).unwrap();
        assert_eq!(validated, test_file.canonicalize().unwrap());

        let uri_from_raw_path = crate::bridge::path_to_uri(&noncanonical).unwrap();
        let uri_from_validated_path = crate::bridge::path_to_uri(&validated).unwrap();
        assert_ne!(
            uri_from_raw_path, uri_from_validated_path,
            "raw and canonical paths must differ here, otherwise this test can't \
             detect a regression back to keying off the raw path"
        );
    }

    /// `validate_path` rejects a non-existent path (canonicalize fails).
    #[tokio::test]
    async fn test_validate_path_rejects_nonexistent_path() {
        use std::path::Path;

        let translator = Translator::new();
        let result = translator.validate_path(Path::new("/this/path/does/not/exist/at/all.rs"));
        assert!(result.is_err());
    }

    /// subscribe cap enforced: after `MAX_SUBSCRIPTIONS` entries, the next call returns `Err`.
    #[tokio::test]
    async fn test_subscription_cap_enforced_in_handler_context() {
        use crate::bridge::resources::MAX_SUBSCRIPTIONS;

        let subscriptions = Arc::new(ResourceSubscriptions::new());
        for i in 0..MAX_SUBSCRIPTIONS {
            subscriptions
                .subscribe(format!("lsp-diagnostics:///file{i}.rs"))
                .await
                .unwrap();
        }
        let over = subscriptions
            .subscribe("lsp-diagnostics:///overflow.rs".to_string())
            .await;
        assert!(over.is_err());
    }

    /// unsubscribing a URI that was never subscribed is a no-op (returns `false`, not an error).
    #[tokio::test]
    async fn test_unsubscribe_nonexistent_is_noop() {
        let subscriptions = Arc::new(ResourceSubscriptions::new());
        let removed = subscriptions
            .unsubscribe("lsp-diagnostics:///nonexistent.rs")
            .await;
        assert!(!removed);
    }

    /// Server capabilities advertise resources support.
    #[tokio::test]
    async fn test_server_capabilities_include_resources() {
        let server = create_test_server();
        let info = server.get_info();
        assert!(info.capabilities.resources.is_some());
    }
}
