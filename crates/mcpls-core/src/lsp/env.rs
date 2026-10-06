//! The environment a spawned server starts from, and the variables mcpls manages in it.

use std::ffi::OsString;

use crate::error::HomeVariable;

/// A lookup into mcpls's own environment, injected so callers and tests need
/// not touch the real process environment.
///
/// Implemented by every `Fn(&str) -> Option<OsString>`; production code passes
/// [`process_env`].
pub trait ParentEnv: Fn(&str) -> Option<OsString> {}

impl<F: Fn(&str) -> Option<OsString>> ParentEnv for F {}

/// The [`ParentEnv`] of the running process.
#[must_use]
pub fn process_env(key: &str) -> Option<OsString> {
    std::env::var_os(key)
}

/// An environment variable untrusted-workspace mode sets or normalizes for a
/// server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedEnvVar {
    /// `PATH`.
    Path,
    /// `HOME`.
    Home,
    /// `USERPROFILE`.
    UserProfile,
    /// `NoDefaultCurrentDirectoryInExePath`, which stops Windows from
    /// searching the current directory for an executable started by name.
    NoDefaultCurrentDirectoryInExePath,
}

impl ManagedEnvVar {
    /// Every managed variable.
    pub const ALL: [Self; 4] = [
        Self::Path,
        Self::Home,
        Self::UserProfile,
        Self::NoDefaultCurrentDirectoryInExePath,
    ];

    /// The variable's name in the environment.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Path => "PATH",
            Self::Home => HomeVariable::Home.name(),
            Self::UserProfile => HomeVariable::UserProfile.name(),
            Self::NoDefaultCurrentDirectoryInExePath => "NoDefaultCurrentDirectoryInExePath",
        }
    }
}

impl From<HomeVariable> for ManagedEnvVar {
    fn from(variable: HomeVariable) -> Self {
        match variable {
            HomeVariable::Home => Self::Home,
            HomeVariable::UserProfile => Self::UserProfile,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_names_match_the_home_variables() {
        for variable in HomeVariable::ALL {
            assert_eq!(ManagedEnvVar::from(variable).name(), variable.name());
        }
    }

    #[test]
    fn managed_names_are_distinct() {
        let mut names: Vec<_> = ManagedEnvVar::ALL.map(ManagedEnvVar::name).into();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ManagedEnvVar::ALL.len());
    }

    #[test]
    fn closures_are_parent_envs() {
        fn read(env: impl ParentEnv) -> Option<OsString> {
            env("KEY")
        }
        assert_eq!(read(|_| Some("v".into())), Some(OsString::from("v")));
        assert_eq!(read(|_| None), None);
    }
}
