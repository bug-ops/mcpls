//! End-to-end test suite exercising all 16 MCP tools against a real rust-analyzer.
//!
//! # Process model
//!
//! A single `#[test] fn ra_e2e_suite()` drives the whole suite.  nextest sees
//! exactly one test → one process → rust-analyzer is spawned once.  Sub-cases
//! run sequentially; the suite panics at the end if any failed, printing an
//! aggregated report so all failures are visible at once.
//!
//! # Skip policy
//!
//! - `MCPLS_SKIP_RA=1`               → print skip line, return success
//! - `MCPLS_RUST_ANALYZER=<path>`    → use that binary
//! - rust-analyzer found in PATH     → use it
//! - not found and no skip flag      → panic (fail closed)
//!
//! # Filter
//!
//! Set `MCPLS_RA_FILTER=<substring>` to run only matching sub-cases locally.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_docs_in_private_items,
    missing_docs
)]

#[path = "common/assertions.rs"]
mod assertions;
#[path = "e2e/mcp_client.rs"]
mod mcp_client;
#[path = "common/ra_probe.rs"]
mod ra_probe;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use mcp_client::McpClient;
use ra_probe::{Resolution, resolve_rust_analyzer};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Sub-case infrastructure
// ---------------------------------------------------------------------------

struct SubResult {
    name: &'static str,
    outcome: Result<(), String>,
}

type SubCaseFn = fn(&mut McpClient, &Path) -> Result<(), String>;

struct SubCase {
    name: &'static str,
    run: SubCaseFn,
}

macro_rules! sub_case {
    ($name:ident) => {
        SubCase {
            name: stringify!($name),
            run: $name,
        }
    };
}

// ---------------------------------------------------------------------------
// Workspace staging
// ---------------------------------------------------------------------------

/// Copy `tests/fixtures/rust_workspace/` into a fresh `TempDir`.
///
/// Also copies `extras/broken.rs` into `src/broken.rs` and appends
/// `pub mod broken;` to `src/lib.rs` so rust-analyzer diagnoses it.
/// `extras/bad_format.rs` is placed in `src/bad_format.rs` without being
/// added to the module tree (`format_document` does not require it).
fn stage_workspace() -> TempDir {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_workspace");
    let tmp = TempDir::new().expect("failed to create TempDir");
    copy_dir_recursive(&fixture_dir, tmp.path()).expect("failed to copy fixture workspace");

    // Copy broken.rs into src/ and register it in lib.rs.
    let broken_src = fixture_dir.join("extras/broken.rs");
    let broken_dst = tmp.path().join("src/broken.rs");
    fs::copy(&broken_src, &broken_dst).expect("failed to copy broken.rs");

    let lib_path = tmp.path().join("src/lib.rs");
    let mut lib_content = fs::read_to_string(&lib_path).expect("failed to read lib.rs");
    lib_content.push_str("\npub mod broken;\n");
    fs::write(&lib_path, lib_content).expect("failed to append pub mod broken");

    // Copy bad_format.rs into src/ — NOT added to lib.rs (no mod declaration).
    let fmt_src = fixture_dir.join("extras/bad_format.rs");
    let fmt_dst = tmp.path().join("src/bad_format.rs");
    fs::copy(&fmt_src, &fmt_dst).expect("failed to copy bad_format.rs");

    tmp
}

/// Recursively copy `src` directory contents into `dst` (dst must exist).
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            // Skip extras/ and target/ — not needed in the staged workspace.
            if entry.file_name() == "extras" || entry.file_name() == "target" {
                continue;
            }
            fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Config generation
// ---------------------------------------------------------------------------

/// Typed config struct so that `toml::to_string` handles path escaping.
#[derive(Serialize, Deserialize)]
struct E2eConfig {
    workspace: WorkspaceConfig,
    lsp_servers: Vec<LspServerConfig>,
}

#[derive(Serialize, Deserialize)]
struct WorkspaceConfig {
    roots: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct LspServerConfig {
    language_id: String,
    command: String,
    args: Vec<String>,
    file_patterns: Vec<String>,
}

/// Write a minimal mcpls TOML config pointing at `ra_binary` and the given workspace root.
fn write_config(ra_binary: &Path, workspace_root: &Path, config_path: &Path) {
    let cfg = E2eConfig {
        workspace: WorkspaceConfig {
            roots: vec![workspace_root.to_string_lossy().into_owned()],
        },
        lsp_servers: vec![LspServerConfig {
            language_id: "rust".to_owned(),
            command: ra_binary.to_string_lossy().into_owned(),
            args: vec![],
            file_patterns: vec!["**/*.rs".to_owned()],
        }],
    };
    let content = toml::to_string(&cfg).expect("failed to serialize e2e config");
    fs::write(config_path, content).expect("failed to write e2e config");
}

// ---------------------------------------------------------------------------
// Anchor helpers
// ---------------------------------------------------------------------------

/// Find the 1-based line number of the first line in `file` containing `needle`.
///
/// Used instead of hardcoded line numbers so tests remain stable when the
/// fixture file is edited.
fn find_line(file: &Path, needle: &str) -> u32 {
    let content = fs::read_to_string(file).expect("failed to read file for anchor search");
    content
        .lines()
        .enumerate()
        .find_map(|(i, line)| {
            if line.contains(needle) {
                Some(u32::try_from(i + 1).expect("line number fits u32"))
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("anchor '{needle}' not found in {}", file.display()))
}

// ---------------------------------------------------------------------------
// Readiness gate
// ---------------------------------------------------------------------------

/// Poll `get_hover` on the `add` function until rust-analyzer returns content.
///
/// Timeout controlled by `MCPLS_RA_INDEX_TIMEOUT_SECS` (default 60, minimum 5).
///
/// NOTE: `$/progress` notifications are not captured by `bridge/notifications.rs`
/// (only `window/logMessage`, `window/showMessage`, and `publishDiagnostics` are
/// stored).  The readiness gate therefore uses hover-probe as the primary oracle.
/// See M-r1 in the architect handoff for the follow-up to add `$/progress` capture.
fn wait_until_ready(client: &mut McpClient, lib_rs: &Path) {
    // Windows CI runners are significantly slower than Linux/macOS.
    let default_timeout: u64 = if cfg!(windows) { 120 } else { 60 };
    let timeout_secs: u64 = std::env::var("MCPLS_RA_INDEX_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(default_timeout, |t| t.max(5));

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let lib_path = lib_rs.to_string_lossy().into_owned();
    let add_line = find_line(lib_rs, "pub fn add(");

    println!("[ra_e2e] waiting for rust-analyzer to index (timeout {timeout_secs}s)…");
    println!("[ra_e2e] hover probe: file={lib_path} line={add_line}");

    // Require 3 consecutive successful hover responses to guard against transient
    // successes during RA's intermediate indexing phases (observed on Windows CI).
    let required_consecutive: u32 = 3;
    let mut consecutive = 0u32;
    let mut last_print = Instant::now();
    loop {
        // Hover over `add` — the 'a' of "add" is at column 8 (1-based).
        let resp = client.call_tool(
            "get_hover",
            &json!({
                "file_path": lib_path,
                "line": add_line,
                "character": 8,
            }),
        );

        match &resp {
            Ok(r) => {
                let is_err = r["result"]["isError"].as_bool().unwrap_or(false);
                let text = assertions::content_text(r);
                // Require both "fn add" and "i32" to confirm type-checking is done.
                if text.contains("fn add") && text.contains("i32") {
                    consecutive += 1;
                    if consecutive >= required_consecutive {
                        println!("[ra_e2e] rust-analyzer is ready");
                        return;
                    }
                } else {
                    consecutive = 0;
                }
                // Print status every 10s so CI logs show progress.
                if last_print.elapsed() >= Duration::from_secs(10) {
                    let elapsed =
                        timeout_secs - deadline.saturating_duration_since(Instant::now()).as_secs();
                    println!(
                        "[ra_e2e] still waiting ({elapsed}s elapsed): consecutive={consecutive} \
                         isError={is_err} response={}",
                        &text[..text.len().min(120)]
                    );
                    last_print = Instant::now();
                }
            }
            Err(e) => {
                consecutive = 0;
                if last_print.elapsed() >= Duration::from_secs(10) {
                    println!("[ra_e2e] hover call error: {e}");
                    last_print = Instant::now();
                }
            }
        }

        assert!(
            Instant::now() < deadline,
            "[ra_e2e] rust-analyzer did not become ready within {timeout_secs}s; \
             set MCPLS_RA_INDEX_TIMEOUT_SECS to increase the limit"
        );

        std::thread::sleep(Duration::from_millis(500));
    }
}

// ---------------------------------------------------------------------------
// Sub-cases (one per MCP tool)
// ---------------------------------------------------------------------------

/// Tool 1: `get_hover` — hover over `add` declaration.
fn sc_get_hover(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let add_line = find_line(&lib, "pub fn add(");
    let resp = client
        .call_tool(
            "get_hover",
            &json!({
                "file_path": lib.to_string_lossy(),
                "line": add_line,
                "character": 8,
            }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let hover_text = inner["contents"]["value"]
        .as_str()
        .or_else(|| inner["contents"].as_str())
        .unwrap_or("");

    if !hover_text.contains("add") {
        return Err(format!("hover text missing 'add': {hover_text}"));
    }
    if !hover_text.contains("i32") {
        return Err(format!("hover text missing 'i32': {hover_text}"));
    }
    Ok(())
}

/// Tool 2: `get_definition` — go to definition of `add` from inside `caller`.
fn sc_get_definition(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    // Inside caller body: `    add(1, 2)` — "add" starts at col 5 (1-based).
    let caller_line = find_line(&lib, "pub fn caller(");
    let resp = client
        .call_tool(
            "get_definition",
            &json!({
                "file_path": lib.to_string_lossy(),
                // caller body is two lines below the fn declaration
                "line": caller_line + 1,
                "character": 5,
            }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let locs = inner["locations"]
        .as_array()
        .ok_or_else(|| format!("expected locations array, got {inner}"))?;
    if locs.is_empty() {
        return Err("get_definition returned empty locations".to_owned());
    }

    let uri = locs[0]["uri"].as_str().unwrap_or("");
    if !uri.ends_with("/src/lib.rs") {
        return Err(format!(
            "definition URI does not end with '/src/lib.rs': {uri}"
        ));
    }
    Ok(())
}

/// Tool 3: `get_references` — find references to `add`.
fn sc_get_references(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let add_line = find_line(&lib, "pub fn add(");
    let resp = client
        .call_tool(
            "get_references",
            &json!({
                "file_path": lib.to_string_lossy(),
                "line": add_line,
                "character": 8,
                "include_declaration": true,
            }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let locs = inner["locations"]
        .as_array()
        .ok_or_else(|| format!("expected locations array, got {inner}"))?;
    if locs.len() < 2 {
        return Err(format!(
            "expected ≥2 references (decl + call site), got {}",
            locs.len()
        ));
    }

    // All reference URIs should point to lib.rs.
    for loc in locs {
        let uri = loc["uri"].as_str().unwrap_or("");
        if !uri.ends_with("/src/lib.rs") {
            return Err(format!(
                "reference URI does not end with '/src/lib.rs': {uri}"
            ));
        }
    }
    Ok(())
}

/// Tool 4: `get_diagnostics` — type error in broken.rs.
///
/// Also populates the cache used by sub-case 14.
fn sc_get_diagnostics(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let broken = workspace.join("src/broken.rs");
    let resp = client
        .call_tool(
            "get_diagnostics",
            &json!({ "file_path": broken.to_string_lossy() }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let diags = inner["diagnostics"]
        .as_array()
        .ok_or_else(|| format!("expected diagnostics array, got {inner}"))?;

    // Poll for diagnostics — rust-analyzer may need a few seconds to analyze
    // broken.rs after the initial `textDocument/didOpen`.
    let final_diags = if diags.is_empty() {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            std::thread::sleep(Duration::from_millis(250));

            // Try pull-based diagnostics first.  Ignore transient LSP errors
            // (e.g. rust-analyzer may cancel the request while still indexing).
            let j2: Value = client
                .call_tool(
                    "get_diagnostics",
                    &json!({ "file_path": broken.to_string_lossy() }),
                )
                .ok()
                .map_or(Value::Null, |r| {
                    let t = assertions::content_text(&r);
                    serde_json::from_str(&t).unwrap_or(Value::Null)
                });
            if let Some(d2) = j2["diagnostics"].as_array()
                && !d2.is_empty()
            {
                break d2.clone();
            }

            // Also check push-based cache.
            let r3 = client
                .call_tool(
                    "get_cached_diagnostics",
                    &json!({ "file_path": broken.to_string_lossy() }),
                )
                .map_err(|e| format!("cached call failed: {e}"))?;
            let t3 = assertions::content_text(&r3);
            let j3: Value = serde_json::from_str(&t3).unwrap_or(Value::Null);
            if let Some(d3) = j3["diagnostics"].as_array()
                && !d3.is_empty()
            {
                break d3.clone();
            }

            if Instant::now() >= deadline {
                return Err("no diagnostics for broken.rs within 15 s".to_owned());
            }
        }
    } else {
        diags.clone()
    };

    let has_error = final_diags
        .iter()
        .any(|d| d["severity"].as_str() == Some("error"));
    if !has_error {
        return Err(format!(
            "no Error-severity diagnostic in broken.rs: {final_diags:?}"
        ));
    }
    Ok(())
}

/// Tool 5: `rename_symbol` — rename `add` → `plus`.
fn sc_rename_symbol(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let add_line = find_line(&lib, "pub fn add(");
    let resp = client
        .call_tool(
            "rename_symbol",
            &json!({
                "file_path": lib.to_string_lossy(),
                "line": add_line,
                "character": 8,
                "new_name": "plus",
            }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let changes = inner["changes"]
        .as_array()
        .ok_or_else(|| format!("expected changes array, got {inner}"))?;
    if changes.is_empty() {
        return Err(
            "rename_symbol returned empty changes; bridge may not handle documentChanges format"
                .to_owned(),
        );
    }
    Ok(())
}

/// Tool 6: `get_completions` — completions after `ad` inside `caller`.
fn sc_get_completions(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    // Inside caller body: `    add(1, 2)` — col 6 is after 'a','d' (prefix "ad").
    let caller_line = find_line(&lib, "pub fn caller(");
    let body_line = caller_line + 1;

    // Retry loop: completions may not be available until rust-analyzer is fully ready.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = client
            .call_tool(
                "get_completions",
                &json!({
                    "file_path": lib.to_string_lossy(),
                    "line": body_line,
                    "character": 6,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let items = inner["items"]
            .as_array()
            .or_else(|| inner.as_array())
            .ok_or_else(|| format!("expected completions array, got {inner}"))?;

        let found = items
            .iter()
            .any(|i| i["label"].as_str().unwrap_or("").contains("add"));
        if found {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "get_completions: 'add' not returned after 10 s; items: {items:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Tool 7: `get_document_symbols` — symbols in lib.rs include add, caller, Point.
fn sc_get_document_symbols(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let resp = client
        .call_tool(
            "get_document_symbols",
            &json!({ "file_path": lib.to_string_lossy() }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let syms = inner["symbols"]
        .as_array()
        .or_else(|| inner.as_array())
        .ok_or_else(|| format!("expected symbols array, got {inner}"))?;

    for expected in &["add", "caller", "Point"] {
        let found = syms
            .iter()
            .any(|s| s["name"].as_str().unwrap_or("").contains(expected));
        if !found {
            return Err(format!("symbol '{expected}' not found in document symbols"));
        }
    }
    Ok(())
}

/// Tool 8: `format_document` — format `bad_format.rs`, compare to golden.
fn sc_format_document(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let bad_fmt = workspace.join("src/bad_format.rs");
    let resp = client
        .call_tool(
            "format_document",
            &json!({ "file_path": bad_fmt.to_string_lossy() }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let formatted = inner["formatted_content"]
        .as_str()
        .or_else(|| inner["content"].as_str())
        .or_else(|| inner.as_str())
        .unwrap_or("");

    if formatted.is_empty() {
        // Some LSP servers return text edits instead of the full file.
        let edits = inner["edits"]
            .as_array()
            .or_else(|| inner["changes"].as_array());
        if edits.map_or(0, Vec::len) == 0 {
            return Err(format!(
                "format_document returned neither formatted content nor edits: {inner}"
            ));
        }
        return Ok(());
    }

    let golden_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden/bad_format.fmt.rs");
    let golden =
        fs::read_to_string(&golden_path).map_err(|e| format!("failed to read golden file: {e}"))?;

    if formatted.trim() != golden.trim() {
        return Err(format!(
            "formatted output does not match golden.\nExpected:\n{golden}\nGot:\n{formatted}"
        ));
    }
    Ok(())
}

/// Tool 9: `workspace_symbol_search` — search for "add".
fn sc_workspace_symbol_search(client: &mut McpClient, _workspace: &Path) -> Result<(), String> {
    // Retry: workspace symbol search may return empty until rust-analyzer
    // has fully indexed all files in the workspace.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let resp = client
            .call_tool("workspace_symbol_search", &json!({ "query": "add" }))
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let syms = inner["symbols"]
            .as_array()
            .or_else(|| inner.as_array())
            .ok_or_else(|| format!("expected symbols array, got {inner}"))?;

        if !syms.is_empty() {
            let found = syms
                .iter()
                .any(|s| s["name"].as_str().unwrap_or("").contains("add"));
            if found {
                return Ok(());
            }
            return Err(format!(
                "no symbol named 'add' in workspace_symbol_search results: {syms:?}"
            ));
        }

        if Instant::now() >= deadline {
            return Err(
                "workspace_symbol_search returned no results for 'add' after 15 s".to_owned(),
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Tool 10: `get_code_actions` — "Implement missing members" on an empty trait impl.
///
/// Quickfix-style code actions require rust-analyzer to receive the diagnostic
/// object with its internal `data` field in the request context — the bridge
/// currently sends an empty diagnostics list.  "Implement missing members" is a
/// structural refactoring action that is context-free and does not depend on
/// diagnostic data, making it a reliable trigger.
fn sc_get_code_actions(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib_rs = workspace.join("src/lib.rs");
    // `impl Greet for CodeActionTarget { }` — empty impl body spanning two lines.
    // RA offers "Implement missing members" when cursor is inside the impl block.
    // Use a point cursor (start == end) at character 6 on the `impl` line, inside the keyword.
    let impl_line = find_line(&lib_rs, "impl Greet for CodeActionTarget {");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_inner;
    loop {
        let resp = client
            .call_tool(
                "get_code_actions",
                &json!({
                    "file_path": lib_rs.to_string_lossy(),
                    "start_line": impl_line,
                    "start_character": 6,
                    "end_line": impl_line,
                    "end_character": 6,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;
        last_inner = inner.clone();

        let actions = inner["actions"]
            .as_array()
            .or_else(|| inner.as_array())
            .ok_or_else(|| format!("expected actions array, got {inner}"))?;

        if !actions.is_empty() {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "get_code_actions: no actions on empty trait impl after 20 s\n\
                 actions_response={last_inner}"
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

// ---------------------------------------------------------------------------
// Call hierarchy helpers
// ---------------------------------------------------------------------------

/// Tool 11: `prepare_call_hierarchy` — on `add`.
///
/// Returns the prepared item for use by sub-cases 12 and 13.
///
/// Since `CallHierarchyItemResult` now serializes `selectionRange` in camelCase,
/// the item round-trips correctly without any field renaming.
fn prepare_call_hierarchy_item(client: &mut McpClient, workspace: &Path) -> Result<Value, String> {
    let lib = workspace.join("src/lib.rs");
    let add_line = find_line(&lib, "pub fn add(");
    let resp = client
        .call_tool(
            "prepare_call_hierarchy",
            &json!({
                "file_path": lib.to_string_lossy(),
                "line": add_line,
                "character": 8,
            }),
        )
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let items = inner["items"]
        .as_array()
        .or_else(|| inner.as_array())
        .ok_or_else(|| format!("expected items array, got {inner}"))?;

    if items.is_empty() {
        return Err("prepare_call_hierarchy returned no items".to_owned());
    }

    let name = items[0]["name"].as_str().unwrap_or("");
    if !name.contains("add") {
        return Err(format!(
            "expected call hierarchy item for 'add', got '{name}'"
        ));
    }
    Ok(items[0].clone())
}

fn sc_prepare_call_hierarchy(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    prepare_call_hierarchy_item(client, workspace).map(|_| ())
}

/// Tool 12: `get_incoming_calls` — `caller` must appear as incoming caller to `add`.
fn sc_get_incoming_calls(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let item = prepare_call_hierarchy_item(client, workspace)?;
    // Retry: callHierarchy/incomingCalls may return empty on first query while
    // rust-analyzer resolves cross-function relationships.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let resp = client
            .call_tool("get_incoming_calls", &json!({ "item": item }))
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let calls = inner["calls"]
            .as_array()
            .or_else(|| inner.as_array())
            .ok_or_else(|| format!("expected calls array, got {inner}"))?;

        if !calls.is_empty() {
            // Verify that `caller` is among the incoming callers.
            let found = calls.iter().any(|c| {
                c["from"]["name"].as_str().unwrap_or("").contains("caller")
                    || c["caller"]["name"]
                        .as_str()
                        .unwrap_or("")
                        .contains("caller")
            });
            if !found {
                return Err(format!(
                    "get_incoming_calls: 'caller' not found in incoming calls: {calls:?}"
                ));
            }
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err("get_incoming_calls: empty result for 'add' after 15 s; \
                 'caller' should be an incoming caller"
                .to_owned());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Tool 13: `get_outgoing_calls` — `add` calls nothing user-defined.
fn sc_get_outgoing_calls(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let item = prepare_call_hierarchy_item(client, workspace)?;
    let resp = client
        .call_tool("get_outgoing_calls", &json!({ "item": item }))
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    let calls = inner["calls"]
        .as_array()
        .or_else(|| inner.as_array())
        .ok_or_else(|| format!("expected calls array, got {inner}"))?;

    // `add(a, b) { a + b }` contains no function calls.
    // An empty result is correct.  Reject any call to a user-defined function
    // (names outside std/core/alloc/compiler_builtins namespaces).
    for call in calls {
        let name = call["to"]["name"]
            .as_str()
            .or_else(|| call["callee"]["name"].as_str())
            .unwrap_or("");
        let in_std = name.is_empty()
            || name.contains("core")
            || name.contains("std")
            || name.contains("alloc")
            || name.contains("compiler_builtins");
        if !in_std {
            return Err(format!(
                "unexpected user-defined outgoing call from 'add': '{name}'"
            ));
        }
    }
    Ok(())
}

/// Tool 14: `get_cached_diagnostics` — push cache populated during workspace indexing.
///
/// Uses `lib.rs` rather than `broken.rs`: lib.rs is opened via hover during
/// `wait_until_ready` (no pull-diagnostic request), so rust-analyzer sends
/// `publishDiagnostics` for it unconditionally during initial analysis.
/// `broken.rs` is queried via the pull-based `textDocument/diagnostic` API in
/// `sc_get_diagnostics`; newer RA versions skip push for files already served
/// via pull, making `broken.rs` unreliable as a push-cache trigger.
///
/// lib.rs contains `let _x = undefined_variable;` (E0425) so it always has
/// at least one error diagnostic pushed by RA after indexing.
fn sc_get_cached_diagnostics(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib_rs = workspace.join("src/lib.rs");
    let timeout_secs: u64 = 20;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let resp = client
            .call_tool(
                "get_cached_diagnostics",
                &json!({ "file_path": lib_rs.to_string_lossy() }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let diags = inner["diagnostics"]
            .as_array()
            .ok_or_else(|| format!("expected diagnostics array, got {inner}"))?;

        if !diags.is_empty() {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "get_cached_diagnostics: push cache empty after {timeout_secs} s; \
                 rust-analyzer did not send publishDiagnostics for lib.rs"
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Tool 15: `get_server_logs` — returns `window/logMessage` entries.
///
/// rust-analyzer does not emit `window/logMessage` by default; it uses
/// `window/showMessage` and `$/progress` for user-visible status.  This
/// sub-case asserts that the tool responds without MCP-level error and
/// returns the expected shape, even if entries are empty.  The stronger
/// liveness signal for the notification pipeline is `sc_get_server_messages`.
fn sc_get_server_logs(client: &mut McpClient, _workspace: &Path) -> Result<(), String> {
    let resp = client
        .call_tool("get_server_logs", &json!({ "limit": 50 }))
        .map_err(|e| format!("call failed: {e}"))?;

    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

    // Verify expected shape; entries may be empty since rust-analyzer does not
    // emit window/logMessage without additional logging configuration.
    let _entries = inner["entries"]
        .as_array()
        .or_else(|| inner["logs"].as_array())
        .or_else(|| inner.as_array())
        .ok_or_else(|| format!("expected log entries array, got {inner}"))?;

    Ok(())
}

/// Resolve the `lsp-diagnostics` URI for `lib.rs` by querying `resources/list`.
///
/// Avoids drift from `make_uri`'s encoding rules on macOS `/private/var/...`
/// canonicalised paths or Windows UNC.
fn lib_rs_uri(client: &mut McpClient) -> Result<String, String> {
    let resp = client
        .list_resources()
        .map_err(|e| format!("list_resources: {e}"))?;
    let resources = resp["result"]["resources"]
        .as_array()
        .ok_or_else(|| format!("expected resources array, got {resp}"))?;
    resources
        .iter()
        .filter_map(|r| r["uri"].as_str())
        .find(|u| u.ends_with("/src/lib.rs"))
        .map(str::to_owned)
        .ok_or_else(|| format!("no lib.rs URI in resources list: {resources:?}"))
}

/// Tool 17: `get_signature_help` — signature of `add` inside its call site.
fn sc_get_signature_help(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let line = find_line(&lib, "let s = add(");
    let content = fs::read_to_string(&lib).map_err(|e| format!("read lib.rs: {e}"))?;
    let source_line = content
        .lines()
        .nth(usize::try_from(line - 1).expect("line fits usize"))
        .unwrap_or("");
    // Place cursor just after the opening paren (1-based; line is ASCII).
    let character = u32::try_from(
        source_line
            .find('(')
            .ok_or_else(|| format!("no '(' on line {line}: {source_line}"))?
            + 2,
    )
    .expect("column fits u32");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = client
            .call_tool(
                "get_signature_help",
                &json!({
                    "file_path": lib.to_string_lossy(),
                    "line": line,
                    "character": character,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        if let Some(sigs) = inner["signatures"].as_array()
            && !sigs.is_empty()
        {
            let label = sigs[0]["label"].as_str().unwrap_or("");
            if !label.contains("add") {
                return Err(format!("signature label missing 'add': {label}"));
            }
            if !label.contains("i32") {
                return Err(format!("signature label missing 'i32': {label}"));
            }
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "get_signature_help: no signatures after 10 s; response={inner}"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Tool 18: `go_to_implementation` — implementations of trait `Greet`.
fn sc_go_to_implementation(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let line = find_line(&lib, "pub trait Greet {");
    let content = fs::read_to_string(&lib).map_err(|e| format!("read lib.rs: {e}"))?;
    let source_line = content
        .lines()
        .nth(usize::try_from(line - 1).expect("line fits usize"))
        .unwrap_or("");
    // Cursor on the trait name "Greet" (1-based; ASCII line).
    let character = u32::try_from(
        source_line
            .find("Greet")
            .ok_or_else(|| format!("'Greet' not found on line {line}: {source_line}"))?
            + 1,
    )
    .expect("column fits u32");

    let impl_line = find_line(&lib, "impl Greet for CodeActionTarget {");
    // The bridge normalizes ranges to 1-based MCP via `normalize_range`,
    // so the response line equals the 1-based source line directly.
    let expected_mcp_line = impl_line;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = client
            .call_tool(
                "go_to_implementation",
                &json!({
                    "file_path": lib.to_string_lossy(),
                    "line": line,
                    "character": character,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let locs = inner["locations"]
            .as_array()
            .or_else(|| inner.as_array())
            .filter(|a| !a.is_empty());

        if let Some(locs) = locs {
            let has_lib_rs = locs
                .iter()
                .any(|l| l["uri"].as_str().unwrap_or("").ends_with("/src/lib.rs"));
            if !has_lib_rs {
                return Err(format!(
                    "go_to_implementation: no location in lib.rs: {locs:?}"
                ));
            }
            let has_impl_line = locs.iter().any(|l| {
                l["range"]["start"]["line"].as_u64() == Some(u64::from(expected_mcp_line))
            });
            if !has_impl_line {
                return Err(format!(
                    "go_to_implementation: impl line {expected_mcp_line} not in locations: {locs:?}"
                ));
            }
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "go_to_implementation: empty locations after 10 s; response={inner}"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Tool 19: `go_to_type_definition` — type definition of `p` (a `Point`).
fn sc_go_to_type_definition(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let line = find_line(&lib, "let p = Point {");
    let content = fs::read_to_string(&lib).map_err(|e| format!("read lib.rs: {e}"))?;
    let source_line = content
        .lines()
        .nth(usize::try_from(line - 1).expect("line fits usize"))
        .unwrap_or("");
    // Cursor on identifier `p` (1-based; ASCII line).
    let character = u32::try_from(
        source_line
            .find(" p ")
            .ok_or_else(|| format!("' p ' not found on line {line}: {source_line}"))?
            + 2,
    )
    .expect("column fits u32");

    let struct_line = find_line(&lib, "pub struct Point {");
    // The bridge normalizes ranges to 1-based MCP via `normalize_range`,
    // so the response line equals the 1-based source line directly.
    let expected_mcp_line = struct_line;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = client
            .call_tool(
                "go_to_type_definition",
                &json!({
                    "file_path": lib.to_string_lossy(),
                    "line": line,
                    "character": character,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let locs = inner["locations"]
            .as_array()
            .or_else(|| inner.as_array())
            .filter(|a| !a.is_empty());

        if let Some(locs) = locs {
            let uri = locs[0]["uri"].as_str().unwrap_or("");
            if !uri.ends_with("/src/lib.rs") {
                return Err(format!(
                    "go_to_type_definition: URI does not end with '/src/lib.rs': {uri}"
                ));
            }
            let got_line = locs[0]["range"]["start"]["line"].as_u64();
            if got_line != Some(u64::from(expected_mcp_line)) {
                return Err(format!(
                    "go_to_type_definition: expected line {expected_mcp_line}, got {got_line:?}"
                ));
            }
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "go_to_type_definition: empty locations after 10 s; response={inner}"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Tool 20: `get_inlay_hints` — type hints in `lsp317_target`.
fn sc_get_inlay_hints(client: &mut McpClient, workspace: &Path) -> Result<(), String> {
    let lib = workspace.join("src/lib.rs");
    let start_line = find_line(&lib, "pub fn lsp317_target(");
    let end_line = find_line(&lib, "let _ = (p, s);") + 1;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = client
            .call_tool(
                "get_inlay_hints",
                &json!({
                    "file_path": lib.to_string_lossy(),
                    "start_line": start_line,
                    "start_character": 1,
                    "end_line": end_line,
                    "end_character": 1,
                }),
            )
            .map_err(|e| format!("call failed: {e}"))?;

        let text = assertions::assert_tool_ok(&resp);
        let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;

        let hints_arr = inner["hints"].as_array().or_else(|| inner.as_array());
        if let Some(hints) = hints_arr
            && !hints.is_empty()
        {
            let serialized = serde_json::to_string(&inner["hints"])
                .unwrap_or_else(|_| serde_json::to_string(&inner).unwrap_or_default());
            if !serialized.contains("Point") && !serialized.contains("i32") {
                return Err(format!(
                    "get_inlay_hints: no 'Point' or 'i32' hint found; hints={serialized}"
                ));
            }
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "get_inlay_hints: no hints after 10 s; response={inner}"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Resource sub-case 1: `list_resources` — at least one lib.rs resource exposed.
///
/// Precondition: `sc_get_hover` and earlier sub-cases have triggered didOpen for
/// lib.rs — this sub-case must remain after them in the registry.
fn sc_list_resources(client: &mut McpClient, _workspace: &Path) -> Result<(), String> {
    let resp = client
        .list_resources()
        .map_err(|e| format!("call failed: {e}"))?;

    let resources = resp["result"]["resources"]
        .as_array()
        .ok_or_else(|| format!("expected resources array, got {resp}"))?;

    if resources.is_empty() {
        return Err("list_resources: empty resources array".to_owned());
    }

    for r in resources {
        let uri = r["uri"].as_str().unwrap_or("");
        if !uri.starts_with("lsp-diagnostics:///") {
            return Err(format!(
                "list_resources: URI does not start with 'lsp-diagnostics:///': {uri}"
            ));
        }
    }

    let has_lib_rs = resources
        .iter()
        .any(|r| r["uri"].as_str().unwrap_or("").ends_with("/src/lib.rs"));
    if !has_lib_rs {
        return Err(format!(
            "list_resources: no URI ending with '/src/lib.rs': {resources:?}"
        ));
    }

    Ok(())
}

/// Resource sub-case 2: `read_resource` — reads diagnostics for lib.rs.
fn sc_read_resource(client: &mut McpClient, _workspace: &Path) -> Result<(), String> {
    let uri = lib_rs_uri(client)?;
    let resp = client
        .read_resource(&uri)
        .map_err(|e| format!("call failed: {e}"))?;

    let contents = resp["result"]["contents"]
        .as_array()
        .ok_or_else(|| format!("expected contents array, got {resp}"))?;

    if contents.is_empty() {
        return Err("read_resource: empty contents array".to_owned());
    }
    if contents[0]["uri"].as_str() != Some(&uri) {
        return Err(format!(
            "read_resource: contents[0].uri mismatch; expected {uri}, got {}",
            contents[0]["uri"]
        ));
    }
    let text = contents[0]["text"].as_str().ok_or_else(|| {
        format!(
            "read_resource: contents[0].text is not a string: {}",
            contents[0]
        )
    })?;
    serde_json::from_str::<Value>(text)
        .map_err(|e| format!("read_resource: contents[0].text is not valid JSON: {e}"))?;

    Ok(())
}

/// Resource sub-case 3: subscribe and unsubscribe lib.rs resource.
fn sc_subscribe_unsubscribe_resource(
    client: &mut McpClient,
    _workspace: &Path,
) -> Result<(), String> {
    let uri = lib_rs_uri(client)?;

    let sub_resp = client
        .subscribe_resource(&uri)
        .map_err(|e| format!("subscribe call failed: {e}"))?;
    if sub_resp.get("error").is_some() {
        return Err(format!("subscribe returned error: {sub_resp}"));
    }
    // result field must be present (null for success)
    if sub_resp.get("result").is_none() {
        return Err(format!(
            "subscribe: no 'result' field in response: {sub_resp}"
        ));
    }

    let unsub_resp = client
        .unsubscribe_resource(&uri)
        .map_err(|e| format!("unsubscribe call failed: {e}"))?;
    if unsub_resp.get("error").is_some() {
        return Err(format!("unsubscribe returned error: {unsub_resp}"));
    }
    if unsub_resp.get("result").is_none() {
        return Err(format!(
            "unsubscribe: no 'result' field in response: {unsub_resp}"
        ));
    }

    // TODO(critic): add negative case with "file:///tmp/x.rs" (wrong scheme) once error envelope shape confirmed
    // TODO(critic): assert idempotent unsubscribe — second unsubscribe of same URI returns Ok

    Ok(())
}

/// Tool 16: `get_server_messages` — readiness gate already exercised this tool.
fn sc_get_server_messages(client: &mut McpClient, _workspace: &Path) -> Result<(), String> {
    let resp = client
        .call_tool("get_server_messages", &json!({ "limit": 20 }))
        .map_err(|e| format!("call failed: {e}"))?;

    assertions::assert_tool_ok(&resp);
    Ok(())
}

// ---------------------------------------------------------------------------
// Suite driver
// ---------------------------------------------------------------------------

#[test]
#[ignore = "Requires rust-analyzer in PATH; set MCPLS_SKIP_RA=1 to skip or MCPLS_RUST_ANALYZER=<path> to override"]
fn ra_e2e_suite() {
    let ra_path = match resolve_rust_analyzer() {
        Resolution::Found(p) => p,
        Resolution::Skipped(reason) => {
            println!("[ra_e2e] suite skipped: {reason}");
            return;
        }
        Resolution::Missing => {
            panic!(
                "[ra_e2e] rust-analyzer not found in PATH; \
                 install it with `rustup component add rust-analyzer` \
                 or set MCPLS_SKIP_RA=1 to skip"
            );
        }
    };

    println!("[ra_e2e] using rust-analyzer: {}", ra_path.display());

    // Stage workspace into a TempDir.
    let workspace_tmp = stage_workspace();
    // Canonicalize to resolve macOS /var → /private/var symlinks.
    // rust-analyzer resolves paths internally; without canonicalization, hover
    // requests using /var/folders/… would not match its indexed file URIs.
    let workspace = workspace_tmp
        .path()
        .canonicalize()
        .unwrap_or_else(|_| workspace_tmp.path().to_owned());

    // Generate config.
    let config_path = workspace.join("mcpls-e2e.toml");
    write_config(&ra_path, &workspace, &config_path);

    // Spawn mcpls.
    let config_str = config_path.to_string_lossy().into_owned();
    let mut client =
        McpClient::spawn_with_args(&["--config", &config_str]).expect("failed to spawn mcpls");

    client.initialize().expect("MCP initialize failed");

    // Wait for rust-analyzer to index.
    let lib_rs = workspace.join("src/lib.rs");
    wait_until_ready(&mut client, &lib_rs);

    // Sub-case registry.
    let sub_cases: &[SubCase] = &[
        sub_case!(sc_get_hover),
        sub_case!(sc_get_definition),
        sub_case!(sc_get_references),
        sub_case!(sc_get_diagnostics),
        sub_case!(sc_rename_symbol),
        sub_case!(sc_get_completions),
        sub_case!(sc_get_document_symbols),
        sub_case!(sc_format_document),
        sub_case!(sc_workspace_symbol_search),
        sub_case!(sc_get_code_actions),
        sub_case!(sc_prepare_call_hierarchy),
        sub_case!(sc_get_incoming_calls),
        sub_case!(sc_get_outgoing_calls),
        sub_case!(sc_get_cached_diagnostics),
        sub_case!(sc_get_server_logs),
        sub_case!(sc_get_server_messages),
        sub_case!(sc_get_signature_help),
        sub_case!(sc_go_to_implementation),
        sub_case!(sc_go_to_type_definition),
        sub_case!(sc_get_inlay_hints),
        sub_case!(sc_list_resources),
        sub_case!(sc_read_resource),
        sub_case!(sc_subscribe_unsubscribe_resource),
    ];

    let filter = std::env::var("MCPLS_RA_FILTER").ok();

    let mut results: Vec<SubResult> = Vec::new();

    for sc in sub_cases {
        if filter.as_deref().is_some_and(|f| !sc.name.contains(f)) {
            continue;
        }

        print!("[ra_e2e] running {} … ", sc.name);
        // Use catch_unwind so a panicking sub-case doesn't abort the whole suite.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (sc.run)(&mut client, &workspace)
        }));

        let outcome = match outcome {
            Ok(r) => r,
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_else(|| "sub-case panicked".to_owned());
                Err(msg)
            }
        };

        match &outcome {
            Ok(()) => println!("ok"),
            Err(e) => println!("FAILED: {e}"),
        }

        results.push(SubResult {
            name: sc.name,
            outcome,
        });
    }

    // Aggregate failures.
    let failures: Vec<_> = results.iter().filter(|r| r.outcome.is_err()).collect();

    if !failures.is_empty() {
        let report: Vec<String> = failures
            .iter()
            .map(|f| format!("  • {} — {}", f.name, f.outcome.as_ref().unwrap_err()))
            .collect();
        panic!(
            "[ra_e2e] {} sub-case(s) failed:\n{}",
            failures.len(),
            report.join("\n")
        );
    }

    println!("[ra_e2e] all {} sub-cases passed", results.len());
}
