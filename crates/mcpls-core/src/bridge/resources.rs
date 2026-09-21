//! MCP resource URI codec and subscription tracking for LSP diagnostics.
//!
//! Resources in mcpls use the `lsp-diagnostics:///` scheme (RFC 3986 compliant,
//! empty authority, percent-encoded path). Each resource corresponds to a single
//! file whose diagnostics are cached from LSP `textDocument/publishDiagnostics`
//! notifications.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use thiserror::Error;
use tokio::sync::RwLock;
use url::Url;

use super::lock_std;
use super::state::encode_rfc3986_path_chars;

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

/// Errors produced when adding a URI to a [`ResourceSubscriptions`] set.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SubscriptionError {
    /// The session's subscription set has already reached [`MAX_SUBSCRIPTIONS`].
    #[error("subscription limit of {MAX_SUBSCRIPTIONS} reached")]
    LimitReached,
}

impl From<SubscriptionError> for crate::error::Error {
    fn from(err: SubscriptionError) -> Self {
        match err {
            SubscriptionError::LimitReached => Self::SubscriptionLimitReached {
                max: MAX_SUBSCRIPTIONS,
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

/// Internal state guarded by [`ResourceSubscriptions`]'s `RwLock`.
#[derive(Debug, Default)]
struct SubscriptionState {
    /// Canonical resource URIs currently subscribed -- what the diagnostics
    /// pump checks against.
    canonical: HashSet<String>,
    /// Client-supplied ("raw") URI -> canonical URI, recorded at subscribe
    /// time for entries where the two differ (symlink, macOS `/var` vs
    /// `/private/var`, ...). Lets a later `unsubscribe` for the same raw URI
    /// still resolve to the right entry even if canonicalizing it at
    /// unsubscribe time fails, e.g. because the file was deleted since
    /// subscribing (#499).
    aliases: HashMap<String, String>,
}

/// Tracks which MCP resource URIs the client has subscribed to.
///
/// The hot read path (pump tasks checking before sending notifications) uses
/// a `RwLock` so concurrent readers do not block each other.
#[derive(Debug)]
pub struct ResourceSubscriptions(RwLock<SubscriptionState>);

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

    /// Add a URI to the subscription set.
    ///
    /// Returns `Ok(true)` if newly inserted, `Ok(false)` if already present.
    ///
    /// # Errors
    ///
    /// Returns [`SubscriptionError::LimitReached`] if the set has already
    /// reached [`MAX_SUBSCRIPTIONS`] and `uri` is not already a member.
    pub async fn subscribe(&self, uri: String) -> Result<bool, SubscriptionError> {
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
    pub(crate) async fn record_alias(&self, raw_uri: String, canonical_uri: String) {
        if raw_uri == canonical_uri {
            return;
        }
        let mut state = self.0.write().await;
        if !state.canonical.contains(&canonical_uri) {
            return;
        }
        state.aliases.retain(|_, c| c != &canonical_uri);
        state.aliases.insert(raw_uri, canonical_uri);
    }

    /// Check whether the subscription set is empty.
    ///
    /// Used as a fast path in the diagnostics pump to skip URI construction
    /// when no client has subscribed yet.
    pub async fn is_empty(&self) -> bool {
        self.0.read().await.canonical.is_empty()
    }

    /// Remove a URI from the subscription set.
    ///
    /// Tries `uri` directly against the canonical set first, then falls back
    /// to resolving it as a recorded raw alias (see `Self::record_alias`,
    /// crate-private) -- covers a caller that could not canonicalize the path at
    /// unsubscribe time (e.g. the file was deleted since subscribing) and so
    /// passed the same raw URI it originally subscribed with.
    ///
    /// Returns `true` if a subscription was found and removed.
    pub async fn unsubscribe(&self, uri: &str) -> bool {
        let mut state = self.0.write().await;
        if state.canonical.remove(uri) {
            state.aliases.retain(|_, canonical| canonical != uri);
            return true;
        }
        if let Some(canonical) = state.aliases.remove(uri) {
            state.aliases.retain(|_, c| c != &canonical);
            return state.canonical.remove(&canonical);
        }
        false
    }

    /// Check if a URI is currently subscribed.
    pub async fn contains(&self, uri: &str) -> bool {
        self.0.read().await.canonical.contains(uri)
    }

    /// Return a snapshot of all subscribed URIs (primarily for tests).
    pub async fn snapshot(&self) -> Vec<String> {
        self.0.read().await.canonical.iter().cloned().collect()
    }
}

/// Tracks every live session's [`ResourceSubscriptions`] set for one mcpls process.
///
/// Lets a process-wide reader (the diagnostics pump) ask "does any live
/// session want this URI?" without holding a strong reference to any one
/// session's set.
///
/// Each HTTP session gets its own [`ResourceSubscriptions`] (registered via
/// [`register`](Self::register)) so [`MAX_SUBSCRIPTIONS`] caps per session
/// rather than process-wide, and unsubscribe/subscribe calls from one session
/// can never affect another's entries. The registry holds only [`Weak`]
/// references, so a session's set becomes reclaimable the moment nothing else
/// holds it (i.e. when the session's `McplsServer` instance is dropped on
/// close) — no explicit close-time bookkeeping is required.
///
/// # Reclaim is lazy, not synchronous with session close
///
/// Unlike `CappedSessionManager`'s concurrency permit, which `close_session`
/// frees synchronously the moment a session ends, a dead entry here is only
/// *actually* dropped from the backing `Vec` the next time [`Self::register`],
/// [`Self::any_contains`], or [`Self::is_all_empty`] runs (each prunes dead
/// entries as a side effect). If sessions stop churning and no LSP server is
/// publishing diagnostics, already-dead entries can sit unpruned indefinitely
/// -- bounded (never growing past what churn has actually produced, since
/// [`Self::register`] itself prunes on every call) but not immediate. This is
/// a deliberate GC-on-next-use design, not a leak: the cost is a few dead
/// `Weak` slots, never unbounded growth or a wrong query answer.
///
/// # Known limitation: rmcp's stateless HTTP path (#482)
///
/// This is scoped per `McplsServer` *instance*, not per durable client
/// identity, which only coincides with "per session" on rmcp's legacy
/// (`initialize`-handshake) session path -- rmcp also serves some requests
/// through a stateless, per-request path with a fresh, ephemeral instance.
/// `mcp::server`'s `reject_if_stateless_http`/`is_stateless_http_request`
/// detect that case on `subscribe`/`unsubscribe` and reject it explicitly
/// instead of silently losing the subscription; see their docs for the exact
/// mechanism. [`MAX_SUBSCRIPTIONS`] is unaffected by this gap: nothing is
/// ever recorded into a set on the path those functions reject. See
/// [issue #482](https://github.com/bug-ops/mcpls/issues/482); `crate::transport`'s
/// test module has HTTP-level regression coverage.
#[derive(Debug, Default, Clone)]
pub struct SubscriptionRegistry(Arc<StdMutex<Vec<Weak<ResourceSubscriptions>>>>);

impl SubscriptionRegistry {
    /// Create an empty registry.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::bridge::resources::SubscriptionRegistry;
    ///
    /// let registry = SubscriptionRegistry::new();
    /// let subs = registry.register();
    /// assert!(!std::sync::Arc::ptr_eq(&subs, &registry.register()));
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a fresh [`ResourceSubscriptions`] set scoped to one session and
    /// register it for the aggregate queries below.
    ///
    /// The returned `Arc` is the session's only strong reference; once it (and
    /// any clone of it) is dropped, the registry drops the entry on the next
    /// call to this method or to [`Self::any_contains`]/[`Self::is_all_empty`]
    /// rather than keeping it alive. Pruning dead entries here (not only in
    /// those aggregate queries) keeps the registry bounded even when nothing
    /// ever calls them -- e.g. a workspace with no LSP server publishing
    /// diagnostics -- under sustained registration churn (a `register` call
    /// per request, not per session, on rmcp's stateless HTTP path; see this
    /// type's "Known limitation" section above).
    #[must_use]
    pub fn register(&self) -> Arc<ResourceSubscriptions> {
        let subs = Arc::new(ResourceSubscriptions::new());
        let mut guard = lock_std(&self.0);
        guard.retain(|weak| weak.strong_count() > 0);
        guard.push(Arc::downgrade(&subs));
        subs
    }

    /// Upgrade every still-live entry, dropping dead ones from the registry
    /// along the way so it doesn't grow unbounded across session churn.
    ///
    /// `pub(crate)` (not private) so a caller checking more than one thing
    /// against the same point-in-time set of sessions -- the diagnostics pump
    /// checks both "is everything empty" and "does anyone want this URI" per
    /// notification -- can take one snapshot and query it twice, instead of
    /// each of [`Self::any_contains`]/[`Self::is_all_empty`] separately
    /// locking the registry and re-upgrading every `Weak`.
    pub(crate) fn live_sessions(&self) -> Vec<Arc<ResourceSubscriptions>> {
        let mut guard = lock_std(&self.0);
        guard.retain(|weak| weak.strong_count() > 0);
        guard.iter().filter_map(Weak::upgrade).collect()
    }

    /// Whether any live session has subscribed to `uri`.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::bridge::resources::SubscriptionRegistry;
    ///
    /// tokio::runtime::Runtime::new().unwrap().block_on(async {
    ///     let registry = SubscriptionRegistry::new();
    ///     let subs = registry.register();
    ///     subs.subscribe("lsp-diagnostics:///a.rs".to_string())
    ///         .await
    ///         .unwrap();
    ///     assert!(registry.any_contains("lsp-diagnostics:///a.rs").await);
    /// });
    /// ```
    pub async fn any_contains(&self, uri: &str) -> bool {
        for subs in self.live_sessions() {
            if subs.contains(uri).await {
                return true;
            }
        }
        false
    }

    /// Whether every live session's subscription set is empty.
    ///
    /// Used as a fast path in the diagnostics pump to skip URI construction
    /// when no live session has subscribed to anything yet.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::bridge::resources::SubscriptionRegistry;
    ///
    /// tokio::runtime::Runtime::new().unwrap().block_on(async {
    ///     let registry = SubscriptionRegistry::new();
    ///     assert!(registry.is_all_empty().await);
    ///
    ///     let subs = registry.register();
    ///     subs.subscribe("lsp-diagnostics:///a.rs".to_string())
    ///         .await
    ///         .unwrap();
    ///     assert!(!registry.is_all_empty().await);
    /// });
    /// ```
    pub async fn is_all_empty(&self) -> bool {
        for subs in self.live_sessions() {
            if !subs.is_empty().await {
                return false;
            }
        }
        true
    }

    /// Raw number of entries currently stored, dead or alive, *without*
    /// pruning them first.
    ///
    /// Test-only, and deliberately not pruning: a version of this method that
    /// pruned before reading would report the same small number whether or
    /// not [`Self::register`] itself prunes on the way in, masking exactly
    /// the growth regression this exists to catch. Reading the unpruned
    /// length is what lets an integration test tell "many dead entries piled
    /// up because nothing ever called a pruning method" apart from "the
    /// registry stayed bounded".
    #[cfg(test)]
    pub(crate) fn raw_len(&self) -> usize {
        lock_std(&self.0).len()
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

    /// #499: a stale raw URI recorded via `record_alias` must still resolve
    /// to the canonical entry `subscribe` created, even though the raw and
    /// canonical strings differ (e.g. a symlink or macOS `/var` vs
    /// `/private/var`).
    #[tokio::test]
    async fn test_unsubscribe_resolves_recorded_alias() {
        let subs = ResourceSubscriptions::new();
        let raw = "lsp-diagnostics:///var/tmp/file.rs".to_string();
        let canonical = "lsp-diagnostics:///private/var/tmp/file.rs".to_string();

        subs.subscribe(canonical.clone()).await.unwrap();
        subs.record_alias(raw.clone(), canonical.clone()).await;
        assert!(subs.contains(&canonical).await);

        assert!(subs.unsubscribe(&raw).await);
        assert!(!subs.contains(&canonical).await);
    }

    /// `record_alias` is a no-op when the raw and canonical URIs are equal
    /// (the common case), so it never grows the alias map for entries that
    /// don't need it.
    #[tokio::test]
    async fn test_record_alias_noop_when_raw_equals_canonical() {
        let subs = ResourceSubscriptions::new();
        let uri = "lsp-diagnostics:///tmp/file.rs".to_string();
        subs.subscribe(uri.clone()).await.unwrap();
        subs.record_alias(uri.clone(), uri.clone()).await;

        // No alias was recorded, so unsubscribing under the canonical URI
        // directly is still what resolves it.
        assert!(subs.unsubscribe(&uri).await);
    }

    /// Unsubscribing under the canonical URI directly must also clear any
    /// aliases that pointed at it, so the alias map does not accumulate
    /// stale entries for already-removed subscriptions.
    #[tokio::test]
    async fn test_unsubscribe_by_canonical_clears_stale_aliases() {
        let subs = ResourceSubscriptions::new();
        let raw = "lsp-diagnostics:///var/tmp/file.rs".to_string();
        let canonical = "lsp-diagnostics:///private/var/tmp/file.rs".to_string();

        subs.subscribe(canonical.clone()).await.unwrap();
        subs.record_alias(raw.clone(), canonical.clone()).await;

        assert!(subs.unsubscribe(&canonical).await);
        // The alias must no longer resolve to anything now that the
        // canonical entry it pointed at is gone.
        assert!(!subs.unsubscribe(&raw).await);
    }

    /// #499 site fix (impl-critic C1): re-subscribing to an already-subscribed
    /// canonical URI under distinct genuine percent-encoding variants of the
    /// same filename (`%66`/`%65`/`%2E` for `f`/`e`/`.` in `file.rs`) must not
    /// grow the alias map without bound -- only the most recently recorded
    /// alias for a given canonical URI is kept, so the alias map's size stays
    /// structurally tied to the (already `MAX_SUBSCRIPTIONS`-capped) canonical
    /// set's size, rather than an independent, exhaustible counter.
    #[tokio::test]
    async fn test_record_alias_keeps_only_latest_alias_per_canonical() {
        let subs = ResourceSubscriptions::new();
        let canonical = "lsp-diagnostics:///file.rs".to_string();
        subs.subscribe(canonical.clone()).await.unwrap();

        let raws = [
            "lsp-diagnostics:///%66ile.rs".to_string(),
            "lsp-diagnostics:///fil%65.rs".to_string(),
            "lsp-diagnostics:///file%2Ers".to_string(),
        ];
        for raw in &raws {
            subs.record_alias(raw.clone(), canonical.clone()).await;
        }

        // Every earlier alias for this canonical was displaced -- none of
        // them resolve anymore.
        for raw in &raws[..raws.len() - 1] {
            assert!(!subs.unsubscribe(raw).await);
        }
        // Only the latest recorded alias still resolves, to the same
        // canonical entry.
        let latest = raws.last().unwrap();
        assert!(subs.unsubscribe(latest).await);
        assert!(!subs.contains(&canonical).await);
    }

    /// #499 site fix (impl-critic C3): `record_alias` is a no-op when its
    /// `canonical_uri` argument is not (or is no longer) an actual
    /// subscribed entry -- covers a concurrent `unsubscribe` landing between
    /// a caller's own `subscribe` and `record_alias` calls, which would
    /// otherwise record a dangling alias for an entry that no longer exists.
    ///
    /// Discriminating: `unsubscribe(&raw)` alone can't tell "the guard
    /// skipped recording the alias" apart from "the alias was recorded but
    /// its canonical target was never subscribed either" (both return
    /// `false` from `unsubscribe`'s final `canonical.remove` either way).
    /// Subscribing `canonical` *after* the no-op `record_alias` call
    /// isolates the guard: if it had recorded the alias anyway, `raw` would
    /// now resolve to the (now real) canonical entry; it must not.
    #[tokio::test]
    async fn test_record_alias_noop_for_unsubscribed_canonical() {
        let subs = ResourceSubscriptions::new();
        let raw = "lsp-diagnostics:///var/tmp/file.rs".to_string();
        let canonical = "lsp-diagnostics:///private/var/tmp/file.rs".to_string();

        // Never subscribed (or already unsubscribed by a racing task) --
        // `record_alias` must not record anything for it.
        subs.record_alias(raw.clone(), canonical.clone()).await;

        // Now make `canonical` a real subscribed entry. If the guard above
        // had not fired, `raw` would incorrectly resolve to it.
        subs.subscribe(canonical.clone()).await.unwrap();
        assert!(!subs.unsubscribe(&raw).await);
        assert!(subs.unsubscribe(&canonical).await);
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
        assert_eq!(result, Err(SubscriptionError::LimitReached));
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

    // ------------------------------------------------------------------
    // SubscriptionRegistry
    // ------------------------------------------------------------------

    #[test]
    fn test_registry_register_returns_distinct_sets() {
        let registry = SubscriptionRegistry::new();
        let a = registry.register();
        let b = registry.register();
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[tokio::test]
    async fn test_registry_sessions_are_isolated() {
        let registry = SubscriptionRegistry::new();
        let session_a = registry.register();
        let session_b = registry.register();

        session_a
            .subscribe("lsp-diagnostics:///a.rs".to_string())
            .await
            .unwrap();

        assert!(session_a.contains("lsp-diagnostics:///a.rs").await);
        assert!(!session_b.contains("lsp-diagnostics:///a.rs").await);

        // Cross-session unsubscribe must not touch another session's entry.
        assert!(!session_b.unsubscribe("lsp-diagnostics:///a.rs").await);
        assert!(session_a.contains("lsp-diagnostics:///a.rs").await);
    }

    #[tokio::test]
    async fn test_registry_cap_is_per_session() {
        let registry = SubscriptionRegistry::new();
        let session_a = registry.register();
        let session_b = registry.register();

        for i in 0..MAX_SUBSCRIPTIONS {
            session_a
                .subscribe(format!("lsp-diagnostics:///a{i}.rs"))
                .await
                .unwrap();
        }

        // Session A is at the cap, but session B's own set is untouched.
        assert_eq!(
            session_a
                .subscribe("lsp-diagnostics:///overflow.rs".to_string())
                .await,
            Err(SubscriptionError::LimitReached)
        );
        assert!(
            session_b
                .subscribe("lsp-diagnostics:///b.rs".to_string())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_registry_any_contains_and_is_all_empty() {
        let registry = SubscriptionRegistry::new();
        assert!(registry.is_all_empty().await);
        assert!(!registry.any_contains("lsp-diagnostics:///a.rs").await);

        let session_a = registry.register();
        let _session_b = registry.register();
        session_a
            .subscribe("lsp-diagnostics:///a.rs".to_string())
            .await
            .unwrap();

        assert!(!registry.is_all_empty().await);
        assert!(registry.any_contains("lsp-diagnostics:///a.rs").await);
        assert!(!registry.any_contains("lsp-diagnostics:///other.rs").await);
    }

    #[tokio::test]
    async fn test_registry_reclaims_dropped_session() {
        let registry = SubscriptionRegistry::new();
        let session_a = registry.register();
        session_a
            .subscribe("lsp-diagnostics:///a.rs".to_string())
            .await
            .unwrap();
        assert!(registry.any_contains("lsp-diagnostics:///a.rs").await);

        drop(session_a);

        assert!(!registry.any_contains("lsp-diagnostics:///a.rs").await);
        assert!(registry.is_all_empty().await);
        assert_eq!(registry.0.lock().unwrap().len(), 0);
    }
}
