//! The LSP language identifier of a configured server.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};

use super::text_newtype::impl_text_newtype;

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

impl_text_newtype!(LanguageId, InvalidLanguageId, non_blank, "language_id");

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
