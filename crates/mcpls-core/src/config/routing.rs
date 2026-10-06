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

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::language_id::LanguageId;
use super::server::LspServerConfig;
use crate::error::{ConfigError, Result};

/// A server id was empty or whitespace-only.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("server id cannot be blank (omit `name` to default to the language id)")]
pub struct InvalidServerId;

/// Unique identity of a configured LSP server within a workspace.
///
/// Derived from [`LspServerConfig::id`]: a server's explicit `name` if set,
/// otherwise its `language_id`. This is the key used throughout the bridge
/// layer (the translator's server slots, notification receivers)
/// instead of a raw language string, so two servers sharing a language no
/// longer silently overwrite each other.
///
/// Deserializes from a string and rejects a blank one, so the config file and
/// [`Self::new`] apply the same rule.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ServerId;
///
/// assert_eq!(ServerId::new("pyright").unwrap().as_str(), "pyright");
/// assert!(ServerId::new("  ").is_err());
/// ```
// TODO(D2): `From<&str>`/`From<String>` below can still build a blank id; replace them with
// `from_static` and migrate the fixtures (follow-up issue "ServerId still has infallible
// From<&str>/From<String> constructors").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, JsonSchema)]
pub struct ServerId(String);

impl ServerId {
    /// Builds an id from any string.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidServerId`] if `id` is blank.
    pub fn new(id: impl Into<String>) -> std::result::Result<Self, InvalidServerId> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(InvalidServerId);
        }
        Ok(Self(id))
    }

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

impl std::str::FromStr for ServerId {
    type Err = InvalidServerId;

    fn from_str(id: &str) -> std::result::Result<Self, Self::Err> {
        Self::new(id)
    }
}

impl<'de> Deserialize<'de> for ServerId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let id = String::deserialize(deserializer)?;
        Self::new(id).map_err(serde::de::Error::custom)
    }
}

impl From<String> for ServerId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<LanguageId> for ServerId {
    fn from(id: LanguageId) -> Self {
        Self(id.into())
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
/// having already synced the document to the *same* server. `TypeHierarchy`
/// covers `prepare_type_hierarchy`, `get_supertypes` and `get_subtypes` for
/// the same reason. `Rename` also covers `prepare_rename`, so the verdict
/// and the edit always come from the same server.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ToolKind;
///
/// assert_eq!(ToolKind::Hover.as_str(), "hover");
/// assert_eq!(ToolKind::ALL.len(), 21);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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
    /// `textDocument/rename` and `textDocument/prepareRename`.
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
    /// `textDocument/declaration`.
    Declaration,
    /// `textDocument/prepareTypeHierarchy`, `typeHierarchy/supertypes`, `typeHierarchy/subtypes`.
    TypeHierarchy,
    /// `textDocument/documentHighlight`.
    DocumentHighlights,
    /// `textDocument/rangeFormatting`.
    FormatRange,
    /// `textDocument/selectionRange`.
    SelectionRange,
    /// `textDocument/foldingRange`.
    FoldingRange,
}

impl ToolKind {
    /// Every routable tool, in a fixed order. Used to compute the §5
    /// coverage warning and to build error messages that enumerate tools.
    pub const ALL: &[Self] = &[
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
        Self::Declaration,
        Self::TypeHierarchy,
        Self::DocumentHighlights,
        Self::FormatRange,
        Self::SelectionRange,
        Self::FoldingRange,
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
            Self::Declaration => "declaration",
            Self::TypeHierarchy => "type_hierarchy",
            Self::DocumentHighlights => "document_highlights",
            Self::FormatRange => "format_range",
            Self::SelectionRange => "selection_range",
            Self::FoldingRange => "folding_range",
        }
    }
}

impl std::fmt::Display for ToolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a list of tools is not a valid [`ToolSet`].
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidToolSet {
    /// The list was empty.
    #[error("handles cannot be empty (omit `handles` for a catch-all server)")]
    Empty,
    /// A tool appeared more than once.
    #[error("duplicate tool '{0}' in `handles`")]
    Duplicate(ToolKind),
}

/// The non-empty, duplicate-free tools one server is restricted to.
///
/// Deserializes from a list of tool names and rejects an empty list or a
/// repeated tool at load time.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::{ToolKind, ToolSet};
///
/// let set = ToolSet::new(vec![ToolKind::Hover, ToolKind::Rename]).unwrap();
/// assert!(set.contains(ToolKind::Hover));
/// assert!(ToolSet::new(vec![]).is_err());
/// assert!(ToolSet::new(vec![ToolKind::Hover, ToolKind::Hover]).is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<ToolKind>", into = "Vec<ToolKind>")]
pub struct ToolSet(Vec<ToolKind>);

impl ToolSet {
    /// Builds a set from `tools`, preserving their order.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidToolSet::Empty`] for an empty list and
    /// [`InvalidToolSet::Duplicate`] for the first repeated tool.
    pub fn new(tools: Vec<ToolKind>) -> std::result::Result<Self, InvalidToolSet> {
        if tools.is_empty() {
            return Err(InvalidToolSet::Empty);
        }
        let mut seen = HashSet::new();
        if let Some(tool) = tools.iter().find(|tool| !seen.insert(**tool)) {
            return Err(InvalidToolSet::Duplicate(*tool));
        }
        Ok(Self(tools))
    }

    /// A set holding exactly `tool`.
    #[must_use]
    pub fn single(tool: ToolKind) -> Self {
        Self(vec![tool])
    }

    /// Whether `tool` is in the set.
    #[must_use]
    pub fn contains(&self, tool: ToolKind) -> bool {
        self.0.contains(&tool)
    }

    /// The tools in configured order; never empty.
    pub fn iter(&self) -> std::slice::Iter<'_, ToolKind> {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a ToolSet {
    type Item = &'a ToolKind;
    type IntoIter = std::slice::Iter<'a, ToolKind>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl TryFrom<Vec<ToolKind>> for ToolSet {
    type Error = InvalidToolSet;

    fn try_from(tools: Vec<ToolKind>) -> std::result::Result<Self, Self::Error> {
        Self::new(tools)
    }
}

impl From<ToolSet> for Vec<ToolKind> {
    fn from(set: ToolSet) -> Self {
        set.0
    }
}

/// Describe a `[[lsp_servers]]` entry for use in error messages that must let
/// a user tell apart two entries sharing the same [`ServerId`] — the id
/// alone is useless there, since it's exactly what collided.
///
/// Deliberately does not include a positional index: [`ToolRouter::from_configs`]
/// only ever sees the post-heuristics *applicable* subset for a given
/// workspace, not the raw `[[lsp_servers]]` array, so a printed index would
/// usually name the wrong TOML entry (misleading, worse than omitting it).
/// `command`/`args` distinguish the entries instead; when two entries are
/// truly identical in every visible field, the description is the same for
/// both halves, which is an honest reflection of the ambiguity.
fn describe_entry(cfg: &LspServerConfig) -> String {
    if cfg.args.is_empty() {
        format!("language '{}', command '{}'", cfg.language_id, cfg.command)
    } else {
        format!(
            "language '{}', command '{}', args {:?}",
            cfg.language_id, cfg.command, cfg.args
        )
    }
}

/// Per-language routing table: which server handles which tool.
#[derive(Debug, Default, Clone)]
struct LanguageRoutes {
    /// Tools explicitly claimed via a server's `handles` list.
    explicit: HashMap<ToolKind, ServerId>,
    /// The single server (if any) that omitted `handles` — serves every
    /// tool not explicitly claimed by another server for this language.
    default: Option<ServerId>,
}

/// Why [`ToolRouter::resolve_any`] could not find a server for a
/// workspace-wide tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NoServerReason {
    /// No server is registered in this workspace at all. Reflects what has
    /// *registered* (i.e. finished spawning), not what is configured in
    /// `mcpls.toml` — a server that is still initializing, or one that was
    /// configured but failed to spawn, is indistinguishable from "nothing
    /// configured" at this layer. Callers with access to the set of servers
    /// still expected to register (e.g. an expected server slot) can
    /// tell these apart.
    NothingRegistered,
    /// At least one server is registered, but none explicitly claims the
    /// requested tool and none is a catch-all.
    NoClaimant,
}

/// How far one configured server's startup has progressed, as seen by
/// [`ToolRouter::rebind`].
///
/// # Examples
///
/// ```
/// use mcpls_core::config::{LanguageId, ServerId, ServerSettlement, ToolKind, ToolRouter};
///
/// let mut router = ToolRouter::catch_all([(ServerId::from("pyright"), LanguageId::from_static("python"))]);
/// router.rebind(|_| ServerSettlement::Failed);
/// assert!(router.resolve("python", ToolKind::Hover).is_none());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerSettlement {
    /// Still initializing: its routes stay as configured.
    Pending,
    /// Initialized and registered.
    Registered,
    /// Failed to start: routes naming it are redirected or dropped.
    Failed,
}

/// Resolves `(language, tool)` to the [`ServerId`] that should handle it.
///
/// Built once at startup by [`Self::from_configs`] over the *applicable*
/// (post-heuristics) server configs. The table in use is derived from that
/// immutable original by [`Self::rebind`] each time a server settles, so no
/// route ever points at a server known to have failed to spawn.
#[derive(Debug, Default, Clone)]
pub struct ToolRouter {
    by_language: HashMap<LanguageId, LanguageRoutes>,
    /// Config declaration order, used by `resolve_any` for a deterministic
    /// choice among candidates. Pruned to non-failed servers by `rebind`.
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
    /// Returns [`crate::error::Error::Config`] naming the conflicting entries if any
    /// of the three rules above is violated.
    pub fn from_configs<'a, I>(cfgs: I) -> Result<Self>
    where
        I: IntoIterator<Item = &'a LspServerConfig>,
    {
        let mut by_language: HashMap<LanguageId, LanguageRoutes> = HashMap::new();
        let mut order: Vec<ServerId> = Vec::new();
        let mut seen_ids: HashMap<ServerId, String> = HashMap::new();

        for cfg in cfgs {
            let id = cfg.id();

            if let Some(prev_description) = seen_ids.get(&id) {
                return Err(ConfigError::DuplicateServerId {
                    id,
                    first: prev_description.clone(),
                    second: describe_entry(cfg),
                }
                .into());
            }
            seen_ids.insert(id.clone(), describe_entry(cfg));
            order.push(id.clone());

            let routes = by_language.entry(cfg.language_id.clone()).or_default();

            match &cfg.handles {
                None => {
                    if let Some(existing) = &routes.default {
                        return Err(ConfigError::TwoCatchAllServers {
                            language: cfg.language_id.clone(),
                            existing: existing.clone(),
                            id,
                        }
                        .into());
                    }
                    routes.default = Some(id);
                }
                Some(tools) => {
                    for tool in tools {
                        if let Some(existing) = routes.explicit.get(tool) {
                            return Err(ConfigError::ToolClaimedTwice {
                                tool: *tool,
                                language: cfg.language_id.clone(),
                                existing: existing.clone(),
                                id,
                            }
                            .into());
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
        I: IntoIterator<Item = (ServerId, LanguageId)>,
    {
        let mut by_language: HashMap<LanguageId, LanguageRoutes> = HashMap::new();
        let mut order = Vec::new();
        for (id, language) in entries {
            order.push(id.clone());
            by_language.entry(language).or_default().default = Some(id);
        }
        Self { by_language, order }
    }

    /// Derives the active routes from the configured ones and the current
    /// [`ServerSettlement`] of every server; a pure function of those, so the
    /// result never depends on the order in which servers settled, and once
    /// none is `Pending` it equals what a single batch rebind produces.
    ///
    /// Per language:
    ///
    /// - an explicit route to a `Registered` or `Pending` server is kept;
    /// - an explicit route to a `Failed` server is rebound to the language's
    ///   catch-all when that is `Registered`, kept pointing at the failed
    ///   server while the catch-all is `Pending` (the route is never bound to
    ///   a server that has not registered), and dropped when the catch-all is
    ///   `Failed` or absent;
    /// - a `Failed` catch-all is dropped;
    /// - a dead route is never rebound to a *narrowly-scoped* live server: a
    ///   server that declared `handles = [...]` has explicitly declined every
    ///   other tool, and conscripting it would override that declaration (and,
    ///   via the diagnostics cache filter, start caching diagnostics the user
    ///   deliberately routed away).
    ///
    /// The server order used by [`Self::resolve_any`] keeps every server that
    /// is not `Failed`, so a pending server's position is preserved.
    ///
    /// Silent: callers log the server that just settled, not every earlier
    /// failure again.
    pub fn rebind(&mut self, settlement: impl Fn(&ServerId) -> ServerSettlement) {
        for routes in self.by_language.values_mut() {
            let catch_all = routes.default.clone().map(|id| (settlement(&id), id));

            let dead: Vec<ToolKind> = routes
                .explicit
                .iter()
                .filter(|(_, id)| settlement(id) == ServerSettlement::Failed)
                .map(|(tool, _)| *tool)
                .collect();
            for tool in dead {
                match &catch_all {
                    Some((ServerSettlement::Registered, id)) => {
                        routes.explicit.insert(tool, id.clone());
                    }
                    Some((ServerSettlement::Pending, _)) => {}
                    Some((ServerSettlement::Failed, _)) | None => {
                        routes.explicit.remove(&tool);
                    }
                }
            }

            if matches!(catch_all, Some((ServerSettlement::Failed, _))) {
                routes.default = None;
            }
        }

        self.order
            .retain(|id| settlement(id) != ServerSettlement::Failed);
    }

    /// [`Self::rebind`] for a finished startup: every server in `registered`
    /// is `Registered`, every other one `Failed`.
    pub fn rebind_to_registered(&mut self, registered: &HashSet<ServerId>) {
        self.rebind(|id| {
            if registered.contains(id) {
                ServerSettlement::Registered
            } else {
                ServerSettlement::Failed
            }
        });
    }

    /// The configured languages with a route (explicit or catch-all) naming
    /// `id`, sorted.
    #[must_use]
    pub fn languages_routed_to(&self, id: &ServerId) -> Vec<LanguageId> {
        let mut languages: Vec<LanguageId> = self
            .by_language
            .iter()
            .filter(|(_, routes)| {
                routes.default.as_ref() == Some(id) || routes.explicit.values().any(|v| v == id)
            })
            .map(|(language, _)| language.clone())
            .collect();
        languages.sort_unstable();
        languages
    }

    /// The catch-all server configured for `language_id`, if any.
    #[must_use]
    pub fn catch_all_for(&self, language_id: &str) -> Option<&ServerId> {
        self.by_language.get(language_id)?.default.as_ref()
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
    /// Resolves in two tiers, in config declaration order:
    /// 1. the first server that explicitly claims `tool`;
    /// 2. else the first catch-all server.
    ///
    /// Deliberately does *not* fall back to "the first server at all" when
    /// neither tier matches: a server with a `handles` list has explicitly
    /// declined every tool not on it, so forwarding an unclaimed workspace-wide
    /// tool to it anyway would silently violate that declaration. Callers get
    /// [`NoServerReason`] instead, distinguishing "nothing configured" from
    /// "something is configured but nothing claims this tool" so they can
    /// report a precise error rather than defaulting to an arbitrary server.
    ///
    /// # Errors
    ///
    /// Returns [`NoServerReason::NothingRegistered`] if no server is
    /// registered at all, or [`NoServerReason::NoClaimant`] if servers are
    /// registered but none explicitly claims `tool` and none is a catch-all.
    pub fn resolve_any(&self, tool: ToolKind) -> std::result::Result<&ServerId, NoServerReason> {
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
            .ok_or(if self.order.is_empty() {
                NoServerReason::NothingRegistered
            } else {
                NoServerReason::NoClaimant
            })
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

    /// Every language this router was built with, sorted, including
    /// languages whose routes [`Self::rebind_to_registered`] has since
    /// emptied -- so a language whose servers all failed to spawn is still
    /// listed (and can be reported as having no server) rather than vanishing.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::config::{LanguageId, ServerId, ToolRouter};
    ///
    /// let router = ToolRouter::catch_all([
    ///     (ServerId::from("pyright"), LanguageId::from_static("python")),
    ///     (ServerId::from("rust-analyzer"), LanguageId::from_static("rust")),
    /// ]);
    /// assert_eq!(router.configured_languages(), ["python", "rust"]);
    /// ```
    #[must_use]
    pub fn configured_languages(&self) -> Vec<LanguageId> {
        let mut languages: Vec<LanguageId> = self.by_language.keys().cloned().collect();
        languages.sort_unstable();
        languages
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::config::{ServerCommand, TimeoutSecs};
    use crate::error::Error;

    #[test]
    fn test_server_id_new_rejects_blank_and_deserialization_agrees() {
        assert_eq!(ServerId::new(""), Err(InvalidServerId));
        assert_eq!(ServerId::new("  \t"), Err(InvalidServerId));
        assert_eq!("pyright".parse::<ServerId>().unwrap().as_str(), "pyright");
        assert!(serde_json::from_str::<ServerId>("\" \"").is_err());
        assert_eq!(
            serde_json::from_str::<ServerId>("\"pylsp\"").unwrap(),
            ServerId::from("pylsp")
        );
    }

    #[test]
    fn test_tool_set_rejects_empty_and_duplicates() {
        assert_eq!(ToolSet::new(vec![]), Err(InvalidToolSet::Empty));
        assert_eq!(
            ToolSet::new(vec![ToolKind::Hover, ToolKind::Rename, ToolKind::Hover]),
            Err(InvalidToolSet::Duplicate(ToolKind::Hover))
        );
    }

    #[test]
    fn test_tool_set_preserves_order_and_round_trips() {
        let set = ToolSet::new(vec![ToolKind::Rename, ToolKind::Hover]).unwrap();
        assert_eq!(
            set.iter().copied().collect::<Vec<_>>(),
            [ToolKind::Rename, ToolKind::Hover]
        );
        assert!(set.contains(ToolKind::Hover));
        assert!(!set.contains(ToolKind::Definition));
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(json, r#"["rename","hover"]"#);
        assert_eq!(serde_json::from_str::<ToolSet>(&json).unwrap(), set);
        assert!(serde_json::from_str::<ToolSet>("[]").is_err());
    }

    fn cfg(
        language_id: &str,
        name: Option<&str>,
        handles: Option<Vec<ToolKind>>,
    ) -> LspServerConfig {
        LspServerConfig {
            language_id: LanguageId::new(language_id).unwrap(),
            command: ServerCommand::from_static("cmd"),
            args: vec![],
            env: HashMap::new(),
            file_patterns: vec![],
            initialization_options: None,
            settings: None,
            timeout_seconds: TimeoutSecs::new(30).unwrap(),
            request_timeout_seconds: TimeoutSecs::new(30).unwrap(),
            heuristics: None,
            name: name.map(ServerId::from),
            handles: handles.map(|tools| ToolSet::new(tools).unwrap()),
            indexing: crate::bridge::IndexingPolicy::Auto,
            selection: crate::config::ServerSelection::Explicit,
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
            Ok(&ServerId::from("rust-catch-all"))
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
            Ok(&ServerId::from("python-explicit"))
        );
    }

    #[test]
    fn test_from_configs_rejects_duplicate_server_id_across_languages() {
        let configs = vec![
            cfg("python", None, None),
            cfg("typescript", Some("python"), None),
        ];
        let err = ToolRouter::from_configs(&configs).unwrap_err();
        assert_matches!(err, Error::Config(ConfigError::DuplicateServerId { .. }));
    }

    #[test]
    fn test_from_configs_duplicate_server_id_error_distinguishes_entries() {
        // Two `[[lsp_servers]]` entries sharing `language_id = "rust"` with
        // neither setting `name`: both resolve to the same ServerId, which
        // used to make the error message name both conflicting halves
        // identically ("used by both the 'rust' and 'rust' language
        // entries"). The message must let a user tell the two entries apart.
        let configs = vec![
            LspServerConfig {
                language_id: LanguageId::from_static("rust"),
                command: ServerCommand::from_static("rust-analyzer"),
                args: vec![],
                env: HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                settings: None,
                timeout_seconds: TimeoutSecs::new(30).unwrap(),
                request_timeout_seconds: TimeoutSecs::new(30).unwrap(),
                heuristics: None,
                name: None,
                handles: None,
                indexing: crate::bridge::IndexingPolicy::Auto,
                selection: crate::config::ServerSelection::Explicit,
            },
            LspServerConfig {
                language_id: LanguageId::from_static("rust"),
                command: ServerCommand::from_static("rust-analyzer"),
                args: vec!["--dummy-second-instance".to_string()],
                env: HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                settings: None,
                timeout_seconds: TimeoutSecs::new(30).unwrap(),
                request_timeout_seconds: TimeoutSecs::new(30).unwrap(),
                heuristics: None,
                name: None,
                handles: None,
                indexing: crate::bridge::IndexingPolicy::Auto,
                selection: crate::config::ServerSelection::Explicit,
            },
        ];
        let err = ToolRouter::from_configs(&configs).unwrap_err();
        let Error::Config(config_err) = err else {
            panic!("expected Config, got {err:?}");
        };
        let msg = config_err.to_string();
        // Must not print a positional index: `from_configs` only ever sees
        // the post-heuristics applicable subset, so any "entry #N" would
        // usually name the wrong `[[lsp_servers]]` array position.
        assert!(!msg.contains("entry #"), "message was: {msg}");
        assert!(msg.contains("rust-analyzer"), "message was: {msg}");
        assert!(
            msg.contains("--dummy-second-instance"),
            "message was: {msg}"
        );
    }

    #[test]
    fn test_from_configs_duplicate_server_id_error_identical_entries_still_reports() {
        // When two colliding entries are identical in every visible field,
        // there's nothing left to distinguish them by; the message should
        // still name the collision (both halves read the same) rather than
        // fabricate a misleading index.
        let configs = vec![cfg("rust", None, None), cfg("rust", None, None)];
        let err = ToolRouter::from_configs(&configs).unwrap_err();
        let Error::Config(config_err) = err else {
            panic!("expected Config, got {err:?}");
        };
        let msg = config_err.to_string();
        assert!(!msg.contains("entry #"), "message was: {msg}");
        assert!(
            msg.contains("duplicate server id 'rust'"),
            "message was: {msg}"
        );
    }

    #[test]
    fn test_from_configs_rejects_two_catch_alls() {
        let configs = vec![
            cfg("python", Some("a"), None),
            cfg("python", Some("b"), None),
        ];
        let err = ToolRouter::from_configs(&configs).unwrap_err();
        assert_matches!(err, Error::Config(ConfigError::TwoCatchAllServers { .. }));
    }

    #[test]
    fn test_from_configs_rejects_duplicate_tool_claim() {
        let configs = vec![
            cfg("python", Some("a"), Some(vec![ToolKind::Hover])),
            cfg("python", Some("b"), Some(vec![ToolKind::Hover])),
        ];
        let err = ToolRouter::from_configs(&configs).unwrap_err();
        assert_matches!(err, Error::Config(ConfigError::ToolClaimedTwice { .. }));
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
        assert_eq!(
            router.resolve_any(ToolKind::Hover),
            Err(NoServerReason::NothingRegistered)
        );
        // A single-server-per-language config whose server fails to spawn
        // must report NoServerForLanguage upstream, not NoServerForTool --
        // has_language must go back to false once every route is dropped.
        assert!(!router.has_language("rust"));
    }

    fn explicit_and_catch_all_router() -> ToolRouter {
        ToolRouter::from_configs(&[
            cfg("python", Some("pyright"), Some(vec![ToolKind::Hover])),
            cfg("python", Some("pylsp"), None),
        ])
        .unwrap()
    }

    #[test]
    fn rebind_keeps_pending_routes_and_order() {
        let mut router = ToolRouter::from_configs(&[
            cfg("rust", Some("slow"), None),
            cfg("python", Some("fast"), None),
        ])
        .unwrap();

        router.rebind(|id| {
            if id.as_str() == "fast" {
                ServerSettlement::Registered
            } else {
                ServerSettlement::Pending
            }
        });

        assert_eq!(
            router.resolve("rust", ToolKind::Hover),
            Some(&ServerId::from("slow"))
        );
        assert_eq!(
            router.resolve_any(ToolKind::Hover),
            Ok(&ServerId::from("slow"))
        );
    }

    #[test]
    fn dead_explicit_route_waits_for_pending_catch_all_and_drops_when_it_fails() {
        let settle = |pyright, pylsp| {
            let mut router = explicit_and_catch_all_router();
            router.rebind(|id| {
                if id.as_str() == "pyright" {
                    pyright
                } else {
                    pylsp
                }
            });
            router.resolve("python", ToolKind::Hover).cloned()
        };

        let pending_catch_all = settle(ServerSettlement::Failed, ServerSettlement::Pending);
        assert_eq!(pending_catch_all, Some(ServerId::from("pyright")));
        let registered_catch_all = settle(ServerSettlement::Failed, ServerSettlement::Registered);
        assert_eq!(registered_catch_all, Some(ServerId::from("pylsp")));
        let failed_catch_all = settle(ServerSettlement::Failed, ServerSettlement::Failed);
        assert_eq!(failed_catch_all, None);
    }

    #[test]
    fn rebind_without_pending_servers_matches_the_batch_rule_for_every_outcome() {
        for pyright_up in [false, true] {
            for pylsp_up in [false, true] {
                let registered: HashSet<ServerId> = [("pyright", pyright_up), ("pylsp", pylsp_up)]
                    .into_iter()
                    .filter(|(_, up)| *up)
                    .map(|(id, _)| ServerId::from(id))
                    .collect();
                let mut router = explicit_and_catch_all_router();
                router.rebind_to_registered(&registered);

                let hover = if pyright_up {
                    Some("pyright")
                } else if pylsp_up {
                    Some("pylsp")
                } else {
                    None
                };
                assert_eq!(
                    router
                        .resolve("python", ToolKind::Hover)
                        .map(ServerId::as_str),
                    hover,
                    "pyright up: {pyright_up}, pylsp up: {pylsp_up}"
                );
                assert_eq!(
                    router
                        .resolve("python", ToolKind::Diagnostics)
                        .map(ServerId::as_str),
                    pylsp_up.then_some("pylsp")
                );
            }
        }
    }

    #[test]
    fn test_rebind_prunes_order_for_resolve_any() {
        let configs = vec![cfg("rust", Some("a"), None), cfg("python", Some("b"), None)];
        let mut router = ToolRouter::from_configs(&configs).unwrap();
        let registered: HashSet<ServerId> = HashSet::from([ServerId::from("b")]);
        router.rebind_to_registered(&registered);
        assert_eq!(
            router.resolve_any(ToolKind::Hover),
            Ok(&ServerId::from("b"))
        );
    }

    #[test]
    fn test_resolve_any_no_claimant_does_not_fall_back_to_arbitrary_server() {
        // A single narrowly-scoped server that does not claim WorkspaceSymbols
        // and has no catch-all anywhere must not be silently conscripted for
        // it -- that would violate its explicit `handles` declaration.
        let configs = vec![cfg("python", Some("pyright"), Some(vec![ToolKind::Hover]))];
        let router = ToolRouter::from_configs(&configs).unwrap();
        assert_eq!(
            router.resolve_any(ToolKind::WorkspaceSymbols),
            Err(NoServerReason::NoClaimant)
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
            (ServerId::from("ts"), LanguageId::from_static("typescript")),
            (
                ServerId::from("tsx"),
                LanguageId::from_static("typescriptreact"),
            ),
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
        assert_eq!(ToolKind::ALL.len(), 21);

        // `ALL` is a slice now, so nothing pins its element count at compile
        // time the way `[Self; 15]` used to -- guard against duplicate or
        // missing entries at runtime instead.
        let unique_names: std::collections::HashSet<&str> =
            ToolKind::ALL.iter().map(ToolKind::as_str).collect();
        assert_eq!(unique_names.len(), ToolKind::ALL.len());
    }

    #[test]
    fn test_server_id_display_and_as_str() {
        let id = ServerId::from("pyright");
        assert_eq!(id.as_str(), "pyright");
        assert_eq!(id.to_string(), "pyright");
    }
}
