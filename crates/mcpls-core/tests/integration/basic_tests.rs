use mcpls_core::bridge::{Translator, WorkspaceRoots};
use mcpls_core::config::{ServerConfig, ServerId, ToolKind, ToolRouter};

#[allow(unused, reason = "shared helpers; not every test uses all of them")]
use crate::common::test_utils::{
    config_fixture_path, rust_analyzer_available, rust_workspace_path,
};

#[test]
fn test_translator_creation() {
    let translator = Translator::new();
    assert_eq!(translator.open_document_paths().len(), 0);
}

#[test]
fn test_config_loading_minimal() {
    let config_path = config_fixture_path("minimal.toml");
    assert!(config_path.exists(), "Config fixture should exist");

    let content = std::fs::read_to_string(&config_path).expect("Failed to read config");
    let config: ServerConfig = toml::from_str(&content).expect("Failed to parse config");

    assert_eq!(config.lsp_servers.len(), 1);
    assert_eq!(config.lsp_servers[0].language_id, "rust");
}

#[test]
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
    let first = tempfile::tempdir().expect("tempdir");
    let second = tempfile::tempdir().expect("tempdir");
    let roots = [first.path(), second.path()]
        .map(|path| mcpls_core::config::ConfiguredRoot::new(path).expect("non-empty root"));

    translator.set_workspace_roots(WorkspaceRoots::from_configured(&roots).expect("roots resolve"));
}

#[test]
fn test_document_tracker_lazy_opening() {
    let translator = Translator::new();

    let test_file = rust_workspace_path().join("src/lib.rs");
    assert!(
        !translator.is_document_open(&test_file),
        "Document should not be open initially"
    );
}

/// #174 §11/§12: two servers sharing one language, routed via explicit
/// `name`/`handles`, load correctly and produce the expected per-tool router.
#[test]
fn test_two_server_routing_fixture_loads_and_routes() {
    let config_path = config_fixture_path("two_server_routing.toml");
    let config = ServerConfig::load_from(&config_path).expect("fixture should load");

    assert_eq!(config.lsp_servers.len(), 2);
    let router = ToolRouter::from_configs(&config.lsp_servers)
        .expect("two_server_routing.toml must not be ambiguous");

    assert_eq!(
        router.resolve("python", ToolKind::Diagnostics),
        Some(&ServerId::from_static("pylsp")),
        "pylsp explicitly claims diagnostics"
    );
    assert_eq!(
        router.resolve("python", ToolKind::Hover),
        Some(&ServerId::from_static("pyright")),
        "pyright is the catch-all for everything else"
    );
}
