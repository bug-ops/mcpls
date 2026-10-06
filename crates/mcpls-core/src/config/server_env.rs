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
/// child process sees. Keys come in through [`Self::from_entries`], which
/// rejects collisions, or [`Self::insert`], which replaces them.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ServerEnv;
/// use mcpls_core::lsp::HostOs;
///
/// let mut env = ServerEnv::default();
/// env.insert("Path".to_owned(), "/a".to_owned(), HostOs::Windows);
/// assert_eq!(env.get("PATH", HostOs::Windows), Some("/a"));
/// assert_eq!(env.get("PATH", HostOs::Other), None);
///
/// env.insert("PATH".to_owned(), "/b".to_owned(), HostOs::Windows);
/// assert_eq!(env.len(), 1);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerEnv(BTreeMap<String, String>);

impl ServerEnv {
    /// Builds the table from `entries`, as `host` compares names.
    ///
    /// # Errors
    ///
    /// [`DuplicateEnvKey`] when two keys name one variable on `host`.
    pub fn from_entries(
        entries: impl IntoIterator<Item = (String, String)>,
        host: HostOs,
    ) -> Result<Self, DuplicateEnvKey> {
        let mut env = Self::default();
        for (key, value) in entries {
            if let Some(first) = env.key_of(&key, host) {
                return Err(DuplicateEnvKey {
                    first: first.to_owned(),
                    second: key,
                });
            }
            env.0.insert(key, value);
        }
        Ok(env)
    }

    /// The value of `key`, looked up as `host` compares names.
    #[must_use]
    pub fn get(&self, key: &str, host: HostOs) -> Option<&str> {
        self.key_of(key, host)
            .and_then(|stored| self.0.get(stored))
            .map(String::as_str)
    }

    /// Whether `key` is set, as `host` compares names.
    #[must_use]
    pub fn contains_key(&self, key: &str, host: HostOs) -> bool {
        self.key_of(key, host).is_some()
    }

    /// Sets `key`, replacing every spelling of it that `host` treats as the
    /// same variable.
    pub fn insert(&mut self, key: String, value: String, host: HostOs) {
        while let Some(stored) = self.key_of(&key, host).map(str::to_owned) {
            self.0.remove(&stored);
        }
        self.0.insert(key, value);
    }

    /// The entries, ordered by key.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }

    /// The number of variables set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no variable is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The stored spelling that `host` treats as `key`, an exact one first.
    fn key_of(&self, key: &str, host: HostOs) -> Option<&str> {
        if let Some((stored, _)) = self.0.get_key_value(key) {
            return Some(stored);
        }
        match host {
            HostOs::Windows => self
                .0
                .keys()
                .find(|stored| stored.eq_ignore_ascii_case(key))
                .map(String::as_str),
            HostOs::Other => None,
        }
    }
}

impl From<ServerEnv> for BTreeMap<String, String> {
    fn from(env: ServerEnv) -> Self {
        env.0
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
            let env = ServerEnv::from_entries(
                entries(&[(&name.to_ascii_lowercase(), "v")]),
                HostOs::Windows,
            )
            .unwrap();
            assert_eq!(env.get(name, HostOs::Windows), Some("v"), "{name}");
            assert_eq!(env.get(name, HostOs::Other), None, "{name}");
            assert!(env.contains_key(name, HostOs::Windows), "{name}");
            assert!(!env.contains_key(name, HostOs::Other), "{name}");
        }
    }

    #[test]
    fn insert_replaces_every_alias_on_windows_and_only_the_exact_key_elsewhere() {
        let mut windows =
            ServerEnv::from_entries(entries(&[("Path", "a")]), HostOs::Windows).unwrap();
        windows.insert("PATH".to_owned(), "b".to_owned(), HostOs::Windows);
        assert_eq!(
            windows.iter().collect::<Vec<_>>(),
            [(&"PATH".to_owned(), &"b".to_owned())]
        );

        let mut other = ServerEnv::from_entries(entries(&[("Path", "a")]), HostOs::Other).unwrap();
        other.insert("PATH".to_owned(), "b".to_owned(), HostOs::Other);
        assert_eq!(other.len(), 2);
        assert_eq!(other.get("Path", HostOs::Other), Some("a"));
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
        assert_eq!(env.get("PATH", HostOs::Windows), Some("b"));
    }
}
