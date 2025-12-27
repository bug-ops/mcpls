//! JSON-RPC 2.0 message types for LSP communication.

use std::borrow::Cow;

// Re-export LSP notification types from lsp_types to avoid duplication.
pub use lsp_types::{LogMessageParams, PublishDiagnosticsParams, ShowMessageParams};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 request message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// JSON-RPC version, always "2.0".
    pub jsonrpc: String,
    /// Request identifier.
    pub id: RequestId,
    /// Method name.
    pub method: String,
    /// Optional method parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 response message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// JSON-RPC version, always "2.0".
    pub jsonrpc: String,
    /// Request identifier.
    pub id: RequestId,
    /// Result value (if successful).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error object (if failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 notification message (no response expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// JSON-RPC version, always "2.0".
    pub jsonrpc: String,
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
pub enum InboundMessage {
    /// Response to a request.
    Response(JsonRpcResponse),
    /// Notification from server.
    Notification(JsonRpcNotification),
}

/// Typed LSP notification variants.
///
/// Uses types from `lsp_types` crate for LSP-standard notifications.
#[derive(Debug)]
pub enum LspNotification {
    /// textDocument/publishDiagnostics
    PublishDiagnostics(PublishDiagnosticsParams),
    /// window/logMessage
    #[allow(dead_code)] // Used in Phase 4
    LogMessage(LogMessageParams),
    /// window/showMessage
    #[allow(dead_code)] // Used in Phase 4
    ShowMessage(ShowMessageParams),
    /// Unknown or unhandled notification
    Other {
        /// Method name.
        #[allow(dead_code)] // Used in Phase 4
        method: Cow<'static, str>,
        /// Optional parameters.
        #[allow(dead_code)] // Used in Phase 4
        params: Option<serde_json::Value>,
    },
}

impl LspNotification {
    /// Parse a notification from method name and params.
    ///
    /// Attempts to deserialize known notification types based on the method name.
    /// Falls back to `Other` variant for unknown methods or deserialization failures.
    ///
    /// # Examples
    ///
    /// ```
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
    ///         // lsp_types uses `typ` field with MessageType struct
    ///         assert_eq!(log.typ, lsp_types::MessageType::INFO);
    ///         assert_eq!(log.message, "Server started");
    ///     }
    ///     _ => panic!("Expected LogMessage variant"),
    /// }
    /// ```
    pub fn parse(method: &str, params: Option<serde_json::Value>) -> Self {
        match method {
            "textDocument/publishDiagnostics" => {
                if let Some(p) = params {
                    if let Ok(parsed) = serde_json::from_value(p) {
                        return Self::PublishDiagnostics(parsed);
                    }
                }
                Self::Other {
                    method: Cow::Owned(method.to_string()),
                    params: None,
                }
            }
            "window/logMessage" => {
                if let Some(p) = params {
                    if let Ok(parsed) = serde_json::from_value(p) {
                        return Self::LogMessage(parsed);
                    }
                }
                Self::Other {
                    method: Cow::Owned(method.to_string()),
                    params: None,
                }
            }
            "window/showMessage" => {
                if let Some(p) = params {
                    if let Ok(parsed) = serde_json::from_value(p) {
                        return Self::ShowMessage(parsed);
                    }
                }
                Self::Other {
                    method: Cow::Owned(method.to_string()),
                    params: None,
                }
            }
            _ => Self::Other {
                method: Cow::Owned(method.to_string()),
                params,
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_request_serialization() {
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
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

        assert_eq!(response.jsonrpc, "2.0");
        assert_eq!(response.id, RequestId::Number(1));
        assert!(response.result.is_some());
        assert!(response.error.is_none());
    }

    #[test]
    fn test_error_response_deserialization() {
        let json_str =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid Request"}}"#;
        let response: JsonRpcResponse = serde_json::from_str(json_str).unwrap();

        assert_eq!(response.jsonrpc, "2.0");
        assert_eq!(response.id, RequestId::Number(1));
        assert!(response.result.is_none());
        assert!(response.error.is_some());

        let error = response.error.unwrap();
        assert_eq!(error.code, -32600);
        assert_eq!(error.message, "Invalid Request");
    }

    #[test]
    fn test_notification_serialization() {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
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

        assert_eq!(response.jsonrpc, "2.0");
        assert_eq!(response.id, RequestId::Number(1));
        assert!(response.result.is_none());
        assert!(response.error.is_none());
    }

    #[test]
    fn test_null_vs_missing_result() {
        let null_json = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let null_response: JsonRpcResponse = serde_json::from_str(null_json).unwrap();
        assert!(null_response.result.is_none());

        let missing_json = r#"{"jsonrpc":"2.0","id":1}"#;
        let missing_response: JsonRpcResponse = serde_json::from_str(missing_json).unwrap();
        assert!(missing_response.result.is_none());

        assert_eq!(null_response.result, missing_response.result);
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
                // lsp_types uses `typ` field with MessageType struct
                assert_eq!(log.typ, lsp_types::MessageType::INFO);
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
                // lsp_types uses `typ` field with MessageType struct
                assert_eq!(msg.typ, lsp_types::MessageType::ERROR);
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
                assert_eq!(diag.diagnostics[0].message, "unused variable");
            }
            _ => panic!("Expected PublishDiagnostics variant"),
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
