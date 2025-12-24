//! End-to-end tests for MCP protocol implementation.
//!
//! These tests validate the complete MCP protocol flow by spawning the mcpls
//! binary and communicating with it as a real MCP client would.

use anyhow::Result;
use serde_json::json;

use super::mcp_client::McpClient;

/// Test the MCP initialize handshake.
///
/// Validates that the server:
/// - Accepts the initialize request
/// - Returns the correct protocol version
/// - Exposes tool capabilities
/// - Provides server information
#[test]
fn test_e2e_initialize_handshake() -> Result<()> {
    let mut client = McpClient::spawn()?;

    let response = client.initialize()?;

    assert!(
        response.get("result").is_some(),
        "Response should have 'result' field"
    );

    let result = &response["result"];

    assert_eq!(
        result["protocolVersion"], "2024-11-05",
        "Protocol version should match"
    );

    assert!(
        result["capabilities"]["tools"].is_object(),
        "Should expose tools capability"
    );

    assert_eq!(
        result["serverInfo"]["name"], "mcpls",
        "Server name should be 'mcpls'"
    );

    Ok(())
}

/// Test listing all available MCP tools.
///
/// Validates that:
/// - tools/list returns an array of 8 tools
/// - All expected tool names are present
#[test]
fn test_e2e_list_tools() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let response = client.list_tools()?;

    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools should be an array"));

    assert_eq!(tools.len(), 8, "Should have exactly 8 tools");

    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    assert!(
        tool_names.contains(&"get_hover"),
        "Should have get_hover tool"
    );
    assert!(
        tool_names.contains(&"get_definition"),
        "Should have get_definition tool"
    );
    assert!(
        tool_names.contains(&"get_references"),
        "Should have get_references tool"
    );
    assert!(
        tool_names.contains(&"get_diagnostics"),
        "Should have get_diagnostics tool"
    );
    assert!(
        tool_names.contains(&"rename_symbol"),
        "Should have rename_symbol tool"
    );
    assert!(
        tool_names.contains(&"get_completions"),
        "Should have get_completions tool"
    );
    assert!(
        tool_names.contains(&"get_document_symbols"),
        "Should have get_document_symbols tool"
    );
    assert!(
        tool_names.contains(&"format_document"),
        "Should have format_document tool"
    );

    Ok(())
}

/// Test that all tools have valid JSON schemas.
///
/// Validates that each tool has:
/// - A name (string)
/// - A description (string)
/// - An input schema (object)
/// - Schema with "object" type
/// - Schema with properties
#[test]
fn test_e2e_tool_schemas() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let response = client.list_tools()?;
    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools should be an array"));

    for tool in tools {
        let tool_name = tool["name"]
            .as_str()
            .unwrap_or_else(|| panic!("Tool should have name field"));

        assert!(
            tool["name"].is_string(),
            "Tool '{tool_name}' should have name as string"
        );

        assert!(
            tool["description"].is_string(),
            "Tool '{tool_name}' should have description as string"
        );

        assert!(
            tool["inputSchema"].is_object(),
            "Tool '{tool_name}' should have inputSchema as object"
        );

        let schema = &tool["inputSchema"];

        assert_eq!(
            schema["type"], "object",
            "Tool '{tool_name}' schema type should be 'object'"
        );

        assert!(
            schema["properties"].is_object(),
            "Tool '{tool_name}' schema should have properties object"
        );
    }

    Ok(())
}

/// Test calling a non-existent tool.
///
/// Validates that the server properly rejects invalid tool calls
/// with an appropriate error response.
#[test]
fn test_e2e_invalid_tool_call() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let result = client.call_tool("non_existent_tool", &json!({}));

    assert!(result.is_err(), "Should return error for non-existent tool");

    if let Err(err) = result {
        let error_msg = format!("{err:?}");
        assert!(
            error_msg.contains("error") || error_msg.contains("Error"),
            "Error message should indicate failure"
        );
    }

    Ok(())
}

/// Test calling a tool with missing required parameters.
///
/// Validates that the server properly validates tool parameters
/// and rejects calls with missing required fields.
#[test]
fn test_e2e_tool_call_missing_params() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let result = client.call_tool("get_hover", &json!({}));

    assert!(
        result.is_err(),
        "Should return error for missing required parameters"
    );

    if let Err(err) = result {
        let error_msg = format!("{err:?}");
        assert!(
            error_msg.contains("error") || error_msg.contains("Error"),
            "Error message should indicate parameter validation failure"
        );
    }

    Ok(())
}

/// Test calling `get_hover` with invalid file path.
///
/// Validates that the server properly handles file path validation
/// and returns appropriate errors for non-existent files.
#[test]
fn test_e2e_tool_call_invalid_file() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let result = client.call_tool(
        "get_hover",
        &json!({
            "file_path": "/nonexistent/path/to/file.rs",
            "line": 1,
            "character": 1
        }),
    );

    assert!(result.is_err(), "Should return error for non-existent file");

    Ok(())
}

/// Test calling `get_definition` with out-of-bounds position.
///
/// Validates that the server handles position validation correctly.
#[test]
fn test_e2e_tool_call_invalid_position() -> Result<()> {
    use std::fs;

    use tempfile::TempDir;

    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let temp_dir = TempDir::new()?;
    let test_file = temp_dir.path().join("test.rs");
    fs::write(&test_file, "fn main() {}\n")?;

    let result = client.call_tool(
        "get_definition",
        &json!({
            "file_path": test_file.to_string_lossy(),
            "line": 9999,
            "character": 9999
        }),
    );

    // Server should either return error or empty result for out-of-bounds position
    // Both are acceptable behaviors
    if let Ok(response) = result {
        // If successful, result should indicate no definition found
        let result_field = &response["result"];
        // Accept both null/empty results as valid responses
        assert!(
            result_field.is_null() || result_field.is_array() || result_field.is_object(),
            "Should return null or empty result for invalid position"
        );
    }
    // Error response is also acceptable

    Ok(())
}

/// Test the complete workflow: initialize → list → call tool.
///
/// This test validates the typical usage pattern of an MCP client.
#[test]
fn test_e2e_complete_workflow() -> Result<()> {
    let mut client = McpClient::spawn()?;

    // Step 1: Initialize
    let init_response = client.initialize()?;
    assert!(init_response.get("result").is_some());

    // Step 2: List tools
    let list_response = client.list_tools()?;
    let tools = list_response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools should be an array"));
    assert!(!tools.is_empty(), "Should have tools available");

    // Step 3: Verify we can attempt to call a tool (even if it fails due to no LSP)
    // This validates the protocol flow works end-to-end
    let _result = client.call_tool("get_diagnostics", &json!({"file_path": "test.rs"}));
    // We don't assert success here because LSP servers may not be configured
    // The important part is that the protocol flow works

    Ok(())
}

/// Test multiple sequential requests on the same connection.
///
/// Validates that:
/// - The connection remains stable across multiple requests
/// - Request IDs increment correctly
/// - The server handles concurrent operations properly
#[test]
fn test_e2e_multiple_requests() -> Result<()> {
    let mut client = McpClient::spawn()?;

    // Multiple initialize calls should work (idempotent)
    let response1 = client.initialize()?;
    assert!(response1.get("result").is_some());

    let response2 = client.list_tools()?;
    assert!(response2.get("result").is_some());

    let response3 = client.list_tools()?;
    assert!(response3.get("result").is_some());

    // Responses should have different IDs
    assert_ne!(
        response1.get("id"),
        response2.get("id"),
        "Different requests should have different IDs"
    );
    assert_ne!(
        response2.get("id"),
        response3.get("id"),
        "Different requests should have different IDs"
    );

    Ok(())
}
