//! LSP server configuration types.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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
}

const fn default_timeout() -> u64 {
    30
}

impl LspServerConfig {
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
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
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
}
