//! Per-server LSP settings pushed via `workspace/didChangeConfiguration` and
//! served through `workspace/configuration`.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// Key under which the `toml` crate encodes a datetime when deserializing
/// into a self-describing format such as [`serde_json::Value`].
const TOML_DATETIME_KEY: &str = "$__toml_private_datetime";

/// Server-specific `initialize` options, passed to the server as written.
///
/// Its `Debug` prints structure only, because option values may carry
/// credentials; a struct holding it can derive `Debug` safely.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::InitializationOptions;
///
/// let options = InitializationOptions::from(serde_json::json!({"token": "hunter2"}));
/// assert!(!format!("{options:?}").contains("hunter2"));
/// assert_eq!(options.as_value()["token"], "hunter2");
/// ```
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct InitializationOptions(Value);

impl<'de> Deserialize<'de> for InitializationOptions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        find_toml_datetime(None, &value).map_or_else(
            || Ok(Self(value)),
            |datetime| {
                Err(serde::de::Error::custom(format_args!(
                    "initialization_options {datetime}"
                )))
            },
        )
    }
}

impl std::fmt::Debug for InitializationOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("InitializationOptions")
            .field(&crate::redaction::JsonShape(&self.0))
            .finish()
    }
}

impl From<Value> for InitializationOptions {
    fn from(value: Value) -> Self {
        Self(value)
    }
}

impl InitializationOptions {
    /// The options as a JSON value.
    #[must_use]
    pub const fn as_value(&self) -> &Value {
        &self.0
    }

    /// Consumes the options into the JSON value sent to the server.
    #[must_use]
    pub fn into_value(self) -> Value {
        self.0
    }
}

/// Reason a `settings` table was rejected at load time.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidLspSettings {
    /// The table is empty; pushing `{}` makes some servers drop their
    /// initialization options.
    #[error("settings must not be empty")]
    Empty,

    /// A top-level dotted key collides with another setting.
    #[error("settings key `{path}` conflicts with another setting")]
    Conflict {
        /// Dotted path of the colliding setting.
        path: String,
    },

    /// A top-level key has an empty segment, such as `a..b` or `.a`.
    #[error("settings key `{key}` has an empty segment")]
    EmptySegment {
        /// The offending top-level key.
        key: String,
    },

    /// A TOML datetime appears in the settings, which JSON cannot represent.
    #[error("settings {0}")]
    Datetime(TomlDatetime),
}

/// Location of a TOML datetime found in a free-form table, which JSON cannot
/// represent.
///
/// Shared by `settings` and `initialization_options` so both reject a
/// datetime with the same wording.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TomlDatetime {
    /// The whole value is a datetime.
    #[error("value is a TOML datetime, which is not supported")]
    Root,

    /// A nested value is a datetime.
    #[error("value at `{path}` is a TOML datetime, which is not supported")]
    At {
        /// Dotted path of the datetime value.
        path: String,
    },
}

/// Non-empty per-server settings object.
///
/// Top-level dotted keys follow VS Code semantics and are expanded into
/// nested objects at load: `"python.analysis.typeCheckingMode" = "strict"`
/// becomes `{"python": {"analysis": {"typeCheckingMode": "strict"}}}`.
/// Keys inside a setting's value are never touched, so gopls flat keys
/// (`{"gopls": {"ui.semanticTokens": true}}`) and yaml-language-server URL
/// keys (`"yaml.schemas" = { "https://x/y.json" = "*.yml" }`) survive intact.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::LspSettings;
///
/// let settings: LspSettings =
///     serde_json::from_str(r#"{"python.analysis.typeCheckingMode": "strict"}"#).unwrap();
/// assert_eq!(
///     settings.section(Some("python.analysis")),
///     serde_json::json!({"typeCheckingMode": "strict"})
/// );
/// ```
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct LspSettings(Map<String, Value>);

/// Prints keys and structure only: setting values may carry credentials.
impl std::fmt::Debug for LspSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LspSettings")
            .field(&crate::redaction::JsonShape(&Value::Object(self.0.clone())))
            .finish()
    }
}

impl LspSettings {
    /// Builds settings from a raw object, expanding top-level dotted keys.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidLspSettings`] when the object is empty, contains a
    /// TOML datetime, or has colliding or malformed dotted keys.
    pub fn new(raw: Map<String, Value>) -> Result<Self, InvalidLspSettings> {
        if raw.is_empty() {
            return Err(InvalidLspSettings::Empty);
        }
        let mut plain = Map::new();
        let mut dotted = Vec::new();
        for (key, value) in raw {
            if let Some(datetime) = find_toml_datetime(Some(&key), &value) {
                return Err(InvalidLspSettings::Datetime(datetime));
            }
            let segments: Vec<String> = key.split('.').map(str::to_owned).collect();
            if segments.iter().any(String::is_empty) {
                return Err(InvalidLspSettings::EmptySegment { key });
            }
            if segments.len() > 1 {
                dotted.push((segments, value));
            } else {
                plain.insert(key, value);
            }
        }
        dotted.sort_by_key(|(segments, _)| segments.len());
        for (segments, value) in dotted {
            insert_at(&mut plain, &segments, value)?;
        }
        Ok(Self(plain))
    }

    /// Returns the value for a dotted configuration section, or the whole
    /// object when `section` is `None`. Missing sections yield `Null`.
    #[must_use]
    pub fn section(&self, section: Option<&str>) -> Value {
        self.section_ref(section).into()
    }

    /// Borrows the section without cloning it, so a caller can bound the
    /// reply size before paying for a copy.
    pub(crate) fn section_ref(&self, section: Option<&str>) -> SectionRef<'_> {
        let Some(section) = section else {
            return SectionRef::Whole(&self.0);
        };
        let mut segments = section.split('.');
        let Some(first) = segments.next() else {
            return SectionRef::Missing;
        };
        let mut current = self.0.get(first);
        for segment in segments {
            current = current.and_then(|value| value.get(segment));
        }
        current.map_or(SectionRef::Missing, SectionRef::Found)
    }

    /// The section of an item that selects nothing; serializes as `null`.
    pub(crate) const fn missing_section() -> SectionRef<'static> {
        SectionRef::Missing
    }

    /// Returns the whole settings object as a JSON value.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(self.0.clone())
    }
}

/// A borrowed configuration section: the whole settings object, a subtree,
/// or nothing (serialized as `null`).
#[derive(Debug, Clone, Copy)]
pub enum SectionRef<'a> {
    Whole(&'a Map<String, Value>),
    Found(&'a Value),
    Missing,
}

/// Deep-clones the section into an owned JSON value.
impl From<SectionRef<'_>> for Value {
    fn from(section: SectionRef<'_>) -> Self {
        match section {
            SectionRef::Whole(map) => Self::Object(map.clone()),
            SectionRef::Found(value) => value.clone(),
            SectionRef::Missing => Self::Null,
        }
    }
}

impl SectionRef<'_> {
    /// The serialized JSON length, measured without allocating the text.
    pub(crate) fn serialized_len(self) -> usize {
        struct Counter(usize);
        impl std::io::Write for Counter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(buf.len());
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut counter = Counter(0);
        let result = match self {
            Self::Whole(map) => serde_json::to_writer(&mut counter, map),
            Self::Found(value) => serde_json::to_writer(&mut counter, value),
            Self::Missing => serde_json::to_writer(&mut counter, &Value::Null),
        };
        // Serializing a `Value` into an infallible writer cannot fail.
        debug_assert!(result.is_ok());
        counter.0
    }
}

impl<'de> Deserialize<'de> for LspSettings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Map::<String, Value>::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

fn insert_at(
    root: &mut Map<String, Value>,
    segments: &[String],
    value: Value,
) -> Result<(), InvalidLspSettings> {
    let conflict = || InvalidLspSettings::Conflict {
        path: segments.join("."),
    };
    let Some((last, parents)) = segments.split_last() else {
        return Err(conflict());
    };
    let mut current = root;
    for segment in parents {
        let entry = current
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        current = entry.as_object_mut().ok_or_else(conflict)?;
    }
    if current.contains_key(last) {
        return Err(conflict());
    }
    current.insert(last.clone(), value);
    Ok(())
}

/// Finds the first TOML datetime under `value`; `path` is `None` at the root.
fn find_toml_datetime(path: Option<&str>, value: &Value) -> Option<TomlDatetime> {
    match value {
        Value::Object(map) => {
            if map.len() == 1 && map.contains_key(TOML_DATETIME_KEY) {
                return Some(path.map_or(TomlDatetime::Root, |path| TomlDatetime::At {
                    path: path.to_owned(),
                }));
            }
            map.iter().find_map(|(key, inner)| {
                let child = path.map_or_else(|| key.clone(), |parent| format!("{parent}.{key}"));
                find_toml_datetime(Some(&child), inner)
            })
        }
        Value::Array(items) => items.iter().enumerate().find_map(|(index, inner)| {
            let child = format!("{}[{index}]", path.unwrap_or_default());
            find_toml_datetime(Some(&child), inner)
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse(json: &str) -> Result<LspSettings, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn expands_top_level_dotted_keys() {
        let settings = parse(r#"{"python.analysis.typeCheckingMode": "strict"}"#).unwrap();
        assert_eq!(
            settings.to_value(),
            json!({"python": {"analysis": {"typeCheckingMode": "strict"}}})
        );
    }

    #[test]
    fn merges_sibling_keys_into_existing_object() {
        let settings = parse(r#"{"python": {"a": 1}, "python.b": 2}"#).unwrap();
        assert_eq!(settings.to_value(), json!({"python": {"a": 1, "b": 2}}));
    }

    #[test]
    fn gopls_flat_keys_round_trip_byte_identical() {
        let input = r#"{"gopls":{"ui.semanticTokens":true}}"#;
        let settings = parse(input).unwrap();
        assert_eq!(serde_json::to_string(&settings).unwrap(), input);
    }

    #[test]
    fn yaml_schemas_url_key_preserved_after_expansion() {
        let settings =
            parse(r#"{"yaml.schemas": {"https://json.schemastore.org/x.json": "*.yml"}}"#).unwrap();
        assert_eq!(
            settings.section(Some("yaml.schemas")),
            json!({"https://json.schemastore.org/x.json": "*.yml"})
        );
        assert_eq!(
            settings.to_value(),
            json!({"yaml": {"schemas": {"https://json.schemastore.org/x.json": "*.yml"}}})
        );
    }

    #[test]
    fn rejects_conflict_between_dotted_and_nested() {
        let err = parse(r#"{"python.analysis.x": 1, "python": {"analysis": {"x": 2}}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("python.analysis.x"), "{err}");
    }

    #[test]
    fn rejects_non_object_intermediate() {
        let raw = json!({"a": 1, "a.b": 2});
        let Value::Object(raw) = raw else {
            unreachable!()
        };
        assert_eq!(
            LspSettings::new(raw),
            Err(InvalidLspSettings::Conflict { path: "a.b".into() })
        );
    }

    #[test]
    fn rejects_empty_object() {
        let err = parse("{}").unwrap_err().to_string();
        assert!(err.contains("must not be empty"), "{err}");
    }

    #[test]
    fn rejects_empty_segment() {
        let err = parse(r#"{"a..b": 1}"#).unwrap_err().to_string();
        assert!(err.contains("empty segment"), "{err}");
    }

    #[test]
    fn rejects_empty_top_level_key() {
        let err = parse(r#"{"": 1}"#).unwrap_err().to_string();
        assert!(err.contains("empty segment"), "{err}");
    }

    #[test]
    fn rejects_toml_datetime_anywhere() {
        type Wrapper = std::collections::HashMap<String, LspSettings>;
        let top = toml::from_str::<Wrapper>("[settings]\nwhen = 1979-05-27T07:32:00Z\n");
        assert!(top.unwrap_err().to_string().contains("datetime"));
        let nested = toml::from_str::<Wrapper>("[settings.a]\nlist = [1979-05-27]\n");
        assert!(nested.unwrap_err().to_string().contains("datetime"));
    }

    #[test]
    fn settings_datetime_message_is_unchanged() {
        let err = parse(r#"{"when": {"$__toml_private_datetime": "1979-05-27"}}"#)
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("settings value at `when` is a TOML datetime, which is not supported"),
            "{err}"
        );
        let nested = parse(r#"{"a": {"b": [1, {"$__toml_private_datetime": "1979-05-27"}]}}"#)
            .unwrap_err()
            .to_string();
        assert!(nested.contains("value at `a.b[1]` is"), "{nested}");
    }

    #[test]
    fn initialization_options_reject_datetime_with_position() {
        type Wrapper = std::collections::HashMap<String, InitializationOptions>;
        let err = toml::from_str::<Wrapper>(
            "[initialization_options.nested]\nwhen = 1979-05-27T07:32:00Z\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(
                "initialization_options value at `nested.when` is a TOML datetime, which is not supported"
            ),
            "{err}"
        );
        assert!(err.contains("line 1, column 2"), "{err}");
    }

    #[test]
    fn initialization_options_accept_plain_values() {
        let options: InitializationOptions =
            serde_json::from_str(r#"{"a": [1, {"b": "2020-01-01"}]}"#).unwrap();
        assert_eq!(options.as_value(), &json!({"a": [1, {"b": "2020-01-01"}]}));
    }

    #[test]
    fn root_datetime_has_no_path() {
        let value = json!({TOML_DATETIME_KEY: "1979-05-27"});
        assert_eq!(find_toml_datetime(None, &value), Some(TomlDatetime::Root));
        assert_eq!(
            TomlDatetime::Root.to_string(),
            "value is a TOML datetime, which is not supported"
        );
    }

    #[test]
    fn parses_toml_table_with_quoted_dotted_key() {
        #[derive(Deserialize)]
        struct Wrapper {
            settings: LspSettings,
        }
        let parsed: Wrapper =
            toml::from_str("[settings]\n\"python.analysis.mode\" = \"strict\"\n").unwrap();
        assert_eq!(
            parsed.settings.section(Some("python.analysis.mode")),
            json!("strict")
        );
    }

    #[test]
    fn section_walks_dotted_paths() {
        let settings = parse(r#"{"a": {"b": {"c": 1}}}"#).unwrap();
        assert_eq!(settings.section(Some("a.b")), json!({"c": 1}));
        assert_eq!(settings.section(Some("a.x")), Value::Null);
        assert_eq!(settings.section(Some("z")), Value::Null);
        assert_eq!(settings.section(None), settings.to_value());
    }
}
