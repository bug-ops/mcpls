//! Client/server routing, document-open preparation, and capability gating
//! shared by every LSP-round-trip tool-call handler.

use std::path::{Path, PathBuf};

use super::Translator;
use crate::bridge::resources::{DiagnosticsResourceUri, parse_uri};
use crate::bridge::state::detect_language;
use crate::bridge::{InFlightGuard, WorkspaceRoots, lexically_normalize, lock_std};
use crate::config::{NoServerReason, ServerId, ToolKind, base_language_id};
use crate::error::{Error, Result, ServerSpawnFailure};
use crate::lsp::LspClient;

/// Maximum allowed position value for validation.
pub(super) const MAX_POSITION_VALUE: u32 = 1_000_000;

/// Maximum allowed range size in lines.
pub(super) const MAX_RANGE_LINES: u32 = 10_000;

/// Total time `Translator::flush_pending_closes` may spend per call.
const FLUSH_PENDING_CLOSES_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

/// A document opened for a handler's LSP round-trip, together with the
/// routed server and client.
///
/// Owns an [`InFlightGuard`], so the document cannot be evicted by the
/// document tracker's LRU while this value is alive (#503). Fields are
/// reachable only through borrowing accessors, so a handler must keep the
/// whole `PreparedDocument` bound until its request completes -- destructuring
/// it would drop the guard early.
#[derive(Debug)]
#[must_use = "dropping a PreparedDocument makes its document evictable mid-request"]
pub(super) struct PreparedDocument {
    server_id: ServerId,
    client: LspClient,
    uri: lsp_types::Uri,
    _in_flight: InFlightGuard,
}

impl PreparedDocument {
    pub(super) const fn server_id(&self) -> &ServerId {
        &self.server_id
    }

    pub(super) const fn client(&self) -> &LspClient {
        &self.client
    }

    pub(super) const fn uri(&self) -> &lsp_types::Uri {
        &self.uri
    }
}

/// Validate that `path` is within one of `workspace_roots`.
///
/// Free function (rather than a `Translator` method) so callers that only need
/// path validation — e.g. cache-only MCP handlers — can validate against a
/// cloned, lock-free snapshot of the workspace roots instead of locking the
/// full `Arc<Mutex<Translator>>`, which may be held elsewhere across a slow
/// in-flight LSP round-trip.
///
/// The lexical check (absolute, `.`/`..` resolved, against the canonical roots
/// and their aliases) is only a pre-filter: it rejects an out-of-workspace path
/// without touching the filesystem, and it can only reject, never accept. The
/// decision that counts uses the physical path: the original `path` is
/// canonicalized (so `..` after a symlink is resolved against the real
/// directory) and that canonical form must lie under a canonical root. The
/// returned path is the canonical one.
///
/// # Errors
///
/// Returns `Error::NoWorkspaceRoots` if `workspace_roots` is empty -- fails
/// closed rather than allowing unrestricted access -- and
/// `Error::PathOutsideWorkspace` if the path is outside all configured
/// workspace roots. `Error::FileIo` is returned when the path cannot be made
/// absolute (for example an empty path), or when the lexical check admitted it
/// but it cannot be canonicalized (for example it does not exist).
pub fn validate_path_against_roots(
    path: &Path,
    workspace_roots: &WorkspaceRoots,
) -> Result<PathBuf> {
    if workspace_roots.is_empty() {
        return Err(Error::NoWorkspaceRoots(path.to_path_buf()));
    }

    let io_error = |source| Error::FileIo {
        path: path.to_path_buf(),
        source,
    };
    let absolute = std::path::absolute(path).map_err(io_error)?;
    let normalized = lexically_normalize(dunce::simplified(&absolute));
    if !workspace_roots.admits_lexically(&normalized) {
        return Err(Error::PathOutsideWorkspace(path.to_path_buf()));
    }

    let canonical = dunce::canonicalize(path).map_err(io_error)?;
    if workspace_roots.contains_canonical(&canonical) {
        Ok(canonical)
    } else {
        Err(Error::PathOutsideWorkspace(path.to_path_buf()))
    }
}

/// Whether a [`Translator::prepare_gated_document`] call site also needs
/// [`Translator::wait_for_indexing_ready`] applied, declared explicitly at
/// the same place capability-gating is declared so a newly added (or newly
/// gated) tool can't silently ship without an indexing-readiness decision
/// either way.
///
/// This only covers call sites that actually go through
/// `prepare_gated_document` -- two production handlers bypass that
/// chokepoint entirely and so make no `IndexingGate` decision at all:
/// - `handle_workspace_symbol` (`workspace_symbol_search`) has no per-file
///   document to resolve or open (it resolves via `resolve_any` instead), so
///   it cannot be routed through this chokepoint as-is. Whether/how to gate
///   it on indexing readiness was deferred as a separate open question (spec
///   FR-008) and remains a known, deliberate limitation -- see #423.
/// - `handle_diagnostics` calls the ungated `Translator::prepare_document`
///   sibling directly, so it gets neither an indexing-readiness decision nor
///   a capability check. The indexing-readiness half of that is deliberate,
///   not an oversight (#445): unlike `handle_incoming_calls`/`handle_outgoing_calls`
///   (#423), it reads from the notification-cache poll path rather than
///   issuing a live whole-workspace LSP request, so blocking it on
///   `wait_for_indexing_ready` the way `Required` does for the other
///   handlers would be the wrong fix shape (it would stall a cache read on a
///   signal the cache itself doesn't need). Instead, the `get_diagnostics`
///   MCP tool (`mcp::server::get_diagnostics`) independently resolves the
///   file's diagnostics-route server and reports its indexing state as an
///   explicit `indexing_in_progress` flag on the response
///   (`mcp::server::DiagnosticsResponse`), so a mid-index pull (which can
///   read as "no errors" while rust-analyzer is still loading) is flagged
///   rather than silently trusted. The missing capability check was not
///   analyzed as part of #445 and remains an open question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IndexingGate {
    /// This tool's answer depends on whole-workspace analysis (e.g. hover,
    /// definition, references, rename, completions, code actions, call
    /// hierarchy incoming/outgoing calls).
    Required,
    /// This tool's answer is valid even mid-index (single-file analysis),
    /// e.g. `document_symbols`. `handle_call_hierarchy_prepare` also uses
    /// this variant, but not for the same reason: unlike `document_symbols`,
    /// `prepareCallHierarchy` does perform position-based name resolution
    /// (the same class of query as `textDocument/definition`, which *is*
    /// [`Self::Required`]) -- leaving it ungated is a deliberate scope
    /// decision for #423 (mid-index it degrades to an empty `prepare`
    /// result rather than an explicit error), not a claim that it is
    /// single-file analysis like `document_symbols`. The incoming/outgoing
    /// calls that follow `prepare` use [`Self::Required`].
    NotRequired,
}

/// An LSP server capability mcpls gates a tool on before dispatching its
/// request, tying the [`ServerCapabilities`](lsp_types::ServerCapabilities)
/// field name (used only for the error message, via [`Self::name`]) to the
/// predicate that actually checks it (via [`Self::is_supported`]) so the two
/// cannot drift apart the way two independent, hand-picked call-site values
/// could.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// `completionProvider` (`textDocument/completion`).
    Completions,
    /// `signatureHelpProvider` (`textDocument/signatureHelp`).
    SignatureHelp,
    /// `inlayHintProvider` (`textDocument/inlayHint`).
    InlayHints,
    /// `hoverProvider` (`textDocument/hover`).
    Hover,
    /// `definitionProvider` (`textDocument/definition`).
    Definition,
    /// `referencesProvider` (`textDocument/references`).
    References,
    /// `implementationProvider` (`textDocument/implementation`).
    Implementation,
    /// `typeDefinitionProvider` (`textDocument/typeDefinition`).
    TypeDefinition,
    /// `callHierarchyProvider` (`textDocument/prepareCallHierarchy`,
    /// `callHierarchy/incomingCalls`, `callHierarchy/outgoingCalls`).
    CallHierarchy,
    /// `renameProvider` (`textDocument/rename`).
    Rename,
    /// `documentFormattingProvider` (`textDocument/formatting`).
    FormatDocument,
    /// `codeActionProvider` (`textDocument/codeAction`).
    CodeActions,
    /// `documentSymbolProvider` (`textDocument/documentSymbol`).
    DocumentSymbols,
    /// `workspaceSymbolProvider` (`workspace/symbol`).
    WorkspaceSymbols,
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "compile-time check; the loop condition keeps `i` below `ALL.len()`"
)]
const _: () = {
    assert!(Capability::ALL.len() == Capability::WorkspaceSymbols as usize + 1);
    assert!(Capability::ALL.len() <= u16::BITS as usize);
    let mut i = 0;
    while i < Capability::ALL.len() {
        assert!(Capability::ALL[i] as usize == i);
        i += 1;
    }
};

impl Capability {
    /// Every capability, in discriminant order.
    pub(crate) const ALL: [Self; 14] = [
        Self::Completions,
        Self::SignatureHelp,
        Self::InlayHints,
        Self::Hover,
        Self::Definition,
        Self::References,
        Self::Implementation,
        Self::TypeDefinition,
        Self::CallHierarchy,
        Self::Rename,
        Self::FormatDocument,
        Self::CodeActions,
        Self::DocumentSymbols,
        Self::WorkspaceSymbols,
    ];

    /// The [`ToolKind`] whose route this capability gates -- the single
    /// source tying a gated handler's routing to its capability check.
    pub(crate) const fn tool_kind(self) -> ToolKind {
        match self {
            Self::Completions => ToolKind::Completions,
            Self::SignatureHelp => ToolKind::SignatureHelp,
            Self::InlayHints => ToolKind::InlayHints,
            Self::Hover => ToolKind::Hover,
            Self::Definition => ToolKind::Definition,
            Self::References => ToolKind::References,
            Self::Implementation => ToolKind::Implementation,
            Self::TypeDefinition => ToolKind::TypeDefinition,
            Self::CallHierarchy => ToolKind::CallHierarchy,
            Self::Rename => ToolKind::Rename,
            Self::FormatDocument => ToolKind::FormatDocument,
            Self::CodeActions => ToolKind::CodeActions,
            Self::DocumentSymbols => ToolKind::DocumentSymbols,
            Self::WorkspaceSymbols => ToolKind::WorkspaceSymbols,
        }
    }

    /// The capability gating `tool`, or `None` for a tool dispatched without
    /// a capability check (`Diagnostics`). Derived from [`Self::tool_kind`]
    /// over [`Self::ALL`], so the pairing has a single source.
    pub(crate) const fn for_tool(tool: ToolKind) -> Option<Self> {
        Self::find_gating(&Self::ALL, tool)
    }

    const fn find_gating(candidates: &[Self], tool: ToolKind) -> Option<Self> {
        match candidates {
            // `PartialEq` is not const, so compare discriminants.
            [first, rest @ ..] => {
                if first.tool_kind() as u8 == tool as u8 {
                    Some(*first)
                } else {
                    Self::find_gating(rest, tool)
                }
            }
            [] => None,
        }
    }

    /// Whether a request gated on this capability may be dispatched against
    /// a server with `caps`. Unknown capabilities (`None`) are assumed
    /// supported, mirroring [`Translator::require_capability`]'s fail-open
    /// stance.
    pub(crate) const fn is_available(self, caps: Option<&lsp_types::ServerCapabilities>) -> bool {
        match caps {
            Some(caps) => self.is_supported(caps),
            None => true,
        }
    }

    /// The `ServerCapabilities` field name, as reported to the MCP caller in
    /// [`Error::CapabilityNotSupported`].
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Completions => "completionProvider",
            Self::SignatureHelp => "signatureHelpProvider",
            Self::InlayHints => "inlayHintProvider",
            Self::Hover => "hoverProvider",
            Self::Definition => "definitionProvider",
            Self::References => "referencesProvider",
            Self::Implementation => "implementationProvider",
            Self::TypeDefinition => "typeDefinitionProvider",
            Self::CallHierarchy => "callHierarchyProvider",
            Self::Rename => "renameProvider",
            Self::FormatDocument => "documentFormattingProvider",
            Self::CodeActions => "codeActionProvider",
            Self::DocumentSymbols => "documentSymbolProvider",
            Self::WorkspaceSymbols => "workspaceSymbolProvider",
        }
    }

    /// Whether `caps` advertises support for this capability.
    pub(crate) const fn is_supported(self, caps: &lsp_types::ServerCapabilities) -> bool {
        match self {
            Self::Completions => caps.completion_provider.is_some(),
            Self::SignatureHelp => caps.signature_help_provider.is_some(),
            Self::InlayHints => matches!(
                caps.inlay_hint_provider,
                Some(
                    lsp_types::InlayHintProvider::Bool(true)
                        | lsp_types::InlayHintProvider::InlayHintOptions(_)
                        | lsp_types::InlayHintProvider::InlayHintRegistrationOptions(_)
                )
            ),
            Self::Hover => matches!(
                caps.hover_provider,
                Some(
                    lsp_types::HoverProvider::Bool(true)
                        | lsp_types::HoverProvider::HoverOptions(_)
                )
            ),
            Self::Definition => matches!(
                caps.definition_provider,
                Some(
                    lsp_types::DefinitionProvider::Bool(true)
                        | lsp_types::DefinitionProvider::DefinitionOptions(_)
                )
            ),
            Self::References => matches!(
                caps.references_provider,
                Some(
                    lsp_types::ReferencesProvider::Bool(true)
                        | lsp_types::ReferencesProvider::ReferenceOptions(_)
                )
            ),
            Self::Implementation => matches!(
                caps.implementation_provider,
                Some(
                    lsp_types::ImplementationProvider::Bool(true)
                        | lsp_types::ImplementationProvider::ImplementationOptions(_)
                        | lsp_types::ImplementationProvider::ImplementationRegistrationOptions(_)
                )
            ),
            Self::TypeDefinition => matches!(
                caps.type_definition_provider,
                Some(
                    lsp_types::TypeDefinitionProvider::Bool(true)
                        | lsp_types::TypeDefinitionProvider::TypeDefinitionOptions(_)
                        | lsp_types::TypeDefinitionProvider::TypeDefinitionRegistrationOptions(_)
                )
            ),
            Self::CallHierarchy => matches!(
                caps.call_hierarchy_provider,
                Some(
                    lsp_types::CallHierarchyProvider::Bool(true)
                        | lsp_types::CallHierarchyProvider::CallHierarchyOptions(_)
                        | lsp_types::CallHierarchyProvider::CallHierarchyRegistrationOptions(_)
                )
            ),
            Self::Rename => matches!(
                caps.rename_provider,
                Some(
                    lsp_types::RenameProvider::Bool(true)
                        | lsp_types::RenameProvider::RenameOptions(_)
                )
            ),
            Self::FormatDocument => matches!(
                caps.document_formatting_provider,
                Some(
                    lsp_types::DocumentFormattingProvider::Bool(true)
                        | lsp_types::DocumentFormattingProvider::DocumentFormattingOptions(_)
                )
            ),
            Self::CodeActions => matches!(
                caps.code_action_provider,
                Some(
                    lsp_types::CodeActionProvider::Bool(true)
                        | lsp_types::CodeActionProvider::CodeActionOptions(_)
                )
            ),
            Self::DocumentSymbols => matches!(
                caps.document_symbol_provider,
                Some(
                    lsp_types::DocumentSymbolProvider::Bool(true)
                        | lsp_types::DocumentSymbolProvider::DocumentSymbolOptions(_)
                )
            ),
            Self::WorkspaceSymbols => matches!(
                caps.workspace_symbol_provider,
                Some(
                    lsp_types::WorkspaceSymbolProvider::Bool(true)
                        | lsp_types::WorkspaceSymbolProvider::WorkspaceSymbolOptions(_)
                )
            ),
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl serde::Serialize for Capability {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

/// Serialized as its [`Capability::name`] string.
impl schemars::JsonSchema for Capability {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Capability".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "LSP server capability the tool is gated on."
        })
    }
}

/// A file's detected language plus its React base-language fallback
/// (`typescriptreact` -> `typescript`), in resolution order.
///
/// Single source of the candidate order for enforcement and the
/// `get_tool_support` snapshot, so an explicit `typescriptreact` server still
/// wins over the `typescript` fallback in both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LanguageCandidates {
    language: String,
    base: Option<&'static str>,
}

impl LanguageCandidates {
    pub(super) fn new(language: String) -> Self {
        let base = base_language_id(&language);
        Self { language, base }
    }

    pub(super) fn language(&self) -> &str {
        &self.language
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.language.as_str()).chain(self.base)
    }
}

/// Outcome of resolving a per-document tool route against the registries,
/// shared by enforcement ([`Translator::client_for_file`]) and the
/// `get_tool_support` snapshot so the two cannot disagree.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RouteLookup<T> {
    /// The routed server is registered; `T` is whatever the registry lookup returned.
    Registered(ServerId, T),
    /// The routed server is expected but has not registered yet.
    Initializing(ServerId),
    /// The router names a server that is neither registered nor expected.
    Dangling {
        language: String,
        server_id: ServerId,
    },
    /// No candidate language has a route for the tool.
    Unrouted,
}

/// Resolve the first candidate language with a route, then classify that
/// route's server. `resolve`, `registered` and `is_expected` are separate
/// closures so a caller backed by independent locks never holds two at once.
pub(super) fn lookup_route<T>(
    candidates: &LanguageCandidates,
    resolve: impl Fn(&str) -> Option<ServerId>,
    registered: impl Fn(&ServerId) -> Option<T>,
    is_expected: impl Fn(&ServerId) -> bool,
) -> RouteLookup<T> {
    for language in candidates.iter() {
        let Some(server_id) = resolve(language) else {
            continue;
        };
        return if let Some(found) = registered(&server_id) {
            RouteLookup::Registered(server_id, found)
        } else if is_expected(&server_id) {
            RouteLookup::Initializing(server_id)
        } else {
            RouteLookup::Dangling {
                language: language.to_string(),
                server_id,
            }
        };
    }
    RouteLookup::Unrouted
}

/// Where a file's diagnostics-route server stands, as seen by the cache-only
/// diagnostics readers.
///
/// Resolved by [`Translator::diagnostics_route_for_path`] with the same rules
/// as [`Translator::client_for_file`], so reading cached diagnostics cannot
/// disagree with the pull tool about whether a server exists.
#[derive(Debug)]
pub enum DiagnosticsRoute {
    /// The routed server is registered.
    Live(ServerId),
    /// The routed server is expected but has not registered yet.
    Initializing(ServerId),
    /// The server that would have served the file failed to start.
    FailedToStart(Box<ServerSpawnFailure>),
    /// No server is configured for the file's language.
    Unrouted,
}

impl DiagnosticsRoute {
    /// The routed server's id while it is registered or still starting.
    pub(crate) const fn server_id(&self) -> Option<&ServerId> {
        match self {
            Self::Live(id) | Self::Initializing(id) => Some(id),
            Self::FailedToStart(_) | Self::Unrouted => None,
        }
    }

    /// Whether the server failed to start.
    pub(crate) const fn is_failed(&self) -> bool {
        matches!(self, Self::FailedToStart(_))
    }

    /// The startup failure, if the server failed to start.
    ///
    /// For subscription paths, where a still-starting server is fine (updates
    /// will follow) but a failed one never publishes anything.
    pub(crate) fn into_startup_failure(self) -> Option<Box<ServerSpawnFailure>> {
        match self {
            Self::FailedToStart(failure) => Some(failure),
            Self::Live(_) | Self::Initializing(_) | Self::Unrouted => None,
        }
    }

    /// Gates a cache read on the route: the live server's id, or `None` when
    /// no server is configured (an empty cache is then the honest answer).
    ///
    /// # Errors
    ///
    /// [`Error::ServerInitializing`] while the server is still starting,
    /// [`Error::ServerFailedToStart`] once it failed to start.
    pub(crate) fn into_read_result(self) -> Result<Option<ServerId>> {
        match self {
            Self::Live(id) => Ok(Some(id)),
            Self::Unrouted => Ok(None),
            Self::Initializing(server_id) => Err(Error::ServerInitializing { server_id }),
            Self::FailedToStart(failure) => Err(Error::ServerFailedToStart(failure)),
        }
    }
}

/// Outcome of resolving a workspace-wide tool route; see [`RouteLookup`].
#[derive(Debug, PartialEq, Eq)]
pub(super) enum WorkspaceRouteLookup {
    /// The resolved server is registered.
    Registered(ServerId),
    /// The resolved server is expected but has not registered yet.
    Initializing(ServerId),
    /// The router resolved a server that is neither registered nor expected.
    Dangling(ServerId),
    /// Nothing is registered yet, but servers are still expected.
    AllInitializing,
    /// Nothing is registered and nothing is expected.
    NothingConfigured,
    /// Servers are registered, but none claims the tool and none is a catch-all.
    NoClaimant,
}

/// Resolve a workspace-wide tool route and classify the resolved server.
///
/// The closures are separate so a caller backed by independent locks never
/// holds two at once, as with [`lookup_route`].
pub(super) fn lookup_workspace_route(
    resolve: impl FnOnce() -> std::result::Result<ServerId, NoServerReason>,
    is_registered: impl Fn(&ServerId) -> bool,
    is_expected: impl Fn(&ServerId) -> bool,
    expected_is_empty: impl FnOnce() -> bool,
) -> WorkspaceRouteLookup {
    match resolve() {
        Ok(id) if is_registered(&id) => WorkspaceRouteLookup::Registered(id),
        Ok(id) if is_expected(&id) => WorkspaceRouteLookup::Initializing(id),
        Ok(id) => WorkspaceRouteLookup::Dangling(id),
        Err(NoServerReason::NothingRegistered) if expected_is_empty() => {
            WorkspaceRouteLookup::NothingConfigured
        }
        Err(NoServerReason::NothingRegistered) => WorkspaceRouteLookup::AllInitializing,
        Err(NoServerReason::NoClaimant) => WorkspaceRouteLookup::NoClaimant,
    }
}

/// Fail with [`Error::CapabilityNotSupported`] if `caps` is known and does
/// not advertise `capability`.
pub(super) fn check_capability(
    server_id: &ServerId,
    caps: Option<&lsp_types::ServerCapabilities>,
    capability: Capability,
) -> Result<()> {
    if capability.is_available(caps) {
        Ok(())
    } else {
        Err(Error::CapabilityNotSupported {
            server_id: server_id.clone(),
            capability: capability.name(),
        })
    }
}

impl Translator {
    /// Validate that a path is within allowed workspace boundaries.
    ///
    /// # Errors
    ///
    /// Returns `Error::NoWorkspaceRoots` if no workspace roots are
    /// configured (fails closed), or `Error::PathOutsideWorkspace` if the
    /// path is outside all configured workspace roots.
    pub(crate) fn validate_path(&self, path: &Path) -> Result<PathBuf> {
        validate_path_against_roots(path, &self.workspace_roots)
    }

    /// Resolve the client and routing identity for `path`/`tool`, giving the
    /// resolved server a chance to be respawned first if its process has
    /// died.
    ///
    /// Thin async wrapper around [`Self::client_for_file`] (kept
    /// synchronous so its existing unit tests don't need a runtime): this is
    /// the entry point async handlers call instead, so a dead server is
    /// transparently replaced before its stale client is handed back.
    pub(super) async fn resolve_client_for_file(
        &self,
        path: &Path,
        tool: ToolKind,
    ) -> Result<(ServerId, LspClient)> {
        let (id, client) = self.client_for_file(path, tool)?;
        self.respawn_if_dead(&id).await?;
        let client = lock_std(&self.lsp_clients)
            .get(&id)
            .cloned()
            .unwrap_or(client);
        Ok((id, client))
    }

    /// Resolve the server that should handle `tool` for the file at `path`,
    /// returning both its routing identity and a cloned client.
    ///
    /// Tries the file's detected language first, then (if that has no route)
    /// its React base language (`.tsx` falling back from `typescriptreact` to
    /// `typescript`, and similarly for `.jsx`) -- in that order, so an
    /// explicit `typescriptreact` server still wins over the `typescript`
    /// fallback when both are configured.
    ///
    /// Locks `router`, `lsp_clients`, and (on the not-yet-registered path)
    /// `expected_servers` only for their respective lookups — every guard is
    /// dropped before this method returns.
    pub(super) fn client_for_file(
        &self,
        path: &Path,
        tool: ToolKind,
    ) -> Result<(ServerId, LspClient)> {
        let candidates = self.language_candidates(path);
        let lookup = lookup_route(
            &candidates,
            |lang| lock_std(&self.router).resolve(lang, tool).cloned(),
            |id| lock_std(&self.lsp_clients).get(id).cloned(),
            |id| lock_std(&self.expected_servers).contains(id),
        );
        match lookup {
            RouteLookup::Registered(id, client) => Ok((id, client)),
            // A route naming a server that is still initializing (e.g. a
            // large Unity solution loading via OmniSharp) -- tell the caller
            // to wait and retry rather than implying no server is configured.
            RouteLookup::Initializing(server_id) => Err(Error::ServerInitializing { server_id }),
            // Unreachable once registration has rebound the router
            // (`Translator::rebind_router`) -- a route can only name a
            // registered server after that point. Logged rather than
            // `debug_assert!`-panicked: this method is reachable by any
            // library consumer calling `with_router` without registering
            // matching clients, not just internal misuse.
            RouteLookup::Dangling {
                language,
                server_id,
            } => {
                tracing::error!(
                    "router route names server '{server_id}' for tool '{tool}' that is neither \
                     registered nor expected"
                );
                Err(Error::NoServerForTool {
                    language_id: language,
                    tool,
                })
            }
            RouteLookup::Unrouted => {
                if let Some(failure) = self.startup_failure_for_candidates(&candidates, tool) {
                    return Err(Error::ServerFailedToStart(Box::new(failure)));
                }
                let has_language = {
                    let router = lock_std(&self.router);
                    candidates.iter().any(|lang| router.has_language(lang))
                };
                let language = candidates.language().to_string();
                if has_language {
                    Err(Error::NoServerForTool {
                        language_id: language,
                        tool,
                    })
                } else {
                    Err(Error::NoServerForLanguage(language))
                }
            }
        }
    }

    /// The detected language of `path` plus its React base-language fallback.
    pub(super) fn language_candidates(&self, path: &Path) -> LanguageCandidates {
        LanguageCandidates::new(detect_language(path, &self.extension_map))
    }

    /// The startup failure of the server the pre-rebind routing table would
    /// have used for `tool` in the first of `languages` that has one.
    ///
    /// Only consulted once no live route exists: a surviving catch-all
    /// still serves the request, so a failed explicit server never masks it.
    fn startup_failure_for_candidates(
        &self,
        candidates: &LanguageCandidates,
        tool: ToolKind,
    ) -> Option<ServerSpawnFailure> {
        candidates.iter().find_map(|lang| {
            let id = self.configured_router.resolve(lang, tool)?;
            self.startup_failure(id)
        })
    }

    /// Classify the diagnostics-route server for `path`'s detected language.
    ///
    /// Mirrors [`Self::client_for_file`]'s language-candidate order and
    /// registered/expected/failed classification, so a cache-only caller
    /// (`get_cached_diagnostics`, the diagnostics resource) can tell an empty
    /// cache from a server that is still starting or never started.
    ///
    /// The id of a [`DiagnosticsRoute::Live`] route is stable across a
    /// respawn (only the registered client behind it changes), unlike
    /// `NotificationCache::diagnostics_owner`, which a respawn clears -- so it
    /// is what the push-degraded flag (#359) is keyed on.
    #[must_use]
    pub(crate) fn diagnostics_route_for_path(&self, path: &Path) -> DiagnosticsRoute {
        let candidates = self.language_candidates(path);
        let lookup = lookup_route(
            &candidates,
            |lang| {
                lock_std(&self.router)
                    .resolve(lang, ToolKind::Diagnostics)
                    .cloned()
            },
            |id| lock_std(&self.lsp_clients).contains_key(id).then_some(()),
            |id| lock_std(&self.expected_servers).contains(id),
        );
        match lookup {
            RouteLookup::Registered(id, ()) => DiagnosticsRoute::Live(id),
            RouteLookup::Initializing(id) => DiagnosticsRoute::Initializing(id),
            RouteLookup::Dangling { server_id, .. } => {
                tracing::error!(
                    "router diagnostics route names server '{server_id}' that is neither \
                     registered nor expected"
                );
                DiagnosticsRoute::Unrouted
            }
            RouteLookup::Unrouted => self
                .startup_failure_for_candidates(&candidates, ToolKind::Diagnostics)
                .map_or(DiagnosticsRoute::Unrouted, |failure| {
                    DiagnosticsRoute::FailedToStart(Box::new(failure))
                }),
        }
    }

    /// As [`Self::diagnostics_route_for_path`], for a canonical diagnostics
    /// resource URI; a URI that does not parse is [`DiagnosticsRoute::Unrouted`].
    #[must_use]
    pub(crate) fn diagnostics_route_for_uri(
        &self,
        uri: &DiagnosticsResourceUri,
    ) -> DiagnosticsRoute {
        parse_uri(uri.as_str()).map_or(DiagnosticsRoute::Unrouted, |path| {
            self.diagnostics_route_for_path(&path)
        })
    }

    /// Validate `file_path`, then resolve its routed client via
    /// [`Self::resolve_client_for_file`] (respawn-aware), without opening
    /// the document.
    ///
    /// Split out from [`Self::prepare_document`] so [`Self::prepare_gated_document`]
    /// can check the routed server's capabilities *before* `ensure_open` sends
    /// `textDocument/didOpen` -- a server rejected by the gate should never
    /// observe an open notification for a request it can't service.
    async fn resolve_validated_client_for_file(
        &self,
        file_path: &str,
        tool: ToolKind,
    ) -> Result<(ServerId, LspClient, PathBuf)> {
        let validated_path = self.validate_path(Path::new(file_path))?;
        let (server_id, client) = self.resolve_client_for_file(&validated_path, tool).await?;
        Ok((server_id, client, validated_path))
    }

    /// As [`Self::resolve_validated_client_for_file`], but for a caller that
    /// already has a `&Path` it validated itself (e.g. `parse_file_uri`'s
    /// return value). `path` is trusted to already be validated -- this does
    /// *not* re-`canonicalize`/re-check it against workspace roots, unlike
    /// the `&str` overload above, which always validates an untrusted MCP
    /// input from scratch.
    async fn resolve_validated_client_for_path(
        &self,
        path: &Path,
        tool: ToolKind,
    ) -> Result<(ServerId, LspClient, PathBuf)> {
        let (server_id, client) = self.resolve_client_for_file(path, tool).await?;
        Ok((server_id, client, path.to_path_buf()))
    }

    /// Resolve the LSP client and ensure the document is open.
    ///
    /// This is the "prepare" phase shared by every LSP-round-trip handler:
    /// it validates the path, selects the client via
    /// [`Self::resolve_validated_client_for_file`] (respawn-aware), and
    /// calls `ensure_open`, which locks the document tracker's state only
    /// for the given path. The returned client and URI are owned values, so
    /// the caller can issue the actual LSP request (the "execute" phase)
    /// without holding any lock across the network round trip.
    ///
    /// `ensure_open`'s own awaits (a `stat`, optionally a re-read of the
    /// file, and the `textDocument/didOpen`/`didChange` notify) run under a
    /// lock scoped to `validated_path` alone — see [`DocumentTracker::ensure_open`]
    /// — so a slow or wedged language server cannot stall `prepare_document`
    /// calls for unrelated files. (Per-tool routing, #228, means the same
    /// file can be routed to more than one server; a wedged server-A notify
    /// still holds this path's lock and can therefore delay a healthy
    /// server-B call for that *same* file.)
    pub(super) async fn prepare_document(
        &self,
        file_path: &str,
        tool: ToolKind,
    ) -> Result<PreparedDocument> {
        let (server_id, client, validated_path) = self
            .resolve_validated_client_for_file(file_path, tool)
            .await?;
        self.open_prepared(server_id, client, &validated_path).await
    }

    /// Marks `validated_path` in flight, then opens it via `ensure_open`.
    ///
    /// The guard is taken before `ensure_open` so there is no window between
    /// the path lock releasing and the guard taking effect (#503); it is
    /// dropped with the error on failure.
    async fn open_prepared(
        &self,
        server_id: ServerId,
        client: LspClient,
        validated_path: &Path,
    ) -> Result<PreparedDocument> {
        let in_flight = self.document_tracker.mark_in_flight(validated_path);
        // Drained unconditionally, before propagating `ensure_open`'s
        // result: even on its error path (e.g. a `didOpen`/`didChange`
        // notify failure), `DocumentTracker::open` may already have evicted
        // a *different*, unrelated document and queued its `didClose` --
        // returning early via `?` before this would lose that queued close,
        // leaving the tracker desynced from that server (#495 S5).
        let result = self
            .document_tracker
            .ensure_open(validated_path, &server_id, &client)
            .await;
        self.flush_pending_closes().await;
        let uri = result?;
        Ok(PreparedDocument {
            server_id,
            client,
            uri,
            _in_flight: in_flight,
        })
    }

    /// Like [`Self::prepare_document`], but checks `capability` against the
    /// routed server's `ServerCapabilities` *before* opening the document --
    /// see [`Self::resolve_client_for_file`]'s doc comment for why the
    /// ordering matters. When `indexing_gate` is
    /// [`IndexingGate::Required`], also waits for
    /// [`Self::wait_for_indexing_ready`] before opening the document, so a
    /// server still indexing never receives (or answers from) an opened
    /// document it would otherwise be asked about.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CapabilityNotSupported`] if the routed server's
    /// `ServerCapabilities` explicitly does not advertise `capability`, or
    /// [`Error::WorkspaceIndexing`] if `indexing_gate` is
    /// [`IndexingGate::Required`] and the server is still indexing.
    pub(super) async fn prepare_gated_document(
        &self,
        file_path: &str,
        capability: Capability,
        indexing_gate: IndexingGate,
    ) -> Result<PreparedDocument> {
        let (server_id, client, validated_path) = self
            .resolve_validated_client_for_file(file_path, capability.tool_kind())
            .await?;
        self.finish_prepare_gated_document(
            server_id,
            client,
            validated_path,
            capability,
            indexing_gate,
        )
        .await
    }

    /// As [`Self::prepare_gated_document`], but for a caller that already
    /// has a `&Path` it validated itself (e.g. `handle_incoming_calls`/`handle_outgoing_calls`,
    /// via `parse_file_uri`) -- see [`Self::resolve_validated_client_for_path`]'s
    /// doc for why this skips re-validation.
    pub(super) async fn prepare_gated_document_for_path(
        &self,
        path: &Path,
        capability: Capability,
        indexing_gate: IndexingGate,
    ) -> Result<PreparedDocument> {
        let (server_id, client, validated_path) = self
            .resolve_validated_client_for_path(path, capability.tool_kind())
            .await?;
        self.finish_prepare_gated_document(
            server_id,
            client,
            validated_path,
            capability,
            indexing_gate,
        )
        .await
    }

    /// Shared tail of [`Self::prepare_gated_document`] and
    /// [`Self::prepare_gated_document_for_path`], once each has resolved and
    /// validated its own path: capability-gate, indexing-gate, then open.
    async fn finish_prepare_gated_document(
        &self,
        server_id: ServerId,
        client: LspClient,
        validated_path: PathBuf,
        capability: Capability,
        indexing_gate: IndexingGate,
    ) -> Result<PreparedDocument> {
        self.require_capability(&server_id, capability)?;
        if indexing_gate == IndexingGate::Required {
            self.wait_for_indexing_ready(&server_id).await?;
        }
        self.open_prepared(server_id, client, &validated_path).await
    }

    /// Sends the `textDocument/didClose` notifications still owed after
    /// [`DocumentTracker::open`]'s LRU eviction (#495), so a server's own
    /// open-document set does not keep growing even though mcpls's own
    /// tracking has stopped counting the document.
    ///
    /// `DocumentTracker` has no access to any server's [`LspClient`] --
    /// `self.lsp_clients` is the registry for that, kept one layer up in
    /// `Translator` -- so this is the chokepoint that reconciles its pending
    /// closes against it. Called after every `ensure_open` that could have
    /// triggered eviction, unconditionally, even when `ensure_open` itself
    /// returned an error, so a different, already-evicted document's close
    /// is never lost on that path (#495 S5).
    ///
    /// Claims, notifies and releases one path at a time, so a busy path is
    /// skipped (it stays pending) and no claim outlives its own notifies.
    /// The whole flush shares one [`FLUSH_PENDING_CLOSES_DEADLINE`]: it runs
    /// inline on the request path, so a wedged server must not add latency to
    /// unrelated calls. Paths not reached before the deadline stay pending.
    ///
    /// Best-effort: a failed close is logged and its debt dropped, never
    /// failing the request that triggered the eviction. A wedged or dead
    /// server is respawned, and `forget_server` clears its history anyway. A
    /// server that is no longer registered is dropped the same way.
    async fn flush_pending_closes(&self) {
        let flush = async {
            for path in self.document_tracker.pending_close_paths() {
                let Some(claim) = self.document_tracker.try_claim_pending_close(&path) else {
                    continue;
                };
                for server_id in &claim.servers {
                    let Some(client) = lock_std(&self.lsp_clients).get(server_id).cloned() else {
                        continue;
                    };
                    if let Err(err) = client
                        .notify_typed::<lsp_types::DidCloseTextDocumentNotification>(
                            lsp_types::DidCloseTextDocumentParams {
                                text_document: lsp_types::TextDocumentIdentifier {
                                    uri: claim.uri.clone(),
                                },
                            },
                        )
                        .await
                    {
                        tracing::warn!(
                            %server_id,
                            path = %claim.path.display(),
                            error = %err,
                            "failed to notify evicted document's server of textDocument/didClose; \
                             dropping the close"
                        );
                    }
                }
            }
        };
        if tokio::time::timeout(FLUSH_PENDING_CLOSES_DEADLINE, flush)
            .await
            .is_err()
        {
            tracing::warn!(
                deadline = ?FLUSH_PENDING_CLOSES_DEADLINE,
                "flushing evicted documents' didClose notifications hit the deadline; paths not yet \
                 reached stay pending, the in-flight path's remaining closes are dropped"
            );
        }
    }

    /// Verify the routed server advertises support for a capability before
    /// dispatching a capability-gated LSP request.
    ///
    /// Production always registers an [`LspServer`] alongside its
    /// [`LspClient`] in the same `register_servers` step (see `lib.rs`), so in
    /// practice a registered client always has known capabilities. If no
    /// `LspServer` is registered for `server_id` regardless -- a client
    /// registered without its server, which only happens in tests, or a
    /// narrow window during registration where the two maps are inserted
    /// under separate locks -- the capability is assumed supported rather
    /// than blocking the request: this mirrors the graceful-degradation
    /// stance used elsewhere in `Translator` when capability information is
    /// unavailable rather than known-absent.
    ///
    /// Note: this checks the `ServerCapabilities` snapshot captured at
    /// `initialize` time. A server that advertises a capability later via
    /// `client/registerCapability` (dynamic registration) is not reflected
    /// here and will be incorrectly rejected; mcpls does not currently apply
    /// dynamic registrations back onto the stored capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CapabilityNotSupported`] if the registered server's
    /// `ServerCapabilities` explicitly does not advertise `capability`.
    pub(super) fn require_capability(
        &self,
        server_id: &ServerId,
        capability: Capability,
    ) -> Result<()> {
        let servers = lock_std(&self.lsp_servers);
        check_capability(
            server_id,
            servers
                .get(server_id)
                .map(crate::lsp::LspServer::capabilities),
            capability,
        )
    }

    /// Returns true when the routed server's `codeActionProvider` capability
    /// advertises `resolveProvider: true` (per LSP 3.16, `CodeActionOptions`),
    /// meaning it will actually answer a `codeAction/resolve` follow-up
    /// request rather than merely receiving mcpls's client-side
    /// `resolve_support` advertisement (`lsp/lifecycle.rs`) with no server
    /// implementation behind it (#432).
    ///
    /// Unlike [`Self::require_capability`], an unregistered server (only
    /// possible in tests, see that method's doc comment) is treated as
    /// *not* supporting resolve rather than assumed-supported: resolve is a
    /// best-effort enhancement, so the safe default when capability
    /// information is unavailable is to skip the extra round-trip, not to
    /// risk it against a server that may not implement it.
    pub(super) fn code_action_resolve_supported(&self, server_id: &ServerId) -> bool {
        let servers = lock_std(&self.lsp_servers);
        matches!(
            servers
                .get(server_id)
                .map(crate::lsp::LspServer::capabilities)
                .and_then(|caps| caps.code_action_provider.as_ref()),
            Some(lsp_types::CodeActionProvider::CodeActionOptions(
                lsp_types::CodeActionOptions {
                    resolve_provider: Some(true),
                    ..
                }
            ))
        )
    }

    /// Parse and validate a file URI, returning the validated path.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The URI doesn't have a file:// scheme, carries an authority, or
    ///   otherwise cannot be converted to a path (see
    ///   [`crate::bridge::state::uri_to_path`])
    /// - The path is outside workspace boundaries
    pub(super) fn parse_file_uri(&self, uri: &lsp_types::Uri) -> Result<PathBuf> {
        let path = crate::bridge::state::uri_to_path(uri).ok_or_else(|| {
            Error::InvalidToolParams(format!(
                "Invalid URI, expected an absolute file:// URI but got: {}",
                uri.as_ref()
            ))
        })?;

        // Validate path is within workspace
        self.validate_path(&path)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::{assert_matches, fs};

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::sync::Mutex;
    use tokio::time::{Duration, timeout};
    use url::Url;

    use super::*;
    use crate::bridge::NotificationCache;
    use crate::bridge::translator::assist::MAX_TRIGGER_CHARACTER_BYTES;
    use crate::bridge::translator::dto::Position;
    use crate::bridge::translator::edits::MAX_NEW_NAME_LENGTH;
    use crate::bridge::translator::testing::*;
    use crate::config::{LspServerConfig, ToolRouter};
    use crate::error::Error;
    use crate::lsp::LspServer;

    type JsonValue = serde_json::Value;

    #[test]
    fn test_client_for_file_server_initializing_when_expected() {
        // A configured/applicable language whose LSP client has not registered
        // yet (large solution still loading via OmniSharp) must surface
        // ServerInitializing — "wait and retry" — not NoServerForLanguage.
        let path = PathBuf::from("/ws/Assets/Scripts/Player.cs");
        let lang = detect_language(&path, &HashMap::new());
        let id = ServerId::from(lang.clone());

        let translator = Translator::new().with_router(ToolRouter::catch_all([(id.clone(), lang)]));
        let mut expected = HashSet::new();
        expected.insert(id.clone());
        translator.set_expected_servers(expected);

        let err = translator
            .client_for_file(&path, ToolKind::Hover)
            .unwrap_err();
        assert_matches!(err, Error::ServerInitializing { server_id } if server_id == id);
    }

    #[test]
    fn test_client_for_file_no_server_when_not_expected() {
        // When no route is configured for the language at all, the error
        // stays NoServerForLanguage.
        let translator = Translator::new();
        let path = PathBuf::from("/ws/Assets/Scripts/Player.cs");
        let lang = detect_language(&path, &translator.extension_map);

        let err = translator
            .client_for_file(&path, ToolKind::Hover)
            .unwrap_err();
        assert_matches!(err, Error::NoServerForLanguage(ref l) if *l == lang);
    }

    use crate::test_lsp::test_extensions;

    fn not_found_failure(id: &ServerId, language: &str, command: &str) -> ServerSpawnFailure {
        ServerSpawnFailure {
            server_id: id.clone(),
            language_id: language.to_string(),
            command: command.to_string(),
            reason: crate::error::StartupFailure::Spawn(Arc::new(Error::ServerNotFound {
                command: command.to_string(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            })),
        }
    }

    fn router_config(
        language: &str,
        name: &str,
        handles: Option<Vec<ToolKind>>,
    ) -> LspServerConfig {
        LspServerConfig {
            language_id: language.to_string(),
            command: "sh".to_string(),
            args: vec![],
            env: HashMap::new(),
            file_patterns: vec![],
            initialization_options: None,
            timeout_seconds: 5,
            request_timeout_seconds: 5,
            heuristics: None,
            name: Some(name.to_string()),
            handles,
            indexing: crate::bridge::IndexingPolicy::Auto,
        }
    }

    /// #527: the sole server for a language failed to spawn, so the routing
    /// error carries the spawn failure and its install guidance instead of
    /// "no LSP server configured".
    #[test]
    fn test_client_for_file_reports_startup_failure_of_sole_server() {
        let id = ServerId::from("rust");
        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]));
        translator.record_startup_failures(&[not_found_failure(&id, "rust", "rust-analyzer")]);
        translator.rebind_router(&HashSet::new());
        translator.clear_expected_servers();

        let err = translator
            .client_for_file(Path::new("/ws/main.rs"), ToolKind::Hover)
            .unwrap_err();

        let Error::ServerFailedToStart(failure) = &err else {
            panic!("expected ServerFailedToStart, got {err:?}");
        };
        assert_eq!(failure.server_id, id);
        let message = err.to_string();
        assert!(
            message.contains("rustup component add rust-analyzer"),
            "{message}"
        );
    }

    #[test]
    fn test_client_for_file_ignores_startup_failure_of_other_language() {
        let id = ServerId::from("rust");
        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]));
        translator.record_startup_failures(&[not_found_failure(&id, "rust", "rust-analyzer")]);
        translator.rebind_router(&HashSet::new());
        translator.clear_expected_servers();

        let err = translator
            .client_for_file(Path::new("/ws/script.py"), ToolKind::Hover)
            .unwrap_err();

        assert_matches!(err, Error::NoServerForLanguage(_), "got {err:?}");
    }

    /// A failed explicit server must not mask a live catch-all it was
    /// rebound to.
    #[tokio::test]
    async fn test_client_for_file_live_catch_all_wins_over_failed_explicit_server() {
        let configs = [
            router_config("rust", "hover-only", Some(vec![ToolKind::Hover])),
            router_config("rust", "catch-all", None),
        ];
        let router = ToolRouter::from_configs(configs.iter()).unwrap();
        let hover_id = ServerId::from("hover-only");
        let live_id = ServerId::from("catch-all");

        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(router);
        let (client, _server) = fake_lsp_client();
        translator.register_client(live_id.clone(), client);
        translator.record_startup_failures(&[not_found_failure(&hover_id, "rust", "sh")]);
        translator.rebind_router(&HashSet::from([live_id.clone()]));
        translator.clear_expected_servers();

        let (id, _client) = translator
            .client_for_file(Path::new("/ws/main.rs"), ToolKind::Hover)
            .unwrap();

        assert_eq!(id, live_id);
    }

    /// A failed catch-all is reported for a tool the live explicit server
    /// does not claim.
    #[tokio::test]
    async fn test_client_for_file_reports_failed_catch_all_for_unclaimed_tool() {
        let configs = [
            router_config("rust", "hover-only", Some(vec![ToolKind::Hover])),
            router_config("rust", "catch-all", None),
        ];
        let router = ToolRouter::from_configs(configs.iter()).unwrap();
        let hover_id = ServerId::from("hover-only");
        let failed_id = ServerId::from("catch-all");

        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(router);
        let (client, _server) = fake_lsp_client();
        translator.register_client(hover_id.clone(), client);
        translator.record_startup_failures(&[not_found_failure(&failed_id, "rust", "sh")]);
        translator.rebind_router(&HashSet::from([hover_id]));
        translator.clear_expected_servers();

        let err = translator
            .client_for_file(Path::new("/ws/main.rs"), ToolKind::Definition)
            .unwrap_err();

        assert_matches!(&err, Error::ServerFailedToStart(f) if f.server_id == failed_id,
            "got {err:?}"
        );
    }

    /// `.tsx` resolves through its `typescript` base language, so a failure
    /// of the `typescript` server is reported for it too.
    #[test]
    fn test_client_for_file_reports_startup_failure_through_react_base_language() {
        let id = ServerId::from("typescript");
        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(ToolRouter::catch_all([(
                id.clone(),
                "typescript".to_string(),
            )]));
        translator.record_startup_failures(&[not_found_failure(
            &id,
            "typescript",
            "typescript-language-server",
        )]);
        translator.rebind_router(&HashSet::new());
        translator.clear_expected_servers();

        let err = translator
            .client_for_file(Path::new("/ws/app.tsx"), ToolKind::Hover)
            .unwrap_err();

        assert_matches!(&err, Error::ServerFailedToStart(f) if f.server_id == id,
            "got {err:?}"
        );
    }

    #[test]
    fn test_validate_path_no_workspace_roots_rejects_any_path() {
        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // With no workspace roots configured, access is rejected (fail closed)
        let result = translator.validate_path(&test_file);
        assert_matches!(result, Err(Error::NoWorkspaceRoots(_)));
    }

    #[test]
    fn test_validate_path_within_workspace() {
        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().to_path_buf();
        translator.set_workspace_roots(WorkspaceRoots::resolve(vec![workspace_root]));

        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator.validate_path(&test_file);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_outside_workspace() {
        let mut translator = Translator::new();
        let temp_dir1 = TempDir::new().unwrap();
        let temp_dir2 = TempDir::new().unwrap();

        // Set workspace root to temp_dir1
        translator.set_workspace_roots(WorkspaceRoots::resolve(vec![
            temp_dir1.path().to_path_buf(),
        ]));

        // Create file in temp_dir2 (outside workspace)
        let test_file = temp_dir2.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator.validate_path(&test_file);
        assert_matches!(result, Err(Error::PathOutsideWorkspace(_)));
    }

    /// #533: an outside path that does not exist must be rejected as outside
    /// the workspace, not leak its absence as `FileIo`.
    #[test]
    fn test_validate_path_nonexistent_outside_path_is_outside_workspace() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let roots = WorkspaceRoots::resolve(vec![root.path().to_path_buf()]);

        let result = validate_path_against_roots(&outside.path().join("missing.rs"), &roots);

        assert_matches!(result, Err(Error::PathOutsideWorkspace(_)));
    }

    #[test]
    fn test_validate_path_dotdot_escape_is_outside_workspace() {
        let root = TempDir::new().unwrap();
        let roots = WorkspaceRoots::resolve(vec![root.path().to_path_buf()]);

        let result = validate_path_against_roots(&root.path().join("../escape.rs"), &roots);

        assert_matches!(result, Err(Error::PathOutsideWorkspace(_)));
    }

    #[test]
    fn test_validate_path_dot_segments_inside_root_are_accepted() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join("a")).unwrap();
        fs::write(root.path().join("b.rs"), "").unwrap();
        let roots = WorkspaceRoots::resolve(vec![root.path().to_path_buf()]);

        let result = validate_path_against_roots(&root.path().join("./a/../b.rs"), &roots);

        assert_eq!(
            result.unwrap(),
            dunce::canonicalize(root.path().join("b.rs")).unwrap()
        );
    }

    /// A client naming the workspace through a symlinked alias of the
    /// canonical root is admitted when the alias was precomputed.
    #[cfg(unix)]
    #[test]
    fn test_validate_path_symlink_alias_of_root_is_accepted() {
        let AliasFixture {
            dir: _dir,
            real,
            alias,
            roots,
        } = alias_fixture();
        fs::write(real.join("a.rs"), "").unwrap();

        let result = validate_path_against_roots(&alias.join("a.rs"), &roots);

        assert_eq!(
            result.unwrap(),
            dunce::canonicalize(real.join("a.rs")).unwrap()
        );
    }

    /// `..` right after an alias root leaves both the alias and the canonical
    /// root, so the pre-check rejects it.
    #[cfg(unix)]
    #[test]
    fn test_validate_path_dotdot_after_alias_root_is_outside_workspace() {
        let AliasFixture {
            dir, alias, roots, ..
        } = alias_fixture();
        fs::write(dir.path().join("outside.rs"), "").unwrap();

        let result = validate_path_against_roots(&alias.join("../outside.rs"), &roots);

        assert_matches!(result, Err(Error::PathOutsideWorkspace(_)));
    }

    /// Inside an alias root the pre-check admits the path, so a missing file
    /// is a plain I/O error.
    #[cfg(unix)]
    #[test]
    fn test_validate_path_missing_file_under_alias_root_is_file_io() {
        let AliasFixture {
            dir: _dir,
            alias,
            roots,
            ..
        } = alias_fixture();

        let result = validate_path_against_roots(&alias.join("missing.rs"), &roots);

        assert_matches!(result, Err(Error::FileIo { .. }), "{result:?}");
    }

    /// A symlink inside an alias root that escapes the workspace is still
    /// rejected by the canonical check.
    #[cfg(unix)]
    #[test]
    fn test_validate_path_escaping_symlink_under_alias_root_is_outside_workspace() {
        let AliasFixture {
            dir: _dir,
            real,
            alias,
            roots,
        } = alias_fixture();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.rs"), "").unwrap();
        std::os::unix::fs::symlink(outside.path(), real.join("link")).unwrap();

        let result = validate_path_against_roots(&alias.join("link/secret.rs"), &roots);

        assert_matches!(result, Err(Error::PathOutsideWorkspace(_)));
    }

    /// A symlink inside the root pointing outside passes the lexical
    /// pre-check but must still fail the authoritative canonical check.
    #[cfg(unix)]
    #[test]
    fn test_validate_path_symlink_escaping_root_is_outside_workspace() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.rs"), "").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let roots = WorkspaceRoots::resolve(vec![root.path().to_path_buf()]);

        let result = validate_path_against_roots(&root.path().join("link/secret.rs"), &roots);

        assert_matches!(result, Err(Error::PathOutsideWorkspace(_)));
    }

    /// Regression guard: `prepare_gated_document`'s `&str` overload (used by
    /// `handle_hover` and nearly every other gated handler) must still
    /// reject an out-of-workspace path end-to-end, i.e.
    /// `resolve_validated_client_for_file` must validate independently
    /// rather than ever delegating to the `&Path` overload (whose
    /// `resolve_validated_client_for_path` sibling trusts its caller to have
    /// already validated and does not check workspace roots itself). Pins
    /// down a near-miss caught during the #423/#425 refactor, where
    /// `prepare_gated_document` briefly delegated through the `&Path`
    /// overload and would have silently skipped this check for every
    /// `&str`-based handler.
    #[tokio::test]
    async fn test_handle_hover_blocked_when_path_outside_workspace() {
        let workspace_dir = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &workspace_dir,
            &server_id,
            lsp_types::ServerCapabilities {
                hover_provider: Some(lsp_types::HoverProvider::Bool(true)),
                ..Default::default()
            },
        );

        let outside_path = outside_dir.path().join("outside.rs");
        fs::write(&outside_path, "fn outside() {}").unwrap();

        let result = translator
            .handle_hover(outside_path.to_string_lossy().to_string(), pos(1, 1))
            .await;

        assert_matches!(result, Err(Error::PathOutsideWorkspace(_)));
    }

    #[tokio::test]
    async fn test_parse_file_uri_invalid_scheme() {
        let translator = Translator::new();
        let uri: lsp_types::Uri = lsp_types::Uri::from("http://example.com/file.rs");
        let result = translator.parse_file_uri(&uri);
        assert_matches!(result, Err(Error::InvalidToolParams(_)));
    }

    #[tokio::test]
    async fn test_parse_file_uri_valid_scheme() {
        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        translator
            .set_workspace_roots(WorkspaceRoots::resolve(vec![temp_dir.path().to_path_buf()]));
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // Use url crate for cross-platform file URI creation
        let file_url = Url::from_file_path(&test_file).unwrap();
        let uri: lsp_types::Uri = lsp_types::Uri::from(file_url.as_str());
        let result = translator.parse_file_uri(&uri);
        assert!(result.is_ok());
    }

    /// #411 regression: a raw-sliced (non-decoded) URI keeps `%20`/`%C3%A9`
    /// literally in the path, so `canonicalize()` fails with `ENOENT` for
    /// any file whose path contains a space or a non-ASCII character, even
    /// though the file exists. `parse_file_uri` must percent-decode first.
    #[tokio::test]
    async fn test_parse_file_uri_percent_decodes_space_and_non_ascii() {
        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        translator
            .set_workspace_roots(WorkspaceRoots::resolve(vec![temp_dir.path().to_path_buf()]));
        let test_file = temp_dir.path().join("my file café.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let file_url = Url::from_file_path(&test_file).unwrap();
        assert!(
            file_url.as_str().contains("%20"),
            "test fixture must exercise percent-encoding"
        );
        let uri: lsp_types::Uri = lsp_types::Uri::from(file_url.as_str());
        let result = translator.parse_file_uri(&uri).unwrap();
        assert_eq!(result, dunce::canonicalize(&test_file).unwrap());
    }

    /// #411: an authority-bearing `file://` URI (e.g. `file://host/path`)
    /// must be rejected, not silently resolved to a path relative to the
    /// process's cwd -- see `uri_to_path`'s authority check.
    #[tokio::test]
    async fn test_parse_file_uri_rejects_authority() {
        let translator = Translator::new();
        let uri: lsp_types::Uri = lsp_types::Uri::from("file://host/some/path.rs");
        let result = translator.parse_file_uri(&uri);
        assert_matches!(result, Err(Error::InvalidToolParams(_)));
    }

    #[test]
    fn test_client_for_file_uses_custom_extension() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("script.nu");
        fs::write(&test_file, "echo hello").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("nu".to_string(), "nushell".to_string());

        let translator = Translator::new().with_extensions(extension_map);

        let result = translator.client_for_file(&test_file, ToolKind::Hover);

        assert!(result.is_err());
        if let Err(Error::NoServerForLanguage(lang)) = result {
            assert_eq!(lang, "nushell");
        } else {
            panic!("Expected NoServerForLanguage(nushell) error");
        }
    }

    #[test]
    fn test_client_for_file_falls_back_to_default() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("unknown.xyz");
        fs::write(&test_file, "content").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("rs".to_string(), "rust".to_string());

        let translator = Translator::new().with_extensions(extension_map);

        let result = translator.client_for_file(&test_file, ToolKind::Hover);

        assert!(result.is_err());
        if let Err(Error::NoServerForLanguage(lang)) = result {
            assert_eq!(lang, "plaintext");
        } else {
            panic!("Expected NoServerForLanguage(plaintext) error");
        }
    }

    #[test]
    fn test_client_for_file_routes_tsx_to_typescript_server() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("component.tsx");
        fs::write(&test_file, "export const Component = () => <div />").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("tsx".to_string(), "typescriptreact".to_string());

        let translator = Translator::new()
            .with_extensions(extension_map)
            .with_router(ToolRouter::catch_all([(
                ServerId::from("typescript"),
                "typescript".to_string(),
            )]));
        translator.register_client(
            "typescript".to_string(),
            LspClient::new(crate::config::LspServerConfig::typescript()),
        );

        let (_id, client) = translator
            .client_for_file(&test_file, ToolKind::Hover)
            .unwrap();
        assert_eq!(client.language_id(), "typescript");
    }

    #[test]
    fn test_client_for_file_prefers_exact_react_server() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("component.tsx");
        fs::write(&test_file, "export const Component = () => <div />").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("tsx".to_string(), "typescriptreact".to_string());

        let typescript_react_config = crate::config::LspServerConfig {
            language_id: "typescriptreact".to_string(),
            command: "typescript-language-server".to_string(),
            args: vec!["--stdio".to_string()],
            env: HashMap::new(),
            file_patterns: vec!["**/*.tsx".to_string()],
            initialization_options: None,
            timeout_seconds: 30,
            request_timeout_seconds: 30,
            heuristics: None,
            name: None,
            handles: None,
            indexing: crate::bridge::IndexingPolicy::Auto,
        };

        let translator = Translator::new()
            .with_extensions(extension_map)
            .with_router(ToolRouter::catch_all([
                (ServerId::from("typescript"), "typescript".to_string()),
                (
                    ServerId::from("typescriptreact"),
                    "typescriptreact".to_string(),
                ),
            ]));
        translator.register_client(
            "typescript".to_string(),
            LspClient::new(crate::config::LspServerConfig::typescript()),
        );
        translator.register_client(
            "typescriptreact".to_string(),
            LspClient::new(typescript_react_config),
        );

        let (_id, client) = translator
            .client_for_file(&test_file, ToolKind::Hover)
            .unwrap();
        assert_eq!(client.language_id(), "typescriptreact");
    }

    /// #359: a registered server's route id stays resolvable (it is what the
    /// push-degraded flag is keyed on) across a respawn, which swaps the
    /// client behind the id without unregistering it.
    #[test]
    fn test_diagnostics_route_for_path_live_when_registered() {
        let id = ServerId::from("rust");
        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]));
        translator.register_client(
            id.clone(),
            LspClient::new(crate::config::LspServerConfig::rust_analyzer()),
        );

        let route = translator.diagnostics_route_for_path(Path::new("/ws/main.rs"));

        assert_matches!(&route, DiagnosticsRoute::Live(live) if *live == id,
            "{route:?}"
        );
        assert_eq!(route.server_id(), Some(&id));
        assert_eq!(route.into_read_result().unwrap(), Some(id));
    }

    /// A routed server that is expected but not yet registered reads as
    /// retryable "still starting", not as an empty cache.
    #[test]
    fn test_diagnostics_route_for_path_initializing_while_expected() {
        let id = ServerId::from("rust");
        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]));
        translator.set_expected_servers(HashSet::from([id.clone()]));

        let route = translator.diagnostics_route_for_path(Path::new("/ws/main.rs"));

        assert_matches!(&route, DiagnosticsRoute::Initializing(i) if *i == id,
            "{route:?}"
        );
        assert_eq!(route.server_id(), Some(&id));
        let err = route.into_read_result().unwrap_err();
        assert_matches!(&err, Error::ServerInitializing { server_id } if *server_id == id,
            "{err:?}"
        );
    }

    /// #535: a server that failed to start is reported, not read as empty.
    #[test]
    fn test_diagnostics_route_for_path_failed_to_start() {
        let id = ServerId::from("rust");
        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]));
        translator.record_startup_failures(&[not_found_failure(&id, "rust", "rust-analyzer")]);
        translator.rebind_router(&HashSet::new());
        translator.clear_expected_servers();

        let route = translator.diagnostics_route_for_path(Path::new("/ws/main.rs"));

        assert!(route.is_failed(), "{route:?}");
        assert_eq!(route.server_id(), None);
        let err = route.into_read_result().unwrap_err();
        assert_matches!(&err, Error::ServerFailedToStart(f) if f.server_id == id,
            "{err:?}"
        );
    }

    /// A language with no configured route is unrouted, and the failure of an
    /// unrelated language's server does not leak into it.
    #[test]
    fn test_diagnostics_route_for_path_unrouted_reads_empty() {
        let id = ServerId::from("rust");
        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]));
        translator.record_startup_failures(&[not_found_failure(&id, "rust", "rust-analyzer")]);
        translator.rebind_router(&HashSet::new());
        translator.clear_expected_servers();

        let route = translator.diagnostics_route_for_path(Path::new("/ws/script.py"));

        assert_matches!(route, DiagnosticsRoute::Unrouted, "{route:?}");
        assert_eq!(route.into_read_result().unwrap(), None);
    }

    /// A router entry naming a server that is neither registered nor expected
    /// is logged and treated as unrouted.
    #[test]
    fn test_diagnostics_route_for_path_dangling_is_unrouted() {
        let id = ServerId::from("rust");
        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(ToolRouter::catch_all([(id, "rust".to_string())]));

        let route = translator.diagnostics_route_for_path(Path::new("/ws/main.rs"));

        assert_matches!(route, DiagnosticsRoute::Unrouted, "{route:?}");
    }

    /// A live catch-all the router was rebound to beats a failed explicit
    /// server, as for `client_for_file`.
    #[tokio::test]
    async fn test_diagnostics_route_for_path_live_catch_all_wins_over_failed_explicit_server() {
        let configs = [
            router_config("rust", "diag-only", Some(vec![ToolKind::Diagnostics])),
            router_config("rust", "catch-all", None),
        ];
        let router = ToolRouter::from_configs(configs.iter()).unwrap();
        let failed_id = ServerId::from("diag-only");
        let live_id = ServerId::from("catch-all");
        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(router);
        let (client, _server) = fake_lsp_client();
        translator.register_client(live_id.clone(), client);
        translator.record_startup_failures(&[not_found_failure(&failed_id, "rust", "sh")]);
        translator.rebind_router(&HashSet::from([live_id.clone()]));
        translator.clear_expected_servers();

        let route = translator.diagnostics_route_for_path(Path::new("/ws/main.rs"));

        assert_matches!(&route, DiagnosticsRoute::Live(id) if *id == live_id,
            "{route:?}"
        );
    }

    /// The failure is found through the React base language, like routing.
    #[test]
    fn test_diagnostics_route_for_path_reports_failure_through_react_base_language() {
        let id = ServerId::from("typescript");
        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(ToolRouter::catch_all([(
                id.clone(),
                "typescript".to_string(),
            )]));
        translator.record_startup_failures(&[not_found_failure(
            &id,
            "typescript",
            "typescript-language-server",
        )]);
        translator.rebind_router(&HashSet::new());
        translator.clear_expected_servers();

        let route = translator.diagnostics_route_for_path(Path::new("/ws/app.tsx"));

        assert_matches!(&route, DiagnosticsRoute::FailedToStart(f) if f.server_id == id,
            "{route:?}"
        );
    }

    /// A failed server whose route excludes diagnostics is not the diagnostics
    /// route, so its failure must not surface for diagnostics reads.
    #[test]
    fn test_diagnostics_route_for_path_ignores_failure_of_server_without_diagnostics_route() {
        let configs = [router_config(
            "rust",
            "hover-only",
            Some(vec![ToolKind::Hover]),
        )];
        let router = ToolRouter::from_configs(configs.iter()).unwrap();
        let failed = ServerId::from("hover-only");
        let translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(router);
        translator.record_startup_failures(&[not_found_failure(&failed, "rust", "sh")]);
        translator.rebind_router(&HashSet::new());
        translator.clear_expected_servers();

        let route = translator.diagnostics_route_for_path(Path::new("/ws/main.rs"));

        assert_matches!(route, DiagnosticsRoute::Unrouted, "{route:?}");
    }

    /// The live pull path (`get_diagnostics`) also reports the failure.
    #[tokio::test]
    async fn test_handle_diagnostics_reports_failed_server_start() {
        let dir = TempDir::new().unwrap();
        let file = dunce::canonicalize(dir.path()).unwrap().join("main.rs");
        fs::write(&file, "fn main() {}").unwrap();
        let id = ServerId::from("rust");
        let mut translator = Translator::new()
            .with_extensions(test_extensions())
            .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]));
        translator.set_workspace_roots(WorkspaceRoots::resolve(vec![dir.path().to_path_buf()]));
        translator.record_startup_failures(&[not_found_failure(&id, "rust", "sh")]);
        translator.rebind_router(&HashSet::new());
        translator.clear_expected_servers();
        let cache = Mutex::new(crate::bridge::NotificationCache::new());

        let err = translator
            .handle_diagnostics(file.to_string_lossy().to_string(), &cache)
            .await
            .unwrap_err();

        assert_matches!(&err, Error::ServerFailedToStart(f) if f.server_id == id,
            "got {err:?}"
        );
    }

    /// `dir/real` (the canonical root) with `dir/alias` symlinked to it, and
    /// roots admitting both spellings.
    #[cfg(unix)]
    struct AliasFixture {
        dir: TempDir,
        real: PathBuf,
        alias: PathBuf,
        roots: WorkspaceRoots,
    }

    #[cfg(unix)]
    fn alias_fixture() -> AliasFixture {
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let roots = WorkspaceRoots::new(
            vec![dunce::canonicalize(&real).unwrap()],
            vec![alias.clone()],
        );
        AliasFixture {
            dir,
            real,
            alias,
            roots,
        }
    }

    #[test]
    fn test_client_for_file_routes_jsx_to_javascript_server() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("component.jsx");
        fs::write(&test_file, "export const Component = () => <div />").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("jsx".to_string(), "javascriptreact".to_string());

        let javascript_config = crate::config::LspServerConfig {
            language_id: "javascript".to_string(),
            command: "typescript-language-server".to_string(),
            args: vec!["--stdio".to_string()],
            env: HashMap::new(),
            file_patterns: vec!["**/*.js".to_string(), "**/*.jsx".to_string()],
            initialization_options: None,
            timeout_seconds: 30,
            request_timeout_seconds: 30,
            heuristics: None,
            name: None,
            handles: None,
            indexing: crate::bridge::IndexingPolicy::Auto,
        };
        let translator = Translator::new()
            .with_extensions(extension_map)
            .with_router(ToolRouter::catch_all([(
                ServerId::from("javascript"),
                "javascript".to_string(),
            )]));
        translator.register_client("javascript".to_string(), LspClient::new(javascript_config));

        let (_id, client) = translator
            .client_for_file(&test_file, ToolKind::Hover)
            .unwrap();
        assert_eq!(client.language_id(), "javascript");
    }

    #[tokio::test]
    async fn test_serve_initializes_translator_with_extensions() {
        use crate::bridge::indexing::DEFAULT_INDEXING_READY_TIMEOUT_SECS;
        use crate::bridge::state::{DEFAULT_MAX_DOCUMENTS, DEFAULT_MAX_FILE_SIZE};
        use crate::config::{LanguageExtensionMapping, WorkspaceConfig};

        let language_extensions = vec![
            LanguageExtensionMapping {
                extensions: vec!["nu".to_string()],
                language_id: "nushell".to_string(),
            },
            LanguageExtensionMapping {
                extensions: vec!["rs".to_string()],
                language_id: "rust".to_string(),
            },
        ];

        let config = crate::config::ServerConfig {
            mcp: crate::config::McpConfig::default(),
            workspace: WorkspaceConfig {
                roots: vec![PathBuf::from("/tmp/test-workspace")],
                position_encodings: vec!["utf-8".to_string()],
                language_extensions: language_extensions.clone(),
                heuristics_max_depth: 10,
                max_documents: DEFAULT_MAX_DOCUMENTS,
                max_file_size: DEFAULT_MAX_FILE_SIZE,
                indexing_ready_timeout_seconds: DEFAULT_INDEXING_READY_TIMEOUT_SECS,
            },
            lsp_servers: vec![],
            project_config_ignored: false,
        };

        let extension_map = config.build_effective_extension_map();
        assert_eq!(extension_map.get("nu"), Some(&"nushell".to_string()));
        assert_eq!(extension_map.get("rs"), Some(&"rust".to_string()));

        // serve() starts in protocol-only mode when no LSP servers are configured;
        // it may return a transport error but must not report a startup failure.
        let result = crate::serve(config).await;
        if let Err(ref err) = result {
            assert!(
                !matches!(err, crate::error::Error::AllServersFailedToInit { .. }),
                "serve() must not report init failures for empty lsp_servers config"
            );
        }
    }

    #[tokio::test]
    async fn test_concurrent_handlers_on_different_files_do_not_serialize() {
        // Before the fix, Translator was shared as Arc<Mutex<Translator>>, so
        // handling one LSP request held that lock across the `.await` on the
        // response -- blocking every other tool call, even for a completely
        // different file and language server, until the first request
        // completed or timed out (up to 30s). With interior mutability, a
        // concurrent call for a different file must complete without waiting
        // on an unrelated in-flight request.
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("aa".to_string(), "lang_a".to_string());
        extensions.insert("bb".to_string(), "lang_b".to_string());

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([
                    (ServerId::from("lang_a"), "lang_a".to_string()),
                    (ServerId::from("lang_b"), "lang_b".to_string()),
                ]));
        translator.set_workspace_roots(WorkspaceRoots::resolve(vec![dir.path().to_path_buf()]));

        let (client_a, mut server_a) = fake_lsp_client();
        let (client_b, mut server_b) = fake_lsp_client();
        translator.register_client("lang_a".to_string(), client_a);
        translator.register_client("lang_b".to_string(), client_b);

        let path_a = dir.path().join("file.aa");
        let path_b = dir.path().join("file.bb");
        fs::write(&path_a, "content a").unwrap();
        fs::write(&path_b, "content b").unwrap();

        let translator = Arc::new(translator);

        // `server_a` is never given a response, simulating a slow server. If
        // any translator-held lock still spanned the LSP round trip, this
        // task blocking forever would also block the "fast" call below.
        let slow = {
            let translator = Arc::clone(&translator);
            let path = path_a.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_hover(
                        path,
                        Position {
                            line: 1,
                            character: 1,
                        },
                    )
                    .await
            })
        };

        // Wait for the slow task to actually reach its LSP request (i.e. the
        // request bytes were written to the wire) before treating it as
        // "in-flight", so the test doesn't race the spawned task's startup.
        let mut wire_a = BufReader::new(&mut server_a.write_stdout);
        let opened_a = read_framed_message(&mut wire_a).await;
        assert_eq!(opened_a["method"], "textDocument/didOpen");
        let hover_request_a = read_framed_message(&mut wire_a).await;
        assert_eq!(hover_request_a["method"], "textDocument/hover");

        // The fast path: a concurrent call for a different file/server.
        let fast = {
            let translator = Arc::clone(&translator);
            let path = path_b.to_string_lossy().to_string();
            tokio::spawn(async move {
                translator
                    .handle_hover(
                        path,
                        Position {
                            line: 1,
                            character: 1,
                        },
                    )
                    .await
            })
        };

        let mut wire_b = BufReader::new(&mut server_b.write_stdout);
        let opened_b = read_framed_message(&mut wire_b).await;
        assert_eq!(opened_b["method"], "textDocument/didOpen");
        let hover_request_b = read_framed_message(&mut wire_b).await;
        assert_eq!(hover_request_b["method"], "textDocument/hover");
        write_response(
            &mut server_b.read_half_stdin,
            &hover_request_b["id"],
            JsonValue::Null,
        )
        .await;

        let fast_result = timeout(Duration::from_secs(2), fast)
            .await
            .expect("fast call must not be blocked by the slow in-flight request")
            .unwrap();
        assert!(fast_result.is_ok());

        assert!(
            !slow.is_finished(),
            "slow call should still be waiting on its (never-sent) response"
        );
        slow.abort();
    }

    #[tokio::test]
    async fn test_concurrent_ensure_open_same_path_sends_single_did_open() {
        // Regression test: concurrent handler calls for the SAME path must
        // serialize on that path's `ensure_open` lock (see `DocumentTracker::lock_path`)
        // so they can't both observe "not open yet" and both send didOpen.
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("aa".to_string(), "lang_a".to_string());

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([(
                    ServerId::from("lang_a"),
                    "lang_a".to_string(),
                )]));
        translator.set_workspace_roots(WorkspaceRoots::resolve(vec![dir.path().to_path_buf()]));

        let (client, mut server) = fake_lsp_client();
        translator.register_client("lang_a".to_string(), client);

        let path = dir.path().join("file.aa");
        fs::write(&path, "content").unwrap();

        let concurrent_calls = 4;

        let translator = Arc::new(translator);
        let path_str = path.to_string_lossy().to_string();

        let handles: Vec<_> = (0..concurrent_calls)
            .map(|_| {
                let translator = Arc::clone(&translator);
                let path_str = path_str.clone();
                tokio::spawn(async move {
                    translator
                        .handle_hover(
                            path_str,
                            Position {
                                line: 1,
                                character: 1,
                            },
                        )
                        .await
                })
            })
            .collect();

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");

        for _ in 0..concurrent_calls {
            let request = read_framed_message(&mut wire).await;
            assert_eq!(
                request["method"], "textDocument/hover",
                "no second didOpen must appear ahead of the hover requests"
            );
            write_response(&mut server.read_half_stdin, &request["id"], JsonValue::Null).await;
        }

        for handle in handles {
            let result = timeout(Duration::from_secs(2), handle)
                .await
                .expect("handler call should not hang")
                .unwrap();
            assert!(result.is_ok());
        }
    }

    /// Translator routing `.aa` files to one fake server, tracking at most one
    /// document so a second open must evict or be refused.
    fn single_document_translator(dir: &TempDir) -> (Translator, crate::test_lsp::FakeServer) {
        use crate::bridge::state::ResourceLimits;

        let mut extensions = HashMap::new();
        extensions.insert("aa".to_string(), "lang_a".to_string());

        let mut translator = Translator::new()
            .with_extensions(extensions)
            .with_router(ToolRouter::catch_all([(
                ServerId::from("lang_a"),
                "lang_a".to_string(),
            )]))
            .with_resource_limits(ResourceLimits {
                max_documents: 1,
                max_file_size: 0,
            });
        translator.set_workspace_roots(WorkspaceRoots::resolve(vec![dir.path().to_path_buf()]));

        let (client, server) = fake_lsp_client();
        translator.register_client("lang_a".to_string(), client);
        (translator, server)
    }

    /// #515: a close that fails for one server must not stop the other
    /// server's close, and the failed debt is dropped, not re-owed.
    #[tokio::test]
    async fn test_flush_pending_closes_drops_failed_servers_debt() {
        let dir = TempDir::new().unwrap();
        let (translator, mut server_a) = single_document_translator(&dir);
        let (client_d, _server_d) = fake_lsp_client();

        let tracker = &translator.document_tracker;
        let path = dir.path().join("p.aa");
        std::fs::write(&path, "p").unwrap();
        let other = dir.path().join("q.aa");
        std::fs::write(&other, "q").unwrap();
        let client_a = lock_std(&translator.lsp_clients)
            .get(&ServerId::from("lang_a"))
            .cloned()
            .unwrap();
        tracker
            .ensure_open(&path, &ServerId::from("lang_a"), &client_a)
            .await
            .unwrap();
        tracker
            .ensure_open(&path, &ServerId::from("lang_d"), &client_d)
            .await
            .unwrap();
        tracker
            .ensure_open(&other, &ServerId::from("lang_a"), &client_a)
            .await
            .unwrap();

        translator.register_client("lang_d".to_string(), client_d.clone());
        client_d.shutdown().await.unwrap();

        translator.flush_pending_closes().await;

        let mut wire = tokio::io::BufReader::new(&mut server_a.write_stdout);
        for method in [
            "textDocument/didOpen",
            "textDocument/didOpen",
            "textDocument/didClose",
        ] {
            assert_eq!(read_framed_message(&mut wire).await["method"], method);
        }
        assert!(
            tracker.pending_close_paths().is_empty(),
            "a failed close is dropped, not re-owed"
        );
    }

    /// #495: once `DocumentTracker::open`'s LRU eviction reclaims a document
    /// to make room under `max_documents`, `prepare_document` must notify
    /// that document's server with `textDocument/didClose` -- `DocumentTracker`
    /// itself has no `LspClient` access to do this, so it's `Translator`'s
    /// job (`flush_pending_closes`) once `ensure_open` returns.
    #[tokio::test]
    async fn test_prepare_document_sends_didclose_for_evicted_document() {
        let dir = TempDir::new().unwrap();
        let (translator, mut server) = single_document_translator(&dir);

        let path_a = dir.path().join("a.aa");
        fs::write(&path_a, "content a").unwrap();
        let path_b = dir.path().join("b.aa");
        fs::write(&path_b, "content b").unwrap();

        drop(
            translator
                .prepare_document(&path_a.to_string_lossy(), ToolKind::Hover)
                .await
                .unwrap(),
        );

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened_a = read_framed_message(&mut wire).await;
        assert_eq!(opened_a["method"], "textDocument/didOpen");

        drop(
            translator
                .prepare_document(&path_b.to_string_lossy(), ToolKind::Hover)
                .await
                .unwrap(),
        );

        let opened_b = read_framed_message(&mut wire).await;
        assert_eq!(opened_b["method"], "textDocument/didOpen");
        let closed_a = read_framed_message(&mut wire).await;
        assert_eq!(
            closed_a["method"], "textDocument/didClose",
            "evicting `a` to make room for `b` under max_documents: 1 must notify its server"
        );
        assert_eq!(
            closed_a["params"]["textDocument"]["uri"], opened_a["params"]["textDocument"]["uri"],
            "the didClose must name the evicted document, not the newly opened one"
        );
    }

    /// #503: a handler still holding its `PreparedDocument` (i.e. mid LSP
    /// round-trip) keeps that document out of eviction, so a concurrent
    /// `prepare_document` for another path at `max_documents: 1` fails with
    /// `DocumentLimitExceeded` instead of evicting it; once the first
    /// `PreparedDocument` drops, the second path opens and evicts the first.
    #[tokio::test]
    async fn test_prepared_document_blocks_eviction_until_dropped() {
        let dir = TempDir::new().unwrap();
        let (translator, _server) = single_document_translator(&dir);

        let path_a = dir.path().join("a.aa");
        fs::write(&path_a, "content a").unwrap();
        let path_b = dir.path().join("b.aa");
        fs::write(&path_b, "content b").unwrap();

        let doc_a = translator
            .prepare_document(&path_a.to_string_lossy(), ToolKind::Hover)
            .await
            .unwrap();

        let err = translator
            .prepare_document(&path_b.to_string_lossy(), ToolKind::Hover)
            .await
            .unwrap_err();
        assert_matches!(err, Error::DocumentLimitExceeded { .. });

        drop(doc_a);
        drop(
            translator
                .prepare_document(&path_b.to_string_lossy(), ToolKind::Hover)
                .await
                .unwrap(),
        );
    }

    /// #503: two handlers holding the same path concurrently each own a
    /// guard; the path stays protected until the *second* one drops.
    #[tokio::test]
    async fn test_two_prepared_documents_on_same_path_are_refcounted() {
        let dir = TempDir::new().unwrap();
        let (translator, _server) = single_document_translator(&dir);

        let path_a = dir.path().join("a.aa");
        fs::write(&path_a, "content a").unwrap();
        let path_b = dir.path().join("b.aa");
        fs::write(&path_b, "content b").unwrap();
        let canonical_a = dunce::canonicalize(&path_a).unwrap();

        let first = translator
            .prepare_document(&path_a.to_string_lossy(), ToolKind::Hover)
            .await
            .unwrap();
        let second = translator
            .prepare_document(&path_a.to_string_lossy(), ToolKind::Hover)
            .await
            .unwrap();
        assert_eq!(translator.document_tracker.in_flight_count(&canonical_a), 2);

        drop(first);
        let err = translator
            .prepare_document(&path_b.to_string_lossy(), ToolKind::Hover)
            .await
            .unwrap_err();
        assert_matches!(err, Error::DocumentLimitExceeded { .. });

        drop(second);
        assert_eq!(translator.document_tracker.in_flight_count(&canonical_a), 0);
        drop(
            translator
                .prepare_document(&path_b.to_string_lossy(), ToolKind::Hover)
                .await
                .unwrap(),
        );
    }

    /// #503: when `ensure_open` fails, `prepare_document` returns the error
    /// without leaving the path marked in flight.
    #[tokio::test]
    async fn test_prepare_document_releases_in_flight_guard_when_ensure_open_fails() {
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("bb".to_string(), "lang_b".to_string());

        let mut translator =
            Translator::new()
                .with_extensions(extensions)
                .with_router(ToolRouter::catch_all([(
                    ServerId::from("lang_b"),
                    "lang_b".to_string(),
                )]));
        translator.set_workspace_roots(WorkspaceRoots::resolve(vec![dir.path().to_path_buf()]));

        let (client, _server) = fake_lsp_client();
        translator.register_client("lang_b".to_string(), client.clone());

        let path_b = dir.path().join("b.bb");
        fs::write(&path_b, "content b").unwrap();
        let canonical_b = dunce::canonicalize(&path_b).unwrap();

        client.shutdown().await.unwrap();
        let err = translator
            .prepare_document(&path_b.to_string_lossy(), ToolKind::Hover)
            .await
            .unwrap_err();

        assert_matches!(err, Error::ServerTerminated);
        assert_eq!(translator.document_tracker.in_flight_count(&canonical_b), 0);
    }

    /// #495 S5: even when `ensure_open` itself fails for the document being
    /// opened (here: its own `didOpen` notify fails), a `didClose` already
    /// queued for a *different* document evicted earlier in that same call
    /// must still be sent -- `prepare_document` must not lose it by
    /// returning early via `?` before flushing pending closes. Uses two
    /// separate servers (`lang_a` stays healthy, `lang_b`'s connection is
    /// broken) so the evicted document's own `didClose` delivery can be
    /// observed independently of the failure that aborts this call.
    #[tokio::test]
    async fn test_prepare_document_still_sends_didclose_when_ensure_open_itself_fails() {
        use crate::bridge::state::ResourceLimits;

        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("aa".to_string(), "lang_a".to_string());
        extensions.insert("bb".to_string(), "lang_b".to_string());

        let mut translator = Translator::new()
            .with_extensions(extensions)
            .with_router(ToolRouter::catch_all([
                (ServerId::from("lang_a"), "lang_a".to_string()),
                (ServerId::from("lang_b"), "lang_b".to_string()),
            ]))
            .with_resource_limits(ResourceLimits {
                max_documents: 1,
                max_file_size: 0,
            });
        translator.set_workspace_roots(WorkspaceRoots::resolve(vec![dir.path().to_path_buf()]));

        let (client_a, mut server_a) = fake_lsp_client();
        translator.register_client("lang_a".to_string(), client_a);
        let (client_b, _server_b) = fake_lsp_client();
        translator.register_client("lang_b".to_string(), client_b.clone());

        let path_a = dir.path().join("a.aa");
        fs::write(&path_a, "content a").unwrap();
        let path_b = dir.path().join("b.bb");
        fs::write(&path_b, "content b").unwrap();

        drop(
            translator
                .prepare_document(&path_a.to_string_lossy(), ToolKind::Hover)
                .await
                .unwrap(),
        );

        let mut wire_a = BufReader::new(&mut server_a.write_stdout);
        let opened_a = read_framed_message(&mut wire_a).await;
        assert_eq!(opened_a["method"], "textDocument/didOpen");

        // Break only `lang_b`'s connection -- see
        // `test_first_open_self_heals_when_did_open_notify_fails` (state.rs)
        // for why shutting down a clone deterministically fails the next
        // `notify()` on any other clone of the same client.
        client_b.shutdown().await.unwrap();

        let err = translator
            .prepare_document(&path_b.to_string_lossy(), ToolKind::Hover)
            .await
            .unwrap_err();
        assert_matches!(err, Error::ServerTerminated);

        // `a` was evicted (LRU, to make room for `b`) before `b`'s own
        // notify failed, and its didClose must still have gone out on
        // `lang_a`'s still-healthy connection.
        let closed_a = read_framed_message(&mut wire_a).await;
        assert_eq!(closed_a["method"], "textDocument/didClose");
        assert_eq!(
            closed_a["params"]["textDocument"]["uri"],
            opened_a["params"]["textDocument"]["uri"]
        );
    }

    /// #174 §12's own headline dispatch scenario: "pyright/pylsp fixture --
    /// hover -> pyright, diagnostics -> pylsp, rename (unclaimed) ->
    /// `NoServerForTool`", exercised through `Translator`'s public handlers
    /// end to end rather than through `ToolRouter`'s unit tests alone.
    #[tokio::test]
    async fn test_dispatch_routes_hover_and_diagnostics_to_different_servers() {
        let dir = TempDir::new().unwrap();
        let mut extensions = HashMap::new();
        extensions.insert("py".to_string(), "python".to_string());

        let pyright_id = ServerId::from("pyright");
        let pylsp_id = ServerId::from("pylsp");
        let configs = vec![
            LspServerConfig {
                language_id: "python".to_string(),
                command: "pyright-langserver".to_string(),
                args: vec![],
                env: HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                timeout_seconds: 30,
                request_timeout_seconds: 30,
                heuristics: None,
                name: Some("pyright".to_string()),
                handles: Some(vec![ToolKind::Hover]),
                indexing: crate::bridge::IndexingPolicy::Auto,
            },
            LspServerConfig {
                language_id: "python".to_string(),
                command: "pylsp".to_string(),
                args: vec![],
                env: HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                timeout_seconds: 30,
                request_timeout_seconds: 30,
                heuristics: None,
                name: Some("pylsp".to_string()),
                handles: Some(vec![ToolKind::Diagnostics]),
                indexing: crate::bridge::IndexingPolicy::Auto,
            },
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();

        let mut translator = Translator::new()
            .with_extensions(extensions)
            .with_router(router);
        translator.set_workspace_roots(WorkspaceRoots::resolve(vec![dir.path().to_path_buf()]));

        let (client_pyright, mut server_pyright) = fake_lsp_client();
        let (client_pylsp, mut server_pylsp) = fake_lsp_client();
        translator.register_client(pyright_id, client_pyright);
        translator.register_client(pylsp_id, client_pylsp);

        let path = dir.path().join("main.py");
        fs::write(&path, "x = 1").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let translator = Arc::new(translator);

        // rename is claimed by neither server -> NoServerForTool, checked
        // first so it can't be masked by either server's wire state.
        let rename_result = translator
            .handle_rename(path_str.clone(), pos(1, 1), "renamed".to_string())
            .await;
        assert_matches!(
            rename_result,
            Err(Error::NoServerForTool {
                tool: ToolKind::Rename,
                ..
            }),
            "expected NoServerForTool for rename, got {rename_result:?}"
        );

        // hover must route to pyright: didOpen + hover request on its wire.
        let hover = {
            let translator = Arc::clone(&translator);
            let path_str = path_str.clone();
            tokio::spawn(async move { translator.handle_hover(path_str, pos(1, 1)).await })
        };
        let mut wire_pyright = BufReader::new(&mut server_pyright.write_stdout);
        let opened = read_framed_message(&mut wire_pyright).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let hover_request = read_framed_message(&mut wire_pyright).await;
        assert_eq!(hover_request["method"], "textDocument/hover");
        write_response(
            &mut server_pyright.read_half_stdin,
            &hover_request["id"],
            JsonValue::Null,
        )
        .await;
        hover
            .await
            .unwrap()
            .expect("hover routed to pyright must succeed");

        // diagnostics must route to pylsp, independently of pyright: its own
        // didOpen (a second server's first sync of the same path) followed
        // by the diagnostic request on pylsp's wire, never pyright's.
        let diagnostics = {
            let translator = Arc::clone(&translator);
            let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
            tokio::spawn(async move {
                translator
                    .handle_diagnostics(path_str, &notification_cache)
                    .await
            })
        };
        let mut wire_pylsp = BufReader::new(&mut server_pylsp.write_stdout);
        let opened = read_framed_message(&mut wire_pylsp).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let diag_request = read_framed_message(&mut wire_pylsp).await;
        assert_eq!(diag_request["method"], "textDocument/diagnostic");
        // Routing is proven by the request landing on pylsp's wire; abort
        // rather than crafting a well-formed DocumentDiagnosticReportResult.
        diagnostics.abort();
    }

    /// No `LspServer` registered for `server_id` (only a raw `LspClient`, as
    /// most tests in this module do) -- capability is unknown, so the gate
    /// must not block the request.
    #[test]
    fn test_require_capability_ok_when_server_not_registered() {
        let translator = Translator::new();
        let result = translator.require_capability(&ServerId::from("rust"), Capability::Rename);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_require_capability_ok_when_capability_present() {
        let translator = Translator::new();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::RenameProvider::Bool(true)),
            ..Default::default()
        };
        translator.register_server(server_id.clone(), LspServer::new_for_test(caps));

        let result = translator.require_capability(&server_id, Capability::Rename);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_require_capability_err_when_capability_absent() {
        let translator = Translator::new();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities::default();
        translator.register_server(server_id.clone(), LspServer::new_for_test(caps));

        let result = translator.require_capability(&server_id, Capability::Rename);
        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "renameProvider",
                ..
            })
        );
    }

    /// #309: an oversized `new_name` must be rejected before any server
    /// routing is attempted, so no LSP server needs to be registered here.
    #[tokio::test]
    async fn test_handle_rename_rejects_oversized_new_name() {
        let translator = Translator::new();
        let new_name = "a".repeat(MAX_NEW_NAME_LENGTH + 1);

        let result = translator
            .handle_rename(
                "/main.rs".to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
                new_name,
            )
            .await;

        assert_matches!(result, Err(Error::InvalidToolParams(_)));
    }

    #[tokio::test]
    async fn test_handle_rename_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_rename(
                path.to_string_lossy().to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
                "renamed".to_string(),
            )
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "renameProvider",
                ..
            })
        );
    }

    #[tokio::test]
    async fn test_handle_code_actions_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_code_actions(
                path.to_string_lossy().to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
                Position {
                    line: 1,
                    character: 5,
                },
                None,
            )
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "codeActionProvider",
                ..
            })
        );
    }

    #[tokio::test]
    async fn test_handle_signature_help_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_signature_help(
                path.to_string_lossy().to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
            )
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "signatureHelpProvider",
                ..
            })
        );
    }

    /// `handle_incoming_calls` resolves its server via `client_for_file`
    /// directly (not `prepare_document`), a separate code path from the other
    /// gated handlers -- exercise it explicitly.
    #[tokio::test]
    async fn test_handle_incoming_calls_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let uri = Url::from_file_path(&path).unwrap().to_string();

        let item = serde_json::json!({
            "name": "test_function",
            "kind": 12,
            "uri": uri,
            "range": {
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 10}
            },
            "selectionRange": {
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 10}
            }
        });

        let result = translator.handle_incoming_calls(item).await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "callHierarchyProvider",
                ..
            })
        );
    }

    #[tokio::test]
    async fn test_handle_outgoing_calls_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let uri = Url::from_file_path(&path).unwrap().to_string();

        let item = serde_json::json!({
            "name": "test_function",
            "kind": 12,
            "uri": uri,
            "range": {
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 10}
            },
            "selectionRange": {
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 10}
            }
        });

        let result = translator.handle_outgoing_calls(item).await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "callHierarchyProvider",
                ..
            })
        );
    }

    #[tokio::test]
    async fn test_handle_format_document_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_format_document(path.to_string_lossy().to_string(), 4, true)
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "documentFormattingProvider",
                ..
            })
        );
    }

    #[tokio::test]
    async fn test_handle_call_hierarchy_prepare_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_call_hierarchy_prepare(
                path.to_string_lossy().to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
            )
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "callHierarchyProvider",
                ..
            })
        );
    }

    #[tokio::test]
    async fn test_handle_inlay_hints_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_inlay_hints(
                path.to_string_lossy().to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
                Position {
                    line: 10,
                    character: 1,
                },
            )
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "inlayHintProvider",
                ..
            })
        );
    }

    #[tokio::test]
    async fn test_handle_hover_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_hover(
                path.to_string_lossy().to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
            )
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "hoverProvider",
                ..
            })
        );
    }

    #[tokio::test]
    async fn test_handle_definition_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_definition(
                path.to_string_lossy().to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
            )
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "definitionProvider",
                ..
            })
        );
    }

    #[tokio::test]
    async fn test_handle_references_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_references(
                path.to_string_lossy().to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
                false,
            )
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "referencesProvider",
                ..
            })
        );
    }

    /// #309 M3: an oversized `trigger` must be rejected before any server
    /// routing is attempted.
    #[tokio::test]
    async fn test_handle_completions_rejects_oversized_trigger() {
        let translator = Translator::new();
        let trigger = "a".repeat(MAX_TRIGGER_CHARACTER_BYTES + 1);

        let result = translator
            .handle_completions(
                "/main.rs".to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
                Some(trigger),
            )
            .await;

        assert_matches!(result, Err(Error::InvalidToolParams(_)));
    }

    #[tokio::test]
    async fn test_handle_completions_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_completions(
                path.to_string_lossy().to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
                None,
            )
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "completionProvider",
                ..
            })
        );
    }

    #[tokio::test]
    async fn test_handle_document_symbols_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_document_symbols(path.to_string_lossy().to_string())
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "documentSymbolProvider",
                ..
            })
        );
    }

    #[tokio::test]
    async fn test_handle_workspace_symbol_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let result = translator
            .handle_workspace_symbol("main".to_string(), None, 100)
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "workspaceSymbolProvider",
                ..
            })
        );
    }

    #[tokio::test]
    async fn test_handle_implementation_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_implementation(
                path.to_string_lossy().to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
            )
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "implementationProvider",
                ..
            })
        );
    }

    #[tokio::test]
    async fn test_handle_type_definition_blocked_when_capability_not_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let (translator, _server) = translator_with_capabilities(
            &dir,
            &server_id,
            lsp_types::ServerCapabilities::default(),
        );

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = translator
            .handle_type_definition(
                path.to_string_lossy().to_string(),
                Position {
                    line: 1,
                    character: 1,
                },
            )
            .await;

        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "typeDefinitionProvider",
                ..
            })
        );
    }

    /// Explicit `Some(RenameProvider::Bool(false))` -- as distinct from an absent
    /// (`None`) field -- must also be rejected: some servers advertise a
    /// provider field with an explicit `false` rather than omitting it.
    #[tokio::test]
    async fn test_require_capability_err_when_capability_explicitly_false() {
        let translator = Translator::new();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::RenameProvider::Bool(false)),
            ..Default::default()
        };
        translator.register_server(server_id.clone(), LspServer::new_for_test(caps));

        let result = translator.require_capability(&server_id, Capability::Rename);
        assert_matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "renameProvider",
                ..
            })
        );
    }

    /// Positive path: when the routed server *does* advertise the gated
    /// capability, the gate must let the request proceed into dispatch rather
    /// than short-circuiting with `CapabilityNotSupported`. Drives the fake
    /// wire to answer the request so the call completes quickly instead of
    /// idling out its internal 30s request timeout.
    #[tokio::test]
    async fn test_handle_rename_proceeds_when_capability_supported() {
        let dir = TempDir::new().unwrap();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::RenameProvider::Bool(true)),
            ..Default::default()
        };
        let (translator, mut server) = translator_with_capabilities(&dir, &server_id, caps);

        let path = dir.path().join("main.rs");
        fs::write(&path, "fn main() {}").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let translator = Arc::new(translator);
        let handle = {
            let translator = Arc::clone(&translator);
            tokio::spawn(async move {
                translator
                    .handle_rename(
                        path_str,
                        Position {
                            line: 1,
                            character: 1,
                        },
                        "renamed".to_string(),
                    )
                    .await
            })
        };

        let mut wire = BufReader::new(&mut server.write_stdout);
        let opened = read_framed_message(&mut wire).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let rename_request = read_framed_message(&mut wire).await;
        assert_eq!(rename_request["method"], "textDocument/rename");
        write_response(
            &mut server.read_half_stdin,
            &rename_request["id"],
            JsonValue::Null,
        )
        .await;

        let result = timeout(Duration::from_secs(2), handle)
            .await
            .expect("handler call should not hang")
            .unwrap();

        assert!(
            !matches!(result, Err(Error::CapabilityNotSupported { .. })),
            "capability is supported, gate must not block dispatch, got {result:?}"
        );
        assert!(
            result.is_ok(),
            "fake server answered, expected Ok: {result:?}"
        );
    }
}
