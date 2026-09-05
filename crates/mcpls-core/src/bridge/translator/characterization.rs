//! Characterization tests for the `lsp-types` -> `gen-lsp-types` migration (#297).
//!
//! These tests pin the real wire behavior of `lsp_types::` union enums and
//! `Uri` as observed against the dependency in place at the time this file
//! was written, per the approved migration plan. They assert JSON key sets
//! (not just Rust-level round-trip equality) so a later commit that silently
//! starts populating an LSP 3.18-only field would fail here.
//!
//! Invariant across the migration: these fixtures and their expected JSON
//! values must not change once the dependency is swapped -- only the
//! `lsp_types::` variant/type names referenced below may be updated to
//! their `gen-lsp-types` equivalents. The one exception is
//! `test_uri_parse_rejects_raw_unencoded_space` (now
//! `test_uri_parse_accepts_raw_unencoded_space`), which pins a deliberate,
//! documented behavior change (see its doc comment and the CHANGELOG entry
//! for #297): `gen-lsp-types`'s `Uri` has no validating parse at all.

// This file's purpose is characterizing wire shapes that include
// deliberately-superseded-but-still-live LSP fields/types (`MarkedString`,
// `SymbolInformation.deprecated`/`DocumentSymbol.deprecated`) -- servers may
// still send them, so mcpls must still round-trip them correctly.
#![allow(deprecated)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use lsp_types::{
    CodeAction, CodeActionResponse, Command, CompletionItem, CompletionList, CompletionResponse,
    Contents, Diagnostic, DiagnosticSeverity, DocumentChange, DocumentSymbol,
    DocumentSymbolResponse, Documentation, Hover, Location, LocationLink, MarkedString,
    MarkedStringWithLanguage, MarkupContent, MarkupKind, Position, Range, RenameFile,
    SymbolInformation, SymbolKind, TextDocumentContentChangeEvent,
    TextDocumentContentChangeWholeDocument, TextDocumentEdit, TextEdit, Uri,
};
use serde_json::json;

fn pos(line: u32, character: u32) -> Position {
    Position { line, character }
}

fn range(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
    Range {
        start: pos(sl, sc),
        end: pos(el, ec),
    }
}

fn uri(s: &str) -> Uri {
    Uri::from(s)
}

// ---------------------------------------------------------------------
// DefinitionResponse (definition / implementation / typeDefinition)
// ---------------------------------------------------------------------

/// C1 net: the link-list shape (what rust-analyzer sends when the client
/// advertises `link_support: true`, as mcpls does) must serialize as a flat
/// JSON array of `LocationLink`-shaped objects, and a raw JSON array in that
/// exact shape must deserialize back into that variant -- not fail, and not
/// get silently absorbed by the plain-location shapes.
#[test]
fn test_goto_definition_response_link_json_is_flat_array_of_location_links() {
    let response = lsp_types::DefinitionResponse::DefinitionLinkList(vec![LocationLink {
        origin_selection_range: None,
        target_uri: uri("file:///target.rs"),
        target_range: range(0, 0, 0, 10),
        target_selection_range: range(0, 0, 0, 5),
    }]);

    let value = serde_json::to_value(&response).unwrap();
    let array = value
        .as_array()
        .expect("Link must serialize as a JSON array");
    assert_eq!(array.len(), 1);
    let item = array[0].as_object().unwrap();
    assert!(item.contains_key("targetUri"));
    assert!(item.contains_key("targetRange"));
    assert!(item.contains_key("targetSelectionRange"));
    assert!(
        !item.contains_key("originSelectionRange"),
        "None optional field must be omitted, not null"
    );

    // The actual regression net: a raw LocationLink[] JSON payload (what a
    // real server sends) must deserialize into the link-list variant, not
    // the plain-location shapes.
    let raw = json!([{
        "targetUri": "file:///target.rs",
        "targetRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 10}},
        "targetSelectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
    }]);
    let parsed: Option<lsp_types::DefinitionResponse> = serde_json::from_value(raw).unwrap();
    match parsed {
        Some(lsp_types::DefinitionResponse::DefinitionLinkList(links)) => {
            assert_eq!(links.len(), 1);
            assert_eq!(links[0].target_uri.as_ref(), "file:///target.rs");
        }
        other => panic!("expected DefinitionLinkList variant, got {other:?}"),
    }
}

#[test]
fn test_goto_definition_response_scalar_json_is_plain_location_object() {
    let response =
        lsp_types::DefinitionResponse::Definition(lsp_types::Definition::Location(Location {
            uri: uri("file:///a.rs"),
            range: range(1, 0, 1, 5),
        }));
    let value = serde_json::to_value(&response).unwrap();
    let object = value
        .as_object()
        .expect("Scalar must serialize as a plain object");
    assert!(object.contains_key("uri"));
    assert!(object.contains_key("range"));
}

#[test]
fn test_goto_definition_response_array_json_is_flat_array_of_locations() {
    let response =
        lsp_types::DefinitionResponse::Definition(lsp_types::Definition::LocationList(vec![
            Location {
                uri: uri("file:///a.rs"),
                range: range(1, 0, 1, 5),
            },
        ]));
    let value = serde_json::to_value(&response).unwrap();
    let array = value
        .as_array()
        .expect("Array must serialize as a JSON array");
    assert_eq!(array.len(), 1);
    assert!(array[0].as_object().unwrap().contains_key("uri"));
}

// ---------------------------------------------------------------------
// Uri
//
// `gen_lsp_types::Uri` is a plain, unvalidated `String` newtype: any
// `Uri::from(s).as_ref() == s` assertion is a tautology about the wrapper,
// not a regression test, so it is not used here as a stand-in for real
// protection. The real protection against a malformed/relative/reserved-char
// path becoming a bad document URI lives in `bridge::state::try_path_to_uri`
// / `uri_to_path` (see their own tests in `bridge/state.rs`, e.g.
// `test_try_path_to_uri_returns_none_for_relative_path`,
// `test_path_to_uri_percent_encodes_all_rfc3986_other_reserved_chars`) --
// the two round-trip tests below exercise that real path-conversion layer
// with non-ASCII/space inputs neither of which `bridge/state.rs` already
// covers on non-Windows.
// ---------------------------------------------------------------------

/// Non-ASCII filesystem path through the real conversion layer:
/// `try_path_to_uri` must percent-encode it, and `uri_to_path` must recover
/// the exact original `Path`.
#[test]
fn test_try_path_to_uri_round_trips_non_ascii_path() {
    let path = std::path::Path::new("/tmp/café/main.rs");
    let uri = crate::bridge::try_path_to_uri(path).expect("valid absolute path");
    assert!(
        uri.as_ref().contains("%C3%A9"),
        "non-ASCII byte must be percent-encoded, got {}",
        uri.as_ref()
    );
    assert_eq!(crate::bridge::uri_to_path(&uri).as_deref(), Some(path));
}

/// A space in a filesystem path through the real conversion layer: must be
/// percent-encoded on the way out and recovered exactly on the way back.
#[test]
fn test_try_path_to_uri_round_trips_space_in_path() {
    let path = std::path::Path::new("/tmp/my file.rs");
    let uri = crate::bridge::try_path_to_uri(path).expect("valid absolute path");
    assert!(
        uri.as_ref().contains("%20"),
        "space must be percent-encoded, got {}",
        uri.as_ref()
    );
    assert_eq!(crate::bridge::uri_to_path(&uri).as_deref(), Some(path));
}

/// R1 (documented behavior change, see the migration's CHANGELOG entry for
/// #297): under `gluon-lang/lsp-types`, an unencoded literal space was
/// rejected by strict URI parsing (`fluent_uri`). `gen-lsp-types`'s `Uri` is
/// a plain string newtype with no validation at all, so the same raw input
/// is now accepted unconditionally at the `Uri` type level -- deliberate,
/// not a regression to silently paper over. What keeps this from becoming
/// an actual document-URI defect is that mcpls itself never constructs a
/// `Uri` from an un-encoded string: `try_path_to_uri` always percent-encodes
/// first (proven by the round-trip test above), so this test's own
/// construction path -- `try_path_to_uri` on a real path with a space --
/// never reaches the state this test pins as "now accepted, not rejected".
#[test]
fn test_uri_parse_accepts_raw_unencoded_space() {
    let raw = uri("file:///tmp/my file.rs");
    assert_eq!(raw.as_ref(), "file:///tmp/my file.rs");

    let path = std::path::Path::new("/tmp/my file.rs");
    let via_try_path_to_uri = crate::bridge::try_path_to_uri(path).expect("valid absolute path");
    assert_ne!(
        via_try_path_to_uri.as_ref(),
        raw.as_ref(),
        "mcpls's own construction path must never produce the raw, unencoded form this test pins"
    );
}

// ---------------------------------------------------------------------
// WorkspaceEdit.document_changes (both shapes)
// ---------------------------------------------------------------------

#[test]
fn test_document_changes_edits_shape_is_flat_array_of_text_document_edits() {
    let changes: Vec<DocumentChange> = vec![DocumentChange::TextDocumentEdit(TextDocumentEdit {
        text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
            version: Some(1),
            text_document_identifier: lsp_types::TextDocumentIdentifier {
                uri: uri("file:///a.rs"),
            },
        },
        edits: vec![lsp_types::Edit::TextEdit(TextEdit {
            range: range(0, 0, 0, 1),
            new_text: "x".to_string(),
        })],
    })];
    let value = serde_json::to_value(&changes).unwrap();
    let array = value
        .as_array()
        .expect("Edits must serialize as a flat array");
    assert_eq!(array.len(), 1);
    let item = array[0].as_object().unwrap();
    assert!(item.contains_key("textDocument"));
    assert!(item.contains_key("edits"));
    assert!(
        !item.contains_key("kind"),
        "a plain text-document edit must carry no resource-operation discriminant"
    );
}

#[test]
fn test_document_changes_operations_shape_is_flat_array_with_kind_discriminant() {
    let changes: Vec<DocumentChange> = vec![DocumentChange::RenameFile(RenameFile {
        old_uri: uri("file:///old.rs"),
        new_uri: uri("file:///new.rs"),
        options: None,
        annotation_id: None,
    })];
    let value = serde_json::to_value(&changes).unwrap();
    let array = value
        .as_array()
        .expect("Operations must serialize as a flat array");
    assert_eq!(array.len(), 1);
    let item = array[0].as_object().unwrap();
    assert_eq!(item.get("kind").and_then(|v| v.as_str()), Some("rename"));
    assert!(item.contains_key("oldUri"));
    assert!(item.contains_key("newUri"));
}

// ---------------------------------------------------------------------
// Hover.contents (all three variants)
// ---------------------------------------------------------------------

#[test]
#[allow(deprecated)]
fn test_hover_contents_scalar_string_is_plain_json_string() {
    let hover = Hover {
        contents: Contents::MarkedString(MarkedString::String("plain".to_string())),
        range: None,
    };
    let value = serde_json::to_value(&hover).unwrap();
    assert_eq!(value["contents"], json!("plain"));
}

#[test]
#[allow(deprecated)]
fn test_hover_contents_scalar_language_string_json_shape() {
    let hover = Hover {
        contents: Contents::MarkedString(MarkedString::MarkedStringWithLanguage(
            MarkedStringWithLanguage {
                language: "rust".to_string(),
                value: "fn main() {}".to_string(),
            },
        )),
        range: None,
    };
    let value = serde_json::to_value(&hover).unwrap();
    let contents = value["contents"].as_object().unwrap();
    assert!(contents.contains_key("language"));
    assert!(contents.contains_key("value"));
}

#[test]
#[allow(deprecated)]
fn test_hover_contents_array_json_is_flat_array() {
    let hover = Hover {
        contents: Contents::MarkedStringList(vec![
            MarkedString::String("a".to_string()),
            MarkedString::String("b".to_string()),
        ]),
        range: None,
    };
    let value = serde_json::to_value(&hover).unwrap();
    let array = value["contents"]
        .as_array()
        .expect("Array must serialize as a JSON array");
    assert_eq!(array.len(), 2);
}

#[test]
fn test_hover_contents_markup_json_shape() {
    let hover = Hover {
        contents: Contents::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: "# Doc".to_string(),
        }),
        range: None,
    };
    let value = serde_json::to_value(&hover).unwrap();
    let contents = value["contents"].as_object().unwrap();
    assert!(contents.contains_key("kind"));
    assert!(contents.contains_key("value"));
}

// ---------------------------------------------------------------------
// CodeActionResponse (single code-action-or-command element)
// ---------------------------------------------------------------------

#[test]
fn test_code_action_or_command_code_action_json_shape() {
    let value = serde_json::to_value(CodeActionResponse::CodeAction(CodeAction {
        title: "Fix".to_string(),
        kind: None,
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: None,
        disabled: None,
        tags: None,
        data: None,
    }))
    .unwrap();
    let object = value.as_object().unwrap();
    assert!(object.contains_key("title"));
    assert!(
        !object.contains_key("command"),
        "None optional fields must be omitted"
    );
}

#[test]
fn test_code_action_or_command_command_json_shape() {
    let value = serde_json::to_value(CodeActionResponse::Command(Command {
        title: "Run".to_string(),
        command: "run.it".to_string(),
        arguments: None,
        tooltip: None,
    }))
    .unwrap();
    let object = value.as_object().unwrap();
    assert!(object.contains_key("title"));
    assert!(object.contains_key("command"));
}

// ---------------------------------------------------------------------
// CompletionResponse
// ---------------------------------------------------------------------

#[test]
fn test_completion_response_array_json_is_flat_array() {
    let value = serde_json::to_value(CompletionResponse::CompletionItemList(vec![
        CompletionItem {
            label: "foo".to_string(),
            ..Default::default()
        },
    ]))
    .unwrap();
    assert!(value.is_array());
}

#[test]
fn test_completion_response_list_json_is_object_with_items() {
    let value = serde_json::to_value(CompletionResponse::CompletionList(CompletionList {
        is_incomplete: false,
        items: vec![CompletionItem {
            label: "foo".to_string(),
            ..Default::default()
        }],
        item_defaults: None,
        apply_kind: None,
    }))
    .unwrap();
    let object = value.as_object().unwrap();
    assert!(object.contains_key("isIncomplete"));
    assert!(object.contains_key("items"));
}

// ---------------------------------------------------------------------
// DocumentDiagnosticReport (both shapes, tagged via `kind`)
// ---------------------------------------------------------------------

#[test]
fn test_document_diagnostic_report_full_json_has_kind_full() {
    use lsp_types::{
        DocumentDiagnosticReport, FullDocumentDiagnosticReport, RelatedFullDocumentDiagnosticReport,
    };

    let report = DocumentDiagnosticReport::RelatedFullDocumentDiagnosticReport(
        RelatedFullDocumentDiagnosticReport {
            related_documents: None,
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                result_id: None,
                items: vec![],
            },
        },
    );
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["kind"], json!("full"));
    assert!(value.as_object().unwrap().contains_key("items"));
}

#[test]
fn test_document_diagnostic_report_unchanged_json_has_kind_unchanged() {
    use lsp_types::{
        DocumentDiagnosticReport, RelatedUnchangedDocumentDiagnosticReport,
        UnchangedDocumentDiagnosticReport,
    };

    let report = DocumentDiagnosticReport::RelatedUnchangedDocumentDiagnosticReport(
        RelatedUnchangedDocumentDiagnosticReport {
            related_documents: None,
            unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                result_id: "1".to_string(),
            },
        },
    );
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["kind"], json!("unchanged"));
}

// ---------------------------------------------------------------------
// DocumentSymbolResponse
// ---------------------------------------------------------------------

#[test]
fn test_document_symbol_response_flat_json_is_array_of_symbol_information() {
    let symbol = SymbolInformation {
        deprecated: None,
        location: Location {
            uri: uri("file:///a.rs"),
            range: range(0, 0, 0, 3),
        },
        base_symbol_information: lsp_types::BaseSymbolInformation {
            name: "foo".to_string(),
            kind: SymbolKind::Function,
            tags: None,
            container_name: None,
        },
    };
    let value =
        serde_json::to_value(DocumentSymbolResponse::SymbolInformationList(vec![symbol])).unwrap();
    let array = value.as_array().unwrap();
    let item = array[0].as_object().unwrap();
    assert!(item.contains_key("location"));
    assert!(
        !item.contains_key("range"),
        "flat symbols have no top-level range"
    );
}

#[test]
fn test_document_symbol_response_nested_json_is_array_of_document_symbol() {
    let symbol = DocumentSymbol {
        name: "foo".to_string(),
        detail: None,
        kind: SymbolKind::Function,
        tags: None,
        deprecated: None,
        range: range(0, 0, 0, 3),
        selection_range: range(0, 0, 0, 3),
        children: None,
    };
    let value =
        serde_json::to_value(DocumentSymbolResponse::DocumentSymbolList(vec![symbol])).unwrap();
    let array = value.as_array().unwrap();
    let item = array[0].as_object().unwrap();
    assert!(item.contains_key("range"));
    assert!(item.contains_key("selectionRange"));
    assert!(
        !item.contains_key("location"),
        "nested symbols carry no top-level location"
    );
}

// ---------------------------------------------------------------------
// Documentation
// ---------------------------------------------------------------------

#[test]
fn test_documentation_string_is_plain_json_string() {
    let value = serde_json::to_value(Documentation::String("doc".to_string())).unwrap();
    assert_eq!(value, json!("doc"));
}

#[test]
fn test_documentation_markup_content_json_shape() {
    let value = serde_json::to_value(Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value: "doc".to_string(),
    }))
    .unwrap();
    let object = value.as_object().unwrap();
    assert!(object.contains_key("kind"));
    assert!(object.contains_key("value"));
}

// ---------------------------------------------------------------------
// TextDocumentContentChangeEvent (full-document replacement shape used by
// `DocumentTracker::sync_phase`)
// ---------------------------------------------------------------------

#[test]
fn test_text_document_content_change_event_full_replace_json_shape() {
    let event = TextDocumentContentChangeEvent::TextDocumentContentChangeWholeDocument(
        TextDocumentContentChangeWholeDocument {
            text: "new content".to_string(),
        },
    );
    let value = serde_json::to_value(&event).unwrap();
    let object = value.as_object().unwrap();
    assert_eq!(
        object.len(),
        1,
        "a full-document replace must carry only `text` on the wire"
    );
    assert!(object.contains_key("text"));
}

// ---------------------------------------------------------------------
// Diagnostic.severity / .code (sanity: these are consumed by
// `diagnostic_to_mcp`, exercised indirectly by `diagnostics.rs` tests, but
// pinned here at the JSON level too since severity/code enum-vs-int wire
// shape is exactly the class this migration can silently break)
// ---------------------------------------------------------------------

#[test]
fn test_diagnostic_severity_serializes_as_integer() {
    let diagnostic = Diagnostic {
        range: range(0, 0, 0, 1),
        severity: Some(DiagnosticSeverity::Error),
        code: None,
        code_description: None,
        source: None,
        message: "x".to_string().into(),
        related_information: None,
        tags: None,
        data: None,
    };
    let value = serde_json::to_value(&diagnostic).unwrap();
    assert_eq!(value["severity"], json!(1));
}
