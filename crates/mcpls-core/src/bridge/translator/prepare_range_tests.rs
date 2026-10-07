//! Fake-server tests for `handle_prepare_rename` and `handle_format_range`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use std::{assert_matches, fs};

use tempfile::TempDir;
use tokio::io::BufReader;
use tokio::time::timeout;

use super::Translator;
use super::dto::{
    FormatDocumentResult, Position2D, PrepareRenameOutcome, PrepareRenameResult, TabSize,
};
use super::testing::*;
use crate::bridge::{IndexingPolicy, WorkspaceRoots};
use crate::config::{
    FileExtension, LanguageId, LspServerConfig, ServerCommand, ServerId, TimeoutSecs, ToolKind,
    ToolRouter, ToolSet,
};
use crate::error::{Error, McpErrorKind, Result};
use crate::lsp::LspServer;
use crate::redaction::Redactions;
use crate::test_lsp::client_path;

fn prepare_caps() -> lsp_types::ServerCapabilities {
    lsp_types::ServerCapabilities {
        rename_provider: Some(lsp_types::RenameProvider::RenameOptions(
            lsp_types::RenameOptions {
                prepare_provider: Some(true),
                ..Default::default()
            },
        )),
        ..Default::default()
    }
}

fn range_caps() -> lsp_types::ServerCapabilities {
    lsp_types::ServerCapabilities {
        document_range_formatting_provider: Some(lsp_types::DocumentRangeFormattingProvider::Bool(
            true,
        )),
        ..Default::default()
    }
}

enum Answer {
    Result(serde_json::Value),
    Error(i32, &'static str),
}

async fn prepare_rename_with(answer: Answer) -> Result<PrepareRenameResult> {
    prepare_rename_redacting(answer, Redactions::default()).await
}

async fn prepare_rename_redacting(
    answer: Answer,
    redactions: Redactions,
) -> Result<PrepareRenameResult> {
    let dir = TempDir::new().unwrap();
    let (translator, mut server) =
        translator_with_capabilities(&dir, &ServerId::from_static("rust"), prepare_caps());
    let translator = translator.with_startup_redactions(Arc::new(redactions));
    let path = dir.path().join("a.rs");
    fs::write(&path, "fn old_name() {}").unwrap();

    let translator = Arc::new(translator);
    let handle = {
        let translator = Arc::clone(&translator);
        let path = path.to_string_lossy().into_owned();
        tokio::spawn(async move {
            translator
                .handle_prepare_rename(client_path(path), pos(1, 5))
                .await
        })
    };
    let mut wire = BufReader::new(&mut server.write_stdout);
    assert_eq!(
        read_framed_message(&mut wire).await["method"],
        "textDocument/didOpen"
    );
    let request = read_framed_message(&mut wire).await;
    assert_eq!(request["method"], "textDocument/prepareRename");
    assert_eq!(request["params"]["position"]["character"], 4);
    match answer {
        Answer::Result(value) => {
            write_response(&mut server.read_half_stdin, &request["id"], value).await;
        }
        Answer::Error(code, message) => {
            write_error_response(
                &mut server.read_half_stdin,
                &request["id"],
                i64::from(code),
                message,
            )
            .await;
        }
    }
    timeout(Duration::from_secs(30), handle)
        .await
        .expect("handler call should not hang")
        .unwrap()
}

fn range_json() -> serde_json::Value {
    serde_json::json!({
        "start": {"line": 0, "character": 3},
        "end": {"line": 0, "character": 11}
    })
}

#[tokio::test]
async fn prepare_rename_returns_range_and_placeholder() {
    let result = prepare_rename_with(Answer::Result(
        serde_json::json!({"range": range_json(), "placeholder": "old_name"}),
    ))
    .await
    .unwrap();
    let wire = serde_json::to_value(&result).unwrap();
    assert_eq!(wire["status"], "renameable");
    assert_eq!(wire["placeholder"], "old_name");
    assert_eq!(wire["range"]["start"]["character"], 4);
    assert_eq!(wire["range"]["end"]["character"], 12);
}

#[tokio::test]
async fn prepare_rename_bare_range_has_no_placeholder() {
    let result = prepare_rename_with(Answer::Result(range_json()))
        .await
        .unwrap();
    assert_matches!(
        result.outcome,
        PrepareRenameOutcome::Renameable {
            placeholder: None,
            ..
        }
    );
}

#[tokio::test]
async fn prepare_rename_default_behavior_invents_no_range() {
    let result = prepare_rename_with(Answer::Result(serde_json::json!({"defaultBehavior": true})))
        .await
        .unwrap();
    assert_eq!(result.outcome, PrepareRenameOutcome::DefaultBehavior);
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        serde_json::json!({"status": "default_behavior"})
    );
}

#[tokio::test]
async fn prepare_rename_null_and_default_false_are_not_renameable() {
    for answer in [
        serde_json::Value::Null,
        serde_json::json!({"defaultBehavior": false}),
    ] {
        let result = prepare_rename_with(Answer::Result(answer)).await.unwrap();
        assert_eq!(
            result.outcome,
            PrepareRenameOutcome::NotRenameable {
                server_message: None
            }
        );
    }
}

#[tokio::test]
async fn prepare_rename_invalid_params_error_is_not_renameable() {
    let result = prepare_rename_with(Answer::Error(-32602, "cannot rename a keyword"))
        .await
        .unwrap();
    assert_eq!(
        result.outcome,
        PrepareRenameOutcome::NotRenameable {
            server_message: Some("cannot rename a keyword".to_string())
        }
    );
}

async fn logs_of(answer: Answer) -> Vec<(tracing::Level, String)> {
    logs_of_redacting(answer, Redactions::default()).await
}

async fn logs_of_redacting(
    answer: Answer,
    redactions: Redactions,
) -> Vec<(tracing::Level, String)> {
    use tracing_subscriber::prelude::*;

    let captured = crate::test_lsp::CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry()
            .with(captured.clone())
            .with(tracing_subscriber::filter::LevelFilter::DEBUG),
    );
    drop(prepare_rename_redacting(answer, redactions).await);
    captured.entries()
}

fn error_response_logs(
    logs: &[(tracing::Level, String)],
    level: tracing::Level,
) -> Vec<&(tracing::Level, String)> {
    logs.iter()
        .filter(|(l, msg)| *l == level && msg.contains("LSP error response"))
        .collect()
}

#[tokio::test]
async fn prepare_rename_not_renameable_does_not_log_an_error() {
    let logs = logs_of(Answer::Error(-32602, "No references found at position")).await;
    assert!(
        error_response_logs(&logs, tracing::Level::ERROR).is_empty(),
        "{logs:?}"
    );
    assert_eq!(
        error_response_logs(&logs, tracing::Level::DEBUG).len(),
        1,
        "{logs:?}"
    );
}

#[tokio::test]
async fn prepare_rename_invalid_offset_still_logs_an_error() {
    let logs = logs_of(Answer::Error(
        -32602,
        "Invalid offset LineCol { line: 9, col: 0 } (line index length: 16)",
    ))
    .await;
    assert_eq!(
        error_response_logs(&logs, tracing::Level::ERROR).len(),
        1,
        "{logs:?}"
    );
}

/// #612: the server's own rejection text goes through the result's
/// `ServerText`, so a configured secret in it is hidden.
#[tokio::test]
async fn prepare_rename_not_renameable_message_is_redacted() {
    use crate::redaction::{Redactions, ServerText as _};

    let mut result = prepare_rename_with(Answer::Error(-32602, "cannot rename bravo-secret-222"))
        .await
        .unwrap();
    result.redact_server_text(&Redactions::new([(
        "B_TOKEN".to_owned(),
        "bravo-secret-222".to_owned(),
    )]));

    let wire = serde_json::to_value(&result).unwrap();
    assert_eq!(wire["server_message"], "cannot rename [redacted:B_TOKEN]");
}

#[tokio::test]
async fn prepare_rename_out_of_range_position_stays_an_error() {
    let err = prepare_rename_with(Answer::Error(
        -32602,
        "Invalid offset LineCol { line: 9, col: 0 } (line index length: 16)",
    ))
    .await
    .unwrap_err();
    assert_matches!(err.mcp_error_kind(), McpErrorKind::InvalidPosition(_));
}

/// clangd rejects a position with no renameable symbol with `-32001`
/// (`UnknownErrorCode`), not `-32602`; pinned so a change to the mapping is
/// a conscious one. The code is LSP's catch-all, so the server's own text
/// stays in `server_message`.
#[tokio::test]
async fn prepare_rename_clangd_no_symbol_error_is_not_renameable() {
    let result = prepare_rename_with(Answer::Error(
        -32001,
        "Cannot rename symbol: there is no symbol at the given location",
    ))
    .await
    .unwrap();
    assert_eq!(
        result.outcome,
        PrepareRenameOutcome::NotRenameable {
            server_message: Some(
                "Cannot rename symbol: there is no symbol at the given location".to_string()
            )
        }
    );
}

/// `RequestFailed` is a generic failure, not a "no symbol" answer.
#[tokio::test]
async fn prepare_rename_request_failed_stays_a_server_error() {
    let err = prepare_rename_with(Answer::Error(-32803, "boom"))
        .await
        .unwrap_err();
    assert_matches!(err, Error::LspServerError { code: -32803, .. });
}

fn tracked_translator(
    dir: &TempDir,
    caps: lsp_types::ServerCapabilities,
    source: &str,
) -> (Translator, std::path::PathBuf, impl Sized) {
    let (translator, server) =
        translator_with_capabilities(dir, &ServerId::from_static("rust"), caps);
    let path = dir.path().join("a.rs");
    fs::write(&path, source).unwrap();
    (translator, path, server)
}

#[tokio::test]
async fn prepare_rename_line_beyond_the_document_is_rejected_before_the_request() {
    let dir = TempDir::new().unwrap();
    let (translator, path, _server) =
        tracked_translator(&dir, prepare_caps(), "fn a() {}\nfn b() {}");

    let err = timeout(
        Duration::from_secs(5),
        translator
            .handle_prepare_rename(client_path(path.to_string_lossy().into_owned()), pos(3, 1)),
    )
    .await
    .expect("must fail before any LSP round-trip")
    .unwrap_err();

    assert_matches!(err, Error::PositionBeyondDocument { line, .. } if line.get() == 3);
    assert_eq!(err.mcp_error_kind(), McpErrorKind::InvalidParams);
}

fn all_positioned_caps() -> lsp_types::ServerCapabilities {
    serde_json::from_value(serde_json::json!({
        "hoverProvider": true,
        "definitionProvider": true,
        "typeDefinitionProvider": true,
        "implementationProvider": true,
        "declarationProvider": true,
        "referencesProvider": true,
        "completionProvider": {},
        "signatureHelpProvider": {},
        "callHierarchyProvider": true,
        "typeHierarchyProvider": true,
        "documentHighlightProvider": true,
        "selectionRangeProvider": true,
        "renameProvider": {"prepareProvider": true},
        "codeActionProvider": true,
        "inlayHintProvider": true,
    }))
    .unwrap()
}

/// Asserts `call` fails with an out-of-document line, as invalid params,
/// without a round-trip to the server.
async fn reject_beyond<T>(name: &str, call: Pin<Box<dyn Future<Output = Result<T>> + Send + '_>>) {
    let err = timeout(Duration::from_secs(5), call)
        .await
        .unwrap_or_else(|_| panic!("{name} must fail before any LSP round-trip"))
        .err()
        .unwrap_or_else(|| panic!("{name} accepted a line beyond the document"));
    assert_matches!(
        err,
        Error::PositionBeyondDocument { line, .. } if line.get() == 99,
        "{name}"
    );
    assert_eq!(err.mcp_error_kind(), McpErrorKind::InvalidParams, "{name}");
}

macro_rules! reject {
    ($name:literal, $call:expr) => {
        reject_beyond($name, Box::pin($call)).await
    };
}

/// #641: every tool that takes a position or range rejects a line past the
/// end of the tracked document as invalid params, before any LSP request.
#[tokio::test]
async fn every_positioned_tool_rejects_a_line_beyond_the_document() {
    use crate::bridge::ResultContext;

    let dir = TempDir::new().unwrap();
    let (translator, path, _server) = tracked_translator(&dir, all_positioned_caps(), "a\nb\n");
    let file = || client_path(path.to_string_lossy().into_owned());
    let (beyond, ctx) = (pos(99, 1), ResultContext::None);
    let t = &translator;

    reject!("hover", t.handle_hover(file(), beyond));
    reject!("definition", t.handle_definition(file(), beyond, ctx));
    reject!(
        "type_definition",
        t.handle_type_definition(file(), beyond, ctx)
    );
    reject!(
        "implementation",
        t.handle_implementation(file(), beyond, ctx)
    );
    reject!("declaration", t.handle_declaration(file(), beyond, ctx));
    reject!("references", t.handle_references(file(), beyond, true, ctx));
    reject!("completions", t.handle_completions(file(), beyond, None));
    reject!("signature_help", t.handle_signature_help(file(), beyond));
    reject!(
        "call_hierarchy",
        t.handle_call_hierarchy_prepare(file(), beyond)
    );
    reject!(
        "type_hierarchy",
        t.handle_type_hierarchy_prepare(file(), beyond)
    );
    reject!("highlights", t.handle_document_highlights(file(), beyond));
    reject!("selection_range", t.handle_selection_range(file(), beyond));
    reject!(
        "rename",
        t.handle_rename(
            file(),
            beyond,
            crate::bridge::NewName::try_new("x").unwrap()
        )
    );
    reject!("prepare_rename", t.handle_prepare_rename(file(), beyond));
    reject!(
        "code_actions",
        t.handle_code_actions(file(), bounded(pos(1, 1), beyond), None)
    );
    reject!(
        "inlay_hints",
        t.handle_inlay_hints(file(), span(pos(1, 1), beyond))
    );
    // The start of a range is checked too, not only its end.
    let past = pos(100, 1);
    reject!(
        "code_actions_start",
        t.handle_code_actions(file(), bounded(beyond, past), None)
    );
    reject!(
        "inlay_hints_start",
        t.handle_inlay_hints(file(), span(beyond, past))
    );
}

#[tokio::test]
async fn prepare_rename_catch_all_error_logs_a_warning() {
    let logs = logs_of(Answer::Error(-32001, "internal clangd failure")).await;
    assert!(
        logs.iter()
            .any(|(level, msg)| *level == tracing::Level::WARN
                && msg.contains("UnknownErrorCode")
                && msg.contains("internal clangd failure")),
        "{logs:?}"
    );
    assert!(
        error_response_logs(&logs, tracing::Level::ERROR).is_empty(),
        "{logs:?}"
    );
}

#[tokio::test]
async fn prepare_rename_catch_all_warning_redacts_the_server_message() {
    let redactions = Redactions::new([("B_TOKEN".to_owned(), "bravo-secret-222".to_owned())]);
    let logs = logs_of_redacting(
        Answer::Error(-32001, "failed for bravo-secret-222"),
        redactions,
    )
    .await;
    let warnings: Vec<_> = logs
        .iter()
        .filter(|(level, _)| *level == tracing::Level::WARN)
        .collect();
    assert!(
        warnings
            .iter()
            .any(|(_, msg)| msg.contains("[redacted:B_TOKEN]")),
        "{logs:?}"
    );
    assert!(
        warnings
            .iter()
            .all(|(_, msg)| !msg.contains("bravo-secret-222")),
        "{logs:?}"
    );
}

#[tokio::test]
async fn capability_error_wins_over_a_line_beyond_the_document() {
    let dir = TempDir::new().unwrap();
    let (translator, path, _server) =
        tracked_translator(&dir, lsp_types::ServerCapabilities::default(), "a\nb\n");
    let file = || client_path(path.to_string_lossy().into_owned());

    let rename = translator
        .handle_prepare_rename(file(), pos(99, 1))
        .await
        .unwrap_err();
    assert_matches!(rename, Error::CapabilityNotSupported { .. });
    let format = translator
        .handle_format_range(
            file(),
            bounded(pos(1, 1), pos(99, 1)),
            TabSize::default(),
            true,
        )
        .await
        .unwrap_err();
    assert_matches!(format, Error::CapabilityNotSupported { .. });
}

#[tokio::test]
async fn an_untracked_document_passes_the_line_check() {
    use super::routing::{Capability, IndexingGate};

    let dir = TempDir::new().unwrap();
    let (translator, path, _server) = tracked_translator(&dir, prepare_caps(), "a\n");
    let doc = translator
        .prepare_gated_document(
            &client_path(path.to_string_lossy().into_owned()),
            IndexingGate::Required(Capability::PrepareRename),
        )
        .await
        .unwrap();
    assert_matches!(
        translator.require_line_in_document(&doc, pos(99, 1)),
        Err(Error::PositionBeyondDocument { line, .. }) if line.get() == 99
    );

    translator.document_tracker.close(doc.path());

    translator
        .require_line_in_document(&doc, pos(99, 1))
        .unwrap();
}

#[tokio::test]
async fn prepare_rename_character_past_the_line_end_is_forwarded() {
    let dir = TempDir::new().unwrap();
    let (translator, mut server) =
        translator_with_capabilities(&dir, &ServerId::from_static("rust"), prepare_caps());
    let path = dir.path().join("a.rs");
    fs::write(&path, "fn a() {}").unwrap();
    let translator = Arc::new(translator);
    let handle = {
        let translator = Arc::clone(&translator);
        let path = path.to_string_lossy().into_owned();
        tokio::spawn(async move {
            translator
                .handle_prepare_rename(client_path(path), pos(1, 999))
                .await
        })
    };
    let mut wire = BufReader::new(&mut server.write_stdout);
    assert_eq!(
        read_framed_message(&mut wire).await["method"],
        "textDocument/didOpen"
    );
    let request = read_framed_message(&mut wire).await;
    assert_eq!(request["method"], "textDocument/prepareRename");
    assert_eq!(request["params"]["position"]["character"], 998);
    write_response(
        &mut server.read_half_stdin,
        &request["id"],
        serde_json::Value::Null,
    )
    .await;
    timeout(Duration::from_secs(30), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn prepare_rename_other_server_errors_propagate() {
    let err = prepare_rename_with(Answer::Error(-32603, "boom"))
        .await
        .unwrap_err();
    assert_matches!(err, Error::LspServerError { code: -32603, .. });
}

#[tokio::test]
async fn prepare_rename_requires_prepare_provider_not_just_rename() {
    let dir = TempDir::new().unwrap();
    let caps = lsp_types::ServerCapabilities {
        rename_provider: Some(lsp_types::RenameProvider::Bool(true)),
        ..Default::default()
    };
    let (translator, _server) =
        translator_with_capabilities(&dir, &ServerId::from_static("rust"), caps);
    let path = dir.path().join("a.rs");
    fs::write(&path, "fn old_name() {}").unwrap();

    let err = translator
        .handle_prepare_rename(client_path(path.to_string_lossy().into_owned()), pos(1, 5))
        .await
        .unwrap_err();
    assert_matches!(err, Error::CapabilityNotSupported { .. });
}

fn handles_config(name: &str, handles: Vec<ToolKind>) -> LspServerConfig {
    LspServerConfig {
        language_id: LanguageId::from_static("rust"),
        command: ServerCommand::new(name.to_string()).unwrap().into(),
        args: vec![],
        env: crate::config::ServerEnv::default(),
        file_patterns: vec![],
        initialization_options: None,
        settings: None,
        timeout_seconds: TimeoutSecs::new(30).unwrap(),
        request_timeout_seconds: TimeoutSecs::new(30).unwrap(),
        heuristics: None,
        name: Some(ServerId::new(name).unwrap()),
        handles: Some(ToolSet::new(handles).unwrap()),
        indexing: IndexingPolicy::Auto,
    }
}

/// `prepare_rename` rides on the `rename` route: the server that handles
/// only `rename` answers it, while a tool claimed by no server (type
/// hierarchy here) is refused rather than sent to either.
#[tokio::test]
async fn prepare_rename_routes_with_rename() {
    let dir = TempDir::new().unwrap();
    let configs = [
        handles_config("renamer", vec![ToolKind::Rename]),
        handles_config("hoverer", vec![ToolKind::Hover]),
    ];
    let mut translator = Translator::new()
        .with_extensions(std::collections::HashMap::from([(
            FileExtension::from_static("rs"),
            crate::config::LanguageId::from_static("rust"),
        )]))
        .with_router(ToolRouter::from_configs(&configs).unwrap());
    translator
        .set_workspace_roots(WorkspaceRoots::from_paths(&[dir.path().to_path_buf()]).unwrap());

    let (renamer_client, mut renamer) = fake_lsp_client();
    let (hoverer_client, _hoverer) = fake_lsp_client();
    for (id, client) in [("renamer", renamer_client), ("hoverer", hoverer_client)] {
        translator.register_client(ServerId::new(id).unwrap(), client);
        translator.register_server(
            ServerId::new(id).unwrap(),
            LspServer::new_for_test(prepare_caps()),
        );
    }
    let path = dir.path().join("a.rs");
    fs::write(&path, "fn old_name() {}").unwrap();
    let path = path.to_string_lossy().into_owned();

    let unclaimed = translator
        .handle_type_hierarchy_prepare(client_path(path.clone()), pos(1, 1))
        .await;
    assert_matches!(
        unclaimed,
        Err(Error::NoServerForTool {
            tool: ToolKind::TypeHierarchy,
            ..
        })
    );

    let translator = Arc::new(translator);
    let handle = {
        let translator = Arc::clone(&translator);
        tokio::spawn(async move {
            translator
                .handle_prepare_rename(client_path(path), pos(1, 5))
                .await
        })
    };
    let mut wire = BufReader::new(&mut renamer.write_stdout);
    assert_eq!(
        read_framed_message(&mut wire).await["method"],
        "textDocument/didOpen"
    );
    let request = read_framed_message(&mut wire).await;
    assert_eq!(request["method"], "textDocument/prepareRename");
    write_response(
        &mut renamer.read_half_stdin,
        &request["id"],
        serde_json::Value::Null,
    )
    .await;
    let result = timeout(Duration::from_secs(30), handle)
        .await
        .expect("handler call should not hang")
        .unwrap()
        .unwrap();
    assert_matches!(result.outcome, PrepareRenameOutcome::NotRenameable { .. });
}

async fn format_range_with(
    source: &str,
    encoding: Option<lsp_types::PositionEncodingKind>,
    response: serde_json::Value,
) -> (serde_json::Value, FormatDocumentResult) {
    let dir = TempDir::new().unwrap();
    let id = ServerId::from_static("rust");
    let (translator, mut server) = encoding.map_or_else(
        || translator_with_capabilities(&dir, &id, range_caps()),
        |encoding| translator_with_capabilities_and_encoding(&dir, &id, range_caps(), encoding),
    );
    let path = dir.path().join("a.rs");
    fs::write(&path, source).unwrap();

    let translator = Arc::new(translator);
    let handle = {
        let translator = Arc::clone(&translator);
        let path = path.to_string_lossy().into_owned();
        tokio::spawn(async move {
            translator
                .handle_format_range(
                    client_path(path),
                    bounded(pos(2, 1), pos(3, 6)),
                    TabSize::try_from(2).unwrap(),
                    false,
                )
                .await
        })
    };
    let mut wire = BufReader::new(&mut server.write_stdout);
    assert_eq!(
        read_framed_message(&mut wire).await["method"],
        "textDocument/didOpen"
    );
    let request = read_framed_message(&mut wire).await;
    assert_eq!(request["method"], "textDocument/rangeFormatting");
    write_response(&mut server.read_half_stdin, &request["id"], response).await;

    let result = timeout(Duration::from_secs(30), handle)
        .await
        .expect("handler call should not hang")
        .unwrap()
        .unwrap();
    (request, result)
}

#[tokio::test]
async fn format_range_sends_zero_based_range_and_options() {
    let (request, result) = format_range_with(
        "a\nb  \nc  d\n",
        None,
        serde_json::json!([{
            "range": {
                "start": {"line": 1, "character": 1},
                "end": {"line": 1, "character": 3}
            },
            "newText": ""
        }]),
    )
    .await;
    let params = &request["params"];
    assert_eq!(
        params["range"]["start"],
        serde_json::json!({"line": 1, "character": 0})
    );
    assert_eq!(
        params["range"]["end"],
        serde_json::json!({"line": 2, "character": 5})
    );
    assert_eq!(params["options"]["tabSize"], 2);
    assert_eq!(params["options"]["insertSpaces"], false);
    assert_eq!(result.edits.len(), 1);
    assert_eq!(
        result.edits[0].range.start,
        Position2D {
            line: 2,
            character: 2
        }
    );
}

#[tokio::test]
async fn format_range_converts_utf8_edit_columns() {
    // "é" is 2 UTF-8 bytes: byte offset 3 on line 2 is character index 2.
    let (_, result) = format_range_with(
        "a\né b\nc",
        Some(lsp_types::PositionEncodingKind::UTF8),
        serde_json::json!([{
            "range": {
                "start": {"line": 1, "character": 3},
                "end": {"line": 1, "character": 4}
            },
            "newText": "_"
        }]),
    )
    .await;
    assert_eq!(result.edits[0].range.start.character, 3);
    assert_eq!(result.positions_degraded, None);
}

#[tokio::test]
async fn format_range_null_response_has_no_edits() {
    let (_, result) = format_range_with("a\nb\nc", None, serde_json::Value::Null).await;
    assert!(result.edits.is_empty());
}

#[tokio::test]
async fn format_range_with_a_line_beyond_the_document_is_rejected_before_the_request() {
    let dir = TempDir::new().unwrap();
    let (translator, path, _server) = tracked_translator(&dir, range_caps(), "a\nb\n");
    let client_file = || client_path(path.to_string_lossy().into_owned());

    for (start, end, line) in [(pos(1, 1), pos(9, 1), 9), (pos(7, 1), pos(8, 1), 7)] {
        let err = timeout(
            Duration::from_secs(5),
            translator.handle_format_range(
                client_file(),
                bounded(start, end),
                TabSize::default(),
                true,
            ),
        )
        .await
        .expect("must fail before any LSP round-trip")
        .unwrap_err();
        assert_matches!(err, Error::PositionBeyondDocument { line: l, .. } if l.get() == line);
    }
}

#[tokio::test]
async fn format_range_end_character_past_the_line_end_is_forwarded() {
    let dir = TempDir::new().unwrap();
    let (translator, mut server) =
        translator_with_capabilities(&dir, &ServerId::from_static("rust"), range_caps());
    let path = dir.path().join("a.rs");
    fs::write(&path, "a\nb\nc").unwrap();
    let translator = Arc::new(translator);
    let handle = {
        let translator = Arc::clone(&translator);
        let path = path.to_string_lossy().into_owned();
        tokio::spawn(async move {
            translator
                .handle_format_range(
                    client_path(path),
                    bounded(pos(1, 1), pos(2, 999)),
                    TabSize::default(),
                    true,
                )
                .await
        })
    };
    let mut wire = BufReader::new(&mut server.write_stdout);
    assert_eq!(
        read_framed_message(&mut wire).await["method"],
        "textDocument/didOpen"
    );
    let request = read_framed_message(&mut wire).await;
    assert_eq!(request["method"], "textDocument/rangeFormatting");
    assert_eq!(
        request["params"]["range"]["end"],
        serde_json::json!({"line": 1, "character": 998})
    );
    write_response(
        &mut server.read_half_stdin,
        &request["id"],
        serde_json::Value::Null,
    )
    .await;
    timeout(Duration::from_secs(30), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn format_range_without_capability_is_rejected() {
    let dir = TempDir::new().unwrap();
    let (translator, _server) = translator_with_capabilities(
        &dir,
        &ServerId::from_static("rust"),
        lsp_types::ServerCapabilities::default(),
    );
    let path = dir.path().join("a.rs");
    fs::write(&path, "a\nb\n").unwrap();
    let err = translator
        .handle_format_range(
            client_path(path.to_string_lossy().into_owned()),
            bounded(pos(1, 1), pos(2, 1)),
            TabSize::default(),
            true,
        )
        .await
        .unwrap_err();
    assert_matches!(err, Error::CapabilityNotSupported { .. });
}
