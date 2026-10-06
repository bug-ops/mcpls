//! The environment overrides of a configured server.

use std::collections::BTreeMap;

use crate::lsp::HostOs;

/// Two keys of one `env` table name the same variable on the host.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "environment variable '{first}' is set twice ('{second}'): names compare case-insensitively on Windows"
)]
pub struct DuplicateEnvKey {
    /// The key seen first.
    pub first: String,
    /// The key that collides with it.
    pub second: String,
}

/// Environment variable overrides for a server process.
///
/// Holds at most one key per variable as the host compares names (exactly on
/// Unix, ASCII-case-insensitively on Windows), so a lookup or a write cannot
/// meet two spellings of one variable and mcpls resolves the same value the
/// child process sees. The host is fixed at construction, so a table built
/// for one host cannot be read or extended as another. Keys come in through
/// [`Self::from_entries`], which rejects collisions, or [`Self::insert`],
/// which replaces them.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ServerEnv;
/// use mcpls_core::lsp::HostOs;
///
/// let mut env = ServerEnv::new(HostOs::Windows);
/// env.insert("Path".to_owned(), "/a".to_owned());
/// assert_eq!(env.get("PATH"), Some("/a"));
///
/// env.insert("PATH".to_owned(), "/b".to_owned());
/// assert_eq!(env.len(), 1);
///
/// let mut other = ServerEnv::new(HostOs::Other);
/// other.insert("Path".to_owned(), "/a".to_owned());
/// assert_eq!(other.get("PATH"), None);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerEnv {
    host: HostOs,
    vars: BTreeMap<String, String>,
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
        entries: impl IntoIterator<Item = (String, String)>,
        host: HostOs,
    ) -> Result<Self, DuplicateEnvKey> {
        let mut env = Self::new(host);
        for (key, value) in entries {
            if let Some(first) = env.key_of(&key) {
                return Err(DuplicateEnvKey {
                    first: first.to_owned(),
                    second: key,
                });
            }
            env.vars.insert(key, value);
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
        self.key_of(key)
            .and_then(|stored| self.vars.get(stored))
            .map(String::as_str)
    }

    /// Whether `key` is set, as the table's host compares names.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.key_of(key).is_some()
    }

    /// Sets `key`, replacing every spelling of it that the table's host
    /// treats as the same variable.
    pub fn insert(&mut self, key: String, value: String) {
        while let Some(stored) = self.key_of(&key).map(str::to_owned) {
            self.vars.remove(&stored);
        }
        self.vars.insert(key, value);
    }

    /// The entries, ordered by key.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.vars.iter()
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

    /// The stored spelling that the table's host treats as `key`, an exact one first.
    fn key_of(&self, key: &str) -> Option<&str> {
        if let Some((stored, _)) = self.vars.get_key_value(key) {
            return Some(stored);
        }
        match self.host {
            HostOs::Windows => self
                .vars
                .keys()
                .find(|stored| stored.eq_ignore_ascii_case(key))
                .map(String::as_str),
            HostOs::Other => None,
        }
    }
}

impl From<ServerEnv> for BTreeMap<String, String> {
    fn from(env: ServerEnv) -> Self {
        env.vars
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
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
        windows.insert("PATH".to_owned(), "b".to_owned());
        assert_eq!(
            windows.iter().collect::<Vec<_>>(),
            [(&"PATH".to_owned(), &"b".to_owned())]
        );

        let mut other = ServerEnv::from_entries(entries(&[("Path", "a")]), HostOs::Other).unwrap();
        other.insert("PATH".to_owned(), "b".to_owned());
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
