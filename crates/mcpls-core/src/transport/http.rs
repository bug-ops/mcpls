//! The streamable-HTTP transport runner.

use super::allowlist::{AllowedOrigin, effective_allowed_hosts};
use super::body::enforce_body_inactivity;
use super::config::{ConnectionLimit, HeaderReadTimeout, HttpConfig, WriteStallTimeout};
use super::connection_io::ConnectionIo;
use super::lease::attach_listen_lease;
use super::session_manager::{CappedSessionManager, enforce_session_cap, run_idle_reaper};
use super::shutdown::ShutdownSignal;

/// Run the MCP server over Streamable HTTP (MCP spec 2025-11-25).
///
/// Binds `cfg.bind`, mounts the MCP service at `cfg.path` (and `/`), and
/// serves until `Ctrl-C` or `SIGTERM` is received.
///
/// Each HTTP session receives its own `McplsServer` instance (see
/// [`crate::mcp::McplsServer::for_new_session`]). The shared `Arc<Translator>`
/// inside is the same across all sessions, so LSP state is still global per
/// process.
///
/// # Resource update notifications
///
/// `resources/updated` goes to each session's standalone GET (SSE) stream, and
/// only for the URIs that session itself subscribed to (`resources/subscribe`
/// must come from a session established via the `initialize` handshake).
/// rmcp caches the last 16 GET-stream events per session
/// (`SessionConfig::DEFAULT_CHANNEL_CAPACITY`) and replays them when a GET
/// opens, including when a dead primary GET is replaced, so a client may see
/// duplicates. A second GET opened while the first is still considered alive
/// is a shadow stream that receives no notifications; once the first ends, a
/// newly opened GET becomes the primary.
///
/// # Stream liveness
///
/// With [`StreamLiveness::Probe`](super::config::StreamLiveness::Probe) (the default), each session's standalone GET
/// stream is probed with an MCP `ping` request every
/// [`ProbeInterval::DEFAULT`](super::config::ProbeInterval::DEFAULT) and closed when the client does not answer by
/// sending the JSON-RPC response in a POST within [`ProbeDeadline::DEFAULT`](super::config::ProbeDeadline::DEFAULT). The probe
/// runs inside mcpls, so it needs no socket options and also catches a peer
/// that vanished behind a reverse proxy. Probe replies are consumed and never
/// forwarded to the MCP service. Closing ends only the GET stream, not the
/// session; the client's reconnect (with `Last-Event-ID`) resumes it. A client
/// that never answers server `ping` requests would be disconnected every
/// interval plus deadline, so [`StreamLiveness::Disabled`](super::config::StreamLiveness::Disabled) switches probing
/// off. Request-wise resumes are not probed.
///
/// Stateless `subscriptions/listen` streams cannot be probed (no session, and
/// a client cannot answer a server `ping`), so they get a lease instead
/// ([`ListenLease`](crate::transport::ListenLease), default 15 to 30 minutes with random jitter): the HTTP
/// body then ends abruptly, without a final result, which a client reads as a
/// transport close and answers by listening again. The replay on listen covers
/// the gap. A client that never re-listens stops receiving push updates after
/// one lease but can still read resources. The lease is off together with
/// [`StreamLiveness::Disabled`](super::config::StreamLiveness::Disabled) unless [`HttpConfig::with_listen_lease`] says
/// otherwise.
///
/// A session is closed, freeing its `max_concurrent_sessions` permit, once it
/// has had no inbound client activity and no open response stream (POST or
/// probed GET) for [`IdleTimeout::DEFAULT`](super::session_manager::IdleTimeout::DEFAULT) (5 minutes), swept every fifth of
/// that. This reaper is the only expiry owner: rmcp's own `keep_alive` timer is
/// switched off, so neither outbound `resources/updated` notifications (#521)
/// nor SSE pings affect expiry. The idle clock starts at the later of the last
/// inbound request and the moment the last stream closed. A client that closes
/// its connections cleanly is noticed on the next write, at most one SSE
/// keep-alive (15 s) later. A silently vanished
/// peer (half-open TCP: sleeping laptop, dropped NAT mapping) is detected by
/// the liveness probe above: its stream closes within one probe interval plus
/// deadline and the session then expires after the idle timeout. The probe
/// works on every platform and behind a reverse proxy. As a kernel-level
/// complement, on Linux and Android every accepted socket also gets
/// `TCP_USER_TIMEOUT` of `HALF_OPEN_TIMEOUT` (60 s), so unacknowledged data
/// (the 15 s SSE pings guarantee some) drops the connection about 75 s after
/// the peer vanishes (#552). macOS and Windows keep the kernel default, and
/// behind the recommended reverse proxy the accepted socket faces the proxy,
/// so the proxy's own timeouts govern that hop. A client that stops reading
/// with a full receive window for longer than the timeout may be dropped too.
/// A client that answers probes and keeps a GET stream open is never reaped
/// (#573). With [`StreamLiveness::Disabled`](super::config::StreamLiveness::Disabled) there is no proof of life, so an
/// open GET stream does not hold its session: the session expires
/// [`IdleTimeout::DEFAULT`](super::session_manager::IdleTimeout::DEFAULT) after the last inbound request even while the stream
/// receives notifications, and such clients must send a request (any `POST`,
/// for example a `ping`) more often than that. A POST or request-wise resume
/// response stream is cut [`ResponseStreamDeadline::DEFAULT`](super::config::ResponseStreamDeadline::DEFAULT) (1 hour) after it
/// opens, in both liveness modes, whatever it is still sending (a stream
/// lifetime bound sized for the longest legitimate tool call): the stream then
/// stops holding the session open and the reaper expires the session after the
/// idle timeout, so a vanished peer no longer pins its slot for good. While
/// hyper's write of a response to a peer that stopped reading is stuck, the
/// body is not polled and the cut cannot fire: `write_stall_timeout` closes
/// the connection and frees its `max_concurrent_connections` permit, and the
/// session slot is freed after the idle timeout. Clients should send `DELETE` on
/// shutdown; after an expiry they must re-initialize and re-subscribe.
///
/// On rmcp's stateless request path, "one instance per session" narrows to
/// "one instance per request"; `resources/subscribe`/`unsubscribe` detect
/// that path and return an explicit error rather than silently accepting a
/// subscription that would never be observed -- see
/// [`SubscriptionRegistry`](crate::mcp::SubscriptionRegistry)'s docs.
///
/// Per-session isolation assumes the `Mcp-Session-Id` stays secret: whoever
/// holds it can open that session's GET stream once its previous stream is
/// gone. mcpls performs no authentication of its own, so keep the id out of
/// logs and place a non-loopback bind behind an authenticating reverse proxy.
///
/// # Resource limits
///
/// POST bodies exceeding `cfg.max_request_body` are rejected with
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
/// `shutdown_signal` is constructed by [`crate::serve_with`] before any
/// startup work runs (see [`ShutdownSignal`]'s docs), so its registration
/// predates this function's own `TcpListener::bind` call — a signal between
/// bind and the graceful-shutdown future's first poll is still caught.
/// `shutdown_signal` is moved into (and dropped by) the
/// shutdown-signal future below once it resolves — i.e. as soon as
/// the *first* signal is received, well before this function returns. A
/// second, freshly constructed `ShutdownSignal` then covers the
/// connection-drain wait that follows (bounded by
/// [`HTTP_GRACEFUL_SHUTDOWN_TIMEOUT`]): a repeat signal caught there cuts the
/// drain short (dropping `serve` the same way the timeout branch already
/// does) instead of making the operator wait out the full timeout.
///
/// Cutting the drain short is *not* an immediate process exit: this function
/// still returns `Ok(())` normally, and its caller ([`crate::serve_with`])
/// proceeds straight into the ordinary post-transport shutdown sequence
/// ([`shutdown`](crate::runtime::shutdown::shutdown) — LSP server shutdown plus any pending background
/// init task, bounded by its own ~15s worst case). [`shutdown`](crate::runtime::shutdown::shutdown)'s own
/// registration (#329) takes over once *this* function returns, covering
/// that cleanup window and escalating to a forced `std::process::exit(1)` on
/// any *further* repeat signal — so an operator wanting a true immediate exit
/// needs a third signal, not a second. This is a deliberate choice, not an
/// oversight: calling `exit(1)` directly from this branch would skip
/// unwinding and cut short the graceful LSP `exit` delivery to still-running
/// servers, which is worse than requiring one more signal.
pub async fn run_http(
    mcp_server: crate::mcp::McplsServer,
    cfg: HttpConfig,
    shutdown_signal: ShutdownSignal,
) -> Result<(), crate::Error> {
    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .map_err(|source| crate::Error::HttpBind {
            addr: cfg.bind,
            source,
        })?;
    serve_http(listener, mcp_server, cfg, shutdown_signal).await
}

/// The `rmcp` service configuration for `cfg` served on `local_addr`.
///
/// `rmcp` accepts a request without `Origin`, so non-browser clients are
/// unaffected; a browser page on any origin outside the allowlist gets `403`.
fn rmcp_service_config(
    local_addr: std::net::SocketAddr,
    cfg: &HttpConfig,
    cancel: tokio_util::sync::CancellationToken,
) -> rmcp::transport::streamable_http_server::StreamableHttpServerConfig {
    use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;

    let allowed_origins = AllowedOrigin::loopback(local_addr.port())
        .into_iter()
        .chain(cfg.allowed_origins.iter().cloned())
        .map(|origin| origin.to_string());
    let allowed_hosts = effective_allowed_hosts(local_addr, &cfg.allowed_hosts)
        .into_iter()
        .map(|host| host.to_string());
    // `StreamableHttpServerConfig` is `#[non_exhaustive]`: construct via Default, then mutate.
    let mut http_cfg = StreamableHttpServerConfig::default()
        .with_allowed_hosts(allowed_hosts)
        .with_allowed_origins(allowed_origins)
        .enforce_origin_validation();
    http_cfg.cancellation_token = cancel;
    http_cfg.max_request_body_bytes = cfg.max_request_body.get();
    http_cfg
}

/// Serves the MCP HTTP transport on an already-bound `listener`.
///
/// Split out of [`run_http`] so callers (tests in particular) can bind the
/// listener themselves and know the exact address before serving starts.
/// `cfg.bind` is ignored; the listener's local address is authoritative.
/// Shutdown behavior is documented on [`run_http`].
// `session_manager` and `service` are moved into `app`, which is served until
// shutdown — clippy's drop-tightening heuristic misreads that as an
// early-droppable temporary because both types embed `tokio::sync` lock types
// (`CappedSessionManager`'s `Mutex`, `StreamableHttpService`'s `RwLock`s).
#[allow(clippy::significant_drop_tightening)]
pub async fn serve_http(
    listener: tokio::net::TcpListener,
    mcp_server: crate::mcp::McplsServer,
    cfg: HttpConfig,
    mut shutdown_signal: ShutdownSignal,
) -> Result<(), crate::Error> {
    use std::sync::Arc;

    use rmcp::transport::streamable_http_server::StreamableHttpService;
    use tokio_util::sync::CancellationToken;

    let session_manager = Arc::new(
        CappedSessionManager::new(cfg.max_concurrent_sessions, cfg.session_idle_timeout)
            .with_stream_liveness(cfg.stream_liveness)
            .with_response_stream_deadline(cfg.response_stream_deadline),
    );
    let reaper_manager = Arc::clone(&session_manager);
    let cancel = CancellationToken::new();

    // `mcp_server` is moved (not cloned): `McplsServer` is deliberately not
    // `Clone` (#478) so this is the only value the factory below can build
    // new sessions from, rather than a shared instance a caller could
    // accidentally hand to multiple sessions.
    let mcp_for_factory = mcp_server;
    let local_addr = listener.local_addr()?;
    let http_cfg = rmcp_service_config(local_addr, &cfg, cancel.clone());

    // `for_new_session`, not `.clone()`: every session must get its own
    // subscription state (#478) rather than sharing `mcp_for_factory`'s, while
    // still sharing its `Arc<Translator>` and the rest of the LSP-facing state
    // via a cheap `Arc` bump. On rmcp's stateless path this factory runs once
    // per request, not per session; those instances never subscribe, so they
    // never register with the `SubscriptionRegistry`.
    let service = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(mcp_for_factory.for_new_session()),
        session_manager,
        http_cfg,
    );

    let app = axum::Router::new()
        .nest_service(cfg.path.as_str(), service.clone())
        .route_service("/", service)
        .layer(axum::middleware::from_fn_with_state(
            cfg.effective_listen_lease(),
            attach_listen_lease,
        ))
        .layer(axum::middleware::from_fn(enforce_session_cap))
        .layer(axum::middleware::from_fn_with_state(
            cfg.header_read_timeout,
            enforce_body_inactivity,
        ));

    let reaper_cancel = cancel.child_token();
    // Stops the reaper on every return path below, not only on shutdown.
    let _reaper_guard = reaper_cancel.clone().drop_guard();
    tokio::spawn(run_idle_reaper(reaper_manager, reaper_cancel));

    tracing::info!(addr = %local_addr, path = %cfg.path, "MCP HTTP transport listening");
    if !local_addr.ip().is_loopback() {
        tracing::warn!(
            addr = %local_addr,
            "binding to a non-loopback address: mcpls performs no authentication of its own on \
             any transport — place this endpoint behind a reverse proxy that enforces \
             authentication. mcpls itself enforces only a header-read/idle timeout, a write-stall \
             timeout and a connection cap. The Host header must be localhost, 127.0.0.1, ::1, \
             the bound IP or one of the configured allowed hosts (a proxy can rewrite it), and \
             browser clients may use only loopback origins on the bound port or the \
             configured allowed origins"
        );
    }

    // `cancel` is cancelled exactly once, when the shutdown signal fires
    // (below). Cloned first so the force-timeout and repeat-signal branches
    // can each observe that same moment independently of the
    // shutdown-signal future, which consumes its own clone.
    let cancel_for_force_timeout = cancel.clone();
    let cancel_for_repeat_signal = cancel.clone();
    let cancel_for_serve = cancel.clone();
    let serve = async move {
        let signal = async move {
            shutdown_signal.recv().await;
            cancel.cancel();
        };
        tokio::join!(
            serve_http1(
                listener,
                app,
                cancel_for_serve,
                cfg.header_read_timeout,
                cfg.write_stall_timeout,
                cfg.max_concurrent_connections,
            ),
            signal
        );
    };

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
        () = serve => Ok(()),
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
        // #349: `shutdown_signal` above is consumed and dropped as soon as
        // the first signal arrives, leaving no listener for a repeat signal
        // during the connection-drain wait that follows. Wait for `cancel`
        // first and only then construct a fresh `ShutdownSignal` -- rather
        // than registering one up front, alongside `shutdown_signal` -- so
        // it starts with no pending signal of its own: `tokio::signal`
        // fans a delivered signal out to every live listener, so a listener
        // already registered before the first signal arrived would
        // independently observe that same signal and misreport it as a
        // repeat. This leaves a much smaller, accepted gap instead: between
        // `cancel.cancel()` firing and `ShutdownSignal::new()` actually
        // registering on the next scheduler hop, there is no pollable
        // listener at all, so a signal delivered in that sub-millisecond
        // window is lost. Registering up front would only trade this for
        // the coalescing problem above -- it is not fully closable either
        // way, and the window is far below human reaction time to a second
        // keypress.
        () = async move {
            cancel_for_repeat_signal.cancelled().await;
            let mut repeat_signal = ShutdownSignal::new();
            repeat_signal.recv().await;
        } => {
            tracing::warn!(
                "repeat shutdown signal received during HTTP connection drain, cutting drain short"
            );
            Ok(())
        }
    }
}

/// Accepts connections on `listener` and serves `app` over HTTP/1 until
/// `cancel` fires, then drains the open connections gracefully.
///
/// Replaces `axum::serve` because it never installs a hyper timer, which
/// leaves `header_read_timeout` inert: a client that opens a connection and
/// stalls would hold it (and, with the cap, a permit) forever. A permit is
/// taken before `accept` so a full house leaves new connections queued in
/// the kernel rather than accepted-and-idle.
///
/// Every stream goes through [`ConnectionIo`]: a write the peer drains slower
/// than the minimum rate over a `write_stall` window closes the connection,
/// and a clean close lingers to discard the unread request body.
pub(super) async fn serve_http1(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    cancel: tokio_util::sync::CancellationToken,
    header_read_timeout: HeaderReadTimeout,
    write_stall: WriteStallTimeout,
    max_connections: ConnectionLimit,
) {
    use std::sync::Arc;

    use hyper_util::rt::{TokioIo, TokioTimer};
    use hyper_util::service::TowerToHyperService;
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout.get());
    let permits = Arc::new(Semaphore::new(max_connections.get()));
    // A `JoinSet` aborts its tasks when dropped, so a force-timeout in
    // `run_http` cannot leave connections being served after it returns.
    let mut connections = JoinSet::new();

    loop {
        while connections.try_join_next().is_some() {}
        let permit = tokio::select! {
            () = cancel.cancelled() => break,
            permit = Arc::clone(&permits).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };
        let stream = tokio::select! {
            () = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(e) if is_connection_error(&e) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "HTTP accept failed, backing off");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            },
        };

        #[cfg(any(target_os = "linux", target_os = "android"))]
        set_half_open_timeout(&stream);

        let io = TokioIo::new(ConnectionIo::new(
            stream,
            write_stall,
            header_read_timeout,
            cancel.clone(),
        ));
        let conn = builder.serve_connection(io, TowerToHyperService::new(app.clone()));
        let cancel = cancel.clone();
        connections.spawn(async move {
            let _permit = permit;
            let mut conn = std::pin::pin!(conn);
            tokio::select! {
                result = conn.as_mut() => {
                    if let Err(e) = result {
                        tracing::trace!(error = %e, "HTTP connection ended with error");
                    }
                }
                () = cancel.cancelled() => {
                    conn.as_mut().graceful_shutdown();
                    if let Err(e) = conn.as_mut().await {
                        tracing::trace!(error = %e, "HTTP connection ended with error");
                    }
                }
            }
        });
    }

    while connections.join_next().await.is_some() {}
}

/// Whether an `accept` error is about the one peer's connection rather than
/// the listener, so the loop can retry at once instead of backing off.
fn is_connection_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}

/// `TCP_USER_TIMEOUT` applied to accepted HTTP sockets on Linux and Android.
///
/// Must exceed the 15 s SSE keep-alive interval so a healthy but quiet stream
/// is never dropped.
#[cfg(any(target_os = "linux", target_os = "android"))]
const HALF_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(1);

/// Bounds how long unacknowledged data may linger on `stream` before the
/// kernel drops the connection; failures are logged and the connection kept.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn set_half_open_timeout(stream: &tokio::net::TcpStream) {
    if let Err(e) = socket2::SockRef::from(stream).set_tcp_user_timeout(Some(HALF_OPEN_TIMEOUT)) {
        tracing::debug!(error = %e, "failed to set TCP_USER_TIMEOUT on accepted connection");
    }
}

/// Upper bound [`run_http`] waits, once shutdown has been signaled, for
/// in-flight connections to finish draining before giving up and returning
/// anyway.
pub(super) const HTTP_GRACEFUL_SHUTDOWN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A client reaching a `:80` bind by IP sends `Host` without a port,
    /// so the bound IP must reach `rmcp` portless.
    #[test]
    fn test_rmcp_config_allows_the_bound_ip_without_a_port() {
        let cfg = HttpConfig::new("192.168.1.5:80".parse().unwrap());

        let rmcp_cfg = rmcp_service_config(
            "192.168.1.5:80".parse().unwrap(),
            &cfg,
            tokio_util::sync::CancellationToken::new(),
        );

        assert!(rmcp_cfg.allowed_hosts.iter().any(|h| h == "192.168.1.5"));
        assert!(rmcp_cfg.allowed_hosts.iter().all(|h| h != "192.168.1.5:80"));
    }

    /// An accepted stream must read back the half-open `TCP_USER_TIMEOUT`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[tokio::test]
    async fn test_set_half_open_timeout_applies_tcp_user_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (accepted, _) = listener.accept().await.unwrap();

        super::set_half_open_timeout(&accepted);

        let timeout = socket2::SockRef::from(&accepted)
            .tcp_user_timeout()
            .unwrap();
        assert_eq!(timeout, Some(super::HALF_OPEN_TIMEOUT));
    }
}
