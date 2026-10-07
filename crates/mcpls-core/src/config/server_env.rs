//! The environment overrides of a configured server.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::host_os::HostOs;
use super::text_newtype::impl_text_newtype;

/// Why a string is not a valid [`EnvKey`].
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidEnvKey {
    /// The name was empty.
    #[error("environment variable name cannot be empty")]
    Empty,
    /// The name contained `=`, which separates a name from its value, or a NUL.
    #[error("environment variable name cannot contain '=' or NUL")]
    ForbiddenChar,
}

const fn is_valid_env_key(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    let mut rest = key.as_bytes();
    while let [byte, tail @ ..] = rest {
        if matches!(byte, b'=' | 0) {
            return false;
        }
        rest = tail;
    }
    true
}

const fn check_env_key(key: &str) -> Result<(), InvalidEnvKey> {
    if key.is_empty() {
        Err(InvalidEnvKey::Empty)
    } else if is_valid_env_key(key) {
        Ok(())
    } else {
        Err(InvalidEnvKey::ForbiddenChar)
    }
}

/// The name of an environment variable: non-empty, with no `=` and no NUL, so
/// the operating system accepts it as one.
///
/// Deserializes from a TOML string and rejects any other name at load time.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::EnvKey;
///
/// assert_eq!(EnvKey::new("RUST_LOG").unwrap(), "RUST_LOG");
/// assert!(EnvKey::new("").is_err());
/// assert!(EnvKey::new("A=B").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EnvKey(Cow<'static, str>);

impl AsRef<std::ffi::OsStr> for EnvKey {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.as_str().as_ref()
    }
}

impl_text_newtype!(
    EnvKey,
    InvalidEnvKey,
    checked = check_env_key,
    valid = is_valid_env_key,
    "environment variable name"
);

/// Two keys of one `env` table name the same variable on the host.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "environment variable '{first}' is set twice ('{second}'): names compare case-insensitively on Windows"
)]
pub struct DuplicateEnvKey {
    /// The key seen first.
    pub first: EnvKey,
    /// The key that collides with it.
    pub second: EnvKey,
}

/// A variable name as a host compares it: the key a table is indexed by.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct HostKey(String);

impl HostKey {
    fn of(name: &str, host: HostOs) -> Self {
        Self(match host {
            HostOs::Windows => name.to_ascii_lowercase(),
            HostOs::Other => name.to_owned(),
        })
    }
}

/// Environment variable overrides for a server process.
///
/// Holds at most one key per variable as the host compares names (exactly on
/// Unix, ASCII-case-insensitively on Windows): the table is indexed by the
/// host's comparison of the name, so a lookup or a write cannot meet two
/// spellings of one variable and mcpls resolves the same value the child
/// process sees. The host is fixed at construction, so a table built for one
/// host cannot be read or extended as another. Keys come in through
/// [`Self::from_entries`], which rejects collisions, or [`Self::insert`],
/// which replaces them.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::{EnvKey, ServerEnv};
/// use mcpls_core::lsp::HostOs;
///
/// let mut env = ServerEnv::new(HostOs::Windows);
/// env.insert(EnvKey::new("Path").unwrap(), "/a".to_owned());
/// assert_eq!(env.get("PATH"), Some("/a"));
///
/// env.insert(EnvKey::new("PATH").unwrap(), "/b".to_owned());
/// assert_eq!(env.len(), 1);
///
/// let mut other = ServerEnv::new(HostOs::Other);
/// other.insert(EnvKey::new("Path").unwrap(), "/a".to_owned());
/// assert_eq!(other.get("PATH"), None);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerEnv {
    host: HostOs,
    vars: BTreeMap<HostKey, (EnvKey, String)>,
}

impl Default for ServerEnv {
    fn default() -> Self {
        Self::new(HostOs::CURRENT)
    }
}

impl ServerEnv {
    /// An empty table whose names compare as `host` compares them.
    #[must_use]
    pub const fn new(host: HostOs) -> Self {
        Self {
            host,
            vars: BTreeMap::new(),
        }
    }

    /// Builds the table from `entries`, as `host` compares names.
    ///
    /// # Errors
    ///
    /// [`DuplicateEnvKey`] when two keys name one variable on `host`.
    pub fn from_entries(
        entries: impl IntoIterator<Item = (EnvKey, String)>,
        host: HostOs,
    ) -> Result<Self, DuplicateEnvKey> {
        let mut env = Self::new(host);
        for (key, value) in entries {
            if let Some((first, _)) = env.vars.get(&HostKey::of(key.as_str(), host)) {
                return Err(DuplicateEnvKey {
                    first: first.clone(),
                    second: key,
                });
            }
            env.insert(key, value);
        }
        Ok(env)
    }

    /// The host whose name comparison this table follows.
    #[must_use]
    pub const fn host(&self) -> HostOs {
        self.host
    }

    /// The value of `key`, looked up as the table's host compares names.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars
            .get(&HostKey::of(key, self.host))
            .map(|(_, value)| value.as_str())
    }

    /// Whether `key` is set, as the table's host compares names.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.vars.contains_key(&HostKey::of(key, self.host))
    }

    /// Sets `key`, replacing the spelling of it that the table's host treats
    /// as the same variable.
    pub fn insert(&mut self, key: EnvKey, value: String) {
        self.vars
            .insert(HostKey::of(key.as_str(), self.host), (key, value));
    }

    /// The entries, ordered by the host's comparison of the name.
    pub fn iter(&self) -> impl Iterator<Item = (&EnvKey, &String)> {
        self.vars.values().map(|(key, value)| (key, value))
    }

    /// The number of variables set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// Whether no variable is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }
}

impl From<ServerEnv> for BTreeMap<EnvKey, String> {
    fn from(env: ServerEnv) -> Self {
        env.vars.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> EnvKey {
        EnvKey::new(name).unwrap()
    }

    fn entries(pairs: &[(&str, &str)]) -> Vec<(EnvKey, String)> {
        pairs
            .iter()
            .map(|(name, value)| (key(name), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn env_key_rejects_what_the_os_would() {
        assert_eq!(EnvKey::new(""), Err(InvalidEnvKey::Empty));
        assert_eq!(EnvKey::new("A=B"), Err(InvalidEnvKey::ForbiddenChar));
        assert_eq!(EnvKey::new("A\0B"), Err(InvalidEnvKey::ForbiddenChar));
        assert!(EnvKey::new("Mixed_Case1").is_ok());
        assert_eq!(EnvKey::from_static("PATH"), "PATH");
    }

    #[test]
    fn env_key_is_checked_when_a_config_is_deserialized() {
        #[derive(Debug, Deserialize)]
        struct Holder {
            env: BTreeMap<EnvKey, String>,
        }

        let holder: Holder = toml::from_str("[env]\nA = \"1\"").unwrap();
        assert_eq!(holder.env.len(), 1);
        assert!(toml::from_str::<Holder>("[env]\n\"A=B\" = \"1\"").is_err());
        assert!(toml::from_str::<Holder>("[env]\n\"\" = \"1\"").is_err());
    }

    /// #691: lookups compare names as the host does, for every variable the
    /// resolution and hardening code asks for.
    #[test]
    fn get_compares_names_as_the_host_does() {
        for name in [
            "PATH",
            "HOME",
            "USERPROFILE",
            "NoDefaultCurrentDirectoryInExePath",
        ] {
            let lowered = entries(&[(&name.to_ascii_lowercase(), "v")]);
            let windows = ServerEnv::from_entries(lowered.clone(), HostOs::Windows).unwrap();
            assert_eq!(windows.get(name), Some("v"), "{name}");
            assert!(windows.contains_key(name), "{name}");

            let other = ServerEnv::from_entries(lowered, HostOs::Other).unwrap();
            assert_eq!(other.get(name), None, "{name}");
            assert!(!other.contains_key(name), "{name}");
        }
    }

    #[test]
    fn insert_replaces_every_alias_on_windows_and_only_the_exact_key_elsewhere() {
        let mut windows =
            ServerEnv::from_entries(entries(&[("Path", "a")]), HostOs::Windows).unwrap();
        windows.insert(key("PATH"), "b".to_owned());
        assert_eq!(
            windows.iter().collect::<Vec<_>>(),
            [(&key("PATH"), &"b".to_owned())]
        );

        let mut other = ServerEnv::from_entries(entries(&[("Path", "a")]), HostOs::Other).unwrap();
        other.insert(key("PATH"), "b".to_owned());
        assert_eq!(other.len(), 2);
        assert_eq!(other.get("Path"), Some("a"));
    }

    #[test]
    fn from_entries_rejects_case_variants_only_on_windows() {
        let err =
            ServerEnv::from_entries(entries(&[("Path", "a"), ("PATH", "b")]), HostOs::Windows)
                .unwrap_err();
        assert_eq!(err.first, "Path");
        assert_eq!(err.second, "PATH");
        let other =
            ServerEnv::from_entries(entries(&[("Path", "a"), ("PATH", "b")]), HostOs::Other)
                .unwrap();
        assert_eq!(other.len(), 2);
    }

    #[test]
    fn get_prefers_an_exact_spelling() {
        let env = ServerEnv::from_entries(entries(&[("Path", "a"), ("PATH", "b")]), HostOs::Other)
            .unwrap();
        assert_eq!(env.get("PATH"), Some("b"));
    }

    #[test]
    fn the_host_is_fixed_at_construction() {
        assert_eq!(ServerEnv::new(HostOs::Windows).host(), HostOs::Windows);
        assert_eq!(ServerEnv::default().host(), HostOs::CURRENT);
    }
}
