//! Configuration types and loading.
//!
//! This module provides configuration structures for MCPLS,
//! including LSP server definitions and workspace settings.

mod server;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
pub use server::{LspServerConfig, ServerHeuristics};

use crate::error::{Error, Result};

/// Maps file extensions to LSP language identifiers.
///
/// Used to detect the language ID for files based on their extension.
/// Extensions are mapped to language IDs like "rust", "python", "cpp", etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageExtensionMapping {
    /// Array of extensions and their corresponding language ID.
    pub extensions: Vec<String>,
    /// Language ID to report to the LSP server.
    pub language_id: String,
}

/// Main configuration for the MCPLS server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Workspace configuration.
    #[serde(default)]
    pub workspace: WorkspaceConfig,

    /// LSP server configurations.
    #[serde(default)]
    pub lsp_servers: Vec<LspServerConfig>,
}

/// Workspace-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Root directories for the workspace.
    #[serde(default)]
    pub roots: Vec<PathBuf>,

    /// Position encoding preference order.
    /// Valid values: "utf-8", "utf-16", "utf-32"
    #[serde(default = "default_position_encodings")]
    pub position_encodings: Vec<String>,

    /// File extension to language ID mappings.
    /// Allows users to customize which file extensions map to which language servers.
    #[serde(default)]
    pub language_extensions: Vec<LanguageExtensionMapping>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            position_encodings: default_position_encodings(),
            language_extensions: default_language_extensions(),
        }
    }
}

impl WorkspaceConfig {
    /// Build a map of file extensions to language IDs from the configuration.
    ///
    /// # Returns
    ///
    /// A `HashMap` where keys are file extensions (without the dot) and values
    /// are the corresponding language IDs to report to LSP servers.
    #[must_use]
    pub fn build_extension_map(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for mapping in &self.language_extensions {
            for ext in &mapping.extensions {
                map.insert(ext.clone(), mapping.language_id.clone());
            }
        }
        map
    }

    /// Get the language ID for a file extension.
    ///
    /// # Arguments
    ///
    /// * `extension` - The file extension (without the dot)
    ///
    /// # Returns
    ///
    /// The language ID if found, `None` otherwise.
    #[must_use]
    pub fn get_language_for_extension(&self, extension: &str) -> Option<String> {
        for mapping in &self.language_extensions {
            if mapping.extensions.contains(&extension.to_string()) {
                return Some(mapping.language_id.clone());
            }
        }
        None
    }
}

fn default_position_encodings() -> Vec<String> {
    vec!["utf-8".to_string(), "utf-16".to_string()]
}

/// Build default language extension mappings.
///
/// Returns all built-in language extensions that MCPLS recognizes by default.
/// These mappings are used when no custom configuration is provided.
#[allow(clippy::too_many_lines)]
fn default_language_extensions() -> Vec<LanguageExtensionMapping> {
    vec![
        LanguageExtensionMapping {
            extensions: vec!["rs".to_string()],
            language_id: "rust".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["py".to_string(), "pyw".to_string(), "pyi".to_string()],
            language_id: "python".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["js".to_string(), "mjs".to_string(), "cjs".to_string()],
            language_id: "javascript".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["ts".to_string(), "mts".to_string(), "cts".to_string()],
            language_id: "typescript".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["tsx".to_string()],
            language_id: "typescriptreact".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["jsx".to_string()],
            language_id: "javascriptreact".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["go".to_string()],
            language_id: "go".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["c".to_string(), "h".to_string()],
            language_id: "c".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec![
                "cpp".to_string(),
                "cc".to_string(),
                "cxx".to_string(),
                "hpp".to_string(),
                "hh".to_string(),
                "hxx".to_string(),
            ],
            language_id: "cpp".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["java".to_string()],
            language_id: "java".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["rb".to_string()],
            language_id: "ruby".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["php".to_string()],
            language_id: "php".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["swift".to_string()],
            language_id: "swift".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["kt".to_string(), "kts".to_string()],
            language_id: "kotlin".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["scala".to_string(), "sc".to_string()],
            language_id: "scala".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["zig".to_string()],
            language_id: "zig".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["lua".to_string()],
            language_id: "lua".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["sh".to_string(), "bash".to_string(), "zsh".to_string()],
            language_id: "shellscript".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["json".to_string()],
            language_id: "json".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["toml".to_string()],
            language_id: "toml".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["yaml".to_string(), "yml".to_string()],
            language_id: "yaml".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["xml".to_string()],
            language_id: "xml".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["html".to_string(), "htm".to_string()],
            language_id: "html".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["css".to_string()],
            language_id: "css".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["scss".to_string()],
            language_id: "scss".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["less".to_string()],
            language_id: "less".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["md".to_string(), "markdown".to_string()],
            language_id: "markdown".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["cs".to_string()],
            language_id: "csharp".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["fs".to_string(), "fsi".to_string(), "fsx".to_string()],
            language_id: "fsharp".to_string(),
        },
        LanguageExtensionMapping {
            extensions: vec!["r".to_string(), "R".to_string()],
            language_id: "r".to_string(),
        },
    ]
}

impl ServerConfig {
    /// Load configuration from the default path.
    ///
    /// Default paths checked in order:
    /// 1. `$MCPLS_CONFIG` environment variable
    /// 2. `./mcpls.toml` (current directory)
    /// 3. `~/.config/mcpls/mcpls.toml` (Linux/macOS)
    /// 4. `%APPDATA%\mcpls\mcpls.toml` (Windows)
    ///
    /// If no configuration file exists, creates a default configuration file
    /// in the user's config directory with all default language extensions.
    ///
    /// # Errors
    ///
    /// Returns an error if parsing an existing config fails.
    /// If config creation fails, returns default config with graceful degradation.
    pub fn load() -> Result<Self> {
        if let Ok(path) = std::env::var("MCPLS_CONFIG") {
            return Self::load_from(Path::new(&path));
        }

        let local_config = PathBuf::from("mcpls.toml");
        if local_config.exists() {
            return Self::load_from(&local_config);
        }

        if let Some(config_dir) = dirs::config_dir() {
            let user_config = config_dir.join("mcpls").join("mcpls.toml");
            if user_config.exists() {
                return Self::load_from(&user_config);
            }

            // No config found - create default config file
            if let Err(e) = Self::create_default_config_file(&user_config) {
                tracing::warn!(
                    "Failed to create default config at {}: {}. Using in-memory defaults.",
                    user_config.display(),
                    e
                );
            } else {
                tracing::info!("Created default config at {}", user_config.display());
            }
        }

        // Return default configuration
        Ok(Self::default())
    }

    /// Load configuration from a specific path.
    ///
    /// # Errors
    ///
    /// Returns an error if the file doesn't exist or parsing fails.
    pub fn load_from(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::ConfigNotFound(path.to_path_buf())
            } else {
                Error::Io(e)
            }
        })?;

        let config: Self = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Create a default configuration file with all built-in extensions.
    ///
    /// Creates the parent directory if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if directory or file creation fails.
    fn create_default_config_file(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let default_config = Self::default();
        let toml_content = toml::to_string_pretty(&default_config)?;
        std::fs::write(path, toml_content)?;

        Ok(())
    }

    /// Validate the configuration.
    fn validate(&self) -> Result<()> {
        for server in &self.lsp_servers {
            if server.language_id.is_empty() {
                return Err(Error::InvalidConfig(
                    "language_id cannot be empty".to_string(),
                ));
            }
            if server.command.is_empty() {
                return Err(Error::InvalidConfig(format!(
                    "command cannot be empty for language '{}'",
                    server.language_id
                )));
            }
        }
        Ok(())
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            workspace: WorkspaceConfig::default(),
            lsp_servers: vec![
                LspServerConfig::rust_analyzer(),
                LspServerConfig::pyright(),
                LspServerConfig::typescript(),
                LspServerConfig::gopls(),
                LspServerConfig::clangd(),
                LspServerConfig::zls(),
            ],
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_default_config() {
        let config = ServerConfig::default();
        assert_eq!(config.lsp_servers.len(), 6);
        assert_eq!(config.lsp_servers[0].language_id, "rust");
        assert_eq!(config.lsp_servers[1].language_id, "python");
        assert_eq!(config.lsp_servers[2].language_id, "typescript");
        assert_eq!(config.lsp_servers[3].language_id, "go");
        assert_eq!(config.lsp_servers[4].language_id, "cpp");
        assert_eq!(config.lsp_servers[5].language_id, "zig");
        assert_eq!(config.workspace.position_encodings, vec!["utf-8", "utf-16"]);
    }

    #[test]
    fn test_default_position_encodings() {
        let encodings = default_position_encodings();
        assert_eq!(encodings, vec!["utf-8", "utf-16"]);
    }

    #[test]
    fn test_load_from_valid_toml() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = r#"
            [workspace]
            roots = ["/tmp/workspace"]
            position_encodings = ["utf-8"]

            [[lsp_servers]]
            language_id = "rust"
            command = "rust-analyzer"
            timeout_seconds = 30
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let config = ServerConfig::load_from(&config_path).unwrap();
        assert_eq!(
            config.workspace.roots,
            vec![PathBuf::from("/tmp/workspace")]
        );
        assert_eq!(config.workspace.position_encodings, vec!["utf-8"]);
        assert_eq!(config.lsp_servers.len(), 1);
        assert_eq!(config.lsp_servers[0].language_id, "rust");
    }

    #[test]
    fn test_load_from_nonexistent_file() {
        let result = ServerConfig::load_from(Path::new("/nonexistent/config.toml"));
        assert!(result.is_err());

        if let Err(Error::ConfigNotFound(path)) = result {
            assert_eq!(path, PathBuf::from("/nonexistent/config.toml"));
        } else {
            panic!("Expected ConfigNotFound error");
        }
    }

    #[test]
    fn test_load_from_invalid_toml() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("invalid.toml");

        fs::write(&config_path, "invalid toml content {{}").unwrap();

        let result = ServerConfig::load_from(&config_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_empty_language_id() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = r#"
            [[lsp_servers]]
            language_id = ""
            command = "test"
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        assert!(result.is_err());

        if let Err(Error::InvalidConfig(msg)) = result {
            assert!(msg.contains("language_id cannot be empty"));
        } else {
            panic!("Expected InvalidConfig error");
        }
    }

    #[test]
    fn test_validate_empty_command() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = r#"
            [[lsp_servers]]
            language_id = "rust"
            command = ""
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        assert!(result.is_err());

        if let Err(Error::InvalidConfig(msg)) = result {
            assert!(msg.contains("command cannot be empty"));
        } else {
            panic!("Expected InvalidConfig error");
        }
    }

    #[test]
    fn test_workspace_config_defaults() {
        let workspace = WorkspaceConfig::default();
        assert!(workspace.roots.is_empty());
        assert_eq!(workspace.position_encodings, vec!["utf-8", "utf-16"]);
        assert!(!workspace.language_extensions.is_empty());
        assert_eq!(workspace.language_extensions.len(), 30);
    }

    #[test]
    fn test_load_multiple_servers() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("multi.toml");

        let toml_content = r#"
            [[lsp_servers]]
            language_id = "rust"
            command = "rust-analyzer"

            [[lsp_servers]]
            language_id = "python"
            command = "pyright-langserver"
            args = ["--stdio"]
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let config = ServerConfig::load_from(&config_path).unwrap();
        assert_eq!(config.lsp_servers.len(), 2);
        assert_eq!(config.lsp_servers[0].language_id, "rust");
        assert_eq!(config.lsp_servers[1].language_id, "python");
        assert_eq!(config.lsp_servers[1].args, vec!["--stdio"]);
    }

    #[test]
    fn test_deny_unknown_fields() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("unknown.toml");

        let toml_content = r#"
            unknown_field = "value"

            [workspace]
            roots = []
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        assert!(result.is_err(), "Should reject unknown fields");
    }

    #[test]
    fn test_empty_config_file() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("empty.toml");

        fs::write(&config_path, "").unwrap();

        let config = ServerConfig::load_from(&config_path).unwrap();
        assert!(config.workspace.roots.is_empty());
        assert!(config.lsp_servers.is_empty());
    }

    #[test]
    fn test_config_with_initialization_options() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("init_opts.toml");

        let toml_content = r#"
            [[lsp_servers]]
            language_id = "rust"
            command = "rust-analyzer"

            [lsp_servers.initialization_options]
            cargo = { allFeatures = true }
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let config = ServerConfig::load_from(&config_path).unwrap();
        assert!(config.lsp_servers[0].initialization_options.is_some());
    }

    #[test]
    fn test_language_extensions_in_config() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("extensions.toml");

        let toml_content = r#"
            [[workspace.language_extensions]]
            extensions = ["cpp", "cc", "cxx", "hpp", "hh", "hxx"]
            language_id = "cpp"

            [[workspace.language_extensions]]
            extensions = ["nu"]
            language_id = "nushell"

            [[workspace.language_extensions]]
            extensions = ["py", "pyw", "pyi"]
            language_id = "python"
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let config = ServerConfig::load_from(&config_path).unwrap();
        assert_eq!(config.workspace.language_extensions.len(), 3);

        // Check C++ extensions
        assert_eq!(config.workspace.language_extensions[0].language_id, "cpp");
        assert_eq!(
            config.workspace.language_extensions[0].extensions,
            vec!["cpp", "cc", "cxx", "hpp", "hh", "hxx"]
        );

        // Check Nushell extension
        assert_eq!(
            config.workspace.language_extensions[1].language_id,
            "nushell"
        );
        assert_eq!(
            config.workspace.language_extensions[1].extensions,
            vec!["nu"]
        );
    }

    #[test]
    fn test_build_extension_map() {
        let workspace = WorkspaceConfig {
            roots: vec![],
            position_encodings: vec![],
            language_extensions: vec![
                LanguageExtensionMapping {
                    extensions: vec!["cpp".to_string(), "cc".to_string(), "cxx".to_string()],
                    language_id: "cpp".to_string(),
                },
                LanguageExtensionMapping {
                    extensions: vec!["nu".to_string()],
                    language_id: "nushell".to_string(),
                },
            ],
        };

        let map = workspace.build_extension_map();
        assert_eq!(map.get("cpp"), Some(&"cpp".to_string()));
        assert_eq!(map.get("cc"), Some(&"cpp".to_string()));
        assert_eq!(map.get("cxx"), Some(&"cpp".to_string()));
        assert_eq!(map.get("nu"), Some(&"nushell".to_string()));
        assert_eq!(map.get("unknown"), None);
    }

    #[test]
    fn test_get_language_for_extension() {
        let workspace = WorkspaceConfig {
            roots: vec![],
            position_encodings: vec![],
            language_extensions: vec![
                LanguageExtensionMapping {
                    extensions: vec!["hpp".to_string(), "hh".to_string()],
                    language_id: "cpp".to_string(),
                },
                LanguageExtensionMapping {
                    extensions: vec!["py".to_string()],
                    language_id: "python".to_string(),
                },
            ],
        };

        assert_eq!(
            workspace.get_language_for_extension("hpp"),
            Some("cpp".to_string())
        );
        assert_eq!(
            workspace.get_language_for_extension("hh"),
            Some("cpp".to_string())
        );
        assert_eq!(
            workspace.get_language_for_extension("py"),
            Some("python".to_string())
        );
        assert_eq!(workspace.get_language_for_extension("unknown"), None);
    }

    #[test]
    fn test_default_language_extensions() {
        let workspace = WorkspaceConfig::default();
        let map = workspace.build_extension_map();
        assert!(!map.is_empty());
        assert_eq!(
            workspace.get_language_for_extension("rs"),
            Some("rust".to_string())
        );
        assert_eq!(
            workspace.get_language_for_extension("py"),
            Some("python".to_string())
        );
        assert_eq!(
            workspace.get_language_for_extension("cpp"),
            Some("cpp".to_string())
        );
    }

    #[test]
    fn test_create_default_config_file() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("mcpls").join("mcpls.toml");

        ServerConfig::create_default_config_file(&config_path).unwrap();

        assert!(config_path.exists());

        let loaded_config = ServerConfig::load_from(&config_path).unwrap();
        assert_eq!(loaded_config.workspace.language_extensions.len(), 30);
        assert_eq!(loaded_config.lsp_servers.len(), 6);
        assert_eq!(loaded_config.lsp_servers[0].language_id, "rust");
    }

    #[test]
    fn test_load_returns_default_config() {
        // When called directly, default() should return config with all language extensions
        let config = ServerConfig::default();
        assert_eq!(config.workspace.language_extensions.len(), 30);
        assert_eq!(config.lsp_servers.len(), 6);
        assert_eq!(config.lsp_servers[0].language_id, "rust");
    }

    #[test]
    fn test_load_does_not_overwrite_existing_config() {
        // Save original directory to restore it after the test
        let original_dir = std::env::current_dir().unwrap();

        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("mcpls.toml");

        let custom_toml = r#"
            [workspace]
            roots = ["/custom/path"]

            [[lsp_servers]]
            language_id = "python"
            command = "pyright-langserver"
        "#;

        fs::write(&config_path, custom_toml).unwrap();

        std::env::set_current_dir(tmp_dir.path()).unwrap();
        let config = ServerConfig::load().unwrap();

        assert_eq!(config.workspace.roots, vec![PathBuf::from("/custom/path")]);
        assert_eq!(config.lsp_servers.len(), 1);
        assert_eq!(config.lsp_servers[0].language_id, "python");

        // Restore original directory to avoid affecting other tests
        std::env::set_current_dir(original_dir).unwrap();
    }

    #[test]
    fn test_config_file_creation_with_proper_structure() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("test_config").join("mcpls.toml");

        ServerConfig::create_default_config_file(&config_path).unwrap();

        let content = fs::read_to_string(&config_path).unwrap();

        assert!(content.contains("[workspace]"));
        assert!(content.contains("[[workspace.language_extensions]]"));
        assert!(content.contains("[[lsp_servers]]"));
        assert!(content.contains("language_id = \"rust\""));
        assert!(content.contains("extensions = [\"rs\"]"));
    }
}
