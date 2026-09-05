//! Client/server routing, document-open preparation, and capability gating
//! shared by every LSP-round-trip tool-call handler.

use std::path::{Path, PathBuf};

use super::Translator;
use crate::bridge::lock_std;
use crate::bridge::state::detect_language;
use crate::config::{ServerId, ToolKind, base_language_id};
use crate::error::{Error, Result};
use crate::lsp::LspClient;

/// Maximum allowed position value for validation.
pub(super) const MAX_POSITION_VALUE: u32 = 1_000_000;

/// Maximum allowed range size in lines.
pub(super) const MAX_RANGE_LINES: u32 = 10_000;

/// Validate that `path` is within one of `workspace_roots`.
///
/// Free function (rather than a `Translator` method) so callers that only need
/// path validation — e.g. cache-only MCP handlers — can validate against a
/// cloned, lock-free snapshot of the workspace roots instead of locking the
/// full `Arc<Mutex<Translator>>`, which may be held elsewhere across a slow
/// in-flight LSP round-trip.
///
/// # Errors
///
/// Returns `Error::PathOutsideWorkspace` if the path is outside all workspace roots.
pub fn validate_path_against_roots(path: &Path, workspace_roots: &[PathBuf]) -> Result<PathBuf> {
    let canonical = path.canonicalize().map_err(|e| Error::FileIo {
        path: path.to_path_buf(),
        source: e,
    })?;

    // If no workspace roots configured, allow any path (backward compatibility)
    if workspace_roots.is_empty() {
        return Ok(canonical);
    }

    // Check if path is within any workspace root
    for root in workspace_roots {
        if let Ok(canonical_root) = root.canonicalize()
            && canonical.starts_with(&canonical_root)
        {
            return Ok(canonical);
        }
    }

    Err(Error::PathOutsideWorkspace(path.to_path_buf()))
}

impl Translator {
    /// Validate that a path is within allowed workspace boundaries.
    ///
    /// # Errors
    ///
    /// Returns `Error::PathOutsideWorkspace` if the path is outside all workspace roots.
    pub(crate) fn validate_path(&self, path: &Path) -> Result<PathBuf> {
        validate_path_against_roots(path, &self.workspace_roots)
    }

    /// Resolve the client and routing identity for `path`/`tool`, giving the
    /// resolved server a chance to be respawned first if its process has
    /// died.
    ///
    /// Thin async wrapper around [`Self::get_client_for_file`] (kept
    /// synchronous so its existing unit tests don't need a runtime): this is
    /// the entry point async handlers call instead, so a dead server is
    /// transparently replaced before its stale client is handed back.
    pub(super) async fn resolve_client_for_file(
        &self,
        path: &Path,
        tool: ToolKind,
    ) -> Result<(ServerId, LspClient)> {
        let (id, client) = self.get_client_for_file(path, tool)?;
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
    pub(super) fn get_client_for_file(
        &self,
        path: &Path,
        tool: ToolKind,
    ) -> Result<(ServerId, LspClient)> {
        let language = detect_language(path, &self.extension_map);
        let mut candidates: Vec<&str> = vec![language.as_str()];
        if let Some(base) = base_language_id(&language) {
            candidates.push(base);
        }

        for lang in &candidates {
            let resolved = lock_std(&self.router).resolve(lang, tool).cloned();
            let Some(id) = resolved else { continue };

            let found = lock_std(&self.lsp_clients).get(&id).cloned();
            if let Some(client) = found {
                return Ok((id, client));
            }
            // A route naming a server that is still initializing (e.g. a
            // large Unity solution loading via OmniSharp) -- tell the caller
            // to wait and retry rather than implying no server is configured.
            if lock_std(&self.expected_servers).contains(&id) {
                return Err(Error::ServerInitializing { server_id: id });
            }
            // Unreachable once registration has rebound the router
            // (`Translator::rebind_router`) -- a route can only name a
            // registered server after that point. Logged rather than
            // `debug_assert!`-panicked: this method is reachable by any
            // library consumer calling `with_router` without registering
            // matching clients, not just internal misuse.
            tracing::error!(
                "router route names server '{id}' for tool '{tool}' that is neither \
                 registered nor expected"
            );
            return Err(Error::NoServerForTool {
                language_id: (*lang).to_string(),
                tool,
            });
        }

        let has_language = {
            let router = lock_std(&self.router);
            candidates.iter().any(|lang| router.has_language(lang))
        };
        if has_language {
            Err(Error::NoServerForTool {
                language_id: language,
                tool,
            })
        } else {
            Err(Error::NoServerForLanguage(language))
        }
    }

    /// Resolve the routing identity of the diagnostics-route server for
    /// `path`'s detected language, without requiring that server to be
    /// currently registered.
    ///
    /// Mirrors [`Self::get_client_for_file`]'s language-candidate order (the
    /// detected language, then its React base language) but only queries the
    /// router: a cache-only caller (`get_cached_diagnostics`) has no LSP
    /// round trip to gate a resolved server's registration on, and only
    /// needs the id to check [`crate::bridge::NotificationCache::is_push_degraded`]
    /// (#359) -- the id itself is stable across a respawn (the routing
    /// identity doesn't change, only the registered client behind it does),
    /// unlike `NotificationCache::diagnostics_owner`, which a respawn clears
    /// along with the crashed server's stale entries.
    #[must_use]
    pub(crate) fn diagnostics_route_id_for_path(&self, path: &Path) -> Option<ServerId> {
        let language = detect_language(path, &self.extension_map);
        let mut candidates: Vec<&str> = vec![language.as_str()];
        if let Some(base) = base_language_id(&language) {
            candidates.push(base);
        }

        let router = lock_std(&self.router);
        candidates
            .iter()
            .find_map(|lang| router.resolve(lang, ToolKind::Diagnostics).cloned())
    }

    /// Validate `file_path`, then resolve its routed client via
    /// [`Self::resolve_client_for_file`] (respawn-aware), without opening
    /// the document.
    ///
    /// Split out from [`Self::prepare_document`] so [`Self::prepare_gated_document`]
    /// can check the routed server's capabilities *before* `ensure_open` sends
    /// `textDocument/didOpen` -- a server rejected by the gate should never
    /// observe an open notification for a request it can't service. Also
    /// used directly by handlers that already have a resolved `PathBuf`
    /// (from `parse_file_uri`) but still need capability gating, e.g.
    /// `handle_incoming_calls`/`handle_outgoing_calls`.
    async fn resolve_validated_client_for_file(
        &self,
        file_path: &str,
        tool: ToolKind,
    ) -> Result<(ServerId, LspClient, PathBuf)> {
        let path = PathBuf::from(file_path);
        let validated_path = self.validate_path(&path)?;
        let (server_id, client) = self.resolve_client_for_file(&validated_path, tool).await?;
        Ok((server_id, client, validated_path))
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
    ) -> Result<(ServerId, LspClient, lsp_types::Uri)> {
        let (server_id, client, validated_path) = self
            .resolve_validated_client_for_file(file_path, tool)
            .await?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &server_id, &client)
            .await?;
        Ok((server_id, client, uri))
    }

    /// Like [`Self::prepare_document`], but checks `capability` against the
    /// routed server's `ServerCapabilities` *before* opening the document --
    /// see [`Self::resolve_client_for_file`]'s doc comment for why the
    /// ordering matters.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CapabilityNotSupported`] if the routed server's
    /// `ServerCapabilities` explicitly does not advertise `capability`.
    pub(super) async fn prepare_gated_document(
        &self,
        file_path: &str,
        tool: ToolKind,
        capability: &'static str,
        supported: impl FnOnce(&lsp_types::ServerCapabilities) -> bool,
    ) -> Result<(ServerId, LspClient, lsp_types::Uri)> {
        let (server_id, client, validated_path) = self
            .resolve_validated_client_for_file(file_path, tool)
            .await?;
        self.require_capability(&server_id, capability, supported)?;
        let uri = self
            .document_tracker
            .ensure_open(&validated_path, &server_id, &client)
            .await?;
        Ok((server_id, client, uri))
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
        capability: &'static str,
        supported: impl FnOnce(&lsp_types::ServerCapabilities) -> bool,
    ) -> Result<()> {
        let servers = lock_std(&self.lsp_servers);
        match servers.get(server_id) {
            Some(server) if !supported(server.capabilities()) => {
                Err(Error::CapabilityNotSupported {
                    server_id: server_id.clone(),
                    capability,
                })
            }
            _ => Ok(()),
        }
    }

    /// Parse and validate a file URI, returning the validated path.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The URI doesn't have a file:// scheme
    /// - The path is outside workspace boundaries
    pub(super) fn parse_file_uri(&self, uri: &lsp_types::Uri) -> Result<PathBuf> {
        let uri_str = uri.as_str();

        // Validate file:// scheme
        if !uri_str.starts_with("file://") {
            return Err(Error::InvalidToolParams(format!(
                "Invalid URI scheme, expected file:// but got: {uri_str}"
            )));
        }

        // Extract path after file://
        let path_str = &uri_str["file://".len()..];

        // Handle Windows paths: file:///C:/path -> /C:/path -> C:/path
        // On Windows, URIs have format file:///C:/path, so we need to strip the leading /
        #[cfg(windows)]
        let path_str = if path_str.len() >= 3
            && path_str.starts_with('/')
            && path_str.chars().nth(2) == Some(':')
        {
            &path_str[1..]
        } else {
            path_str
        };

        let path = PathBuf::from(path_str);

        // Validate path is within workspace
        self.validate_path(&path)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::sync::Arc;

    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::sync::Mutex;
    use tokio::time::{Duration, timeout};
    use url::Url;

    use super::*;
    use crate::bridge::NotificationCache;
    use crate::bridge::translator::assist::MAX_TRIGGER_CHARACTER_BYTES;
    use crate::bridge::translator::edits::MAX_NEW_NAME_LENGTH;
    use crate::bridge::translator::testing::*;
    use crate::config::{LspServerConfig, ToolRouter};
    use crate::error::Error;
    use crate::lsp::LspServer;

    type JsonValue = serde_json::Value;

    #[test]
    fn test_get_client_for_file_server_initializing_when_expected() {
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
            .get_client_for_file(&path, ToolKind::Hover)
            .unwrap_err();
        assert!(matches!(err, Error::ServerInitializing { server_id } if server_id == id));
    }

    #[test]
    fn test_get_client_for_file_no_server_when_not_expected() {
        // When no route is configured for the language at all, the error
        // stays NoServerForLanguage.
        let translator = Translator::new();
        let path = PathBuf::from("/ws/Assets/Scripts/Player.cs");
        let lang = detect_language(&path, &translator.extension_map);

        let err = translator
            .get_client_for_file(&path, ToolKind::Hover)
            .unwrap_err();
        assert!(matches!(err, Error::NoServerForLanguage(ref l) if *l == lang));
    }

    #[test]
    fn test_validate_path_no_workspace_roots() {
        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // With no workspace roots, any valid path should be accepted
        let result = translator.validate_path(&test_file);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_within_workspace() {
        let mut translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().to_path_buf();
        translator.set_workspace_roots(vec![workspace_root]);

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
        translator.set_workspace_roots(vec![temp_dir1.path().to_path_buf()]);

        // Create file in temp_dir2 (outside workspace)
        let test_file = temp_dir2.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let result = translator.validate_path(&test_file);
        assert!(matches!(result, Err(Error::PathOutsideWorkspace(_))));
    }

    #[tokio::test]
    async fn test_parse_file_uri_invalid_scheme() {
        let translator = Translator::new();
        let uri: lsp_types::Uri = "http://example.com/file.rs".parse().unwrap();
        let result = translator.parse_file_uri(&uri);
        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
    }

    #[tokio::test]
    async fn test_parse_file_uri_valid_scheme() {
        let translator = Translator::new();
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // Use url crate for cross-platform file URI creation
        let file_url = Url::from_file_path(&test_file).unwrap();
        let uri: lsp_types::Uri = file_url.as_str().parse().unwrap();
        let result = translator.parse_file_uri(&uri);
        assert!(result.is_ok());
    }

    #[test]
    fn test_get_client_for_file_uses_custom_extension() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("script.nu");
        fs::write(&test_file, "echo hello").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("nu".to_string(), "nushell".to_string());

        let translator = Translator::new().with_extensions(extension_map);

        let result = translator.get_client_for_file(&test_file, ToolKind::Hover);

        assert!(result.is_err());
        if let Err(Error::NoServerForLanguage(lang)) = result {
            assert_eq!(lang, "nushell");
        } else {
            panic!("Expected NoServerForLanguage(nushell) error");
        }
    }

    #[test]
    fn test_get_client_for_file_falls_back_to_default() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("unknown.xyz");
        fs::write(&test_file, "content").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("rs".to_string(), "rust".to_string());

        let translator = Translator::new().with_extensions(extension_map);

        let result = translator.get_client_for_file(&test_file, ToolKind::Hover);

        assert!(result.is_err());
        if let Err(Error::NoServerForLanguage(lang)) = result {
            assert_eq!(lang, "plaintext");
        } else {
            panic!("Expected NoServerForLanguage(plaintext) error");
        }
    }

    #[test]
    fn test_get_client_for_file_routes_tsx_to_typescript_server() {
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
            .get_client_for_file(&test_file, ToolKind::Hover)
            .unwrap();
        assert_eq!(client.language_id(), "typescript");
    }

    #[test]
    fn test_get_client_for_file_prefers_exact_react_server() {
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
            .get_client_for_file(&test_file, ToolKind::Hover)
            .unwrap();
        assert_eq!(client.language_id(), "typescriptreact");
    }

    /// #359: `diagnostics_route_id_for_path` must resolve the same id
    /// `is_diagnostics_route`/the router would, without requiring a
    /// registered client -- `get_cached_diagnostics` relies on this to look
    /// up `NotificationCache::is_push_degraded` even when the file's server
    /// is currently down (mid-respawn or crash-looping).
    #[test]
    fn test_diagnostics_route_id_for_path_resolves_without_registered_client() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("main.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        let mut extension_map = HashMap::new();
        extension_map.insert("rs".to_string(), "rust".to_string());
        let id = ServerId::from("rust");

        let translator = Translator::new()
            .with_extensions(extension_map)
            .with_router(ToolRouter::catch_all([(id.clone(), "rust".to_string())]));
        // Deliberately no `register_client`/`register_server`: this must not
        // require a live registration, unlike `get_client_for_file`.

        assert_eq!(
            translator.diagnostics_route_id_for_path(&test_file),
            Some(id)
        );
    }

    /// A file whose language has no configured route resolves to `None`
    /// rather than panicking or falling back to some default server.
    #[test]
    fn test_diagnostics_route_id_for_path_returns_none_for_unrouted_language() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("unknown.xyz");
        fs::write(&test_file, "content").unwrap();

        let translator = Translator::new();

        assert_eq!(translator.diagnostics_route_id_for_path(&test_file), None);
    }

    #[test]
    fn test_get_client_for_file_routes_jsx_to_javascript_server() {
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
        };
        let translator = Translator::new()
            .with_extensions(extension_map)
            .with_router(ToolRouter::catch_all([(
                ServerId::from("javascript"),
                "javascript".to_string(),
            )]));
        translator.register_client("javascript".to_string(), LspClient::new(javascript_config));

        let (_id, client) = translator
            .get_client_for_file(&test_file, ToolKind::Hover)
            .unwrap();
        assert_eq!(client.language_id(), "javascript");
    }

    #[tokio::test]
    async fn test_serve_initializes_translator_with_extensions() {
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
            },
            lsp_servers: vec![],
            project_config_ignored: false,
        };

        let extension_map = config.build_effective_extension_map();
        assert_eq!(extension_map.get("nu"), Some(&"nushell".to_string()));
        assert_eq!(extension_map.get("rs"), Some(&"rust".to_string()));

        // serve() starts in protocol-only mode when no LSP servers are configured;
        // it may return a transport error but must not return NoServersAvailable.
        let result = crate::serve(config).await;
        if let Err(ref err) = result {
            assert!(
                !matches!(err, crate::error::Error::NoServersAvailable(_)),
                "serve() must not return NoServersAvailable for empty lsp_servers config"
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
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

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
            tokio::spawn(async move { translator.handle_hover(path, 1, 1).await })
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
            tokio::spawn(async move { translator.handle_hover(path, 1, 1).await })
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
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

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
                tokio::spawn(async move { translator.handle_hover(path_str, 1, 1).await })
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
            },
        ];
        let router = ToolRouter::from_configs(&configs).unwrap();

        let mut translator = Translator::new()
            .with_extensions(extensions)
            .with_router(router);
        translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

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
            .handle_rename(path_str.clone(), 1, 1, "renamed".to_string())
            .await;
        assert!(
            matches!(
                rename_result,
                Err(Error::NoServerForTool {
                    tool: ToolKind::Rename,
                    ..
                })
            ),
            "expected NoServerForTool for rename, got {rename_result:?}"
        );

        // hover must route to pyright: didOpen + hover request on its wire.
        let hover = {
            let translator = Arc::clone(&translator);
            let path_str = path_str.clone();
            tokio::spawn(async move { translator.handle_hover(path_str, 1, 1).await })
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
        let result =
            translator.require_capability(&ServerId::from("rust"), "renameProvider", |_| false);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_require_capability_ok_when_capability_present() {
        let translator = Translator::new();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::OneOf::Left(true)),
            ..Default::default()
        };
        translator.register_server(server_id.clone(), LspServer::new_for_test(caps));

        let result = translator.require_capability(&server_id, "renameProvider", |c| {
            matches!(
                c.rename_provider,
                Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
            )
        });
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_require_capability_err_when_capability_absent() {
        let translator = Translator::new();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities::default();
        translator.register_server(server_id.clone(), LspServer::new_for_test(caps));

        let result = translator.require_capability(&server_id, "renameProvider", |c| {
            matches!(
                c.rename_provider,
                Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
            )
        });
        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "renameProvider",
                ..
            })
        ));
    }

    /// #309: an oversized `new_name` must be rejected before any server
    /// routing is attempted, so no LSP server needs to be registered here.
    #[tokio::test]
    async fn test_handle_rename_rejects_oversized_new_name() {
        let translator = Translator::new();
        let new_name = "a".repeat(MAX_NEW_NAME_LENGTH + 1);

        let result = translator
            .handle_rename("/main.rs".to_string(), 1, 1, new_name)
            .await;

        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
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
                1,
                1,
                "renamed".to_string(),
            )
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "renameProvider",
                ..
            })
        ));
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
            .handle_code_actions(path.to_string_lossy().to_string(), 1, 1, 1, 5, None)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "codeActionProvider",
                ..
            })
        ));
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
            .handle_signature_help(path.to_string_lossy().to_string(), 1, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "signatureHelpProvider",
                ..
            })
        ));
    }

    /// `handle_incoming_calls` resolves its server via `get_client_for_file`
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

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "callHierarchyProvider",
                ..
            })
        ));
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

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "callHierarchyProvider",
                ..
            })
        ));
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

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "documentFormattingProvider",
                ..
            })
        ));
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
            .handle_call_hierarchy_prepare(path.to_string_lossy().to_string(), 1, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "callHierarchyProvider",
                ..
            })
        ));
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
            .handle_inlay_hints(path.to_string_lossy().to_string(), 1, 1, 10, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "inlayHintProvider",
                ..
            })
        ));
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
            .handle_hover(path.to_string_lossy().to_string(), 1, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "hoverProvider",
                ..
            })
        ));
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
            .handle_definition(path.to_string_lossy().to_string(), 1, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "definitionProvider",
                ..
            })
        ));
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
            .handle_references(path.to_string_lossy().to_string(), 1, 1, false)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "referencesProvider",
                ..
            })
        ));
    }

    /// #309 M3: an oversized `trigger` must be rejected before any server
    /// routing is attempted.
    #[tokio::test]
    async fn test_handle_completions_rejects_oversized_trigger() {
        let translator = Translator::new();
        let trigger = "a".repeat(MAX_TRIGGER_CHARACTER_BYTES + 1);

        let result = translator
            .handle_completions("/main.rs".to_string(), 1, 1, Some(trigger))
            .await;

        assert!(matches!(result, Err(Error::InvalidToolParams(_))));
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
            .handle_completions(path.to_string_lossy().to_string(), 1, 1, None)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "completionProvider",
                ..
            })
        ));
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

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "documentSymbolProvider",
                ..
            })
        ));
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

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "workspaceSymbolProvider",
                ..
            })
        ));
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
            .handle_implementation(path.to_string_lossy().to_string(), 1, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "implementationProvider",
                ..
            })
        ));
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
            .handle_type_definition(path.to_string_lossy().to_string(), 1, 1)
            .await;

        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "typeDefinitionProvider",
                ..
            })
        ));
    }

    /// Explicit `Some(OneOf::Left(false))` -- as distinct from an absent
    /// (`None`) field -- must also be rejected: some servers advertise a
    /// provider field with an explicit `false` rather than omitting it.
    #[tokio::test]
    async fn test_require_capability_err_when_capability_explicitly_false() {
        let translator = Translator::new();
        let server_id = ServerId::from("rust");
        let caps = lsp_types::ServerCapabilities {
            rename_provider: Some(lsp_types::OneOf::Left(false)),
            ..Default::default()
        };
        translator.register_server(server_id.clone(), LspServer::new_for_test(caps));

        let result = translator.require_capability(&server_id, "renameProvider", |c| {
            matches!(
                c.rename_provider,
                Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
            )
        });
        assert!(matches!(
            result,
            Err(Error::CapabilityNotSupported {
                capability: "renameProvider",
                ..
            })
        ));
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
            rename_provider: Some(lsp_types::OneOf::Left(true)),
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
                    .handle_rename(path_str, 1, 1, "renamed".to_string())
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
