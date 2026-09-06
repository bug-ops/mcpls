//! Shared test fixtures for the `translator` module's sibling `tests`
//! submodules: an `EncodingCtx` builder, a fake in-memory LSP server (see
//! `crate::test_lsp`), and JSON-RPC framing helpers.

use std::collections::HashMap;
use std::sync::Arc;

use tempfile::TempDir;

use super::Translator;
use super::dto::Position;
use super::encoding_ctx::EncodingCtx;
use crate::bridge::encoding::PositionEncoding;
use crate::bridge::state::ResourceLimits;
use crate::bridge::{DiagnosticInfo, DocumentTracker};
use crate::config::{ServerId, ToolRouter};
use crate::lsp::LspServer;
pub(super) use crate::test_lsp::{
    FakeServer, fake_lsp_client, read_framed_message, write_error_response, write_response,
};

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
    lsp_types::Uri::from("file:///test.rs")
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
        message: message.to_string().into(),
        code: code.map(|c| lsp_types::Code::String(c.to_string())),
        source: None,
        code_description: None,
        related_information: None,
        tags: None,
        data: None,
    }
}

pub(super) fn diag_info(diagnostics: Vec<lsp_types::Diagnostic>) -> DiagnosticInfo {
    DiagnosticInfo {
        uri: lsp_types::Uri::from("file:///test.rs"),
        version: Some(1),
        diagnostics,
    }
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
