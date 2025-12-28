//! Configuration types and loading.
//!
//! This module provides configuration structures for MCPLS,
//! including LSP server definitions and workspace settings.

mod server;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
pub use server::LspServerConfig;

use crate::error::{Error, Result};

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
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            position_encodings: default_position_encodings(),
        }
    }
}

fn default_position_encodings() -> Vec<String> {
    vec!["utf-8".to_string(), "utf-16".to_string()]
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
    /// # Errors
    ///
    /// Returns an error if no configuration file is found or if parsing fails.
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
        }

        // Return default configuration if no config file found
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
            lsp_servers: vec![LspServerConfig::rust_analyzer()],
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
        assert_eq!(config.lsp_servers.len(), 1);
        assert_eq!(config.lsp_servers[0].language_id, "rust");
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
    fn test_workspace_roots_empty_by_default() {
        let workspace = WorkspaceConfig::default();
        assert!(workspace.roots.is_empty());
    }

    #[test]
    fn test_workspace_config_defaults() {
        let workspace = WorkspaceConfig::default();
        assert!(workspace.roots.is_empty());
        assert_eq!(workspace.position_encodings, vec!["utf-8", "utf-16"]);
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
}
