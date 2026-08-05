//! Configuration types and loading.
//!
//! This module provides configuration structures for MCPLS,
//! including LSP server definitions and workspace settings.

mod language;
mod routing;
mod server;

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

pub use language::{base_language_id, react_variant_language_id};
pub use routing::{NoServerReason, ServerId, ToolKind, ToolRouter};
use serde::{Deserialize, Serialize};
pub use server::{
    DEFAULT_HEURISTICS_MAX_DEPTH, LspServerConfig, MAX_TIMEOUT_SECONDS, ServerHeuristics,
};

use crate::bridge::{DEFAULT_MAX_DOCUMENTS, DEFAULT_MAX_FILE_SIZE, ResourceLimits};
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

    /// Whether a CWD-discovered `./mcpls.toml` was ignored as untrusted
    /// during this load (see [`ProjectConfigTrust`]).
    ///
    /// Load-time metadata, not user-configurable: never read from or written
    /// to a TOML file. Consumed by `McplsServer::get_info` (the
    /// `ServerHandler` implementation in `crate::mcp::server`) to surface
    /// the ignore decision in-band to MCP clients, supplementing the
    /// `tracing::warn!` emitted at load time (which is stderr-only and
    /// typically invisible to an MCP client).
    #[serde(skip)]
    pub project_config_ignored: bool,
}

/// Workspace-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Root directories for the workspace.
    #[serde(default)]
    pub roots: Vec<PathBuf>,

    /// Position encoding preference order, offered to each spawned LSP
    /// server as `capabilities.general.positionEncodings` during the
    /// `initialize` handshake (see [`crate::lsp::LspServer::spawn`]), in the
    /// order configured here.
    ///
    /// Valid values: `"utf-8"`, `"utf-16"`, `"utf-32"`. Must be non-empty;
    /// [`ServerConfig::validate`] rejects an empty list or an unrecognized
    /// value.
    #[serde(default = "default_position_encodings")]
    pub position_encodings: Vec<String>,

    /// File extension to language ID mappings.
    /// Allows users to customize which file extensions map to which language servers.
    #[serde(default)]
    pub language_extensions: Vec<LanguageExtensionMapping>,

    /// Maximum depth for recursive project marker search.
    /// Controls how deeply nested projects can be detected.
    /// Default: 10
    #[serde(default = "default_heuristics_max_depth")]
    pub heuristics_max_depth: usize,

    /// Maximum number of documents `DocumentTracker` will keep open
    /// simultaneously. A `textDocument/didOpen`-triggering tool call (hover,
    /// definition, diagnostics, etc.) for a document beyond this count fails
    /// with `DocumentLimitExceeded`. Documents stay tracked for the whole
    /// mcpls process lifetime (there is no eviction), so once the ceiling is
    /// reached, opening any further new path fails until either the process
    /// is restarted or this limit is raised; already-tracked paths are
    /// unaffected. `0` disables the limit.
    /// Default: 100
    #[serde(default = "default_max_documents")]
    pub max_documents: usize,

    /// Maximum size, in bytes, of a single file `DocumentTracker` will open.
    /// A file larger than this fails with `FileSizeLimitExceeded`. `0`
    /// disables the limit.
    /// Default: 10485760 (10MB)
    #[serde(default = "default_max_file_size")]
    pub max_file_size: u64,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            position_encodings: default_position_encodings(),
            language_extensions: default_language_extensions(),
            heuristics_max_depth: default_heuristics_max_depth(),
            max_documents: default_max_documents(),
            max_file_size: default_max_file_size(),
        }
    }
}

const fn default_heuristics_max_depth() -> usize {
    DEFAULT_HEURISTICS_MAX_DEPTH
}

const fn default_max_documents() -> usize {
    DEFAULT_MAX_DOCUMENTS
}

const fn default_max_file_size() -> u64 {
    DEFAULT_MAX_FILE_SIZE
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

    /// Maps the configured `max_documents`/`max_file_size` onto the bridge
    /// layer's [`ResourceLimits`], for [`Translator::with_resource_limits`](crate::bridge::Translator::with_resource_limits).
    #[must_use]
    pub const fn resource_limits(&self) -> ResourceLimits {
        ResourceLimits {
            max_documents: self.max_documents,
            max_file_size: self.max_file_size,
        }
    }
}

/// Extract a file extension from a glob-like file pattern.
///
/// Supports common patterns such as `**/*.rs` and `*.h`.
/// Returns `None` for patterns without a simple trailing extension.
fn extract_extension_from_pattern(pattern: &str) -> Option<String> {
    let basename = pattern.rsplit('/').next().unwrap_or(pattern);
    if basename.starts_with('.') {
        return None;
    }

    let (_, ext) = basename.rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }

    // Keep this conservative: only accept plain extension-like tokens.
    if ext
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Some(ext.to_string())
    } else {
        None
    }
}

fn language_id_for_pattern_extension(server_language_id: &str, extension: &str) -> String {
    react_variant_language_id(server_language_id, extension)
        .unwrap_or(server_language_id)
        .to_string()
}

/// The client-preference order offered to every spawned server during
/// `initialize`.
///
/// `utf-8` is listed first deliberately, not just historically: probing both
/// rust-analyzer and clangd (this project's two flagship servers) against
/// exactly this offer shows both negotiate down to `utf-8`, so it is the
/// common case, not a rare fallback. Earlier revisions of this file
/// (`#290`/`#291`) treated the non-UTF-16 conversion path in
/// `bridge/encoding.rs` as an edge case on that (false) assumption, which
/// hid a char-boundary panic and an uncached-disk-read cost on what turned
/// out to be the default path for both servers. Both are now fixed
/// (`bridge/encoding.rs`'s boundary guards; `bridge/translator.rs`'s
/// `EncodingCtx` preferring `DocumentTracker`'s in-memory content over
/// disk), so there is no longer a correctness or performance reason to
/// prefer `utf-16` here -- reordering would only reintroduce UTF-16 by
/// default bias, undoing the point of negotiating an encoding at all.
pub(crate) fn default_position_encodings() -> Vec<String> {
    vec!["utf-8".to_string(), "utf-16".to_string()]
}

/// Parse a configured position-encoding string into an [`lsp_types::PositionEncodingKind`].
///
/// Recognizes the three values the LSP spec defines for
/// `PositionEncodingKind`: `"utf-8"`, `"utf-16"`, `"utf-32"`. Returns `None`
/// for anything else, letting the caller decide how to handle an invalid
/// value (see [`ServerConfig::validate`], which rejects it at load time, and
/// [`crate::lsp::LspServer::spawn`], which falls back to a default rather
/// than failing the handshake for a config built without going through
/// `validate`).
pub(crate) fn parse_position_encoding(value: &str) -> Option<lsp_types::PositionEncodingKind> {
    match value {
        "utf-8" => Some(lsp_types::PositionEncodingKind::UTF8),
        "utf-16" => Some(lsp_types::PositionEncodingKind::UTF16),
        "utf-32" => Some(lsp_types::PositionEncodingKind::UTF32),
        _ => None,
    }
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

/// Trust level applied to a `./mcpls.toml` discovered relative to the
/// process's current working directory.
///
/// A CWD-discovered project-local config is not the same trust tier as an
/// explicit `--config`/`MCPLS_CONFIG` path: it can be planted by whoever
/// controls the checked-out repository, and it controls the `command` and
/// `args` mcpls spawns as well as `[workspace]` (which can redirect the
/// spawn target via `roots` or drive a filesystem-walk `DoS` via
/// `heuristics_max_depth`). [`ServerConfig::load`] treats it as
/// [`Untrusted`](Self::Untrusted) by default; callers that want it honored
/// must opt in via [`ServerConfig::load_with_trust`].
///
/// An explicitly passed `--config`/`MCPLS_CONFIG` path is unaffected by this
/// enum and is always trusted: naming a path is itself the user's consent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectConfigTrust {
    /// Ignore a CWD-discovered `./mcpls.toml` entirely; fall through to the
    /// global config tier or built-in defaults.
    Untrusted,
    /// Load a CWD-discovered `./mcpls.toml` normally.
    Trusted,
}

/// Maximum size, in bytes, of a config file `load_from` will read.
///
/// A config file is trusted TOML on a normal setup, but nothing stops a
/// path from pointing at an arbitrarily large or adversarial file (e.g. a
/// misconfigured `$MCPLS_CONFIG`) -- `load_from` used to call
/// `std::fs::read_to_string` with no upper bound, so it could be made to
/// buffer an unbounded amount of memory before `toml::from_str` ever runs
/// (#309). 8 MiB is far larger than any legitimate `mcpls.toml`, which
/// realistically stays in the low kilobytes even with dozens of configured
/// servers.
///
/// Enforced via a bounded read (`Read::take`), not a `std::fs::metadata`
/// pre-check: `metadata().len()` reports `0` for character devices, FIFOs,
/// and many procfs entries regardless of how much data they can actually
/// produce (e.g. `/dev/zero`), so a path pointing at one of those would
/// sail past a size-only pre-check and still block `read_to_string` on an
/// effectively infinite read -- the exact "slow/infinite device" case #309
/// named. A pure metadata check is also TOCTOU-able for a regular file that
/// grows between the check and the read. Reading `MAX_CONFIG_FILE_BYTES +
/// 1` bytes, one past the cap, is what distinguishes "exactly at the
/// boundary" (allowed) from "over" (rejected) without needing a second
/// syscall.
const MAX_CONFIG_FILE_BYTES: u64 = 8 * 1024 * 1024;

impl ServerConfig {
    /// Build the effective extension map used for language detection.
    ///
    /// Starts with workspace mappings and overlays mappings inferred from
    /// configured LSP server `file_patterns`.
    #[must_use]
    pub fn build_effective_extension_map(&self) -> HashMap<String, String> {
        let mut map = self.workspace.build_extension_map();

        for server in &self.lsp_servers {
            for pattern in &server.file_patterns {
                if let Some(ext) = extract_extension_from_pattern(pattern) {
                    let language_id = language_id_for_pattern_extension(&server.language_id, &ext);
                    map.insert(ext, language_id);
                }
            }
        }

        map
    }

    /// Load configuration from the default path, treating a CWD-discovered
    /// `./mcpls.toml` as untrusted.
    ///
    /// Default paths checked in order:
    /// 1. `$MCPLS_CONFIG` environment variable (always trusted)
    /// 2. `./mcpls.toml` (current directory) — **skipped**; see
    ///    [`load_with_trust`](Self::load_with_trust) to opt in
    /// 3. Platform user-config directory:
    ///    - Linux: `$XDG_CONFIG_HOME/mcpls/mcpls.toml`, else `~/.config/mcpls/mcpls.toml`
    ///    - macOS: `~/Library/Application Support/mcpls/mcpls.toml`
    /// 4. `%APPDATA%\mcpls\mcpls.toml` (Windows)
    ///
    /// If no configuration file exists, creates a default configuration file
    /// in the user's config directory with all default language extensions.
    ///
    /// This is a thin wrapper around
    /// [`load_with_trust(ProjectConfigTrust::Untrusted)`](Self::load_with_trust) —
    /// the safe default for library callers that haven't made a trust
    /// decision.
    ///
    /// # Errors
    ///
    /// Returns an error if parsing an existing config fails.
    /// If config creation fails, returns default config with graceful degradation.
    pub fn load() -> Result<Self> {
        Self::load_with_trust(ProjectConfigTrust::Untrusted)
    }

    /// Load configuration from the default path, with explicit control over
    /// whether a CWD-discovered `./mcpls.toml` is honored.
    ///
    /// Behaves like [`load`](Self::load), except a `./mcpls.toml` found in
    /// the current directory is only loaded when `trust` is
    /// [`ProjectConfigTrust::Trusted`]. When untrusted, the file is skipped
    /// entirely (including its `[workspace]` section) and a warning is
    /// logged naming the ignored path; discovery falls through to the
    /// global config tier or built-in defaults, so project-marker
    /// heuristics (e.g. `Cargo.toml` → rust-analyzer) still apply normally.
    /// The returned config's [`project_config_ignored`](Self::project_config_ignored)
    /// is set to `true` in that case, so callers with access to the loaded
    /// config (e.g. `McplsServer::get_info`) can surface the ignore decision
    /// in-band, not just via the stderr-only warning.
    ///
    /// `$MCPLS_CONFIG` and an explicit path are unaffected by `trust` and
    /// are always loaded: naming a path is itself the user's consent.
    ///
    /// # Errors
    ///
    /// Returns an error if parsing an existing config fails.
    /// If config creation fails, returns default config with graceful degradation.
    pub fn load_with_trust(trust: ProjectConfigTrust) -> Result<Self> {
        // This `$MCPLS_CONFIG` check is unreachable from the `mcpls` binary:
        // `crates/mcpls-cli/src/args.rs` already binds `env = "MCPLS_CONFIG"`
        // to `--config`, so the CLI resolves that variable before `load`/
        // `load_with_trust` is ever called. It only fires for library
        // callers that invoke this function directly without going through
        // `Args`. The actual, CLI-enforced guarantee that `$MCPLS_CONFIG` is
        // always trusted lives in `main.rs`'s `--config` branch, not here.
        if let Ok(path) = std::env::var("MCPLS_CONFIG") {
            return Self::load_from(Path::new(&path));
        }

        let mut project_config_ignored = false;

        let local_config = PathBuf::from("mcpls.toml");
        if local_config.exists() {
            match trust {
                ProjectConfigTrust::Trusted => return Self::load_from(&local_config),
                ProjectConfigTrust::Untrusted => {
                    project_config_ignored = true;
                    let display_path = local_config.canonicalize().unwrap_or_else(|_| {
                        std::env::current_dir()
                            .map_or_else(|_| local_config.clone(), |cwd| cwd.join(&local_config))
                    });
                    tracing::warn!(
                        "ignoring untrusted project-local config at {}; pass \
                         --trust-project-config (or set MCPLS_TRUST_PROJECT_CONFIG=true) to \
                         load it",
                        display_path.display()
                    );
                }
            }
        }

        if let Some(config_dir) = dirs::config_dir() {
            let user_config = config_dir.join("mcpls").join("mcpls.toml");
            if user_config.exists() {
                let mut config = Self::load_from(&user_config)?;
                config.project_config_ignored = project_config_ignored;
                return Ok(config);
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
        Ok(Self {
            project_config_ignored,
            ..Self::default()
        })
    }

    /// Load configuration from a specific path.
    ///
    /// # Errors
    ///
    /// Returns an error if the file doesn't exist, exceeds the maximum
    /// allowed config file size, or parsing fails.
    pub fn load_from(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::ConfigNotFound(path.to_path_buf())
            } else {
                Error::Io(e)
            }
        })?;

        // Bounded read, not a `metadata().len()` pre-check -- see
        // `MAX_CONFIG_FILE_BYTES`'s doc for why the pre-check alone is
        // bypassable.
        let mut buf = Vec::new();
        file.take(MAX_CONFIG_FILE_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(Error::Io)?;
        if buf.len() as u64 > MAX_CONFIG_FILE_BYTES {
            return Err(Error::FileSizeLimitExceeded {
                size: buf.len() as u64,
                max: MAX_CONFIG_FILE_BYTES,
            });
        }
        let content = String::from_utf8(buf)
            .map_err(|e| Error::InvalidConfig(format!("config file is not valid UTF-8: {e}")))?;

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
    ///
    /// This covers only workspace-*independent* rules — checks that hold
    /// regardless of which servers end up applicable in a given workspace.
    /// Workspace-scoped routing rules (duplicate `ServerId`, conflicting
    /// `handles` claims across applicable servers) are enforced later, by
    /// `ToolRouter::from_configs` over the post-heuristics config subset in
    /// `serve_with` — see that function's module docs for why the split
    /// exists (two servers for one language with mutually exclusive
    /// `heuristics` is a legitimate config that must still load here).
    ///
    /// [`Self::load_from`] always calls this, and so do [`crate::serve`] and
    /// [`crate::serve_with`] for every `ServerConfig` regardless of origin —
    /// a caller-constructed config (not loaded via TOML) gets the same
    /// diagnosable [`Error::InvalidConfig`] rejection as one loaded from
    /// disk, instead of only failing later via silent accessor-level
    /// clamping (see [`crate::lsp::LspClient::request_timeout`]). Remains
    /// `pub` so a caller can also validate a config up front, before handing
    /// it to `serve`/`serve_with` (which consume it by value and run until
    /// shutdown).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] on the first rule violated.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::config::ServerConfig;
    ///
    /// let config = ServerConfig::default();
    /// assert!(config.validate().is_ok());
    /// ```
    pub fn validate(&self) -> Result<()> {
        if self.workspace.position_encodings.is_empty() {
            return Err(Error::InvalidConfig(
                "workspace.position_encodings cannot be empty".to_string(),
            ));
        }
        for encoding in &self.workspace.position_encodings {
            if parse_position_encoding(encoding).is_none() {
                return Err(Error::InvalidConfig(format!(
                    "invalid workspace.position_encodings value '{encoding}'; expected one of \
                     \"utf-8\", \"utf-16\", \"utf-32\""
                )));
            }
        }

        let mut seen_names: HashMap<&str, &str> = HashMap::new();
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
            if server.timeout_seconds == 0 {
                return Err(Error::InvalidConfig(format!(
                    "timeout_seconds cannot be 0 for language '{}'",
                    server.language_id
                )));
            }
            if server.timeout_seconds > MAX_TIMEOUT_SECONDS {
                return Err(Error::InvalidConfig(format!(
                    "timeout_seconds ({}) exceeds the maximum of {} seconds for language '{}'",
                    server.timeout_seconds, MAX_TIMEOUT_SECONDS, server.language_id
                )));
            }
            if server.request_timeout_seconds == 0 {
                return Err(Error::InvalidConfig(format!(
                    "request_timeout_seconds cannot be 0 for language '{}'",
                    server.language_id
                )));
            }
            if server.request_timeout_seconds > MAX_TIMEOUT_SECONDS {
                return Err(Error::InvalidConfig(format!(
                    "request_timeout_seconds ({}) exceeds the maximum of {} seconds for \
                     language '{}'",
                    server.request_timeout_seconds, MAX_TIMEOUT_SECONDS, server.language_id
                )));
            }
            if let Some(name) = &server.name {
                if name.is_empty() {
                    return Err(Error::InvalidConfig(format!(
                        "name cannot be empty for language '{}' (omit `name` to default to \
                         the language id)",
                        server.language_id
                    )));
                }
                if let Some(prev_language) = seen_names.insert(name.as_str(), &server.language_id) {
                    // Not a hard error here: whether this is actually ambiguous
                    // depends on which of these servers end up applicable in a
                    // given workspace, which this function cannot know. The
                    // workspace-scoped check in `ToolRouter::from_configs` is
                    // authoritative.
                    tracing::warn!(
                        "duplicate explicit server name '{name}' in config (language ids: \
                         '{prev_language}', '{}'); this is only an error if both entries are \
                         applicable in the same workspace",
                        server.language_id
                    );
                }
            }
            if let Some(handles) = &server.handles {
                if handles.is_empty() {
                    return Err(Error::InvalidConfig(format!(
                        "handles cannot be empty for language '{}' (omit `handles` for a \
                         catch-all server)",
                        server.language_id
                    )));
                }
                let mut seen_tools = HashSet::new();
                for tool in handles {
                    if !seen_tools.insert(*tool) {
                        return Err(Error::InvalidConfig(format!(
                            "duplicate tool '{tool}' in `handles` for language '{}'",
                            server.language_id
                        )));
                    }
                }
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
            project_config_ignored: false,
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
    fn test_load_from_toml_without_request_timeout_seconds_defaults_to_thirty() {
        // Mirrors the shape of every auto-generated pre-#267 config file:
        // `timeout_seconds` present, `request_timeout_seconds` absent.
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = r#"
            [[lsp_servers]]
            language_id = "rust"
            command = "rust-analyzer"
            timeout_seconds = 30
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let config = ServerConfig::load_from(&config_path).unwrap();
        assert_eq!(config.lsp_servers[0].request_timeout_seconds, 30);
    }

    #[test]
    fn test_validate_rejects_zero_timeout_seconds() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = r#"
            [[lsp_servers]]
            language_id = "rust"
            command = "rust-analyzer"
            timeout_seconds = 0
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        if let Err(Error::InvalidConfig(msg)) = result {
            // `contains("timeout_seconds cannot be 0")` would also match the
            // `request_timeout_seconds` message below (it ends in the same
            // suffix), so assert the exact message to actually discriminate
            // which field triggered the error.
            assert_eq!(msg, "timeout_seconds cannot be 0 for language 'rust'");
        } else {
            panic!("Expected InvalidConfig error, got {result:?}");
        }
    }

    #[test]
    fn test_validate_rejects_zero_request_timeout_seconds() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = r#"
            [[lsp_servers]]
            language_id = "rust"
            command = "rust-analyzer"
            request_timeout_seconds = 0
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        if let Err(Error::InvalidConfig(msg)) = result {
            assert_eq!(
                msg,
                "request_timeout_seconds cannot be 0 for language 'rust'"
            );
        } else {
            panic!("Expected InvalidConfig error, got {result:?}");
        }
    }

    #[test]
    fn test_validate_rejects_request_timeout_seconds_above_max() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = format!(
            r#"
            [[lsp_servers]]
            language_id = "rust"
            command = "rust-analyzer"
            request_timeout_seconds = {}
        "#,
            MAX_TIMEOUT_SECONDS + 1
        );

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        if let Err(Error::InvalidConfig(msg)) = result {
            assert!(msg.contains("request_timeout_seconds"));
            assert!(msg.contains("exceeds the maximum"));
        } else {
            panic!("Expected InvalidConfig error, got {result:?}");
        }
    }

    #[test]
    fn test_validate_accepts_request_timeout_seconds_at_max() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = format!(
            r#"
            [[lsp_servers]]
            language_id = "rust"
            command = "rust-analyzer"
            request_timeout_seconds = {MAX_TIMEOUT_SECONDS}
        "#
        );

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[test]
    fn test_validate_rejects_timeout_seconds_above_max() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = format!(
            r#"
            [[lsp_servers]]
            language_id = "rust"
            command = "rust-analyzer"
            timeout_seconds = {}
        "#,
            MAX_TIMEOUT_SECONDS + 1
        );

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        if let Err(Error::InvalidConfig(msg)) = result {
            assert!(msg.contains("timeout_seconds"));
            assert!(msg.contains("exceeds the maximum"));
        } else {
            panic!("Expected InvalidConfig error, got {result:?}");
        }
    }

    #[test]
    fn test_validate_accepts_timeout_seconds_at_max() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = format!(
            r#"
            [[lsp_servers]]
            language_id = "rust"
            command = "rust-analyzer"
            timeout_seconds = {MAX_TIMEOUT_SECONDS}
        "#
        );

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        assert!(result.is_ok(), "expected Ok, got {result:?}");
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

    /// #309: a config file larger than `MAX_CONFIG_FILE_BYTES` must be
    /// rejected before `read_to_string` buffers it, not merely fail to
    /// parse as TOML afterward.
    #[test]
    fn test_load_from_rejects_oversized_file() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("oversized.toml");

        // One byte over the cap; content doesn't need to be valid TOML since
        // the size check runs before parsing.
        let oversized = "#".repeat(usize::try_from(MAX_CONFIG_FILE_BYTES).unwrap() + 1);
        fs::write(&config_path, &oversized).unwrap();

        let result = ServerConfig::load_from(&config_path);
        assert!(matches!(
            result,
            Err(Error::FileSizeLimitExceeded { max, .. }) if max == MAX_CONFIG_FILE_BYTES
        ));
    }

    #[test]
    fn test_load_from_accepts_file_at_exact_size_cap() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("exact.toml");

        // Pad a valid, minimal TOML document with a trailing comment up to
        // exactly the cap -- the boundary itself must not be rejected.
        let mut toml_content = "[workspace]\n# ".to_string();
        toml_content.push_str(
            &"a".repeat(usize::try_from(MAX_CONFIG_FILE_BYTES).unwrap() - toml_content.len()),
        );
        assert_eq!(toml_content.len() as u64, MAX_CONFIG_FILE_BYTES);
        fs::write(&config_path, &toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    /// #309 S1: `std::fs::metadata` reports `len() == 0` for character
    /// devices regardless of how much data they can actually produce --
    /// `/dev/zero` is the canonical example. A size check based on metadata
    /// alone would pass and let `load_from` block on an effectively
    /// infinite read; the bounded `Read::take` must still reject it via
    /// `MAX_CONFIG_FILE_BYTES`, not hang or OOM.
    #[cfg(unix)]
    #[test]
    fn test_load_from_rejects_infinite_special_file() {
        let path = Path::new("/dev/zero");
        assert_eq!(
            fs::metadata(path).unwrap().len(),
            0,
            "test assumption: /dev/zero must report zero length"
        );

        let result = ServerConfig::load_from(path);
        assert!(matches!(
            result,
            Err(Error::FileSizeLimitExceeded { max, .. }) if max == MAX_CONFIG_FILE_BYTES
        ));
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
    fn test_validate_empty_name() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = r#"
            [[lsp_servers]]
            name = ""
            language_id = "python"
            command = "pyright-langserver"
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        assert!(result.is_err());

        if let Err(Error::InvalidConfig(msg)) = result {
            assert!(msg.contains("name cannot be empty"));
        } else {
            panic!("Expected InvalidConfig error");
        }
    }

    #[test]
    fn test_validate_empty_handles() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = r#"
            [[lsp_servers]]
            language_id = "python"
            command = "pylsp"
            handles = []
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        assert!(result.is_err());

        if let Err(Error::InvalidConfig(msg)) = result {
            assert!(msg.contains("handles cannot be empty"));
        } else {
            panic!("Expected InvalidConfig error");
        }
    }

    #[test]
    fn test_validate_duplicate_tool_in_handles() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = r#"
            [[lsp_servers]]
            language_id = "python"
            command = "pylsp"
            handles = ["diagnostics", "diagnostics"]
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        assert!(result.is_err());

        if let Err(Error::InvalidConfig(msg)) = result {
            assert!(msg.contains("duplicate tool"));
            assert!(msg.contains("diagnostics"));
        } else {
            panic!("Expected InvalidConfig error");
        }
    }

    #[test]
    fn test_validate_rejects_empty_position_encodings() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = r"
            [workspace]
            position_encodings = []
        ";

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        if let Err(Error::InvalidConfig(msg)) = result {
            assert_eq!(msg, "workspace.position_encodings cannot be empty");
        } else {
            panic!("Expected InvalidConfig error, got {result:?}");
        }
    }

    #[test]
    fn test_validate_rejects_unrecognized_position_encoding() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = r#"
            [workspace]
            position_encodings = ["utf-8", "utf-7"]
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        if let Err(Error::InvalidConfig(msg)) = result {
            assert!(msg.contains("invalid workspace.position_encodings value 'utf-7'"));
        } else {
            panic!("Expected InvalidConfig error, got {result:?}");
        }
    }

    #[test]
    fn test_parse_position_encoding_maps_valid_values_and_rejects_unknown() {
        assert_eq!(
            parse_position_encoding("utf-8"),
            Some(lsp_types::PositionEncodingKind::UTF8)
        );
        assert_eq!(
            parse_position_encoding("utf-16"),
            Some(lsp_types::PositionEncodingKind::UTF16)
        );
        assert_eq!(
            parse_position_encoding("utf-32"),
            Some(lsp_types::PositionEncodingKind::UTF32)
        );
        assert_eq!(parse_position_encoding("utf-7"), None);
    }

    #[test]
    fn test_validate_duplicate_name_warns_but_loads() {
        // Duplicate explicit `name` is only an error if both entries end up
        // applicable in the same workspace (enforced later by
        // `ToolRouter::from_configs`, see routing.rs); at load time it must
        // still succeed.
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("config.toml");

        let toml_content = r#"
            [[lsp_servers]]
            name = "dup"
            language_id = "python"
            command = "pyright-langserver"

            [[lsp_servers]]
            name = "dup"
            language_id = "typescript"
            command = "typescript-language-server"
        "#;

        fs::write(&config_path, toml_content).unwrap();

        let result = ServerConfig::load_from(&config_path);
        assert!(result.is_ok(), "duplicate name must only warn at load time");
    }

    #[test]
    fn test_workspace_config_defaults() {
        let workspace = WorkspaceConfig::default();
        assert!(workspace.roots.is_empty());
        assert_eq!(workspace.position_encodings, vec!["utf-8", "utf-16"]);
        assert!(!workspace.language_extensions.is_empty());
        assert_eq!(workspace.language_extensions.len(), 30);
        assert_eq!(workspace.heuristics_max_depth, DEFAULT_HEURISTICS_MAX_DEPTH);
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
            heuristics_max_depth: DEFAULT_HEURISTICS_MAX_DEPTH,
            max_documents: DEFAULT_MAX_DOCUMENTS,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        };

        let map = workspace.build_extension_map();
        assert_eq!(map.get("cpp"), Some(&"cpp".to_string()));
        assert_eq!(map.get("cc"), Some(&"cpp".to_string()));
        assert_eq!(map.get("cxx"), Some(&"cpp".to_string()));
        assert_eq!(map.get("nu"), Some(&"nushell".to_string()));
        assert_eq!(map.get("unknown"), None);
    }

    #[test]
    fn test_extract_extension_from_pattern_empty_string() {
        assert_eq!(extract_extension_from_pattern(""), None);
    }

    #[test]
    fn test_extract_extension_from_pattern_without_dot() {
        assert_eq!(extract_extension_from_pattern("**/*"), None);
    }

    #[test]
    fn test_extract_extension_from_pattern_dotfile() {
        assert_eq!(extract_extension_from_pattern(".gitignore"), None);
    }

    #[test]
    fn test_extract_extension_from_pattern_multi_dot_extension() {
        assert_eq!(
            extract_extension_from_pattern("foo.tar.gz"),
            Some("gz".to_string())
        );
    }

    #[test]
    fn test_build_effective_extension_map_overrides_with_file_patterns() {
        let config = ServerConfig {
            workspace: WorkspaceConfig::default(),
            lsp_servers: vec![LspServerConfig {
                language_id: "cpp".to_string(),
                command: "clangd".to_string(),
                args: vec![],
                env: HashMap::new(),
                file_patterns: vec!["**/*.c".to_string(), "**/*.h".to_string()],
                initialization_options: None,
                timeout_seconds: 30,
                request_timeout_seconds: 30,
                heuristics: None,
                name: None,
                handles: None,
            }],
            project_config_ignored: false,
        };

        let map = config.build_effective_extension_map();
        assert_eq!(map.get("c"), Some(&"cpp".to_string()));
        assert_eq!(map.get("h"), Some(&"cpp".to_string()));
    }

    #[test]
    fn test_build_effective_extension_map_derives_tsx_language_id() {
        let config = ServerConfig {
            workspace: WorkspaceConfig::default(),
            lsp_servers: vec![LspServerConfig {
                language_id: "typescript".to_string(),
                command: "tsgo".to_string(),
                args: vec!["--lsp".to_string(), "--stdio".to_string()],
                env: HashMap::new(),
                file_patterns: vec!["**/*.ts".to_string(), "**/*.tsx".to_string()],
                initialization_options: None,
                timeout_seconds: 30,
                request_timeout_seconds: 30,
                heuristics: None,
                name: None,
                handles: None,
            }],
            project_config_ignored: false,
        };

        let map = config.build_effective_extension_map();
        assert_eq!(map.get("ts"), Some(&"typescript".to_string()));
        assert_eq!(map.get("tsx"), Some(&"typescriptreact".to_string()));
    }

    #[test]
    fn test_build_effective_extension_map_derives_jsx_language_id() {
        let config = ServerConfig {
            workspace: WorkspaceConfig::default(),
            lsp_servers: vec![LspServerConfig {
                language_id: "javascript".to_string(),
                command: "typescript-language-server".to_string(),
                args: vec!["--stdio".to_string()],
                env: HashMap::new(),
                file_patterns: vec!["**/*.js".to_string(), "**/*.jsx".to_string()],
                initialization_options: None,
                timeout_seconds: 30,
                request_timeout_seconds: 30,
                heuristics: None,
                name: None,
                handles: None,
            }],
            project_config_ignored: false,
        };

        let map = config.build_effective_extension_map();
        assert_eq!(map.get("js"), Some(&"javascript".to_string()));
        assert_eq!(map.get("jsx"), Some(&"javascriptreact".to_string()));
    }

    #[test]
    fn test_build_effective_extension_map_ignores_complex_patterns_without_extension() {
        let config = ServerConfig {
            workspace: WorkspaceConfig::default(),
            lsp_servers: vec![LspServerConfig {
                language_id: "cpp".to_string(),
                command: "clangd".to_string(),
                args: vec![],
                env: HashMap::new(),
                file_patterns: vec!["**/*".to_string(), "**/*.{h,hpp}".to_string()],
                initialization_options: None,
                timeout_seconds: 30,
                request_timeout_seconds: 30,
                heuristics: None,
                name: None,
                handles: None,
            }],
            project_config_ignored: false,
        };

        let map = config.build_effective_extension_map();
        // Default C/C++ mappings remain unchanged when patterns cannot be parsed.
        assert_eq!(map.get("h"), Some(&"c".to_string()));
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
            heuristics_max_depth: DEFAULT_HEURISTICS_MAX_DEPTH,
            max_documents: DEFAULT_MAX_DOCUMENTS,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
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

    // These tests mutate the process-wide CWD via `set_current_dir`, so they
    // must not run concurrently with each other or with any other test that
    // relies on CWD (e.g. via a bare `load()`/`load_with_trust()` call).
    // Nextest runs each test in its own process, but `cargo test` in-process
    // would race; guard with a mutex. `CwdGuard` below additionally restores
    // the original directory on drop, so a panic mid-test (e.g. a failed
    // `assert_eq!` between the temp-dir switch and the manual restore) can
    // never leave the process cwd changed for the rest of the run.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that serializes CWD-mutating tests behind [`CWD_LOCK`] and
    /// switches into `dir` for the guard's lifetime, restoring the original
    /// working directory on drop — including on an early return or panic.
    struct CwdGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        original_dir: PathBuf,
    }

    impl CwdGuard {
        fn enter(dir: &Path) -> Self {
            let lock = CWD_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let original_dir = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            Self {
                _lock: lock,
                original_dir,
            }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let restored = std::env::set_current_dir(&self.original_dir);
            // A failure here during an already-unwinding panic must not
            // panic again (double panic aborts the process, losing the
            // original failure's message). On the normal path, though,
            // silently swallowing this would leave the process cwd wrong
            // for every subsequent test with no diagnostic — panic loudly
            // instead, since that's exactly the failure mode this guard
            // exists to prevent.
            if !std::thread::panicking() {
                #[allow(clippy::expect_used)]
                restored.expect("CwdGuard failed to restore original working directory");
            }
        }
    }

    #[test]
    fn test_cwd_guard_restores_cwd_on_panic() {
        let original_dir = std::env::current_dir().unwrap();
        let tmp_dir = TempDir::new().unwrap();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = CwdGuard::enter(tmp_dir.path());
            panic!("boom");
        }));

        assert!(result.is_err());
        assert_eq!(std::env::current_dir().unwrap(), original_dir);
    }

    /// Precondition for tests that assert on `ServerConfig::load_with_trust`'s
    /// CWD-local-file branch: a `$MCPLS_CONFIG` set in the ambient
    /// environment makes `load_with_trust` return before ever looking at
    /// CWD (see its `MCPLS_CONFIG` branch above), which would otherwise fail
    /// the test for a reason unrelated to the code under test.
    ///
    /// Scrubbing the variable for the test's duration would be the more
    /// thorough fix, but `std::env::remove_var`/`set_var` are `unsafe`
    /// (mutate process-wide state) and this crate denies `unsafe_code`
    /// workspace-wide with no existing exception — so this asserts the
    /// precondition instead of silently working around it, turning an
    /// environment-dependent false failure into an explicit, legible one.
    fn assert_mcpls_config_env_unset() {
        assert!(
            std::env::var_os("MCPLS_CONFIG").is_none(),
            "this test requires MCPLS_CONFIG to be unset in the test environment, since \
             load_with_trust returns before consulting CWD when it's set"
        );
    }

    #[test]
    fn test_load_ignores_untrusted_project_local_config() {
        // `ServerConfig::default()` (what untrusted discovery falls back to
        // once neither an untrusted local file nor a global config apply)
        // still exposes rust-analyzer via built-in project-marker
        // heuristics — see `test_default_config` above, which already
        // covers this without any filesystem interaction. This test only
        // needs to prove the planted attacker file's content never leaks
        // through `load()`.
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("mcpls.toml");

        // A marker language id / root that cannot collide with either the
        // built-in defaults or a machine-local global config, so this
        // assertion holds regardless of what `load()` actually falls
        // through to (built-in defaults on a clean machine, or the
        // machine's own customized global config in CI/dev environments).
        let custom_toml = r#"
            [workspace]
            roots = ["/should-never-load-attacker-path"]

            [[lsp_servers]]
            language_id = "definitely-not-a-real-language-marker"
            command = "rm"
            args = ["-rf", "/"]
        "#;

        fs::write(&config_path, custom_toml).unwrap();

        let config = {
            let _guard = CwdGuard::enter(tmp_dir.path());
            ServerConfig::load().unwrap()
        };

        assert!(
            !config
                .workspace
                .roots
                .contains(&PathBuf::from("/should-never-load-attacker-path"))
        );
        assert!(
            !config
                .lsp_servers
                .iter()
                .any(|s| s.language_id == "definitely-not-a-real-language-marker")
        );
    }

    #[test]
    fn test_load_with_trust_loads_trusted_project_local_config() {
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

        let config = {
            let _guard = CwdGuard::enter(tmp_dir.path());
            ServerConfig::load_with_trust(ProjectConfigTrust::Trusted).unwrap()
        };

        assert_eq!(config.workspace.roots, vec![PathBuf::from("/custom/path")]);
        assert_eq!(config.lsp_servers.len(), 1);
        assert_eq!(config.lsp_servers[0].language_id, "python");
    }

    #[test]
    fn test_load_with_trust_untrusted_ignores_workspace_and_servers() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("mcpls.toml");

        let custom_toml = r#"
            [workspace]
            roots = ["/attacker/controlled"]
            heuristics_max_depth = 999999

            [[lsp_servers]]
            language_id = "evil"
            command = "rm"
            args = ["-rf", "/"]
        "#;

        fs::write(&config_path, custom_toml).unwrap();

        let config = {
            let _guard = CwdGuard::enter(tmp_dir.path());
            ServerConfig::load_with_trust(ProjectConfigTrust::Untrusted).unwrap()
        };

        assert!(
            !config
                .workspace
                .roots
                .contains(&PathBuf::from("/attacker/controlled"))
        );
        assert_ne!(config.workspace.heuristics_max_depth, 999_999);
        assert!(!config.lsp_servers.iter().any(|s| s.language_id == "evil"));
    }

    #[test]
    fn test_load_with_trust_sets_project_config_ignored_flag() {
        assert_mcpls_config_env_unset();

        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("mcpls.toml");
        fs::write(&config_path, "[workspace]\nroots = []\n").unwrap();

        let config = {
            let _guard = CwdGuard::enter(tmp_dir.path());
            ServerConfig::load_with_trust(ProjectConfigTrust::Untrusted).unwrap()
        };
        assert!(config.project_config_ignored);

        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("mcpls.toml");
        fs::write(&config_path, "[workspace]\nroots = []\n").unwrap();

        let config = {
            let _guard = CwdGuard::enter(tmp_dir.path());
            ServerConfig::load_with_trust(ProjectConfigTrust::Trusted).unwrap()
        };
        assert!(!config.project_config_ignored);
    }

    #[test]
    fn test_load_no_local_config_leaves_flag_unset() {
        assert_mcpls_config_env_unset();

        let tmp_dir = TempDir::new().unwrap();

        let config = {
            let _guard = CwdGuard::enter(tmp_dir.path());
            ServerConfig::load_with_trust(ProjectConfigTrust::Untrusted).unwrap()
        };
        assert!(!config.project_config_ignored);
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

    #[test]
    fn test_heuristics_max_depth_default() {
        let config = WorkspaceConfig::default();
        assert_eq!(config.heuristics_max_depth, 10);
    }

    #[test]
    fn test_heuristics_max_depth_from_config() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("depth.toml");

        let toml_content = r"
            [workspace]
            heuristics_max_depth = 5
        ";

        fs::write(&config_path, toml_content).unwrap();

        let config = ServerConfig::load_from(&config_path).unwrap();
        assert_eq!(config.workspace.heuristics_max_depth, 5);
    }

    #[test]
    fn test_heuristics_max_depth_uses_default_when_not_specified() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("no_depth.toml");

        let toml_content = r"
            [workspace]
            roots = []
        ";

        fs::write(&config_path, toml_content).unwrap();

        let config = ServerConfig::load_from(&config_path).unwrap();
        assert_eq!(
            config.workspace.heuristics_max_depth,
            DEFAULT_HEURISTICS_MAX_DEPTH
        );
    }

    #[test]
    fn test_max_documents_default() {
        let config = WorkspaceConfig::default();
        assert_eq!(config.max_documents, DEFAULT_MAX_DOCUMENTS);
    }

    #[test]
    fn test_max_file_size_default() {
        let config = WorkspaceConfig::default();
        assert_eq!(config.max_file_size, DEFAULT_MAX_FILE_SIZE);
    }

    #[test]
    fn test_max_documents_from_config() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("limits.toml");

        let toml_content = r"
            [workspace]
            max_documents = 500
        ";

        fs::write(&config_path, toml_content).unwrap();

        let config = ServerConfig::load_from(&config_path).unwrap();
        assert_eq!(config.workspace.max_documents, 500);
    }

    #[test]
    fn test_max_file_size_from_config() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("limits.toml");

        let toml_content = r"
            [workspace]
            max_file_size = 20971520
        ";

        fs::write(&config_path, toml_content).unwrap();

        let config = ServerConfig::load_from(&config_path).unwrap();
        assert_eq!(config.workspace.max_file_size, 20_971_520);
    }

    #[test]
    fn test_max_documents_uses_default_when_not_specified() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("no_limits.toml");

        let toml_content = r"
            [workspace]
            roots = []
        ";

        fs::write(&config_path, toml_content).unwrap();

        let config = ServerConfig::load_from(&config_path).unwrap();
        assert_eq!(config.workspace.max_documents, DEFAULT_MAX_DOCUMENTS);
        assert_eq!(config.workspace.max_file_size, DEFAULT_MAX_FILE_SIZE);
    }

    /// `max_file_size = 0` is the documented "unlimited" sentinel (see
    /// `ResourceLimits::max_file_size`'s doc comment); config loading must
    /// pass it through unchanged rather than treating `0` as "unset".
    #[test]
    fn test_max_file_size_zero_means_unlimited() {
        let tmp_dir = TempDir::new().unwrap();
        let config_path = tmp_dir.path().join("unlimited.toml");

        let toml_content = r"
            [workspace]
            max_file_size = 0
        ";

        fs::write(&config_path, toml_content).unwrap();

        let config = ServerConfig::load_from(&config_path).unwrap();
        assert_eq!(config.workspace.max_file_size, 0);
        assert_eq!(config.workspace.resource_limits().max_file_size, 0);
    }

    #[test]
    fn test_workspace_config_resource_limits_maps_fields() {
        let workspace = WorkspaceConfig {
            max_documents: 250,
            max_file_size: 0,
            ..WorkspaceConfig::default()
        };

        let limits = workspace.resource_limits();
        assert_eq!(limits.max_documents, 250);
        assert_eq!(limits.max_file_size, 0);
    }

    #[test]
    fn test_workspace_config_toml_round_trip() {
        let original = WorkspaceConfig {
            roots: vec![PathBuf::from("/tmp/round-trip")],
            position_encodings: vec!["utf-8".to_string()],
            language_extensions: vec![LanguageExtensionMapping {
                extensions: vec!["nu".to_string()],
                language_id: "nushell".to_string(),
            }],
            heuristics_max_depth: 5,
            max_documents: 500,
            max_file_size: 0,
        };

        let toml_content = toml::to_string_pretty(&original).unwrap();
        let round_tripped: WorkspaceConfig = toml::from_str(&toml_content).unwrap();

        assert_eq!(round_tripped.roots, original.roots);
        assert_eq!(
            round_tripped.position_encodings,
            original.position_encodings
        );
        assert_eq!(
            round_tripped.language_extensions.len(),
            original.language_extensions.len()
        );
        assert_eq!(
            round_tripped.language_extensions[0].extensions,
            original.language_extensions[0].extensions
        );
        assert_eq!(
            round_tripped.language_extensions[0].language_id,
            original.language_extensions[0].language_id
        );
        assert_eq!(
            round_tripped.heuristics_max_depth,
            original.heuristics_max_depth
        );
        assert_eq!(round_tripped.max_documents, original.max_documents);
        assert_eq!(round_tripped.max_file_size, original.max_file_size);
    }
}
