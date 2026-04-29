//! Assertion helpers for e2e test sub-cases.
#![allow(dead_code)]

use serde_json::Value;

/// Extract the text content from an MCP tool call response.
///
/// MCP tool responses have the shape:
/// `{"result": {"content": [{"type": "text", "text": "<json string>"}]}}`
///
/// Returns the inner text string or an empty string if absent.
pub fn content_text(response: &Value) -> String {
    response["result"]["content"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|item| item["text"].as_str())
        .unwrap_or("")
        .to_owned()
}

/// Assert that the MCP response is not an MCP-level error (isError = true).
///
/// Returns the text content on success.
pub fn assert_tool_ok(response: &Value) -> String {
    let is_error = response["result"]["isError"].as_bool().unwrap_or(false);
    assert!(
        !is_error,
        "Expected successful tool response, got isError=true: {}",
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("<no text>")
    );
    content_text(response)
}

/// Assert that a JSON string parsed from tool text contains a symbol with the given name.
///
/// `symbols` should be an array of objects each having at least a `name` field.
pub fn assert_contains_symbol(symbols: &Value, name: &str) {
    let arr = symbols
        .as_array()
        .unwrap_or_else(|| panic!("expected array of symbols, got {symbols}"));
    let found = arr.iter().any(|s| s["name"].as_str().unwrap_or("") == name);
    assert!(found, "symbol '{name}' not found in {symbols}");
}

/// Assert that a URI ends with the given suffix.
pub fn assert_uri_ends_with(uri: &str, suffix: &str) {
    assert!(
        uri.ends_with(suffix),
        "expected URI to end with '{suffix}', got '{uri}'"
    );
}

/// Build a `file://` URI for an absolute path.
///
/// Handles macOS `/private/var` → `/var` symlinks by using the path as-is.
pub fn file_uri(path: &std::path::Path) -> String {
    url::Url::from_file_path(path)
        .unwrap_or_else(|()| panic!("cannot convert path to file URI: {}", path.display()))
        .to_string()
}
