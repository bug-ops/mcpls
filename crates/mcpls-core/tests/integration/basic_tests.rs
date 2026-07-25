use std::path::PathBuf;

use mcpls_core::bridge::Translator;
use mcpls_core::config::{ServerConfig, ServerId, ToolKind, ToolRouter};

#[allow(unused)]
use crate::common::test_utils::{
    config_fixture_path, rust_analyzer_available, rust_workspace_path,
};

#[tokio::test]
async fn test_translator_creation() {
    let translator = Translator::new();
    assert!(translator.open_document_paths().await.is_empty());
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
    let translator = Translator::new();

    let test_file = rust_workspace_path().join("src/lib.rs");
    assert!(
        !translator.is_document_open(&test_file).await,
        "Document should not be open initially"
    );
}

/// #174 §11/§12: two servers sharing one language, routed via explicit
/// `name`/`handles`, load correctly and produce the expected per-tool router.
#[test]
#[allow(clippy::expect_used)]
fn test_two_server_routing_fixture_loads_and_routes() {
    let config_path = config_fixture_path("two_server_routing.toml");
    let config = ServerConfig::load_from(&config_path).expect("fixture should load");

    assert_eq!(config.lsp_servers.len(), 2);
    let router = ToolRouter::from_configs(&config.lsp_servers)
        .expect("two_server_routing.toml must not be ambiguous");

    assert_eq!(
        router.resolve("python", ToolKind::Diagnostics),
        Some(&ServerId::from("pylsp")),
        "pylsp explicitly claims diagnostics"
    );
    assert_eq!(
        router.resolve("python", ToolKind::Hover),
        Some(&ServerId::from("pyright")),
        "pyright is the catch-all for everything else"
    );
}

/// #174 §5/§12 (S3 regression): two servers for one language with mutually
/// exclusive `heuristics.project_markers` must still load successfully --
/// the workspace-scoped ambiguity rules apply only to servers that are both
/// applicable in the same workspace, and at most one of these ever is.
#[test]
#[allow(clippy::expect_used)]
fn test_mutually_exclusive_heuristics_fixture_loads() {
    let config_path = config_fixture_path("mutually_exclusive_heuristics.toml");
    let config = ServerConfig::load_from(&config_path)
        .expect("mutually exclusive heuristics must load, not just parse");

    assert_eq!(config.lsp_servers.len(), 2);

    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tmp.path().join("pyrightconfig.json"), "{}").expect("write marker");

    let applicable: Vec<_> = config
        .lsp_servers
        .iter()
        .filter(|s| s.should_spawn(tmp.path(), None))
        .collect();
    assert_eq!(
        applicable.len(),
        1,
        "only the server whose marker exists should be applicable in this workspace"
    );
    assert_eq!(applicable[0].command, "pyright-langserver");

    // The single applicable server never collides with itself.
    ToolRouter::from_configs(applicable)
        .expect("a single applicable server must never be ambiguous");
}

/// #174 §5/§12 (S3 regression, other half): when *both* mutually-exclusive
/// markers happen to be present in one workspace, both servers become
/// applicable and are genuinely ambiguous (neither has a `name` or
/// `handles`, so they collide on both the derived `ServerId` and the
/// catch-all rule) -- `ToolRouter::from_configs` must reject this with a
/// startup error rather than silently picking one.
#[test]
#[allow(clippy::expect_used)]
fn test_mutually_exclusive_heuristics_fixture_errors_when_both_applicable() {
    let config_path = config_fixture_path("mutually_exclusive_heuristics.toml");
    let config = ServerConfig::load_from(&config_path).expect("fixture should load");

    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tmp.path().join("pyrightconfig.json"), "{}").expect("write marker");
    std::fs::write(tmp.path().join("setup.cfg"), "").expect("write marker");

    let applicable: Vec<_> = config
        .lsp_servers
        .iter()
        .filter(|s| s.should_spawn(tmp.path(), None))
        .collect();
    assert_eq!(
        applicable.len(),
        2,
        "both markers present must make both servers applicable"
    );

    let err = ToolRouter::from_configs(applicable)
        .expect_err("two applicable nameless servers for one language must be ambiguous");
    assert!(matches!(err, mcpls_core::error::Error::InvalidConfig(_)));
}
