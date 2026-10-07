//! JSON-RPC 2.0 message types for LSP communication.

use std::borrow::Cow;

// Brings `Notification::METHOD` into scope for `<Type>::METHOD` below.
use lsp_types::Notification as _;
// Re-export LSP notification types from lsp_types to avoid duplication.
pub use lsp_types::{
    LogMessageParams, ProgressParams, PublishDiagnosticsParams, ShowMessageParams,
};
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use tracing::debug;

/// JSON-RPC protocol version carried by every message.
pub const JSONRPC_VERSION: &str = "2.0";

/// The `"jsonrpc": "2.0"` member of a message: serializes as [`JSONRPC_VERSION`]
/// and refuses to deserialize from anything else.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JsonRpcVersion;

impl Serialize for JsonRpcVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(JSONRPC_VERSION)
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let version = Cow::<str>::deserialize(deserializer)?;
        if version == JSONRPC_VERSION {
            Ok(Self)
        } else {
            Err(de::Error::invalid_value(
                de::Unexpected::Str(&version),
                &JSONRPC_VERSION,
            ))
        }
    }
}

/// JSON-RPC 2.0 request message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// JSON-RPC version, always "2.0".
    pub jsonrpc: JsonRpcVersion,
    /// Request identifier.
    pub id: RequestId,
    /// Method name.
    pub method: String,
    /// Optional method parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 response message.
///
/// Deserializing refuses a response that carries both `result` and `error`,
/// and reads one that carries neither as a `null` result.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    /// JSON-RPC version, always "2.0".
    pub jsonrpc: JsonRpcVersion,
    /// Request identifier.
    pub id: RequestId,
    /// How the request ended.
    #[serde(flatten)]
    pub outcome: JsonRpcOutcome,
}

impl<'de> Deserialize<'de> for JsonRpcResponse {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A present `result: null` must stay distinguishable from an absent one.
        fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
            Value::deserialize(deserializer).map(Some)
        }

        #[derive(Deserialize)]
        struct Raw {
            jsonrpc: JsonRpcVersion,
            id: RequestId,
            #[serde(default, deserialize_with = "present")]
            result: Option<Value>,
            #[serde(default)]
            error: Option<JsonRpcError>,
        }

        let Raw {
            jsonrpc,
            id,
            result,
            error,
        } = Raw::deserialize(deserializer)?;
        let outcome = match (result, error) {
            (Some(_), Some(_)) => {
                return Err(de::Error::custom("response carries both result and error"));
            }
            (Some(result), None) => JsonRpcOutcome::Result(result),
            (None, Some(error)) => JsonRpcOutcome::Error(error),
            (None, None) => JsonRpcOutcome::Result(Value::Null),
        };
        Ok(Self {
            jsonrpc,
            id,
            outcome,
        })
    }
}

/// JSON-RPC 2.0 notification message (no response expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// JSON-RPC version, always "2.0".
    pub jsonrpc: JsonRpcVersion,
    /// Method name.
    pub method: String,
    /// Optional method parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Error code.
    pub code: i32,
    /// Error message.
    pub message: String,
    /// Optional additional error data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    /// An error object with `code` and `message` and no data.
    #[must_use]
    pub fn new(code: lsp_types::ErrorCodes, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data: None,
        }
    }
}

/// How a JSON-RPC response ends: a result or an error, never both and never
/// neither.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JsonRpcOutcome {
    /// The request succeeded with this result (`null` included).
    Result(Value),
    /// The request failed.
    Error(JsonRpcError),
}

impl From<Result<Value, JsonRpcError>> for JsonRpcOutcome {
    fn from(outcome: Result<Value, JsonRpcError>) -> Self {
        match outcome {
            Ok(result) => Self::Result(result),
            Err(error) => Self::Error(error),
        }
    }
}

/// A JSON-RPC 2.0 response mcpls sends to the server: the one place the
/// outbound envelope is built.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcReply {
    jsonrpc: JsonRpcVersion,
    id: RequestId,
    #[serde(flatten)]
    outcome: JsonRpcOutcome,
}

impl JsonRpcReply {
    /// A reply to request `id`.
    #[must_use]
    pub const fn new(id: RequestId, outcome: JsonRpcOutcome) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            id,
            outcome,
        }
    }
}

/// Request ID can be a number or string per JSON-RPC 2.0.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric request ID.
    Number(i64),
    /// String request ID.
    String(String),
}

/// Inbound message from LSP server.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum InboundMessage {
    /// Response to a request.
    Response(JsonRpcResponse),
    /// Request from server to client.
    Request(JsonRpcRequest),
    /// Notification from server.
    Notification(JsonRpcNotification),
    /// A response whose body could not be decoded (for example a result
    /// nested deeper than the JSON parser allows) but whose framing and `id`
    /// were intact: only the request it answers fails, the connection stays
    /// up.
    UndecodableResponse {
        /// Identifier of the request the response answers.
        id: RequestId,
    },
    /// A server request whose body could not be decoded but whose framing
    /// and `id` were intact: it is answered with an error and the connection
    /// stays up.
    UndecodableRequest {
        /// Identifier of the request to answer.
        id: RequestId,
    },
    /// A server notification that could not be decoded; it is dropped.
    UndecodableNotification,
    /// A framed message that is not JSON, or that carries nothing to tell what
    /// it was or whom it addresses; it is dropped.
    UndecodableFrame,
}

impl InboundMessage {
    /// Whether this message stands for a frame that could not be decoded.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::lsp::{InboundMessage, RequestId};
    ///
    /// assert!(InboundMessage::UndecodableResponse { id: RequestId::Number(1) }.is_undecodable());
    /// assert!(InboundMessage::UndecodableNotification.is_undecodable());
    /// ```
    #[must_use]
    pub const fn is_undecodable(&self) -> bool {
        matches!(
            self,
            Self::UndecodableResponse { .. }
                | Self::UndecodableRequest { .. }
                | Self::UndecodableNotification
                | Self::UndecodableFrame
        )
    }
}

/// Typed LSP notification variants.
///
/// Uses types from `lsp_types` crate for LSP-standard notifications.
#[derive(Debug)]
pub enum LspNotification {
    /// textDocument/publishDiagnostics
    PublishDiagnostics(PublishDiagnosticsParams),
    /// window/logMessage
    LogMessage(LogMessageParams),
    /// window/showMessage
    ShowMessage(ShowMessageParams),
    /// $/progress
    Progress(ProgressParams),
    /// Unknown or unhandled notification
    Other {
        /// Method name.
        method: Cow<'static, str>,
        /// Optional parameters.
        params: Option<serde_json::Value>,
    },
}

/// A `$/progress` notification's `value.kind`, decoded from the raw JSON
/// payload (`ProgressParams::value` is `serde_json::Value`; `gen-lsp-types`
/// 0.11.0 has no union type for it).
///
/// Single source of truth for both `LspClient::lifecycle_lane` (routes
/// `begin`/`end` to the lifecycle lane, drops everything else) and
/// `bridge::indexing::IndexingTracker::observe_progress` (the settle/latch
/// transition) -- previously each read `value["kind"]` independently, so
/// the two could silently drift out of sync (Fix 9). No `Report` variant:
/// a `report` frame is dropped at the transport boundary rather than
/// classified into a variant here. A frame whose `kind` is missing,
/// `"report"`, or anything else unrecognized parses to `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressKind {
    /// A `$/progress` frame with `kind: "begin"`.
    Begin,
    /// A `$/progress` frame with `kind: "end"`.
    End,
}

impl ProgressKind {
    /// Classify a `$/progress` notification's raw `value` payload.
    #[must_use]
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        match value.get("kind").and_then(Value::as_str) {
            Some("begin") => Some(Self::Begin),
            Some("end") => Some(Self::End),
            _ => None,
        }
    }
}

impl LspNotification {
    /// Parse a notification from method name and params.
    ///
    /// Attempts to deserialize known notification types based on the method name.
    /// Falls back to `Other` variant for unknown methods or deserialization failures.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use mcpls_core::lsp::types::LspNotification;
    /// use serde_json::json;
    ///
    /// let params = json!({
    ///     "type": 3,
    ///     "message": "Server started"
    /// });
    /// let notification = LspNotification::parse("window/logMessage", Some(params));
    ///
    /// match notification {
    ///     LspNotification::LogMessage(log) => {
    ///         assert_eq!(log.kind, lsp_types::MessageType::Info);
    ///         assert_eq!(log.message, "Server started");
    ///     }
    ///     _ => panic!("Expected LogMessage variant"),
    /// }
    /// ```
    #[must_use]
    pub fn parse(method: &str, params: Option<serde_json::Value>) -> Self {
        match method {
            m if m == lsp_types::PublishDiagnosticsNotification::METHOD.as_str() => {
                Self::typed(m, params, Self::PublishDiagnostics)
            }
            m if m == lsp_types::LogMessageNotification::METHOD.as_str() => {
                Self::typed(m, params, Self::LogMessage)
            }
            m if m == lsp_types::ShowMessageNotification::METHOD.as_str() => {
                Self::typed(m, params, Self::ShowMessage)
            }
            m if m == lsp_types::ProgressNotification::METHOD.as_str() => {
                Self::typed(m, params, Self::Progress)
            }
            _ => Self::Other {
                method: Cow::Owned(method.to_string()),
                params,
            },
        }
    }

    /// Shared body for the deserialize-or-fallback arms of [`Self::parse`]:
    /// deserializes `params` as `T` and passes it to `wrap`, or falls back to
    /// `Other` (logging why) when `params` is absent or fails to deserialize.
    fn typed<T, F>(method: &str, params: Option<serde_json::Value>, wrap: F) -> Self
    where
        T: serde::de::DeserializeOwned,
        F: FnOnce(T) -> Self,
    {
        if let Some(p) = params {
            match serde_json::from_value(p) {
                Ok(parsed) => return wrap(parsed),
                Err(e) => {
                    debug!(method, error = ?e.classify(), "failed to parse notification params");
                }
            }
        }
        Self::Other {
            method: Cow::Owned(method.to_string()),
            params: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use serde_json::json;

    use super::*;

    #[test]
    fn test_reply_carries_exactly_one_of_result_and_error() {
        let id = RequestId::Number(7);
        let ok = JsonRpcReply::new(id.clone(), JsonRpcOutcome::Result(Value::Null));
        let failed = JsonRpcReply::new(
            id,
            JsonRpcOutcome::Error(JsonRpcError::new(
                lsp_types::ErrorCodes::MethodNotFound,
                "x",
            )),
        );

        assert_eq!(
            serde_json::to_value(ok).unwrap(),
            json!({"jsonrpc": "2.0", "id": 7, "result": null})
        );
        assert_eq!(
            serde_json::to_value(failed).unwrap(),
            json!({"jsonrpc": "2.0", "id": 7, "error": {"code": -32601, "message": "x"}})
        );
    }

    #[test]
    fn test_request_serialization() {
        let request = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(1),
            method: "textDocument/hover".to_string(),
            params: Some(json!({"key": "value"})),
        };

        let serialized = serde_json::to_string(&request).unwrap();
        assert!(serialized.contains("\"jsonrpc\":\"2.0\""));
        assert!(serialized.contains("\"id\":1"));
        assert!(serialized.contains("\"method\":\"textDocument/hover\""));
    }

    #[test]
    fn test_response_deserialization() {
        let json_str = r#"{"jsonrpc":"2.0","id":1,"result":{"key":"value"}}"#;
        let response: JsonRpcResponse = serde_json::from_str(json_str).unwrap();

        assert_eq!(response.id, RequestId::Number(1));
        assert_matches!(
            response.outcome,
            JsonRpcOutcome::Result(value) if value == json!({"key": "value"})
        );
    }

    #[test]
    fn test_error_response_deserialization() {
        let json_str =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid Request"}}"#;
        let response: JsonRpcResponse = serde_json::from_str(json_str).unwrap();

        assert_eq!(response.id, RequestId::Number(1));
        let JsonRpcOutcome::Error(error) = response.outcome else {
            panic!("expected an error outcome");
        };
        assert_eq!(error.code, -32600);
        assert_eq!(error.message, "Invalid Request");
    }

    #[test]
    fn test_notification_serialization() {
        let notification = JsonRpcNotification {
            jsonrpc: JsonRpcVersion,
            method: "initialized".to_string(),
            params: None,
        };

        let serialized = serde_json::to_string(&notification).unwrap();
        assert!(serialized.contains("\"jsonrpc\":\"2.0\""));
        assert!(serialized.contains("\"method\":\"initialized\""));
        assert!(!serialized.contains("\"id\""));
    }

    #[test]
    fn test_request_id_types() {
        let num_id = RequestId::Number(42);
        let str_id = RequestId::String("request-1".to_string());

        let num_json = serde_json::to_string(&num_id).unwrap();
        assert_eq!(num_json, "42");

        let str_json = serde_json::to_string(&str_id).unwrap();
        assert_eq!(str_json, "\"request-1\"");

        let parsed_num: RequestId = serde_json::from_str("42").unwrap();
        assert_eq!(parsed_num, RequestId::Number(42));

        let parsed_str: RequestId = serde_json::from_str("\"request-1\"").unwrap();
        assert_eq!(parsed_str, RequestId::String("request-1".to_string()));
    }

    #[test]
    fn test_null_response_deserialization() {
        let json_str = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let response: JsonRpcResponse = serde_json::from_str(json_str).unwrap();

        assert_eq!(response.id, RequestId::Number(1));
        assert_matches!(response.outcome, JsonRpcOutcome::Result(Value::Null));
    }

    #[test]
    fn test_null_vs_missing_result() {
        let null_json = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let null_response: JsonRpcResponse = serde_json::from_str(null_json).unwrap();
        assert_matches!(null_response.outcome, JsonRpcOutcome::Result(Value::Null));

        let missing_json = r#"{"jsonrpc":"2.0","id":1}"#;
        let missing_response: JsonRpcResponse = serde_json::from_str(missing_json).unwrap();
        assert_matches!(
            missing_response.outcome,
            JsonRpcOutcome::Result(Value::Null)
        );
    }

    #[test]
    fn test_response_with_both_result_and_error_is_refused() {
        let both = r#"{"jsonrpc":"2.0","id":1,"result":null,"error":{"code":1,"message":"x"}}"#;

        assert!(serde_json::from_str::<JsonRpcResponse>(both).is_err());
    }

    #[test]
    fn test_response_with_a_null_error_is_a_result() {
        let json_str = r#"{"jsonrpc":"2.0","id":1,"result":3,"error":null}"#;

        let response: JsonRpcResponse = serde_json::from_str(json_str).unwrap();

        assert_matches!(response.outcome, JsonRpcOutcome::Result(value) if value == json!(3));
    }

    #[test]
    fn test_any_other_jsonrpc_version_is_refused() {
        for version in [r#""1.0""#, r#""2""#, "2.0", "null"] {
            let response = format!(r#"{{"jsonrpc":{version},"id":1,"result":null}}"#);
            let request = format!(r#"{{"jsonrpc":{version},"id":1,"method":"m"}}"#);

            assert!(serde_json::from_str::<JsonRpcResponse>(&response).is_err());
            assert!(serde_json::from_str::<JsonRpcRequest>(&request).is_err());
        }
    }

    #[test]
    fn test_log_message_notification_parsing() {
        let params = json!({
            "type": 3,
            "message": "Server started successfully"
        });

        let notification = super::LspNotification::parse("window/logMessage", Some(params));

        match notification {
            super::LspNotification::LogMessage(log) => {
                // lsp_types uses `kind` field with MessageType struct
                assert_eq!(log.kind, lsp_types::MessageType::Info);
                assert_eq!(log.message, "Server started successfully");
            }
            _ => panic!("Expected LogMessage variant"),
        }
    }

    #[test]
    fn test_show_message_notification_parsing() {
        let params = json!({
            "type": 1,
            "message": "Error occurred"
        });

        let notification = super::LspNotification::parse("window/showMessage", Some(params));

        match notification {
            super::LspNotification::ShowMessage(msg) => {
                // lsp_types uses `kind` field with MessageType struct
                assert_eq!(msg.kind, lsp_types::MessageType::Error);
                assert_eq!(msg.message, "Error occurred");
            }
            _ => panic!("Expected ShowMessage variant"),
        }
    }

    #[test]
    fn test_publish_diagnostics_notification_parsing() {
        let params = json!({
            "uri": "file:///test.rs",
            "version": 1,
            "diagnostics": [
                {
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 5}
                    },
                    "severity": 1,
                    "message": "unused variable"
                }
            ]
        });

        let notification =
            super::LspNotification::parse("textDocument/publishDiagnostics", Some(params));

        match notification {
            super::LspNotification::PublishDiagnostics(diag) => {
                assert_eq!(diag.uri.to_string(), "file:///test.rs");
                assert_eq!(diag.version, Some(1));
                assert_eq!(diag.diagnostics.len(), 1);
                assert_eq!(
                    diag.diagnostics[0].message,
                    lsp_types::Message::String("unused variable".to_string())
                );
            }
            _ => panic!("Expected PublishDiagnostics variant"),
        }
    }

    #[test]
    fn test_progress_notification_parsing() {
        let params = json!({
            "token": "indexing",
            "value": {"kind": "begin", "title": "Indexing"}
        });

        let notification = super::LspNotification::parse("$/progress", Some(params));

        match notification {
            super::LspNotification::Progress(p) => {
                assert_eq!(p.value.get("kind").and_then(|v| v.as_str()), Some("begin"));
            }
            _ => panic!("Expected Progress variant"),
        }
    }

    #[test]
    fn test_malformed_progress_params() {
        // Missing the mandatory `token` field.
        let malformed_params = json!({"value": {"kind": "begin"}});

        let notification = super::LspNotification::parse("$/progress", Some(malformed_params));

        match notification {
            super::LspNotification::Other { method, params } => {
                assert_eq!(method, "$/progress");
                assert!(params.is_none());
            }
            _ => panic!("Expected Other variant for malformed params"),
        }
    }

    #[test]
    fn test_progress_with_none_params() {
        let notification = super::LspNotification::parse("$/progress", None);

        match notification {
            super::LspNotification::Other { method, params } => {
                assert_eq!(method, "$/progress");
                assert!(params.is_none());
            }
            _ => panic!("Expected Other variant when params is None"),
        }
    }

    #[test]
    fn test_unknown_notification_method() {
        let params = json!({"someKey": "someValue"});

        let notification = super::LspNotification::parse("unknown/method", Some(params.clone()));

        match notification {
            super::LspNotification::Other { method, params: p } => {
                assert_eq!(method, "unknown/method");
                assert_eq!(p, Some(params));
            }
            _ => panic!("Expected Other variant"),
        }
    }

    #[test]
    fn test_notification_with_no_params() {
        let notification = super::LspNotification::parse("some/notification", None);

        match notification {
            super::LspNotification::Other { method, params } => {
                assert_eq!(method, "some/notification");
                assert!(params.is_none());
            }
            _ => panic!("Expected Other variant"),
        }
    }

    #[test]
    fn test_malformed_log_message_params() {
        let malformed_params = json!({
            "invalidField": "value"
        });

        let notification =
            super::LspNotification::parse("window/logMessage", Some(malformed_params));

        match notification {
            super::LspNotification::Other { method, params } => {
                assert_eq!(method, "window/logMessage");
                assert!(params.is_none());
            }
            _ => panic!("Expected Other variant for malformed params"),
        }
    }

    #[test]
    fn test_malformed_show_message_params() {
        let malformed_params = json!({
            "type": "not_a_number",
            "message": "test"
        });

        let notification =
            super::LspNotification::parse("window/showMessage", Some(malformed_params));

        match notification {
            super::LspNotification::Other { method, params } => {
                assert_eq!(method, "window/showMessage");
                assert!(params.is_none());
            }
            _ => panic!("Expected Other variant for malformed params"),
        }
    }

    #[test]
    fn test_malformed_publish_diagnostics_params() {
        let malformed_params = json!({
            "uri": 123,
            "diagnostics": "not_an_array"
        });

        let notification = super::LspNotification::parse(
            "textDocument/publishDiagnostics",
            Some(malformed_params),
        );

        match notification {
            super::LspNotification::Other { method, params } => {
                assert_eq!(method, "textDocument/publishDiagnostics");
                assert!(params.is_none());
            }
            _ => panic!("Expected Other variant for malformed params"),
        }
    }

    #[test]
    fn test_log_message_with_none_params() {
        let notification = super::LspNotification::parse("window/logMessage", None);

        match notification {
            super::LspNotification::Other { method, params } => {
                assert_eq!(method, "window/logMessage");
                assert!(params.is_none());
            }
            _ => panic!("Expected Other variant when params is None"),
        }
    }

    #[test]
    fn test_show_message_with_none_params() {
        let notification = super::LspNotification::parse("window/showMessage", None);

        match notification {
            super::LspNotification::Other { method, params } => {
                assert_eq!(method, "window/showMessage");
                assert!(params.is_none());
            }
            _ => panic!("Expected Other variant when params is None"),
        }
    }

    #[test]
    fn test_publish_diagnostics_with_none_params() {
        let notification = super::LspNotification::parse("textDocument/publishDiagnostics", None);

        match notification {
            super::LspNotification::Other { method, params } => {
                assert_eq!(method, "textDocument/publishDiagnostics");
                assert!(params.is_none());
            }
            _ => panic!("Expected Other variant when params is None"),
        }
    }
}
