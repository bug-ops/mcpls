//! Shared in-memory mock LSP transport for `#[cfg(test)]` code across the
//! crate.
//!
//! Wires an [`LspClient`]'s [`LspTransport`] to a pair of
//! [`tokio::io::duplex`] pipes instead of a real subprocess loopback
//! (previously two `cat` child processes echoing bytes back to themselves,
//! duplicated near-verbatim across four modules -- see #378). This removes
//! the OS process-spawn/schedule dependency from the framing round-trip
//! itself. `LspServer`'s test-only constructors (`new_for_test`,
//! `lsp::fake_lsp_server`) no longer need a real child process either --
//! `LspServer`'s internal `child` field is `Option<tokio::process::Child>`,
//! `None` for every fixture built through this module.
//!
//! `pub`, not `pub(crate)`, on the items below: this module is itself
//! private (unexported), so `pub(crate)` here would be redundant -- see
//! `clippy::redundant_pub_crate`. Still only reachable crate-internally via
//! `crate::test_lsp::*`, since the module isn't `pub`.

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::time::Duration;

use crate::config::LspServerConfig;
use crate::lsp::{LspClient, LspTransport, LspTransportReader, ServerInitConfig};

/// Duplex buffer capacity for the mock pipes below. Framed JSON-RPC
/// messages exchanged in these tests run from a few dozen bytes to a
/// handful of KB, so this is generous headroom, not a tuned value.
const MOCK_PIPE_CAPACITY: usize = 256 * 1024;

/// Upper bound [`read_framed_message`] waits for one complete frame before
/// failing loudly, instead of hanging until an external test-runner kill
/// (see that function's docs for why this matters). The longest legitimate
/// wait anywhere in the current suite is ~2s (the backoff delays in
/// `lsp::client::tests::retry_behavior`), so this still leaves a wide
/// margin on a slow/contended CI runner.
///
/// Not safe under `#[tokio::test(start_paused = true)]`: a paused clock
/// auto-advances to the next scheduled timer once the runtime goes idle,
/// so a call that is merely waiting on real (unadvanced) time would have
/// this timeout fire immediately instead of after a genuine 30s wall-clock
/// wait. Safe today only because no current caller of
/// [`read_framed_message`] runs under a paused-clock test.
const READ_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

/// Test-side counterpart of a mocked LSP server, connected to an
/// [`LspClient`]'s transport through two independent [`tokio::io::duplex`]
/// pipes rather than a real subprocess.
pub struct FakeServer {
    /// Written by the test to inject a fake server response or
    /// notification; the client reads it as incoming bytes.
    pub read_half_stdin: DuplexStream,
    /// Written by the client; read by the test to observe the framed bytes
    /// the client actually sent.
    pub write_stdout: DuplexStream,
}

/// Builds an [`LspClient`] wired to an in-memory mock server, for a caller
/// that doesn't care which [`LspServerConfig`] is attached.
pub fn fake_lsp_client() -> (LspClient, FakeServer) {
    fake_lsp_client_with_config(LspServerConfig::rust_analyzer())
}

/// As [`fake_lsp_client`], with a caller-chosen [`LspServerConfig`].
pub fn fake_lsp_client_with_config(config: LspServerConfig) -> (LspClient, FakeServer) {
    let (transport, fake_server) = fake_transport();
    (LspClient::from_transport(config, transport), fake_server)
}

/// Both notification lanes of a client built by
/// [`fake_lsp_client_with_lanes`].
pub struct FakeLanes {
    /// Diagnostics/log/showMessage lane.
    pub notification_rx: tokio::sync::mpsc::Receiver<crate::lsp::LspNotification>,
    /// Lifecycle lane (`$/progress` `begin`/`end`, unrecognized notifications).
    pub lifecycle_rx: tokio::sync::mpsc::Receiver<crate::lsp::LspNotification>,
}

/// As [`fake_lsp_client`], but the client forwards notifications onto real
/// lanes, returned for the test to drain or hand to a pump.
pub fn fake_lsp_client_with_lanes() -> (LspClient, FakeServer, FakeLanes) {
    let (transport, fake_server) = fake_transport();
    let (notification_tx, notification_rx) = tokio::sync::mpsc::channel(32);
    let (lifecycle_tx, lifecycle_rx) = tokio::sync::mpsc::channel(8);
    let client = LspClient::from_transport_with_notifications(
        LspServerConfig::rust_analyzer(),
        transport,
        notification_tx,
        lifecycle_tx,
    );
    (
        client,
        fake_server,
        FakeLanes {
            notification_rx,
            lifecycle_rx,
        },
    )
}

fn fake_transport() -> ((LspTransport, LspTransportReader), FakeServer) {
    let (client_stdin, write_stdout) = tokio::io::duplex(MOCK_PIPE_CAPACITY);
    let (read_half_stdin, client_stdout) = tokio::io::duplex(MOCK_PIPE_CAPACITY);
    (
        LspTransport::new(client_stdin, client_stdout),
        FakeServer {
            read_half_stdin,
            write_stdout,
        },
    )
}

/// An [`LspTransport`] backed by duplex pipes whose peer half is dropped
/// immediately -- for tests that only need a real transport to satisfy a
/// constructor and never exercise it over the wire. A subsequent write
/// fails with a broken-pipe error and a read observes EOF, both
/// deterministically and immediately (unlike the old `cat`-backed
/// placeholder, which accepted writes and raced on EOF) -- the message
/// loop this feeds dies right away either way, so callers that construct
/// an `LspServer`/`LspClient` around this and never send real traffic
/// through it are unaffected.
#[must_use]
pub fn inert_transport() -> (LspTransport, LspTransportReader) {
    let (stdin, _unused_read) = tokio::io::duplex(1);
    let (_unused_write, stdout) = tokio::io::duplex(1);
    LspTransport::new(stdin, stdout)
}

/// Reads one `Content-Length`-framed JSON-RPC message off `reader`.
///
/// `reader` must be reused across calls, not recreated per message: a fresh
/// `BufReader` would silently drop any bytes of a later message it
/// over-read into its internal buffer while parsing an earlier one.
///
/// Bounded by [`READ_FRAME_TIMEOUT`]: a test bug that never writes the
/// expected message (or an EOF on the underlying duplex pipe, e.g. its
/// writer half was dropped) must fail fast with a clear panic message,
/// rather than hanging until nextest's external 120s kill turns it into an
/// opaque, unattributed timeout.
///
/// # Panics
///
/// Panics if no complete frame arrives within [`READ_FRAME_TIMEOUT`], or if
/// the stream reaches EOF before a complete frame is read.
pub async fn read_framed_message(reader: &mut BufReader<&mut DuplexStream>) -> Value {
    tokio::time::timeout(READ_FRAME_TIMEOUT, read_framed_message_inner(reader))
        .await
        .expect("timed out waiting for a complete framed JSON-RPC message")
}

async fn read_framed_message_inner(reader: &mut BufReader<&mut DuplexStream>) -> Value {
    let mut content_length = None;
    let mut line = String::new();
    loop {
        line.clear();
        // `read_line` returns `Ok(0)` at EOF without erroring; left
        // unchecked, `line` stays `""`, which matches neither line-ending
        // check below, so the loop would spin forever re-reading EOF
        // instead of failing.
        let bytes_read = reader.read_line(&mut line).await.unwrap();
        assert!(bytes_read != 0, "EOF before a complete frame was read");
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((key, value)) = line.trim_end().split_once(':')
            && key.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut buf = vec![0u8; content_length.unwrap()];
    reader.read_exact(&mut buf).await.unwrap();
    serde_json::from_slice(&buf).unwrap()
}

/// Writes a framed JSON-RPC success response, as a real LSP server would.
pub async fn write_response(writer: &mut DuplexStream, id: &Value, result: Value) {
    write_framed(
        writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
    )
    .await;
}

/// Writes a framed JSON-RPC error response, e.g. to simulate a push-only
/// server answering a request with method-not-found.
pub async fn write_error_response(writer: &mut DuplexStream, id: &Value, code: i64, message: &str) {
    write_framed(
        writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    )
    .await;
}

/// Writes a framed server-to-client JSON-RPC request.
pub async fn write_request(writer: &mut DuplexStream, id: &Value, method: &str, params: Value) {
    write_framed(
        writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }),
    )
    .await;
}

/// Writes a framed JSON-RPC notification, as a real LSP server would.
pub async fn write_notification(writer: &mut DuplexStream, method: &str, params: Value) {
    write_framed(
        writer,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }),
    )
    .await;
}

async fn write_framed(writer: &mut DuplexStream, message: &Value) {
    let content = serde_json::to_string(message).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", content.len());
    writer.write_all(header.as_bytes()).await.unwrap();
    writer.write_all(content.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();
}

/// Captures `(level, message)` pairs for `tracing` events emitted while a
/// closure runs, as a `tracing_subscriber::Layer`.
///
/// Shared here rather than duplicated per-module: `transport.rs`, `lib.rs`
/// and `bridge/notifications.rs` each used to carry their own
/// message-only `CapturedMessages` copy. This is a strict superset (it
/// also records the event's [`tracing::Level`], needed to assert that a
/// log line is or is not at `error!` severity, not just that it contains
/// certain text) so it replaces all of them.
#[derive(Clone, Default)]
pub struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<(tracing::Level, String)>>>);

impl CapturedLogs {
    /// Snapshot of everything captured so far, as `(level, message)` pairs.
    pub fn entries(&self) -> Vec<(tracing::Level, String)> {
        self.0.lock().unwrap().clone()
    }

    /// Snapshot of captured message text only, for callers that don't care
    /// about severity.
    pub fn messages(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, message)| message.clone())
            .collect()
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedLogs {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct MessageVisitor(String);
        impl tracing::field::Visit for MessageVisitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        self.0
            .lock()
            .unwrap()
            .push((*event.metadata().level(), visitor.0));
    }
}

/// Minimal [`ServerInitConfig`] around `server_config` for test fixtures.
pub const fn init_config_for(server_config: LspServerConfig) -> ServerInitConfig {
    ServerInitConfig {
        server_config,
        workspace_roots: vec![],
        initialization_options: None,
        position_encodings: vec![],
        notification_tx: None,
    }
}

/// Extension map shared by routing tests: `.rs` and `.tsx`.
pub fn test_extensions() -> std::collections::HashMap<String, String> {
    std::collections::HashMap::from([
        ("rs".to_string(), "rust".to_string()),
        ("tsx".to_string(), "typescriptreact".to_string()),
    ])
}

/// Writes `script_body` to `dir/server.sh` and returns a config that runs it
/// under `sh` as an LSP server.
#[cfg(unix)]
pub fn sh_script_init_config(dir: &std::path::Path, script_body: &str) -> ServerInitConfig {
    let script = dir.join("server.sh");
    std::fs::write(&script, script_body).unwrap();
    let mut server_config = LspServerConfig::rust_analyzer();
    server_config.command = "sh".to_string();
    server_config.args = vec![script.to_string_lossy().to_string()];
    init_config_for(server_config)
}

/// Reads and discards a full LSP-framed request from stdin, so a fixture
/// replying after it can't answer before the request is even sent (#447).
#[cfg(unix)]
pub const READ_REQUEST_SH: &str = r#"content_length=0
while IFS= read -r header; do
  header=$(printf '%s' "$header" | tr -d '\r')
  [ -z "$header" ] && break
  case "$header" in
    Content-Length:*) content_length=$(printf '%s' "$header" | sed 's/^Content-Length: *//') ;;
  esac
done
[ "$content_length" -gt 0 ] 2>/dev/null && dd bs=1 count="$content_length" 2>/dev/null >/dev/null
"#;

/// Prepends [`READ_REQUEST_SH`] to a fake `sh`-server fixture body.
#[cfg(unix)]
pub fn with_read_preamble(body: &str) -> String {
    format!("{READ_REQUEST_SH}{body}")
}

/// Spawns a real [`crate::diagnostics_pump`] over `subs` and returns the
/// sender feeding it plus the cancel sender (keep it alive: dropping it stops
/// the pump).
pub fn spawn_test_pump(
    subs: crate::mcp::SubscriptionRegistry,
    workspace_roots: std::sync::Arc<[std::path::PathBuf]>,
) -> (
    tokio::sync::mpsc::Sender<crate::lsp::LspNotification>,
    tokio::sync::watch::Sender<bool>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let (lifecycle_tx, lifecycle_rx) = tokio::sync::mpsc::channel(8);
    // Held so the lifecycle lane stays open for the pump's lifetime.
    let (_cache, cancel_tx) = spawn_pump(rx, lifecycle_rx, lifecycle_tx, subs, workspace_roots);
    (tx, cancel_tx)
}

/// As [`spawn_test_pump`], but over a client's own [`FakeLanes`]; returns the
/// pump's notification cache (for asserting on indexing state) and the
/// cancel sender (keep it alive: dropping it stops the pump).
pub fn spawn_test_pump_over_lanes(
    lanes: FakeLanes,
) -> (
    std::sync::Arc<tokio::sync::Mutex<crate::bridge::NotificationCache>>,
    tokio::sync::watch::Sender<bool>,
) {
    spawn_pump(
        lanes.notification_rx,
        lanes.lifecycle_rx,
        (),
        crate::mcp::SubscriptionRegistry::default(),
        std::sync::Arc::from([]),
    )
}

fn spawn_pump<K: Send + 'static>(
    rx: tokio::sync::mpsc::Receiver<crate::lsp::LspNotification>,
    lifecycle_rx: tokio::sync::mpsc::Receiver<crate::lsp::LspNotification>,
    keep_alive: K,
    subs: crate::mcp::SubscriptionRegistry,
    workspace_roots: std::sync::Arc<[std::path::PathBuf]>,
) -> (
    std::sync::Arc<tokio::sync::Mutex<crate::bridge::NotificationCache>>,
    tokio::sync::watch::Sender<bool>,
) {
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let notification_cache = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::bridge::NotificationCache::new(),
    ));
    let shared = crate::PumpShared {
        notification_cache: std::sync::Arc::clone(&notification_cache),
        subs,
        workspace_roots,
    };
    tokio::spawn(async move {
        let _keep_alive = keep_alive;
        crate::diagnostics_pump(
            crate::config::ServerId::from("rust"),
            rx,
            lifecycle_rx,
            cancel_rx,
            true,
            shared,
        )
        .await;
    });
    (notification_cache, cancel_tx)
}

/// A temp workspace holding `main.rs`: the guard, the canonical root and the file.
pub fn workspace_with_main_rs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    let file = root.join("main.rs");
    std::fs::write(&file, "fn main() {}").unwrap();
    (dir, root, file)
}

/// An absolute path that is valid on the host platform (Windows needs a drive
/// letter for `lsp-diagnostics:///` URIs to parse), for URIs that never touch disk.
pub fn absolute_path(relative: &str) -> std::path::PathBuf {
    let root = if cfg!(windows) { r"C:\" } else { "/" };
    std::path::PathBuf::from(root).join(relative)
}

/// The `lsp-diagnostics:///` URI of the path [`absolute_path`] returns.
pub fn absolute_uri(relative: &str) -> String {
    crate::bridge::resources::make_uri(&absolute_path(relative)).unwrap()
}

/// A diagnostics resource URI built from a literal, bypassing resolution.
pub fn diagnostics_uri(uri: &str) -> crate::bridge::resources::DiagnosticsResourceUri {
    crate::bridge::resources::DiagnosticsResourceUri::for_test(uri)
}
