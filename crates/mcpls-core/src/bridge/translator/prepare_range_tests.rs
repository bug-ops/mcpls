//! Fake-server tests for `handle_prepare_rename` and `handle_format_range`.

use std::sync::Arc;
use std::time::Duration;
use std::{assert_matches, fs};

use tempfile::TempDir;
use tokio::io::BufReader;
use tokio::time::timeout;

use super::Translator;
use super::dto::{FormatDocumentResult, Position2D, PrepareRenameOutcome, PrepareRenameResult};
use super::testing::*;
use crate::bridge::{IndexingPolicy, WorkspaceRoots};
use crate::config::{LanguageId, LspServerConfig, ServerId, TimeoutSecs, ToolKind, ToolRouter};
use crate::error::{Error, McpErrorKind, Result};
use crate::lsp::LspServer;
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
    let dir = TempDir::new().unwrap();
    let (translator, mut server) =
        translator_with_capabilities(&dir, &ServerId::from("rust"), prepare_caps());
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
/// a conscious one.
#[tokio::test]
async fn prepare_rename_clangd_no_symbol_error_stays_a_server_error() {
    let err = prepare_rename_with(Answer::Error(
        -32001,
        "Cannot rename symbol: there is no symbol at the given location",
    ))
    .await
    .unwrap_err();
    assert_matches!(err, Error::LspServerError { code: -32001, .. });
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
    let (translator, _server) = translator_with_capabilities(&dir, &ServerId::from("rust"), caps);
    let path = dir.path().join("a.rs");
    fs::write(&path, "fn old_name() {}").unwrap();

    let err = translator
        .handle_prepare_rename(client_path(path.to_string_lossy().into_owned()), pos(1, 5))
        .await
        .unwrap_err();
    assert_matches!(err, Error::CapabilityNotSupported { .. });
}

#[tokio::test]
async fn prepare_rename_rejects_zero_position() {
    let result = Translator::new()
        .handle_prepare_rename(client_path("a.rs"), pos(0, 1))
        .await;
    assert_matches!(result, Err(Error::InvalidToolParams(_)));
}

fn handles_config(name: &str, handles: Vec<ToolKind>) -> LspServerConfig {
    LspServerConfig {
        language_id: LanguageId::from_static("rust"),
        command: name.to_string(),
        args: vec![],
        env: std::collections::HashMap::new(),
        file_patterns: vec![],
        initialization_options: None,
        settings: None,
        timeout_seconds: TimeoutSecs::new(30).unwrap(),
        request_timeout_seconds: TimeoutSecs::new(30).unwrap(),
        heuristics: None,
        name: Some(name.to_string()),
        handles: Some(handles),
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
            "rs".to_string(),
            "rust".to_string(),
        )]))
        .with_router(ToolRouter::from_configs(&configs).unwrap());
    translator
        .set_workspace_roots(WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap());

    let (renamer_client, mut renamer) = fake_lsp_client();
    let (hoverer_client, _hoverer) = fake_lsp_client();
    for (id, client) in [("renamer", renamer_client), ("hoverer", hoverer_client)] {
        translator.register_client(ServerId::from(id), client);
        translator.register_server(ServerId::from(id), LspServer::new_for_test(prepare_caps()));
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
    let id = ServerId::from("rust");
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
                .handle_format_range(client_path(path), pos(2, 1), pos(3, 6), 2, false)
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
async fn format_range_rejects_reversed_and_zero_ranges() {
    let translator = Translator::new();
    for (start, end) in [
        (pos(3, 1), pos(2, 1)),
        (pos(2, 5), pos(2, 4)),
        (pos(0, 1), pos(2, 1)),
    ] {
        let result = translator
            .handle_format_range(client_path("a.rs"), start, end, 4, true)
            .await;
        assert_matches!(result, Err(Error::InvalidToolParams(_)));
    }
}

#[tokio::test]
async fn format_range_without_capability_is_rejected() {
    let dir = TempDir::new().unwrap();
    let (translator, _server) = translator_with_capabilities(
        &dir,
        &ServerId::from("rust"),
        lsp_types::ServerCapabilities::default(),
    );
    let path = dir.path().join("a.rs");
    fs::write(&path, "a\nb\n").unwrap();
    let err = translator
        .handle_format_range(
            client_path(path.to_string_lossy().into_owned()),
            pos(1, 1),
            pos(2, 1),
            4,
            true,
        )
        .await
        .unwrap_err();
    assert_matches!(err, Error::CapabilityNotSupported { .. });
}
