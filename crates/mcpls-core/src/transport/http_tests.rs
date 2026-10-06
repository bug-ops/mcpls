//! End-to-end tests for the HTTP transport: each drives `run_http`,
//! `serve_http` or `serve_http1` over a real loopback socket.

use std::net::SocketAddr;

use super::config::{
    ConnectionLimit, HeaderReadTimeout, HttpConfig, ProbeDeadline, ProbeInterval, StreamLiveness,
};
use super::http::{HTTP_GRACEFUL_SHUTDOWN_TIMEOUT, serve_http, serve_http1};
use super::run_http;
use super::session_manager::{IdleTimeout, SESSION_CAP_MARKER, enforce_session_cap};
use super::test_support::{
    E2E_DEADLINE, TEST_IDLE, short_lease, test_server, test_server_with_roots,
};
use crate::bridge::WorkspaceRoots;
use crate::test_lsp::CapturedLogs;

/// Verifies `run_http` binds successfully and accepts TCP connections.
#[tokio::test]
async fn test_run_http_binds() {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use crate::bridge::{NotificationCache, Translator};
    use crate::config::McpConfig;
    use crate::mcp::{McplsServer, SubscriptionRegistry};

    let translator = Arc::new(Translator::new());
    let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
    let workspace_roots = WorkspaceRoots::default();
    let subs = SubscriptionRegistry::new();
    let server = McplsServer::new(
        translator,
        notification_cache,
        workspace_roots,
        subs,
        crate::ProjectConfigStatus::NotIgnored,
        McpConfig::default(),
    );

    // Bind port 0 so the OS assigns a free port.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);

    let cfg = HttpConfig::new(addr);

    let server_task = tokio::spawn(run_http(server, cfg, super::ShutdownSignal::new()));

    // A successful TCP connect proves the listener is up.
    assert!(
        connect_with_retry(addr).await.is_some(),
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
    let (_addr, server_task) = spawn_http_server(test_server(), |cfg| cfg).await;

    // Let the spawned task make initial progress (bind the
    // listener, enter its `select!`) without depending on any real
    // or virtual delay.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    // Fast-forward well past `HTTP_GRACEFUL_SHUTDOWN_TIMEOUT` with
    // no shutdown signal ever sent.
    tokio::time::advance(HTTP_GRACEFUL_SHUTDOWN_TIMEOUT + std::time::Duration::from_secs(5)).await;
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
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use crate::bridge::{NotificationCache, Translator};
    use crate::config::McpConfig;
    use crate::mcp::{McplsServer, SubscriptionRegistry};

    // Hold a listener to make the port unavailable.
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = occupied.local_addr().unwrap();

    let translator = Arc::new(Translator::new());
    let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
    let workspace_roots = WorkspaceRoots::default();
    let subs = SubscriptionRegistry::new();
    let server = McplsServer::new(
        translator,
        notification_cache,
        workspace_roots,
        subs,
        crate::ProjectConfigStatus::NotIgnored,
        McpConfig::default(),
    );

    let cfg = HttpConfig::new(addr);

    let result = run_http(server, cfg, super::ShutdownSignal::new()).await;
    assert!(
        matches!(
            &result,
            Err(crate::Error::HttpBind { addr: bound, source })
                if *bound == addr && source.kind() == std::io::ErrorKind::AddrInUse
        ),
        "run_http should fail with HttpBind(AddrInUse) when the port is occupied, got {result:?}"
    );

    drop(occupied);
}

/// Spawns `run_http` on a fresh loopback port.
async fn spawn_run_http(
    cfg_for: impl FnOnce(SocketAddr) -> HttpConfig,
) -> (
    SocketAddr,
    tokio::task::JoinHandle<Result<(), crate::Error>>,
) {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let task = tokio::spawn(run_http(
        test_server(),
        cfg_for(addr),
        super::ShutdownSignal::new(),
    ));
    let mut listening = false;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            listening = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(listening, "run_http never started listening on {addr}");
    (addr, task)
}

/// #465: a client that sends an incomplete request header and stalls
/// is disconnected once `header_read_timeout` elapses.
#[tokio::test]
async fn test_run_http_closes_connection_that_stalls_mid_header() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let timeout = std::time::Duration::from_millis(200);
    let (addr, server_task) = spawn_run_http(|addr| {
        HttpConfig::new(addr).with_header_read_timeout(HeaderReadTimeout::new(timeout).unwrap())
    })
    .await;

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();

    let mut sink = Vec::new();
    tokio::time::timeout(
        timeout + std::time::Duration::from_secs(1),
        stream.read_to_end(&mut sink),
    )
    .await
    .unwrap_or_else(|_| panic!("server must close a stalled connection within the header timeout"))
    .ok();

    server_task.abort();
}

/// #465: with `max_concurrent_connections = 1`, a second connection is
/// not served until the first closes.
#[tokio::test]
async fn test_run_http_connection_cap_queues_second_connection() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (addr, server_task) = spawn_run_http(|addr| {
        HttpConfig::new(addr).with_max_concurrent_connections(ConnectionLimit::new(1).unwrap())
    })
    .await;

    let first = tokio::net::TcpStream::connect(addr).await.unwrap();

    let mut second = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!("GET /nowhere HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    second.write_all(request.as_bytes()).await.unwrap();
    let mut buf = [0u8; 64];
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), second.read(&mut buf))
            .await
            .is_err(),
        "second connection must not be served while the cap is held"
    );

    drop(first);
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), second.read(&mut buf))
        .await
        .unwrap_or_else(|_| panic!("second connection must be served once the first closes"))
        .unwrap();
    assert!(n > 0);

    server_task.abort();
}

/// Security M1: a POST announcing a body it never sends is answered
/// `408` after the inactivity timeout and releases its connection
/// permit, so a one-connection cap still serves the next client.
#[tokio::test]
async fn test_run_http_answers_408_to_stalled_request_body_and_frees_the_permit() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let timeout = std::time::Duration::from_millis(200);
    let (addr, server_task) = spawn_run_http(|addr| {
        HttpConfig::new(addr)
            .with_header_read_timeout(HeaderReadTimeout::new(timeout).unwrap())
            .with_max_concurrent_connections(ConnectionLimit::new(1).unwrap())
    })
    .await;

    let mut stalled = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n"
    );
    stalled.write_all(head.as_bytes()).await.unwrap();

    let mut response = Vec::new();
    tokio::time::timeout(
        timeout + std::time::Duration::from_secs(2),
        stalled.read_to_end(&mut response),
    )
    .await
    .unwrap_or_else(|_| panic!("a stalled body must not pin the connection"))
    .ok();
    assert!(
        String::from_utf8_lossy(&response).starts_with("HTTP/1.1 408"),
        "got {:?}",
        String::from_utf8_lossy(&response)
    );

    let mut next = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!("GET /nowhere HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    next.write_all(request.as_bytes()).await.unwrap();
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), next.read(&mut buf))
        .await
        .unwrap_or_else(|_| panic!("the permit must be free after the 408"))
        .unwrap();
    assert!(n > 0);

    server_task.abort();
}

/// Starts `serve_http1` over an empty router (every path is a 404).
async fn spawn_serve_http1(
    limit: ConnectionLimit,
) -> (
    SocketAddr,
    tokio_util::sync::CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    spawn_serve_http1_with(
        axum::Router::new(),
        crate::WriteStallTimeout::DEFAULT,
        limit,
    )
    .await
}

/// Starts `serve_http1` over `app` with an explicit write-stall timeout.
async fn spawn_serve_http1_with(
    app: axum::Router,
    write_stall: crate::WriteStallTimeout,
    limit: ConnectionLimit,
) -> (
    SocketAddr,
    tokio_util::sync::CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(serve_http1(
        listener,
        app,
        cancel.clone(),
        HeaderReadTimeout::DEFAULT,
        write_stall,
        limit,
    ));
    (addr, cancel, task)
}

/// A response body that never ends.
struct EndlessBody;

impl http_body::Body for EndlessBody {
    type Data = axum::body::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::task::Poll::Ready(Some(Ok(http_body::Frame::data(
            axum::body::Bytes::from_static(&[b'x'; 16 * 1024]),
        ))))
    }
}

/// A peer that requests an endless response and never reads it loses
/// its connection after the write-stall timeout, so a one-connection
/// cap serves the next client.
#[tokio::test]
async fn test_serve_http1_write_stall_frees_the_permit_of_a_non_reading_peer() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let stall = std::time::Duration::from_millis(300);
    let app = axum::Router::new().route(
        "/stream",
        axum::routing::get(|| async { axum::body::Body::new(EndlessBody) }),
    );
    let (addr, cancel, task) = spawn_serve_http1_with(
        app,
        crate::WriteStallTimeout::new(stall).unwrap(),
        ConnectionLimit::new(1).unwrap(),
    )
    .await;

    let mut hog = tokio::net::TcpStream::connect(addr).await.unwrap();
    hog.write_all(format!("GET /stream HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let started = tokio::time::Instant::now();

    let mut next = tokio::net::TcpStream::connect(addr).await.unwrap();
    next.write_all(
        format!("GET /nowhere HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(
        stall + std::time::Duration::from_secs(5),
        next.read(&mut buf),
    )
    .await
    .unwrap_or_else(|_| panic!("the stalled connection must release its permit"))
    .unwrap();

    assert!(n > 0);
    assert!(
        started.elapsed() >= stall,
        "the permit was freed before the stall deadline"
    );
    drop((hog, next));
    cancel.cancel();
    task.await.unwrap();
}

/// Cancelling drains an idle keep-alive connection instead of
/// waiting on it, and closes it.
#[tokio::test]
async fn test_serve_http1_cancel_closes_idle_keep_alive_connection_and_returns() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (addr, cancel, task) = spawn_serve_http1(ConnectionLimit::DEFAULT).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!("GET /nowhere HTTP/1.1\r\nHost: {addr}\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = [0u8; 512];
    assert!(stream.read(&mut buf).await.unwrap() > 0);

    cancel.cancel();

    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap_or_else(|_| panic!("serve_http1 must return after cancel"))
        .unwrap();
    assert_eq!(stream.read(&mut buf).await.unwrap(), 0);
}

/// A cancel arriving while every permit is taken and another client
/// waits in the accept queue must still end the loop.
#[tokio::test]
async fn test_serve_http1_cancel_while_at_the_connection_cap_returns() {
    let (addr, cancel, task) = spawn_serve_http1(ConnectionLimit::new(1).unwrap()).await;
    let _held = tokio::net::TcpStream::connect(addr).await.unwrap();
    let _queued = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    cancel.cancel();

    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap_or_else(|_| panic!("serve_http1 must return after cancel at the cap"))
        .unwrap();
}

/// Polls a TCP connect until it succeeds or the 5s budget runs out;
/// for tests that go through `run_http`, which binds internally.
async fn connect_with_retry(addr: SocketAddr) -> Option<tokio::net::TcpStream> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(stream) = tokio::net::TcpStream::connect(addr).await {
            return Some(stream);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Binds an ephemeral loopback listener and serves `server` on it via
/// `serve_http`. The listener is bound before this returns, so the
/// kernel queues connections immediately and no readiness wait is needed.
async fn spawn_http_server(
    server: crate::mcp::McplsServer,
    configure: impl FnOnce(HttpConfig) -> HttpConfig,
) -> (
    SocketAddr,
    tokio::task::JoinHandle<Result<(), crate::Error>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = configure(HttpConfig::new(addr));
    let task = tokio::spawn(serve_http(
        listener,
        server,
        cfg,
        super::ShutdownSignal::new(),
    ));
    (addr, task)
}

/// Sends a raw HTTP/1.1 POST request over TCP and returns the raw response
/// text (status line, headers, and body). Used because neither `reqwest`
/// nor `tower`/`http-body-util` are available as dev-dependencies here.
async fn raw_http_post(addr: SocketAddr, path: &str, extra_headers: &str, body: &[u8]) -> String {
    raw_http_post_as(addr, &addr.to_string(), path, extra_headers, body).await
}

/// [`raw_http_post`] with an explicit `Host` header value.
async fn raw_http_post_as(
    addr: SocketAddr,
    host: &str,
    path: &str,
    extra_headers: &str,
    body: &[u8],
) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n{extra_headers}Content-Length: {}\r\n\r\n",
        body.len()
    );
    // One write, so a server that answers from the headers alone does
    // not close on an unread body and reset the connection.
    let wire = [request.as_bytes(), body].concat();
    stream.write_all(&wire).await.unwrap();

    let mut response = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => response.extend_from_slice(&buf[..n]),
            // Server closes after the early 403 with the body unread, which surfaces as RST.
            Ok(Err(e))
                if e.kind() == std::io::ErrorKind::ConnectionReset && !response.is_empty() =>
            {
                break;
            }
            Ok(Err(e)) => panic!("read error: {e}"),
        }
    }
    String::from_utf8_lossy_owned(response)
}

/// A POST body exceeding `cfg.max_request_body` must be rejected
/// with `413 Payload Too Large`, proving the config value reaches
/// `StreamableHttpServerConfig::max_request_body_bytes`.
#[tokio::test]
async fn test_run_http_rejects_oversized_body_with_413() {
    let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
        cfg.with_max_request_body(crate::RequestBodyLimit::new(64).unwrap())
    })
    .await;

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

/// A POST body within `cfg.max_request_body` must not be rejected
/// for size — it reaches JSON deserialization instead (the body here is
/// intentionally not valid JSON-RPC, so a non-413 error distinguishes
/// "passed the size check" from "was a valid request").
#[tokio::test]
async fn test_run_http_accepts_body_within_limit() {
    let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
        cfg.with_max_request_body(crate::RequestBodyLimit::new(64).unwrap())
    })
    .await;

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

/// #556: a browser request from a foreign origin is rejected with `403`
/// on POST and GET; loopback origins on the bound port and requests
/// without `Origin` still work.
#[tokio::test]
async fn test_run_http_enforces_loopback_origin() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (addr, server_task) = spawn_http_server(test_server(), |cfg| cfg).await;
    let initialize_body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
    let post = |origin: Option<String>| async move {
        let origin_header = origin.map_or_else(String::new, |o| format!("Origin: {o}\r\n"));
        raw_http_post(
            addr,
            "/mcp",
            &format!(
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n{origin_header}"
            ),
            initialize_body,
        )
        .await
    };

    for rejected in [
        "http://evil.example",
        "null",
        "http://127.0.0.1:1",
        "http://localhost:1",
    ] {
        let response = post(Some(rejected.to_owned())).await;
        assert!(
            response.starts_with("HTTP/1.1 403"),
            "Origin {rejected} should be rejected, got: {response}"
        );
    }
    for accepted in [
        Some(format!("http://127.0.0.1:{}", addr.port())),
        Some(format!("http://localhost:{}", addr.port())),
        Some(format!("http://[::1]:{}", addr.port())),
        None,
    ] {
        let response = post(accepted.clone()).await;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "Origin {accepted:?} should be accepted, got: {response}"
        );
    }

    for method in ["GET", "DELETE"] {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "{method} /mcp HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: text/event-stream\r\nOrigin: http://evil.example\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(
            response.starts_with("HTTP/1.1 403"),
            "{method} with a foreign Origin should be rejected, got: {response}"
        );
    }

    server_task.abort();
}

/// #584: every configured origin is matched by rmcp's own parser, which
/// silently drops entries it cannot read, against the `Origin` a
/// browser would send; unlisted origins stay rejected.
#[tokio::test]
async fn test_run_http_accepts_configured_allowed_origins() {
    let configured = [
        "https://app.example.com",
        "http://[::1]:8080",
        "http://[2001:db8::1]:8080",
        "HTTPS://Cased.Example.com:8443",
        "http://example.org:9000",
    ];
    let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
        cfg.with_allowed_origins(
            configured
                .iter()
                .map(|origin| origin.parse::<crate::AllowedOrigin>().unwrap()),
        )
    })
    .await;
    let initialize_body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
    let post = |origin: &'static str| async move {
        raw_http_post(
            addr,
            "/mcp",
            &format!(
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nOrigin: {origin}\r\n"
            ),
            initialize_body,
        )
        .await
    };

    for accepted in [
        "https://app.example.com",
        "https://app.example.com:443",
        "http://[::1]:8080",
        "http://[2001:db8::1]:8080",
        "https://cased.example.com:8443",
        "http://example.org:9000",
    ] {
        let response = post(accepted).await;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "Origin {accepted} should be accepted, got: {response}"
        );
    }
    for rejected in [
        "http://app.example.com",
        "https://app.example.com:444",
        "http://example.org:9001",
        "http://evil.example",
    ] {
        let response = post(rejected).await;
        assert!(
            response.starts_with("HTTP/1.1 403"),
            "Origin {rejected} should be rejected, got: {response}"
        );
    }

    server_task.abort();
}

/// #597: a `Host` outside the loopback names is rejected with `403`
/// unless it is configured; a host without a port matches any port and
/// one with a port matches only that port.
#[tokio::test]
async fn test_run_http_enforces_configured_allowed_hosts() {
    let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
        cfg.with_allowed_hosts(
            ["evil.example", "Pinned.example:8443"]
                .map(|host| host.parse::<crate::AllowedHost>().unwrap()),
        )
    })
    .await;
    let initialize_body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
    let post = |host: String| async move {
        raw_http_post_as(
            addr,
            &host,
            "/mcp",
            "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n",
            initialize_body,
        )
        .await
    };

    for accepted in [
        format!("localhost:{}", addr.port()),
        format!("127.0.0.1:{}", addr.port()),
        "localhost".to_owned(),
        "evil.example".to_owned(),
        "evil.example:80".to_owned(),
        "EVIL.example:1234".to_owned(),
        "pinned.example:8443".to_owned(),
    ] {
        let response = post(accepted.clone()).await;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "Host {accepted} should be accepted, got: {response}"
        );
    }
    for rejected in [
        "other.example",
        "evil.example.attacker.test",
        "pinned.example:1",
        "pinned.example",
    ] {
        let response = post(rejected.to_owned()).await;
        assert!(
            response.starts_with("HTTP/1.1 403"),
            "Host {rejected} should be rejected, got: {response}"
        );
    }

    server_task.abort();
}

/// Reads to EOF or error, returning what arrived and the error kind.
async fn read_until_close(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
    limit: std::time::Duration,
) -> (String, Option<std::io::ErrorKind>) {
    use tokio::io::AsyncReadExt as _;

    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + limit;
    let error = loop {
        match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
            Ok(Ok(0)) => break None,
            Ok(Ok(n)) => response.extend_from_slice(&buf[..n]),
            Ok(Err(e)) => break Some(e.kind()),
            Err(elapsed) => panic!("the connection stayed open past {limit:?}: {elapsed}"),
        }
    };
    (String::from_utf8_lossy_owned(response), error)
}

/// POSTs `extra_headers` with a request body of `body_len` bytes (all of
/// it, or never-ending when `None`) streamed while the response is read.
async fn post_streamed_body(
    addr: SocketAddr,
    extra_headers: &str,
    body_len: Option<usize>,
    read_delay: std::time::Duration,
    limit: std::time::Duration,
) -> (
    String,
    Option<std::io::ErrorKind>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::AsyncWriteExt as _;

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut reader, mut writer) = stream.into_split();
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\n{extra_headers}Content-Length: {}\r\n\r\n",
        body_len.unwrap_or(1 << 40)
    );
    let sender = tokio::spawn(async move {
        if writer.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let chunk = [b'a'; 16 * 1024];
        let mut remaining = body_len;
        loop {
            let len = remaining.map_or(chunk.len(), |r| r.min(chunk.len()));
            if len == 0 || writer.write_all(&chunk[..len]).await.is_err() {
                return;
            }
            remaining = remaining.map(|r| r - len);
        }
    });
    tokio::time::sleep(read_delay).await;
    let (response, error) = read_until_close(&mut reader, limit).await;
    (response, error, sender)
}

const JSON_POST_HEADERS: &str =
    "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n";

/// #602: a `403` sent while a large request body is unread arrives
/// intact and the connection then ends cleanly, not with a reset.
#[tokio::test]
async fn test_early_403_with_a_large_unread_body_delivers_the_status() {
    let (addr, server_task) = spawn_http_server(test_server(), |cfg| cfg).await;
    let headers = format!("{JSON_POST_HEADERS}Origin: http://evil.example\r\n");

    let (response, error, sender) = post_streamed_body(
        addr,
        &headers,
        Some(512 << 10),
        std::time::Duration::from_millis(300),
        std::time::Duration::from_secs(10),
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 403"), "got: {response}");
    assert_eq!(error, None);
    sender.abort();
    server_task.abort();
}

/// #602: a `413` for a body over the limit also arrives intact.
#[tokio::test]
async fn test_early_413_with_a_large_unread_body_delivers_the_status() {
    let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
        cfg.with_max_request_body(crate::RequestBodyLimit::new(64).unwrap())
    })
    .await;

    let (response, error, sender) = post_streamed_body(
        addr,
        JSON_POST_HEADERS,
        Some(512 << 10),
        std::time::Duration::from_millis(300),
        std::time::Duration::from_secs(10),
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 413"), "got: {response}");
    assert_eq!(error, None);
    sender.abort();
    server_task.abort();
}

/// #602: a peer that keeps sending after an early `403` is cut once the
/// linger budget (`header_read_timeout` here) is spent, which frees
/// the connection permit.
#[tokio::test]
async fn test_early_403_with_an_endless_body_frees_the_permit_within_the_linger_cap() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
        cfg.with_header_read_timeout(
            HeaderReadTimeout::new(std::time::Duration::from_millis(500)).unwrap(),
        )
        .with_max_concurrent_connections(ConnectionLimit::new(1).unwrap())
    })
    .await;
    let headers = format!("{JSON_POST_HEADERS}Origin: http://evil.example\r\n");

    let (response, _, sender) = post_streamed_body(
        addr,
        &headers,
        None,
        std::time::Duration::ZERO,
        std::time::Duration::from_secs(10),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 403"), "got: {response}");

    let mut next = tokio::net::TcpStream::connect(addr).await.unwrap();
    next.write_all(
        format!("GET /nowhere HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), next.read(&mut buf))
        .await
        .unwrap_or_else(|_| panic!("the lingering connection must release its permit"))
        .unwrap();

    assert!(n > 0);
    sender.abort();
    server_task.abort();
}

/// End-to-end: with `max_concurrent_sessions(1)`, a second concurrent
/// `initialize` handshake over `run_http` must be rejected with `429`
/// once the first session is established.
#[tokio::test]
async fn test_run_http_rejects_new_session_at_capacity_with_429() {
    let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
        cfg.with_max_concurrent_sessions(crate::SessionLimit::new(1).unwrap())
    })
    .await;

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

/// `GHSA-9pj6-vhgr-3mwh` regression guard: well-formed non-`initialize`
/// POSTs without a session id must not allocate or leak a session permit,
/// so a legitimate `initialize` still succeeds on a server capped at one.
#[tokio::test]
async fn test_run_http_invalid_non_initialize_posts_do_not_leak_session_permits() {
    let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
        cfg.with_max_concurrent_sessions(crate::SessionLimit::new(1).unwrap())
    })
    .await;

    let accept_headers =
        "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n";
    let non_initialize = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    for _ in 0..3 {
        let response = raw_http_post(addr, "/mcp", accept_headers, non_initialize).await;
        assert!(
            !response.starts_with("HTTP/1.1 2") && !response.starts_with("HTTP/1.1 429"),
            "a sessionless non-initialize POST must be rejected without touching the cap, got: {response}"
        );
    }

    let initialize_body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
    let initialize = raw_http_post(addr, "/mcp", accept_headers, initialize_body).await;
    assert!(
        initialize.starts_with("HTTP/1.1 200"),
        "initialize must still succeed after rejected POSTs, got: {initialize}"
    );

    server_task.abort();
}

/// rmcp 3.5.1 keeps handler-generated `-32602` errors in-band: an unknown
/// tool answers HTTP 200 with the JSON-RPC error in the body, not HTTP 400.
#[tokio::test]
async fn test_run_http_unknown_tool_is_in_band_invalid_params_with_http_200() {
    let (addr, server_task) = spawn_http_server(test_server(), |cfg| cfg).await;
    let session_id = initialize_legacy_session(addr).await;
    let initialized = post_in_session(
        addr,
        &session_id,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert!(
        initialized.starts_with("HTTP/1.1 202"),
        "got: {initialized}"
    );

    let response = post_in_session(
        addr,
        &session_id,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "unknown tool must not map to an HTTP error, got: {response}"
    );
    assert!(
        response.contains("-32602"),
        "the in-band error must carry code -32602, got: {response}"
    );

    server_task.abort();
}

/// S1 non-regression: a non-`initialize` request carrying SEP-2575
/// per-request `_meta` protocol-version metadata
/// (`io.modelcontextprotocol/protocolVersion` = `2026-07-28` plus the
/// required `clientCapabilities` key) takes rmcp's stateless
/// discover-lifecycle path and never calls
/// `SessionManager::create_session` — `rmcp` serves it directly
/// without touching the session table — so it must not be rejected
/// by the cap even while `max_concurrent_sessions` legacy sessions
/// are already active. (An `initialize` request is always
/// classified legacy regardless of the protocol version it
/// names, so it cannot be used to probe the stateless path.) This
/// guards against a future refactor reintroducing request-header
/// sniffing for the cap decision (the bug this design replaced).
#[tokio::test]
async fn test_run_http_stateless_request_bypasses_session_cap() {
    let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
        cfg.with_max_concurrent_sessions(crate::SessionLimit::new(1).unwrap())
    })
    .await;

    let accept_headers =
        "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n";

    // Fill the sole legacy-session slot.
    let legacy_initialize = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
    let legacy = raw_http_post(addr, "/mcp", accept_headers, legacy_initialize).await;
    assert!(
        legacy.starts_with("HTTP/1.1 200"),
        "legacy initialize should succeed and consume the sole session slot, got: {legacy}"
    );

    // A non-`initialize` request carrying per-request `_meta`
    // protocol-version metadata takes rmcp's stateless
    // discover-lifecycle path and never creates a session, so it
    // must bypass the cap entirely even though the slot above is
    // still held. The `MCP-Protocol-Version` header must match the
    // `_meta` value once the latter is present, and declaring
    // `2026-07-28` also brings in SEP-2243's `Mcp-Method` header
    // requirement.
    let stateless_headers = "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: resources/list\r\n";
    let stateless_request = br#"{"jsonrpc":"2.0","id":2,"method":"resources/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    let stateless = raw_http_post(addr, "/mcp", stateless_headers, stateless_request).await;
    assert!(
        stateless.starts_with("HTTP/1.1 200"),
        "stateless requests must bypass the session cap entirely, got: {stateless}"
    );

    server_task.abort();
}

/// #478/#482/#492 regression pin: on rmcp's stateless path (see
/// `test_run_http_stateless_request_bypasses_session_cap` above), the
/// service factory -- and therefore `McplsServer::for_new_session` --
/// runs once per *request*. Such an instance never subscribes, so it
/// must never enter `SubscriptionRegistry` at all, however many
/// requests are served.
#[tokio::test]
async fn test_stateless_requests_never_register_with_subscription_registry() {
    const REQUEST_COUNT: u32 = 20;

    let server = test_server();
    let registry = server.subscription_registry();

    let (addr, server_task) = spawn_http_server(server, |cfg| cfg).await;

    let stateless_headers = "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: resources/list\r\n";

    for id in 0..REQUEST_COUNT {
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"resources/list","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#
        );
        let response = raw_http_post(addr, "/mcp", stateless_headers, body.as_bytes()).await;
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "stateless request {id} should succeed, got: {response}"
        );
    }

    assert_eq!(
        registry.raw_len(),
        0,
        "stateless instances must never register with the subscription registry"
    );

    server_task.abort();
}

/// Shared setup for the `#482` regression tests below: a real
/// `run_http` server with one file inside its sole workspace root, so
/// `resources/subscribe` requests validate and reach the handler.
struct SubscribeTestServer {
    addr: SocketAddr,
    registry: crate::mcp::SubscriptionRegistry,
    uri: String,
    server_task: tokio::task::JoinHandle<Result<(), crate::Error>>,
    // Held so the file `subscribe` canonicalizes stays on disk.
    _workspace: tempfile::TempDir,
}

async fn spawn_subscribe_test_server() -> SubscribeTestServer {
    let workspace = tempfile::TempDir::new().unwrap();
    let file_path = workspace.path().join("main.rs");
    std::fs::write(&file_path, "fn main() {}").unwrap();
    let uri = crate::bridge::resources::make_uri(&file_path).unwrap();

    let server = test_server_with_roots(
        WorkspaceRoots::from_paths(&[workspace.path().to_path_buf()]).unwrap(),
    );
    let registry = server.subscription_registry();

    let (addr, server_task) = spawn_http_server(server, |cfg| cfg).await;

    SubscribeTestServer {
        addr,
        registry,
        uri,
        server_task,
        _workspace: workspace,
    }
}

/// #482: `_meta` negotiating `2026-07-28` per request is stateless by
/// both rmcp and mcpls' reckoning, so rmcp itself answers
/// `-32601 method not found` before dispatch.
#[tokio::test]
async fn test_stateless_subscribe_negotiated_per_request_is_rejected_by_rmcp() {
    let srv = spawn_subscribe_test_server().await;
    let uri = &srv.uri;

    let headers = format!(
        "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: resources/subscribe\r\nMcp-Name: {uri}\r\n"
    );
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"resources/subscribe","params":{{"uri":"{uri}","_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#
    );
    let response = raw_http_post(srv.addr, "/mcp", &headers, body.as_bytes()).await;
    assert!(
        response.contains("-32601"),
        "expected rmcp to refuse dispatching a 2026-07-28-negotiated resources/subscribe \
         (method not found), got: {response}"
    );

    srv.server_task.abort();
}

/// #482 regression matrix: `_meta` naming a pre-`2026-07-28` version
/// (see `request_uses_discover_lifecycle_meta`'s docs
/// for why rmcp still serves this statelessly) must be rejected by
/// mcpls' own guard for both `subscribe` and `unsubscribe`, and a
/// fabricated `Mcp-Session-Id` must not bypass it.
#[tokio::test]
async fn test_stateless_lifecycle_mismatch_is_rejected_by_mcpls() {
    let srv = spawn_subscribe_test_server().await;
    let uri = &srv.uri;

    for (method, extra_header) in [
        ("resources/subscribe", ""),
        (
            "resources/subscribe",
            "Mcp-Session-Id: not-a-real-session\r\n",
        ),
        ("resources/unsubscribe", ""),
    ] {
        let headers = format!(
            "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2025-06-18\r\n{extra_header}"
        );
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{{"uri":"{uri}","_meta":{{"io.modelcontextprotocol/protocolVersion":"2025-06-18","io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#
        );
        let response = raw_http_post(srv.addr, "/mcp", &headers, body.as_bytes()).await;
        assert!(
            response.contains("-32052") && !response.contains(r#""result":{}"#),
            "{method} (extra header: {extra_header:?}) must be rejected by mcpls' \
             stateless-subscription guard, not silently succeed, got: {response}"
        );
    }
    assert_eq!(
        srv.registry.raw_len(),
        0,
        "a rejected stateless subscribe must never register a session"
    );

    srv.server_task.abort();
}

/// #482 non-regression: a legacy session's `resources/subscribe`,
/// sent with the session's assigned `Mcp-Session-Id` echoed back and
/// no per-request `_meta`, must not be rejected by either guard above.
#[tokio::test]
async fn test_legacy_session_subscribe_is_not_rejected_as_stateless() {
    let srv = spawn_subscribe_test_server().await;
    let uri = &srv.uri;
    let session_id = initialize_legacy_session(srv.addr).await;

    let session_headers = format!(
        "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMcp-Session-Id: {session_id}\r\n"
    );
    let subscribe_body = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"resources/subscribe","params":{{"uri":"{uri}"}}}}"#
    );
    let session_response = raw_http_post(
        srv.addr,
        "/mcp",
        &session_headers,
        subscribe_body.as_bytes(),
    )
    .await;
    assert!(
        !session_response.contains("-32601") && !session_response.contains("-32052"),
        "a legacy session's subscribe must not be rejected as stateless, got: \
         {session_response}"
    );

    srv.server_task.abort();
}

/// #482: a live session's `resources/subscribe` that also carries
/// per-request discover-lifecycle `_meta` is rejected too -- rmcp
/// serves it statelessly regardless of the session id, so this is
/// intentional, not a regression.
#[tokio::test]
async fn test_legacy_session_subscribe_with_discover_meta_is_rejected_as_stateless() {
    let srv = spawn_subscribe_test_server().await;
    let uri = &srv.uri;
    let session_id = initialize_legacy_session(srv.addr).await;

    let headers = format!(
        "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMcp-Session-Id: {session_id}\r\nMCP-Protocol-Version: 2025-06-18\r\n"
    );
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"resources/subscribe","params":{{"uri":"{uri}","_meta":{{"io.modelcontextprotocol/protocolVersion":"2025-06-18","io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#
    );
    let response = raw_http_post(srv.addr, "/mcp", &headers, body.as_bytes()).await;
    assert!(
        response.contains("-32052"),
        "a live session's subscribe with per-request discover _meta must still be \
         rejected -- rmcp serves it statelessly regardless of the session id, got: \
         {response}"
    );

    srv.server_task.abort();
}

/// Performs the `initialize` handshake for a legacy HTTP session and
/// returns its assigned `Mcp-Session-Id`.
async fn initialize_legacy_session(addr: SocketAddr) -> String {
    let accept_headers =
        "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n";
    let initialize_body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
    let init_response = raw_http_post(addr, "/mcp", accept_headers, initialize_body).await;
    assert!(
        init_response.starts_with("HTTP/1.1 200"),
        "legacy initialize should succeed, got: {init_response}"
    );
    init_response
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("mcp-session-id")
                .then(|| value.trim().to_string())
        })
        .unwrap()
}

/// A raw-TCP Server-Sent-Events GET stream for one MCP session.
struct SseStream {
    stream: tokio::net::TcpStream,
    buf: String,
}

impl SseStream {
    /// Opens the session's standalone GET stream and returns once the
    /// `200` response headers have arrived, so nothing published
    /// afterwards can race the stream's registration.
    async fn open(addr: SocketAddr, session_id: &str) -> Self {
        let request = format!(
            "GET /mcp HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\nMcp-Session-Id: {session_id}\r\n\r\n"
        );
        Self::send(addr, request.as_bytes()).await
    }

    /// Opens a stateless 2026-07-28 `subscriptions/listen` stream.
    async fn open_listen(addr: SocketAddr, notifications: &serde_json::Value) -> Self {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "subscriptions/listen",
            "params": {
                "notifications": notifications,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            },
        })
        .to_string();
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: subscriptions/listen\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        Self::send(addr, request.as_bytes()).await
    }

    async fn send(addr: SocketAddr, request: &[u8]) -> Self {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(request).await.unwrap();

        let mut head = String::new();
        let mut chunk = [0u8; 4096];
        let body_start = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0, "GET stream closed before headers arrived");
                head.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if let Some(pos) = head.find("\r\n\r\n") {
                    return pos + 4;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("GET stream headers not received within 5 s"));
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "GET stream must open with 200, got: {head}"
        );
        Self {
            stream,
            buf: head[body_start..].to_owned(),
        }
    }

    /// Next JSON-RPC message on the stream, skipping `retry:` priming
    /// events, keep-alive comments and chunked-encoding framing.
    async fn next_message(&mut self) -> serde_json::Value {
        use tokio::io::AsyncReadExt as _;

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                while let Some(pos) = self.buf.find('\n') {
                    let line: String = self.buf.drain(..=pos).collect();
                    let Some(data) = line.trim().strip_prefix("data:") else {
                        continue;
                    };
                    if let Ok(message) = serde_json::from_str::<serde_json::Value>(data.trim()) {
                        return message;
                    }
                }
                let mut chunk = [0u8; 4096];
                let n = self.stream.read(&mut chunk).await.unwrap();
                assert!(n > 0, "stream closed before the expected message");
                self.buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no message on the stream within 5 s"))
    }

    /// Next liveness `ping` request on the stream.
    async fn next_ping(&mut self) -> serde_json::Value {
        loop {
            let message = self.next_message().await;
            if message["method"] == "ping" {
                return message;
            }
        }
    }

    /// Whether the server ends the stream within `limit`.
    async fn ends_within(&mut self, limit: std::time::Duration) -> bool {
        use tokio::io::AsyncReadExt as _;

        tokio::time::timeout(limit, async {
            loop {
                if self.buf.contains("0\r\n\r\n") {
                    return;
                }
                let mut chunk = [0u8; 4096];
                let n = self.stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    return;
                }
                self.buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
            }
        })
        .await
        .is_ok()
    }

    /// Next `notifications/resources/updated` message.
    async fn next_resource_update_message(&mut self) -> serde_json::Value {
        loop {
            let message = self.next_message().await;
            if message["method"] == "notifications/resources/updated" {
                return message;
            }
        }
    }

    /// Next `notifications/resources/updated` URI on the stream.
    async fn next_resource_update(&mut self) -> String {
        self.next_resource_update_message().await["params"]["uri"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

/// POSTs `body` within `session_id` and returns the raw response.
async fn post_in_session(addr: SocketAddr, session_id: &str, body: &str) -> String {
    let headers = format!(
        "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMcp-Session-Id: {session_id}\r\n"
    );
    raw_http_post(addr, "/mcp", &headers, body.as_bytes()).await
}

/// Completes the handshake for a fresh legacy session and opens its
/// GET stream.
async fn establish_session(addr: SocketAddr) -> (String, SseStream) {
    let session_id = initialize_legacy_session(addr).await;
    let initialized = post_in_session(
        addr,
        &session_id,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert!(
        initialized.starts_with("HTTP/1.1 202"),
        "notifications/initialized must be accepted, got: {initialized}"
    );
    let stream = SseStream::open(addr, &session_id).await;
    (session_id, stream)
}

async fn subscribe_in_session(addr: SocketAddr, session_id: &str, uri: &str) {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"resources/subscribe","params":{{"uri":"{uri}"}}}}"#
    );
    let response = post_in_session(addr, session_id, &body).await;
    assert!(
        response.starts_with("HTTP/1.1 200") && !response.contains(r#""error""#),
        "subscribe to {uri} must succeed, got: {response}"
    );
}

/// #468 end to end over HTTP: each session's GET stream carries
/// `resources/updated` only for that session's own subscriptions.
/// A subscribes to X and Y, B only to Y; X is published first, so B's
/// first update being Y proves X never reached B.
#[tokio::test]
async fn test_http_sessions_receive_updates_only_for_own_subscriptions() {
    let workspace = tempfile::TempDir::new().unwrap();
    let root = dunce::canonicalize(workspace.path()).unwrap();
    let file_x = root.join("x.rs");
    let file_y = root.join("y.rs");
    std::fs::write(&file_x, "fn x() {}").unwrap();
    std::fs::write(&file_y, "fn y() {}").unwrap();
    let uri_x = crate::bridge::resources::make_uri(&file_x).unwrap();
    let uri_y = crate::bridge::resources::make_uri(&file_y).unwrap();

    let server =
        test_server_with_roots(WorkspaceRoots::from_paths(std::slice::from_ref(&root)).unwrap());
    let registry = server.subscription_registry();
    let (addr, server_task) = spawn_http_server(server, |cfg| cfg).await;

    let (session_a, mut stream_a) = establish_session(addr).await;
    let (session_b, mut stream_b) = establish_session(addr).await;
    subscribe_in_session(addr, &session_a, &uri_x).await;
    subscribe_in_session(addr, &session_a, &uri_y).await;
    subscribe_in_session(addr, &session_b, &uri_y).await;

    let (tx, _cancel_tx) =
        crate::test_lsp::spawn_test_pump(registry, WorkspaceRoots::from_paths(&[root]).unwrap());
    let publish = |file: &std::path::Path| {
        let notification =
            crate::lsp::LspNotification::PublishDiagnostics(lsp_types::PublishDiagnosticsParams {
                uri: crate::bridge::path_to_uri(file).unwrap(),
                diagnostics: vec![],
                version: None,
            });
        let tx = tx.clone();
        async move { tx.send(notification).await.unwrap() }
    };

    publish(&file_x).await;
    assert_eq!(stream_a.next_resource_update().await, uri_x);
    // Gives a wrongly queued X time to reach B's stream before Y exists.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    publish(&file_y).await;
    assert_eq!(stream_b.next_resource_update().await, uri_y);
    assert_eq!(stream_a.next_resource_update().await, uri_y);

    server_task.abort();
}

/// A probe reaches the GET stream, a reply sent by POST keeps it open, and a
/// client that goes silent has the stream closed. Real time with
/// sub-second settings.
#[tokio::test]
async fn test_http_probe_pings_get_stream_keeps_answering_client_and_closes_silent_one() {
    let interval = std::time::Duration::from_millis(200);
    let deadline = std::time::Duration::from_secs(2);
    let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
        cfg.with_stream_liveness(StreamLiveness::Probe {
            interval: ProbeInterval::new(interval).unwrap(),
            deadline: ProbeDeadline::new(deadline).unwrap(),
        })
    })
    .await;
    let (session, mut stream) = establish_session(addr).await;

    let mut previous = None;
    for _ in 0..2 {
        let ping = stream.next_ping().await;
        assert_ne!(previous.as_ref(), Some(&ping["id"]));
        let reply = serde_json::json!({"jsonrpc": "2.0", "id": ping["id"], "result": {}});
        let response = post_in_session(addr, &session, &reply.to_string()).await;
        assert!(
            response.starts_with("HTTP/1.1 202"),
            "a probe reply must be accepted, got: {response}"
        );
        previous = Some(ping["id"].clone());
    }

    assert!(
        stream
            .ends_within(interval + deadline + std::time::Duration::from_secs(2))
            .await,
        "a client that stops answering must have its GET stream closed"
    );
    server_task.abort();
}

/// Starts `run_http` with the e2e idle timeout and a session cap of
/// `max_sessions`.
async fn spawn_idle_test_server(
    max_sessions: usize,
    server: crate::mcp::McplsServer,
) -> (
    SocketAddr,
    tokio::task::JoinHandle<Result<(), crate::Error>>,
) {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let mut cfg = HttpConfig::new(addr)
        .with_max_concurrent_sessions(crate::SessionLimit::new(max_sessions).unwrap());
    cfg.session_idle_timeout = IdleTimeout::new(TEST_IDLE).unwrap();
    let task = tokio::spawn(run_http(server, cfg, super::ShutdownSignal::new()));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, task)
}

fn publish_notification(file: &std::path::Path) -> crate::lsp::LspNotification {
    crate::lsp::LspNotification::PublishDiagnostics(lsp_types::PublishDiagnosticsParams {
        uri: crate::bridge::path_to_uri(file).unwrap(),
        diagnostics: vec![],
        version: None,
    })
}

/// #521: a subscribed session whose client vanished must still
/// expire, even though its files keep producing notifications.
/// Keeps publishing until a fresh `initialize` finds the sole slot
/// free, since releasing the dead stream needs a failed write.
#[tokio::test]
async fn test_abandoned_subscribed_session_expires_despite_notifications() {
    let workspace = tempfile::TempDir::new().unwrap();
    let root = dunce::canonicalize(workspace.path()).unwrap();
    let file = root.join("main.rs");
    std::fs::write(&file, "fn main() {}").unwrap();
    let uri = crate::bridge::resources::make_uri(&file).unwrap();

    let server =
        test_server_with_roots(WorkspaceRoots::from_paths(std::slice::from_ref(&root)).unwrap());
    let registry = server.subscription_registry();
    let (addr, server_task) = spawn_idle_test_server(1, server).await;

    let (session, stream) = establish_session(addr).await;
    subscribe_in_session(addr, &session, &uri).await;
    drop(stream);

    let (tx, _cancel_tx) =
        crate::test_lsp::spawn_test_pump(registry, WorkspaceRoots::from_paths(&[root]).unwrap());
    let accept_headers =
        "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n";
    let initialize = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
    let deadline = tokio::time::Instant::now() + E2E_DEADLINE;
    loop {
        tx.send(publish_notification(&file)).await.unwrap();
        let response = raw_http_post(addr, "/mcp", accept_headers, initialize).await;
        if response.starts_with("HTTP/1.1 200") {
            break;
        }
        assert!(
            response.starts_with("HTTP/1.1 429"),
            "unexpected response while waiting for expiry: {response}"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "abandoned session still holds its slot after {E2E_DEADLINE:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    server_task.abort();
}

/// #521 non-regression: a healthy session that only listens on its
/// GET stream (no POSTs) is never reaped, and keeps receiving updates.
#[tokio::test]
async fn test_healthy_get_only_listener_is_not_reaped() {
    let workspace = tempfile::TempDir::new().unwrap();
    let root = dunce::canonicalize(workspace.path()).unwrap();
    let file = root.join("main.rs");
    std::fs::write(&file, "fn main() {}").unwrap();
    let uri = crate::bridge::resources::make_uri(&file).unwrap();

    let server =
        test_server_with_roots(WorkspaceRoots::from_paths(std::slice::from_ref(&root)).unwrap());
    let registry = server.subscription_registry();
    let (addr, server_task) = spawn_idle_test_server(1, server).await;

    let (session, mut stream) = establish_session(addr).await;
    subscribe_in_session(addr, &session, &uri).await;
    let (tx, _cancel_tx) =
        crate::test_lsp::spawn_test_pump(registry, WorkspaceRoots::from_paths(&[root]).unwrap());

    let started = tokio::time::Instant::now();
    while started.elapsed() < TEST_IDLE * 3 {
        tx.send(publish_notification(&file)).await.unwrap();
        assert_eq!(stream.next_resource_update().await, uri);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let ping = post_in_session(
        addr,
        &session,
        r#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#,
    )
    .await;
    assert!(
        ping.starts_with("HTTP/1.1 200"),
        "session with an open GET stream must survive 3x the idle timeout, got: {ping}"
    );

    server_task.abort();
}

const SUBSCRIPTION_ID_KEY: &str = "io.modelcontextprotocol/subscriptionId";

/// Binds `run_http` over a fresh workspace holding `main.rs` and returns
/// what the listen tests need.
struct ListenFixture {
    addr: SocketAddr,
    registry: crate::mcp::SubscriptionRegistry,
    root: std::path::PathBuf,
    file: std::path::PathBuf,
    uri: String,
    server_task: tokio::task::JoinHandle<Result<(), crate::Error>>,
    _workspace: tempfile::TempDir,
}

async fn spawn_listen_fixture() -> ListenFixture {
    let workspace = tempfile::TempDir::new().unwrap();
    let root = dunce::canonicalize(workspace.path()).unwrap();
    let file = root.join("main.rs");
    std::fs::write(&file, "fn main() {}").unwrap();
    let uri = crate::bridge::resources::make_uri(&file).unwrap();
    let server =
        test_server_with_roots(WorkspaceRoots::from_paths(std::slice::from_ref(&root)).unwrap());
    let registry = server.subscription_registry();
    let (addr, server_task) = spawn_idle_test_server(1, server).await;
    ListenFixture {
        addr,
        registry,
        root,
        file,
        uri,
        server_task,
        _workspace: workspace,
    }
}

/// Polls until `registry` holds no live session, or fails after the
/// e2e deadline.
async fn assert_registry_empties(registry: &crate::mcp::SubscriptionRegistry) {
    let deadline = tokio::time::Instant::now() + E2E_DEADLINE;
    while !registry.live_sessions().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "listen registration outlived its connection"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// #522: a stateless `subscriptions/listen` stream acknowledges only
/// syntactically valid URIs and then delivers one `resources/updated`
/// per raw spelling of a published file, tagged with the subscription id.
#[tokio::test]
async fn test_http_listen_delivers_one_update_per_raw_uri() {
    let fx = spawn_listen_fixture().await;
    let alias = fx.uri.replace("main.rs", "%6Dain.rs");
    let other = tempfile::TempDir::new().unwrap();
    let outside = crate::bridge::resources::make_uri(
        &dunce::canonicalize(other.path()).unwrap().join("x.rs"),
    )
    .unwrap();
    let requested = [
        &fx.uri,
        &alias,
        &outside,
        &"file:///not-a-diagnostics-uri".to_owned(),
    ];
    let mut stream = SseStream::open_listen(
        fx.addr,
        &serde_json::json!({"resourceSubscriptions": requested}),
    )
    .await;

    let ack = loop {
        let message = stream.next_message().await;
        if message["method"] == "notifications/subscriptions/acknowledged" {
            break message;
        }
    };
    assert_eq!(
        ack["params"]["notifications"]["resourceSubscriptions"],
        serde_json::json!([fx.uri, alias, outside])
    );

    // The acknowledgment precedes registration; a publish before it is
    // only recovered by cache replay, which this pump's own cache lacks.
    let deadline = tokio::time::Instant::now() + E2E_DEADLINE;
    while fx.registry.live_sessions().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "listen never registered"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let (tx, _cancel_tx) = crate::test_lsp::spawn_test_pump(
        fx.registry.clone(),
        WorkspaceRoots::from_paths(std::slice::from_ref(&fx.root)).unwrap(),
    );
    tx.send(publish_notification(&fx.file)).await.unwrap();

    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..2 {
        let update = stream.next_resource_update_message().await;
        assert_eq!(update["params"]["_meta"][SUBSCRIPTION_ID_KEY], 7);
        seen.insert(update["params"]["uri"].as_str().unwrap().to_owned());
    }
    assert_eq!(seen, [fx.uri.clone(), alias].into_iter().collect());

    drop(stream);
    assert_registry_empties(&fx.registry).await;
    fx.server_task.abort();
}

/// #522: once all listen slots are taken, the next listen is
/// acknowledged and then fails with the retryable `-32053`.
#[tokio::test]
async fn test_http_listen_beyond_slot_limit_fails_with_retryable_error() {
    let fx = spawn_listen_fixture().await;
    let notifications = serde_json::json!({"resourceSubscriptions": [fx.uri]});
    let mut streams = Vec::new();
    for _ in 0..crate::bridge::resources::MAX_LISTEN_STREAMS {
        streams.push(SseStream::open_listen(fx.addr, &notifications).await);
    }
    let deadline = tokio::time::Instant::now() + E2E_DEADLINE;
    while fx.registry.live_sessions().len() < streams.len() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "listen streams never all registered"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let mut refused = SseStream::open_listen(fx.addr, &notifications).await;
    let error = loop {
        let message = refused.next_message().await;
        if message["error"].is_object() {
            break message["error"].clone();
        }
    };
    assert_eq!(
        error["code"],
        crate::error::LISTEN_STREAMS_EXHAUSTED_ERROR_CODE
    );
    assert_eq!(
        error["data"]["max_listen_streams"],
        crate::bridge::resources::MAX_LISTEN_STREAMS
    );

    drop(streams);
    assert_registry_empties(&fx.registry).await;
    fx.server_task.abort();
}

/// #522: diagnostics already cached when the stream opens are replayed
/// right after the acknowledgment.
#[tokio::test]
async fn test_http_listen_replays_cached_diagnostics() {
    let (_workspace, root, file) = crate::test_lsp::workspace_with_main_rs();
    let uri = crate::bridge::resources::make_uri(&file).unwrap();
    let cache = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::bridge::NotificationCache::new(),
    ));
    cache.lock().await.store_diagnostics(
        &crate::config::ServerId::from_static("rust"),
        &crate::bridge::path_to_uri(&file).unwrap(),
        None,
        vec![],
    );
    let server = crate::mcp::McplsServer::new(
        std::sync::Arc::new(crate::bridge::Translator::new()),
        cache,
        WorkspaceRoots::from_paths(&[root]).unwrap(),
        crate::mcp::SubscriptionRegistry::new(),
        crate::ProjectConfigStatus::NotIgnored,
        crate::config::McpConfig::default(),
    );
    let registry = server.subscription_registry();
    let (addr, server_task) = spawn_idle_test_server(1, server).await;

    let mut stream =
        SseStream::open_listen(addr, &serde_json::json!({"resourceSubscriptions": [uri]})).await;
    assert_eq!(stream.next_resource_update().await, uri);

    drop(stream);
    assert_registry_empties(&registry).await;
    server_task.abort();
}

/// #522: a listen whose URIs all fall outside the workspace is
/// acknowledged and then fails with invalid-params, holding no slot.
#[tokio::test]
async fn test_http_listen_with_only_unresolvable_uris_fails() {
    let fx = spawn_listen_fixture().await;
    let other = tempfile::TempDir::new().unwrap();
    let outside = crate::bridge::resources::make_uri(
        &dunce::canonicalize(other.path()).unwrap().join("x.rs"),
    )
    .unwrap();
    let mut stream = SseStream::open_listen(
        fx.addr,
        &serde_json::json!({"resourceSubscriptions": [outside]}),
    )
    .await;

    let error = loop {
        let message = stream.next_message().await;
        if message["error"].is_object() {
            break message["error"].clone();
        }
    };
    assert_eq!(error["code"], -32602);
    assert!(fx.registry.live_sessions().is_empty());
    fx.server_task.abort();
}

/// #522: more URIs than a stream may watch yields an empty
/// acknowledgment followed by an invalid-params error.
#[tokio::test]
async fn test_http_listen_oversized_request_is_rejected() {
    let fx = spawn_listen_fixture().await;
    let uris: Vec<String> = (0..=crate::bridge::resources::MAX_SUBSCRIPTIONS)
        .map(|i| format!("lsp-diagnostics:///f{i}.rs"))
        .collect();
    let mut stream =
        SseStream::open_listen(fx.addr, &serde_json::json!({"resourceSubscriptions": uris})).await;

    let mut acknowledged = false;
    let error = loop {
        let message = stream.next_message().await;
        if message["method"] == "notifications/subscriptions/acknowledged" {
            assert!(
                message["params"]["notifications"]["resourceSubscriptions"].is_null(),
                "oversized request must not echo any URI: {message}"
            );
            acknowledged = true;
        }
        if message["error"].is_object() {
            break message["error"].clone();
        }
    };
    assert!(acknowledged);
    assert_eq!(error["code"], -32602);
    fx.server_task.abort();
}

/// #551: a listen stream ends abruptly after its lease -- no final
/// result and no SSE event id, so a client reads it as a transport
/// close and listens again -- freeing its slot, and a re-listen works.
#[tokio::test]
async fn test_http_listen_lease_ends_stream_abruptly_and_frees_slot() {
    let workspace = tempfile::TempDir::new().unwrap();
    let root = dunce::canonicalize(workspace.path()).unwrap();
    std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
    let uri = crate::bridge::resources::make_uri(&root.join("main.rs")).unwrap();
    let server = test_server_with_roots(WorkspaceRoots::from_paths(&[root]).unwrap());
    let registry = server.subscription_registry();
    let (addr, server_task) =
        spawn_http_server(server, |cfg| cfg.with_listen_lease(short_lease(300))).await;
    let notifications = serde_json::json!({"resourceSubscriptions": [uri]});

    let mut stream = SseStream::open_listen(addr, &notifications).await;
    assert!(
        stream.ends_within(std::time::Duration::from_secs(10)).await,
        "the lease must end the stream"
    );
    assert!(
        !stream.buf.contains("\"result\""),
        "a lease end must not carry a final result: {}",
        stream.buf
    );
    assert!(
        !stream.buf.lines().any(|line| line.starts_with("id:")),
        "stateless SSE must not emit event ids: {}",
        stream.buf
    );
    assert_registry_empties(&registry).await;

    let mut again = SseStream::open_listen(addr, &notifications).await;
    assert!(
        again.ends_within(std::time::Duration::from_secs(10)).await,
        "a re-listen gets a fresh lease"
    );
    server_task.abort();
}

/// #551: with the lease off (`--http-stream-liveness off` resolves to
/// this) a listen stream is not ended on a timer.
#[tokio::test]
async fn test_http_listen_without_lease_is_not_ended() {
    let workspace = tempfile::TempDir::new().unwrap();
    let root = dunce::canonicalize(workspace.path()).unwrap();
    std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
    let uri = crate::bridge::resources::make_uri(&root.join("main.rs")).unwrap();
    let server = test_server_with_roots(WorkspaceRoots::from_paths(&[root]).unwrap());
    let (addr, server_task) = spawn_http_server(server, |cfg| {
        cfg.with_stream_liveness(StreamLiveness::Disabled)
    })
    .await;
    let mut stream =
        SseStream::open_listen(addr, &serde_json::json!({"resourceSubscriptions": [uri]})).await;
    assert!(
        !stream
            .ends_within(std::time::Duration::from_millis(1500))
            .await
    );
    server_task.abort();
}

/// #551 with a real rmcp client at its default 64-slot subscription
/// buffer: 200 subscribed URIs of which 10 are cached and one had its
/// clear evicted. Each listen replays only those 11 (never all 200, which
/// would overflow the buffer), ends abruptly at the lease, and the
/// re-listen replays them again.
#[tokio::test]
async fn test_http_listen_lease_with_real_client_replays_cache_and_evictions() {
    use rmcp::ClientServiceExt as _;
    use rmcp::model::{ClientConfig, ProtocolVersion, ServerNotification, SubscriptionFilter};
    use rmcp::service::SubscriptionEnd;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

    const REQUESTED: usize = 200;
    const CACHED: usize = 10;
    let server_id = crate::config::ServerId::from_static("rust");
    let workspace = tempfile::TempDir::new().unwrap();
    let root = dunce::canonicalize(workspace.path()).unwrap();
    let make = |name: &str| {
        let path = root.join(name);
        std::fs::write(&path, "").unwrap();
        (
            crate::bridge::path_to_uri(&path).unwrap(),
            crate::bridge::resources::make_uri(&path).unwrap(),
        )
    };
    let mut cache = crate::bridge::NotificationCache::new();
    let (evicted_lsp, evicted_uri) = make("evicted.rs");
    cache.store_diagnostics(&server_id, &evicted_lsp, None, vec![]);
    let mut expected = std::collections::BTreeSet::from([evicted_uri.clone()]);
    let mut requested = vec![evicted_uri];
    for i in 0..REQUESTED - 1 {
        let (lsp, uri) = make(&format!("f{i}.rs"));
        if i < CACHED {
            let diagnostic = lsp_types::Diagnostic {
                message: "broken".to_owned().into(),
                ..lsp_types::Diagnostic::default()
            };
            cache.store_diagnostics(&server_id, &lsp, None, vec![diagnostic]);
            expected.insert(uri.clone());
        }
        requested.push(uri);
    }
    let padding = lsp_types::Diagnostic {
        message: "filler".to_owned().into(),
        ..lsp_types::Diagnostic::default()
    };
    let mut fillers = 0;
    while cache.has_diagnostics(&evicted_lsp) {
        let uri = lsp_types::Uri::from(format!("file:///filler{fillers}.rs"));
        cache.store_diagnostics(&server_id, &uri, None, vec![padding.clone()]);
        fillers += 1;
    }
    assert_eq!(expected.len(), CACHED + 1);

    let server = crate::mcp::McplsServer::new(
        std::sync::Arc::new(crate::bridge::Translator::new()),
        std::sync::Arc::new(tokio::sync::Mutex::new(cache)),
        WorkspaceRoots::from_paths(&[root]).unwrap(),
        crate::mcp::SubscriptionRegistry::new(),
        crate::ProjectConfigStatus::NotIgnored,
        crate::config::McpConfig::default(),
    );
    let (addr, server_task) =
        spawn_http_server(server, |cfg| cfg.with_listen_lease(short_lease(500))).await;

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp")),
    );
    let client = ClientConfig::default()
        .serve_with_lifecycle(
            transport,
            rmcp::ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap();
    let mut filter = SubscriptionFilter::new();
    filter.resource_subscriptions = Some(requested);

    for round in 0..2 {
        let mut subscription = client.listen(filter.clone()).await.unwrap();
        let mut seen = std::collections::BTreeSet::new();
        while let Some(notification) =
            tokio::time::timeout(std::time::Duration::from_secs(10), subscription.next())
                .await
                .unwrap()
                .unwrap()
        {
            if let ServerNotification::ResourceUpdatedNotification(update) = notification {
                seen.insert(update.params.uri);
            }
        }
        assert_eq!(seen, expected, "round {round} replay");
        assert!(
            matches!(subscription.end(), Some(SubscriptionEnd::Abrupt)),
            "round {round} must end abruptly, got {:?}",
            subscription.end()
        );
    }

    client.cancel().await.unwrap();
    server_task.abort();
}

/// #233: binding to a non-loopback address must log a warning that
/// tells operators to put the endpoint behind a reverse proxy that
/// *enforces* authentication (not the inverted "ensure no
/// authentication is required" wording it replaced).
#[tokio::test]
async fn test_run_http_non_loopback_bind_warns_to_use_reverse_proxy() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let cfg = HttpConfig::new(addr);

    let captured = CapturedLogs::default();
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    let guard = tracing::subscriber::set_default(subscriber);

    // The warning fires synchronously right after bind, before
    // `axum::serve` starts running indefinitely, so a short timeout
    // is enough to observe it.
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        run_http(test_server(), cfg, super::ShutdownSignal::new()),
    )
    .await;

    drop(guard);

    let messages = captured.messages();
    assert!(
        messages.iter().any(|m| m
            .contains("place this endpoint behind a reverse proxy that enforces authentication")),
        "expected reverse-proxy warning in captured tracing events, got: {messages:?}"
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
                        "Encounter an error when create session: {SESSION_CAP_MARKER}: maximum concurrent \
                         HTTP sessions already active"
                    ),
                )
            }),
        )
        .layer(axum::middleware::from_fn(enforce_session_cap));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

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
        .layer(axum::middleware::from_fn(enforce_session_cap));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let response = raw_http_post(addr, "/", "", b"{}").await;
    assert!(
        response.starts_with("HTTP/1.1 500"),
        "unrelated 500s must not be rewritten to 429, got: {response}"
    );

    server_task.abort();
}

/// #574 end to end over real sockets: a `get_diagnostics` pull that changes a
/// file's diagnostics notifies exactly the sessions subscribed to that file, an
/// identical pull notifies nobody, and a session that never reads its stream
/// delays no other.
///
/// A and B (and the unread D) subscribe to `main.rs` and to `sentinel.rs`, C to
/// `util.rs`. A push for `util.rs` through the pump shows the pull never
/// reached C. Two pushes for `sentinel.rs` then flush A's and B's streams: an
/// update left queued by the repeated pull would sit ahead of the second
/// sentinel and fail the comparison.
#[tokio::test]
#[allow(clippy::too_many_lines, reason = "one end-to-end scenario")]
async fn test_http_pull_notifies_only_subscribers_of_the_pulled_file() {
    use std::sync::Arc;

    use tokio::io::BufReader;
    use tokio::sync::Mutex;

    use crate::bridge::{NotificationCache, ResultContext, Translator};
    use crate::config::{LanguageId, ServerId, ToolRouter};
    use crate::runtime::pump::{PumpShared, PumpWiring};
    use crate::test_lsp::{client_path, fake_lsp_client, read_framed_message, write_response};

    let workspace = tempfile::TempDir::new().unwrap();
    let root = dunce::canonicalize(workspace.path()).unwrap();
    let file_main = root.join("main.rs");
    let file_util = root.join("util.rs");
    let file_sentinel = root.join("sentinel.rs");
    std::fs::write(&file_main, "fn main() {}").unwrap();
    std::fs::write(&file_util, "fn util() {}").unwrap();
    std::fs::write(&file_sentinel, "fn sentinel() {}").unwrap();
    let uri_main = crate::bridge::resources::make_uri(&file_main).unwrap();
    let uri_util = crate::bridge::resources::make_uri(&file_util).unwrap();
    let uri_sentinel = crate::bridge::resources::make_uri(&file_sentinel).unwrap();
    let roots = WorkspaceRoots::from_paths(std::slice::from_ref(&root)).unwrap();

    let server = test_server_with_roots(roots.clone());
    let registry = server.subscription_registry();
    let (addr, server_task) = spawn_http_server(server, |cfg| cfg).await;
    let cache = Arc::new(Mutex::new(NotificationCache::new()));

    let mut translator = Translator::new()
        .with_extensions(crate::test_lsp::test_extensions())
        .with_router(ToolRouter::catch_all([(
            ServerId::from_static("rust"),
            LanguageId::from_static("rust"),
        )]));
    translator.set_workspace_roots(roots.clone());
    let (client, mut fake) = fake_lsp_client();
    translator.register_client(ServerId::from_static("rust"), client);
    translator.install_wiring(Arc::new(PumpWiring::new(
        PumpShared {
            roles: crate::runtime::pump::DiagnosticsRoles::default(),
            notification_cache: Arc::clone(&cache),
            subs: registry.clone(),
            workspace_roots: roots.clone(),
        },
        tokio_util::sync::CancellationToken::new(),
    )));
    let translator = Arc::new(translator);

    let (session_a, mut stream_a) = establish_session(addr).await;
    let (session_b, mut stream_b) = establish_session(addr).await;
    let (session_c, mut stream_c) = establish_session(addr).await;
    let (session_d, _unread_stream_d) = establish_session(addr).await;
    for session in [&session_a, &session_b, &session_d] {
        subscribe_in_session(addr, session, &uri_main).await;
        subscribe_in_session(addr, session, &uri_sentinel).await;
    }
    subscribe_in_session(addr, &session_c, &uri_util).await;

    let mut wire = BufReader::new(&mut fake.write_stdout);
    let report = serde_json::json!({
        "kind": "full",
        "items": [{
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 4}},
            "severity": 1,
            "message": "E0308 expected i32, found &str",
            "code": "E0308"
        }]
    });
    for round in 0..2 {
        let task = {
            let (translator, cache, path) = (
                Arc::clone(&translator),
                Arc::clone(&cache),
                file_main.clone(),
            );
            tokio::spawn(async move {
                translator
                    .handle_diagnostics(client_path(path), ResultContext::None, &cache)
                    .await
            })
        };
        loop {
            let request = read_framed_message(&mut wire).await;
            if request["method"] == "textDocument/diagnostic" {
                write_response(&mut fake.read_half_stdin, &request["id"], report.clone()).await;
                break;
            }
        }
        let result = task.await.unwrap().unwrap();
        assert_eq!(result.diagnostics.len(), 1, "round {round}");
        if round == 0 {
            assert_eq!(stream_a.next_resource_update().await, uri_main);
            assert_eq!(stream_b.next_resource_update().await, uri_main);
        }
    }

    let (tx, _cancel_tx) =
        crate::test_lsp::spawn_test_pump_with_cache(registry, roots, Arc::clone(&cache));
    let publish = |file: &std::path::Path| {
        let notification =
            crate::lsp::LspNotification::PublishDiagnostics(lsp_types::PublishDiagnosticsParams {
                uri: crate::bridge::path_to_uri(file).unwrap(),
                diagnostics: vec![],
                version: None,
            });
        let tx = tx.clone();
        async move { tx.send(notification).await.unwrap() }
    };
    publish(&file_util).await;
    assert_eq!(stream_c.next_resource_update().await, uri_util);
    for _ in 0..2 {
        publish(&file_sentinel).await;
        assert_eq!(stream_a.next_resource_update().await, uri_sentinel);
        assert_eq!(stream_b.next_resource_update().await, uri_sentinel);
    }

    server_task.abort();
}

/// #574: a file known only through a pull is replayed exactly once, on a
/// legacy `resources/subscribe` and on a `subscriptions/listen`.
#[tokio::test]
async fn test_pull_only_file_is_replayed_once_on_subscribe_and_listen() {
    let (_workspace, root, file) = crate::test_lsp::workspace_with_main_rs();
    let uri = crate::bridge::resources::make_uri(&file).unwrap();
    let cache = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::bridge::NotificationCache::new(),
    ));
    cache.lock().await.store_pulled_for_test(
        &crate::config::ServerId::from_static("rust"),
        &crate::bridge::path_to_uri(&file).unwrap(),
        vec![lsp_types::Diagnostic::default()],
    );
    let server = crate::mcp::McplsServer::new(
        std::sync::Arc::new(crate::bridge::Translator::new()),
        cache,
        WorkspaceRoots::from_paths(&[root]).unwrap(),
        crate::mcp::SubscriptionRegistry::new(),
        crate::ProjectConfigStatus::NotIgnored,
        crate::config::McpConfig::default(),
    );
    let (addr, server_task) = spawn_http_server(server, |cfg| cfg).await;
    let quiet = std::time::Duration::from_millis(300);

    let (session, mut stream) = establish_session(addr).await;
    subscribe_in_session(addr, &session, &uri).await;
    assert_eq!(stream.next_resource_update().await, uri);
    assert!(
        tokio::time::timeout(quiet, stream.next_resource_update())
            .await
            .is_err(),
        "subscribe replayed more than once"
    );

    let mut listen =
        SseStream::open_listen(addr, &serde_json::json!({"resourceSubscriptions": [uri]})).await;
    assert_eq!(listen.next_resource_update().await, uri);
    assert!(
        tokio::time::timeout(quiet, listen.next_resource_update())
            .await
            .is_err(),
        "listen replayed more than once"
    );

    server_task.abort();
}
