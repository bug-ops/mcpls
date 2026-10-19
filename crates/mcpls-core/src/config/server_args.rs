//! The command-line arguments of a configured server.

use std::collections::BTreeMap;
use std::ops::Deref;

use serde::{Deserialize, Serialize};

use super::server::ServerCommand;
use super::text_newtype::contains_nul;

/// An argument holds a NUL, which the operating system cannot pass.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("args[{index}] cannot contain NUL")]
pub struct InvalidServerArgs {
    /// The position of the first argument that holds a NUL.
    pub index: usize,
}

/// The arguments passed to a server's command: any text, none of it holding a
/// NUL, so a spawn cannot fail on one.
///
/// Reads as the `[String]` it wraps. Built through [`Self::new`] or
/// `TryFrom<Vec<String>>`, which refuse a NUL, and by deserializing.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ServerArgs;
///
/// let args = ServerArgs::new(["--stdio", "--log"])?;
/// assert_eq!(args.len(), 2);
/// assert_eq!(args[0], "--stdio");
/// assert_eq!(ServerArgs::new(["ok", "a\0b"]).unwrap_err().index, 1);
/// # Ok::<(), mcpls_core::config::InvalidServerArgs>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct ServerArgs(Vec<String>);

impl ServerArgs {
    /// Validates `args`.
    ///
    /// # Errors
    ///
    /// [`InvalidServerArgs`] naming the first argument that holds a NUL.
    pub fn new<S: Into<String>>(
        args: impl IntoIterator<Item = S>,
    ) -> Result<Self, InvalidServerArgs> {
        Self::try_from(args.into_iter().map(Into::into).collect::<Vec<String>>())
    }
}

impl ServerArgs {
    /// Arguments written as literals in the source.
    ///
    /// # Panics
    ///
    /// Panics if a literal holds a NUL, which a source literal never should.
    #[must_use]
    pub fn from_static(args: &'static [&'static str]) -> Self {
        assert!(
            !args.iter().any(|arg| contains_nul(arg)),
            "a literal argument must not hold a NUL"
        );
        Self(args.iter().map(ToString::to_string).collect())
    }

    /// These arguments preceded by `program`, which holds no NUL by its type.
    pub(crate) fn with_leading(mut self, program: ServerCommand) -> Self {
        self.0.insert(0, String::from(program));
        self
    }

    /// These arguments with the one at each key of `programs` replaced by the
    /// program, which holds no NUL by its type. Programs are found by index,
    /// so no ordering of the analysis can make a replacement be skipped.
    pub(crate) fn with_programs(self, mut programs: BTreeMap<usize, ServerCommand>) -> Self {
        Self(
            self.0
                .into_iter()
                .enumerate()
                .map(|(index, arg)| programs.remove(&index).map_or(arg, String::from))
                .collect(),
        )
    }
}

impl TryFrom<Vec<String>> for ServerArgs {
    type Error = InvalidServerArgs;

    fn try_from(args: Vec<String>) -> Result<Self, Self::Error> {
        if let Some(index) = args.iter().position(|arg| contains_nul(arg)) {
            return Err(InvalidServerArgs { index });
        }
        Ok(Self(args))
    }
}

impl From<ServerArgs> for Vec<String> {
    fn from(args: ServerArgs) -> Self {
        args.0
    }
}

impl Deref for ServerArgs {
    type Target = [String];

    fn deref(&self) -> &[String] {
        &self.0
    }
}

impl<S: AsRef<str>> PartialEq<Vec<S>> for ServerArgs {
    fn eq(&self, other: &Vec<S>) -> bool {
        self.0
            .iter()
            .map(String::as_str)
            .eq(other.iter().map(AsRef::as_ref))
    }
}

impl<S: AsRef<str>, const N: usize> PartialEq<[S; N]> for ServerArgs {
    fn eq(&self, other: &[S; N]) -> bool {
        self.0
            .iter()
            .map(String::as_str)
            .eq(other.iter().map(AsRef::as_ref))
    }
}

impl<'a> IntoIterator for &'a ServerArgs {
    type Item = &'a String;
    type IntoIter = std::slice::Iter<'a, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nul_names_the_first_offending_argument() {
        assert_eq!(
            ServerArgs::new(["a", "b\0", "c\0"]),
            Err(InvalidServerArgs { index: 1 })
        );
        assert_eq!(ServerArgs::new(["a", ""]).unwrap().len(), 2);
    }

    #[test]
    fn deserializing_checks_the_arguments() {
        let ok: ServerArgs = serde_json::from_str(r#"["--x"]"#).unwrap();
        assert_eq!(ok[0], "--x");
        assert!(serde_json::from_str::<ServerArgs>(r#"["a\u0000"]"#).is_err());
    }
}
