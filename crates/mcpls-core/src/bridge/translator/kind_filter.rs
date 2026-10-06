//! Typed `kind_filter` tool inputs.
//!
//! A kind filter reaches the tool boundary as text. [`KindFilterInput`] turns
//! it into a closed value there, case-insensitively, so everything past the
//! boundary holds a typed kind and sends it in its canonical spelling.
//! Deserialization never fails on the text itself: an unknown spelling becomes
//! [`KindFilterInput::Rejected`], and [`KindFilterInput::into_known`] turns it
//! into the invalid-params error, so a bad filter keeps the error class it
//! always had instead of becoming a generic deserialization failure.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize, Serializer};

use crate::error::Error;
use crate::lsp::SUPPORTED_SYMBOL_KINDS;

/// A closed set of kinds a filter can name.
pub trait KindFilter: Copy {
    /// The schema name of the filter type.
    const SCHEMA_NAME: &'static str;

    /// Parses `text`, ignoring ASCII case; `None` when it names no kind.
    fn parse(text: &str) -> Option<Self>;

    /// The spelling sent on to the server.
    fn canonical(self) -> Cow<'static, str>;

    /// The message of the invalid-params error for a spelling that names no
    /// kind.
    fn rejection_message(rejected: &str) -> String;

    /// The JSON schema of the accepted spellings.
    fn schema() -> Schema;
}

/// A `kind_filter` spelling that names no known kind, kept as written for the
/// error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedKindFilter(String);

impl RejectedKindFilter {
    /// The spelling as the client wrote it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A `kind_filter` tool input: a known kind, or the spelling that named none.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::{CodeActionKindFilter, KindFilterInput};
///
/// let input = KindFilterInput::<CodeActionKindFilter>::from("QuickFix".to_owned());
/// assert_eq!(input.into_known().unwrap(), CodeActionKindFilter::QuickFix);
///
/// let bad = KindFilterInput::<CodeActionKindFilter>::from("nope".to_owned());
/// assert!(bad.into_known().is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String", bound(deserialize = "K: KindFilter"))]
pub enum KindFilterInput<K> {
    /// The text named this kind.
    Known(K),
    /// The text named no kind.
    Rejected(RejectedKindFilter),
}

impl<K: KindFilter> From<String> for KindFilterInput<K> {
    fn from(text: String) -> Self {
        K::parse(&text).map_or_else(|| Self::Rejected(RejectedKindFilter(text)), Self::Known)
    }
}

impl<K: KindFilter> KindFilterInput<K> {
    /// The kind, or the invalid-params error for a spelling that named none.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidToolParams`] naming the spelling and the valid kinds.
    pub fn into_known(self) -> Result<K, Error> {
        match self {
            Self::Known(kind) => Ok(kind),
            Self::Rejected(rejected) => Err(Error::InvalidToolParams(K::rejection_message(
                rejected.as_str(),
            ))),
        }
    }
}

impl<K: KindFilter> Serialize for KindFilterInput<K> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Known(kind) => serializer.serialize_str(&kind.canonical()),
            Self::Rejected(rejected) => serializer.serialize_str(rejected.as_str()),
        }
    }
}

impl<K: KindFilter> JsonSchema for KindFilterInput<K> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed(K::SCHEMA_NAME)
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        K::schema()
    }
}

/// A code action kind a `get_code_actions` filter can name.
///
/// Accepted in any case and sent to the server in its canonical LSP spelling,
/// which is case-sensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeActionKindFilter {
    /// `quickfix`.
    QuickFix,
    /// `refactor`.
    Refactor,
    /// `refactor.extract`.
    RefactorExtract,
    /// `refactor.inline`.
    RefactorInline,
    /// `refactor.rewrite`.
    RefactorRewrite,
    /// `source`.
    Source,
    /// `source.organizeImports`.
    SourceOrganizeImports,
}

impl CodeActionKindFilter {
    /// Every kind, in documentation order.
    pub const ALL: [Self; 7] = [
        Self::QuickFix,
        Self::Refactor,
        Self::RefactorExtract,
        Self::RefactorInline,
        Self::RefactorRewrite,
        Self::Source,
        Self::SourceOrganizeImports,
    ];

    /// The canonical LSP spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::QuickFix => "quickfix",
            Self::Refactor => "refactor",
            Self::RefactorExtract => "refactor.extract",
            Self::RefactorInline => "refactor.inline",
            Self::RefactorRewrite => "refactor.rewrite",
            Self::Source => "source",
            Self::SourceOrganizeImports => "source.organizeImports",
        }
    }
}

impl KindFilter for CodeActionKindFilter {
    const SCHEMA_NAME: &'static str = "CodeActionKindFilter";

    fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str().eq_ignore_ascii_case(text))
    }

    fn canonical(self) -> Cow<'static, str> {
        Cow::Borrowed(self.as_str())
    }

    fn rejection_message(rejected: &str) -> String {
        let valid: Vec<&str> = Self::ALL.iter().map(|kind| kind.as_str()).collect();
        format!("Invalid kind_filter: '{rejected}'. Valid values: {valid:?}")
    }

    fn schema() -> Schema {
        let values: Vec<String> = Self::ALL
            .iter()
            .map(|kind| kind.as_str().to_ascii_lowercase())
            .collect();
        schemars::json_schema!({"type": "string", "enum": values})
    }
}

/// A symbol kind a `workspace_symbol_search` filter can name, by name or by its
/// numeric LSP value.
///
/// A name is validated against the kinds mcpls advertises; a number is taken as
/// it is, because a server may use custom kinds. The kind is sent on as its
/// numeric value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolKindFilter(lsp_types::SymbolKind);

impl SymbolKindFilter {
    /// The kind this filter selects.
    #[must_use]
    pub const fn kind(self) -> lsp_types::SymbolKind {
        self.0
    }
}

impl KindFilter for SymbolKindFilter {
    const SCHEMA_NAME: &'static str = "SymbolKindFilter";

    fn parse(text: &str) -> Option<Self> {
        if let Ok(numeric) = text.parse::<u32>() {
            return Some(Self(lsp_types::SymbolKind::from(numeric)));
        }
        SUPPORTED_SYMBOL_KINDS
            .into_iter()
            .find(|kind| format!("{kind:?}").eq_ignore_ascii_case(text))
            .map(Self)
    }

    fn canonical(self) -> Cow<'static, str> {
        Cow::Owned(u32::from(self.0).to_string())
    }

    fn rejection_message(rejected: &str) -> String {
        let valid: Vec<String> = SUPPORTED_SYMBOL_KINDS
            .iter()
            .map(|kind| format!("{kind:?}"))
            .collect();
        format!(
            "Invalid kind_filter: '{rejected}'. Valid values: {valid:?}, or the numeric LSP \
             SymbolKind value"
        )
    }

    fn schema() -> Schema {
        let names: Vec<String> = SUPPORTED_SYMBOL_KINDS
            .iter()
            .map(|kind| format!("{kind:?}").to_ascii_lowercase())
            .collect();
        schemars::json_schema!({
            "type": "string",
            "anyOf": [{"enum": names}, {"pattern": "^[0-9]+$"}],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type CodeAction = KindFilterInput<CodeActionKindFilter>;
    type Symbol = KindFilterInput<SymbolKindFilter>;

    #[test]
    fn test_code_action_kinds_parse_in_any_case_and_send_canonically() {
        for (written, expected) in [
            ("QuickFix", CodeActionKindFilter::QuickFix),
            ("QUICKFIX", CodeActionKindFilter::QuickFix),
            (
                "source.organizeimports",
                CodeActionKindFilter::SourceOrganizeImports,
            ),
            (
                "Source.OrganizeImports",
                CodeActionKindFilter::SourceOrganizeImports,
            ),
            ("refactor.EXTRACT", CodeActionKindFilter::RefactorExtract),
        ] {
            let kind = CodeAction::from(written.to_owned()).into_known().unwrap();
            assert_eq!(kind, expected, "{written}");
        }
        assert_eq!(
            CodeActionKindFilter::SourceOrganizeImports.canonical(),
            "source.organizeImports"
        );
    }

    #[test]
    fn test_an_unknown_code_action_kind_is_invalid_params_naming_the_valid_ones() {
        let err = CodeAction::from("bogus".to_owned())
            .into_known()
            .unwrap_err();
        let Error::InvalidToolParams(message) = err else {
            panic!("expected InvalidToolParams, got {err:?}");
        };
        assert!(
            message.contains("Invalid kind_filter: 'bogus'"),
            "{message}"
        );
        assert!(message.contains("source.organizeImports"), "{message}");
    }

    #[test]
    fn test_deserializing_any_string_never_fails() {
        let known: CodeAction = serde_json::from_str("\"Refactor\"").unwrap();
        assert_eq!(
            known,
            KindFilterInput::Known(CodeActionKindFilter::Refactor)
        );
        let rejected: CodeAction = serde_json::from_str("\"nope\"").unwrap();
        assert_eq!(
            rejected,
            KindFilterInput::Rejected(RejectedKindFilter("nope".to_owned()))
        );
        assert!(serde_json::from_str::<CodeAction>("3").is_err());
    }

    #[test]
    fn test_serialization_uses_the_canonical_spelling() {
        let known: CodeAction = serde_json::from_str("\"source.ORGANIZEIMPORTS\"").unwrap();
        assert_eq!(
            serde_json::to_string(&known).unwrap(),
            "\"source.organizeImports\""
        );
    }

    #[test]
    fn test_symbol_kinds_accept_names_case_insensitively_and_numbers() {
        let by_name = Symbol::from("enummember".to_owned()).into_known().unwrap();
        assert_eq!(by_name.kind(), lsp_types::SymbolKind::EnumMember);
        assert_eq!(by_name.canonical(), "22");
        let by_number = Symbol::from("22".to_owned()).into_known().unwrap();
        assert_eq!(by_number, by_name);
        let custom = Symbol::from("4000".to_owned()).into_known().unwrap();
        assert_eq!(custom.canonical(), "4000");
    }

    #[test]
    fn test_an_unknown_symbol_kind_is_invalid_params() {
        let err = Symbol::from("NotAKind".to_owned())
            .into_known()
            .unwrap_err();
        let Error::InvalidToolParams(message) = err else {
            panic!("expected InvalidToolParams, got {err:?}");
        };
        assert!(message.contains("'NotAKind'"), "{message}");
        assert!(message.contains("numeric LSP SymbolKind"), "{message}");
    }

    #[test]
    fn test_schemas_list_lowercase_spellings() {
        let code_actions = CodeActionKindFilter::schema();
        let value = code_actions.as_value();
        assert_eq!(value["type"], "string");
        let listed: Vec<&str> = value["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            listed,
            [
                "quickfix",
                "refactor",
                "refactor.extract",
                "refactor.inline",
                "refactor.rewrite",
                "source",
                "source.organizeimports",
            ]
        );
        let symbols = SymbolKindFilter::schema();
        let any_of = symbols.as_value()["anyOf"].as_array().unwrap().clone();
        assert_eq!(any_of.len(), 2);
        assert!(
            any_of[0]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "function")
        );
        assert_eq!(any_of[1]["pattern"], "^[0-9]+$");
    }
}
