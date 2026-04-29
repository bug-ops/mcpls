//! MCP resource URI codec and subscription tracking for LSP diagnostics.
//!
//! Resources in mcpls use the `lsp-diagnostics:///` scheme (RFC 3986 compliant,
//! empty authority, percent-encoded path). Each resource corresponds to a single
//! file whose diagnostics are cached from LSP `textDocument/publishDiagnostics`
//! notifications.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::sync::RwLock;
use url::Url;

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

/// Encode an absolute filesystem path into a `lsp-diagnostics:///…` resource URI.
///
/// Percent-encoding is delegated to [`url::Url::from_file_path`], which
/// handles spaces, unicode, `%`, `?`, `#`, and platform separators correctly.
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
    // already-percent-encoded path and authority (empty) components.
    let uri = format!("{SCHEME}://{}", &file_url[url::Position::BeforeHost..]);
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

/// Tracks which MCP resource URIs the client has subscribed to.
///
/// The hot read path (pump tasks checking before sending notifications) uses
/// a `RwLock` so concurrent readers do not block each other.
#[derive(Debug)]
pub struct ResourceSubscriptions(RwLock<HashSet<String>>);

impl Default for ResourceSubscriptions {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceSubscriptions {
    /// Create an empty subscription set.
    #[must_use]
    pub fn new() -> Self {
        Self(RwLock::new(HashSet::new()))
    }

    /// Add a URI to the subscription set.
    ///
    /// Returns `Ok(true)` if newly inserted, `Ok(false)` if already present.
    /// Returns `Err` if the subscription set has reached [`MAX_SUBSCRIPTIONS`].
    ///
    /// # Errors
    ///
    /// Returns an error string when the cap is exceeded.
    pub async fn subscribe(&self, uri: String) -> Result<bool, String> {
        let mut set = self.0.write().await;
        if !set.contains(&uri) && set.len() >= MAX_SUBSCRIPTIONS {
            return Err(format!("subscription limit of {MAX_SUBSCRIPTIONS} reached"));
        }
        Ok(set.insert(uri))
    }

    /// Check whether the subscription set is empty.
    ///
    /// Used as a fast path in the diagnostics pump to skip URI construction
    /// when no client has subscribed yet.
    pub async fn is_empty(&self) -> bool {
        self.0.read().await.is_empty()
    }

    /// Remove a URI from the subscription set.
    ///
    /// Returns `true` if the URI was present and removed.
    pub async fn unsubscribe(&self, uri: &str) -> bool {
        self.0.write().await.remove(uri)
    }

    /// Check if a URI is currently subscribed.
    pub async fn contains(&self, uri: &str) -> bool {
        self.0.read().await.contains(uri)
    }

    /// Return a snapshot of all subscribed URIs (primarily for tests).
    pub async fn snapshot(&self) -> Vec<String> {
        self.0.read().await.iter().cloned().collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // URI codec
    // ------------------------------------------------------------------

    #[test]
    fn test_make_uri_simple_path() {
        let uri = make_uri(Path::new("/home/user/main.rs")).unwrap();
        assert_eq!(uri, "lsp-diagnostics:///home/user/main.rs");
    }

    #[test]
    fn test_make_uri_scheme_prefix() {
        let uri = make_uri(Path::new("/tmp/file.rs")).unwrap();
        assert!(uri.starts_with("lsp-diagnostics:///"));
    }

    #[test]
    fn test_make_uri_rejects_relative_path() {
        let result = make_uri(Path::new("relative/path.rs"));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_uri_simple() {
        let path = PathBuf::from("/home/user/main.rs");
        let uri = make_uri(&path).unwrap();
        let recovered = parse_uri(&uri).unwrap();
        assert_eq!(recovered, path);
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

    /// Round-trip: paths with spaces, unicode, `%`, `?`, `#`.
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
    #[test]
    fn test_wire_format_percent_encoded() {
        let path = Path::new("/home/user/my file.rs");
        let uri = make_uri(path).unwrap();
        // Space must be percent-encoded as %20
        assert!(uri.contains("%20"), "Expected %20 in: {uri}");
        assert!(uri.starts_with("lsp-diagnostics:///"));
    }

    // ------------------------------------------------------------------
    // ResourceSubscriptions
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_subscribe_and_contains() {
        let subs = ResourceSubscriptions::new();
        let uri = "lsp-diagnostics:///home/user/main.rs".to_string();

        assert!(!subs.contains(&uri).await);
        assert!(subs.subscribe(uri.clone()).await.unwrap());
        assert!(subs.contains(&uri).await);
    }

    #[tokio::test]
    async fn test_subscribe_duplicate_returns_false() {
        let subs = ResourceSubscriptions::new();
        let uri = "lsp-diagnostics:///tmp/file.rs".to_string();
        assert!(subs.subscribe(uri.clone()).await.unwrap());
        assert!(!subs.subscribe(uri).await.unwrap());
    }

    #[tokio::test]
    async fn test_unsubscribe() {
        let subs = ResourceSubscriptions::new();
        let uri = "lsp-diagnostics:///tmp/file.rs".to_string();
        subs.subscribe(uri.clone()).await.unwrap();
        assert!(subs.unsubscribe(&uri).await);
        assert!(!subs.contains(&uri).await);
    }

    #[tokio::test]
    async fn test_unsubscribe_nonexistent_returns_false() {
        let subs = ResourceSubscriptions::new();
        assert!(!subs.unsubscribe("lsp-diagnostics:///nonexistent.rs").await);
    }

    #[tokio::test]
    async fn test_subscribe_cap_exceeded() {
        let subs = ResourceSubscriptions::new();
        for i in 0..MAX_SUBSCRIPTIONS {
            subs.subscribe(format!("lsp-diagnostics:///file{i}.rs"))
                .await
                .unwrap();
        }
        let result = subs
            .subscribe("lsp-diagnostics:///overflow.rs".to_string())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_snapshot() {
        let subs = ResourceSubscriptions::new();
        subs.subscribe("lsp-diagnostics:///a.rs".to_string())
            .await
            .unwrap();
        subs.subscribe("lsp-diagnostics:///b.rs".to_string())
            .await
            .unwrap();
        let mut snap = subs.snapshot().await;
        snap.sort();
        assert_eq!(snap, ["lsp-diagnostics:///a.rs", "lsp-diagnostics:///b.rs"]);
    }
}
