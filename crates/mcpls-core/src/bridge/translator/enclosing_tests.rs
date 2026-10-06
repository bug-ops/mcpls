//! Tests for the opt-in enclosing-symbol enrichment.

use std::sync::Arc;
use std::time::Duration;
use std::{assert_matches, fs};

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::BufReader;
use tokio::sync::Mutex;
use tokio::time::timeout;
use url::Url;

use super::dto::{Position2D, Range, Symbol};
use super::enclosing::{
    EnclosingSymbol, EnclosingSymbolOutcome, NotComputedReason, ResultContext, SymbolFidelity,
    UnavailableReason, innermost_flat, innermost_hierarchical,
};
use super::symbols::FlatSymbol;
use super::testing::*;
use crate::bridge::NotificationCache;
use crate::bridge::state::ResourceLimits;
use crate::config::{DocumentLimit, ServerId};
use crate::test_lsp::client_path;

fn at(line: u32, character: u32) -> Position2D {
    Position2D { line, character }
}

fn range(start: (u32, u32), end: (u32, u32)) -> Range {
    Range {
        start: at(start.0, start.1),
        end: at(end.0, end.1),
    }
}

fn symbol(name: &str, span: Range, children: Option<Vec<Symbol>>) -> Symbol {
    Symbol {
        name: name.to_string(),
        kind: 12,
        selection_range: span.clone(),
        range: span,
        children,
    }
}

fn name_path(found: Option<EnclosingSymbol>) -> Vec<String> {
    found.expect("an enclosing symbol").name_path
}

#[test]
fn test_hierarchical_picks_innermost_and_builds_ancestor_path() {
    let tree = vec![symbol(
        "S",
        range((1, 1), (10, 1)),
        Some(vec![
            symbol("a", range((2, 1), (4, 1)), None),
            symbol("b", range((5, 1), (9, 1)), None),
        ]),
    )];

    assert_eq!(
        name_path(innermost_hierarchical(&tree, &range((6, 3), (6, 5)))),
        ["S", "b"]
    );
    assert_eq!(
        name_path(innermost_hierarchical(&tree, &range((4, 5), (4, 6)))),
        ["S"]
    );
    assert!(innermost_hierarchical(&tree, &range((11, 1), (11, 2))).is_none());
}

#[test]
fn test_hierarchical_straddling_range_falls_back_to_start_containment() {
    let tree = vec![
        symbol("a", range((1, 1), (3, 1)), None),
        symbol("b", range((4, 1), (6, 1)), None),
    ];

    assert_eq!(
        name_path(innermost_hierarchical(&tree, &range((2, 1), (5, 1)))),
        ["a"]
    );
}

#[test]
fn test_identical_ranges_resolve_independently_of_server_order() {
    let span = range((1, 1), (3, 1));
    let forward = vec![
        symbol("a", span.clone(), None),
        symbol("b", span.clone(), None),
    ];
    let backward = vec![symbol("b", span.clone(), None), symbol("a", span, None)];
    let hit = range((2, 1), (2, 2));

    assert_eq!(
        innermost_hierarchical(&forward, &hit),
        innermost_hierarchical(&backward, &hit)
    );
}

#[test]
fn test_flat_picks_smallest_range_and_uses_container_name() {
    let flat = |name: &str, span: Range, container: Option<&str>| FlatSymbol {
        symbol: symbol(name, span, None),
        container_name: container.map(str::to_string),
    };
    let tree = vec![
        flat("S", range((1, 1), (10, 1)), None),
        flat("a", range((2, 1), (4, 1)), Some("S")),
        flat("b", range((5, 1), (6, 1)), Some("")),
    ];

    let found = innermost_flat(&tree, &range((3, 1), (3, 2))).unwrap();
    assert_eq!(found.name_path, ["S", "a"]);
    assert_eq!(found.fidelity, SymbolFidelity::Flat);
    assert_eq!(
        name_path(innermost_flat(&tree, &range((5, 2), (5, 3)))),
        ["b"]
    );
}

fn full_caps() -> lsp_types::ServerCapabilities {
    lsp_types::ServerCapabilities {
        references_provider: Some(lsp_types::ReferencesProvider::Bool(true)),
        definition_provider: Some(lsp_types::DefinitionProvider::Bool(true)),
        implementation_provider: Some(lsp_types::ImplementationProvider::Bool(true)),
        type_definition_provider: Some(lsp_types::TypeDefinitionProvider::Bool(true)),
        declaration_provider: Some(lsp_types::DeclarationProvider::Bool(true)),
        document_symbol_provider: Some(lsp_types::DocumentSymbolProvider::Bool(true)),
        ..Default::default()
    }
}

fn lsp_range(start: (u32, u32), end: (u32, u32)) -> Value {
    json!({
        "start": {"line": start.0, "character": start.1},
        "end": {"line": end.0, "character": end.1},
    })
}

fn location(uri: &str, line: u32) -> Value {
    json!({"uri": uri, "range": lsp_range((line, 2), (line, 4))})
}

fn document_symbol(name: &str, span: &Value, children: &[Value]) -> Value {
    json!({
        "name": name,
        "kind": 12,
        "range": span,
        "selectionRange": span,
        "children": children,
    })
}

fn file_uri(path: &std::path::Path) -> String {
    Url::from_file_path(path).unwrap().to_string()
}

struct Fixture {
    dir: TempDir,
    translator: Arc<super::Translator>,
    server: FakeServer,
}

fn fixture(caps: lsp_types::ServerCapabilities, limits: Option<ResourceLimits>) -> Fixture {
    let dir = TempDir::new().unwrap();
    let (translator, server) =
        translator_with_capabilities(&dir, &ServerId::from_static("rust"), caps);
    let translator = match limits {
        Some(limits) => translator.with_resource_limits(limits),
        None => translator,
    };
    Fixture {
        dir,
        translator: Arc::new(translator),
        server,
    }
}

impl Fixture {
    fn write(&self, name: &str, text: &str) -> std::path::PathBuf {
        let path = canonical_dir(&self.dir).join(name);
        fs::write(&path, text).unwrap();
        path
    }
}

async fn expect_no_more_requests(server: &mut FakeServer) {
    let mut wire = BufReader::new(&mut server.write_stdout);
    assert!(
        timeout(Duration::from_millis(150), read_framed_message(&mut wire))
            .await
            .is_err(),
        "no further LSP message expected"
    );
}

async fn expect_request(wire: &mut BufReader<&mut tokio::io::DuplexStream>, method: &str) -> Value {
    let request = read_framed_message(wire).await;
    assert_eq!(request["method"], method);
    request
}

async fn run_references(
    fx: &mut Fixture,
    main: &std::path::Path,
    context: ResultContext,
    references: Value,
    symbol_answers: Vec<(&str, Option<Value>)>,
) -> crate::bridge::ReferencesResult {
    let handle = {
        let translator = Arc::clone(&fx.translator);
        let path = main.to_string_lossy().into_owned();
        tokio::spawn(async move {
            translator
                .handle_references(client_path(path), pos(1, 1), true, context)
                .await
        })
    };
    {
        let mut wire = BufReader::new(&mut fx.server.write_stdout);
        expect_request(&mut wire, "textDocument/didOpen").await;
        let request = expect_request(&mut wire, "textDocument/references").await;
        write_response(&mut fx.server.read_half_stdin, &request["id"], references).await;
        for (uri_hint, answer) in symbol_answers {
            let mut next = read_framed_message(&mut wire).await;
            if next["method"] == "textDocument/didOpen" {
                assert!(
                    next["params"]["textDocument"]["uri"]
                        .as_str()
                        .unwrap()
                        .ends_with(uri_hint)
                );
                next = read_framed_message(&mut wire).await;
            }
            assert_eq!(next["method"], "textDocument/documentSymbol");
            match answer {
                Some(value) => {
                    write_response(&mut fx.server.read_half_stdin, &next["id"], value).await;
                }
                None => {
                    write_error_response(
                        &mut fx.server.read_half_stdin,
                        &next["id"],
                        -32603,
                        "boom",
                    )
                    .await;
                }
            }
        }
    }
    timeout(Duration::from_secs(5), handle)
        .await
        .expect("handler should not hang")
        .unwrap()
        .unwrap()
}

fn outcome(result: &crate::bridge::ReferencesResult, index: usize) -> EnclosingSymbolOutcome {
    result.locations[index].enclosing_symbol.clone().unwrap()
}

#[tokio::test]
async fn test_references_enclosing_symbol_one_request_per_file() {
    let mut fx = fixture(full_caps(), None);
    let main = fx.write(
        "main.rs",
        "impl S {\n fn a() {}\n fn b() {}\n}\nconst X: u8 = 1;\n",
    );
    let uri = file_uri(&main);
    let symbols = json!([document_symbol(
        "S",
        &lsp_range((0, 0), (3, 1)),
        &[
            document_symbol("a", &lsp_range((1, 1), (1, 11)), &[]),
            document_symbol("b", &lsp_range((2, 1), (2, 11)), &[]),
        ],
    )]);

    let result = run_references(
        &mut fx,
        &main,
        ResultContext::EnclosingSymbol,
        json!([location(&uri, 1), location(&uri, 2), location(&uri, 4)]),
        vec![("main.rs", Some(symbols))],
    )
    .await;

    let EnclosingSymbolOutcome::Resolved(first) = outcome(&result, 0) else {
        panic!("expected a resolved symbol");
    };
    assert_eq!(first.name_path, ["S", "a"]);
    assert_eq!(first.fidelity, SymbolFidelity::Hierarchical);
    assert_eq!(first.range.start.line, 2);
    let EnclosingSymbolOutcome::Resolved(second) = outcome(&result, 1) else {
        panic!("expected a resolved symbol");
    };
    assert_eq!(second.name_path, ["S", "b"]);
    assert_eq!(outcome(&result, 2), EnclosingSymbolOutcome::TopLevel);
    let summary = result.enrichment.unwrap();
    assert_eq!(
        (
            summary.files_enriched,
            summary.files_skipped,
            summary.cut_short
        ),
        (1, 0, false)
    );
    expect_no_more_requests(&mut fx.server).await;
}

#[tokio::test]
async fn test_references_default_output_is_unchanged_and_issues_no_symbol_request() {
    let mut fx = fixture(full_caps(), None);
    let main = fx.write("main.rs", "fn main() {}\n");
    let uri = file_uri(&main);

    let result = run_references(
        &mut fx,
        &main,
        ResultContext::None,
        json!([location(&uri, 0)]),
        vec![],
    )
    .await;

    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        json!({
            "locations": [{
                "uri": uri,
                "range": {
                    "start": {"line": 1, "character": 3},
                    "end": {"line": 1, "character": 5},
                },
            }],
        })
    );
    expect_no_more_requests(&mut fx.server).await;
}

#[tokio::test]
async fn test_references_without_document_symbol_capability_is_unavailable() {
    let caps = lsp_types::ServerCapabilities {
        references_provider: Some(lsp_types::ReferencesProvider::Bool(true)),
        ..Default::default()
    };
    let mut fx = fixture(caps, None);
    let main = fx.write("main.rs", "fn main() {}\n");
    let uri = file_uri(&main);

    let result = run_references(
        &mut fx,
        &main,
        ResultContext::EnclosingSymbol,
        json!([location(&uri, 0)]),
        vec![],
    )
    .await;

    assert_eq!(
        outcome(&result, 0),
        EnclosingSymbolOutcome::Unavailable {
            reason: UnavailableReason::CapabilityAbsent
        }
    );
    assert_eq!(result.locations.len(), 1);
    let summary = result.enrichment.unwrap();
    assert_eq!((summary.files_enriched, summary.files_skipped), (0, 1));
    expect_no_more_requests(&mut fx.server).await;
}

#[tokio::test]
async fn test_references_out_of_workspace_location_is_not_opened() {
    let mut fx = fixture(full_caps(), None);
    let main = fx.write("main.rs", "fn main() {}\n");

    let result = run_references(
        &mut fx,
        &main,
        ResultContext::EnclosingSymbol,
        json!([location("file:///outside/workspace/lib.rs", 0)]),
        vec![],
    )
    .await;

    assert_eq!(
        outcome(&result, 0),
        EnclosingSymbolOutcome::NotComputed {
            reason: NotComputedReason::OutOfWorkspace
        }
    );
    assert!(!result.enrichment.unwrap().cut_short);
    expect_no_more_requests(&mut fx.server).await;
}

#[tokio::test]
async fn test_references_file_cap_skips_later_files_and_reports_cut_short() {
    let limits = ResourceLimits {
        max_documents: DocumentLimit::new(4),
        ..ResourceLimits::default()
    };
    let mut fx = fixture(full_caps(), Some(limits));
    let main = fx.write("main.rs", "fn main() {}\n");
    let first = fx.write("a.rs", "fn a() {}\n");
    let second = fx.write("b.rs", "fn b() {}\n");
    let symbols = json!([document_symbol("a", &lsp_range((0, 0), (0, 10)), &[])]);

    let result = run_references(
        &mut fx,
        &main,
        ResultContext::EnclosingSymbol,
        json!([
            location(&file_uri(&first), 0),
            location(&file_uri(&second), 0)
        ]),
        vec![("a.rs", Some(symbols))],
    )
    .await;

    assert_matches!(outcome(&result, 0), EnclosingSymbolOutcome::Resolved(_));
    assert_eq!(
        outcome(&result, 1),
        EnclosingSymbolOutcome::NotComputed {
            reason: NotComputedReason::FileCap
        }
    );
    let summary = result.enrichment.unwrap();
    assert_eq!(
        (
            summary.files_enriched,
            summary.files_skipped,
            summary.cut_short
        ),
        (1, 1, true)
    );
    expect_no_more_requests(&mut fx.server).await;
}

#[tokio::test]
async fn test_references_flat_symbol_information_uses_container_name() {
    let mut fx = fixture(full_caps(), None);
    let main = fx.write("main.rs", "impl S {\n fn a() {}\n}\n");
    let uri = file_uri(&main);
    let flat = json!([
        {"name": "S", "kind": 5, "location": {"uri": uri, "range": lsp_range((0, 0), (2, 1))}},
        {
            "name": "a", "kind": 12, "containerName": "S",
            "location": {"uri": uri, "range": lsp_range((1, 1), (1, 11))},
        },
    ]);

    let result = run_references(
        &mut fx,
        &main,
        ResultContext::EnclosingSymbol,
        json!([location(&uri, 1)]),
        vec![("main.rs", Some(flat))],
    )
    .await;

    let EnclosingSymbolOutcome::Resolved(found) = outcome(&result, 0) else {
        panic!("expected a resolved symbol");
    };
    assert_eq!(found.name_path, ["S", "a"]);
    assert_eq!(found.fidelity, SymbolFidelity::Flat);
}

#[tokio::test]
async fn test_references_failed_symbol_request_keeps_primary_result() {
    let mut fx = fixture(full_caps(), None);
    let main = fx.write("main.rs", "fn main() {}\n");
    let uri = file_uri(&main);

    let result = run_references(
        &mut fx,
        &main,
        ResultContext::EnclosingSymbol,
        json!([location(&uri, 0)]),
        vec![("main.rs", None)],
    )
    .await;

    assert_eq!(result.locations.len(), 1);
    assert_eq!(
        outcome(&result, 0),
        EnclosingSymbolOutcome::Unavailable {
            reason: UnavailableReason::RequestFailed
        }
    );
}

#[tokio::test]
async fn test_diagnostics_enclosing_symbol() {
    let mut fx = fixture(full_caps(), None);
    let main = fx.write("main.rs", "fn a() {\n  bad;\n}\nbad2;\n");
    let cache = Mutex::new(NotificationCache::new());

    let handle = {
        let translator = Arc::clone(&fx.translator);
        let path = main.to_string_lossy().into_owned();
        tokio::spawn(async move {
            translator
                .handle_diagnostics(client_path(path), ResultContext::EnclosingSymbol, &cache)
                .await
        })
    };
    {
        let mut wire = BufReader::new(&mut fx.server.write_stdout);
        expect_request(&mut wire, "textDocument/didOpen").await;
        let pull = expect_request(&mut wire, "textDocument/diagnostic").await;
        let item = |line: u32| json!({"range": lsp_range((line, 0), (line, 3)), "severity": 1, "message": "bad"});
        write_response(
            &mut fx.server.read_half_stdin,
            &pull["id"],
            json!({"kind": "full", "items": [item(1), item(3)]}),
        )
        .await;
        let symbols = expect_request(&mut wire, "textDocument/documentSymbol").await;
        write_response(
            &mut fx.server.read_half_stdin,
            &symbols["id"],
            json!([document_symbol("a", &lsp_range((0, 0), (2, 1)), &[])]),
        )
        .await;
    }
    let result = timeout(Duration::from_secs(5), handle)
        .await
        .expect("handler should not hang")
        .unwrap()
        .unwrap();

    let outcomes: Vec<_> = result
        .diagnostics
        .iter()
        .map(|d| d.enclosing_symbol.clone().unwrap())
        .collect();
    assert_matches!(&outcomes[0], EnclosingSymbolOutcome::Resolved(s) if s.name_path == ["a"]);
    assert_eq!(outcomes[1], EnclosingSymbolOutcome::TopLevel);
    assert_eq!(result.enrichment.unwrap().files_enriched, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn test_references_symlink_escaping_workspace_is_not_opened() {
    let mut fx = fixture(full_caps(), None);
    let main = fx.write("main.rs", "fn main() {}\n");
    let outside = TempDir::new().unwrap();
    let target = canonical_dir(&outside).join("secret.rs");
    fs::write(&target, "fn secret() {}\n").unwrap();
    let link = canonical_dir(&fx.dir).join("link.rs");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let result = run_references(
        &mut fx,
        &main,
        ResultContext::EnclosingSymbol,
        json!([location(&file_uri(&link), 0)]),
        vec![],
    )
    .await;

    assert_eq!(
        outcome(&result, 0),
        EnclosingSymbolOutcome::NotComputed {
            reason: NotComputedReason::OutOfWorkspace
        }
    );
    expect_no_more_requests(&mut fx.server).await;
}

#[test]
fn test_hierarchical_overlapping_siblings_pick_the_innermost_by_range() {
    let tree = vec![
        symbol("Outer", range((1, 1), (10, 1)), None),
        symbol("inner", range((2, 1), (4, 1)), None),
    ];

    assert_eq!(
        name_path(innermost_hierarchical(&tree, &range((3, 1), (3, 2)))),
        ["inner"]
    );
}

#[derive(Clone, Copy)]
enum Goto {
    Definition,
    Implementation,
    TypeDefinition,
    Declaration,
}

impl Goto {
    const ALL: [Self; 4] = [
        Self::Definition,
        Self::Implementation,
        Self::TypeDefinition,
        Self::Declaration,
    ];

    const fn method(self) -> &'static str {
        match self {
            Self::Definition => "textDocument/definition",
            Self::Implementation => "textDocument/implementation",
            Self::TypeDefinition => "textDocument/typeDefinition",
            Self::Declaration => "textDocument/declaration",
        }
    }
}

async fn run_goto(
    fx: &mut Fixture,
    main: &std::path::Path,
    goto: Goto,
    context: ResultContext,
    response: Value,
    symbols: Option<Value>,
) -> Value {
    let handle = {
        let translator = Arc::clone(&fx.translator);
        let path = main.to_string_lossy().into_owned();
        tokio::spawn(async move {
            match goto {
                Goto::Definition => serde_json::to_value(
                    translator
                        .handle_definition(client_path(path), pos(1, 1), context)
                        .await
                        .unwrap(),
                ),
                Goto::Implementation => serde_json::to_value(
                    translator
                        .handle_implementation(client_path(path), pos(1, 1), context)
                        .await
                        .unwrap(),
                ),
                Goto::TypeDefinition => serde_json::to_value(
                    translator
                        .handle_type_definition(client_path(path), pos(1, 1), context)
                        .await
                        .unwrap(),
                ),
                Goto::Declaration => serde_json::to_value(
                    translator
                        .handle_declaration(client_path(path), pos(1, 1), context)
                        .await
                        .unwrap(),
                ),
            }
            .unwrap()
        })
    };
    {
        let mut wire = BufReader::new(&mut fx.server.write_stdout);
        expect_request(&mut wire, "textDocument/didOpen").await;
        let request = expect_request(&mut wire, goto.method()).await;
        write_response(&mut fx.server.read_half_stdin, &request["id"], response).await;
        if let Some(symbols) = symbols {
            let request = expect_request(&mut wire, "textDocument/documentSymbol").await;
            write_response(&mut fx.server.read_half_stdin, &request["id"], symbols).await;
        }
    }
    timeout(Duration::from_secs(5), handle)
        .await
        .expect("handler should not hang")
        .unwrap()
}

#[tokio::test]
async fn test_goto_default_output_is_unchanged_and_issues_no_symbol_request() {
    for goto in Goto::ALL {
        let mut fx = fixture(full_caps(), None);
        let main = fx.write("main.rs", "fn main() {}\n");
        let uri = file_uri(&main);

        let result = run_goto(
            &mut fx,
            &main,
            goto,
            ResultContext::None,
            json!([location(&uri, 0)]),
            None,
        )
        .await;

        assert_eq!(
            result,
            json!({
                "locations": [{
                    "uri": uri,
                    "range": {
                        "start": {"line": 1, "character": 3},
                        "end": {"line": 1, "character": 5},
                    },
                }],
            })
        );
        expect_no_more_requests(&mut fx.server).await;
    }
}

#[tokio::test]
async fn test_goto_enclosing_symbol_resolves_for_every_tool() {
    for goto in Goto::ALL {
        let mut fx = fixture(full_caps(), None);
        let main = fx.write("main.rs", "fn a() {}\n");
        let uri = file_uri(&main);
        let symbols = json!([document_symbol("a", &lsp_range((0, 0), (0, 10)), &[])]);

        let result = run_goto(
            &mut fx,
            &main,
            goto,
            ResultContext::EnclosingSymbol,
            json!([location(&uri, 0)]),
            Some(symbols),
        )
        .await;

        let item = &result["locations"][0]["enclosing_symbol"];
        assert_eq!(item["status"], "resolved");
        assert_eq!(item["name_path"], json!(["a"]));
        assert_eq!(result["enrichment"]["files_enriched"], 1);
        expect_no_more_requests(&mut fx.server).await;
    }
}

#[tokio::test]
async fn test_diagnostics_default_output_has_no_enrichment_and_no_symbol_request() {
    let mut fx = fixture(full_caps(), None);
    let main = fx.write("main.rs", "fn a() {\n  bad;\n}\n");
    let cache = Mutex::new(NotificationCache::new());

    let handle = {
        let translator = Arc::clone(&fx.translator);
        let path = main.to_string_lossy().into_owned();
        tokio::spawn(async move {
            translator
                .handle_diagnostics(client_path(path), ResultContext::None, &cache)
                .await
        })
    };
    {
        let mut wire = BufReader::new(&mut fx.server.write_stdout);
        expect_request(&mut wire, "textDocument/didOpen").await;
        let pull = expect_request(&mut wire, "textDocument/diagnostic").await;
        write_response(
            &mut fx.server.read_half_stdin,
            &pull["id"],
            json!({"kind": "full", "items": [
                {"range": lsp_range((1, 0), (1, 3)), "severity": 1, "message": "bad"},
            ]}),
        )
        .await;
    }
    let result = timeout(Duration::from_secs(5), handle)
        .await
        .expect("handler should not hang")
        .unwrap()
        .unwrap();

    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        json!({
            "diagnostics": [{
                "range": {
                    "start": {"line": 2, "character": 1},
                    "end": {"line": 2, "character": 4},
                },
                "severity": "error",
                "message": "bad",
                "code": null,
            }],
        })
    );
    expect_no_more_requests(&mut fx.server).await;
}

#[tokio::test(start_paused = true)]
async fn test_enrichment_deadline_skips_remaining_files() {
    let mut fx = fixture(full_caps(), None);
    let main = fx.write("main.rs", "fn main() {}\n");
    let first = fx.write("a.rs", "fn a() {}\n");
    let second = fx.write("b.rs", "fn b() {}\n");

    let handle = {
        let translator = Arc::clone(&fx.translator);
        let path = main.to_string_lossy().into_owned();
        tokio::spawn(async move {
            translator
                .handle_references(
                    client_path(path),
                    pos(1, 1),
                    true,
                    ResultContext::EnclosingSymbol,
                )
                .await
        })
    };
    {
        let mut wire = BufReader::new(&mut fx.server.write_stdout);
        expect_request(&mut wire, "textDocument/didOpen").await;
        let request = expect_request(&mut wire, "textDocument/references").await;
        write_response(
            &mut fx.server.read_half_stdin,
            &request["id"],
            json!([
                location(&file_uri(&first), 0),
                location(&file_uri(&second), 0)
            ]),
        )
        .await;
        expect_request(&mut wire, "textDocument/didOpen").await;
        expect_request(&mut wire, "textDocument/documentSymbol").await;
    }
    let result = handle.await.unwrap().unwrap();

    assert_matches!(
        outcome(&result, 0),
        EnclosingSymbolOutcome::Unavailable { .. }
    );
    assert_eq!(
        outcome(&result, 1),
        EnclosingSymbolOutcome::NotComputed {
            reason: NotComputedReason::Deadline
        }
    );
    let summary = result.enrichment.unwrap();
    assert_eq!(
        (
            summary.files_enriched,
            summary.files_skipped,
            summary.cut_short
        ),
        (0, 2, true)
    );
}
