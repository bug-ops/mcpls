//! Typed catalogue of the MCP tool surface and the `get_tool_support` report.
//!
//! [`McpTool`] names every tool exactly once and declares, via
//! [`McpTool::spec`], its name and which LSP route (if any) serves it, so the
//! report is derived from the same routing vocabulary the bridge enforces with.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Serialize, Serializer};

use crate::bridge::{Capability, RouteSupport, ToolSupportSnapshot};
use crate::config::{ToolKind, ToolPrefix};
use crate::redaction::{Redactions, ServerText};

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

/// The static description of one [`McpTool`]: the single record from which
/// [`McpTool::name`] and [`McpTool::spec`] are derived.
#[derive(Debug, Clone, Copy)]
pub(super) struct ToolSpec {
    /// The unprefixed MCP tool name.
    pub(super) name: &'static str,
    /// The route serving the tool.
    pub(super) backend: ToolBackend,
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
    PrepareTypeHierarchy,
    GetSupertypes,
    GetSubtypes,
    PrepareRename,
    GetDocumentHighlights,
    FormatRange,
    GetCachedDiagnostics,
    GetServerLogs,
    GetServerMessages,
    GetSignatureHelp,
    GoToImplementation,
    GoToTypeDefinition,
    GetInlayHints,
    GetToolSupport,
    GoToDeclaration,
    RestartServer,
}

impl McpTool {
    /// Every tool, in registration order.
    pub(super) const ALL: [Self; 29] = [
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
        Self::PrepareTypeHierarchy,
        Self::GetSupertypes,
        Self::GetSubtypes,
        Self::PrepareRename,
        Self::GetDocumentHighlights,
        Self::FormatRange,
        Self::GetCachedDiagnostics,
        Self::GetServerLogs,
        Self::GetServerMessages,
        Self::GetSignatureHelp,
        Self::GoToImplementation,
        Self::GoToTypeDefinition,
        Self::GetInlayHints,
        Self::GetToolSupport,
        Self::GoToDeclaration,
        Self::RestartServer,
    ];

    /// Byte length of the longest unprefixed tool name.
    #[allow(
        clippy::indexing_slicing,
        reason = "the loop condition keeps `i` below `ALL.len()`"
    )]
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

    /// The name and route of this tool.
    pub(super) const fn spec(self) -> ToolSpec {
        use ToolBackend::{Document, Local, Workspace};
        let (name, backend) = match self {
            Self::GetHover => ("get_hover", Document(ToolKind::Hover)),
            Self::GetDefinition => ("get_definition", Document(ToolKind::Definition)),
            Self::GetReferences => ("get_references", Document(ToolKind::References)),
            Self::GetDiagnostics => ("get_diagnostics", Document(ToolKind::Diagnostics)),
            Self::RenameSymbol => ("rename_symbol", Document(ToolKind::Rename)),
            Self::GetCompletions => ("get_completions", Document(ToolKind::Completions)),
            Self::GetDocumentSymbols => {
                ("get_document_symbols", Document(ToolKind::DocumentSymbols))
            }
            Self::FormatDocument => ("format_document", Document(ToolKind::FormatDocument)),
            Self::WorkspaceSymbolSearch => (
                "workspace_symbol_search",
                Workspace(ToolKind::WorkspaceSymbols),
            ),
            Self::GetCodeActions => ("get_code_actions", Document(ToolKind::CodeActions)),
            Self::PrepareCallHierarchy => {
                ("prepare_call_hierarchy", Document(ToolKind::CallHierarchy))
            }
            Self::GetIncomingCalls => ("get_incoming_calls", Document(ToolKind::CallHierarchy)),
            Self::GetOutgoingCalls => ("get_outgoing_calls", Document(ToolKind::CallHierarchy)),
            Self::PrepareTypeHierarchy => {
                ("prepare_type_hierarchy", Document(ToolKind::TypeHierarchy))
            }
            Self::GetSupertypes => ("get_supertypes", Document(ToolKind::TypeHierarchy)),
            Self::GetSubtypes => ("get_subtypes", Document(ToolKind::TypeHierarchy)),
            Self::PrepareRename => ("prepare_rename", Document(ToolKind::Rename)),
            Self::GetDocumentHighlights => (
                "get_document_highlights",
                Document(ToolKind::DocumentHighlights),
            ),
            Self::FormatRange => ("format_range", Document(ToolKind::FormatRange)),
            Self::GetCachedDiagnostics => ("get_cached_diagnostics", Local),
            Self::GetServerLogs => ("get_server_logs", Local),
            Self::GetServerMessages => ("get_server_messages", Local),
            Self::GetSignatureHelp => ("get_signature_help", Document(ToolKind::SignatureHelp)),
            Self::GoToImplementation => {
                ("go_to_implementation", Document(ToolKind::Implementation))
            }
            Self::GoToTypeDefinition => {
                ("go_to_type_definition", Document(ToolKind::TypeDefinition))
            }
            Self::GetInlayHints => ("get_inlay_hints", Document(ToolKind::InlayHints)),
            Self::GetToolSupport => ("get_tool_support", Local),
            Self::GoToDeclaration => ("go_to_declaration", Document(ToolKind::Declaration)),
            Self::RestartServer => ("restart_server", Local),
        };
        ToolSpec { name, backend }
    }

    /// The unprefixed MCP tool name.
    pub(super) const fn name(self) -> &'static str {
        self.spec().name
    }

    /// The capability this tool's call is gated on, or `None` when it is
    /// ungated or answered locally. A tool sharing another tool's route can
    /// need a different capability (`prepare_rename` on the `Rename` route),
    /// so this is per tool, not per [`ToolKind`].
    pub(super) const fn capability(self) -> Option<Capability> {
        match self {
            Self::PrepareRename => Some(Capability::PrepareRename),
            _ => match self.spec().backend {
                ToolBackend::Document(kind) | ToolBackend::Workspace(kind) => {
                    Capability::for_tool(kind)
                }
                ToolBackend::Local => None,
            },
        }
    }
}

/// The client-visible name of `name` under an optional configured prefix.
pub(super) fn prefixed_tool_name(prefix: Option<&ToolPrefix>, name: &str) -> String {
    prefix.map_or_else(|| name.to_string(), |prefix| format!("{prefix}_{name}"))
}

/// How widely a tool is usable across the languages in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
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
            total = total.saturating_add(1);
            match route {
                RouteSupport::Supported { .. } => supported = supported.saturating_add(1),
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

/// The languages of one tool that share an identical [`RouteSupport`].
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(super) struct LanguageGroup {
    languages: Vec<String>,
    #[serde(flatten)]
    support: RouteSupport,
}

/// The routes of one tool in the report.
///
/// Serializes as a flat array in both variants: document routes are grouped
/// by identical support (`languages` lists the members), a workspace route has
/// no language and is a single element without `languages`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ToolRoutes {
    /// Per-document tool: languages grouped by identical support, in
    /// first-seen order over the sorted languages.
    Document(Vec<LanguageGroup>),
    /// Workspace-wide tool: its one route.
    Workspace(RouteSupport),
}

impl ToolRoutes {
    /// Group `routes` (one per language) by identical support, keeping the
    /// order in which each distinct support is first seen.
    fn grouped(routes: Vec<(String, RouteSupport)>) -> Self {
        let mut groups: Vec<LanguageGroup> = Vec::new();
        for (language, support) in routes {
            match groups.iter_mut().find(|group| group.support == support) {
                Some(group) => group.languages.push(language),
                None => groups.push(LanguageGroup {
                    languages: vec![language],
                    support,
                }),
            }
        }
        Self::Document(groups)
    }
}

/// One route of a tool in the report: the languages sharing this support
/// status (absent for workspace-wide tools) and the status itself.
#[derive(JsonSchema)]
#[allow(
    dead_code,
    reason = "describes the wire shape for schema generation only"
)]
struct RouteShape {
    languages: Option<Vec<String>>,
    #[serde(flatten)]
    support: RouteSupport,
}

impl JsonSchema for ToolRoutes {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ToolRoutes")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        <Vec<RouteShape>>::json_schema(generator)
    }
}

impl Serialize for ToolRoutes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Document(groups) => serializer.collect_seq(groups),
            Self::Workspace(support) => serializer.collect_seq(std::iter::once(support)),
        }
    }
}

/// One tool in the report.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ToolEntry {
    name: String,
    coverage: ToolCoverage,
    #[serde(skip_serializing_if = "Option::is_none")]
    routes: Option<ToolRoutes>,
}

/// The `get_tool_support` response.
#[derive(Debug, Serialize, JsonSchema)]
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
                let spec = tool.spec();
                let (coverage, routes) = match spec.backend {
                    ToolBackend::Local => (ToolCoverage::Always, None),
                    ToolBackend::Document(kind) => {
                        let routes: Vec<_> = languages
                            .iter()
                            .map(|language| {
                                (
                                    language.clone(),
                                    snapshot.document_support_gated(
                                        language,
                                        kind,
                                        tool.capability(),
                                    ),
                                )
                            })
                            .collect();
                        let coverage = ToolCoverage::from_routes(routes.iter().map(|(_, s)| s));
                        (
                            coverage,
                            (coverage != ToolCoverage::All).then(|| ToolRoutes::grouped(routes)),
                        )
                    }
                    ToolBackend::Workspace(kind) => {
                        let support = snapshot.workspace_support(kind);
                        let coverage = ToolCoverage::from_routes(std::iter::once(&support));
                        (
                            coverage,
                            (coverage != ToolCoverage::All)
                                .then_some(ToolRoutes::Workspace(support)),
                        )
                    }
                };
                ToolEntry {
                    name: prefixed_tool_name(prefix, spec.name),
                    coverage,
                    routes,
                }
            })
            .collect();
        Self { languages, tools }
    }
}

// Languages and tool names come from configuration, not from a server.
impl ServerText for ToolSupportReport {
    fn redact_server_text(&mut self, _redactions: &Redactions) {
        let Self {
            languages: _,
            tools: _,
        } = self;
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

    #[test]
    fn document_routes_group_identical_support_in_first_seen_order() {
        let none = RouteSupport::NoServer;
        let routes = ToolRoutes::grouped(vec![
            ("c".to_string(), none.clone()),
            ("go".to_string(), supported()),
            ("java".to_string(), none),
            ("rust".to_string(), supported()),
        ]);
        assert_eq!(
            serde_json::to_value(&routes).unwrap(),
            serde_json::json!([
                {"languages": ["c", "java"], "status": "no_server"},
                {"languages": ["go", "rust"], "status": "supported", "server": "s"},
            ])
        );
    }

    #[test]
    fn workspace_route_serializes_without_languages() {
        assert_eq!(
            serde_json::to_value(ToolRoutes::Workspace(RouteSupport::NoServer)).unwrap(),
            serde_json::json!([{"status": "no_server"}])
        );
    }
}
