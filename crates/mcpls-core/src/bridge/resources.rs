//! MCP resource URI codec and subscription tracking for LSP diagnostics.
//!
//! Resources in mcpls use the `lsp-diagnostics:///` scheme (RFC 3986 compliant,
//! empty authority, percent-encoded path). Each resource corresponds to a single
//! file whose diagnostics are cached from LSP `textDocument/publishDiagnostics`
//! notifications.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::sync::RwLock;
use url::Url;

use super::state::{encode_rfc3986_path_chars, uri_to_path};
use super::{WorkspaceRoots, validate_path_against_roots};

/// URI scheme used for diagnostic resources.
const SCHEME: &str = "lsp-diagnostics";

/// Full scheme + authority prefix (`scheme://`).
///
/// Three-slash form (`lsp-diagnostics:///`) is produced by appending an empty
/// authority and the absolute path: `{PREFIX}{path}`.
const PREFIX: &str = "lsp-diagnostics://";

/// Maximum number of resource URIs a single client session may subscribe to.
///
/// Guards against memory exhaustion from a misbehaving or adversarial client.
pub const MAX_SUBSCRIPTIONS: usize = 1_000;

/// Maximum number of concurrent `subscriptions/listen` streams, across all
/// transports.
///
/// Listen streams are bounded by their connection and take no
/// `max_concurrent_sessions` slot, so they need a cap of their own. A
/// constant, not a knob: it equals the default `HttpConfig` session cap and
/// is independent of the configured one.
pub(crate) const MAX_LISTEN_STREAMS: usize = 100;

/// Errors produced by the resource URI codec.
#[derive(Debug, Error)]
pub enum ResourceUriError {
    /// The path is relative or contains non-UTF-8 components.
    #[error("path must be absolute and valid UTF-8: {0}")]
    InvalidPath(String),

    /// The URI has the wrong scheme or malformed structure.
    #[error("expected '{SCHEME}:///' prefix in URI: {0}")]
    InvalidScheme(String),

    /// The URI path could not be decoded to a filesystem path.
    #[error("failed to decode URI to filesystem path: {0}")]
    DecodeFailed(String),
}

impl From<ResourceUriError> for crate::error::Error {
    fn from(err: ResourceUriError) -> Self {
        Self::InvalidUri(err.to_string())
    }
}

/// Errors produced when adding a URI to a [`ResourceSubscriptions`] set.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum SubscriptionError {
    /// The session's subscription set has already reached [`MAX_SUBSCRIPTIONS`].
    #[error("subscription limit of {MAX_SUBSCRIPTIONS} reached")]
    LimitReached,

    /// [`MAX_LISTEN_STREAMS`] listen streams are already open.
    #[error("listen stream limit of {MAX_LISTEN_STREAMS} reached")]
    ListenLimitReached,
}

impl From<SubscriptionError> for crate::error::Error {
    fn from(err: SubscriptionError) -> Self {
        match err {
            SubscriptionError::LimitReached => Self::SubscriptionLimitReached {
                max: MAX_SUBSCRIPTIONS,
            },
            SubscriptionError::ListenLimitReached => Self::ListenStreamsExhausted {
                max: MAX_LISTEN_STREAMS,
            },
        }
    }
}

/// Encode an absolute filesystem path into a `lsp-diagnostics:///…` resource URI.
///
/// Percent-encoding is delegated to [`url::Url::from_file_path`], which
/// handles spaces, unicode, `%`, `?`, `#`, and platform separators correctly,
/// plus an additional pass for the RFC 3986 §2.2 "other reserved" characters
/// (`[ ] ^ |`) that `url` otherwise leaves unescaped — the same encoding
/// applied to `file://` URIs.
///
/// # Errors
///
/// Returns [`ResourceUriError::InvalidPath`] if the path is relative or
/// cannot be expressed as a valid file URI.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use mcpls_core::bridge::resources::make_uri;
///
/// let uri = make_uri(Path::new("/home/user/main.rs")).unwrap();
/// assert!(uri.starts_with("lsp-diagnostics:///"));
/// ```
pub fn make_uri(path: &Path) -> Result<String, ResourceUriError> {
    let file_url = Url::from_file_path(path)
        .map_err(|()| ResourceUriError::InvalidPath(path.display().to_string()))?;

    // Replace the "file" scheme with our custom scheme while keeping the
    // percent-encoded path and authority (empty) components.
    let encoded = encode_rfc3986_path_chars(&file_url);
    let after_scheme = encoded.strip_prefix(file_url.scheme()).unwrap_or(&encoded);
    let uri = format!("{SCHEME}{after_scheme}");
    Ok(uri)
}

/// Decode a `lsp-diagnostics:///…` resource URI back to an absolute filesystem path.
///
/// # Errors
///
/// Returns an error if the URI does not start with the expected scheme,
/// or if the percent-encoded path cannot be mapped to a filesystem path.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use mcpls_core::bridge::resources::{make_uri, parse_uri};
///
/// let path = Path::new("/home/user/main.rs");
/// let uri = make_uri(path).unwrap();
/// let recovered = parse_uri(&uri).unwrap();
/// assert_eq!(recovered, path);
/// ```
pub fn parse_uri(uri: &str) -> Result<PathBuf, ResourceUriError> {
    if !uri.starts_with(PREFIX) {
        return Err(ResourceUriError::InvalidScheme(uri.to_string()));
    }

    // Require empty authority: the character immediately after `://` must be `/`.
    // This blocks `lsp-diagnostics://evil-host/path` → UNC path on Windows.
    let after_prefix = &uri[PREFIX.len()..];
    if !after_prefix.starts_with('/') {
        return Err(ResourceUriError::InvalidScheme(format!(
            "non-empty authority in URI: {uri}"
        )));
    }

    let file_uri = format!("file://{after_prefix}");
    let url = Url::parse(&file_uri).map_err(|e| ResourceUriError::DecodeFailed(e.to_string()))?;

    url.to_file_path()
        .map_err(|()| ResourceUriError::DecodeFailed(file_uri))
}

/// A `lsp-diagnostics:///` resource URI in the form the diagnostics pump
/// publishes under -- the only key subscription and delivery state uses.
///
/// The field is private, so a value is either the result of
/// [`Self::resolve`] (a client URI parsed, workspace-checked and re-encoded
/// from its canonical path) or of [`Self::for_published`] (derived from a
/// path an LSP server published diagnostics for). Raw client strings
/// therefore cannot reach the subscription set, the pending set or the
/// delivery target without going through one of them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DiagnosticsResourceUri(String);

/// A URI an LSP server published diagnostics for, paired with the canonical
/// URI of the same file.
///
/// A server may publish through a symlinked spelling of a path (rust-analyzer
/// does, for cargo-metadata paths); keying by that raw spelling would never
/// match a subscription or read made through the canonical path. The only
/// constructor is [`Self::from_canonical_path`], fed by the pump's resolver,
/// so a published URI cannot reach the cache index or a subscription key
/// without being canonicalized and workspace-checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublishedDiagnosticsUri {
    source: lsp_types::Uri,
    canonical: lsp_types::Uri,
    /// Whether the server spelled the path canonically, decided on the paths
    /// rather than the URI text so percent-encoding and drive-letter case
    /// cannot make a canonical spelling look like an alias.
    is_canonical: bool,
}

/// How trustworthy the canonical path handed to
/// [`PublishedDiagnosticsUri::from_canonical_path`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CanonicalForm {
    /// Produced by canonicalizing the filesystem (now or earlier).
    Resolved,
    /// The filesystem could not be asked; the published spelling stood in.
    /// Never reported as canonical, so a stand-in cannot pass for the real
    /// key in the alias index.
    Fallback,
}

impl PublishedDiagnosticsUri {
    /// Pairs `published` with its already-resolved `canonical_path`, checking
    /// the result against `canonical_roots`.
    ///
    /// Returns `None` for a canonical path that cannot be encoded as a URI or
    /// that lies outside every root (for example a symlink pointing out of
    /// the workspace). Resolving the path is
    /// [`PublishedPathResolver`](super::PublishedPathResolver)'s job.
    pub(super) fn from_canonical_path(
        published: &lsp_types::Uri,
        published_path: &Path,
        canonical_path: &Path,
        form: CanonicalForm,
        canonical_roots: &[PathBuf],
    ) -> Option<Self> {
        let canonical = super::try_path_to_uri(canonical_path)?;
        super::uri_in_workspace_roots(&canonical, canonical_roots).then(|| Self {
            source: published.clone(),
            canonical,
            is_canonical: form == CanonicalForm::Resolved
                && same_path(published_path, canonical_path),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(source: lsp_types::Uri, canonical: lsp_types::Uri) -> Self {
        let is_canonical = source == canonical;
        Self {
            source,
            canonical,
            is_canonical,
        }
    }

    /// Whether the server published under the canonical spelling of the path.
    pub(crate) const fn is_canonical(&self) -> bool {
        self.is_canonical
    }

    /// The URI exactly as the server published it.
    pub(crate) const fn source(&self) -> &lsp_types::Uri {
        &self.source
    }

    /// The canonical URI of the published file.
    pub(crate) const fn canonical(&self) -> &lsp_types::Uri {
        &self.canonical
    }
}

/// Path equality as the platform's filesystem sees the spelling: drive
/// letters and names are case-insensitive on Windows only.
fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
    } else {
        a == b
    }
}

/// A client resource URI resolved against the workspace roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedResource {
    /// Canonical filesystem path the URI refers to.
    pub(crate) path: PathBuf,
    /// URI re-encoded from [`Self::path`].
    pub(crate) uri: DiagnosticsResourceUri,
}

impl DiagnosticsResourceUri {
    /// Parses `raw`, validates the path against `roots` (canonicalizing it)
    /// and re-encodes the canonical path.
    ///
    /// Touches the filesystem; call from a blocking context when resolving
    /// many URIs.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::InvalidUri`] when `raw` is not a well-formed
    /// `lsp-diagnostics:///` URI, or the workspace validation error when the
    /// path is missing or outside `roots`.
    pub(crate) fn resolve(
        raw: &str,
        roots: &WorkspaceRoots,
    ) -> crate::error::Result<ResolvedResource> {
        let parsed = parse_uri(raw)?;
        // `canonicalize` yields a `\\?\` verbatim path on Windows, which LSP
        // servers never publish; `dunce` strips it where that is safe.
        let validated = validate_path_against_roots(&parsed, roots)?;
        let path = dunce::simplified(&validated).to_path_buf();
        let uri = Self(make_uri(&path)?);
        Ok(ResolvedResource { path, uri })
    }

    /// The key the pump derives for a file an LSP server published
    /// diagnostics for: the canonical form, so it matches the key
    /// [`Self::resolve`] derives from a client URI for the same file.
    pub(crate) fn for_published(published: &PublishedDiagnosticsUri) -> Option<Self> {
        make_uri(&uri_to_path(&published.canonical)?).ok().map(Self)
    }

    /// The wire form of the URI.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn for_test(uri: &str) -> Self {
        Self(uri.to_owned())
    }
}

impl fmt::Display for DiagnosticsResourceUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<DiagnosticsResourceUri> for String {
    fn from(uri: DiagnosticsResourceUri) -> Self {
        uri.0
    }
}

/// Internal state guarded by [`ResourceSubscriptions`]'s `RwLock`.
#[derive(Debug, Default)]
struct SubscriptionState {
    /// Canonical resource URIs currently subscribed -- what the diagnostics
    /// pump checks against.
    canonical: HashSet<DiagnosticsResourceUri>,
    /// Client-supplied ("raw") URI -> canonical URI, recorded at subscribe
    /// time for entries where the two differ (symlink, macOS `/var` vs
    /// `/private/var`, ...). Lets a later `unsubscribe` for the same raw URI
    /// still resolve to the right entry even if canonicalizing it at
    /// unsubscribe time fails, e.g. because the file was deleted since
    /// subscribing (#499).
    aliases: HashMap<String, DiagnosticsResourceUri>,
}

/// Tracks which MCP resource URIs the client has subscribed to.
///
/// The hot read path (pump tasks checking before sending notifications) uses
/// a `RwLock` so concurrent readers do not block each other.
#[derive(Debug)]
pub(crate) struct ResourceSubscriptions(RwLock<SubscriptionState>);

impl Default for ResourceSubscriptions {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceSubscriptions {
    /// Create an empty subscription set.
    #[must_use]
    pub fn new() -> Self {
        Self(RwLock::new(SubscriptionState::default()))
    }

    /// Create a set pre-populated with `uris`, for request-scoped sessions
    /// whose subscriptions are fixed up front (`subscriptions/listen`).
    pub(crate) fn from_canonical(uris: impl IntoIterator<Item = DiagnosticsResourceUri>) -> Self {
        Self(RwLock::new(SubscriptionState {
            canonical: uris.into_iter().collect(),
            aliases: HashMap::new(),
        }))
    }

    /// Add a URI to the subscription set.
    ///
    /// Returns `Ok(true)` if newly inserted, `Ok(false)` if already present.
    ///
    /// # Errors
    ///
    /// Returns [`SubscriptionError::LimitReached`] if the set has already
    /// reached [`MAX_SUBSCRIPTIONS`] and `uri` is not already a member.
    pub(crate) async fn subscribe(
        &self,
        uri: DiagnosticsResourceUri,
    ) -> Result<bool, SubscriptionError> {
        let mut state = self.0.write().await;
        if !state.canonical.contains(&uri) && state.canonical.len() >= MAX_SUBSCRIPTIONS {
            return Err(SubscriptionError::LimitReached);
        }
        Ok(state.canonical.insert(uri))
    }

    /// Record that `raw_uri` (the client-supplied URI, before
    /// canonicalization) currently corresponds to `canonical_uri`, so a
    /// later [`Self::unsubscribe`] for the same raw URI still resolves even
    /// if canonicalization fails by then (#499). A no-op when the two are
    /// equal, or when `canonical_uri` is not (or is no longer) an actual
    /// subscribed entry -- the latter also closes a race against a
    /// concurrent [`Self::unsubscribe`] landing between a caller's own
    /// `subscribe`/`record_alias` pair.
    ///
    /// Drops any existing alias that already points at `canonical_uri`
    /// before inserting the new one, so at most one alias is kept per
    /// canonical entry. This keeps the alias map's size structurally bounded
    /// by the canonical set's size (itself capped at [`MAX_SUBSCRIPTIONS`] by
    /// [`Self::subscribe`]), instead of an independent bound: without this,
    /// re-subscribing under many distinct raw encodings of the same
    /// already-subscribed file (each a no-op against the canonical set, so
    /// never gated by the subscribe cap) could otherwise exhaust an
    /// independent alias-count bound on a single file, starving aliases for
    /// every other subscription.
    ///
    /// Residual (#499): one-alias-per-canonical narrows but does not fully
    /// eliminate the leak this exists to close. A file subscribed under two
    /// distinct raw URIs, then deleted, then unsubscribed via the
    /// non-latest raw form still leaks one slot -- self-healing the moment
    /// the client instead unsubscribes via the latest recorded form.
    pub(crate) async fn record_alias(
        &self,
        raw_uri: String,
        canonical_uri: &DiagnosticsResourceUri,
    ) {
        if raw_uri == canonical_uri.as_str() {
            return;
        }
        let mut state = self.0.write().await;
        if !state.canonical.contains(canonical_uri) {
            return;
        }
        state.aliases.retain(|_, c| c != canonical_uri);
        state.aliases.insert(raw_uri, canonical_uri.clone());
    }

    /// Check whether the subscription set is empty.
    ///
    /// Used as a fast path in the diagnostics pump to skip URI construction
    /// when no client has subscribed yet.
    pub async fn is_empty(&self) -> bool {
        self.0.read().await.canonical.is_empty()
    }

    /// Remove a subscription from the set.
    ///
    /// A recorded alias for `raw` (see `Self::record_alias`, crate-private)
    /// wins, so a symlink retargeted since subscribing still removes the entry
    /// it was subscribed under. Otherwise `canonical` (when the caller could
    /// canonicalize the client URI) is tried, then `raw` verbatim.
    ///
    /// Returns the canonical URI that was removed, or `None` if no
    /// subscription matched -- the caller needs it to purge the same entry
    /// from any state keyed by canonical URI when `raw` was an alias.
    pub(crate) async fn unsubscribe(
        &self,
        canonical: Option<&DiagnosticsResourceUri>,
        raw: &str,
    ) -> Option<DiagnosticsResourceUri> {
        let mut state = self.0.write().await;
        let removed = state
            .aliases
            .remove(raw)
            .and_then(|aliased| state.canonical.take(&aliased))
            .or_else(|| canonical.and_then(|c| state.canonical.take(c)))
            // Lookup key only: `take` returns the stored entry, never this value.
            .or_else(|| {
                state
                    .canonical
                    .take(&DiagnosticsResourceUri(raw.to_owned()))
            });
        if let Some(removed) = &removed {
            state.aliases.retain(|_, c| c != removed);
        }
        drop(state);
        removed
    }

    /// A copy of the canonical subscription set, so a caller can evaluate a
    /// predicate on each URI without holding the subscription lock.
    pub(crate) async fn snapshot(&self) -> Vec<DiagnosticsResourceUri> {
        self.0.read().await.canonical.iter().cloned().collect()
    }

    /// Check if a URI is currently subscribed.
    pub(crate) async fn contains(&self, uri: &DiagnosticsResourceUri) -> bool {
        self.0.read().await.canonical.contains(uri)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::assert_matches;

    use super::*;

    // ------------------------------------------------------------------
    // URI codec
    // ------------------------------------------------------------------

    #[test]
    fn test_make_uri_rejects_relative_path() {
        let result = make_uri(Path::new("relative/path.rs"));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_uri_rejects_wrong_scheme() {
        let result = parse_uri("file:///home/user/main.rs");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_uri_rejects_http_scheme() {
        let result = parse_uri("https://example.com/file.rs");
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_make_uri_simple_path() {
        let uri = make_uri(Path::new("/home/user/main.rs")).unwrap();
        assert_eq!(uri, "lsp-diagnostics:///home/user/main.rs");
    }

    #[cfg(unix)]
    #[test]
    fn test_make_uri_scheme_prefix() {
        let uri = make_uri(Path::new("/tmp/file.rs")).unwrap();
        assert!(uri.starts_with("lsp-diagnostics:///"));
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_uri_simple() {
        let path = PathBuf::from("/home/user/main.rs");
        let uri = make_uri(&path).unwrap();
        let recovered = parse_uri(&uri).unwrap();
        assert_eq!(recovered, path);
    }

    /// Round-trip: paths with spaces, unicode, `%`, `?`, `#`.
    #[cfg(unix)]
    #[test]
    fn test_round_trip_special_chars() {
        let paths = [
            "/home/user/my file.rs",
            "/tmp/café/main.rs",
            "/data/100%/test.rs",
            "/workspace/query?param/file.rs",
            "/repo/branch#fragment/src.rs",
            "/путь/к/файлу.rs",
        ];

        for raw in &paths {
            let path = PathBuf::from(raw);
            let uri = make_uri(&path).expect(raw);
            assert!(
                uri.starts_with("lsp-diagnostics:///"),
                "URI should start with correct scheme: {uri}"
            );
            let recovered = parse_uri(&uri).expect(&uri);
            assert_eq!(recovered, path, "Round-trip failed for: {raw}");
        }
    }

    /// Snapshot test: verify the on-wire form uses three slashes and percent-encoding.
    #[cfg(unix)]
    #[test]
    fn test_wire_format_percent_encoded() {
        let path = Path::new("/home/user/my file.rs");
        let uri = make_uri(path).unwrap();
        // Space must be percent-encoded as %20
        assert!(uri.contains("%20"), "Expected %20 in: {uri}");
        assert!(uri.starts_with("lsp-diagnostics:///"));
    }

    /// #265 regression: all seven RFC 3986 §2.2 "other reserved" characters
    /// must be percent-encoded in `lsp-diagnostics://` URIs, same as
    /// `file://` URIs from `try_path_to_uri` (see
    /// `test_path_to_uri_percent_encodes_all_rfc3986_other_reserved_chars`
    /// in `state.rs`). `{`, `}`, and backtick are already encoded by the
    /// `url` crate on serialization; `[`, `]`, `^`, `|` are handled
    /// explicitly by `encode_rfc3986_path_chars`.
    #[cfg(unix)]
    #[test]
    fn test_make_uri_percent_encodes_reserved_chars() {
        let path = Path::new("/home/user/test[]^|{}`.ts");
        let uri = make_uri(path).unwrap();

        for (raw, encoded) in [
            ('[', "%5B"),
            (']', "%5D"),
            ('^', "%5E"),
            ('|', "%7C"),
            ('{', "%7B"),
            ('}', "%7D"),
            ('`', "%60"),
        ] {
            assert!(
                uri.contains(encoded),
                "expected {raw:?} to be percent-encoded as {encoded} in {uri}"
            );
        }
        assert!(
            !uri.contains(['[', ']', '^', '|', '{', '}', '`']),
            "no raw reserved characters should remain in {uri}"
        );
        assert_eq!(parse_uri(&uri).unwrap(), path);
    }

    // ------------------------------------------------------------------
    // DiagnosticsResourceUri
    // ------------------------------------------------------------------

    use crate::bridge::resolve_one;
    use crate::test_lsp::{diagnostics_uri as u, workspace_with_main_rs as workspace};

    #[test]
    fn test_resolve_rejects_foreign_scheme() {
        let (_dir, root, _file) = workspace();
        let err = DiagnosticsResourceUri::resolve(
            "file:///tmp/main.rs",
            &WorkspaceRoots::resolve(vec![root]),
        )
        .unwrap_err();
        assert_matches!(err, crate::Error::InvalidUri(_), "got {err:?}");
    }

    #[test]
    fn test_resolve_rejects_non_empty_authority() {
        let (_dir, root, _file) = workspace();
        let err = DiagnosticsResourceUri::resolve(
            "lsp-diagnostics://host/main.rs",
            &WorkspaceRoots::resolve(vec![root]),
        )
        .unwrap_err();
        assert_matches!(err, crate::Error::InvalidUri(_), "got {err:?}");
    }

    #[test]
    fn test_resolve_rejects_path_outside_workspace() {
        let (_dir, root, _file) = workspace();
        let (_other_dir, _other_root, other_file) = workspace();
        let raw = make_uri(&other_file).unwrap();
        assert!(
            DiagnosticsResourceUri::resolve(&raw, &WorkspaceRoots::resolve(vec![root])).is_err()
        );
    }

    #[test]
    fn test_resolve_rejects_missing_file() {
        let (_dir, root, _file) = workspace();
        let raw = make_uri(&root.join("missing.rs")).unwrap();
        assert!(
            DiagnosticsResourceUri::resolve(&raw, &WorkspaceRoots::resolve(vec![root])).is_err()
        );
    }

    #[test]
    fn test_resolve_yields_canonical_path_and_uri() {
        let (_dir, root, file) = workspace();
        let raw = make_uri(&file).unwrap();
        let resolved =
            DiagnosticsResourceUri::resolve(&raw, &WorkspaceRoots::resolve(vec![root])).unwrap();
        assert_eq!(resolved.path, file);
        assert_eq!(resolved.uri.as_str(), raw);
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_canonicalizes_symlinked_path() {
        let (_dir, root, file) = workspace();
        let link = root.join("link.rs");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let raw = make_uri(&link).unwrap();
        let resolved =
            DiagnosticsResourceUri::resolve(&raw, &WorkspaceRoots::resolve(vec![root])).unwrap();
        assert_eq!(resolved.uri.as_str(), make_uri(&file).unwrap());
    }

    #[tokio::test]
    async fn test_for_published_matches_resolve_for_same_file() {
        let (_dir, root, file) = workspace();
        let resolved = DiagnosticsResourceUri::resolve(
            &make_uri(&file).unwrap(),
            &WorkspaceRoots::resolve(vec![root.clone()]),
        )
        .unwrap();
        let published = crate::bridge::path_to_uri(&file).unwrap();
        let published = resolve_one(&published, std::slice::from_ref(&root))
            .await
            .unwrap();
        assert_eq!(
            DiagnosticsResourceUri::for_published(&published),
            Some(resolved.uri)
        );
    }

    #[tokio::test]
    async fn test_published_resolve_rejects_non_file_uri() {
        let (_dir, root, _file) = workspace();
        let uri = lsp_types::Uri::from("untitled:Untitled-1");
        assert_eq!(resolve_one(&uri, &[root]).await, None);
    }

    /// Percent-encoding the path must not make a canonical spelling look like
    /// an alias.
    #[tokio::test]
    async fn test_published_resolve_percent_encoded_spelling_is_canonical() {
        let (_dir, root, file) = workspace();
        let canonical = crate::bridge::path_to_uri(&file).unwrap();
        let encoded = lsp_types::Uri::from(canonical.as_ref().replace("main.rs", "main%2Ers"));
        assert_ne!(encoded, canonical);

        let published = resolve_one(&encoded, &[root]).await.unwrap();

        assert!(published.is_canonical());
        assert_eq!(published.canonical(), &canonical);
    }

    #[tokio::test]
    async fn test_published_resolve_deleted_file_keeps_canonical_form() {
        let (_dir, root, file) = workspace();
        std::fs::remove_file(&file).unwrap();
        let uri = crate::bridge::path_to_uri(&file).unwrap();

        let published = resolve_one(&uri, &[root]).await.unwrap();

        assert_eq!(published.canonical(), &uri);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_published_resolve_maps_symlink_to_canonical_key() {
        let (_dir, root, file) = workspace();
        let link = root.join("link.rs");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let link_uri = crate::bridge::path_to_uri(&link).unwrap();

        let published = resolve_one(&link_uri, std::slice::from_ref(&root))
            .await
            .unwrap();

        assert_eq!(published.source(), &link_uri);
        assert_eq!(
            published.canonical(),
            &crate::bridge::path_to_uri(&file).unwrap()
        );
        let client_side = crate::bridge::Translator::cached_diagnostics_uri(
            &WorkspaceRoots::resolve(vec![root]),
            link.to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(published.canonical(), &client_side);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_published_resolve_drops_symlink_pointing_outside() {
        let (_dir, root, _file) = workspace();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("x.rs"), "").unwrap();
        let link = root.join("out.rs");
        std::os::unix::fs::symlink(outside.path().join("x.rs"), &link).unwrap();
        let link_uri = crate::bridge::path_to_uri(&link).unwrap();

        assert_eq!(resolve_one(&link_uri, &[root]).await, None);
    }

    // ------------------------------------------------------------------
    // ResourceSubscriptions
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_subscribe_and_contains() {
        let subs = ResourceSubscriptions::new();
        let uri = u("lsp-diagnostics:///home/user/main.rs");

        assert!(!subs.contains(&uri).await);
        assert!(subs.subscribe(uri.clone()).await.unwrap());
        assert!(subs.contains(&uri).await);
    }

    #[tokio::test]
    async fn test_from_canonical_prepopulates() {
        let uri = u("lsp-diagnostics:///tmp/file.rs");
        let subs = ResourceSubscriptions::from_canonical([uri.clone()]);
        assert!(subs.contains(&uri).await);
        assert!(!subs.is_empty().await);
    }

    #[tokio::test]
    async fn test_subscribe_duplicate_returns_false() {
        let subs = ResourceSubscriptions::new();
        let uri = u("lsp-diagnostics:///tmp/file.rs");
        assert!(subs.subscribe(uri.clone()).await.unwrap());
        assert!(!subs.subscribe(uri).await.unwrap());
    }

    #[tokio::test]
    async fn test_unsubscribe() {
        let subs = ResourceSubscriptions::new();
        let uri = u("lsp-diagnostics:///tmp/file.rs");
        subs.subscribe(uri.clone()).await.unwrap();
        assert_eq!(
            subs.unsubscribe(Some(&uri), uri.as_str()).await,
            Some(uri.clone())
        );
        assert!(!subs.contains(&uri).await);
    }

    #[tokio::test]
    async fn test_unsubscribe_nonexistent_returns_none() {
        let subs = ResourceSubscriptions::new();
        let missing = u("lsp-diagnostics:///nonexistent.rs");
        assert!(
            subs.unsubscribe(Some(&missing), missing.as_str())
                .await
                .is_none()
        );
    }

    /// Without a canonical URI (canonicalization failed) the raw string still
    /// removes an entry whose canonical form equals it.
    #[tokio::test]
    async fn test_unsubscribe_without_canonical_matches_raw_verbatim() {
        let subs = ResourceSubscriptions::new();
        let uri = u("lsp-diagnostics:///tmp/file.rs");
        subs.subscribe(uri.clone()).await.unwrap();
        assert_eq!(subs.unsubscribe(None, uri.as_str()).await, Some(uri));
    }

    /// #499: a stale raw URI recorded via `record_alias` must still resolve
    /// to the canonical entry `subscribe` created, even though the raw and
    /// canonical strings differ (e.g. a symlink or macOS `/var` vs
    /// `/private/var`).
    #[tokio::test]
    async fn test_unsubscribe_resolves_recorded_alias() {
        let subs = ResourceSubscriptions::new();
        let raw = "lsp-diagnostics:///var/tmp/file.rs";
        let canonical = u("lsp-diagnostics:///private/var/tmp/file.rs");

        subs.subscribe(canonical.clone()).await.unwrap();
        subs.record_alias(raw.to_owned(), &canonical).await;
        assert!(subs.contains(&canonical).await);

        assert_eq!(subs.unsubscribe(None, raw).await, Some(canonical.clone()));
        assert!(!subs.contains(&canonical).await);
    }

    /// A re-canonicalization that now yields a different URI (symlink
    /// retargeted since subscribing) falls back to the recorded alias.
    #[tokio::test]
    async fn test_unsubscribe_alias_wins_over_retargeted_canonical() {
        let subs = ResourceSubscriptions::new();
        let raw = "lsp-diagnostics:///link.rs";
        let (a, b) = (u("lsp-diagnostics:///a.rs"), u("lsp-diagnostics:///b.rs"));
        subs.subscribe(a.clone()).await.unwrap();
        subs.subscribe(b.clone()).await.unwrap();
        subs.record_alias(raw.to_owned(), &a).await;

        assert_eq!(subs.unsubscribe(Some(&b), raw).await, Some(a.clone()));
        assert!(!subs.contains(&a).await);
        assert!(subs.contains(&b).await, "the retargeted entry must survive");
    }

    #[tokio::test]
    async fn test_unsubscribe_falls_back_to_alias_when_canonical_changed() {
        let subs = ResourceSubscriptions::new();
        let raw = "lsp-diagnostics:///link.rs";
        let original = u("lsp-diagnostics:///old.rs");
        let retargeted = u("lsp-diagnostics:///new.rs");

        subs.subscribe(original.clone()).await.unwrap();
        subs.record_alias(raw.to_owned(), &original).await;

        assert_eq!(
            subs.unsubscribe(Some(&retargeted), raw).await,
            Some(original.clone())
        );
        assert!(!subs.contains(&original).await);
    }

    /// `record_alias` is a no-op when the raw and canonical URIs are equal
    /// (the common case), so it never grows the alias map for entries that
    /// don't need it.
    #[tokio::test]
    async fn test_record_alias_noop_when_raw_equals_canonical() {
        let subs = ResourceSubscriptions::new();
        let uri = u("lsp-diagnostics:///tmp/file.rs");
        subs.subscribe(uri.clone()).await.unwrap();
        subs.record_alias(uri.as_str().to_owned(), &uri).await;

        assert_eq!(subs.unsubscribe(Some(&uri), uri.as_str()).await, Some(uri));
    }

    /// Unsubscribing under the canonical URI directly must also clear any
    /// aliases that pointed at it, so the alias map does not accumulate
    /// stale entries for already-removed subscriptions.
    #[tokio::test]
    async fn test_unsubscribe_by_canonical_clears_stale_aliases() {
        let subs = ResourceSubscriptions::new();
        let raw = "lsp-diagnostics:///var/tmp/file.rs";
        let canonical = u("lsp-diagnostics:///private/var/tmp/file.rs");

        subs.subscribe(canonical.clone()).await.unwrap();
        subs.record_alias(raw.to_owned(), &canonical).await;

        assert!(subs.unsubscribe(Some(&canonical), "other").await.is_some());
        assert!(subs.unsubscribe(None, raw).await.is_none());
    }

    /// #499 site fix (impl-critic C1): only the most recently recorded alias
    /// for a given canonical URI is kept, so the alias map's size stays
    /// structurally tied to the (already `MAX_SUBSCRIPTIONS`-capped)
    /// canonical set's size.
    #[tokio::test]
    async fn test_record_alias_keeps_only_latest_alias_per_canonical() {
        let subs = ResourceSubscriptions::new();
        let canonical = u("lsp-diagnostics:///file.rs");
        subs.subscribe(canonical.clone()).await.unwrap();

        let raws = [
            "lsp-diagnostics:///%66ile.rs",
            "lsp-diagnostics:///fil%65.rs",
            "lsp-diagnostics:///file%2Ers",
        ];
        for raw in raws {
            subs.record_alias(raw.to_owned(), &canonical).await;
        }

        for raw in &raws[..raws.len() - 1] {
            assert!(subs.unsubscribe(None, raw).await.is_none());
        }
        let latest = raws.last().unwrap();
        assert!(subs.unsubscribe(None, latest).await.is_some());
        assert!(!subs.contains(&canonical).await);
    }

    /// #499 site fix (impl-critic C3): `record_alias` is a no-op when its
    /// canonical URI is not (or is no longer) an actual subscribed entry.
    /// Subscribing `canonical` *after* the no-op call isolates the guard: had
    /// it recorded the alias anyway, `raw` would now resolve to the entry.
    #[tokio::test]
    async fn test_record_alias_noop_for_unsubscribed_canonical() {
        let subs = ResourceSubscriptions::new();
        let raw = "lsp-diagnostics:///var/tmp/file.rs";
        let canonical = u("lsp-diagnostics:///private/var/tmp/file.rs");

        subs.record_alias(raw.to_owned(), &canonical).await;

        subs.subscribe(canonical.clone()).await.unwrap();
        assert!(subs.unsubscribe(None, raw).await.is_none());
        assert!(subs.unsubscribe(Some(&canonical), raw).await.is_some());
    }

    #[tokio::test]
    async fn test_subscribe_cap_exceeded() {
        let subs = ResourceSubscriptions::new();
        for i in 0..MAX_SUBSCRIPTIONS {
            subs.subscribe(u(&format!("lsp-diagnostics:///file{i}.rs")))
                .await
                .unwrap();
        }
        let result = subs.subscribe(u("lsp-diagnostics:///overflow.rs")).await;
        assert_eq!(result, Err(SubscriptionError::LimitReached));
    }
}
