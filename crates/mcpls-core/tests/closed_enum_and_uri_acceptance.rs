//! Acceptance tests for #919: closed enums advertise their spellings in the output
//! schema, and a publish's URI has one shortcut accessor.

use lsp_types::{PublishDiagnosticsParams, Uri};
use mcpls_core::bridge::Capability;
use mcpls_core::lsp::DecodedPublish;
use serde_json::{Value, json};

#[test]
fn test_capability_schema_enum_lists_every_spelling_in_order() {
    let schema = serde_json::to_value(schemars::schema_for!(Capability)).unwrap();

    let expected: Vec<Value> = Capability::ALL
        .iter()
        .map(|capability| json!(capability.as_str()))
        .collect();
    assert_eq!(schema.get("enum"), Some(&Value::Array(expected)));
    assert_eq!(schema.get("type"), Some(&json!("string")));
}

#[test]
fn test_get_tool_support_output_schema_closes_the_capability_set() {
    let tools: Vec<Value> = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcp/tool_surface.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "get_tool_support")
        .expect("get_tool_support is listed");

    let capability = tool
        .pointer("/outputSchema/$defs/Capability")
        .expect("Capability definition");

    let expected: Vec<Value> = Capability::ALL
        .iter()
        .map(|capability| json!(capability.as_str()))
        .collect();
    assert_eq!(capability.get("enum"), Some(&Value::Array(expected)));
}

#[test]
fn test_decoded_publish_file_uri_is_the_published_uri() {
    let uri = Uri::from("file:///workspace/main.rs".to_owned());
    let publish = DecodedPublish::try_from(PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics: Vec::new(),
        version: None,
    })
    .unwrap();

    assert_eq!(publish.file_uri(), &uri);
    assert_eq!(publish.file_uri(), publish.uri().uri());
}
