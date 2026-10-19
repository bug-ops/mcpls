//! The environment overrides of a configured server.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::host_os::HostOs;
use super::text_newtype::{contains_nul, impl_text_newtype};

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

const fn env_key_fault(key: &str) -> Option<InvalidEnvKey> {
    if key.is_empty() {
        return Some(InvalidEnvKey::Empty);
    }
    let mut rest = key.as_bytes();
    while let [byte, tail @ ..] = rest {
        if matches!(byte, b'=' | 0) {
            return Some(InvalidEnvKey::ForbiddenChar);
        }
        rest = tail;
    }
    None
}

/// Why a string is not a valid [`EnvValue`].
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidEnvValue {
    /// The value contained a NUL, which the operating system cannot pass in
    /// an environment.
    #[error("environment variable value cannot contain NUL")]
    ContainsNul,
}

const fn env_value_fault(value: &str) -> Option<InvalidEnvValue> {
    if contains_nul(value) {
        Some(InvalidEnvValue::ContainsNul)
    } else {
        None
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
    fault = env_key_fault,
    "environment variable name"
);

/// The value of an environment variable: any text without a NUL, so the
/// operating system accepts it as one.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::EnvValue;
///
/// assert_eq!(EnvValue::new("debug").unwrap(), "debug");
/// assert!(EnvValue::new("").is_ok());
/// assert!(EnvValue::new("a\0b").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnvValue(Cow<'static, str>);

impl AsRef<std::ffi::OsStr> for EnvValue {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.as_str().as_ref()
    }
}

impl_text_newtype!(
    EnvValue,
    InvalidEnvValue,
    fault = env_value_fault,
    "environment variable value"
);

/// The largest total size, in bytes, of a server's `env` table, counting each
/// entry as `NAME=value` and a terminator.
///
/// Well under the 1 MiB macOS allows for arguments and environment together,
/// so the table leaves room for the inherited environment and the arguments;
/// past the platform limit a spawn fails with `E2BIG`.
pub const MAX_ENV_TOTAL_BYTES: usize = 128 * 1024;

/// The `=` between name and value and the terminator the OS adds to an entry.
const ENTRY_OVERHEAD_BYTES: usize = 2;

fn entry_bytes(key: &EnvKey, value: &EnvValue) -> usize {
    key.as_str()
        .len()
        .saturating_add(value.as_str().len())
        .saturating_add(ENTRY_OVERHEAD_BYTES)
}

/// Why a [`ServerEnv`] is too large to pass to a process.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnvTooLarge {
    /// One entry is over [`HostOs::max_env_entry_bytes`] of the table's host.
    #[error(
        "environment variable '{key}' is {bytes} bytes as NAME=value and a terminator, over the {limit} byte limit"
    )]
    Entry {
        /// The variable.
        key: EnvKey,
        /// Its size as `NAME=value`.
        bytes: usize,
        /// The most one entry may take on the table's host.
        limit: usize,
    },
    /// The entries together are over [`MAX_ENV_TOTAL_BYTES`].
    #[error("environment is {bytes} bytes in total, over the {MAX_ENV_TOTAL_BYTES} byte limit")]
    Total {
        /// The size of all entries.
        bytes: usize,
    },
}

/// Why entries do not make a valid [`ServerEnv`].
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidServerEnv {
    /// Two keys name one variable on the host.
    #[error(transparent)]
    Duplicate(DuplicateEnvKey),
    /// The table is too large to pass to a process.
    #[error(transparent)]
    TooLarge(EnvTooLarge),
}

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
            HostOs::Windows => name.chars().map(windows_fold).collect(),
            HostOs::Other => name.to_owned(),
        })
    }
}

/// The character Windows compares `c` as, approximately: its Unicode
/// uppercase (the full mapping, not the simple one) when that is a single
/// character and both stay in the Basic Multilingual Plane, `c` itself
/// otherwise. Windows folds names with a per-UTF-16-unit table, so a
/// supplementary-plane letter or a mapping that expands (`ß` to `SS`) does not
/// fold. A character whose full mapping expands but whose simple mapping is one
/// character (U+1F80) is left unfolded here although Windows folds it.
fn windows_fold(c: char) -> char {
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(folded), None) if c.len_utf16() == 1 && folded.len_utf16() == 1 => folded,
        _ => c,
    }
}

/// Environment variable overrides for a server process.
///
/// Holds at most one key per variable as the host compares names (exactly on
/// Unix, case-insensitively as Windows compares names): the table is indexed by the
/// host's comparison of the name, so a lookup or a write cannot meet two
/// spellings of one variable and mcpls resolves the same value the child
/// process sees. The host is fixed at construction, so a table built for one
/// host cannot be read or extended as another. Keys come in through
/// [`Self::from_entries`], which rejects collisions, or [`Self::insert`],
/// which replaces them. Neither lets the table outgrow what the host can pass
/// to a process: [`HostOs::max_env_entry_bytes`] for one entry and
/// [`MAX_ENV_TOTAL_BYTES`] for all.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::{EnvKey, EnvValue, HostOs, ServerEnv};
///
/// let mut env = ServerEnv::new(HostOs::Windows);
/// env.insert(EnvKey::new("Path").unwrap(), EnvValue::new("/a").unwrap())?;
/// assert_eq!(env.get("PATH"), Some("/a"));
///
/// env.insert(EnvKey::new("PATH").unwrap(), EnvValue::new("/b").unwrap())?;
/// assert_eq!(env.len(), 1);
///
/// let mut other = ServerEnv::new(HostOs::Other);
/// other.insert(EnvKey::new("Path").unwrap(), EnvValue::new("/a").unwrap())?;
/// assert_eq!(other.get("PATH"), None);
/// # Ok::<(), mcpls_core::config::EnvTooLarge>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerEnv {
    host: HostOs,
    vars: BTreeMap<HostKey, (EnvKey, EnvValue)>,
    /// The size of all entries as `NAME=value` and a terminator.
    total_bytes: usize,
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
            total_bytes: 0,
        }
    }

    /// Builds the table from `entries`, as `host` compares names.
    ///
    /// # Errors
    ///
    /// [`InvalidServerEnv::Duplicate`] when two keys name one variable on
    /// `host`, [`InvalidServerEnv::TooLarge`] when an entry or the table is
    /// too large to pass to a process there.
    pub fn from_entries(
        entries: impl IntoIterator<Item = (EnvKey, EnvValue)>,
        host: HostOs,
    ) -> Result<Self, InvalidServerEnv> {
        let mut env = Self::new(host);
        for (key, value) in entries {
            if let Some((first, _)) = env.vars.get(&HostKey::of(key.as_str(), host)) {
                return Err(InvalidServerEnv::Duplicate(DuplicateEnvKey {
                    first: first.clone(),
                    second: key,
                }));
            }
            env.insert(key, value).map_err(InvalidServerEnv::TooLarge)?;
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
    ///
    /// # Errors
    ///
    /// [`EnvTooLarge`] when the entry is over [`HostOs::max_env_entry_bytes`]
    /// or the table would be over [`MAX_ENV_TOTAL_BYTES`], the sizes past which
    /// a spawn fails with `E2BIG`; the table is left as it was.
    pub fn insert(&mut self, key: EnvKey, value: EnvValue) -> Result<(), EnvTooLarge> {
        let bytes = key
            .as_str()
            .len()
            .saturating_add(value.as_str().len())
            .saturating_add(ENTRY_OVERHEAD_BYTES);
        let limit = self.host.max_env_entry_bytes();
        if bytes > limit {
            return Err(EnvTooLarge::Entry { key, bytes, limit });
        }
        let host_key = HostKey::of(key.as_str(), self.host);
        let replaced = self
            .vars
            .get(&host_key)
            .map_or(0, |(old_key, old_value)| entry_bytes(old_key, old_value));
        let total = self
            .total_bytes
            .saturating_sub(replaced)
            .saturating_add(bytes);
        if total > MAX_ENV_TOTAL_BYTES {
            return Err(EnvTooLarge::Total { bytes: total });
        }
        self.total_bytes = total;
        self.vars.insert(host_key, (key, value));
        Ok(())
    }

    /// The entries, ordered by the host's comparison of the name.
    pub fn iter(&self) -> impl Iterator<Item = (&EnvKey, &EnvValue)> {
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
        env.vars
            .into_values()
            .map(|(key, value)| (key, String::from(value)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    fn key(name: &str) -> EnvKey {
        EnvKey::new(name).unwrap()
    }

    fn value(text: &str) -> EnvValue {
        EnvValue::new(text).unwrap()
    }

    fn entries(pairs: &[(&str, &str)]) -> Vec<(EnvKey, EnvValue)> {
        pairs
            .iter()
            .map(|(name, text)| (key(name), value(text)))
            .collect()
    }

    #[test]
    fn insert_bounds_each_entry_by_the_tables_host() {
        for host in [HostOs::Other, HostOs::Windows] {
            let max = host.max_env_entry_bytes();
            let mut env = ServerEnv::new(host);
            assert_eq!(env.insert(key("A"), value(&"x".repeat(max - 3))), Ok(()));

            assert_eq!(
                env.insert(key("A"), value(&"x".repeat(max - 2))),
                Err(EnvTooLarge::Entry {
                    key: key("A"),
                    bytes: max + 1,
                    limit: max,
                })
            );
            assert_eq!(env.get("A").map(str::len), Some(max - 3));
        }
    }

    #[test]
    fn insert_bounds_the_total_and_counts_a_replacement_once() {
        let mut env = ServerEnv::new(HostOs::Other);
        for index in 0..6 {
            env.insert(key(&format!("K{index}")), value(&"x".repeat(20_000)))
                .unwrap();
        }
        assert_matches!(
            env.insert(key("K6"), value(&"x".repeat(20_000))),
            Err(EnvTooLarge::Total { .. })
        );
        assert_eq!(env.len(), 6);

        env.insert(key("K0"), value(&"y".repeat(20_000))).unwrap();
        assert_eq!(env.len(), 6);
    }

    #[test]
    fn from_entries_refuses_a_table_that_is_too_large_or_has_a_duplicate() {
        let big = |name: &str| (key(name), value(&"x".repeat(20_000)));
        let too_large =
            ServerEnv::from_entries((0..8).map(|n| big(&format!("K{n}"))), HostOs::Other);
        assert_matches!(
            too_large,
            Err(InvalidServerEnv::TooLarge(EnvTooLarge::Total { .. }))
        );
        let duplicate =
            ServerEnv::from_entries(entries(&[("Path", "a"), ("PATH", "b")]), HostOs::Windows);
        assert_matches!(duplicate, Err(InvalidServerEnv::Duplicate(_)));
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
    fn env_value_rejects_nul_and_accepts_empty_text() {
        assert_eq!(EnvValue::new("a\0b"), Err(InvalidEnvValue::ContainsNul));
        assert!(EnvValue::new("").is_ok());
        assert_eq!(EnvValue::from_static("1"), "1");
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
        windows.insert(key("PATH"), value("b")).unwrap();
        assert_eq!(
            windows.iter().collect::<Vec<_>>(),
            [(&key("PATH"), &value("b"))]
        );

        let mut other = ServerEnv::from_entries(entries(&[("Path", "a")]), HostOs::Other).unwrap();
        other.insert(key("PATH"), value("b")).unwrap();
        assert_eq!(other.len(), 2);
        assert_eq!(other.get("Path"), Some("a"));
    }

    #[test]
    fn from_entries_rejects_case_variants_only_on_windows() {
        let err =
            ServerEnv::from_entries(entries(&[("Path", "a"), ("PATH", "b")]), HostOs::Windows)
                .unwrap_err();
        let InvalidServerEnv::Duplicate(err) = err else {
            panic!("expected a duplicate, got {err:?}");
        };
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
    fn windows_folds_letters_in_the_basic_plane_only() {
        let key = |name: &str| HostKey::of(name, HostOs::Windows);
        assert_eq!(key("\u{e9}"), key("\u{c9}"));
        assert_eq!(key("Path"), key("PATH"));
        assert_ne!(key("\u{10428}"), key("\u{10400}"));
        assert_ne!(key("\u{df}"), key("SS"));
        assert_eq!(key("\u{df}"), key("\u{df}"));
    }

    #[test]
    fn other_hosts_compare_names_exactly() {
        let key = |name: &str| HostKey::of(name, HostOs::Other);
        assert_ne!(key("\u{e9}"), key("\u{c9}"));
    }

    #[test]
    fn the_host_is_fixed_at_construction() {
        assert_eq!(ServerEnv::new(HostOs::Windows).host(), HostOs::Windows);
        assert_eq!(ServerEnv::default().host(), HostOs::CURRENT);
    }
}
