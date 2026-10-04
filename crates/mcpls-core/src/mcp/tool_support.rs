//! Typed catalogue of the MCP tool surface and the `get_tool_support` report.
//!
//! [`McpTool`] names every tool exactly once and declares, via
//! [`McpTool::backend`], which LSP route (if any) serves it, so the report is
//! derived from the same routing vocabulary the bridge enforces with.

use serde::Serialize;

use crate::bridge::{RouteSupport, ToolSupportSnapshot};
use crate::config::{ToolKind, ToolPrefix};

/// Where a tool's request is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolBackend {
    /// Routed per file, by the language of the file argument.
    Document(ToolKind),
    /// Routed workspace-wide, without a language.
    Workspace(ToolKind),
    /// Answered by mcpls itself; needs no language server.
    Local,
}

/// Every MCP tool mcpls exposes, one variant per `#[tool]` handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum McpTool {
    GetHover,
    GetDefinition,
    GetReferences,
    GetDiagnostics,
    RenameSymbol,
    GetCompletions,
    GetDocumentSymbols,
    FormatDocument,
    WorkspaceSymbolSearch,
    GetCodeActions,
    PrepareCallHierarchy,
    GetIncomingCalls,
    GetOutgoingCalls,
    GetCachedDiagnostics,
    GetServerLogs,
    GetServerMessages,
    GetSignatureHelp,
    GoToImplementation,
    GoToTypeDefinition,
    GetInlayHints,
    GetToolSupport,
}

impl McpTool {
    /// Every tool, in registration order.
    pub(super) const ALL: [Self; 21] = [
        Self::GetHover,
        Self::GetDefinition,
        Self::GetReferences,
        Self::GetDiagnostics,
        Self::RenameSymbol,
        Self::GetCompletions,
        Self::GetDocumentSymbols,
        Self::FormatDocument,
        Self::WorkspaceSymbolSearch,
        Self::GetCodeActions,
        Self::PrepareCallHierarchy,
        Self::GetIncomingCalls,
        Self::GetOutgoingCalls,
        Self::GetCachedDiagnostics,
        Self::GetServerLogs,
        Self::GetServerMessages,
        Self::GetSignatureHelp,
        Self::GoToImplementation,
        Self::GoToTypeDefinition,
        Self::GetInlayHints,
        Self::GetToolSupport,
    ];

    /// Byte length of the longest unprefixed tool name.
    pub(super) const MAX_NAME_BYTES: usize = {
        let mut max = 0;
        let mut i = 0;
        while i < Self::ALL.len() {
            let len = Self::ALL[i].name().len();
            if len > max {
                max = len;
            }
            i += 1;
        }
        max
    };

    /// The unprefixed MCP tool name.
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::GetHover => "get_hover",
            Self::GetDefinition => "get_definition",
            Self::GetReferences => "get_references",
            Self::GetDiagnostics => "get_diagnostics",
            Self::RenameSymbol => "rename_symbol",
            Self::GetCompletions => "get_completions",
            Self::GetDocumentSymbols => "get_document_symbols",
            Self::FormatDocument => "format_document",
            Self::WorkspaceSymbolSearch => "workspace_symbol_search",
            Self::GetCodeActions => "get_code_actions",
            Self::PrepareCallHierarchy => "prepare_call_hierarchy",
            Self::GetIncomingCalls => "get_incoming_calls",
            Self::GetOutgoingCalls => "get_outgoing_calls",
            Self::GetCachedDiagnostics => "get_cached_diagnostics",
            Self::GetServerLogs => "get_server_logs",
            Self::GetServerMessages => "get_server_messages",
            Self::GetSignatureHelp => "get_signature_help",
            Self::GoToImplementation => "go_to_implementation",
            Self::GoToTypeDefinition => "go_to_type_definition",
            Self::GetInlayHints => "get_inlay_hints",
            Self::GetToolSupport => "get_tool_support",
        }
    }

    /// The route serving this tool.
    pub(super) const fn backend(self) -> ToolBackend {
        match self {
            Self::GetHover => ToolBackend::Document(ToolKind::Hover),
            Self::GetDefinition => ToolBackend::Document(ToolKind::Definition),
            Self::GetReferences => ToolBackend::Document(ToolKind::References),
            Self::GetDiagnostics => ToolBackend::Document(ToolKind::Diagnostics),
            Self::RenameSymbol => ToolBackend::Document(ToolKind::Rename),
            Self::GetCompletions => ToolBackend::Document(ToolKind::Completions),
            Self::GetDocumentSymbols => ToolBackend::Document(ToolKind::DocumentSymbols),
            Self::FormatDocument => ToolBackend::Document(ToolKind::FormatDocument),
            Self::WorkspaceSymbolSearch => ToolBackend::Workspace(ToolKind::WorkspaceSymbols),
            Self::GetCodeActions => ToolBackend::Document(ToolKind::CodeActions),
            Self::PrepareCallHierarchy | Self::GetIncomingCalls | Self::GetOutgoingCalls => {
                ToolBackend::Document(ToolKind::CallHierarchy)
            }
            Self::GetSignatureHelp => ToolBackend::Document(ToolKind::SignatureHelp),
            Self::GoToImplementation => ToolBackend::Document(ToolKind::Implementation),
            Self::GoToTypeDefinition => ToolBackend::Document(ToolKind::TypeDefinition),
            Self::GetInlayHints => ToolBackend::Document(ToolKind::InlayHints),
            Self::GetCachedDiagnostics
            | Self::GetServerLogs
            | Self::GetServerMessages
            | Self::GetToolSupport => ToolBackend::Local,
        }
    }
}

/// The client-visible name of `name` under an optional configured prefix.
pub(super) fn prefixed_tool_name(prefix: Option<&ToolPrefix>, name: &str) -> String {
    prefix.map_or_else(|| name.to_string(), |prefix| format!("{prefix}_{name}"))
}

/// How widely a tool is usable across the languages in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ToolCoverage {
    /// Supported for every listed language.
    All,
    /// Supported for some listed languages but not all.
    #[serde(rename = "some")]
    Partial,
    /// Supported for no listed language (or nothing is configured).
    #[serde(rename = "none")]
    Unsupported,
    /// Not supported anywhere yet, but at least one server is still initializing.
    Unknown,
    /// Answered by mcpls itself; independent of language servers.
    Always,
}

impl ToolCoverage {
    fn from_routes<'a>(routes: impl Iterator<Item = &'a RouteSupport>) -> Self {
        let (mut total, mut supported, mut initializing) = (0_usize, 0_usize, false);
        for route in routes {
            total += 1;
            match route {
                RouteSupport::Supported { .. } => supported += 1,
                RouteSupport::Initializing => initializing = true,
                RouteSupport::CapabilityNotAdvertised { .. } | RouteSupport::NoServer => {}
            }
        }
        if total == 0 {
            Self::Unsupported
        } else if supported == total {
            Self::All
        } else if supported > 0 {
            Self::Partial
        } else if initializing {
            Self::Unknown
        } else {
            Self::Unsupported
        }
    }
}

/// One route of a tool in the report: a language (absent for workspace-wide
/// tools) and its support status.
#[derive(Debug, Serialize)]
pub(super) struct RouteEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(flatten)]
    support: RouteSupport,
}

/// One tool in the report.
#[derive(Debug, Serialize)]
pub(super) struct ToolEntry {
    name: String,
    coverage: ToolCoverage,
    #[serde(skip_serializing_if = "Option::is_none")]
    routes: Option<Vec<RouteEntry>>,
}

/// The `get_tool_support` response.
#[derive(Debug, Serialize)]
pub(super) struct ToolSupportReport {
    languages: Vec<String>,
    tools: Vec<ToolEntry>,
}

impl ToolSupportReport {
    /// Build the report over `languages` (the domain of every per-document
    /// tool) from `snapshot`, naming tools as clients see them under `prefix`.
    pub(super) fn build(
        snapshot: &ToolSupportSnapshot,
        languages: Vec<String>,
        prefix: Option<&ToolPrefix>,
    ) -> Self {
        let tools = McpTool::ALL
            .iter()
            .map(|tool| {
                let name = prefixed_tool_name(prefix, tool.name());
                let routes: Vec<RouteEntry> = match tool.backend() {
                    ToolBackend::Local => {
                        return ToolEntry {
                            name,
                            coverage: ToolCoverage::Always,
                            routes: None,
                        };
                    }
                    ToolBackend::Document(kind) => languages
                        .iter()
                        .map(|language| RouteEntry {
                            language: Some(language.clone()),
                            support: snapshot.document_support(language, kind),
                        })
                        .collect(),
                    ToolBackend::Workspace(kind) => vec![RouteEntry {
                        language: None,
                        support: snapshot.workspace_support(kind),
                    }],
                };
                let coverage = ToolCoverage::from_routes(routes.iter().map(|r| &r.support));
                ToolEntry {
                    name,
                    coverage,
                    routes: (coverage != ToolCoverage::All).then_some(routes),
                }
            })
            .collect();
        Self { languages, tools }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn supported() -> RouteSupport {
        RouteSupport::Supported { server: "s".into() }
    }

    #[test]
    fn coverage_distinguishes_all_some_none_unknown() {
        let missing = RouteSupport::NoServer;
        assert_eq!(
            ToolCoverage::from_routes([].iter()),
            ToolCoverage::Unsupported
        );
        assert_eq!(
            ToolCoverage::from_routes([supported(), supported()].iter()),
            ToolCoverage::All
        );
        assert_eq!(
            ToolCoverage::from_routes([supported(), missing.clone()].iter()),
            ToolCoverage::Partial
        );
        assert_eq!(
            ToolCoverage::from_routes([supported(), RouteSupport::Initializing].iter()),
            ToolCoverage::Partial
        );
        assert_eq!(
            ToolCoverage::from_routes([missing.clone(), RouteSupport::Initializing].iter()),
            ToolCoverage::Unknown
        );
        assert_eq!(
            ToolCoverage::from_routes([missing, RouteSupport::NoServer].iter()),
            ToolCoverage::Unsupported
        );
    }

    #[test]
    fn coverage_serializes_to_spec_names() {
        let names: Vec<String> = [
            ToolCoverage::All,
            ToolCoverage::Partial,
            ToolCoverage::Unsupported,
            ToolCoverage::Unknown,
            ToolCoverage::Always,
        ]
        .iter()
        .map(|c| {
            serde_json::to_value(c)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
        assert_eq!(names, ["all", "some", "none", "unknown", "always"]);
    }

    #[test]
    fn tool_names_are_unique() {
        let mut names: Vec<&str> = McpTool::ALL.iter().map(|t| t.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), McpTool::ALL.len());
    }

    #[test]
    fn prefixed_tool_name_joins_with_underscore() {
        let prefix: ToolPrefix = "optics".parse().unwrap();
        assert_eq!(
            prefixed_tool_name(Some(&prefix), "get_hover"),
            "optics_get_hover"
        );
        assert_eq!(prefixed_tool_name(None, "get_hover"), "get_hover");
    }
}
