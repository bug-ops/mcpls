//! Acceptance tests for #909 at the tool-surface level: which tools accept
//! `symbol_source` and which output schemas can carry a `source`.

#![expect(
    clippy::unwrap_used,
    reason = "test code; a panic is the failure signal"
)]

use mcpls_core::bridge::LocationContext;
use serde_json::Value;

const LOCATION_TOOLS: [&str; 5] = [
    "get_definition",
    "get_references",
    "go_to_implementation",
    "go_to_type_definition",
    "go_to_declaration",
];

fn golden_tool(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcp/tool_surface.json");
    let tools: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    tools
        .into_iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("{name} is listed"))
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap()
}

#[test]
fn test_location_context_parses_each_spelling_and_defaults_to_none() {
    let parse = |text: &str| serde_json::from_str::<LocationContext>(text);

    assert_eq!(parse("\"none\"").unwrap(), LocationContext::None);
    assert_eq!(
        parse("\"enclosing_symbol\"").unwrap(),
        LocationContext::EnclosingSymbol
    );
    assert_eq!(
        parse("\"symbol_source\"").unwrap(),
        LocationContext::SymbolSource
    );
    assert!(parse("\"source\"").is_err());
    assert_eq!(LocationContext::default(), LocationContext::None);
}

#[test]
fn test_location_tools_accept_symbol_source_in_their_input_schema() {
    for name in LOCATION_TOOLS {
        let input = compact(&golden_tool(name)["inputSchema"]);

        assert!(input.contains("\"symbol_source\""), "{name}");
        assert!(input.contains("\"enclosing_symbol\""), "{name}");
    }
}

#[test]
fn test_get_diagnostics_input_schema_has_no_symbol_source() {
    let input = compact(&golden_tool("get_diagnostics")["inputSchema"]);

    assert!(input.contains("\"enclosing_symbol\""));
    assert!(!input.contains("symbol_source"));
}

#[test]
fn test_get_diagnostics_output_schema_cannot_carry_a_source_outcome() {
    let output = compact(&golden_tool("get_diagnostics")["outputSchema"]);

    for forbidden in [
        "SourceOutcome",
        "SourceSkipReason",
        "same_as",
        "symbol_lookup_skipped",
        "call_budget",
    ] {
        assert!(!output.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn test_location_tools_output_schema_describes_every_source_outcome() {
    for name in LOCATION_TOOLS {
        let output = compact(&golden_tool(name)["outputSchema"]);

        for expected in [
            "\"included\"",
            "\"same_as\"",
            "\"not_computed\"",
            "\"no_span\"",
            "\"call_budget\"",
            "\"document_changed\"",
            "\"symbol_lookup_skipped\"",
        ] {
            assert!(output.contains(expected), "{name}: {expected}");
        }
    }
}

#[test]
fn test_location_tools_output_item_declares_a_source_property_beside_the_uri() {
    for name in LOCATION_TOOLS {
        let tool = golden_tool(name);
        let defs = tool["outputSchema"]["$defs"].as_object().expect("$defs");

        let carries_source = defs.values().any(|def| {
            def.pointer("/properties/uri").is_some()
                && def.pointer("/properties/range").is_some()
                && def.pointer("/properties/enclosing_symbol").is_some()
                && def.pointer("/properties/source").is_some()
        });

        assert!(carries_source, "{name}");
    }
}
