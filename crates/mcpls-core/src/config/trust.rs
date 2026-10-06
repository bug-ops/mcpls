//! Trust the user places in the analyzed workspace.
//!
//! Language servers run workspace code (build scripts, procedural macros,
//! tsconfig plugins). [`WorkspaceTrust::Untrusted`] makes mcpls start only the
//! servers the user named, so an unvetted checkout cannot reach the rest.

use std::path::PathBuf;

use super::ServerId;

/// The current user's home directory as the account database records it,
/// canonicalized; `None` when it cannot be determined.
///
/// Deliberately not `$HOME`: a checkout's tooling can set that, which would
/// let it name itself "the home directory" and so exempt itself from the
/// untrusted-mode workspace boundary.
pub fn login_home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    let home = nix::unistd::User::from_uid(nix::unistd::Uid::current())
        .ok()
        .flatten()
        .map(|user| user.dir);
    #[cfg(not(unix))]
    let home = dirs::home_dir();
    home.and_then(|home| dunce::canonicalize(home).ok())
}

/// Whether the analyzed workspace is trusted to be handed to language servers.
///
/// Distinct from [`ProjectConfigTrust`](super::ProjectConfigTrust), which
/// governs only a project-local `./mcpls.toml` and defaults to untrusted.
/// This value defaults to [`Trusted`](Self::Trusted), the behavior of every
/// earlier release. It is never read from a config file: consent to start a
/// server comes from the command line of the user who launches mcpls, so a
/// planted `mcpls.toml` cannot grant it.
///
/// Untrusted mode is not a sandbox. A server the user allows still runs the
/// workspace code it would run in a trusted workspace.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::{ServerId, WorkspaceTrust};
///
/// let rust = ServerId::from("rust");
/// assert!(WorkspaceTrust::default().allows(&rust));
///
/// let untrusted = WorkspaceTrust::untrusted([rust.clone()]);
/// assert!(untrusted.allows(&rust));
/// assert!(!untrusted.allows(&ServerId::from("python")));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum WorkspaceTrust {
    /// Every applicable server is started.
    #[default]
    Trusted,
    /// Only the servers in the allowlist are started.
    Untrusted(ServerAllowlist),
}

impl WorkspaceTrust {
    /// Untrusted mode that starts exactly the servers in `allowed`.
    #[must_use]
    pub fn untrusted(allowed: impl IntoIterator<Item = ServerId>) -> Self {
        Self::Untrusted(ServerAllowlist::new(allowed))
    }

    /// Whether the server `id` may be started.
    #[must_use]
    pub fn allows(&self, id: &ServerId) -> bool {
        match self {
            Self::Trusted => true,
            Self::Untrusted(allowlist) => allowlist.contains(id),
        }
    }
}

/// The servers a user explicitly allowed in an untrusted workspace.
///
/// Duplicates are dropped, keeping the first occurrence, so the order the user
/// gave is preserved for error messages.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::{ServerAllowlist, ServerId};
///
/// let list = ServerAllowlist::new(["rust", "rust", "go"].map(ServerId::from));
/// assert_eq!(list.as_slice().len(), 2);
/// assert!(list.contains(&ServerId::from("go")));
/// assert!(!ServerAllowlist::default().contains(&ServerId::from("go")));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerAllowlist(Vec<ServerId>);

impl ServerAllowlist {
    /// Collects `allowed`, dropping repeated ids.
    #[must_use]
    pub fn new(allowed: impl IntoIterator<Item = ServerId>) -> Self {
        let mut ids: Vec<ServerId> = Vec::new();
        for id in allowed {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Self(ids)
    }

    /// Whether `id` was allowed.
    #[must_use]
    pub fn contains(&self, id: &ServerId) -> bool {
        self.0.contains(id)
    }

    /// The allowed ids, in the order they were first given.
    #[must_use]
    pub fn as_slice(&self) -> &[ServerId] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_is_trusted_and_allows_everything() {
        assert_eq!(WorkspaceTrust::default(), WorkspaceTrust::Trusted);
        assert!(WorkspaceTrust::Trusted.allows(&ServerId::from("anything")));
    }

    #[test]
    fn test_untrusted_allows_only_listed_ids() {
        let trust = WorkspaceTrust::untrusted([ServerId::from("rust")]);
        assert!(trust.allows(&ServerId::from("rust")));
        assert!(!trust.allows(&ServerId::from("python")));
        assert!(!WorkspaceTrust::untrusted([]).allows(&ServerId::from("rust")));
    }

    #[test]
    fn test_allowlist_deduplicates_keeping_first_occurrence() {
        let list = ServerAllowlist::new(["b", "a", "b", "a"].map(ServerId::from));
        assert_eq!(list.as_slice(), ["b", "a"].map(ServerId::from));
    }
}
