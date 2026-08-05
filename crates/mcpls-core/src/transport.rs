//! Transport selection for the MCP server.
//!
//! This module defines the [`Transport`] enum that controls how the MCP server
//! communicates with clients. Stdio is always available; HTTP transport is
//! opt-in via the `transport-http` Cargo feature.
//!
//! # Selecting a transport
//!
//! Pass a [`Transport`] value to [`crate::serve_with`] to choose the runtime
//! binding. The default entry point [`crate::serve`] always uses
//! [`Transport::Stdio`].

/// The transport over which the MCP server communicates with clients.
///
/// # Examples
///
/// See [`crate::serve_with`]'s "Shutdown" section before copying this
/// verbatim: under [`Transport::Stdio`], `main` must call
/// `std::process::exit` rather than returning normally, or `SIGTERM`/
/// `SIGINT` can hang while an MCP client's stdin write end is still open
/// (#308).
///
/// ```rust,ignore
/// use mcpls_core::{Transport, serve_with, ServerConfig};
///
/// #[tokio::main]
/// async fn main() {
///     let config = ServerConfig::load().expect("failed to load config");
///     let result = serve_with(config, Transport::Stdio).await;
///     std::process::exit(if result.is_ok() { 0 } else { 1 });
/// }
/// ```
#[non_exhaustive]
pub enum Transport {
    /// Standard I/O transport (default).
    ///
    /// Reads from `stdin` and writes to `stdout`. This is the transport used
    /// by MCP clients that launch mcpls as a child process.
    Stdio,

    /// Streamable HTTP transport (MCP spec 2025-11-25).
    ///
    /// Binds a TCP listener and serves the MCP protocol over HTTP, enabling
    /// network-accessible deployments and clients that speak HTTP rather than
    /// stdio. Only available when the `transport-http` feature is enabled.
    #[cfg(feature = "transport-http")]
    Http(HttpConfig),
}

/// Configuration for the HTTP transport.
///
/// Passed inside [`Transport::Http`] to control the TCP bind address and the
/// URL path the MCP service is mounted at.
///
/// # Note on DNS rebinding
///
/// `rmcp`'s `StreamableHttpService` validates the `Host` header against an
/// allow-list that defaults to loopback addresses only (`localhost`,
/// `127.0.0.1`, `::1`). If you bind to `0.0.0.0` or a non-loopback address,
/// clients must send requests with a `Host` that matches the allow-list, or
/// use a reverse proxy that rewrites the `Host` header.
///
/// # Examples
///
/// ```rust,ignore
/// use std::net::SocketAddr;
/// use mcpls_core::{HttpConfig, Transport};
///
/// let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap(), "/mcp");
/// let transport = Transport::Http(cfg);
/// ```
#[cfg(feature = "transport-http")]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HttpConfig {
    /// TCP address to bind (e.g. `127.0.0.1:3000`).
    pub bind: std::net::SocketAddr,
    /// URL path prefix the MCP service is mounted at (e.g. `"/mcp"`).
    pub path: String,
    /// Maximum size, in bytes, of a single POST request body.
    ///
    /// Enforced by `rmcp`'s `StreamableHttpService` while streaming the body,
    /// independent of `Content-Length` or chunked transfer encoding. Requests
    /// exceeding this limit receive `413 Payload Too Large`. Defaults to
    /// [`HttpConfig::DEFAULT_MAX_REQUEST_BODY_BYTES`] (4 MiB), which
    /// comfortably covers MCP tool-call request bodies (large results, e.g.
    /// from `workspace/symbol` or bulk edits, are returned in the response,
    /// which this limit does not constrain). A value of `0` rejects every
    /// POST body.
    pub max_request_body_bytes: usize,
    /// Maximum number of concurrent HTTP sessions.
    ///
    /// This is a hard bound, enforced atomically at session creation via a
    /// semaphore — never more than this many sessions can be active at once,
    /// regardless of request concurrency.
    /// Requests that would start a new session beyond this limit receive
    /// `429 Too Many Requests`. Defaults to
    /// [`HttpConfig::DEFAULT_MAX_CONCURRENT_SESSIONS`]. A value of `0`
    /// rejects every session.
    pub max_concurrent_sessions: usize,
}

#[cfg(feature = "transport-http")]
impl HttpConfig {
    /// Default request body size cap (4 MiB), matching `rmcp`'s own default.
    pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;
    /// Default concurrent HTTP session cap.
    pub const DEFAULT_MAX_CONCURRENT_SESSIONS: usize = 100;

    /// Create an [`HttpConfig`] with default body-size and session caps.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use mcpls_core::HttpConfig;
    ///
    /// let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap(), "/mcp");
    /// ```
    pub fn new(bind: std::net::SocketAddr, path: impl Into<String>) -> Self {
        Self {
            bind,
            path: path.into(),
            max_request_body_bytes: Self::DEFAULT_MAX_REQUEST_BODY_BYTES,
            max_concurrent_sessions: Self::DEFAULT_MAX_CONCURRENT_SESSIONS,
        }
    }

    /// Override the maximum POST request body size in bytes.
    #[must_use]
    pub const fn with_max_request_body_bytes(mut self, bytes: usize) -> Self {
        self.max_request_body_bytes = bytes;
        self
    }

    /// Override the maximum number of concurrent HTTP sessions.
    #[must_use]
    pub const fn with_max_concurrent_sessions(mut self, max: usize) -> Self {
        self.max_concurrent_sessions = max;
        self
    }
}

use rmcp::ServiceExt as _;
#[cfg(feature = "transport-http")]
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
#[cfg(feature = "transport-http")]
use rmcp::transport::streamable_http_server::session::local::{
    LocalSessionManager, LocalSessionManagerError,
};
#[cfg(feature = "transport-http")]
use rmcp::transport::streamable_http_server::session::{
    ServerSseMessage, SessionId, SessionManager,
};

/// Waits for a shutdown signal: `SIGTERM` on Unix (as sent by containers and
/// systemd) or `Ctrl-C` (`SIGINT`) on any platform.
///
/// Shared between [`run_stdio`] and [`run_http`] so both transports react to
/// the same signals the same way.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = sigterm.recv() => {},
                }
            }
            Err(e) => {
                tracing::warn!(
                    "SIGTERM handler registration failed ({e}), falling back to SIGINT only"
                );
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Run the MCP server over stdio.
///
/// Serves the given `mcp_server` using stdin/stdout and populates `peer_cell`
/// once the transport is established so that diagnostic pump tasks can begin
/// forwarding `resources/updated` notifications. Returns as soon as either
/// the stdio transport closes (client disconnect / stdin EOF) or a `SIGTERM`/
/// `SIGINT` is received, so callers can run orderly cleanup — such as
/// [`crate::bridge::Translator::shutdown_servers`] — before the process
/// exits. On signal, the in-flight `RunningService` is dropped rather than
/// awaited to completion; `rmcp` closes it asynchronously in that case,
/// which is acceptable here since the process exits shortly after --
/// callers must exit via `std::process::exit` rather than returning
/// normally from `main`, or an uncancellable `tokio::io::stdin()` blocking
/// thread can stall runtime shutdown indefinitely (see `mcpls-cli`'s
/// `main.rs` and #308).
pub(crate) async fn run_stdio(
    mcp_server: crate::mcp::McplsServer,
    peer_cell: &tokio::sync::OnceCell<rmcp::Peer<rmcp::RoleServer>>,
) -> Result<(), crate::Error> {
    let service = mcp_server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| crate::Error::McpServer(format!("Failed to start MCP server: {e}")))?;

    if let Err(e) = peer_cell.set(service.peer().clone()) {
        tracing::debug!("Peer cell already set ({}), ignoring", e);
    }

    tokio::select! {
        result = service.waiting() => result
            .map(|_| ())
            .map_err(|e| crate::Error::McpServer(format!("MCP server error: {e}"))),
        () = wait_for_shutdown_signal() => {
            tracing::info!("shutdown signal received, stopping stdio transport");
            Ok(())
        }
    }
}

/// Run the MCP server over Streamable HTTP (MCP spec 2025-11-25).
///
/// Binds `cfg.bind`, mounts the MCP service at `cfg.path` (and `/`), and
/// serves until `Ctrl-C` or `SIGTERM` is received.
///
/// Each HTTP session receives its own `McplsServer` clone. The shared
/// `Arc<Translator>` inside is the same across all sessions, so LSP state is
/// still global per process.
///
/// # Note
///
/// Diagnostic push notifications (`resources/updated`) are not forwarded to
/// HTTP sessions in this release — the single-peer pump architecture from
/// stdio is kept as-is. Clients can still poll diagnostics via the existing
/// MCP tools. A follow-up issue will add per-session broadcast.
///
/// # Resource limits
///
/// POST bodies exceeding `cfg.max_request_body_bytes` are rejected with
/// `413 Payload Too Large` (enforced by `rmcp`). Once `cfg.max_concurrent_sessions`
/// sessions are active, a request that would start a new one is rejected with
/// `429 Too Many Requests` — enforced as a hard bound at session creation by
/// [`CappedSessionManager`] and surfaced over HTTP by [`enforce_session_cap`].
///
/// # Shutdown
///
/// On `SIGTERM`/`SIGINT`, in-flight connections get up to
/// [`HTTP_GRACEFUL_SHUTDOWN_TIMEOUT`] to finish before this function returns
/// regardless — bounding shutdown this way lets the caller run its own
/// post-shutdown cleanup (e.g. closing registered LSP servers) even if a
/// connection never observes the cancellation (a stuck SSE stream, say).
#[cfg(feature = "transport-http")]
// `session_manager` and `service` are moved into `app`, which is served until
// shutdown — clippy's drop-tightening heuristic misreads that as an
// early-droppable temporary because both types embed `tokio::sync` lock types
// (`CappedSessionManager`'s `Mutex`, `StreamableHttpService`'s `RwLock`s).
#[allow(clippy::significant_drop_tightening)]
pub(crate) async fn run_http(
    mcp_server: crate::mcp::McplsServer,
    cfg: HttpConfig,
) -> Result<(), crate::Error> {
    use std::sync::Arc;

    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    use tokio_util::sync::CancellationToken;

    let session_manager = Arc::new(CappedSessionManager::new(cfg.max_concurrent_sessions));
    let cancel = CancellationToken::new();

    let mcp_for_factory = mcp_server.clone();
    // StreamableHttpServerConfig is #[non_exhaustive]; construct via Default then mutate.
    let mut http_cfg = StreamableHttpServerConfig::default();
    http_cfg.cancellation_token = cancel.clone();
    http_cfg.max_request_body_bytes = cfg.max_request_body_bytes;

    let service = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(mcp_for_factory.clone()),
        session_manager,
        http_cfg,
    );

    let app = axum::Router::new()
        .nest_service(&cfg.path, service.clone())
        .route_service("/", service)
        .layer(axum::middleware::from_fn(enforce_session_cap));

    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .map_err(|e| crate::Error::McpServer(format!("bind {}: {e}", cfg.bind)))?;

    tracing::info!(addr = %cfg.bind, path = %cfg.path, "MCP HTTP transport listening");
    if !cfg.bind.ip().is_loopback() {
        tracing::warn!(
            addr = %cfg.bind,
            "binding to a non-loopback address: mcpls performs no authentication of its own on \
             any transport — place this endpoint behind a reverse proxy that enforces \
             authentication. The proxy must also rewrite the Host header, since rmcp's Host \
             validation allows only localhost/127.0.0.1/::1 by default"
        );
    }

    // `cancel` is cancelled exactly once, when the shutdown signal fires
    // (below). Cloned first so the force-timeout branch can observe that
    // same moment independently of the `with_graceful_shutdown` closure,
    // which consumes its own clone.
    let cancel_for_force_timeout = cancel.clone();
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        wait_for_shutdown_signal().await;
        cancel.cancel();
    });

    // The force-timeout only starts counting once `cancel` is actually
    // cancelled — i.e. once a shutdown signal has been received — not from
    // server startup. Without that ordering, `tokio::time::timeout` wrapping
    // `serve` directly would tear down the listener after
    // `HTTP_GRACEFUL_SHUTDOWN_TIMEOUT` of ordinary uptime, signal or not.
    // This bounds only the "drain in-flight connections after shutdown was
    // requested" phase, so a connection that never observes `cancel` (e.g. a
    // stuck SSE stream) can't hang the caller's post-shutdown cleanup
    // (draining/closing LSP servers) indefinitely.
    tokio::select! {
        result = serve => result.map_err(|e| crate::Error::McpServer(format!("http serve: {e}"))),
        () = async move {
            cancel_for_force_timeout.cancelled().await;
            tokio::time::sleep(HTTP_GRACEFUL_SHUTDOWN_TIMEOUT).await;
        } => {
            tracing::warn!(
                timeout = ?HTTP_GRACEFUL_SHUTDOWN_TIMEOUT,
                "HTTP graceful shutdown did not complete in time, proceeding with shutdown anyway"
            );
            Ok(())
        }
    }
}

/// Upper bound [`run_http`] waits, once shutdown has been signaled, for
/// `axum`'s graceful shutdown to finish draining in-flight connections
/// before giving up and returning anyway.
#[cfg(feature = "transport-http")]
const HTTP_GRACEFUL_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Wraps [`LocalSessionManager`], bounding concurrent HTTP sessions to a
/// fixed capacity.
///
/// A [`tokio::sync::Semaphore`] permit is acquired atomically inside
/// [`create_session`](SessionManager::create_session) — before delegating to
/// the inner manager — and held for the session's lifetime, released in
/// [`close_session`](SessionManager::close_session). This makes the cap a
/// true hard bound: the check and the reservation happen as one step, so no
/// number of concurrent requests can observe spare capacity and all proceed
/// past it (a "check-then-create" race that a separate read of the session
/// count could not avoid).
///
/// Enforcement lives here, at the `SessionManager` layer, rather than in Axum
/// middleware sniffing request headers, because that is the only place
/// guaranteed to run exactly when — and only when — a session is actually
/// created. `rmcp` 3.0.0's `StreamableHttpService::handle_post` calls
/// `create_session` solely on legacy-mode POSTs with no `Mcp-Session-Id`
/// header (the `initialize` handshake); modern-protocol POSTs (SEP-2567,
/// protocol `>= 2026-07-28`, which removes sessions entirely) and
/// `server/discover` requests share that same header-less shape but take a
/// stateless code path that never calls `create_session`. A header-based
/// middleware heuristic can't tell these apart without duplicating `rmcp`'s
/// internal protocol classification, so it either 429s traffic that never
/// consumed a session slot, or — in an all-stateless deployment — never
/// fires at all.
///
/// `restore_session` and `event_store` deliberately use
/// [`SessionManager`]'s trait defaults (`NotSupported` / `None`) instead of
/// delegating to `inner`: `HttpConfig` exposes no session-store knob, so
/// these are unreachable today, but delegating them would let a restored
/// session skip the semaphore entirely — a cap bypass. Leave them as
/// defaults; overriding them to delegate is not a bug fix.
#[cfg(feature = "transport-http")]
struct CappedSessionManager {
    inner: LocalSessionManager,
    semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    permits:
        tokio::sync::Mutex<std::collections::HashMap<SessionId, tokio::sync::OwnedSemaphorePermit>>,
}

#[cfg(feature = "transport-http")]
impl CappedSessionManager {
    fn new(max_sessions: usize) -> Self {
        Self {
            inner: LocalSessionManager::default(),
            semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(max_sessions)),
            permits: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

/// Marker embedded in [`CappedSessionManagerError::CapReached`]'s rendered
/// message.
///
/// `rmcp`'s `StreamableHttpService` always maps `create_session` failures to
/// a generic `500 Internal Server Error` (`internal_error_response` in
/// `server_side_http.rs` is a fixed, non-configurable mapping — `rmcp` gives
/// callers no other hook). [`enforce_session_cap`] looks for this marker in
/// the response body to translate a capacity rejection into
/// `429 Too Many Requests` without misclassifying other `create_session`
/// failures as capacity issues.
#[cfg(feature = "transport-http")]
const SESSION_CAP_MARKER: &str = "mcpls-http-session-cap-reached";

/// Error type for [`CappedSessionManager`].
#[cfg(feature = "transport-http")]
#[derive(Debug, thiserror::Error)]
enum CappedSessionManagerError {
    /// The concurrent-session cap was already reached.
    #[error("{SESSION_CAP_MARKER}: maximum concurrent HTTP sessions already active")]
    CapReached,
    /// The wrapped [`LocalSessionManager`] failed.
    #[error(transparent)]
    Inner(#[from] LocalSessionManagerError),
}

#[cfg(feature = "transport-http")]
impl SessionManager for CappedSessionManager {
    type Error = CappedSessionManagerError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let permit = self
            .semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| CappedSessionManagerError::CapReached)?;
        let (id, transport) = self.inner.create_session().await?;
        self.permits.lock().await.insert(id.clone(), permit);
        Ok((id, transport))
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, Self::Error> {
        Ok(self.inner.initialize_session(id, message).await?)
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        Ok(self.inner.has_session(id).await?)
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        // Release the permit unconditionally, before propagating any error from
        // `inner.close_session`: on error the inner manager has already dropped
        // the session from its own table (see `LocalSessionManager::close_session`),
        // so skipping the removal here would leak the permit permanently and
        // monotonically shrink capacity.
        self.permits.lock().await.remove(id);
        self.inner.close_session(id).await?;
        Ok(())
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl futures::Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error>
    {
        Ok(self.inner.create_stream(id, message).await?)
    }

    async fn accept_message(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<(), Self::Error> {
        Ok(self.inner.accept_message(id, message).await?)
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl futures::Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error>
    {
        Ok(self.inner.create_standalone_stream(id).await?)
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl futures::Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error>
    {
        Ok(self.inner.resume(id, last_event_id).await?)
    }
}

/// Axum middleware that rewrites `rmcp`'s generic `500 Internal Server Error`
/// into `429 Too Many Requests` when the failure was
/// [`CappedSessionManagerError::CapReached`] (detected via
/// [`SESSION_CAP_MARKER`] in the response body), adding a `Retry-After`
/// header.
///
/// This runs as response post-processing rather than a request pre-check
/// because only the real [`SessionManager::create_session`] call — deep
/// inside `rmcp` — knows whether a given request actually attempts to create
/// a session; see [`CappedSessionManager`]'s docs for why that can't be
/// determined from the request alone.
#[cfg(feature = "transport-http")]
async fn enforce_session_cap(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let response = next.run(request).await;
    if response.status() != axum::http::StatusCode::INTERNAL_SERVER_ERROR {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    // `create_session` failures always render as a small `Full<Bytes>` body
    // (`internal_error_response` in rmcp's `server_side_http.rs`); the large
    // streaming SSE/JSON success bodies never carry a 500 status, so this
    // never touches them. 64 KiB is far beyond any realistic error message.
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        // Buffering the original error body failed (e.g. it exceeded the 64
        // KiB cap, which should never happen per the comment above, or the
        // body stream errored). Preserve the 500 status but substitute a
        // minimal fallback body rather than dropping the error entirely.
        return axum::response::Response::from_parts(
            parts,
            axum::body::Body::from("Internal Server Error"),
        );
    };

    if bytes
        .windows(SESSION_CAP_MARKER.len())
        .any(|window| window == SESSION_CAP_MARKER.as_bytes())
    {
        parts.status = axum::http::StatusCode::TOO_MANY_REQUESTS;
        parts.headers.insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from_static("1"),
        );
        return axum::response::Response::from_parts(
            parts,
            axum::body::Body::from("Too Many Requests: maximum concurrent HTTP sessions reached"),
        );
    }

    axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    /// `Transport::Stdio` is always constructible regardless of feature flags.
    #[test]
    fn test_transport_stdio_variant() {
        let t = super::Transport::Stdio;
        assert!(matches!(t, super::Transport::Stdio));
    }

    /// #241: `run_stdio` must not hang when the transport never even
    /// establishes — it must surface the failure promptly.
    ///
    /// This is the closest portable coverage of `run_stdio`'s non-signal
    /// path achievable here: `run_stdio` is hardcoded to the process's real
    /// stdin/stdout (no injectable transport), and this crate is
    /// `deny(unsafe_code)`, so a test can't redirect the fd to simulate "the
    /// MCP handshake completes, *then* stdin closes" — the specific
    /// scenario that would drive `service.waiting()` to resolve inside the
    /// `tokio::select!` and hit its `Ok(())` arm. What a test *can* rely on:
    /// under `cargo nextest`, each test's stdin is already closed before the
    /// test body runs, so `mcp_server.serve(...)` fails during the initial
    /// `initialize` handshake — before `run_stdio` ever reaches the
    /// `select!`. That still exercises real production code (the `.serve()`
    /// call and its error mapping) and proves `run_stdio` returns promptly
    /// rather than hanging, which is what a broken `select!` (e.g. one
    /// missing a branch, or awaiting the wrong future) would look like.
    #[tokio::test]
    async fn test_run_stdio_returns_promptly_when_stdin_is_already_closed() {
        use std::path::PathBuf;
        use std::sync::Arc;

        use tokio::sync::Mutex;

        use crate::bridge::{NotificationCache, ResourceSubscriptions, Translator};
        use crate::mcp::McplsServer;

        let translator = Arc::new(Translator::new());
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let workspace_roots: Arc<[PathBuf]> = Arc::from(Vec::new());
        let subs = Arc::new(ResourceSubscriptions::new());
        let server = McplsServer::new(translator, notification_cache, workspace_roots, subs, false);
        let peer_cell = tokio::sync::OnceCell::new();

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::run_stdio(server, &peer_cell),
        )
        .await;

        assert!(
            outcome.is_ok(),
            "run_stdio must not hang when stdin is already closed"
        );
        let result = outcome.unwrap();
        assert!(
            matches!(result, Err(crate::Error::McpServer(_))),
            "expected a McpServer error from the failed handshake, got: {result:?}"
        );
    }

    #[cfg(feature = "transport-http")]
    mod http_tests {
        use std::net::SocketAddr;

        use super::super::{HttpConfig, Transport};

        #[test]
        fn test_http_config_fields() {
            let addr: SocketAddr = "127.0.0.1:3000".parse().unwrap();
            let cfg = HttpConfig::new(addr, "/mcp");
            assert_eq!(cfg.bind, addr);
            assert_eq!(cfg.path, "/mcp");
        }

        #[test]
        fn test_http_config_clone() {
            let cfg = HttpConfig::new("127.0.0.1:3001".parse().unwrap(), "/test");
            let cloned = cfg.clone();
            assert_eq!(cloned.bind, cfg.bind);
            assert_eq!(cloned.path, cfg.path);
        }

        #[test]
        fn test_transport_http_variant() {
            let cfg = HttpConfig::new("127.0.0.1:3002".parse().unwrap(), "/mcp");
            let t = Transport::Http(cfg);
            assert!(matches!(t, Transport::Http(_)));
        }

        #[test]
        fn test_http_config_new_uses_default_limits() {
            let cfg = HttpConfig::new("127.0.0.1:3003".parse().unwrap(), "/mcp");
            assert_eq!(
                cfg.max_request_body_bytes,
                HttpConfig::DEFAULT_MAX_REQUEST_BODY_BYTES
            );
            assert_eq!(
                cfg.max_concurrent_sessions,
                HttpConfig::DEFAULT_MAX_CONCURRENT_SESSIONS
            );
        }

        #[test]
        fn test_http_config_with_max_request_body_bytes_overrides_default() {
            let cfg = HttpConfig::new("127.0.0.1:3004".parse().unwrap(), "/mcp")
                .with_max_request_body_bytes(1024);
            assert_eq!(cfg.max_request_body_bytes, 1024);
            assert_eq!(
                cfg.max_concurrent_sessions,
                HttpConfig::DEFAULT_MAX_CONCURRENT_SESSIONS
            );
        }

        #[test]
        fn test_http_config_with_max_concurrent_sessions_overrides_default() {
            let cfg = HttpConfig::new("127.0.0.1:3005".parse().unwrap(), "/mcp")
                .with_max_concurrent_sessions(5);
            assert_eq!(cfg.max_concurrent_sessions, 5);
            assert_eq!(
                cfg.max_request_body_bytes,
                HttpConfig::DEFAULT_MAX_REQUEST_BODY_BYTES
            );
        }

        /// Verifies `run_http` binds successfully and accepts TCP connections.
        #[tokio::test]
        async fn test_run_http_binds() {
            use std::path::PathBuf;
            use std::sync::Arc;

            use tokio::sync::Mutex;

            use crate::bridge::{NotificationCache, ResourceSubscriptions, Translator};
            use crate::mcp::McplsServer;

            let translator = Arc::new(Translator::new());
            let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
            let workspace_roots: Arc<[PathBuf]> = Arc::from(Vec::new());
            let subs = Arc::new(ResourceSubscriptions::new());
            let server =
                McplsServer::new(translator, notification_cache, workspace_roots, subs, false);

            // Bind port 0 so the OS assigns a free port.
            let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = probe.local_addr().unwrap();
            drop(probe);

            let cfg = HttpConfig::new(addr, "/mcp");

            let server_task = tokio::spawn(super::super::run_http(server, cfg));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            // A successful TCP connect proves the listener is up.
            let connected = tokio::net::TcpStream::connect(addr).await;
            assert!(
                connected.is_ok(),
                "HTTP listener should accept TCP connections"
            );

            server_task.abort();
        }

        /// #241 C1 regression: `run_http` must not self-terminate after
        /// `HTTP_GRACEFUL_SHUTDOWN_TIMEOUT` of ordinary uptime when no
        /// shutdown signal has been sent — the graceful-shutdown timeout
        /// must only start counting once a signal actually arrives, not
        /// from server startup.
        ///
        /// Uses `#[tokio::test(start_paused = true)]` plus
        /// `tokio::time::advance` to fast-forward virtual time past the
        /// timeout instead of sleeping the real 30s. Under the bug this
        /// regresses against — `tokio::time::timeout(HTTP_GRACEFUL_SHUTDOWN_TIMEOUT,
        /// serve)` wrapping the whole `serve` future from construction —
        /// advancing virtual time past the timeout resolves that timer and
        /// finishes the task immediately, even with no signal sent. Under
        /// the fix, nothing inside `run_http` starts a timer until `cancel`
        /// is cancelled, so this advance must have no effect and the task
        /// must still be running.
        #[tokio::test(start_paused = true)]
        async fn test_run_http_does_not_self_terminate_without_signal() {
            let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = probe.local_addr().unwrap();
            drop(probe);

            let cfg = HttpConfig::new(addr, "/mcp");
            let server_task = tokio::spawn(super::super::run_http(test_server(), cfg));

            // Let the spawned task make initial progress (bind the
            // listener, enter its `select!`) without depending on any real
            // or virtual delay.
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }

            // Fast-forward well past `HTTP_GRACEFUL_SHUTDOWN_TIMEOUT` with
            // no shutdown signal ever sent.
            tokio::time::advance(
                super::super::HTTP_GRACEFUL_SHUTDOWN_TIMEOUT + std::time::Duration::from_secs(5),
            )
            .await;
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }

            assert!(
                !server_task.is_finished(),
                "run_http must still be serving after HTTP_GRACEFUL_SHUTDOWN_TIMEOUT of uptime \
                 with no shutdown signal sent"
            );

            server_task.abort();
        }

        /// Verifies `run_http` returns an error when the bind address is already in use.
        #[tokio::test]
        async fn test_run_http_bind_error() {
            use std::path::PathBuf;
            use std::sync::Arc;

            use tokio::sync::Mutex;

            use crate::bridge::{NotificationCache, ResourceSubscriptions, Translator};
            use crate::mcp::McplsServer;

            // Hold a listener to make the port unavailable.
            let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = occupied.local_addr().unwrap();

            let translator = Arc::new(Translator::new());
            let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
            let workspace_roots: Arc<[PathBuf]> = Arc::from(Vec::new());
            let subs = Arc::new(ResourceSubscriptions::new());
            let server =
                McplsServer::new(translator, notification_cache, workspace_roots, subs, false);

            let cfg = HttpConfig::new(addr, "/mcp");

            let result = super::super::run_http(server, cfg).await;
            assert!(
                result.is_err(),
                "run_http should fail when port is occupied"
            );

            drop(occupied);
        }

        /// Builds a `McplsServer` with empty/default collaborators, matching the
        /// setup shared by every `run_http`-driving test in this module.
        fn test_server() -> crate::mcp::McplsServer {
            use std::path::PathBuf;
            use std::sync::Arc;

            use tokio::sync::Mutex;

            use crate::bridge::{NotificationCache, ResourceSubscriptions, Translator};
            use crate::mcp::McplsServer;

            let translator = Arc::new(Translator::new());
            let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
            let workspace_roots: Arc<[PathBuf]> = Arc::from(Vec::new());
            let subs = Arc::new(ResourceSubscriptions::new());
            McplsServer::new(translator, notification_cache, workspace_roots, subs, false)
        }

        /// Sends a raw HTTP/1.1 POST request over TCP and returns the raw response
        /// text (status line, headers, and body). Used because neither `reqwest`
        /// nor `tower`/`http-body-util` are available as dev-dependencies here.
        async fn raw_http_post(
            addr: SocketAddr,
            path: &str,
            extra_headers: &str,
            body: &[u8],
        ) -> String {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let request = format!(
                "POST {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{extra_headers}Content-Length: {}\r\n\r\n",
                body.len()
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();

            let mut response = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                match tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut buf))
                    .await
                {
                    Ok(Ok(0)) | Err(_) => break,
                    Ok(Ok(n)) => response.extend_from_slice(&buf[..n]),
                    Ok(Err(e)) => panic!("read error: {e}"),
                }
            }
            String::from_utf8_lossy(&response).into_owned()
        }

        /// A POST body exceeding `cfg.max_request_body_bytes` must be rejected
        /// with `413 Payload Too Large`, proving the config value reaches
        /// `StreamableHttpServerConfig::max_request_body_bytes`.
        #[tokio::test]
        async fn test_run_http_rejects_oversized_body_with_413() {
            let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = probe.local_addr().unwrap();
            drop(probe);

            let cfg = HttpConfig::new(addr, "/mcp").with_max_request_body_bytes(64);
            let server_task = tokio::spawn(super::super::run_http(test_server(), cfg));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let oversized_body = vec![b'a'; 65];
            let response = raw_http_post(
                addr,
                "/mcp",
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n",
                &oversized_body,
            )
            .await;

            assert!(
                response.starts_with("HTTP/1.1 413"),
                "expected 413 Payload Too Large, got: {response}"
            );

            server_task.abort();
        }

        /// A POST body within `cfg.max_request_body_bytes` must not be rejected
        /// for size — it reaches JSON deserialization instead (the body here is
        /// intentionally not valid JSON-RPC, so a non-413 error distinguishes
        /// "passed the size check" from "was a valid request").
        #[tokio::test]
        async fn test_run_http_accepts_body_within_limit() {
            let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = probe.local_addr().unwrap();
            drop(probe);

            let cfg = HttpConfig::new(addr, "/mcp").with_max_request_body_bytes(64);
            let server_task = tokio::spawn(super::super::run_http(test_server(), cfg));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let small_body = vec![b'a'; 32];
            let response = raw_http_post(
                addr,
                "/mcp",
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n",
                &small_body,
            )
            .await;

            assert!(
                !response.starts_with("HTTP/1.1 413"),
                "body within limit must not be rejected as too large, got: {response}"
            );

            server_task.abort();
        }

        /// `CappedSessionManager::create_session` must enforce a hard bound:
        /// once `max_sessions` sessions exist, the next `create_session` call
        /// fails with the capacity marker, and closing a session frees the
        /// slot back up for a subsequent `create_session` to succeed.
        // `manager` is used until the end of the test — clippy's drop-tightening
        // heuristic misreads that as an early-droppable temporary because
        // `CappedSessionManager` embeds a `tokio::sync::Mutex`.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test]
        async fn test_capped_session_manager_enforces_hard_bound() {
            use rmcp::transport::streamable_http_server::session::SessionManager as _;

            let manager = super::super::CappedSessionManager::new(1);

            let (first_id, _transport) = manager.create_session().await.unwrap();

            let second_err = manager.create_session().await.map(|_| ()).unwrap_err();
            assert!(
                matches!(
                    second_err,
                    super::super::CappedSessionManagerError::CapReached
                ),
                "expected CapReached once at capacity, got: {second_err:?}"
            );

            manager.close_session(&first_id).await.unwrap();

            let (third_id, _transport) = manager.create_session().await.unwrap();
            assert_ne!(first_id, third_id);
        }

        /// Regression guard for S2: concurrent `create_session` calls must not
        /// overshoot `max_sessions`. Unlike the sequential test above (which
        /// would pass even against a racy check-then-create implementation),
        /// this spawns `N > max_sessions` calls at once and asserts exactly
        /// `max_sessions` succeed — the one test shape that actually
        /// distinguishes the atomic-semaphore design from a TOCTOU race.
        // `manager` is used until the end of the test — see the identical
        // drop-tightening note on `test_capped_session_manager_enforces_hard_bound`.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test]
        async fn test_capped_session_manager_bounds_concurrent_create_session() {
            use rmcp::transport::streamable_http_server::session::SessionManager as _;

            const MAX_SESSIONS: usize = 5;
            const CONCURRENT_ATTEMPTS: usize = 25;

            let manager =
                std::sync::Arc::new(super::super::CappedSessionManager::new(MAX_SESSIONS));

            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..CONCURRENT_ATTEMPTS {
                let manager = manager.clone();
                tasks.spawn(async move { manager.create_session().await.is_ok() });
            }

            let mut succeeded = 0usize;
            while let Some(result) = tasks.join_next().await {
                if result.unwrap() {
                    succeeded += 1;
                }
            }

            assert_eq!(
                succeeded, MAX_SESSIONS,
                "exactly max_sessions concurrent create_session calls must succeed"
            );
        }

        /// Narrower unit test of the `enforce_session_cap` middleware itself
        /// (rather than the full `run_http` wiring): a `500` response whose
        /// body carries `SESSION_CAP_MARKER` must be rewritten to `429` with
        /// a `Retry-After` header.
        #[tokio::test]
        async fn test_enforce_session_cap_rewrites_capacity_marker_to_429() {
            let app = axum::Router::new()
                .route(
                    "/",
                    axum::routing::post(|| async {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            format!(
                                "Encounter an error when create session: {}: maximum concurrent \
                                 HTTP sessions already active",
                                super::super::SESSION_CAP_MARKER
                            ),
                        )
                    }),
                )
                .layer(axum::middleware::from_fn(super::super::enforce_session_cap));

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let response = raw_http_post(addr, "/", "", b"{}").await;
            assert!(
                response.starts_with("HTTP/1.1 429"),
                "expected 429 for a marker-carrying 500, got: {response}"
            );
            assert!(
                response.to_lowercase().contains("retry-after"),
                "expected a Retry-After header, got: {response}"
            );

            server_task.abort();
        }

        /// A `500` response whose body does *not* carry `SESSION_CAP_MARKER`
        /// (an unrelated internal error) must pass through unchanged, proving
        /// the middleware doesn't misclassify every `500` as a capacity
        /// rejection.
        #[tokio::test]
        async fn test_enforce_session_cap_leaves_unrelated_500_untouched() {
            let app = axum::Router::new()
                .route(
                    "/",
                    axum::routing::post(|| async {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            "Encounter an error when create session: some unrelated failure",
                        )
                    }),
                )
                .layer(axum::middleware::from_fn(super::super::enforce_session_cap));

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let response = raw_http_post(addr, "/", "", b"{}").await;
            assert!(
                response.starts_with("HTTP/1.1 500"),
                "unrelated 500s must not be rewritten to 429, got: {response}"
            );

            server_task.abort();
        }

        /// End-to-end: with `max_concurrent_sessions(1)`, a second concurrent
        /// `initialize` handshake over `run_http` must be rejected with `429`
        /// once the first session is established.
        #[tokio::test]
        async fn test_run_http_rejects_new_session_at_capacity_with_429() {
            let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = probe.local_addr().unwrap();
            drop(probe);

            let cfg = HttpConfig::new(addr, "/mcp").with_max_concurrent_sessions(1);
            let server_task = tokio::spawn(super::super::run_http(test_server(), cfg));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let initialize_body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
            let accept_headers =
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n";

            // First handshake must succeed and establish a session.
            let first = raw_http_post(addr, "/mcp", accept_headers, initialize_body).await;
            assert!(
                first.starts_with("HTTP/1.1 200"),
                "first initialize handshake should succeed, got: {first}"
            );

            // Second handshake, with the sole slot still held, must be capped.
            let second = raw_http_post(addr, "/mcp", accept_headers, initialize_body).await;
            assert!(
                second.starts_with("HTTP/1.1 429"),
                "second initialize handshake should be rejected once at capacity, got: {second}"
            );

            server_task.abort();
        }

        /// S1 non-regression: a modern-protocol (`>= 2026-07-28`, SEP-2567
        /// stateless) request never calls `SessionManager::create_session` —
        /// `rmcp` serves it directly without touching the session table — so
        /// it must not be rejected by the cap even while
        /// `max_concurrent_sessions` legacy sessions are already active. This
        /// guards against a future refactor reintroducing request-header
        /// sniffing for the cap decision (the bug this design replaced).
        #[tokio::test]
        async fn test_run_http_stateless_request_bypasses_session_cap() {
            let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = probe.local_addr().unwrap();
            drop(probe);

            let cfg = HttpConfig::new(addr, "/mcp").with_max_concurrent_sessions(1);
            let server_task = tokio::spawn(super::super::run_http(test_server(), cfg));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let accept_headers =
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n";

            // Fill the sole legacy-session slot.
            let legacy_initialize = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
            let legacy = raw_http_post(addr, "/mcp", accept_headers, legacy_initialize).await;
            assert!(
                legacy.starts_with("HTTP/1.1 200"),
                "legacy initialize should succeed and consume the sole session slot, got: {legacy}"
            );

            // A stateless request (protocolVersion >= 2026-07-28) never
            // creates a session, so it must bypass the cap entirely even
            // though the slot above is still held.
            let stateless_initialize = br#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
            let stateless = raw_http_post(addr, "/mcp", accept_headers, stateless_initialize).await;
            assert!(
                !stateless.starts_with("HTTP/1.1 429"),
                "stateless requests must bypass the session cap entirely, got: {stateless}"
            );

            server_task.abort();
        }

        /// Captures `tracing` events emitted while a closure runs, since this
        /// codebase has no existing `tracing_test`-style capture helper to
        /// reuse (only `mcpls-cli/src/logging.rs` sets up a real/global
        /// subscriber, which isn't suitable for assertions).
        #[derive(Clone, Default)]
        struct CapturedMessages(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedMessages {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                struct MessageVisitor(String);
                impl tracing::field::Visit for MessageVisitor {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        if field.name() == "message" {
                            self.0 = format!("{value:?}");
                        }
                    }
                }
                let mut visitor = MessageVisitor(String::new());
                event.record(&mut visitor);
                self.0.lock().unwrap().push(visitor.0);
            }
        }

        /// #233: binding to a non-loopback address must log a warning that
        /// tells operators to put the endpoint behind a reverse proxy that
        /// *enforces* authentication (not the inverted "ensure no
        /// authentication is required" wording it replaced).
        #[tokio::test]
        async fn test_run_http_non_loopback_bind_warns_to_use_reverse_proxy() {
            use tracing_subscriber::layer::SubscriberExt as _;

            let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
            let cfg = HttpConfig::new(addr, "/mcp");

            let captured = CapturedMessages::default();
            let subscriber = tracing_subscriber::registry().with(captured.clone());
            let guard = tracing::subscriber::set_default(subscriber);

            // The warning fires synchronously right after bind, before
            // `axum::serve` starts running indefinitely, so a short timeout
            // is enough to observe it.
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                super::super::run_http(test_server(), cfg),
            )
            .await;

            drop(guard);

            let messages = captured.0.lock().unwrap().clone();
            assert!(
                messages.iter().any(|m| m.contains(
                    "place this endpoint behind a reverse proxy that enforces authentication"
                )),
                "expected reverse-proxy warning in captured tracing events, got: {messages:?}"
            );
        }
    }
}
