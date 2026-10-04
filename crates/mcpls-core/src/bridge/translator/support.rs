//! Advisory per-tool support snapshot behind the `get_tool_support` MCP tool.
//!
//! The snapshot answers "would a call to this tool be dispatched to a server
//! that advertises its capability?" through the same pure decision functions
//! enforcement uses ([`lookup_route`], [`lookup_workspace_route`]) and the
//! same per-capability predicate ([`Capability::is_supported`], folded into a
//! [`CapabilitySet`] per server), so the report and the per-call
//! `require_capability` gate cannot drift apart. `require_capability` stays
//! the authoritative enforcement point.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use lsp_types::ServerCapabilities;
use schemars::JsonSchema;
use serde::Serialize;

use super::Translator;
use super::routing::{
    Capability, LanguageCandidates, RouteLookup, WorkspaceRouteLookup, lookup_route,
    lookup_workspace_route,
};
use crate::bridge::lock_std;
use crate::config::{ServerId, ToolKind, ToolRouter};
use crate::error::Result;

/// Whether one route of a tool would be dispatched to a capable server.
///
/// `Supported` means "will be dispatched", not "will succeed": push-only
/// diagnostics, respawn backoff and indexing can still fail the call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RouteSupport {
    /// The routed server is registered and advertises the tool's capability.
    Supported {
        /// The server the call would be dispatched to.
        server: ServerId,
    },
    /// The routed server is registered but does not advertise the capability.
    CapabilityNotAdvertised {
        /// The server the call would be dispatched to.
        server: ServerId,
        /// The capability the server does not advertise.
        capability: Capability,
    },
    /// The routed server has not finished initializing, or its capabilities
    /// are not known yet.
    Initializing,
    /// No server is routed for this tool.
    NoServer,
}

/// The set of [`Capability`] values one server advertises, one bit each.
///
/// Computed once per snapshot through [`Capability::is_supported`], so the
/// snapshot never clones a server's full `ServerCapabilities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CapabilitySet(u16);

impl CapabilitySet {
    /// The capabilities `caps` advertises.
    pub fn of(caps: &ServerCapabilities) -> Self {
        Capability::ALL
            .into_iter()
            .filter(|capability| capability.is_supported(caps))
            .fold(Self::default(), |set, capability| {
                Self(set.0 | 1 << capability as u8)
            })
    }

    /// Whether `capability` is in the set.
    pub const fn contains(self, capability: Capability) -> bool {
        self.0 & (1 << capability as u8) != 0
    }
}

/// A point-in-time copy of the registries needed to answer
/// [`RouteSupport`] queries without holding any `Translator` lock.
///
/// Dynamic capability registrations are invisible to both this snapshot and
/// `require_capability`; see `Translator::require_capability`.
#[derive(Debug)]
pub struct ToolSupportSnapshot {
    router: Arc<ToolRouter>,
    expected: HashSet<ServerId>,
    capabilities: HashMap<ServerId, CapabilitySet>,
    registered: HashSet<ServerId>,
}

impl ToolSupportSnapshot {
    /// Every configured language, sorted.
    pub(crate) fn languages(&self) -> Vec<String> {
        self.router.configured_languages()
    }

    /// Support for a per-document `tool` on files of `language`.
    pub(crate) fn document_support(&self, language: &str, tool: ToolKind) -> RouteSupport {
        let lookup = lookup_route(
            &LanguageCandidates::new(language.to_string()),
            |lang| self.router.resolve(lang, tool).cloned(),
            |id| self.registered.contains(id).then_some(()),
            |id| self.expected.contains(id),
            |_| None,
        );
        match lookup {
            RouteLookup::Registered(server, ()) => self.registered_support(server, tool),
            RouteLookup::Initializing(_) => RouteSupport::Initializing,
            RouteLookup::Failed(_) | RouteLookup::Dangling { .. } | RouteLookup::Unrouted => {
                RouteSupport::NoServer
            }
        }
    }

    /// Support for a workspace-wide `tool` (no document, hence no language).
    pub(crate) fn workspace_support(&self, tool: ToolKind) -> RouteSupport {
        let lookup = lookup_workspace_route(
            || self.router.resolve_any(tool).cloned(),
            |id| self.registered.contains(id),
            |id| self.expected.contains(id),
            || self.expected.is_empty(),
        );
        match lookup {
            WorkspaceRouteLookup::Registered(server) => self.registered_support(server, tool),
            WorkspaceRouteLookup::Initializing(_) | WorkspaceRouteLookup::AllInitializing => {
                RouteSupport::Initializing
            }
            WorkspaceRouteLookup::Dangling(_)
            | WorkspaceRouteLookup::NothingConfigured
            | WorkspaceRouteLookup::NoClaimant => RouteSupport::NoServer,
        }
    }

    fn registered_support(&self, server: ServerId, tool: ToolKind) -> RouteSupport {
        let Some(caps) = self.capabilities.get(&server) else {
            return RouteSupport::Initializing;
        };
        match Capability::for_tool(tool) {
            Some(capability) if !caps.contains(capability) => {
                RouteSupport::CapabilityNotAdvertised { server, capability }
            }
            _ => RouteSupport::Supported { server },
        }
    }
}

/// A boundary in [`Translator::tool_support_snapshot`]'s sequential reads.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotStage {
    Start,
    ExpectedRead,
    ServersRead,
    ClientsRead,
    RouterRead,
}

/// The snapshot's sequential reads, in the one order both the production
/// path and the observed test path use. The optional observer is invoked at
/// each boundary and expands to nothing when absent.
macro_rules! snapshot_reads {
    ($translator:expr $(, $observe:ident)?) => {{
        let translator = $translator;
        $($observe(SnapshotStage::Start);)?
        let expected = lock_std(&translator.expected_servers).clone();
        $($observe(SnapshotStage::ExpectedRead);)?
        let capabilities = lock_std(&translator.lsp_servers)
            .iter()
            .map(|(id, server)| (id.clone(), CapabilitySet::of(server.capabilities())))
            .collect();
        $($observe(SnapshotStage::ServersRead);)?
        let registered = lock_std(&translator.lsp_clients).keys().cloned().collect();
        $($observe(SnapshotStage::ClientsRead);)?
        let router = Arc::clone(&lock_std(&translator.router));
        $($observe(SnapshotStage::RouterRead);)?
        ToolSupportSnapshot {
            router,
            expected,
            capabilities,
            registered,
        }
    }};
}

impl Translator {
    /// Copy the registries `get_tool_support` reads, taking each lock once,
    /// sequentially, and never nested.
    ///
    /// Read order is `expected_servers`, then `lsp_servers`, then
    /// `lsp_clients`, then the router. Registration writes the client, the
    /// server, rebinds the router, then clears `expected_servers`. Reading
    /// `expected_servers` first means a healthy server mid-registration is
    /// seen as registered or still expected, never as neither (which would
    /// be misreported as `no_server`).
    pub(crate) fn tool_support_snapshot(&self) -> ToolSupportSnapshot {
        snapshot_reads!(self)
    }

    /// [`Self::tool_support_snapshot`] with `observe` called at every
    /// boundary between reads, so a test can interleave registration writes.
    #[cfg(test)]
    fn tool_support_snapshot_observed(
        &self,
        mut observe: impl FnMut(SnapshotStage),
    ) -> ToolSupportSnapshot {
        snapshot_reads!(self, observe)
    }

    /// The language of the file at `path`, after workspace-root validation.
    ///
    /// # Errors
    ///
    /// Returns the same path-validation errors as every document tool.
    pub(crate) fn language_for_path(&self, path: &str) -> Result<String> {
        let validated = self.validate_path(Path::new(path))?;
        Ok(self.language_candidates(&validated).language().to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use tempfile::TempDir;

    use super::super::testing::translator_with_capabilities;
    use super::*;

    fn rust_caps(hover: bool) -> ServerCapabilities {
        ServerCapabilities {
            hover_provider: hover.then_some(lsp_types::HoverProvider::Bool(true)),
            ..Default::default()
        }
    }

    fn snapshot(
        registered: &[&str],
        expected: &[&str],
        caps: &[(&str, ServerCapabilities)],
    ) -> ToolSupportSnapshot {
        let ids = |names: &[&str]| names.iter().map(|n| ServerId::from(*n)).collect();
        ToolSupportSnapshot {
            router: Arc::new(ToolRouter::catch_all([(
                ServerId::from("rust"),
                "rust".to_string(),
            )])),
            expected: ids(expected),
            capabilities: caps
                .iter()
                .map(|(id, caps)| (ServerId::from(*id), CapabilitySet::of(caps)))
                .collect(),
            registered: ids(registered),
        }
    }

    #[test]
    fn capability_tool_kind_round_trips() {
        for capability in Capability::ALL {
            assert_eq!(
                Capability::for_tool(capability.tool_kind()),
                Some(capability)
            );
        }
        assert_eq!(Capability::for_tool(ToolKind::Diagnostics), None);
        for tool in ToolKind::ALL {
            let expected = usize::from(*tool != ToolKind::Diagnostics);
            assert_eq!(
                Capability::ALL
                    .iter()
                    .filter(|c| c.tool_kind() == *tool)
                    .count(),
                expected,
                "{tool}"
            );
        }
    }

    #[test]
    fn registered_server_with_capability_is_supported() {
        let snap = snapshot(&["rust"], &[], &[("rust", rust_caps(true))]);
        assert_eq!(
            snap.document_support("rust", ToolKind::Hover),
            RouteSupport::Supported {
                server: ServerId::from("rust")
            }
        );
    }

    #[test]
    fn registered_server_without_capability_is_not_advertised() {
        let snap = snapshot(&["rust"], &[], &[("rust", rust_caps(false))]);
        assert_eq!(
            snap.document_support("rust", ToolKind::Hover),
            RouteSupport::CapabilityNotAdvertised {
                server: ServerId::from("rust"),
                capability: Capability::Hover,
            }
        );
        assert_eq!(
            snap.document_support("rust", ToolKind::Diagnostics),
            RouteSupport::Supported {
                server: ServerId::from("rust")
            }
        );
    }

    #[test]
    fn client_without_server_capabilities_is_initializing() {
        let snap = snapshot(&["rust"], &[], &[]);
        assert_eq!(
            snap.document_support("rust", ToolKind::Hover),
            RouteSupport::Initializing
        );
    }

    #[test]
    fn expected_but_unregistered_server_is_initializing() {
        let snap = snapshot(&[], &["rust"], &[]);
        assert_eq!(
            snap.document_support("rust", ToolKind::Hover),
            RouteSupport::Initializing
        );
        assert_eq!(
            snap.workspace_support(ToolKind::WorkspaceSymbols),
            RouteSupport::Initializing
        );
    }

    #[test]
    fn capability_set_holds_exactly_the_advertised_capabilities() {
        let set = CapabilitySet::of(&rust_caps(true));
        for capability in Capability::ALL {
            assert_eq!(
                set.contains(capability),
                capability == Capability::Hover,
                "{capability:?}"
            );
        }
        assert_eq!(
            CapabilitySet::of(&ServerCapabilities::default()),
            CapabilitySet::default()
        );
    }

    /// Applies the four registration writes in the order production performs
    /// them (`Translator::settle_started`: client then server; then
    /// `rebind_router`; then the expected-set removal, here
    /// `clear_expected_servers`) at every boundary between the snapshot's
    /// reads, over all 70 monotone placements. A healthy server must never
    /// read as `no_server`.
    #[tokio::test]
    async fn healthy_server_never_misreported_for_any_write_read_interleaving() {
        use crate::lsp::LspServer;
        use crate::test_lsp::fake_lsp_client;

        const STAGES: usize = 5;
        let id = ServerId::from("rust");
        let mut schedules = 0;
        for g0 in 0..STAGES {
            for g1 in g0..STAGES {
                for g2 in g1..STAGES {
                    for g3 in g2..STAGES {
                        schedules += 1;
                        let gaps = [g0, g1, g2, g3];
                        let translator = Translator::new()
                            .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]));
                        translator.set_expected_servers(HashSet::from([id.clone()]));
                        let (client, _fake) = fake_lsp_client();
                        let mut client = Some(client);

                        let snap = translator.tool_support_snapshot_observed(|stage| {
                            for (step, gap) in gaps.iter().enumerate() {
                                if *gap != stage as usize {
                                    continue;
                                }
                                match step {
                                    0 => translator
                                        .register_client(id.clone(), client.take().unwrap()),
                                    1 => translator.register_server(
                                        id.clone(),
                                        LspServer::new_for_test(rust_caps(true)),
                                    ),
                                    2 => translator.rebind_router(&HashSet::from([id.clone()])),
                                    _ => translator.clear_expected_servers(),
                                }
                            }
                        });

                        assert_ne!(
                            snap.document_support("rust", ToolKind::Hover),
                            RouteSupport::NoServer,
                            "document route, gaps {gaps:?}"
                        );
                        assert_ne!(
                            snap.workspace_support(ToolKind::WorkspaceSymbols),
                            RouteSupport::NoServer,
                            "workspace route, gaps {gaps:?}"
                        );
                    }
                }
            }
        }
        assert_eq!(schedules, 70);
    }

    #[test]
    fn language_without_live_route_is_listed_as_no_server() {
        let mut router = ToolRouter::catch_all([(ServerId::from("rust"), "rust".to_string())]);
        router.rebind_to_registered(&HashSet::new());
        let snap = ToolSupportSnapshot {
            router: Arc::new(router),
            expected: HashSet::new(),
            capabilities: HashMap::new(),
            registered: HashSet::new(),
        };
        assert_eq!(snap.languages(), ["rust"]);
        assert_eq!(
            snap.document_support("rust", ToolKind::Hover),
            RouteSupport::NoServer
        );
    }

    #[test]
    fn nothing_configured_workspace_support_is_no_server() {
        let snap = ToolSupportSnapshot {
            router: Arc::new(ToolRouter::default()),
            expected: HashSet::new(),
            capabilities: HashMap::new(),
            registered: HashSet::new(),
        };
        assert!(snap.languages().is_empty());
        assert_eq!(
            snap.workspace_support(ToolKind::WorkspaceSymbols),
            RouteSupport::NoServer
        );
    }

    #[tokio::test]
    async fn translator_snapshot_reflects_registered_capabilities() {
        let dir = TempDir::new().unwrap();
        let id = ServerId::from("rust");
        let (translator, _fake) = translator_with_capabilities(&dir, &id, rust_caps(true));
        let snap = translator.tool_support_snapshot();
        assert_eq!(
            snap.document_support("rust", ToolKind::Hover),
            RouteSupport::Supported { server: id.clone() }
        );
        assert_eq!(
            snap.document_support("rust", ToolKind::Rename),
            RouteSupport::CapabilityNotAdvertised {
                server: id,
                capability: Capability::Rename,
            }
        );
    }

    #[tokio::test]
    async fn language_for_path_validates_and_detects() {
        let dir = TempDir::new().unwrap();
        let (translator, _fake) = translator_with_capabilities(
            &dir,
            &ServerId::from("rust"),
            ServerCapabilities::default(),
        );
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "").unwrap();
        assert_eq!(
            translator
                .language_for_path(file.to_str().unwrap())
                .unwrap(),
            "rust"
        );
        assert!(
            translator
                .language_for_path("/definitely/missing.rs")
                .is_err()
        );
    }
}
