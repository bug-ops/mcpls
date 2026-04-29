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
/// ```rust,ignore
/// use mcpls_core::{Transport, serve_with, ServerConfig};
///
/// #[tokio::main]
/// async fn main() -> Result<(), mcpls_core::Error> {
///     let config = ServerConfig::load()?;
///     serve_with(config, Transport::Stdio).await
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
/// let cfg = HttpConfig {
///     bind: "127.0.0.1:3000".parse().unwrap(),
///     path: "/mcp".to_string(),
/// };
/// let transport = Transport::Http(cfg);
/// ```
#[cfg(feature = "transport-http")]
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// TCP address to bind (e.g. `127.0.0.1:3000`).
    pub bind: std::net::SocketAddr,
    /// URL path prefix the MCP service is mounted at (e.g. `"/mcp"`).
    pub path: String,
}

use rmcp::ServiceExt as _;

/// Run the MCP server over stdio.
///
/// Serves the given `mcp_server` using stdin/stdout and populates `peer_cell`
/// once the transport is established so that diagnostic pump tasks can begin
/// forwarding `resources/updated` notifications.
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

    service
        .waiting()
        .await
        .map(|_| ())
        .map_err(|e| crate::Error::McpServer(format!("MCP server error: {e}")))
}

/// Run the MCP server over Streamable HTTP (MCP spec 2025-11-25).
///
/// Binds `cfg.bind`, mounts the MCP service at `cfg.path` (and `/`), and
/// serves until `Ctrl-C` or `SIGTERM` is received.
///
/// Each HTTP session receives its own `McplsServer` clone. The shared
/// `Arc<Mutex<Translator>>` inside is the same across all sessions, so LSP
/// state is still global per process.
///
/// # Note
///
/// Diagnostic push notifications (`resources/updated`) are not forwarded to
/// HTTP sessions in this release — the single-peer pump architecture from
/// stdio is kept as-is. Clients can still poll diagnostics via the existing
/// MCP tools. A follow-up issue will add per-session broadcast.
#[cfg(feature = "transport-http")]
pub(crate) async fn run_http(
    mcp_server: crate::mcp::McplsServer,
    cfg: HttpConfig,
) -> Result<(), crate::Error> {
    use std::sync::Arc;

    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    use tokio_util::sync::CancellationToken;

    let session_manager = Arc::new(LocalSessionManager::default());
    let cancel = CancellationToken::new();

    let mcp_for_factory = mcp_server.clone();
    // StreamableHttpServerConfig is #[non_exhaustive]; construct via Default then mutate.
    let mut http_cfg = StreamableHttpServerConfig::default();
    http_cfg.cancellation_token = cancel.clone();

    let service = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(mcp_for_factory.clone()),
        session_manager,
        http_cfg,
    );

    let app = axum::Router::new()
        .nest_service(&cfg.path, service.clone())
        .route_service("/", service);

    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .map_err(|e| crate::Error::McpServer(format!("bind {}: {e}", cfg.bind)))?;

    tracing::info!(addr = %cfg.bind, path = %cfg.path, "MCP HTTP transport listening");
    if !cfg.bind.ip().is_loopback() {
        tracing::warn!(
            addr = %cfg.bind,
            "binding to a non-loopback address: rmcp Host validation allows only \
             localhost/127.0.0.1/::1 by default — use a reverse proxy for non-loopback deployments \
             and ensure no authentication is required"
        );
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            // On Unix, containers (Docker/systemd) send SIGTERM; handle both
            // SIGTERM and SIGINT (Ctrl-C) so shutdown is clean in all environments.
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut sigterm = signal(SignalKind::terminate())
                    .map_err(|e| crate::Error::McpServer(format!("SIGTERM handler: {e}")));
                match sigterm {
                    Ok(ref mut s) => {
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => {},
                            _ = s.recv() => {},
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
            cancel.cancel();
        })
        .await
        .map_err(|e| crate::Error::McpServer(format!("http serve: {e}")))
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

    #[cfg(feature = "transport-http")]
    mod http_tests {
        use std::net::SocketAddr;

        use super::super::{HttpConfig, Transport};

        #[test]
        fn test_http_config_fields() {
            let addr: SocketAddr = "127.0.0.1:3000".parse().unwrap();
            let cfg = HttpConfig {
                bind: addr,
                path: "/mcp".to_string(),
            };
            assert_eq!(cfg.bind, addr);
            assert_eq!(cfg.path, "/mcp");
        }

        #[test]
        fn test_http_config_clone() {
            let cfg = HttpConfig {
                bind: "127.0.0.1:3001".parse().unwrap(),
                path: "/test".to_string(),
            };
            let cloned = cfg.clone();
            assert_eq!(cloned.bind, cfg.bind);
            assert_eq!(cloned.path, cfg.path);
        }

        #[test]
        fn test_transport_http_variant() {
            let cfg = HttpConfig {
                bind: "127.0.0.1:3002".parse().unwrap(),
                path: "/mcp".to_string(),
            };
            let t = Transport::Http(cfg);
            assert!(matches!(t, Transport::Http(_)));
        }

        /// Verifies `run_http` binds successfully and accepts TCP connections.
        #[tokio::test]
        async fn test_run_http_binds() {
            use std::sync::Arc;

            use tokio::sync::Mutex;

            use crate::bridge::{ResourceSubscriptions, Translator};
            use crate::mcp::McplsServer;

            let translator = Arc::new(Mutex::new(Translator::new()));
            let subs = Arc::new(ResourceSubscriptions::new());
            let server = McplsServer::new(translator, subs);

            // Bind port 0 so the OS assigns a free port.
            let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = probe.local_addr().unwrap();
            drop(probe);

            let cfg = HttpConfig {
                bind: addr,
                path: "/mcp".to_string(),
            };

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

        /// Verifies `run_http` returns an error when the bind address is already in use.
        #[tokio::test]
        async fn test_run_http_bind_error() {
            use std::sync::Arc;

            use tokio::sync::Mutex;

            use crate::bridge::{ResourceSubscriptions, Translator};
            use crate::mcp::McplsServer;

            // Hold a listener to make the port unavailable.
            let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = occupied.local_addr().unwrap();

            let translator = Arc::new(Mutex::new(Translator::new()));
            let subs = Arc::new(ResourceSubscriptions::new());
            let server = McplsServer::new(translator, subs);

            let cfg = HttpConfig {
                bind: addr,
                path: "/mcp".to_string(),
            };

            let result = super::super::run_http(server, cfg).await;
            assert!(
                result.is_err(),
                "run_http should fail when port is occupied"
            );

            drop(occupied);
        }
    }
}
