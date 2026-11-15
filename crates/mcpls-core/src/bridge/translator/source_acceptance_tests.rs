//! Acceptance tests for #909: `symbol_source` attaches the source text of the
//! definition (go-to tools) or of the enclosing symbol (`get_references`) to each
//! location, bounded per item and per call.

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::BufReader;
use tokio::time::timeout;
use url::Url;

use super::enclosing::LocationContext;
use super::testing::*;
use crate::config::ServerId;
use crate::test_lsp::client_path;

const ITEM_CAP_BYTES: usize = 16 * 1024;

fn full_caps() -> lsp_types::ServerCapabilities {
    lsp_types::ServerCapabilities {
        references_provider: Some(lsp_types::ReferencesProvider::Bool(true)),
        definition_provider: Some(lsp_types::DefinitionProvider::Bool(true)),
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

fn location_at(uri: &str, start: (u32, u32), end: (u32, u32)) -> Value {
    json!({"uri": uri, "range": lsp_range(start, end)})
}

fn symbol(name: &str, range: &Value, selection: &Value) -> Value {
    json!({"name": name, "kind": 12, "range": range, "selectionRange": selection})
}

fn line_symbol(name: &str, line: u32, len: usize) -> Value {
    let span = lsp_range((line, 0), (line, u32::try_from(len).unwrap()));
    symbol(name, &span, &span)
}

fn file_uri(path: &std::path::Path) -> String {
    Url::from_file_path(path).unwrap().to_string()
}

struct Fixture {
    dir: TempDir,
    translator: Arc<super::Translator>,
    server: FakeServer,
}

fn fixture() -> Fixture {
    let dir = TempDir::new().unwrap();
    let (translator, server) =
        translator_with_capabilities(&dir, &ServerId::from_static("rust"), full_caps());
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

#[derive(Clone, Copy)]
enum Call {
    References,
    Definition,
}

impl Call {
    const fn method(self) -> &'static str {
        match self {
            Self::References => "textDocument/references",
            Self::Definition => "textDocument/definition",
        }
    }
}

async fn expect_request(wire: &mut BufReader<&mut tokio::io::DuplexStream>, method: &str) -> Value {
    let request = read_framed_message(wire).await;
    assert_eq!(request["method"], method);
    request
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

/// Drives one call: answers the primary request with `answer`, then each file's
/// `documentSymbol` request in the order the handler asks, calling `before_answer`
/// first. A `None` answer is a server error. Returns the serialized locations result.
async fn run(
    fx: &mut Fixture,
    call: Call,
    context: LocationContext,
    main: &std::path::Path,
    answer: Value,
    symbols: Vec<Option<Value>>,
    before_answer: &dyn Fn(),
) -> Value {
    let handle = {
        let translator = Arc::clone(&fx.translator);
        let path = main.to_string_lossy().into_owned();
        tokio::spawn(async move {
            match call {
                Call::References => translator
                    .handle_references(client_path(path), pos(1, 1), true, context)
                    .await
                    .map(|references| serde_json::to_value(references.result).unwrap()),
                Call::Definition => translator
                    .handle_definition(client_path(path), pos(1, 1), context)
                    .await
                    .map(|locations| serde_json::to_value(locations).unwrap()),
            }
        })
    };
    {
        let mut wire = BufReader::new(&mut fx.server.write_stdout);
        expect_request(&mut wire, "textDocument/didOpen").await;
        let request = expect_request(&mut wire, call.method()).await;
        write_response(&mut fx.server.read_half_stdin, &request["id"], answer).await;
        for answer in symbols {
            let mut next = read_framed_message(&mut wire).await;
            if next["method"] == "textDocument/didOpen" {
                next = read_framed_message(&mut wire).await;
            }
            assert_eq!(next["method"], "textDocument/documentSymbol");
            before_answer();
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

fn source_of(result: &Value, index: usize) -> &Value {
    &result["locations"][index]["source"]
}

fn included(text: &str, truncated: bool) -> Value {
    json!({"status": "included", "text": text, "truncated": truncated})
}

fn not_computed(reason: &str) -> Value {
    json!({"status": "not_computed", "reason": reason})
}

fn same_as(item: u32) -> Value {
    json!({"status": "same_as", "item": item})
}

fn included_text(result: &Value, index: usize) -> &str {
    source_of(result, index)["text"]
        .as_str()
        .unwrap_or_else(|| panic!("item {index} has no included text: {result}"))
}

async fn noop_run(
    fx: &mut Fixture,
    call: Call,
    context: LocationContext,
    main: &std::path::Path,
    answer: Value,
    symbols: Vec<Option<Value>>,
) -> Value {
    run(fx, call, context, main, answer, symbols, &|| {}).await
}

#[tokio::test]
async fn test_references_symbol_source_charges_one_enclosing_body_once() {
    let mut fx = fixture();
    let mut lines = vec!["fn f() {".to_owned()];
    lines.extend(std::iter::repeat_n("    x();".to_owned(), 148));
    lines.push("}".to_owned());
    let body = lines.join("\n");
    let main = fx.write("main.rs", &format!("{body}\n"));
    let uri = file_uri(&main);
    let span = lsp_range((0, 0), (149, 1));
    let locations: Vec<Value> = (1..=30)
        .map(|line| location_at(&uri, (line, 2), (line, 4)))
        .collect();

    let result = noop_run(
        &mut fx,
        Call::References,
        LocationContext::SymbolSource,
        &main,
        Value::Array(locations),
        vec![Some(json!([symbol(
            "f",
            &span,
            &lsp_range((0, 3), (0, 4))
        )]))],
    )
    .await;

    assert_eq!(source_of(&result, 0), &included(&body, false));
    for index in 1..30 {
        assert_eq!(source_of(&result, index), &same_as(0), "item {index}");
    }
    expect_no_more_requests(&mut fx.server).await;
}

#[tokio::test]
async fn test_references_default_context_emits_neither_source_nor_enclosing_symbol() {
    let mut fx = fixture();
    let main = fx.write("main.rs", "fn main() {}\n");
    let uri = file_uri(&main);

    let result = noop_run(
        &mut fx,
        Call::References,
        LocationContext::None,
        &main,
        json!([location_at(&uri, (0, 3), (0, 7))]),
        vec![],
    )
    .await;

    assert!(result["locations"][0].get("source").is_none());
    assert!(result["locations"][0].get("enclosing_symbol").is_none());
    expect_no_more_requests(&mut fx.server).await;
}

#[tokio::test]
async fn test_references_enclosing_symbol_context_emits_no_source() {
    let mut fx = fixture();
    let main = fx.write("main.rs", "fn main() {}\n");
    let uri = file_uri(&main);
    let span = lsp_range((0, 0), (0, 12));

    let result = noop_run(
        &mut fx,
        Call::References,
        LocationContext::EnclosingSymbol,
        &main,
        json!([location_at(&uri, (0, 3), (0, 7))]),
        vec![Some(json!([symbol("main", &span, &span)]))],
    )
    .await;

    assert!(result["locations"][0].get("source").is_none());
    assert_eq!(
        result["locations"][0]["enclosing_symbol"]["status"],
        "resolved"
    );
    expect_no_more_requests(&mut fx.server).await;
}

#[tokio::test]
async fn test_references_top_level_item_has_no_span() {
    let mut fx = fixture();
    let main = fx.write("main.rs", "fn f() {\n}\nconst X: u8 = 1;\n");
    let uri = file_uri(&main);
    let span = lsp_range((0, 0), (1, 1));

    let result = noop_run(
        &mut fx,
        Call::References,
        LocationContext::SymbolSource,
        &main,
        json!([location_at(&uri, (2, 6), (2, 7))]),
        vec![Some(json!([symbol("f", &span, &span)]))],
    )
    .await;

    assert_eq!(
        result["locations"][0]["enclosing_symbol"]["status"],
        "top_level"
    );
    assert_eq!(source_of(&result, 0), &not_computed("no_span"));
}

#[tokio::test]
async fn test_definition_of_a_symbol_returns_the_symbol_range_text() {
    let mut fx = fixture();
    let main = fx.write("main.rs", "fn main() {}\n");
    let lib = fx.write("lib.rs", "// c\nfn target() {\n    1\n}\n");
    let name = lsp_range((1, 3), (1, 9));

    let result = noop_run(
        &mut fx,
        Call::Definition,
        LocationContext::SymbolSource,
        &main,
        json!([{"uri": file_uri(&lib), "range": name}]),
        vec![Some(json!([symbol(
            "target",
            &lsp_range((1, 0), (3, 1)),
            &name
        )]))],
    )
    .await;

    assert_eq!(
        source_of(&result, 0),
        &included("fn target() {\n    1\n}", false)
    );
    expect_no_more_requests(&mut fx.server).await;
}

#[tokio::test]
async fn test_definition_slices_a_non_ascii_line_by_utf16_columns() {
    let mut fx = fixture();
    let main = fx.write("main.rs", "/*\u{1F600}*/ fn x() {}\n");
    let uri = file_uri(&main);

    let result = noop_run(
        &mut fx,
        Call::Definition,
        LocationContext::SymbolSource,
        &main,
        json!([location_at(&uri, (0, 10), (0, 11))]),
        vec![Some(json!([symbol(
            "x",
            &lsp_range((0, 7), (0, 16)),
            &lsp_range((0, 10), (0, 11))
        )]))],
    )
    .await;

    assert_eq!(source_of(&result, 0), &included("fn x() {}", false));
}

fn local_definition_fixture(fx: &Fixture) -> (std::path::PathBuf, String, Value) {
    let main = fx.write("main.rs", "fn outer() {\n    let x = 1;\n    x\n}\n");
    let uri = file_uri(&main);
    let outer = json!([symbol(
        "outer",
        &lsp_range((0, 0), (3, 1)),
        &lsp_range((0, 3), (0, 8))
    )]);
    (main, uri, outer)
}

#[tokio::test]
async fn test_definition_link_of_a_local_uses_the_link_target_range() {
    let mut fx = fixture();
    let (main, uri, outer) = local_definition_fixture(&fx);
    let link = json!([{
        "targetUri": uri,
        "targetRange": lsp_range((1, 4), (1, 14)),
        "targetSelectionRange": lsp_range((1, 8), (1, 9)),
    }]);

    let result = noop_run(
        &mut fx,
        Call::Definition,
        LocationContext::SymbolSource,
        &main,
        link,
        vec![Some(outer)],
    )
    .await;

    assert_eq!(source_of(&result, 0), &included("let x = 1;", false));
}

#[tokio::test]
async fn test_definition_plain_location_of_a_local_has_no_span() {
    let mut fx = fixture();
    let (main, uri, outer) = local_definition_fixture(&fx);

    let result = noop_run(
        &mut fx,
        Call::Definition,
        LocationContext::SymbolSource,
        &main,
        json!([location_at(&uri, (1, 8), (1, 9))]),
        vec![Some(outer)],
    )
    .await;

    assert_eq!(source_of(&result, 0), &not_computed("no_span"));
}

#[tokio::test]
async fn test_definition_failed_symbol_lookup_skips_source_even_with_a_link_range() {
    let mut fx = fixture();
    let (main, uri, _) = local_definition_fixture(&fx);
    let link = json!([{
        "targetUri": uri,
        "targetRange": lsp_range((1, 4), (1, 14)),
        "targetSelectionRange": lsp_range((1, 8), (1, 9)),
    }]);

    let result = noop_run(
        &mut fx,
        Call::Definition,
        LocationContext::SymbolSource,
        &main,
        link,
        vec![None],
    )
    .await;

    assert_eq!(
        result["locations"][0]["enclosing_symbol"]["status"],
        "unavailable"
    );
    assert_eq!(
        source_of(&result, 0),
        &not_computed("symbol_lookup_skipped")
    );
}

#[tokio::test]
async fn test_symbol_source_out_of_workspace_item_is_neither_opened_nor_sliced() {
    let mut fx = fixture();
    let main = fx.write("main.rs", "fn main() {}\n");

    let result = noop_run(
        &mut fx,
        Call::References,
        LocationContext::SymbolSource,
        &main,
        json!([location_at(
            "file:///outside/workspace/lib.rs",
            (0, 2),
            (0, 4)
        )]),
        vec![],
    )
    .await;

    assert_eq!(
        result["locations"][0]["enclosing_symbol"],
        json!({"status": "not_computed", "reason": "out_of_workspace"})
    );
    assert_eq!(
        source_of(&result, 0),
        &not_computed("symbol_lookup_skipped")
    );
    expect_no_more_requests(&mut fx.server).await;
}

#[tokio::test]
async fn test_symbol_source_caps_a_long_body_at_200_lines() {
    let mut fx = fixture();
    let mut lines = vec!["fn long() {".to_owned()];
    lines.extend((0..298).map(|n| format!("    step_{n}();")));
    lines.push("}".to_owned());
    let main = fx.write("main.rs", &format!("{}\n", lines.join("\n")));
    let uri = file_uri(&main);
    let span = lsp_range((0, 0), (299, 1));

    let result = noop_run(
        &mut fx,
        Call::References,
        LocationContext::SymbolSource,
        &main,
        json!([location_at(&uri, (1, 4), (1, 10))]),
        vec![Some(json!([symbol("long", &span, &span)]))],
    )
    .await;

    assert_eq!(source_of(&result, 0)["truncated"], true);
    let text = included_text(&result, 0);
    assert_eq!(text.split('\n').count(), 200);
    assert_eq!(text, lines[..200].join("\n"));
}

#[tokio::test]
async fn test_symbol_source_cuts_a_minified_line_on_a_char_boundary() {
    let mut fx = fixture();
    let line = "\u{e9}".repeat(10_000);
    let main = fx.write("main.rs", &format!("{line}\n"));
    let uri = file_uri(&main);

    let result = noop_run(
        &mut fx,
        Call::References,
        LocationContext::SymbolSource,
        &main,
        json!([location_at(&uri, (0, 2), (0, 4))]),
        vec![Some(json!([line_symbol("min", 0, 10_000)]))],
    )
    .await;

    assert_eq!(source_of(&result, 0)["truncated"], true);
    let text = included_text(&result, 0);
    assert!(text.len() <= ITEM_CAP_BYTES);
    assert!(line.starts_with(text));
}

fn wide_line(marker: char, len: usize) -> String {
    std::iter::repeat_n(marker, len).collect()
}

#[tokio::test]
async fn test_symbol_source_skips_an_over_budget_span_and_keeps_filling_greedily() {
    let mut fx = fixture();
    let widths = [20_000, 20_000, 20_000, 10_000, 20_000, 100];
    let text: String = widths
        .iter()
        .enumerate()
        .map(|(n, &width)| {
            format!(
                "{}\n",
                wide_line(char::from(b'a' + u8::try_from(n).unwrap()), width)
            )
        })
        .collect();
    let main = fx.write("main.rs", &text);
    let uri = file_uri(&main);
    let symbols: Vec<Value> = widths
        .iter()
        .enumerate()
        .map(|(n, &width)| line_symbol(&format!("f{n}"), u32::try_from(n).unwrap(), width))
        .collect();
    let locations: Vec<Value> = [0, 1, 2, 3, 4, 5]
        .into_iter()
        .map(|line| location_at(&uri, (line, 2), (line, 4)))
        .chain([location_at(&uri, (4, 10), (4, 12))])
        .collect();

    let result = noop_run(
        &mut fx,
        Call::References,
        LocationContext::SymbolSource,
        &main,
        Value::Array(locations),
        vec![Some(Value::Array(symbols))],
    )
    .await;

    let statuses: Vec<&str> = (0..7)
        .map(|index| source_of(&result, index)["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        statuses,
        [
            "included",
            "included",
            "included",
            "included",
            "not_computed",
            "included",
            "not_computed"
        ]
    );
    assert_eq!(source_of(&result, 4), &not_computed("call_budget"));
    assert_eq!(source_of(&result, 6), &not_computed("call_budget"));
    assert_eq!(source_of(&result, 0)["truncated"], true);
    assert_eq!(source_of(&result, 3)["truncated"], false);
    let total: usize = [0, 1, 2, 3, 5]
        .iter()
        .map(|&i| included_text(&result, i).len())
        .sum();
    assert!(total <= 64 * 1024, "{total}");
}

#[tokio::test]
async fn test_symbol_source_spends_the_call_budget_in_file_first_seen_order() {
    let mut fx = fixture();
    let a_text: String = (0..4_u8)
        .map(|n| format!("{}\n", wide_line(char::from(b'a' + n), 20_000)))
        .collect();
    let main = fx.write("a.rs", &a_text);
    let b = fx.write("b.rs", &format!("{}\n", wide_line('z', 100)));
    let (a_uri, b_uri) = (file_uri(&main), file_uri(&b));
    let a_symbols: Vec<Value> = (0..4)
        .map(|n| line_symbol(&format!("a{n}"), n, 20_000))
        .collect();
    let locations = json!([
        location_at(&a_uri, (0, 2), (0, 4)),
        location_at(&b_uri, (0, 2), (0, 4)),
        location_at(&a_uri, (1, 2), (1, 4)),
        location_at(&a_uri, (2, 2), (2, 4)),
        location_at(&a_uri, (3, 2), (3, 4)),
    ]);

    let result = noop_run(
        &mut fx,
        Call::References,
        LocationContext::SymbolSource,
        &main,
        locations,
        vec![
            Some(Value::Array(a_symbols)),
            Some(json!([line_symbol("b0", 0, 100)])),
        ],
    )
    .await;

    for index in [0, 2, 3, 4] {
        assert_eq!(
            source_of(&result, index)["status"],
            "included",
            "item {index}"
        );
    }
    assert_eq!(source_of(&result, 1), &not_computed("call_budget"));
}

#[tokio::test]
async fn test_symbol_source_reports_a_document_that_moved_during_the_lookup() {
    let mut fx = fixture();
    let main = fx.write("main.rs", "fn main() {}\n");
    let uri = file_uri(&main);
    let span = lsp_range((0, 0), (0, 12));
    let tracker = Arc::clone(fx.translator.document_tracker());
    let server = ServerId::from_static("rust");
    let moved = main.clone();

    let result = run(
        &mut fx,
        Call::References,
        LocationContext::SymbolSource,
        &main,
        json!([location_at(&uri, (0, 3), (0, 7))]),
        vec![Some(json!([symbol("main", &span, &span)]))],
        &move || tracker.set_synced_version_for_test(&moved, &server, 99),
    )
    .await;

    assert_eq!(source_of(&result, 0), &not_computed("document_changed"));
}
