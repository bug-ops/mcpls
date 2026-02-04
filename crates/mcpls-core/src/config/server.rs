//! LSP server configuration types.

use std::collections::HashMap;
use std::path::Path;

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

/// Default max depth for recursive marker search.
pub const DEFAULT_HEURISTICS_MAX_DEPTH: usize = 10;

/// Directories excluded from recursive marker search.
/// These are well-known directories that should never contain project markers.
const EXCLUDED_DIRECTORIES: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    "build",
    "dist",
    ".cargo",
    ".rustup",
    "vendor",
    "coverage",
    ".next",
    ".nuxt",
];

/// Heuristics for determining if an LSP server should be spawned.
///
/// Used to prevent spawning servers in projects where they are not applicable
/// (e.g., rust-analyzer in a Python-only project).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerHeuristics {
    /// Files or directories that indicate this server is applicable.
    /// The server will spawn if ANY of these markers exist in the workspace root.
    /// If empty, the server will always attempt to spawn.
    #[serde(default)]
    pub project_markers: Vec<String>,
}

impl ServerHeuristics {
    /// Create heuristics with the given project markers.
    #[must_use]
    pub fn with_markers<I, S>(markers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            project_markers: markers.into_iter().map(Into::into).collect(),
        }
    }

    /// Check if any marker exists at the given workspace root.
    ///
    /// Returns `true` if:
    /// - No markers are defined (empty = always applicable)
    /// - At least one marker file/directory exists
    #[must_use]
    pub fn is_applicable(&self, workspace_root: &Path) -> bool {
        if self.project_markers.is_empty() {
            return true;
        }
        self.project_markers
            .iter()
            .any(|marker| workspace_root.join(marker).exists())
    }

    /// Check if any marker exists anywhere in the workspace tree.
    ///
    /// Recursively searches the workspace for project markers, excluding
    /// well-known directories like `node_modules`, `target`, `.git`, etc.
    ///
    /// # Arguments
    ///
    /// * `workspace_root` - Root directory to search from
    /// * `max_depth` - Maximum recursion depth (default: 10)
    ///
    /// # Returns
    ///
    /// `true` if any marker is found, `false` otherwise.
    #[must_use]
    pub fn is_applicable_recursive(&self, workspace_root: &Path, max_depth: Option<usize>) -> bool {
        if self.project_markers.is_empty() {
            return true;
        }

        // First check the root level (fast path)
        if self.is_applicable(workspace_root) {
            return true;
        }

        let depth = max_depth.unwrap_or(DEFAULT_HEURISTICS_MAX_DEPTH);
        self.find_any_marker_recursive(workspace_root, depth)
    }

    /// Search recursively for any marker file.
    fn find_any_marker_recursive(&self, workspace_root: &Path, max_depth: usize) -> bool {
        let mut builder = WalkBuilder::new(workspace_root);
        builder
            .max_depth(Some(max_depth))
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(false)
            .follow_links(false)
            .standard_filters(false)
            .filter_entry(|entry| {
                // Skip excluded directories entirely (prevents descending into them)
                if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                    if let Some(name) = entry.file_name().to_str() {
                        if EXCLUDED_DIRECTORIES.contains(&name) {
                            return false;
                        }
                    }
                }
                true
            });

        for entry in builder.build().flatten() {
            let path = entry.path();

            // Check if this entry matches any marker
            if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                if self.project_markers.iter().any(|m| m == file_name) {
                    return true;
                }
            }
        }

        false
    }
}

/// Configuration for a single LSP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspServerConfig {
    /// Language identifier (e.g., "rust", "python", "typescript").
    pub language_id: String,

    /// Command to start the LSP server.
    pub command: String,

    /// Arguments to pass to the LSP server command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables for the LSP server process.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// File patterns this server handles (glob patterns).
    #[serde(default)]
    pub file_patterns: Vec<String>,

    /// LSP initialization options (server-specific).
    #[serde(default)]
    pub initialization_options: Option<serde_json::Value>,

    /// Request timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,

    /// Heuristics for determining if this server should be spawned.
    /// If not specified, the server will always attempt to spawn.
    #[serde(default)]
    pub heuristics: Option<ServerHeuristics>,
}

const fn default_timeout() -> u64 {
    30
}

impl LspServerConfig {
    /// Check if this server should be spawned for the given workspace.
    ///
    /// Uses recursive marker search to detect nested projects.
    ///
    /// # Arguments
    ///
    /// * `workspace_root` - Root directory of the workspace
    /// * `max_depth` - Maximum depth for recursive search (default: 10)
    #[must_use]
    pub fn should_spawn(&self, workspace_root: &Path, max_depth: Option<usize>) -> bool {
        self.heuristics
            .as_ref()
            .is_none_or(|h| h.is_applicable_recursive(workspace_root, max_depth))
    }

    /// Create a default configuration for rust-analyzer.
    #[must_use]
    pub fn rust_analyzer() -> Self {
        Self {
            language_id: "rust".to_string(),
            command: "rust-analyzer".to_string(),
            args: vec![],
            env: HashMap::new(),
            file_patterns: vec!["**/*.rs".to_string()],
            initialization_options: None,
            timeout_seconds: default_timeout(),
            heuristics: Some(ServerHeuristics::with_markers([
                "Cargo.toml",
                "rust-toolchain.toml",
            ])),
        }
    }

    /// Create a default configuration for pyright.
    #[must_use]
    pub fn pyright() -> Self {
        Self {
            language_id: "python".to_string(),
            command: "pyright-langserver".to_string(),
            args: vec!["--stdio".to_string()],
            env: HashMap::new(),
            file_patterns: vec!["**/*.py".to_string()],
            initialization_options: None,
            timeout_seconds: default_timeout(),
            heuristics: Some(ServerHeuristics::with_markers([
                "pyproject.toml",
                "setup.py",
                "requirements.txt",
                "pyrightconfig.json",
            ])),
        }
    }

    /// Create a default configuration for TypeScript language server.
    #[must_use]
    pub fn typescript() -> Self {
        Self {
            language_id: "typescript".to_string(),
            command: "typescript-language-server".to_string(),
            args: vec!["--stdio".to_string()],
            env: HashMap::new(),
            file_patterns: vec!["**/*.ts".to_string(), "**/*.tsx".to_string()],
            initialization_options: None,
            timeout_seconds: default_timeout(),
            heuristics: Some(ServerHeuristics::with_markers([
                "package.json",
                "tsconfig.json",
                "jsconfig.json",
            ])),
        }
    }

    /// Create a default configuration for gopls.
    #[must_use]
    pub fn gopls() -> Self {
        Self {
            language_id: "go".to_string(),
            command: "gopls".to_string(),
            args: vec!["serve".to_string()],
            env: HashMap::new(),
            file_patterns: vec!["**/*.go".to_string()],
            initialization_options: None,
            timeout_seconds: default_timeout(),
            heuristics: Some(ServerHeuristics::with_markers(["go.mod", "go.sum"])),
        }
    }

    /// Create a default configuration for clangd.
    #[must_use]
    pub fn clangd() -> Self {
        Self {
            language_id: "cpp".to_string(),
            command: "clangd".to_string(),
            args: vec![],
            env: HashMap::new(),
            file_patterns: vec![
                "**/*.c".to_string(),
                "**/*.cpp".to_string(),
                "**/*.h".to_string(),
                "**/*.hpp".to_string(),
            ],
            initialization_options: None,
            timeout_seconds: default_timeout(),
            heuristics: Some(ServerHeuristics::with_markers([
                "CMakeLists.txt",
                "compile_commands.json",
                "Makefile",
                ".clangd",
            ])),
        }
    }

    /// Create a default configuration for zls.
    #[must_use]
    pub fn zls() -> Self {
        Self {
            language_id: "zig".to_string(),
            command: "zls".to_string(),
            args: vec![],
            env: HashMap::new(),
            file_patterns: vec!["**/*.zig".to_string()],
            initialization_options: None,
            timeout_seconds: default_timeout(),
            heuristics: Some(ServerHeuristics::with_markers([
                "build.zig",
                "build.zig.zon",
            ])),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_rust_analyzer_defaults() {
        let config = LspServerConfig::rust_analyzer();

        assert_eq!(config.language_id, "rust");
        assert_eq!(config.command, "rust-analyzer");
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
        assert_eq!(config.file_patterns, vec!["**/*.rs"]);
        assert!(config.initialization_options.is_none());
        assert_eq!(config.timeout_seconds, 30);
    }

    #[test]
    fn test_pyright_defaults() {
        let config = LspServerConfig::pyright();

        assert_eq!(config.language_id, "python");
        assert_eq!(config.command, "pyright-langserver");
        assert_eq!(config.args, vec!["--stdio"]);
        assert!(config.env.is_empty());
        assert_eq!(config.file_patterns, vec!["**/*.py"]);
        assert!(config.initialization_options.is_none());
        assert_eq!(config.timeout_seconds, 30);
    }

    #[test]
    fn test_typescript_defaults() {
        let config = LspServerConfig::typescript();

        assert_eq!(config.language_id, "typescript");
        assert_eq!(config.command, "typescript-language-server");
        assert_eq!(config.args, vec!["--stdio"]);
        assert!(config.env.is_empty());
        assert_eq!(config.file_patterns, vec!["**/*.ts", "**/*.tsx"]);
        assert!(config.initialization_options.is_none());
        assert_eq!(config.timeout_seconds, 30);
    }

    #[test]
    fn test_default_timeout() {
        assert_eq!(default_timeout(), 30);
    }

    #[test]
    fn test_custom_config() {
        let mut env = HashMap::new();
        env.insert("RUST_LOG".to_string(), "debug".to_string());

        let config = LspServerConfig {
            language_id: "custom".to_string(),
            command: "custom-lsp".to_string(),
            args: vec!["--flag".to_string()],
            env: env.clone(),
            file_patterns: vec!["**/*.custom".to_string()],
            initialization_options: Some(serde_json::json!({"key": "value"})),
            timeout_seconds: 60,
            heuristics: None,
        };

        assert_eq!(config.language_id, "custom");
        assert_eq!(config.command, "custom-lsp");
        assert_eq!(config.args, vec!["--flag"]);
        assert_eq!(config.env.get("RUST_LOG"), Some(&"debug".to_string()));
        assert_eq!(config.file_patterns, vec!["**/*.custom"]);
        assert!(config.initialization_options.is_some());
        assert_eq!(config.timeout_seconds, 60);
    }

    #[test]
    fn test_serde_roundtrip() {
        let original = LspServerConfig::rust_analyzer();

        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: LspServerConfig = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.language_id, original.language_id);
        assert_eq!(deserialized.command, original.command);
        assert_eq!(deserialized.args, original.args);
        assert_eq!(deserialized.timeout_seconds, original.timeout_seconds);
    }

    #[test]
    fn test_clone() {
        let config = LspServerConfig::rust_analyzer();
        let cloned = config.clone();

        assert_eq!(cloned.language_id, config.language_id);
        assert_eq!(cloned.command, config.command);
        assert_eq!(cloned.timeout_seconds, config.timeout_seconds);
    }

    #[test]
    fn test_empty_env() {
        let config = LspServerConfig::rust_analyzer();
        assert!(config.env.is_empty());
    }

    #[test]
    fn test_multiple_file_patterns() {
        let config = LspServerConfig::typescript();
        assert_eq!(config.file_patterns.len(), 2);
        assert!(config.file_patterns.contains(&"**/*.ts".to_string()));
        assert!(config.file_patterns.contains(&"**/*.tsx".to_string()));
    }

    #[test]
    fn test_initialization_options_none_by_default() {
        let configs = vec![
            LspServerConfig::rust_analyzer(),
            LspServerConfig::pyright(),
            LspServerConfig::typescript(),
        ];

        for config in configs {
            assert!(config.initialization_options.is_none());
        }
    }

    // Heuristics tests
    #[test]
    fn test_heuristics_empty_always_applicable() {
        let heuristics = ServerHeuristics::default();
        let tmp = TempDir::new().unwrap();
        assert!(heuristics.is_applicable(tmp.path()));
    }

    #[test]
    fn test_heuristics_marker_present() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        assert!(heuristics.is_applicable(tmp.path()));
    }

    #[test]
    fn test_heuristics_marker_absent() {
        let tmp = TempDir::new().unwrap();
        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        assert!(!heuristics.is_applicable(tmp.path()));
    }

    #[test]
    fn test_heuristics_any_marker_matches() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("setup.py"), "").unwrap();

        let heuristics =
            ServerHeuristics::with_markers(["pyproject.toml", "setup.py", "requirements.txt"]);
        assert!(heuristics.is_applicable(tmp.path()));
    }

    #[test]
    fn test_should_spawn_without_heuristics() {
        let config = LspServerConfig {
            language_id: "test".to_string(),
            command: "test-lsp".to_string(),
            args: vec![],
            env: HashMap::new(),
            file_patterns: vec![],
            initialization_options: None,
            timeout_seconds: 30,
            heuristics: None,
        };

        let tmp = TempDir::new().unwrap();
        assert!(config.should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_should_spawn_with_heuristics() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();

        let config = LspServerConfig::rust_analyzer();
        assert!(config.should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_should_not_spawn_without_markers() {
        let tmp = TempDir::new().unwrap();
        let config = LspServerConfig::rust_analyzer();
        assert!(!config.should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_heuristics_serde_roundtrip() {
        let heuristics = ServerHeuristics::with_markers(["Cargo.toml", "rust-toolchain.toml"]);
        let json = serde_json::to_string(&heuristics).unwrap();
        let deserialized: ServerHeuristics = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.project_markers, heuristics.project_markers);
    }

    #[test]
    fn test_default_rust_analyzer_heuristics() {
        let config = LspServerConfig::rust_analyzer();
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&"Cargo.toml".to_string()));
    }

    #[test]
    fn test_gopls_defaults() {
        let config = LspServerConfig::gopls();

        assert_eq!(config.language_id, "go");
        assert_eq!(config.command, "gopls");
        assert_eq!(config.args, vec!["serve"]);
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&"go.mod".to_string()));
        assert!(markers.contains(&"go.sum".to_string()));
    }

    #[test]
    fn test_clangd_defaults() {
        let config = LspServerConfig::clangd();

        assert_eq!(config.language_id, "cpp");
        assert_eq!(config.command, "clangd");
        assert!(config.args.is_empty());
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&"CMakeLists.txt".to_string()));
        assert!(markers.contains(&"compile_commands.json".to_string()));
    }

    #[test]
    fn test_zls_defaults() {
        let config = LspServerConfig::zls();

        assert_eq!(config.language_id, "zig");
        assert_eq!(config.command, "zls");
        assert!(config.args.is_empty());
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&"build.zig".to_string()));
        assert!(markers.contains(&"build.zig.zon".to_string()));
    }

    // Recursive scanning tests
    #[test]
    fn test_recursive_empty_markers_always_applicable() {
        let heuristics = ServerHeuristics::default();
        let tmp = TempDir::new().unwrap();
        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_marker_at_root() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_nested_python_project() {
        let tmp = TempDir::new().unwrap();
        // Create Rust project at root
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        // Create nested Python project
        let python_dir = tmp.path().join("python");
        std::fs::create_dir(&python_dir).unwrap();
        std::fs::write(python_dir.join("pyproject.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["pyproject.toml", "setup.py"]);
        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_deeply_nested_marker() {
        let tmp = TempDir::new().unwrap();
        // Create a deeply nested structure
        let deep_path = tmp.path().join("level1").join("level2").join("level3");
        std::fs::create_dir_all(&deep_path).unwrap();
        std::fs::write(deep_path.join("go.mod"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["go.mod"]);
        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_no_marker_found() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src").join("main.rs"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        assert!(!heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_max_depth_respected() {
        let tmp = TempDir::new().unwrap();
        // Create marker at depth 5
        let deep_path = tmp.path().join("a").join("b").join("c").join("d").join("e");
        std::fs::create_dir_all(&deep_path).unwrap();
        std::fs::write(deep_path.join("Cargo.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        // With max_depth=3, should not find marker at depth 5
        assert!(!heuristics.is_applicable_recursive(tmp.path(), Some(3)));
        // With max_depth=10 (default), should find it
        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_excludes_node_modules() {
        let tmp = TempDir::new().unwrap();
        // Create package.json inside node_modules (should be ignored)
        let node_modules = tmp.path().join("node_modules").join("some-package");
        std::fs::create_dir_all(&node_modules).unwrap();
        std::fs::write(node_modules.join("package.json"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["package.json"]);
        assert!(!heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_excludes_target_directory() {
        let tmp = TempDir::new().unwrap();
        // Create Cargo.toml inside target (should be ignored)
        let target = tmp.path().join("target").join("debug");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("Cargo.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        assert!(!heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_excludes_git_directory() {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join(".git").join("hooks");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("Cargo.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        assert!(!heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_excludes_pycache() {
        let tmp = TempDir::new().unwrap();
        let pycache = tmp.path().join("__pycache__");
        std::fs::create_dir_all(&pycache).unwrap();
        std::fs::write(pycache.join("pyproject.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["pyproject.toml"]);
        assert!(!heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_excludes_venv() {
        let tmp = TempDir::new().unwrap();
        let venv = tmp.path().join(".venv").join("lib");
        std::fs::create_dir_all(&venv).unwrap();
        std::fs::write(venv.join("setup.py"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["setup.py"]);
        assert!(!heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_finds_marker_outside_excluded() {
        let tmp = TempDir::new().unwrap();
        // Create excluded dir with marker
        let node_modules = tmp.path().join("node_modules");
        std::fs::create_dir_all(&node_modules).unwrap();
        std::fs::write(node_modules.join("package.json"), "").unwrap();
        // Create valid marker in src
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("package.json"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["package.json"]);
        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_monorepo_structure() {
        let tmp = TempDir::new().unwrap();
        // Create monorepo with multiple language projects
        let rust_pkg = tmp.path().join("packages").join("rust-lib");
        let python_pkg = tmp.path().join("packages").join("python-bindings");
        let ts_pkg = tmp.path().join("packages").join("typescript-client");

        std::fs::create_dir_all(&rust_pkg).unwrap();
        std::fs::create_dir_all(&python_pkg).unwrap();
        std::fs::create_dir_all(&ts_pkg).unwrap();

        std::fs::write(rust_pkg.join("Cargo.toml"), "").unwrap();
        std::fs::write(python_pkg.join("pyproject.toml"), "").unwrap();
        std::fs::write(ts_pkg.join("package.json"), "").unwrap();

        // All should be detected
        let rust_heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        let python_heuristics = ServerHeuristics::with_markers(["pyproject.toml"]);
        let ts_heuristics = ServerHeuristics::with_markers(["package.json"]);

        assert!(rust_heuristics.is_applicable_recursive(tmp.path(), None));
        assert!(python_heuristics.is_applicable_recursive(tmp.path(), None));
        assert!(ts_heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_should_spawn_recursive() {
        let tmp = TempDir::new().unwrap();
        // Create nested Python project in Rust workspace
        let python_dir = tmp.path().join("bindings").join("python");
        std::fs::create_dir_all(&python_dir).unwrap();
        std::fs::write(python_dir.join("pyproject.toml"), "").unwrap();

        let config = LspServerConfig::pyright();
        assert!(config.should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_should_spawn_with_custom_max_depth() {
        let tmp = TempDir::new().unwrap();
        let deep_path = tmp.path().join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&deep_path).unwrap();
        std::fs::write(deep_path.join("Cargo.toml"), "").unwrap();

        let config = LspServerConfig::rust_analyzer();
        // Shallow depth should not find it
        assert!(!config.should_spawn(tmp.path(), Some(2)));
        // Default depth should find it
        assert!(config.should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_default_heuristics_max_depth() {
        assert_eq!(DEFAULT_HEURISTICS_MAX_DEPTH, 10);
    }

    #[test]
    fn test_excluded_directories_constant() {
        assert!(EXCLUDED_DIRECTORIES.contains(&"node_modules"));
        assert!(EXCLUDED_DIRECTORIES.contains(&"target"));
        assert!(EXCLUDED_DIRECTORIES.contains(&".git"));
        assert!(EXCLUDED_DIRECTORIES.contains(&"__pycache__"));
        assert!(EXCLUDED_DIRECTORIES.contains(&".venv"));
    }
}
