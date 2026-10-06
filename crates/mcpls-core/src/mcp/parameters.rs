//! Tool-argument extractor that bounds the client text echoed in errors.
//!
//! rmcp's own `Parameters` formats serde's message verbatim, so a client key or
//! string value of any length is reflected in the error. [`Parameters`] keeps
//! rmcp's schema and error contract but bounds that echo.

use std::borrow::Cow;
use std::fmt::{self, Write as _};

use rmcp::ErrorData as McpError;
use rmcp::handler::server::common::FromContextPart;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::JsonObject;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::bridge::MAX_SYMBOL_NAME_BYTES;
use crate::lsp::MAX_ERROR_MESSAGE_CALLER_BYTES;
use crate::util::truncate_string;

/// Prefix rmcp's router maps to an `isError` tool result instead of a
/// protocol error.
const DESERIALIZE_ERROR_PREFIX: &str = "failed to deserialize parameters:";

/// Deserialized tool arguments, with client text in the error bounded.
///
/// Drop-in for `rmcp::handler::server::wrapper::Parameters`: the macro finds
/// the input schema by this type's last path segment, and [`JsonSchema`]
/// delegates to `P`, so every tool schema is unchanged. On a malformed call
/// the error keeps rmcp's `failed to deserialize parameters:` prefix (so the
/// call is an `isError` result) but object keys longer than
/// `MAX_SYMBOL_NAME_BYTES` are replaced by their length and the whole message
/// is capped at `MAX_ERROR_MESSAGE_CALLER_BYTES`.
#[derive(Debug)]
pub struct Parameters<P>(pub P);

impl<P: JsonSchema> JsonSchema for Parameters<P> {
    fn schema_name() -> Cow<'static, str> {
        P::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        P::json_schema(generator)
    }
}

impl<S, P> FromContextPart<ToolCallContext<'_, S>> for Parameters<P>
where
    P: DeserializeOwned,
{
    fn from_context_part(context: &mut ToolCallContext<'_, S>) -> Result<Self, McpError> {
        parse_arguments(context.arguments.take()).map(Self)
    }
}

fn parse_arguments<P: DeserializeOwned>(arguments: Option<JsonObject>) -> Result<P, McpError> {
    let mut value = Value::Object(arguments.unwrap_or_default());
    let original = match P::deserialize(&value) {
        Ok(parsed) => return Ok(parsed),
        Err(error) => error,
    };
    // A rename must never turn a failed call into a successful one.
    let error = if bound_keys(&mut value) {
        P::deserialize(&value).err().unwrap_or(original)
    } else {
        original
    };
    Err(McpError::invalid_params(rejection_message(&error), None))
}

/// Formats the rejection without ever materializing more than the cap, since
/// serde echoes client string values of any size.
fn rejection_message(error: &serde_json::Error) -> String {
    let mut out = CappedWriter(String::new());
    // Overflow is expected and handled by `truncate_string` below.
    write!(out, "{DESERIALIZE_ERROR_PREFIX} {error}").unwrap_or_default();
    truncate_string(out.0, MAX_ERROR_MESSAGE_CALLER_BYTES)
}

/// String sink that stops accepting text a few bytes past the message cap, so
/// `truncate_string` still sees an over-long message and adds its marker.
struct CappedWriter(String);

impl CappedWriter {
    const LIMIT: usize = MAX_ERROR_MESSAGE_CALLER_BYTES + char::MAX_LEN_UTF8;
}

impl fmt::Write for CappedWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let room = Self::LIMIT.saturating_sub(self.0.len());
        if text.len() <= room {
            self.0.push_str(text);
            return Ok(());
        }
        self.0.push_str(&text[..text.floor_char_boundary(room)]);
        Err(fmt::Error)
    }
}

/// Replaces every object key longer than `MAX_SYMBOL_NAME_BYTES`, at any
/// depth, by `<N-byte name>` (`<N-byte name #k>` for the k-th key of one
/// object that would otherwise collide). Returns whether any key was renamed;
/// an object without a long key is not rebuilt.
fn bound_keys(value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            let mut changed = map
                .values_mut()
                .fold(false, |acc, child| bound_keys(child) | acc);
            if map.keys().any(|key| key.len() > MAX_SYMBOL_NAME_BYTES) {
                changed = true;
                let entries = std::mem::take(map);
                for (key, child) in entries {
                    let key = if key.len() > MAX_SYMBOL_NAME_BYTES {
                        unused_name(map, key.len())
                    } else {
                        key
                    };
                    map.insert(key, child);
                }
            }
            changed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |acc, item| bound_keys(item) | acc),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

fn unused_name(map: &JsonObject, len: usize) -> String {
    let mut attempt = 1_usize;
    loop {
        let name = if attempt == 1 {
            format!("<{len}-byte name>")
        } else {
            format!("<{len}-byte name #{attempt}>")
        };
        if !map.contains_key(&name) {
            return name;
        }
        attempt = attempt.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::mcp::tools::PositionParams;

    fn object(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn parse_error(value: Value) -> String {
        let error = parse_arguments::<PositionParams>(Some(object(value)))
            .expect_err("arguments must be rejected");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        error.message.into_owned()
    }

    #[test]
    fn test_valid_arguments_parse() {
        let parsed = parse_arguments::<PositionParams>(Some(object(
            json!({"file_path": "/a.rs", "line": 1, "character": 2}),
        )))
        .unwrap();
        assert_eq!(parsed.character, 2);
    }

    #[test]
    fn test_error_keeps_the_rmcp_prefix() {
        let message = parse_error(json!({"file_path": "/a.rs", "line": 1, "character": 2, "x": 1}));
        assert!(message.starts_with(DESERIALIZE_ERROR_PREFIX), "{message}");
        assert!(message.contains("`x`"), "{message}");
        assert!(message.contains("`file_path`"), "{message}");
    }

    #[test]
    fn test_long_unknown_key_is_replaced_by_its_length() {
        let long = "k".repeat(2048);
        let message = parse_error(json!({
            "file_path": "/a.rs", "line": 1, "character": 2, long.clone(): 1
        }));
        assert!(message.contains("<2048-byte name>"), "{message}");
        assert!(!message.contains(&long), "{message}");
        assert!(message.contains("`file_path`"), "{message}");
    }

    #[test]
    fn test_key_at_the_bound_is_kept() {
        let key = "k".repeat(MAX_SYMBOL_NAME_BYTES);
        let message = parse_error(json!({
            "file_path": "/a.rs", "line": 1, "character": 2, key.clone(): 1
        }));
        assert!(message.contains(&key), "{message}");
    }

    #[test]
    fn test_long_string_value_is_capped() {
        let message = parse_error(json!({
            "file_path": "/a.rs", "line": "z".repeat(1 << 20), "character": 2
        }));
        assert!(
            message.len() <= MAX_ERROR_MESSAGE_CALLER_BYTES + 64,
            "{}",
            message.len()
        );
        assert!(message.starts_with(DESERIALIZE_ERROR_PREFIX));
    }

    #[test]
    fn test_nested_long_keys_are_bounded() {
        let mut value = json!({"a": [{"b": 1}]});
        let long = "n".repeat(MAX_SYMBOL_NAME_BYTES + 1);
        value["a"][0][long.as_str()] = json!(1);
        bound_keys(&mut value);
        let text = value.to_string();
        assert!(!text.contains(&long));
        assert!(text.contains(&format!("<{}-byte name>", long.len())));
    }

    #[test]
    fn test_equal_length_long_keys_stay_distinct() {
        let mut value = json!({});
        for c in ['a', 'b', 'c'] {
            value[c.to_string().repeat(MAX_SYMBOL_NAME_BYTES + 1)] = json!(1);
        }
        assert!(bound_keys(&mut value));
        let len = MAX_SYMBOL_NAME_BYTES + 1;
        let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys.len(), 3, "{keys:?}");
        assert!(keys.contains(&format!("<{len}-byte name>")));
        assert!(keys.contains(&format!("<{len}-byte name #2>")));
    }

    #[test]
    fn test_object_without_long_keys_is_untouched() {
        let mut value = json!({"a": [{"b": 1}]});
        assert!(!bound_keys(&mut value));
        assert_eq!(value, json!({"a": [{"b": 1}]}));
    }
}
