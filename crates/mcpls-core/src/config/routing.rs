//! Explicit per-tool routing (#174).
//!
//! `language_id` alone is not a unique server identity: two servers can
//! share one language (e.g. pyright and pylsp both for `python`), each
//! handling a different subset of MCP tools. This module defines the typed
//! vocabulary for that routing — [`ServerId`], [`ToolKind`] — and
//! [`ToolRouter`], which resolves `(language, tool)` to the server that
//! should handle it.
//!
//! `ToolKind` lives here, in `config`, rather than in `mcp` (which is where
//! its variants are semantically drawn from) to keep `config` a leaf module:
//! `mcp` and `bridge` both depend on `config`, so putting `ToolKind` in `mcp`
//! would create a `config -> mcp -> bridge -> config` cycle. When a new
//! routable MCP tool is added, extend [`ToolKind::ALL`] here.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::server::LspServerConfig;
use crate::error::{Error, Result};

/// Unique identity of a configured LSP server within a workspace.
///
/// Derived from [`LspServerConfig::id`]: a server's explicit `name` if set,
/// otherwise its `language_id`. This is the key used throughout the bridge
/// layer (`Translator::lsp_clients`, `lsp_servers`, notification receivers)
/// instead of a raw language string, so two servers sharing a language no
/// longer silently overwrite each other.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerId(String);

impl ServerId {
    /// Borrow the identity as a plain string, e.g. for log messages or map
    /// lookups against external APIs that expect `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ServerId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for ServerId {
    fn from(id: &str) -> Self {
        Self(id.to_string())
    }
}

/// A routable MCP tool: every MCP tool that dispatches a request to a
/// specific LSP server via [`ToolRouter`].
///
/// Cache-only tools (`get_cached_diagnostics`, `get_server_logs`,
/// `get_server_messages`) are deliberately excluded — they never reach a
/// client directly, so they have nothing to route.
///
/// `CallHierarchy` covers `prepare`, `incoming_calls`, and `outgoing_calls`
/// as a single route: the opaque item returned by `prepare` is only
/// meaningful to the server that produced it, and the incoming/outgoing
/// handlers never call `ensure_open` themselves — they rely on `prepare`
/// having already synced the document to the *same* server.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ToolKind;
///
/// assert_eq!(ToolKind::Hover.as_str(), "hover");
/// assert_eq!(ToolKind::ALL.len(), 15);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// `textDocument/hover`.
    Hover,
    /// `textDocument/definition`.
    Definition,
    /// `textDocument/typeDefinition`.
    TypeDefinition,
    /// `textDocument/implementation`.
    Implementation,
    /// `textDocument/references`.
    References,
    /// `textDocument/diagnostic` (pull) and the `publishDiagnostics` cache filter.
    Diagnostics,
    /// `textDocument/rename`.
    Rename,
    /// `textDocument/completion`.
    Completions,
    /// `textDocument/signatureHelp`.
    SignatureHelp,
    /// `textDocument/documentSymbol`.
    DocumentSymbols,
    /// `workspace/symbol`.
    WorkspaceSymbols,
    /// `textDocument/formatting`.
    FormatDocument,
    /// `textDocument/codeAction`.
    CodeActions,
    /// `textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls`, `callHierarchy/outgoingCalls`.
    CallHierarchy,
    /// `textDocument/inlayHint`.
    InlayHints,
}

impl ToolKind {
    /// Every routable tool, in a fixed order. Used to compute the §5
    /// coverage warning and to build error messages that enumerate tools.
    pub const ALL: [Self; 15] = [
        Self::Hover,
        Self::Definition,
        Self::TypeDefinition,
        Self::Implementation,
        Self::References,
        Self::Diagnostics,
        Self::Rename,
        Self::Completions,
        Self::SignatureHelp,
        Self::DocumentSymbols,
        Self::WorkspaceSymbols,
        Self::FormatDocument,
        Self::CodeActions,
        Self::CallHierarchy,
        Self::InlayHints,
    ];

    /// The `snake_case` name used in config `handles` lists and error messages.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Hover => "hover",
            Self::Definition => "definition",
            Self::TypeDefinition => "type_definition",
            Self::Implementation => "implementation",
            Self::References => "references",
            Self::Diagnostics => "diagnostics",
            Self::Rename => "rename",
            Self::Completions => "completions",
            Self::SignatureHelp => "signature_help",
            Self::DocumentSymbols => "document_symbols",
            Self::WorkspaceSymbols => "workspace_symbols",
            Self::FormatDocument => "format_document",
            Self::CodeActions => "code_actions",
            Self::CallHierarchy => "call_hierarchy",
            Self::InlayHints => "inlay_hints",
        }
    }
}

impl std::fmt::Display for ToolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-language routing table: which server handles which tool.
#[derive(Debug, Default)]
struct LanguageRoutes {
    /// Tools explicitly claimed via a server's `handles` list.
    explicit: HashMap<ToolKind, ServerId>,
    /// The single server (if any) that omitted `handles` — serves every
    /// tool not explicitly claimed by another server for this language.
    default: Option<ServerId>,
}

/// Resolves `(language, tool)` to the [`ServerId`] that should handle it.
///
/// Built once at startup by [`Self::from_configs`] over the *applicable*
/// (post-heuristics) server configs, then rebound once at registration time
/// by [`Self::rebind_to_registered`] so that no route ever points at a
/// server that failed to spawn.
#[derive(Debug, Default)]
pub struct ToolRouter {
    by_language: HashMap<String, LanguageRoutes>,
    /// Config declaration order, used by `resolve_any` for a deterministic
    /// choice among candidates. Pruned to registered servers by
    /// `rebind_to_registered`.
    order: Vec<ServerId>,
}

impl ToolRouter {
    /// Build a router from the configs applicable in this workspace,
    /// enforcing the workspace-scoped validation rules:
    ///
    /// 1. No two applicable servers (in any language) may share a
    ///    [`ServerId`] — it is the key of every map keyed by server identity.
    /// 2. No two applicable servers for one language may both omit `handles`
    ///    (two catch-alls).
    /// 3. No tool may be claimed via `handles` by two applicable servers of
    ///    the same language.
    ///
    /// Also emits a `tracing::warn!` for any language whose union of
    /// `handles` claims is partial and has no catch-all server, naming the
    /// tools nobody will serve.
    ///
    /// # Errors
    ///
    /// Returns `Error::InvalidConfig` naming the conflicting entries if any
    /// of the three rules above is violated.
    pub fn from_configs<'a, I>(cfgs: I) -> Result<Self>
    where
        I: IntoIterator<Item = &'a LspServerConfig>,
    {
        let mut by_language: HashMap<String, LanguageRoutes> = HashMap::new();
        let mut order: Vec<ServerId> = Vec::new();
        let mut seen_ids: HashMap<ServerId, String> = HashMap::new();

        for cfg in cfgs {
            let id = cfg.id();

            if let Some(prev_language) = seen_ids.get(&id) {
                return Err(Error::InvalidConfig(format!(
                    "duplicate server id '{id}' in this workspace (used by both the \
                     '{prev_language}' and '{}' language entries); add a unique `name` to \
                     each `[[lsp_servers]]` entry",
                    cfg.language_id
                )));
            }
            seen_ids.insert(id.clone(), cfg.language_id.clone());
            order.push(id.clone());

            let routes = by_language.entry(cfg.language_id.clone()).or_default();

            match &cfg.handles {
                None => {
                    if let Some(existing) = &routes.default {
                        return Err(Error::InvalidConfig(format!(
                            "language '{}' has two catch-all servers ('{existing}' and '{id}'); \
                             at most one server per language may omit `handles`",
                            cfg.language_id
                        )));
                    }
                    routes.default = Some(id);
                }
                Some(tools) => {
                    for tool in tools {
                        if let Some(existing) = routes.explicit.get(tool) {
                            return Err(Error::InvalidConfig(format!(
                                "tool '{tool}' for language '{}' is claimed by both \
                                 '{existing}' and '{id}'",
                                cfg.language_id
                            )));
                        }
                        routes.explicit.insert(*tool, id.clone());
                    }
                }
            }
        }

        // Deliberately untested (M4): asserting on `tracing` output would
        // need a subscriber/capture dev-dependency this crate doesn't
        // otherwise pull in. Verified by inspection instead; the `uncovered`
        // computation itself is exercised indirectly by every `resolve`
        // test above that checks an unclaimed tool returns `None`.
        for (language, routes) in &by_language {
            if routes.default.is_none() {
                let uncovered: Vec<&str> = ToolKind::ALL
                    .iter()
                    .filter(|t| !routes.explicit.contains_key(t))
                    .map(ToolKind::as_str)
                    .collect();
                if !uncovered.is_empty() {
                    tracing::warn!(
                        "language '{language}' has no catch-all server and does not claim: {}",
                        uncovered.join(", ")
                    );
                }
            }
        }

        Ok(Self { by_language, order })
    }

    /// Build a router where every entry is a catch-all for its language.
    ///
    /// Test helper: takes `(id, language)` pairs rather than a single entry
    /// because some tests (e.g. the `typescript`/`typescriptreact` exact-match
    /// preference) need two catch-alls registered at once.
    #[must_use]
    pub fn catch_all<I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (ServerId, String)>,
    {
        let mut by_language: HashMap<String, LanguageRoutes> = HashMap::new();
        let mut order = Vec::new();
        for (id, language) in entries {
            order.push(id.clone());
            by_language.entry(language).or_default().default = Some(id);
        }
        Self { by_language, order }
    }

    /// Rebind every route pointing at a server that did not register — i.e.
    /// failed to spawn — to that language's live catch-all, or drop the
    /// route entirely if no catch-all is live.
    ///
    /// A dead route is never rebound to a *narrowly-scoped* live server: a
    /// server that declared `handles = [...]` has explicitly declined every
    /// other tool, and conscripting it would override that declaration (and,
    /// via the diagnostics cache filter, start caching diagnostics the user
    /// deliberately routed away).
    ///
    /// # Preconditions
    ///
    /// Call this exactly once, after all spawn attempts for a `serve_with`
    /// invocation have completed and before any request can observe the
    /// router. This is sound only because `LspServer::spawn_batch` is a
    /// sequential loop that produces one `ServerInitResult` registered under
    /// a single lock — registration is one atomic all-or-nothing event, so
    /// no request can observe a half-rebound router. If server registration
    /// is ever made incremental (servers registering as they finish spawning,
    /// rather than all together), an early rebind here would permanently
    /// steal a slow server's routes with no way back; this function would
    /// need to be replaced with a design that derives the active table on
    /// each lookup instead of mutating it once.
    pub fn rebind_to_registered(&mut self, registered: &HashSet<ServerId>) {
        for (language, routes) in &mut self.by_language {
            let live_catch_all = routes.default.clone().filter(|id| registered.contains(id));

            let mut dead: HashMap<ServerId, Vec<ToolKind>> = HashMap::new();
            for (tool, id) in &routes.explicit {
                if !registered.contains(id) {
                    dead.entry(id.clone()).or_default().push(*tool);
                }
            }

            for (dead_id, tools) in dead {
                let tool_names: Vec<&str> = tools.iter().map(ToolKind::as_str).collect();
                if let Some(catch_all_id) = &live_catch_all {
                    for tool in &tools {
                        routes.explicit.insert(*tool, catch_all_id.clone());
                    }
                    tracing::warn!(
                        "language '{language}': server '{dead_id}' failed to spawn; \
                         rebinding [{}] to catch-all '{catch_all_id}'",
                        tool_names.join(", ")
                    );
                } else {
                    for tool in &tools {
                        routes.explicit.remove(tool);
                    }
                    tracing::warn!(
                        "language '{language}': server '{dead_id}' failed to spawn and no \
                         live catch-all is available; [{}] will report no server available",
                        tool_names.join(", ")
                    );
                }
            }

            if let Some(dead_catch_all) = routes
                .default
                .as_ref()
                .filter(|id| !registered.contains(*id))
                .cloned()
            {
                routes.default = None;
                tracing::warn!(
                    "language '{language}': catch-all server '{dead_catch_all}' failed to \
                     spawn; every tool it wasn't already explicitly rebound above will report \
                     no server available"
                );
            }
        }

        self.order.retain(|id| registered.contains(id));
    }

    /// Resolve the server that should handle `tool` for `language_id`.
    ///
    /// Explicit claims win over the language's catch-all; if neither exists,
    /// returns `None`.
    #[must_use]
    pub fn resolve(&self, language_id: &str, tool: ToolKind) -> Option<&ServerId> {
        let routes = self.by_language.get(language_id)?;
        routes.explicit.get(&tool).or(routes.default.as_ref())
    }

    /// Resolve a server for `tool` without a specific language — used for
    /// workspace-wide tools like `workspace_symbol_search` that have no
    /// document to detect a language from.
    ///
    /// Resolves in three tiers, in config declaration order:
    /// 1. the first server that explicitly claims `tool`;
    /// 2. else the first catch-all server;
    /// 3. else the first server at all.
    ///
    /// Tier 2 exists so that a narrowly-scoped server declared before a
    /// catch-all cannot win a tool it explicitly declined: a catch-all
    /// claims every tool *implicitly*, so it must still lose to an *explicit*
    /// claimer in tier 1, but it must beat tier 3's arbitrary "any server"
    /// fallback.
    #[must_use]
    pub fn resolve_any(&self, tool: ToolKind) -> Option<&ServerId> {
        let claims_explicitly = |id: &ServerId| {
            self.by_language
                .values()
                .any(|r| r.explicit.get(&tool) == Some(id))
        };
        let is_catch_all = |id: &ServerId| {
            self.by_language
                .values()
                .any(|r| r.default.as_ref() == Some(id))
        };

        self.order
            .iter()
            .find(|id| claims_explicitly(id))
            .or_else(|| self.order.iter().find(|id| is_catch_all(id)))
            .or_else(|| self.order.first())
    }

    /// Whether `language_id` currently has at least one live-or-configured
    /// route (a catch-all or an explicit claim), used to distinguish
    /// `NoServerForTool` (some server handles this language, just not this
    /// tool) from `NoServerForLanguage` (nothing does).
    ///
    /// Deliberately checks route *contents*, not just map-key presence: after
    /// `rebind_to_registered` drops every route for a language whose sole
    /// server failed to spawn, this must go back to `false` so that language
    /// reports `NoServerForLanguage` exactly as it did before per-tool
    /// routing existed, not `NoServerForTool`.
    #[must_use]
    pub fn has_language(&self, language_id: &str) -> bool {
        self.by_language
            .get(language_id)
            .is_some_and(|r| r.default.is_some() || !r.explicit.is_empty())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn cfg(
        language_id: &str,
        name: Option<&str>,
        handles: Option<Vec<ToolKind>>,
    ) -> LspServerConfig {
        LspServerConfig {
            language_id: language_id.to_string(),
            command: "cmd".to_string(),
            args: vec![],
            env: HashMap::new(),
            file_patterns: vec![],
            initialization_options: None,
            timeout_seconds: 30,
            heuristics: None,
            name: name.map(str::to_string),
            handles,
        }
    }

    #[test]
    fn test_resolve_explicit_wins_over_catch_all() {
        let configs = vec![
            cfg("python", Some("pyright"), Some(vec![ToolKind::Hover])),
            cfg("python", Some("pylsp"), None),
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();
        assert_eq!(
            router.resolve("python", ToolKind::Hover),
            Some(&ServerId::from("pyright"))
        );
        assert_eq!(
            router.resolve("python", ToolKind::Diagnostics),
            Some(&ServerId::from("pylsp"))
        );
    }

    #[test]
    fn test_resolve_no_catch_all_unclaimed_is_none() {
        let configs = vec![cfg("python", Some("pyright"), Some(vec![ToolKind::Hover]))];
        let router = ToolRouter::from_configs(&configs).unwrap();
        assert_eq!(router.resolve("python", ToolKind::Diagnostics), None);
    }

    #[test]
    fn test_resolve_any_explicit_claimer_beats_catch_all_declared_first() {
        let configs = vec![
            cfg("python", Some("python-narrow"), Some(vec![ToolKind::Hover])),
            cfg("rust", Some("rust-catch-all"), None),
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();
        // Neither server explicitly claims WorkspaceSymbols, so the rust
        // catch-all must win over the narrowly-scoped python server, even
        // though python was declared first.
        assert_eq!(
            router.resolve_any(ToolKind::WorkspaceSymbols),
            Some(&ServerId::from("rust-catch-all"))
        );
    }

    #[test]
    fn test_resolve_any_prefers_explicit_claimer_over_catch_all() {
        let configs = vec![
            cfg("rust", Some("rust-catch-all"), None),
            cfg(
                "python",
                Some("python-explicit"),
                Some(vec![ToolKind::WorkspaceSymbols]),
            ),
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();
        assert_eq!(
            router.resolve_any(ToolKind::WorkspaceSymbols),
            Some(&ServerId::from("python-explicit"))
        );
    }

    #[test]
    fn test_from_configs_rejects_duplicate_server_id_across_languages() {
        let configs = vec![
            cfg("python", None, None),
            cfg("typescript", Some("python"), None),
        ];
        let err = ToolRouter::from_configs(&configs).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn test_from_configs_rejects_two_catch_alls() {
        let configs = vec![
            cfg("python", Some("a"), None),
            cfg("python", Some("b"), None),
        ];
        let err = ToolRouter::from_configs(&configs).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn test_from_configs_rejects_duplicate_tool_claim() {
        let configs = vec![
            cfg("python", Some("a"), Some(vec![ToolKind::Hover])),
            cfg("python", Some("b"), Some(vec![ToolKind::Hover])),
        ];
        let err = ToolRouter::from_configs(&configs).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn test_rebind_to_registered_dead_server_with_live_catch_all() {
        let configs = vec![
            cfg("python", Some("pyright"), Some(vec![ToolKind::Hover])),
            cfg("python", Some("pylsp"), None),
        ];
        let mut router = ToolRouter::from_configs(&configs).unwrap();
        let registered: HashSet<ServerId> = HashSet::from([ServerId::from("pylsp")]);
        router.rebind_to_registered(&registered);

        assert_eq!(
            router.resolve("python", ToolKind::Hover),
            Some(&ServerId::from("pylsp"))
        );
    }

    #[test]
    fn test_rebind_to_registered_dead_server_no_catch_all_drops_route() {
        let configs = vec![
            cfg("python", Some("pyright"), Some(vec![ToolKind::Hover])),
            cfg("python", Some("pylsp"), Some(vec![ToolKind::Diagnostics])),
        ];
        let mut router = ToolRouter::from_configs(&configs).unwrap();
        let registered: HashSet<ServerId> = HashSet::from([ServerId::from("pylsp")]);
        router.rebind_to_registered(&registered);

        // pyright died, no catch-all exists, and pylsp never claimed Hover:
        // the route must drop rather than conscript pylsp.
        assert_eq!(router.resolve("python", ToolKind::Hover), None);
        assert_eq!(
            router.resolve("python", ToolKind::Diagnostics),
            Some(&ServerId::from("pylsp"))
        );
    }

    #[test]
    fn test_rebind_to_registered_all_failed_drops_everything() {
        let configs = vec![cfg("rust", None, None)];
        let mut router = ToolRouter::from_configs(&configs).unwrap();
        router.rebind_to_registered(&HashSet::new());
        assert_eq!(router.resolve("rust", ToolKind::Hover), None);
        assert_eq!(router.resolve_any(ToolKind::Hover), None);
        // A single-server-per-language config whose server fails to spawn
        // must report NoServerForLanguage upstream, not NoServerForTool --
        // has_language must go back to false once every route is dropped.
        assert!(!router.has_language("rust"));
    }

    #[test]
    fn test_rebind_prunes_order_for_resolve_any() {
        let configs = vec![cfg("rust", Some("a"), None), cfg("python", Some("b"), None)];
        let mut router = ToolRouter::from_configs(&configs).unwrap();
        let registered: HashSet<ServerId> = HashSet::from([ServerId::from("b")]);
        router.rebind_to_registered(&registered);
        assert_eq!(
            router.resolve_any(ToolKind::Hover),
            Some(&ServerId::from("b"))
        );
    }

    #[test]
    fn test_has_language() {
        let configs = vec![cfg("rust", None, None)];
        let router = ToolRouter::from_configs(&configs).unwrap();
        assert!(router.has_language("rust"));
        assert!(!router.has_language("python"));
    }

    #[test]
    fn test_catch_all_helper_registers_two_entries() {
        let router = ToolRouter::catch_all([
            (ServerId::from("ts"), "typescript".to_string()),
            (ServerId::from("tsx"), "typescriptreact".to_string()),
        ]);
        assert_eq!(
            router.resolve("typescript", ToolKind::Hover),
            Some(&ServerId::from("ts"))
        );
        assert_eq!(
            router.resolve("typescriptreact", ToolKind::Hover),
            Some(&ServerId::from("tsx"))
        );
    }

    #[test]
    fn test_tool_kind_as_str_and_all_len() {
        assert_eq!(ToolKind::Hover.as_str(), "hover");
        assert_eq!(ToolKind::CallHierarchy.as_str(), "call_hierarchy");
        assert_eq!(ToolKind::ALL.len(), 15);
    }

    #[test]
    fn test_server_id_display_and_as_str() {
        let id = ServerId::from("pyright");
        assert_eq!(id.as_str(), "pyright");
        assert_eq!(id.to_string(), "pyright");
    }
}
