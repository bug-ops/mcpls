//! JSON-RPC 2.0 message types for LSP communication.

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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
