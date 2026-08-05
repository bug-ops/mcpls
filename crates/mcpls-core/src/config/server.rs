//! LSP server configuration types.

use std::collections::HashMap;
use std::path::Path;

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

use super::routing::{ServerId, ToolKind};

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
    ///
    /// The server will spawn if ANY of these markers exist anywhere in the workspace tree
    /// (searched recursively up to `heuristics_max_depth`). Well-known directories like
    /// `node_modules`, `target`, `.git` are excluded from the search.
    ///
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
                if entry.file_type().is_some_and(|ft| ft.is_dir())
                    && let Some(name) = entry.file_name().to_str()
                    && EXCLUDED_DIRECTORIES.contains(&name)
                {
                    return false;
                }
                true
            });

        for entry in builder.build().flatten() {
            let path = entry.path();

            // Check if this entry matches any marker
            if let Some(file_name) = path.file_name().and_then(|n| n.to_str())
                && self.project_markers.iter().any(|m| m == file_name)
            {
                return true;
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

    /// Handshake timeout in seconds: bounds the `initialize` request during
    /// server startup. Does not affect individual tool-call requests sent
    /// after initialization; see [`Self::request_timeout_seconds`] for that.
    /// The LSP server's `shutdown` request during teardown uses a separate,
    /// fixed 5-second timeout that is not configurable by this field.
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,

    /// Per-request timeout in seconds, applied to each LSP request issued
    /// while translating an MCP tool call (hover, definition, references, etc.).
    ///
    /// This bounds a single request attempt, not a whole tool call: on a
    /// `-32802` (content modified) response, [`crate::lsp::LspClient::request`]
    /// retries up to 4 attempts with backoff, so the worst-case latency for one
    /// tool call is `4 * request_timeout_seconds + 3.5` seconds. Completion
    /// requests are further capped at 10 seconds regardless of this value; see
    /// [`crate::lsp::LspClient::completion_timeout`].
    #[serde(default = "default_request_timeout")]
    pub request_timeout_seconds: u64,

    /// Heuristics for determining if this server should be spawned.
    /// If not specified, the server will always attempt to spawn.
    #[serde(default)]
    pub heuristics: Option<ServerHeuristics>,

    /// Human-readable server identity used as the routing key.
    ///
    /// Defaults to `language_id` when omitted (see [`Self::id`]). Must be
    /// unique across all applicable servers in a workspace, regardless of
    /// language: this is what lets two servers share one `language_id`
    /// (e.g. pyright and pylsp both for `python`) without one silently
    /// overwriting the other in the maps keyed by [`ServerId`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Tools this server handles.
    ///
    /// `None` means this server is a catch-all: it serves every tool not
    /// explicitly claimed by another server for the same language.
    /// `Some(list)` restricts the server to exactly those tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handles: Option<Vec<ToolKind>>,
}

const fn default_timeout() -> u64 {
    30
}

const fn default_request_timeout() -> u64 {
    30
}

/// Maximum allowed value, in seconds, for both [`LspServerConfig::timeout_seconds`]
/// and [`LspServerConfig::request_timeout_seconds`].
///
/// Both fields are passed straight into `Duration::from_secs` — `timeout_seconds`
/// in the `initialize` handshake (`lsp::lifecycle::LspServer::initialize`),
/// `request_timeout_seconds` in [`crate::lsp::LspClient::request_timeout`].
/// tokio's `timeout`/`sleep` fall back to `Instant::far_future()` for
/// astronomically large durations instead of panicking, so an unbounded value
/// on either field (misconfiguration or typo) would silently disable the
/// timeout rather than fail with a diagnosable error. One shared constant
/// bounds both, since the underlying defect and fix are identical for each.
///
/// Set to 900 (15 minutes), not a rounder 3600 (1 hour): [`LspClient::request`]
/// retries a request up to 4 times total on a `-32802` (`ServerCancelled`)
/// response, so the worst-case latency for a single call bounded by this
/// value is `4 * 900 + 3.5s` ≈ 1 hour, not 4 hours — this constant bounds one
/// attempt, so it is chosen such that the actually-experienced worst case
/// (the retried total) stays within about an hour.
///
/// [`LspClient::request`]: crate::lsp::LspClient::request
pub const MAX_TIMEOUT_SECONDS: u64 = 900;

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

    /// The routing identity of this server: `name` if set, otherwise `language_id`.
    ///
    /// This is the key used across `Translator`'s client/server maps, so two
    /// servers for the same language must set distinct `name`s or they
    /// collide (see `ToolRouter::from_configs` for the enforcement).
    #[must_use]
    pub fn id(&self) -> ServerId {
        self.name
            .clone()
            .map_or_else(|| ServerId::from(self.language_id.clone()), ServerId::from)
    }

    /// Build a built-in server config, filling in every field not passed as
    /// a parameter.
    fn builtin(
        language_id: &str,
        command: &str,
        args: &[&str],
        file_patterns: &[&str],
        markers: impl IntoIterator<Item = &'static str>,
    ) -> Self {
        Self {
            language_id: language_id.to_string(),
            command: command.to_string(),
            args: args.iter().map(ToString::to_string).collect(),
            env: HashMap::new(),
            file_patterns: file_patterns.iter().map(ToString::to_string).collect(),
            initialization_options: None,
            timeout_seconds: default_timeout(),
            request_timeout_seconds: default_request_timeout(),
            heuristics: Some(ServerHeuristics::with_markers(markers)),
            name: None,
            handles: None,
        }
    }

    /// Create a default configuration for rust-analyzer.
    #[must_use]
    pub fn rust_analyzer() -> Self {
        Self::builtin(
            "rust",
            "rust-analyzer",
            &[],
            &["**/*.rs"],
            ["Cargo.toml", "rust-toolchain.toml"],
        )
    }

    /// Create a default configuration for pyright.
    #[must_use]
    pub fn pyright() -> Self {
        Self::builtin(
            "python",
            "pyright-langserver",
            &["--stdio"],
            &["**/*.py"],
            [
                "pyproject.toml",
                "setup.py",
                "requirements.txt",
                "pyrightconfig.json",
            ],
        )
    }

    /// Create a default configuration for TypeScript language server.
    #[must_use]
    pub fn typescript() -> Self {
        Self::builtin(
            "typescript",
            "typescript-language-server",
            &["--stdio"],
            &["**/*.ts", "**/*.tsx"],
            ["package.json", "tsconfig.json", "jsconfig.json"],
        )
    }

    /// Create a default configuration for gopls.
    #[must_use]
    pub fn gopls() -> Self {
        Self::builtin(
            "go",
            "gopls",
            &["serve"],
            &["**/*.go"],
            ["go.mod", "go.sum"],
        )
    }

    /// Create a default configuration for clangd.
    #[must_use]
    pub fn clangd() -> Self {
        Self::builtin(
            "cpp",
            "clangd",
            &[],
            &["**/*.c", "**/*.cpp", "**/*.h", "**/*.hpp"],
            [
                "CMakeLists.txt",
                "compile_commands.json",
                "Makefile",
                ".clangd",
            ],
        )
    }

    /// Create a default configuration for zls.
    #[must_use]
    pub fn zls() -> Self {
        Self::builtin(
            "zig",
            "zls",
            &[],
            &["**/*.zig"],
            ["build.zig", "build.zig.zon"],
        )
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
            request_timeout_seconds: 45,
            heuristics: None,
            name: None,
            handles: None,
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
        assert_eq!(
            deserialized.request_timeout_seconds,
            original.request_timeout_seconds
        );
    }

    #[test]
    fn test_default_request_timeout() {
        assert_eq!(default_request_timeout(), 30);
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
            request_timeout_seconds: 30,
            heuristics: None,
            name: None,
            handles: None,
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
