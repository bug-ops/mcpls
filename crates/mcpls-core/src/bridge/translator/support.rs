//! Advisory per-tool support snapshot behind the `get_tool_support` MCP tool.
//!
//! The snapshot answers "would a call to this tool be dispatched to a server
//! that advertises its capability?" through the same pure decision functions
//! enforcement uses ([`lookup_route`], [`lookup_workspace_route`],
//! [`Capability::is_available`]), so the report and the per-call
//! `require_capability` gate cannot drift apart. `require_capability` stays
//! the authoritative enforcement point.

use std::collections::{HashMap, HashSet};
use std::path::Path;

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

/// A point-in-time copy of the registries needed to answer
/// [`RouteSupport`] queries without holding any `Translator` lock.
///
/// Dynamic capability registrations are invisible to both this snapshot and
/// `require_capability`; see `Translator::require_capability`.
#[derive(Debug)]
pub struct ToolSupportSnapshot {
    router: ToolRouter,
    expected: HashSet<ServerId>,
    capabilities: HashMap<ServerId, ServerCapabilities>,
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
        );
        match lookup {
            RouteLookup::Registered(server, ()) => self.registered_support(server, tool),
            RouteLookup::Initializing(_) => RouteSupport::Initializing,
            RouteLookup::Dangling { .. } | RouteLookup::Unrouted => RouteSupport::NoServer,
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
            Some(capability) if !capability.is_available(Some(caps)) => {
                RouteSupport::CapabilityNotAdvertised { server, capability }
            }
            _ => RouteSupport::Supported { server },
        }
    }
}

impl Translator {
    /// Copy the registries `get_tool_support` reads, taking each lock once,
    /// sequentially, and never nested.
    ///
    /// Read order is `expected_servers`, then `lsp_servers`, then
    /// `lsp_clients`, then the router -- the reverse of the order
    /// registration mutates them (insert clients/servers, rebind router,
    /// clear expected). A healthy server mid-registration is therefore seen
    /// as registered or still expected, never as neither (which would be
    /// misreported as `no_server`).
    pub(crate) fn tool_support_snapshot(&self) -> ToolSupportSnapshot {
        let expected = lock_std(&self.expected_servers).clone();
        let capabilities = lock_std(&self.lsp_servers)
            .iter()
            .map(|(id, server)| (id.clone(), server.capabilities().clone()))
            .collect();
        let registered = lock_std(&self.lsp_clients).keys().cloned().collect();
        let router = lock_std(&self.router).clone();
        ToolSupportSnapshot {
            router,
            expected,
            capabilities,
            registered,
        }
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
            router: ToolRouter::catch_all([(ServerId::from("rust"), "rust".to_string())]),
            expected: ids(expected),
            capabilities: caps
                .iter()
                .map(|(id, caps)| (ServerId::from(*id), caps.clone()))
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

    /// Registration order is insert (t1), rebind (t2), clear expected (t3).
    /// A snapshot reading `expected` first and clients after sees the server
    /// in at least one of the two sets for every interleaving; the opposite
    /// read order can see neither, which reads as `no_server`.
    #[test]
    fn expected_read_first_never_misreports_healthy_server() {
        let before_registration = snapshot(&[], &["rust"], &[]);
        let after_registration = snapshot(&["rust"], &[], &[("rust", rust_caps(true))]);
        let expected_read_after_clear_clients_read_before_insert = snapshot(&[], &[], &[]);

        for snap in [&before_registration, &after_registration] {
            assert_ne!(
                snap.document_support("rust", ToolKind::Hover),
                RouteSupport::NoServer
            );
        }
        assert_eq!(
            expected_read_after_clear_clients_read_before_insert
                .document_support("rust", ToolKind::Hover),
            RouteSupport::NoServer
        );
    }

    #[test]
    fn language_without_live_route_is_listed_as_no_server() {
        let mut router = ToolRouter::catch_all([(ServerId::from("rust"), "rust".to_string())]);
        router.rebind_to_registered(&HashSet::new());
        let snap = ToolSupportSnapshot {
            router,
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
            router: ToolRouter::default(),
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
