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
use crate::lsp::{LspClient, LspTransport};

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
    let (client_stdin, write_stdout) = tokio::io::duplex(MOCK_PIPE_CAPACITY);
    let (read_half_stdin, client_stdout) = tokio::io::duplex(MOCK_PIPE_CAPACITY);

    let transport = LspTransport::new(client_stdin, client_stdout);
    let client = LspClient::from_transport(config, transport);

    (
        client,
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
pub fn inert_transport() -> LspTransport {
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

async fn write_framed(writer: &mut DuplexStream, message: &Value) {
    let content = serde_json::to_string(message).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", content.len());
    writer.write_all(header.as_bytes()).await.unwrap();
    writer.write_all(content.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();
}
