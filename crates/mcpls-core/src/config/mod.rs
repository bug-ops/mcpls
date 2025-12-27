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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
