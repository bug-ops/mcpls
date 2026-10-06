//! The LSP language identifier of a configured server.

use std::borrow::{Borrow, Cow};
use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};

/// A language identifier was empty or whitespace-only.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("language_id cannot be blank")]
pub struct InvalidLanguageId;

/// A non-empty LSP language identifier such as `"rust"` or `"typescript"`.
///
/// Deserializes from a TOML string and rejects an empty one at load time.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::LanguageId;
///
/// let id = LanguageId::new("rust").unwrap();
/// assert_eq!(id, "rust");
/// assert!(LanguageId::new("").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LanguageId(Cow<'static, str>);

impl LanguageId {
    /// Language reported for a file whose extension has no mapping.
    pub const PLAINTEXT: Self = Self::from_static("plaintext");

    /// Builds an id from a literal, checked at compile time when evaluated in a
    /// `const` context.
    ///
    /// # Panics
    ///
    /// Panics if `id` is blank or not ASCII. A literal that passes is also
    /// accepted by [`Self::new`], which additionally allows non-ASCII text.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::config::LanguageId;
    ///
    /// const RUST: LanguageId = LanguageId::from_static("rust");
    /// assert_eq!(RUST.as_str(), "rust");
    /// ```
    #[must_use]
    pub const fn from_static(id: &'static str) -> Self {
        assert!(
            id.is_ascii() && !id.trim_ascii().is_empty(),
            "language_id must be ASCII and not blank"
        );
        Self(Cow::Borrowed(id))
    }

    /// Builds an id from any string.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidLanguageId`] if `id` is blank.
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidLanguageId> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(InvalidLanguageId);
        }
        Ok(Self(Cow::Owned(id)))
    }

    /// The identifier text; never empty.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl JsonSchema for LanguageId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("LanguageId")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({"type": "string"})
    }
}

impl fmt::Display for LanguageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<String> for LanguageId {
    type Error = InvalidLanguageId;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        Self::new(id)
    }
}

impl From<LanguageId> for String {
    fn from(id: LanguageId) -> Self {
        id.0.into_owned()
    }
}

impl FromStr for LanguageId {
    type Err = InvalidLanguageId;

    fn from_str(id: &str) -> Result<Self, Self::Err> {
        Self::new(id)
    }
}

impl AsRef<str> for LanguageId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for LanguageId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for LanguageId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for LanguageId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Holder {
        id: LanguageId,
    }

    #[test]
    fn test_rejects_empty() {
        assert_eq!(LanguageId::new(""), Err(InvalidLanguageId));
        assert!("".parse::<LanguageId>().is_err());
    }

    #[test]
    #[should_panic(expected = "language_id must be ASCII and not blank")]
    fn test_from_static_panics_on_empty() {
        let _ = LanguageId::from_static("");
    }

    #[test]
    #[should_panic(expected = "language_id must be ASCII and not blank")]
    fn test_from_static_panics_on_non_ascii_blank() {
        let _ = LanguageId::from_static("\u{a0}");
    }

    #[test]
    fn test_rejects_whitespace_only() {
        assert_eq!(LanguageId::new("  \u{a0}"), Err(InvalidLanguageId));
    }

    #[test]
    fn test_round_trip_and_comparisons() {
        let holder: Holder = toml::from_str(r#"id = "python""#).unwrap();
        assert_eq!(holder.id, "python");
        assert_eq!(holder.id, *"python");
        assert_eq!(holder.id.to_string(), "python");
        assert_eq!(holder.id, LanguageId::from_static("python"));
        let text = toml::to_string(&holder).unwrap();
        assert_eq!(toml::from_str::<Holder>(&text).unwrap(), holder);
    }

    #[test]
    fn test_schema_is_an_inline_string() {
        let mut generator = SchemaGenerator::default();
        let schema = LanguageId::json_schema(&mut generator);
        assert_eq!(schema.as_value(), &serde_json::json!({"type": "string"}));
    }

    #[test]
    fn test_hash_lookup_by_str() {
        let mut map = std::collections::HashMap::new();
        map.insert(LanguageId::from_static("go"), 1);
        assert_eq!(map.get("go"), Some(&1));
    }
}
