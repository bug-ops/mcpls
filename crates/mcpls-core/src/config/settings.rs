//! Per-server LSP settings pushed via `workspace/didChangeConfiguration` and
//! served through `workspace/configuration`.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// Key under which the `toml` crate encodes a datetime when deserializing
/// into a self-describing format such as [`serde_json::Value`].
const TOML_DATETIME_KEY: &str = "$__toml_private_datetime";

/// Reason a `settings` table was rejected at load time.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
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
    #[error("settings value at `{path}` is a TOML datetime, which is not supported")]
    Datetime {
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct LspSettings(Map<String, Value>);

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
            reject_datetime(&key, &value)?;
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
        let Some(section) = section else {
            return self.to_value();
        };
        let mut segments = section.split('.');
        let Some(first) = segments.next() else {
            return Value::Null;
        };
        let mut current = self.0.get(first);
        for segment in segments {
            current = current.and_then(|value| value.get(segment));
        }
        current.cloned().unwrap_or(Value::Null)
    }

    /// Returns the whole settings object as a JSON value.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(self.0.clone())
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

fn reject_datetime(path: &str, value: &Value) -> Result<(), InvalidLspSettings> {
    match value {
        Value::Object(map) => {
            if map.len() == 1 && map.contains_key(TOML_DATETIME_KEY) {
                return Err(InvalidLspSettings::Datetime {
                    path: path.to_owned(),
                });
            }
            map.iter()
                .try_for_each(|(key, inner)| reject_datetime(&format!("{path}.{key}"), inner))
        }
        Value::Array(items) => items
            .iter()
            .enumerate()
            .try_for_each(|(index, inner)| reject_datetime(&format!("{path}[{index}]"), inner)),
        _ => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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
