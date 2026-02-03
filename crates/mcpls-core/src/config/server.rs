//! LSP server configuration types.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

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
    #[must_use]
    pub fn should_spawn(&self, workspace_root: &Path) -> bool {
        self.heuristics
            .as_ref()
            .is_none_or(|h| h.is_applicable(workspace_root))
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
        assert!(config.should_spawn(tmp.path()));
    }

    #[test]
    fn test_should_spawn_with_heuristics() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();

        let config = LspServerConfig::rust_analyzer();
        assert!(config.should_spawn(tmp.path()));
    }

    #[test]
    fn test_should_not_spawn_without_markers() {
        let tmp = TempDir::new().unwrap();
        let config = LspServerConfig::rust_analyzer();
        assert!(!config.should_spawn(tmp.path()));
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
}
