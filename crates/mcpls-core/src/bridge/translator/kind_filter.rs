//! Typed `kind_filter` tool inputs.
//!
//! A kind filter reaches the tool boundary as text. [`KindFilterInput`] turns
//! it into a closed value there, case-insensitively, so everything past the
//! boundary holds a typed kind and sends it in its canonical spelling.
//! Deserialization never fails on the text itself: an unknown or over-long
//! spelling becomes [`KindFilterInput::Rejected`], and
//! [`KindFilterInput::into_known`] turns it into the invalid-params error, so
//! every kind filter input fails the same way, with the same casing rules and a
//! bounded message, instead of as a generic deserialization failure.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize, Serializer};

use super::addressing::MAX_SYMBOL_NAME_BYTES;
use crate::error::Error;
use crate::lsp::SUPPORTED_SYMBOL_KINDS;

pub(super) mod sealed {
    /// Keeps [`super::KindFilter`] implementable only inside this crate.
    pub trait Sealed {}
}

/// The tool input a kind filter arrives in, named in rejection messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindFilterField {
    /// `kind_filter` of `workspace_symbol_search` and `get_code_actions`.
    KindFilter,
    /// `symbol_kind` of an addressed tool.
    SymbolKind,
    /// `kind` of `get_folding_ranges`.
    Kind,
}

impl KindFilterField {
    /// The input's name on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::KindFilter => "kind_filter",
            Self::SymbolKind => "symbol_kind",
            Self::Kind => "kind",
        }
    }
}

/// A closed set of kinds a filter can name.
///
/// Sealed: only the filters of this crate implement it.
pub trait KindFilter: sealed::Sealed + Copy {
    /// The schema name of the filter type.
    const SCHEMA_NAME: &'static str;

    /// Parses `text`, ignoring ASCII case; `None` when it names no kind.
    fn parse(text: &str) -> Option<Self>;

    /// The spelling sent on to the server.
    fn canonical(self) -> Cow<'static, str>;

    /// The spellings a rejection message offers instead.
    fn valid_values() -> String;

    /// The JSON schema of the accepted spellings.
    fn schema() -> Schema;
}

/// A `kind_filter` spelling that names no known kind, kept as written for the
/// error message; at most [`MAX_SYMBOL_NAME_BYTES`] long.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedKindFilter(String);

impl RejectedKindFilter {
    /// The spelling as the client wrote it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a kind filter spelling was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The spelling is longer than [`MAX_SYMBOL_NAME_BYTES`]; it is not kept.
    TooLong {
        /// The spelling's length in bytes.
        len: usize,
    },
    /// The spelling names no kind.
    Unknown(RejectedKindFilter),
}

/// A `kind_filter` tool input: a known kind, or the reason it named none.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::{CodeActionKindFilter, KindFilterField, KindFilterInput};
///
/// let input = KindFilterInput::<CodeActionKindFilter>::from("QuickFix".to_owned());
/// assert_eq!(
///     input.into_known(KindFilterField::KindFilter).unwrap(),
///     CodeActionKindFilter::QuickFix
/// );
///
/// let bad = KindFilterInput::<CodeActionKindFilter>::from("nope".to_owned());
/// assert!(bad.into_known(KindFilterField::KindFilter).is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String", bound(deserialize = "K: KindFilter"))]
pub enum KindFilterInput<K> {
    /// The text named this kind.
    Known(K),
    /// The text was refused.
    Rejected(Rejection),
}

impl<K: KindFilter> From<String> for KindFilterInput<K> {
    fn from(text: String) -> Self {
        if text.len() > MAX_SYMBOL_NAME_BYTES {
            return Self::Rejected(Rejection::TooLong { len: text.len() });
        }
        K::parse(&text).map_or_else(
            || Self::Rejected(Rejection::Unknown(RejectedKindFilter(text))),
            Self::Known,
        )
    }
}

impl<K: Default> Default for KindFilterInput<K> {
    fn default() -> Self {
        Self::Known(K::default())
    }
}

impl<K: KindFilter> KindFilterInput<K> {
    /// The kind, or the invalid-params error for a spelling that named none.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidToolParams`] naming `field`, the spelling (unless it is
    /// over-long) and the valid kinds.
    pub fn into_known(self, field: KindFilterField) -> Result<K, Error> {
        let field = field.as_str();
        match self {
            Self::Known(kind) => Ok(kind),
            Self::Rejected(Rejection::TooLong { len }) => Err(Error::InvalidToolParams(format!(
                "`{field}` is too long: {len} bytes, at most {MAX_SYMBOL_NAME_BYTES}"
            ))),
            Self::Rejected(Rejection::Unknown(rejected)) => Err(Error::InvalidToolParams(format!(
                "Invalid {field}: '{}'. Valid values: {}",
                rejected.as_str(),
                K::valid_values()
            ))),
        }
    }
}

impl<K: KindFilter> Serialize for KindFilterInput<K> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Known(kind) => serializer.serialize_str(&kind.canonical()),
            Self::Rejected(Rejection::Unknown(rejected)) => {
                serializer.serialize_str(rejected.as_str())
            }
            Self::Rejected(Rejection::TooLong { .. }) => serializer.serialize_str(""),
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

    /// The spellings the schema documents and the rejection message lists:
    /// the canonical ones in lowercase, which parsing accepts like any case.
    fn spellings() -> Vec<String> {
        Self::ALL
            .iter()
            .map(|kind| kind.as_str().to_ascii_lowercase())
            .collect()
    }

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

impl sealed::Sealed for CodeActionKindFilter {}

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

    fn valid_values() -> String {
        format!("{:?}", Self::spellings())
    }

    fn schema() -> Schema {
        schemars::json_schema!({"type": "string", "enum": Self::spellings()})
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
    /// The names the schema documents and the rejection message lists: the
    /// supported kinds in lowercase, which parsing accepts like any case.
    fn spellings() -> Vec<&'static str> {
        SUPPORTED_SYMBOL_KINDS
            .iter()
            .map(|&(_, name)| name)
            .collect()
    }

    /// The kind this filter selects.
    #[must_use]
    pub const fn kind(self) -> lsp_types::SymbolKind {
        self.0
    }
}

impl sealed::Sealed for SymbolKindFilter {}

impl KindFilter for SymbolKindFilter {
    const SCHEMA_NAME: &'static str = "SymbolKindFilter";

    fn parse(text: &str) -> Option<Self> {
        if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
            return text
                .parse::<u32>()
                .ok()
                .map(|numeric| Self(lsp_types::SymbolKind::from(numeric)));
        }
        SUPPORTED_SYMBOL_KINDS
            .into_iter()
            .find(|&(_, name)| name.eq_ignore_ascii_case(text))
            .map(|(kind, _)| Self(kind))
    }

    fn canonical(self) -> Cow<'static, str> {
        Cow::Owned(u32::from(self.0).to_string())
    }

    fn valid_values() -> String {
        format!(
            "{:?}, or the numeric LSP SymbolKind value",
            Self::spellings()
        )
    }

    fn schema() -> Schema {
        schemars::json_schema!({
            "type": "string",
            "anyOf": [{"enum": Self::spellings()}, {"pattern": "^[0-9]+$"}],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type CodeAction = KindFilterInput<CodeActionKindFilter>;
    type Symbol = KindFilterInput<SymbolKindFilter>;

    const FIELD: KindFilterField = KindFilterField::KindFilter;

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
            let kind = CodeAction::from(written.to_owned())
                .into_known(FIELD)
                .unwrap();
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
            .into_known(FIELD)
            .unwrap_err();
        let Error::InvalidToolParams(message) = err else {
            panic!("expected InvalidToolParams, got {err:?}");
        };
        assert!(
            message.contains("Invalid kind_filter: 'bogus'"),
            "{message}"
        );
        assert!(message.contains("source.organizeimports"), "{message}");
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
            KindFilterInput::Rejected(Rejection::Unknown(RejectedKindFilter("nope".to_owned())))
        );
        assert!(serde_json::from_str::<CodeAction>("3").is_err());
    }

    #[test]
    fn test_an_over_long_spelling_is_rejected_without_being_echoed() {
        let long = "x".repeat(MAX_SYMBOL_NAME_BYTES + 1);
        for field in [
            KindFilterField::KindFilter,
            KindFilterField::SymbolKind,
            KindFilterField::Kind,
        ] {
            let err = Symbol::from(long.clone()).into_known(field).unwrap_err();
            let Error::InvalidToolParams(message) = err else {
                panic!("expected InvalidToolParams, got {err:?}");
            };
            assert!(message.contains(field.as_str()), "{message}");
            assert!(message.contains("too long"), "{message}");
            assert!(!message.contains(&long), "{message}");
        }
        let at_cap = "x".repeat(MAX_SYMBOL_NAME_BYTES);
        let message = match Symbol::from(at_cap).into_known(FIELD).unwrap_err() {
            Error::InvalidToolParams(message) => message,
            other => panic!("expected InvalidToolParams, got {other:?}"),
        };
        assert!(message.starts_with("Invalid kind_filter: '"), "{message}");
    }

    #[test]
    fn test_the_rejection_names_the_input_it_came_from() {
        let err = Symbol::from("nope".to_owned())
            .into_known(KindFilterField::SymbolKind)
            .unwrap_err();
        let Error::InvalidToolParams(message) = err else {
            panic!("expected InvalidToolParams, got {err:?}");
        };
        assert!(
            message.starts_with("Invalid symbol_kind: 'nope'"),
            "{message}"
        );
    }

    #[test]
    fn test_serialization_uses_the_canonical_spelling() {
        let known: CodeAction = serde_json::from_str("\"source.ORGANIZEIMPORTS\"").unwrap();
        assert_eq!(
            serde_json::to_string(&known).unwrap(),
            "\"source.organizeImports\""
        );
    }

    /// The names are part of the published `tools/list` schema, so they must
    /// stay what they were when they were derived from `Debug`.
    #[test]
    fn test_symbol_kind_names_are_the_lowercase_variant_names() {
        for (kind, name) in SUPPORTED_SYMBOL_KINDS {
            assert_eq!(name, format!("{kind:?}").to_ascii_lowercase());
        }
        assert!(SymbolKindFilter::spellings().contains(&"enummember"));
        assert!(SymbolKindFilter::spellings().contains(&"typeparameter"));
    }

    #[test]
    fn test_symbol_kinds_accept_names_case_insensitively_and_numbers() {
        let by_name = Symbol::from("enummember".to_owned())
            .into_known(FIELD)
            .unwrap();
        assert_eq!(by_name.kind(), lsp_types::SymbolKind::EnumMember);
        assert_eq!(by_name.canonical(), "22");
        let by_number = Symbol::from("22".to_owned()).into_known(FIELD).unwrap();
        assert_eq!(by_number, by_name);
        let custom = Symbol::from("4000".to_owned()).into_known(FIELD).unwrap();
        assert_eq!(custom.canonical(), "4000");
    }

    /// The schema's `^[0-9]+$` is the whole numeric form: no sign, no padding.
    #[test]
    fn test_only_plain_digits_are_a_numeric_symbol_kind() {
        for rejected in ["+5", "-5", " 5", "5 ", "", "4294967296"] {
            assert!(
                Symbol::from(rejected.to_owned()).into_known(FIELD).is_err(),
                "{rejected:?}"
            );
        }
        assert!(Symbol::from("007".to_owned()).into_known(FIELD).is_ok());
    }

    #[test]
    fn test_an_unknown_symbol_kind_is_invalid_params() {
        let err = Symbol::from("NotAKind".to_owned())
            .into_known(FIELD)
            .unwrap_err();
        let Error::InvalidToolParams(message) = err else {
            panic!("expected InvalidToolParams, got {err:?}");
        };
        assert!(message.contains("'NotAKind'"), "{message}");
        assert!(message.contains("numeric LSP SymbolKind"), "{message}");
    }

    /// A client validating against the schema must accept every spelling the
    /// rejection message recommends.
    #[test]
    fn test_rejection_messages_list_exactly_the_schema_spellings() {
        let code_actions = CodeActionKindFilter::schema();
        let listed = code_actions.as_value()["enum"].clone();
        let message = CodeActionKindFilter::valid_values();
        assert_eq!(message, debug_list(&listed));

        let symbols = SymbolKindFilter::schema();
        let listed = symbols.as_value()["anyOf"][0]["enum"].clone();
        let message = SymbolKindFilter::valid_values();
        assert!(message.starts_with(&debug_list(&listed)), "{message}");
    }

    fn debug_list(values: &serde_json::Value) -> String {
        let names: Vec<&str> = values
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        format!("{names:?}")
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
