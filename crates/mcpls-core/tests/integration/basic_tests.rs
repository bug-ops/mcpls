use std::path::PathBuf;

use mcpls_core::bridge::Translator;
use mcpls_core::config::ServerConfig;

#[allow(unused)]
use crate::common::test_utils::{
    config_fixture_path, rust_analyzer_available, rust_workspace_path,
};

#[test]
fn test_translator_creation() {
    let translator = Translator::new();
    assert!(translator.document_tracker().is_empty());
}

#[test]
#[allow(clippy::expect_used)]
fn test_config_loading_minimal() {
    let config_path = config_fixture_path("minimal.toml");
    assert!(config_path.exists(), "Config fixture should exist");

    let content = std::fs::read_to_string(&config_path).expect("Failed to read config");
    let config: ServerConfig = toml::from_str(&content).expect("Failed to parse config");

    assert_eq!(config.lsp_servers.len(), 1);
    assert_eq!(config.lsp_servers[0].language_id, "rust");
}

#[test]
#[allow(clippy::expect_used)]
fn test_config_loading_multi_language() {
    let config_path = config_fixture_path("multi_language.toml");
    assert!(config_path.exists(), "Config fixture should exist");

    let content = std::fs::read_to_string(&config_path).expect("Failed to read config");
    let config: ServerConfig = toml::from_str(&content).expect("Failed to parse config");

    assert_eq!(config.lsp_servers.len(), 3);
    assert_eq!(config.lsp_servers[0].language_id, "rust");
    assert_eq!(config.lsp_servers[1].language_id, "python");
    assert_eq!(config.lsp_servers[2].language_id, "typescript");
}

#[test]
fn test_rust_workspace_fixture_exists() {
    let workspace_path = rust_workspace_path();
    assert!(
        workspace_path.exists(),
        "Rust workspace fixture should exist"
    );

    let cargo_toml = workspace_path.join("Cargo.toml");
    assert!(cargo_toml.exists(), "Cargo.toml should exist in fixture");

    let lib_rs = workspace_path.join("src/lib.rs");
    assert!(lib_rs.exists(), "src/lib.rs should exist in fixture");
}

#[test]
fn test_workspace_roots_configuration() {
    let mut translator = Translator::new();
    let roots = vec![PathBuf::from("/tmp/test1"), PathBuf::from("/tmp/test2")];

    translator.set_workspace_roots(roots);
}

#[tokio::test]
async fn test_document_tracker_lazy_opening() {
    let mut translator = Translator::new();
    let tracker = translator.document_tracker_mut();

    let test_file = rust_workspace_path().join("src/lib.rs");
    assert!(
        !tracker.is_open(&test_file),
        "Document should not be open initially"
    );
}
