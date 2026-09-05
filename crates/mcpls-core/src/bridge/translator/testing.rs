//! Shared test fixtures for the `translator` module's sibling `tests`
//! submodules: an `EncodingCtx` builder, a fake in-process LSP server driven
//! over `cat` pipes, and JSON-RPC framing helpers.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::Translator;
use super::dto::Position;
use super::encoding_ctx::EncodingCtx;
use crate::bridge::encoding::PositionEncoding;
use crate::bridge::state::ResourceLimits;
use crate::bridge::{DiagnosticInfo, DocumentTracker};
use crate::config::{LspServerConfig, ServerId, ToolRouter};
use crate::lsp::{LspClient, LspServer, LspTransport};

type JsonValue = serde_json::Value;

/// Shorthand for building a [`Position`] test fixture.
pub(super) const fn pos(line: u32, character: u32) -> Position {
    Position { line, character }
}

/// A UTF-16 `EncodingCtx`, matching the pre-negotiation behavior: no
/// disk reads, pure line/column offsetting.
pub(super) fn test_ctx() -> EncodingCtx {
    test_ctx_with(PositionEncoding::Utf16)
}

/// An `EncodingCtx` with a fresh, empty `DocumentTracker` -- suitable for
/// tests that need a non-UTF-16 encoding and don't care about the
/// tracker fast path (e.g. exercising the disk-read fallback directly).
pub(super) fn test_ctx_with(encoding: PositionEncoding) -> EncodingCtx {
    EncodingCtx {
        encoding,
        tracker: Arc::new(DocumentTracker::new(
            ResourceLimits::default(),
            HashMap::new(),
        )),
    }
}

pub(super) fn test_uri() -> lsp_types::Uri {
    "file:///test.rs".parse().unwrap()
}

/// A fresh, empty `DocumentTracker` for tests that call
/// `diagnostics_from_cache_entry`/`merge_diagnostics` directly and don't
/// care about the tracker fast path.
pub(super) fn test_tracker() -> Arc<DocumentTracker> {
    Arc::new(DocumentTracker::new(
        ResourceLimits::default(),
        HashMap::new(),
    ))
}

/// Builds an LSP-side diagnostic for `merge_diagnostics` cache fixtures.
pub(super) fn lsp_diag(
    line: u32,
    end_character: u32,
    severity: lsp_types::DiagnosticSeverity,
    message: &str,
    code: Option<&str>,
) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: lsp_types::Range {
            start: lsp_types::Position { line, character: 0 },
            end: lsp_types::Position {
                line,
                character: end_character,
            },
        },
        severity: Some(severity),
        message: message.to_string(),
        code: code.map(|c| lsp_types::NumberOrString::String(c.to_string())),
        source: None,
        code_description: None,
        related_information: None,
        tags: None,
        data: None,
    }
}

pub(super) fn diag_info(diagnostics: Vec<lsp_types::Diagnostic>) -> DiagnosticInfo {
    DiagnosticInfo {
        uri: "file:///test.rs".parse().unwrap(),
        version: Some(1),
        diagnostics,
    }
}

pub(super) struct FakeServer {
    _write_half: Child,
    _read_half: Child,
    pub(super) read_half_stdin: ChildStdin,
    pub(super) write_stdout: ChildStdout,
}

pub(super) fn fake_lsp_client() -> (LspClient, FakeServer) {
    let mut write_half = Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let write_stdin = write_half.stdin.take().unwrap();
    let write_stdout = write_half.stdout.take().unwrap();

    let mut read_half = Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let read_stdout = read_half.stdout.take().unwrap();
    let read_stdin = read_half.stdin.take().unwrap();

    let transport = LspTransport::new(write_stdin, read_stdout);
    let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);

    (
        client,
        FakeServer {
            _write_half: write_half,
            _read_half: read_half,
            read_half_stdin: read_stdin,
            write_stdout,
        },
    )
}

/// Reads one `Content-Length`-framed JSON-RPC message off `reader`.
///
/// `reader` must be reused across calls, not recreated per message: a
/// fresh `BufReader` would silently drop any bytes of a later message it
/// over-read into its internal buffer while parsing an earlier one.
pub(super) async fn read_framed_message(reader: &mut BufReader<&mut ChildStdout>) -> JsonValue {
    let mut content_length = None;
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
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
pub(super) async fn write_response(stdin: &mut ChildStdin, id: &JsonValue, result: JsonValue) {
    let message = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    let content = serde_json::to_string(&message).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", content.len());
    stdin.write_all(header.as_bytes()).await.unwrap();
    stdin.write_all(content.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
}

/// Writes a framed JSON-RPC error response, e.g. to simulate a push-only
/// server answering `textDocument/diagnostic` with method-not-found.
pub(super) async fn write_error_response(
    stdin: &mut ChildStdin,
    id: &JsonValue,
    code: i64,
    message: &str,
) {
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    });
    let content = serde_json::to_string(&response).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", content.len());
    stdin.write_all(header.as_bytes()).await.unwrap();
    stdin.write_all(content.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
}

/// Builds a single-server translator routed to `server_id` for every tool,
/// with a registered `LspServer` fixture carrying `capabilities` (default
/// capabilities advertise nothing).
pub(super) fn translator_with_capabilities(
    dir: &TempDir,
    server_id: &ServerId,
    capabilities: lsp_types::ServerCapabilities,
) -> (Translator, FakeServer) {
    let mut extensions = HashMap::new();
    extensions.insert("rs".to_string(), "rust".to_string());

    let mut translator =
        Translator::new()
            .with_extensions(extensions)
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                "rust".to_string(),
            )]));
    translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

    let (client, server) = fake_lsp_client();
    translator.register_client(server_id.clone(), client);
    translator.register_server(server_id.clone(), LspServer::new_for_test(capabilities));

    (translator, server)
}

/// As [`translator_with_capabilities`], but with a caller-chosen
/// negotiated `position_encoding` -- for tests exercising a non-UTF-16
/// `EncodingCtx` conversion path through a full mocked LSP round trip.
pub(super) fn translator_with_capabilities_and_encoding(
    dir: &TempDir,
    server_id: &ServerId,
    capabilities: lsp_types::ServerCapabilities,
    position_encoding: lsp_types::PositionEncodingKind,
) -> (Translator, FakeServer) {
    let mut extensions = HashMap::new();
    extensions.insert("rs".to_string(), "rust".to_string());

    let mut translator =
        Translator::new()
            .with_extensions(extensions)
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                "rust".to_string(),
            )]));
    translator.set_workspace_roots(vec![dir.path().to_path_buf()]);

    let (client, server) = fake_lsp_client();
    translator.register_client(server_id.clone(), client);
    translator.register_server(
        server_id.clone(),
        LspServer::new_for_test_with_encoding(capabilities, position_encoding),
    );

    (translator, server)
}
