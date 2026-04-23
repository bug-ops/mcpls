//! LSP client implementation with async request/response handling.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};
use tracing::{debug, error, trace, warn};

use crate::config::LspServerConfig;
use crate::error::{Error, Result};
use crate::lsp::transport::LspTransport;
use crate::lsp::types::{InboundMessage, JsonRpcRequest, LspNotification, RequestId};

/// JSON-RPC protocol version.
const JSONRPC_VERSION: &str = "2.0";

/// Type alias for pending request tracking map.
type PendingRequests = HashMap<RequestId, oneshot::Sender<Result<Value>>>;

/// LSP client with async request/response handling.
///
/// This client manages communication with an LSP server, handling:
/// - Concurrent requests with unique ID tracking
/// - Background message loop for receiving responses
/// - Timeout support for all requests
/// - Graceful shutdown
#[derive(Debug)]
pub struct LspClient {
    /// Configuration for this LSP server.
    config: LspServerConfig,

    /// Current server state.
    state: Arc<Mutex<super::ServerState>>,

    /// Atomic counter for request IDs.
    request_counter: Arc<AtomicI64>,

    /// Command sender for outbound messages.
    command_tx: mpsc::Sender<ClientCommand>,

    /// Background receiver task handle.
    receiver_task: Option<JoinHandle<Result<()>>>,
}

impl Clone for LspClient {
    /// Creates a clone that shares the underlying connection.
    ///
    /// The clone does not own the receiver task and cannot perform shutdown.
    /// All clones share the same command channel for sending requests.
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            state: Arc::clone(&self.state),
            request_counter: Arc::clone(&self.request_counter),
            command_tx: self.command_tx.clone(),
            receiver_task: None,
        }
    }
}

/// Commands for client control.
enum ClientCommand {
    /// Send a request and wait for response.
    SendRequest {
        request: JsonRpcRequest,
        response_tx: oneshot::Sender<Result<Value>>,
    },
    /// Send a notification (no response expected).
    SendNotification {
        method: String,
        params: Option<Value>,
    },
    /// Shutdown the client.
    Shutdown,
}

impl LspClient {
    /// Create a new LSP client with the given configuration.
    ///
    /// The client starts in an uninitialized state. Call `initialize()` to
    /// start the server and complete the initialization handshake.
    #[must_use]
    pub fn new(config: LspServerConfig) -> Self {
        // Placeholder channel - the receiver is intentionally dropped since
        // the client starts uninitialized. A real channel is created when
        // `from_transport` or `from_transport_with_notifications` is called.
        let (command_tx, _command_rx) = mpsc::channel(1); // Minimal capacity for placeholder

        Self {
            config,
            state: Arc::new(Mutex::new(super::ServerState::Uninitialized)),
            request_counter: Arc::new(AtomicI64::new(1)),
            command_tx,
            receiver_task: None,
        }
    }

    /// Create client from transport (for testing or custom spawning).
    ///
    /// This method initializes the background message loop with the provided transport.
    pub(crate) fn from_transport(config: LspServerConfig, transport: LspTransport) -> Self {
        let state = Arc::new(Mutex::new(super::ServerState::Initializing));
        let request_counter = Arc::new(AtomicI64::new(1));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));

        let (command_tx, command_rx) = mpsc::channel(100);

        let receiver_task = tokio::spawn(Self::message_loop(
            transport,
            command_rx,
            pending_requests,
            None,
        ));

        Self {
            config,
            state,
            request_counter,
            command_tx,
            receiver_task: Some(receiver_task),
        }
    }

    /// Create client from transport with notification forwarding.
    ///
    /// Notifications received from the LSP server will be parsed and sent
    /// through the provided channel.
    #[allow(dead_code)] // Used in Phase 4
    pub(crate) fn from_transport_with_notifications(
        config: LspServerConfig,
        transport: LspTransport,
        notification_tx: mpsc::Sender<LspNotification>,
    ) -> Self {
        let state = Arc::new(Mutex::new(super::ServerState::Initializing));
        let request_counter = Arc::new(AtomicI64::new(1));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));

        let (command_tx, command_rx) = mpsc::channel(100);

        let receiver_task = tokio::spawn(Self::message_loop(
            transport,
            command_rx,
            pending_requests,
            Some(notification_tx),
        ));

        Self {
            config,
            state,
            request_counter,
            command_tx,
            receiver_task: Some(receiver_task),
        }
    }

    /// Get the language ID for this client.
    #[must_use]
    pub fn language_id(&self) -> &str {
        &self.config.language_id
    }

    /// Get the current server state.
    pub async fn state(&self) -> super::ServerState {
        *self.state.lock().await
    }

    /// Get the configuration for this client.
    #[must_use]
    pub const fn config(&self) -> &LspServerConfig {
        &self.config
    }

    /// Send request and wait for response with timeout.
    ///
    /// # Type Parameters
    ///
    /// * `P` - The type of the request parameters (must be serializable)
    /// * `R` - The type of the response result (must be deserializable)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Server has shut down
    /// - Request times out
    /// - Response cannot be deserialized
    /// - LSP server returns an error
    pub async fn request<P, R>(
        &self,
        method: &str,
        params: P,
        timeout_duration: Duration,
    ) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = RequestId::Number(self.request_counter.fetch_add(1, Ordering::SeqCst));
        let params_value = serde_json::to_value(params)?;

        let (response_tx, response_rx) = oneshot::channel();

        let request = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.clone(),
            method: method.to_string(),
            params: Some(params_value),
        };

        debug!("Sending request: {} (id={:?})", method, id);

        self.command_tx
            .send(ClientCommand::SendRequest {
                request,
                response_tx,
            })
            .await
            .map_err(|_| Error::ServerTerminated)?;

        let result_value = timeout(timeout_duration, response_rx)
            .await
            .map_err(|_| Error::Timeout(timeout_duration.as_secs()))?
            .map_err(|_| Error::ServerTerminated)??;

        serde_json::from_value(result_value)
            .map_err(|e| Error::LspProtocolError(format!("Failed to deserialize response: {e}")))
    }

    /// Send notification (fire-and-forget, no response expected).
    ///
    /// # Errors
    ///
    /// Returns an error if the server has shut down.
    pub async fn notify<P>(&self, method: &str, params: P) -> Result<()>
    where
        P: Serialize,
    {
        let params_value = serde_json::to_value(params)?;

        debug!("Sending notification: {}", method);

        self.command_tx
            .send(ClientCommand::SendNotification {
                method: method.to_string(),
                params: Some(params_value),
            })
            .await
            .map_err(|_| Error::ServerTerminated)?;

        Ok(())
    }

    /// Shutdown client gracefully.
    ///
    /// This sends a shutdown command to the background task and waits for it to complete.
    ///
    /// # Errors
    ///
    /// Returns an error if the background task failed.
    pub async fn shutdown(mut self) -> Result<()> {
        debug!("Shutting down LSP client");

        let _ = self.command_tx.send(ClientCommand::Shutdown).await;

        if let Some(task) = self.receiver_task.take() {
            task.await
                .map_err(|e| Error::Transport(format!("Receiver task failed: {e}")))??;
        }

        *self.state.lock().await = super::ServerState::Shutdown;

        Ok(())
    }

    /// Background task: handle message I/O.
    ///
    /// This task runs in the background, handling:
    /// - Outbound requests and notifications
    /// - Inbound responses and server notifications
    /// - Matching responses to pending requests
    async fn message_loop(
        mut transport: LspTransport,
        mut command_rx: mpsc::Receiver<ClientCommand>,
        pending_requests: Arc<Mutex<PendingRequests>>,
        notification_tx: Option<mpsc::Sender<LspNotification>>,
    ) -> Result<()> {
        debug!("Message loop started");
        let result = Self::message_loop_inner(
            &mut transport,
            &mut command_rx,
            &pending_requests,
            notification_tx.as_ref(),
        )
        .await;
        if let Err(ref e) = result {
            error!("Message loop exiting with error: {}", e);
        } else {
            debug!("Message loop exiting normally");
        }
        result
    }

    async fn message_loop_inner(
        transport: &mut LspTransport,
        command_rx: &mut mpsc::Receiver<ClientCommand>,
        pending_requests: &Arc<Mutex<PendingRequests>>,
        notification_tx: Option<&mpsc::Sender<LspNotification>>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                Some(command) = command_rx.recv() => {
                    match command {
                        ClientCommand::SendRequest { request, response_tx } => {
                            pending_requests.lock().await.insert(
                                request.id.clone(),
                                response_tx,
                            );

                            let value = serde_json::to_value(&request)?;
                            transport.send(&value).await?;
                        }
                        ClientCommand::SendNotification { method, params } => {
                            let notification = serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": method,
                                "params": params,
                            });
                            transport.send(&notification).await?;
                        }
                        ClientCommand::Shutdown => {
                            debug!("Client shutdown requested");
                            break;
                        }
                    }
                }

                message = transport.receive() => {
                    let message = match message {
                        Ok(m) => m,
                        Err(e) => {
                            error!("Transport receive error: {}", e);
                            return Err(e);
                        }
                    };
                    match message {
                        InboundMessage::Response(response) => {
                            trace!("Received response: id={:?}", response.id);

                            let sender = pending_requests.lock().await.remove(&response.id);

                            if let Some(sender) = sender {
                                if let Some(error) = response.error {
                                    let message = if error.message.len() > 200 {
                                        format!("{}... (truncated)", &error.message[..200])
                                    } else {
                                        error.message.clone()
                                    };
                                    error!("LSP error response: {} (code {})", message, error.code);
                                    let _ = sender.send(Err(Error::LspServerError {
                                        code: error.code,
                                        message: error.message,
                                    }));
                                } else if let Some(result) = response.result {
                                    let _ = sender.send(Ok(result));
                                } else {
                                    // LSP spec allows null result for some requests (e.g., hover with no info).
                                    // Treat as successful response with null value.
                                    trace!("Response with null result: {:?}", response.id);
                                    let _ = sender.send(Ok(Value::Null));
                                }
                            } else {
                                warn!("Received response for unknown request ID: {:?}", response.id);
                            }
                        }
                        InboundMessage::Notification(notification) => {
                            debug!("Received notification: {}", notification.method);

                            // Parse notification into typed variant
                            let typed = LspNotification::parse(&notification.method, notification.params);

                            // Forward to notification handler if sender is available
                            if let Some(tx) = notification_tx {
                                // Log diagnostics count since it's useful for debugging
                                if let LspNotification::PublishDiagnostics(ref params) = typed {
                                    debug!(
                                        "Forwarding diagnostics for {}: {} items",
                                        params.uri.as_str(),
                                        params.diagnostics.len()
                                    );
                                } else {
                                    trace!("Forwarding notification: {:?}", typed);
                                }

                                // Send the notification with backpressure handling
                                if tx.try_send(typed).is_err() {
                                    warn!("Notification channel full or closed, dropping notification");
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_request_id_generation() {
        let counter = AtomicI64::new(1);

        let id1 = counter.fetch_add(1, Ordering::SeqCst);
        let id2 = counter.fetch_add(1, Ordering::SeqCst);
        let id3 = counter.fetch_add(1, Ordering::SeqCst);

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_client_creation() {
        let config = LspServerConfig::rust_analyzer();

        let client = LspClient::new(config);
        assert_eq!(client.language_id(), "rust");
    }

    #[test]
    fn test_client_clone() {
        let config = LspServerConfig::rust_analyzer();
        let client = LspClient::new(config);

        #[allow(clippy::redundant_clone)]
        let cloned = client.clone();
        assert_eq!(cloned.language_id(), "rust");

        assert!(
            cloned.receiver_task.is_none(),
            "Cloned client should not own receiver task"
        );
    }

    #[tokio::test]
    async fn test_null_response_handling() {
        use crate::lsp::types::{JsonRpcResponse, RequestId};

        let pending_requests: Arc<Mutex<PendingRequests>> = Arc::new(Mutex::new(HashMap::new()));

        let (response_tx, response_rx) = oneshot::channel::<Result<Value>>();

        pending_requests
            .lock()
            .await
            .insert(RequestId::Number(1), response_tx);

        let null_response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(1),
            result: None,
            error: None,
        };

        let sender = pending_requests.lock().await.remove(&null_response.id);
        if let Some(sender) = sender {
            let _ = sender.send(Ok(Value::Null));
        }

        let timeout_result =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), response_rx).await;

        assert!(timeout_result.is_ok(), "Should not timeout");

        let channel_result = timeout_result.unwrap();
        assert!(
            channel_result.is_ok(),
            "Channel should not be closed: {:?}",
            channel_result.err()
        );

        let response = channel_result.unwrap();
        assert!(
            response.is_ok(),
            "Should receive Ok(Value::Null), not Err: {:?}",
            response.err()
        );

        let value = response.unwrap();
        assert_eq!(value, Value::Null, "Should receive Value::Null");
    }

    #[tokio::test]
    async fn test_error_response_handling() {
        use crate::lsp::types::{JsonRpcError, JsonRpcResponse, RequestId};

        let pending_requests: Arc<Mutex<PendingRequests>> = Arc::new(Mutex::new(HashMap::new()));
        let (response_tx, response_rx) = oneshot::channel::<Result<Value>>();

        pending_requests
            .lock()
            .await
            .insert(RequestId::Number(1), response_tx);

        let error_response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(1),
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: "Method not found".to_string(),
                data: None,
            }),
        };

        let sender = pending_requests.lock().await.remove(&error_response.id);
        if let Some(sender) = sender {
            if let Some(error) = error_response.error {
                let _ = sender.send(Err(Error::LspServerError {
                    code: error.code,
                    message: error.message,
                }));
            }
        }

        let result = response_rx.await.unwrap();
        assert!(result.is_err(), "Should receive error");

        if let Err(Error::LspServerError { code, message }) = result {
            assert_eq!(code, -32601);
            assert_eq!(message, "Method not found");
        } else {
            panic!("Expected LspServerError");
        }
    }

    #[tokio::test]
    async fn test_unknown_request_id() {
        use crate::lsp::types::{JsonRpcResponse, RequestId};

        let pending_requests: Arc<Mutex<PendingRequests>> = Arc::new(Mutex::new(HashMap::new()));

        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(999),
            result: Some(Value::Null),
            error: None,
        };

        let sender = pending_requests.lock().await.remove(&response.id);
        assert!(sender.is_none(), "Should not find sender for unknown ID");
    }

    #[tokio::test]
    async fn test_long_error_message_truncation() {
        use crate::lsp::types::{JsonRpcError, JsonRpcResponse, RequestId};

        let pending_requests: Arc<Mutex<PendingRequests>> = Arc::new(Mutex::new(HashMap::new()));
        let (response_tx, response_rx) = oneshot::channel::<Result<Value>>();

        pending_requests
            .lock()
            .await
            .insert(RequestId::Number(1), response_tx);

        let long_message = "x".repeat(250);
        let error_response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(1),
            result: None,
            error: Some(JsonRpcError {
                code: -32700,
                message: long_message.clone(),
                data: None,
            }),
        };

        let sender = pending_requests.lock().await.remove(&error_response.id);
        if let Some(sender) = sender {
            if let Some(error) = error_response.error {
                let _ = sender.send(Err(Error::LspServerError {
                    code: error.code,
                    message: error.message,
                }));
            }
        }

        let result = response_rx.await.unwrap();
        assert!(result.is_err());

        if let Err(Error::LspServerError { code, message }) = result {
            assert_eq!(code, -32700);
            assert_eq!(
                message.len(),
                250,
                "Full message should be preserved in Error"
            );
        } else {
            panic!("Expected LspServerError");
        }
    }

    #[tokio::test]
    async fn test_concurrent_request_ids() {
        let counter = Arc::new(AtomicI64::new(1));

        let counter1 = Arc::clone(&counter);
        let counter2 = Arc::clone(&counter);
        let counter3 = Arc::clone(&counter);

        let handles = vec![
            tokio::spawn(async move { counter1.fetch_add(1, Ordering::SeqCst) }),
            tokio::spawn(async move { counter2.fetch_add(1, Ordering::SeqCst) }),
            tokio::spawn(async move { counter3.fetch_add(1, Ordering::SeqCst) }),
        ];

        let mut ids = Vec::new();
        for handle in handles {
            ids.push(handle.await.unwrap());
        }

        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3], "IDs should be unique and sequential");
    }

    #[test]
    fn test_jsonrpc_version_constant() {
        assert_eq!(JSONRPC_VERSION, "2.0");
    }

    #[tokio::test]
    async fn test_state_returns_current_state() {
        let config = LspServerConfig::rust_analyzer();
        let client = LspClient::new(config);

        let state = client.state().await;
        assert_eq!(state, super::super::ServerState::Uninitialized);
    }

    #[test]
    fn test_config_returns_config_reference() {
        let config = LspServerConfig::rust_analyzer();
        let client = LspClient::new(config);

        let config_ref = client.config();
        assert_eq!(config_ref.language_id, "rust");
        assert_eq!(config_ref.command, "rust-analyzer");
    }

    #[test]
    fn test_config_returns_custom_config() {
        use std::collections::HashMap;

        let mut env = HashMap::new();
        env.insert("TEST_VAR".to_string(), "test_value".to_string());

        let config = LspServerConfig {
            language_id: "custom".to_string(),
            command: "custom-server".to_string(),
            args: vec!["--arg1".to_string()],
            env,
            file_patterns: vec!["**/*.custom".to_string()],
            initialization_options: Some(serde_json::json!({"option": true})),
            timeout_seconds: 45,
        };
        let client = LspClient::new(config);

        let config_ref = client.config();
        assert_eq!(config_ref.language_id, "custom");
        assert_eq!(config_ref.command, "custom-server");
        assert_eq!(config_ref.args, vec!["--arg1"]);
        assert_eq!(config_ref.env.get("TEST_VAR"), Some(&"test_value".to_string()));
        assert_eq!(config_ref.timeout_seconds, 45);
    }

    #[test]
    fn test_config_same_after_clone() {
        let config = LspServerConfig::pyright();
        let client = LspClient::new(config);
        let cloned = client.clone();

        let orig_config = client.config();
        let cloned_config = cloned.config();

        assert_eq!(orig_config.language_id, cloned_config.language_id);
        assert_eq!(orig_config.command, cloned_config.command);
        assert_eq!(orig_config.args, cloned_config.args);
        assert_eq!(orig_config.timeout_seconds, cloned_config.timeout_seconds);
    }
}
