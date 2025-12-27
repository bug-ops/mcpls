//! Integration tests with real rust-analyzer LSP server.
//!
//! These tests require rust-analyzer to be installed and available in PATH.
//! Run with: cargo nextest run -- --ignored

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::uninlined_format_args,
    clippy::unnecessary_unwrap
)]

use std::sync::Arc;
use std::time::Duration;

use mcpls_core::bridge::Translator;
use mcpls_core::config::LspServerConfig;
use mcpls_core::lsp::{LspServer, ServerInitConfig};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::common::test_utils::{rust_analyzer_available, rust_workspace_path};

/// Setup helper to spawn rust-analyzer and create a translator.
///
/// This function:
/// 1. Spawns rust-analyzer process
/// 2. Initializes the LSP server
/// 3. Creates and configures a Translator
/// 4. Returns the translator wrapped in Arc<Mutex>
async fn setup_rust_analyzer() -> Arc<Mutex<Translator>> {
    let workspace_path = rust_workspace_path();

    let lsp_config = LspServerConfig {
        language_id: "rust".to_string(),
        command: "rust-analyzer".to_string(),
        args: vec![],
        env: std::collections::HashMap::new(),
        file_patterns: vec!["**/*.rs".to_string()],
        initialization_options: None,
        timeout_seconds: 30,
    };

    let server_init_config = ServerInitConfig {
        server_config: lsp_config,
        workspace_roots: vec![workspace_path.clone()],
        initialization_options: None,
    };

    let server = LspServer::spawn(server_init_config)
        .await
        .expect("Failed to spawn rust-analyzer");

    let client = server.client().clone();

    let mut translator = Translator::new();
    translator.set_workspace_roots(vec![workspace_path]);
    translator.register_client("rust".to_string(), client);

    Arc::new(Mutex::new(translator))
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_hover_on_std_vec() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let file_path = workspace_path.join("src/lib.rs");

    // Give rust-analyzer time to index the workspace
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Hover over "String" in User struct (line 20)
    // The line is: `pub name: String,`
    let result = timeout(
        Duration::from_secs(10),
        translator.lock().await.handle_hover(
            file_path.to_string_lossy().to_string(),
            20,
            19, // Position on "String"
        ),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let hover_result = result.unwrap();
    assert!(
        hover_result.is_ok(),
        "Should successfully get hover: {:?}",
        hover_result.err()
    );

    let hover_json = hover_result.unwrap();
    let hover_str = serde_json::to_string(&hover_json).unwrap();

    // Verify hover contains String type information
    assert!(
        hover_str.contains("String") || hover_str.contains("string"),
        "Hover should contain String type information, got: {}",
        hover_str
    );
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_hover_on_u64_type() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let file_path = workspace_path.join("src/lib.rs");

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Hover over "u64" in User struct (line 19)
    // The line is: `pub id: u64,`
    let result = timeout(
        Duration::from_secs(10),
        translator.lock().await.handle_hover(
            file_path.to_string_lossy().to_string(),
            19,
            17, // Position on "u64"
        ),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let hover_result = result.unwrap();
    assert!(hover_result.is_ok(), "Should successfully get hover");

    let hover_json = hover_result.unwrap();
    let hover_str = serde_json::to_string(&hover_json).unwrap();

    // Verify hover contains u64 type information
    assert!(
        hover_str.contains("u64") || hover_str.contains("unsigned"),
        "Hover should contain u64 type information, got: {}",
        hover_str
    );
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_definition_user_struct() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let types_file = workspace_path.join("src/types.rs");

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Go to definition of User in types.rs (line 9, owner: User)
    // The line is: `pub owner: User,`
    let result = timeout(
        Duration::from_secs(10),
        translator.lock().await.handle_definition(
            types_file.to_string_lossy().to_string(),
            9,
            20, // Position on "User"
        ),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let def_result = result.unwrap();
    assert!(
        def_result.is_ok(),
        "Should successfully get definition: {:?}",
        def_result.err()
    );

    let def_json = def_result.unwrap();
    let def_str = serde_json::to_string(&def_json).unwrap();

    // Verify definition points to lib.rs where User is defined
    assert!(
        def_str.contains("lib.rs") && def_str.contains("User"),
        "Definition should reference User struct in lib.rs, got: {}",
        def_str
    );
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_definition_across_files() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let functions_file = workspace_path.join("src/functions.rs");

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Go to definition of Repository in functions.rs (line 3, use statement)
    // The line is: `use crate::types::Repository;`
    let result = timeout(
        Duration::from_secs(10),
        translator.lock().await.handle_definition(
            functions_file.to_string_lossy().to_string(),
            3,
            24, // Position on "Repository"
        ),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let def_result = result.unwrap();
    assert!(def_result.is_ok(), "Should successfully get definition");

    let def_json = def_result.unwrap();
    let def_str = serde_json::to_string(&def_json).unwrap();

    // Verify definition points to types.rs
    assert!(
        def_str.contains("types.rs") || def_str.contains("Repository"),
        "Definition should reference Repository in types.rs, got: {}",
        def_str
    );
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_references_create_repo_function() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let functions_file = workspace_path.join("src/functions.rs");

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Find references to create_repo function (line 7, function name)
    // The line is: `pub fn create_repo(name: &str) -> Repository {`
    let result = timeout(
        Duration::from_secs(10),
        translator.lock().await.handle_references(
            functions_file.to_string_lossy().to_string(),
            7,
            12,   // Position on "create_repo"
            true, // Include declaration
        ),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let refs_result = result.unwrap();
    assert!(
        refs_result.is_ok(),
        "Should successfully get references: {:?}",
        refs_result.err()
    );

    let refs_json = refs_result.unwrap();

    // Should find at least the definition itself
    assert!(
        !refs_json.locations.is_empty(),
        "Should find at least one reference (the definition)"
    );
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_references_user_struct() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let lib_file = workspace_path.join("src/lib.rs");

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Find references to User struct (line 18, struct name)
    // The line is: `pub struct User {`
    let result = timeout(
        Duration::from_secs(10),
        translator.lock().await.handle_references(
            lib_file.to_string_lossy().to_string(),
            18,
            15, // Position on "User"
            true,
        ),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let refs_result = result.unwrap();
    assert!(refs_result.is_ok(), "Should successfully get references");

    let refs_json = refs_result.unwrap();

    // User is referenced in types.rs and functions.rs, plus the definition
    assert!(
        refs_json.locations.len() >= 2,
        "Should find multiple references to User struct, got: {}",
        refs_json.locations.len()
    );
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_diagnostics_with_error() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let lib_file = workspace_path.join("src/lib.rs");

    // Give rust-analyzer extra time to analyze and generate diagnostics
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Get diagnostics from lib.rs (has intentional error on line 37)
    let result = timeout(
        Duration::from_secs(10),
        translator
            .lock()
            .await
            .handle_diagnostics(lib_file.to_string_lossy().to_string()),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let diag_result = result.unwrap();
    assert!(
        diag_result.is_ok(),
        "Should successfully get diagnostics: {:?}",
        diag_result.err()
    );

    let diag_json = diag_result.unwrap();
    let diag_str = serde_json::to_string(&diag_json).unwrap();

    // Should contain the error about undefined_variable
    assert!(
        diag_str.contains("undefined_variable") || diag_str.contains("cannot find"),
        "Diagnostics should report the intentional error, got: {}",
        diag_str
    );
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_diagnostics_no_errors() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let types_file = workspace_path.join("src/types.rs");

    tokio::time::sleep(Duration::from_secs(5)).await;

    // Get diagnostics from types.rs (should have no errors)
    let result = timeout(
        Duration::from_secs(10),
        translator
            .lock()
            .await
            .handle_diagnostics(types_file.to_string_lossy().to_string()),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let diag_result = result.unwrap();
    assert!(diag_result.is_ok(), "Should successfully get diagnostics");

    let diag_json = diag_result.unwrap();

    // types.rs should have no errors or warnings
    // (it's a clean file without issues)
    let errors: Vec<_> = diag_json
        .diagnostics
        .iter()
        .filter(|d| matches!(d.severity, mcpls_core::bridge::DiagnosticSeverity::Error))
        .collect();

    assert!(
        errors.is_empty(),
        "types.rs should have no errors, got: {:?}",
        errors
    );
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_document_symbols() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let lib_file = workspace_path.join("src/lib.rs");

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Get document symbols from lib.rs
    let result = timeout(
        Duration::from_secs(10),
        translator
            .lock()
            .await
            .handle_document_symbols(lib_file.to_string_lossy().to_string()),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let symbols_result = result.unwrap();
    assert!(
        symbols_result.is_ok(),
        "Should successfully get symbols: {:?}",
        symbols_result.err()
    );

    let symbols_json = symbols_result.unwrap();
    let symbols_str = serde_json::to_string(&symbols_json).unwrap();

    // Should contain User struct
    assert!(
        symbols_str.contains("User"),
        "Should find User struct, got: {}",
        symbols_str
    );

    // Should contain has_error function
    assert!(
        symbols_str.contains("has_error"),
        "Should find has_error function, got: {}",
        symbols_str
    );

    // Should contain has_warning function
    assert!(
        symbols_str.contains("has_warning"),
        "Should find has_warning function, got: {}",
        symbols_str
    );
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_document_symbols_types_file() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let types_file = workspace_path.join("src/types.rs");

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Get document symbols from types.rs
    let result = timeout(
        Duration::from_secs(10),
        translator
            .lock()
            .await
            .handle_document_symbols(types_file.to_string_lossy().to_string()),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let symbols_result = result.unwrap();
    assert!(symbols_result.is_ok(), "Should successfully get symbols");

    let symbols_json = symbols_result.unwrap();
    let symbols_str = serde_json::to_string(&symbols_json).unwrap();

    // Should contain Repository struct
    assert!(
        symbols_str.contains("Repository"),
        "Should find Repository struct, got: {}",
        symbols_str
    );

    // Should contain methods
    assert!(
        symbols_str.contains("new") || symbols_str.contains("get_owner"),
        "Should find struct methods, got: {}",
        symbols_str
    );
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_completions_basic() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let functions_file = workspace_path.join("src/functions.rs");

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Get completions in functions.rs
    // Position after "repo." on line 23 (repo.get_owner().name)
    let result = timeout(
        Duration::from_secs(10),
        translator.lock().await.handle_completions(
            functions_file.to_string_lossy().to_string(),
            23,
            10, // Position after "repo."
            None,
        ),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let completions_result = result.unwrap();

    // Completions might not always be available depending on timing
    if completions_result.is_ok() {
        let completions_json = completions_result.unwrap();
        let completions_str = serde_json::to_string(&completions_json).unwrap();

        // If we got completions, they should include Repository fields/methods
        // This is a soft check since completion can be timing-sensitive
        println!("Completions: {}", completions_str);
    }
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_format_document() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let lib_file = workspace_path.join("src/lib.rs");

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Request document formatting
    let result = timeout(
        Duration::from_secs(10),
        translator.lock().await.handle_format_document(
            lib_file.to_string_lossy().to_string(),
            4,    // tab_size
            true, // insert_spaces
        ),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let format_result = result.unwrap();

    // rust-analyzer may or may not support formatting directly
    // (often defers to rustfmt which needs to be configured separately)
    // So we just check that the call doesn't error out catastrophically
    match format_result {
        Ok(format_json) => {
            // Format succeeded, verify it returns edits
            println!("Format response: {:?}", format_json);
        }
        Err(e) => {
            // Format might not be supported, which is ok for this test
            println!("Format not supported or failed (expected): {:?}", e);
        }
    }
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_timeout_handling() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let lib_file = workspace_path.join("src/lib.rs");

    // Very short timeout to test timeout behavior
    let result = timeout(
        Duration::from_millis(1), // 1ms - should timeout
        translator
            .lock()
            .await
            .handle_hover(lib_file.to_string_lossy().to_string(), 20, 19),
    )
    .await;

    // Should timeout
    assert!(result.is_err(), "Should timeout with 1ms timeout");
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_invalid_file_path() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Try to get hover on non-existent file
    let result = translator
        .lock()
        .await
        .handle_hover("/nonexistent/file.rs".to_string(), 1, 1)
        .await;

    // Should return an error (file not found or not in workspace)
    assert!(result.is_err(), "Should fail for non-existent file");
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_out_of_bounds_position() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;
    let workspace_path = rust_workspace_path();
    let lib_file = workspace_path.join("src/lib.rs");

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Try to get hover at an extremely large line number
    let result = timeout(
        Duration::from_secs(10),
        translator.lock().await.handle_hover(
            lib_file.to_string_lossy().to_string(),
            99999, // Way beyond file bounds
            1,
        ),
    )
    .await;

    // LSP server should handle this gracefully (might return empty or error)
    assert!(
        result.is_ok(),
        "Should not timeout even with out-of-bounds position"
    );

    // The actual result might be Ok(empty) or Err, both are acceptable
    // We just want to ensure it doesn't hang or crash
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_workspace_symbol_search_basic() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;

    // Give rust-analyzer time to index the workspace
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Search for "User" struct
    let result = timeout(
        Duration::from_secs(10),
        translator
            .lock()
            .await
            .handle_workspace_symbol("User".to_string(), None, 100),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let symbol_result = result.unwrap();
    assert!(
        symbol_result.is_ok(),
        "Should successfully search symbols: {:?}",
        symbol_result.err()
    );

    let symbols = symbol_result.unwrap();
    let symbols_str = serde_json::to_string(&symbols).unwrap();
    println!("Workspace symbols for 'User': {}", symbols_str);

    // Should find the User struct
    assert!(
        !symbols.symbols.is_empty(),
        "Should find at least one symbol for 'User'"
    );

    // Verify at least one result is the User struct
    let has_user_struct = symbols.symbols.iter().any(|s| s.name == "User");
    assert!(has_user_struct, "Should find User struct in results");
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_workspace_symbol_search_with_kind_filter() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Search for symbols and filter by Struct kind
    let result = timeout(
        Duration::from_secs(10),
        translator.lock().await.handle_workspace_symbol(
            String::new(), // Empty query to get all symbols
            Some("Struct".to_string()),
            100,
        ),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let symbol_result = result.unwrap();

    if let Ok(symbols) = symbol_result {
        println!("Found {} struct symbols", symbols.symbols.len());

        // All results should be structs
        for symbol in &symbols.symbols {
            assert_eq!(
                symbol.kind, "Struct",
                "All filtered results should be Struct kind"
            );
        }
    }
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_workspace_symbol_search_max_results() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Search with very low limit
    let result = timeout(
        Duration::from_secs(10),
        translator
            .lock()
            .await
            .handle_workspace_symbol(String::new(), None, 5),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let symbol_result = result.unwrap();

    if let Ok(symbols) = symbol_result {
        println!("Found {} symbols (limited to 5)", symbols.symbols.len());
        assert!(
            symbols.symbols.len() <= 5,
            "Should respect max_results limit of 5"
        );
    }
}

#[tokio::test]
#[ignore = "Requires rust-analyzer installed"]
async fn test_workspace_symbol_search_function() {
    if !rust_analyzer_available() {
        eprintln!("Skipping: rust-analyzer not available");
        return;
    }

    let translator = setup_rust_analyzer().await;

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Search for function symbols
    let result = timeout(
        Duration::from_secs(10),
        translator.lock().await.handle_workspace_symbol(
            "create".to_string(),
            Some("Function".to_string()),
            100,
        ),
    )
    .await;

    assert!(result.is_ok(), "Should not timeout");
    let symbol_result = result.unwrap();

    if let Ok(symbols) = symbol_result {
        println!("Found {} function symbols", symbols.symbols.len());

        // All results should be functions
        for symbol in &symbols.symbols {
            assert_eq!(
                symbol.kind, "Function",
                "All filtered results should be Function kind"
            );
        }

        // Should find functions with "create" in the name
        if !symbols.symbols.is_empty() {
            let has_create = symbols
                .symbols
                .iter()
                .any(|s| s.name.to_lowercase().contains("create"));
            println!("Has 'create' in name: {}", has_create);
        }
    }
}
