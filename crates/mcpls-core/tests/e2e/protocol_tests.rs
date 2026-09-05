//! End-to-end tests for MCP protocol implementation.
//!
//! These tests validate the complete MCP protocol flow by spawning the mcpls
//! binary and communicating with it as a real MCP client would.

use anyhow::Result;
use serde_json::json;
use tempfile::TempDir;

use super::mcp_client::McpClient;
use crate::common::test_utils::{rust_analyzer_available, rust_workspace_path};

/// Test the MCP initialize handshake.
///
/// Validates that the server:
/// - Accepts the initialize request
/// - Returns the correct protocol version
/// - Exposes tool capabilities
/// - Provides server information
#[test]
#[ignore = "Requires mcpls binary built"]
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

/// Test that a configured `[mcp].title` reaches the real `initialize`
/// response, driving the actual `serve_with` -> `McplsServer::new` ->
/// `get_info` path (#347) rather than constructing `McpConfig` in-process.
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_initialize_reflects_configured_mcp_title() -> Result<()> {
    let config_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_config.toml");

    let mut client = McpClient::spawn_with_args(&[
        "--config",
        config_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid config path"))?,
    ])?;

    let response = client.initialize()?;
    let result = &response["result"];

    assert_eq!(
        result["serverInfo"]["title"], "E2E Custom Title",
        "Configured mcp.title should surface in serverInfo.title"
    );
    assert_eq!(
        result["serverInfo"]["name"], "mcpls",
        "serverInfo.name stays hardcoded regardless of [mcp] config"
    );

    Ok(())
}

/// Test listing all available MCP tools.
///
/// Validates that:
/// - tools/list returns an array of 16 tools
/// - All expected tool names are present
#[test]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_list_tools() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    let response = client.list_tools()?;

    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools should be an array"));

    assert_eq!(tools.len(), 20, "Should have exactly 20 tools");

    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    for expected in &[
        "get_hover",
        "get_definition",
        "get_references",
        "get_diagnostics",
        "rename_symbol",
        "get_completions",
        "get_document_symbols",
        "format_document",
        "workspace_symbol_search",
        "get_code_actions",
        "prepare_call_hierarchy",
        "get_incoming_calls",
        "get_outgoing_calls",
        "get_cached_diagnostics",
        "get_server_logs",
        "get_server_messages",
        "get_signature_help",
        "go_to_implementation",
        "go_to_type_definition",
        "get_inlay_hints",
    ] {
        assert!(tool_names.contains(expected), "Should have {expected} tool");
    }

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
#[ignore = "Requires mcpls binary built"]
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
#[ignore = "Requires mcpls binary built"]
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
#[ignore = "Requires mcpls binary built"]
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
#[ignore = "Requires mcpls binary built"]
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
#[ignore = "Requires mcpls binary built"]
fn test_e2e_tool_call_invalid_position() -> Result<()> {
    use std::fs;

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
#[ignore = "Requires mcpls binary built"]
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
#[ignore = "Requires mcpls binary built"]
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

/// Test that mcpls exits promptly on `SIGTERM` while the client's stdin
/// write end is still open (regression test for #308).
///
/// The MCP stdio transport is backed by `tokio::io::stdin()`, which parks an
/// uncancellable blocking-pool thread in a raw `read()` syscall. Without the
/// `std::process::exit` fix in `mcpls-cli`'s `main`, `#[tokio::main]`'s
/// runtime-shutdown wait for that thread would hang indefinitely as long as
/// the client (this test, via `McpClient`) keeps stdin's write end open.
///
/// Sending `SIGTERM` immediately after the handshake completes (no
/// artificial delay) also touches the tail of #318's window — the narrow gap
/// between `run_stdio`'s two `select!` blocks — but only weakly: signaling
/// this soon after `initialize()` returns reproduced the pre-fix bug in just
/// 1/15 runs, since the client-side I/O latency before the `kill` command
/// even runs dwarfs that gap. `test_e2e_sigterm_exits_promptly_during_handshake_wait`
/// below is the reliable reproducer for #318 (5/5 against pre-fix code).
#[test]
#[cfg(unix)]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_sigterm_exits_promptly_while_client_stdin_open() -> Result<()> {
    let mut client = McpClient::spawn()?;
    client.initialize()?;

    // No delay here is intentional: `run_stdio` now registers its SIGTERM
    // handler before awaiting the handshake at all (see #318), so the signal
    // is raced against the handshake/select loop from the moment the
    // process starts. Sending SIGTERM immediately after `initialize()`
    // returns exercises the narrowest part of that window instead of
    // masking it behind an artificial delay.
    let pid = client.pid();
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()?;
    assert!(
        status.success(),
        "failed to send SIGTERM to mcpls (pid {pid})"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(exit_status) = client.try_wait()? {
            // Distinguishes a graceful `process::exit(0)` from the process
            // being killed outright by the default SIGTERM disposition
            // (e.g. if the signal handler failed to register) -- the latter
            // would also make `try_wait` return `Some`, but with no LSP
            // shutdown having run. On Unix, `code()` is `None` for
            // signal-termination, so this one assertion covers both.
            assert_eq!(
                exit_status.code(),
                Some(0),
                "mcpls should exit with status 0 via its own shutdown path, not be killed \
                 by the default SIGTERM disposition (issue #308 regression)"
            );
            return Ok(());
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mcpls did not exit within 5s of SIGTERM while the client's stdin write end \
             was still open (issue #308 regression)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Test that mcpls exits promptly on `SIGTERM` sent *before* the client ever
/// sends the `initialize` request -- i.e. strictly during the MCP handshake
/// wait itself (regression test for #318).
///
/// This targets the actual bug in #318 directly: pre-fix, `run_stdio`
/// registered its `SIGTERM` handler only *after* `mcp_server.serve(..)`
/// resolved, so any signal arriving while `serve(..)` was still awaiting the
/// client's `initialize` request -- which can be an arbitrarily long wait in
/// real usage -- fell through to the OS's default disposition (immediate
/// kill, no graceful shutdown, no LSP cleanup). Sending `SIGTERM`
/// immediately after spawning, before writing anything to the child's
/// stdin, reliably lands inside that wait rather than racing the much
/// narrower post-handshake gap that
/// `test_e2e_sigterm_exits_promptly_while_client_stdin_open` exercises.
#[test]
#[cfg(unix)]
#[ignore = "Requires mcpls binary built"]
fn test_e2e_sigterm_exits_promptly_during_handshake_wait() -> Result<()> {
    let mut client = McpClient::spawn()?;

    // A brief sleep before signaling clears the unrelated, unfixable gap
    // between `fork`/`exec` and the point where *any* process code (the
    // runtime init that precedes even the fixed `ShutdownSignal::new()`)
    // has run -- the OS applies the default disposition until then no
    // matter what the binary does, so signaling with zero delay would fail
    // even against the fix and wouldn't be exercising #318 at all. 50ms is
    // far below the 5s deadline below and well within the handshake wait,
    // since `initialize()` is deliberately never called: the child is left
    // parked inside `mcp_server.serve(..)`, waiting to read the client's
    // first request.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let pid = client.pid();
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()?;
    assert!(
        status.success(),
        "failed to send SIGTERM to mcpls (pid {pid})"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(exit_status) = client.try_wait()? {
            assert_eq!(
                exit_status.code(),
                Some(0),
                "mcpls should exit with status 0 via its own shutdown path even when SIGTERM \
                 arrives before the MCP handshake completes, not be killed by the default \
                 SIGTERM disposition (issue #318 regression)"
            );
            return Ok(());
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mcpls did not exit within 5s of SIGTERM sent before the handshake completed \
             (issue #318 regression)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// #325: workspace resource-limit config, enforced through the real
// `serve()`/`serve_with()` startup path.
// ---------------------------------------------------------------------------

/// Spawns the real `mcpls` binary against a config naming a real
/// rust-analyzer server for `tests/fixtures/rust_workspace`, with
/// `[workspace]` extended by `extra_workspace_toml` (e.g. `max_documents = 3`).
///
/// This is what makes the resulting tests exercise the actual wiring in
/// `crates/mcpls-core/src/lib.rs`'s `serve_with` (`ServerConfig::load_from`
/// -> `WorkspaceConfig::resource_limits()` -> `Translator::with_resource_limits`)
/// rather than a hand-copied reconstruction of it. The returned `TempDir`
/// must be kept alive for as long as `McpClient`; it's only needed at
/// startup (the config file is read once), but dropping it early is
/// needless risk for no benefit.
fn spawn_mcpls_with_workspace_config(extra_workspace_toml: &str) -> Result<(TempDir, McpClient)> {
    let workspace_path = rust_workspace_path();
    let config_dir = TempDir::new()?;
    let config_path = config_dir.path().join("mcpls.toml");
    let toml_content = format!(
        r#"
        [workspace]
        roots = ["{}"]
        {extra_workspace_toml}

        [[lsp_servers]]
        language_id = "rust"
        command = "rust-analyzer"
        args = []
        file_patterns = ["**/*.rs"]
        "#,
        workspace_path.to_string_lossy().replace('\\', "\\\\")
    );
    std::fs::write(&config_path, toml_content)?;

    let mut client = McpClient::spawn_with_args(&[
        "--config",
        config_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid config path"))?,
    ])?;
    client.initialize()?;

    Ok((config_dir, client))
}

fn hover_args(path: &std::path::Path) -> serde_json::Value {
    json!({
        "file_path": path.to_string_lossy(),
        "line": 1,
        "character": 1,
    })
}

/// Calls `get_hover` on `path`, retrying while the error is
/// `Error::ServerInitializing`/`WorkspaceServersInitializing` (message
/// contains "initializing") -- the transient state before rust-analyzer's
/// background `initialize` handshake (`spawn_lsp_servers_background` in
/// `lib.rs`) has completed and it registers with the translator. This is the
/// only rust-analyzer-readiness state that can affect the calls below:
/// opening a document happens in `prepare_gated_document` *before* the
/// capability-gated LSP round trip (`bridge/translator/routing.rs`), so
/// whether rust-analyzer has finished *indexing* the workspace never affects
/// whether a document is counted against `max_documents`/`max_file_size`.
fn call_hover_past_server_init(
    client: &mut McpClient,
    path: &std::path::Path,
    deadline: std::time::Instant,
) -> Result<serde_json::Value> {
    loop {
        match client.call_tool("get_hover", &hover_args(path)) {
            Err(e) if e.to_string().contains("initializing") => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "rust-analyzer never finished initializing: {e}"
                );
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            other => return other,
        }
    }
}

/// #325: end-to-end proof that `workspace.max_documents`, set in a real
/// `mcpls.toml`, is enforced by `DocumentTracker` through the actual
/// `serve()`/`serve_with()` startup path (`main` -> `serve` -> `serve_with`
/// -> `Translator::with_resource_limits`), not an in-process reconstruction
/// of that wiring.
///
/// `DocumentTracker::open` rejects (returns `Err(DocumentLimitExceeded)`)
/// rather than evicting the oldest document once the limit is reached (see
/// `crates/mcpls-core/src/bridge/state.rs`), so this asserts rejection, not
/// eviction: the in-limit calls must not fail with that specific error, and
/// the over-limit call must.
#[test]
#[ignore = "Requires mcpls binary built and rust-analyzer installed"]
fn test_e2e_max_documents_config_enforced() -> Result<()> {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return Ok(());
    }

    let max_documents = 3;
    let workspace_path = rust_workspace_path();
    let (_config_dir, mut client) =
        spawn_mcpls_with_workspace_config(&format!("max_documents = {max_documents}"))?;

    // More real, distinct files than the configured limit, so opening them
    // one by one crosses the boundary set in TOML.
    let files = [
        workspace_path.join("src/lib.rs"),
        workspace_path.join("src/types.rs"),
        workspace_path.join("src/functions.rs"),
        workspace_path.join("extras/untouched.rs"),
    ];
    assert!(
        files.len() > max_documents,
        "fixture must provide more files than the configured limit to exercise the (N+1)th open"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    for (i, path) in files[..max_documents].iter().enumerate() {
        let result = call_hover_past_server_init(&mut client, path, deadline);
        if let Err(e) = &result {
            assert!(
                !e.to_string().contains("document limit exceeded"),
                "opening document {} of {max_documents} (within the configured limit) must \
                 not hit the limit: {e}",
                i + 1
            );
        }
    }

    let over_limit_path = &files[max_documents];
    let result = call_hover_past_server_init(&mut client, over_limit_path, deadline);
    match result {
        Err(e) => assert!(
            e.to_string().contains("document limit exceeded"),
            "expected a DocumentLimitExceeded error (per DocumentTracker::open's actual \
             reject-not-evict behavior), got: {e}"
        ),
        Ok(_) => panic!(
            "opening the (N+1)th distinct document must be rejected once \
             workspace.max_documents is reached"
        ),
    }

    Ok(())
}

/// #325: end-to-end proof that `workspace.max_file_size`, set in a real
/// `mcpls.toml`, is enforced by `DocumentTracker` through the real
/// `serve()`/`serve_with()` startup path. Companion to
/// `test_e2e_max_documents_config_enforced`, covering the other
/// `[workspace]` resource-limit field #325 asked for.
#[test]
#[ignore = "Requires mcpls binary built and rust-analyzer installed"]
fn test_e2e_max_file_size_config_enforced() -> Result<()> {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return Ok(());
    }

    let workspace_path = rust_workspace_path();
    // Every real fixture file is larger than 1 byte, so this deterministically
    // rejects the very first document opened, regardless of which one.
    let (_config_dir, mut client) = spawn_mcpls_with_workspace_config("max_file_size = 1")?;

    let lib_rs = workspace_path.join("src/lib.rs");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let result = call_hover_past_server_init(&mut client, &lib_rs, deadline);
    match result {
        Err(e) => assert!(
            e.to_string().contains("file size limit exceeded"),
            "expected a FileSizeLimitExceeded error, got: {e}"
        ),
        Ok(_) => panic!("a file exceeding the configured max_file_size must be rejected"),
    }

    Ok(())
}
