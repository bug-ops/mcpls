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
use crate::lsp::types::{
    InboundMessage, JsonRpcError, JsonRpcRequest, JsonRpcResponse, LspNotification, RequestId,
};

/// JSON-RPC protocol version.
const JSONRPC_VERSION: &str = "2.0";

/// LSP error code returned when the server cancels a request and wants the client to retry.
const SERVER_CANCELLED_CODE: i32 = -32802;

/// Maximum number of retry attempts for server-cancelled requests.
const SERVER_CANCELLED_MAX_RETRIES: u32 = 3;

/// Initial backoff delay for server-cancelled retries (milliseconds).
const SERVER_CANCELLED_INITIAL_DELAY_MS: u64 = 500;

/// Upper bound on the effective timeout for completion requests, regardless
/// of `request_timeout_seconds`.
///
/// Completions are latency-sensitive: a completion list that takes longer
/// than this is no longer useful to the caller. This is a deliberate MVP
/// ceiling, not an oversight — completions cannot be configured above this
/// value today. See [`LspClient::completion_timeout`].
const COMPLETION_TIMEOUT_CAP: Duration = Duration::from_secs(10);

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

    /// Requests awaiting a response, shared with the background message loop.
    ///
    /// Exposed here (not just captured by the loop) so [`Self::request`] can
    /// remove its own entry on timeout instead of leaking it, and so a
    /// connection known to be dead can fail its stragglers immediately via
    /// [`Self::fail_pending_requests`] rather than leaving each to discover
    /// that only when its own timeout elapses.
    pending_requests: Arc<Mutex<PendingRequests>>,

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
            pending_requests: Arc::clone(&self.pending_requests),
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
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            receiver_task: None,
        }
    }

    /// Create client from transport (for testing or custom spawning).
    ///
    /// This method initializes the background message loop with the provided transport.
    #[cfg(test)]
    pub(crate) fn from_transport(config: LspServerConfig, transport: LspTransport) -> Self {
        let state = Arc::new(Mutex::new(super::ServerState::Initializing));
        let request_counter = Arc::new(AtomicI64::new(1));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));

        let (command_tx, command_rx) = mpsc::channel(100);

        let receiver_task = tokio::spawn(Self::message_loop(
            transport,
            command_rx,
            Arc::clone(&pending_requests),
            None,
        ));

        Self {
            config,
            state,
            request_counter,
            command_tx,
            pending_requests,
            receiver_task: Some(receiver_task),
        }
    }

    /// Create client from transport with notification forwarding.
    ///
    /// Notifications received from the LSP server will be parsed and sent
    /// through the provided channel.
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
            Arc::clone(&pending_requests),
            Some(notification_tx),
        ));

        Self {
            config,
            state,
            request_counter,
            command_tx,
            pending_requests,
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

    /// The timeout applied to a single LSP request attempt, derived from
    /// [`LspServerConfig::request_timeout_seconds`].
    ///
    /// This bounds one attempt, not a whole tool call: [`Self::request`]
    /// retries up to `SERVER_CANCELLED_MAX_RETRIES` (3) additional times on a
    /// `-32802` (`ServerCancelled`) response, so the worst-case latency for a
    /// single tool call is `4 * request_timeout() + 3.5s` (the sum of the
    /// retry backoff delays).
    ///
    /// The configured value is clamped to at least 1 second. [`ServerConfig::load_from`]
    /// rejects `request_timeout_seconds == 0` at load time for TOML-sourced
    /// configs, but [`crate::serve`] accepts a caller-built `ServerConfig`
    /// without going through that validation, so this clamp is the last line
    /// of defense against a zero-duration timeout that would fail every
    /// request instantly.
    ///
    /// [`ServerConfig::load_from`]: crate::config::ServerConfig::load_from
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcpls_core::config::LspServerConfig;
    /// use mcpls_core::lsp::LspClient;
    ///
    /// let mut config = LspServerConfig::rust_analyzer();
    /// config.request_timeout_seconds = 45;
    /// let client = LspClient::new(config);
    ///
    /// assert_eq!(client.request_timeout(), Duration::from_secs(45));
    /// ```
    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.config.request_timeout_seconds.max(1))
    }

    /// The timeout applied to completion (`textDocument/completion`) requests.
    ///
    /// Equal to [`Self::request_timeout`], capped at 10 seconds. Completions
    /// cannot be configured above this cap by any
    /// value of `request_timeout_seconds` — if that proves insufficient in
    /// practice, the fix is a dedicated `completion_timeout_seconds` field,
    /// not raising this cap.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcpls_core::config::LspServerConfig;
    /// use mcpls_core::lsp::LspClient;
    ///
    /// let mut config = LspServerConfig::rust_analyzer();
    /// config.request_timeout_seconds = 300;
    /// let client = LspClient::new(config);
    ///
    /// // Capped at 10s even though request_timeout_seconds is 300.
    /// assert_eq!(client.completion_timeout(), Duration::from_secs(10));
    /// assert!(client.completion_timeout() <= client.request_timeout());
    /// ```
    #[must_use]
    pub fn completion_timeout(&self) -> Duration {
        self.request_timeout().min(COMPLETION_TIMEOUT_CAP)
    }

    /// Send request and wait for response with timeout.
    ///
    /// Automatically retries up to 3 times when the server returns error code
    /// -32802 (`ServerCancelled`) with `data.retriggerRequest == true`, using
    /// exponential backoff starting at 500 ms.
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
        let params_value = serde_json::to_value(params)?;
        let mut delay_ms = SERVER_CANCELLED_INITIAL_DELAY_MS;

        for attempt in 0..=SERVER_CANCELLED_MAX_RETRIES {
            if attempt > 0 {
                debug!(
                    "Retrying {} after ServerCancelled (attempt {}/{}), backoff={}ms",
                    method, attempt, SERVER_CANCELLED_MAX_RETRIES, delay_ms
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                delay_ms *= 2;
            }

            let id = RequestId::Number(self.request_counter.fetch_add(1, Ordering::SeqCst));
            let (response_tx, response_rx) = oneshot::channel();
            let request = JsonRpcRequest {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: id.clone(),
                method: method.to_string(),
                params: Some(params_value.clone()),
            };

            debug!("Sending request: {} (id={:?})", method, id);

            self.command_tx
                .send(ClientCommand::SendRequest {
                    request,
                    response_tx,
                })
                .await
                .map_err(|_| Error::ServerTerminated)?;

            let outcome = match timeout(timeout_duration, response_rx).await {
                Ok(received) => received.map_err(|_| Error::ServerTerminated)?,
                Err(_elapsed) => {
                    // The response may still arrive after this point (the
                    // server is just slow, not dead), but nothing will ever
                    // read it again -- drop the now-orphaned entry instead of
                    // leaking it in `pending_requests` forever.
                    self.pending_requests.lock().await.remove(&id);
                    return Err(Error::Timeout(timeout_duration.as_secs()));
                }
            };

            match outcome {
                Ok(result_value) => {
                    return serde_json::from_value(result_value).map_err(|e| {
                        Error::LspProtocolError(format!("Failed to deserialize response: {e}"))
                    });
                }
                Err(Error::LspServerError {
                    code,
                    ref message,
                    ref data,
                }) if code == SERVER_CANCELLED_CODE && Self::should_retrigger(data.as_ref()) => {
                    warn!(
                        "ServerCancelled (-32802) on '{}', will retry: {}",
                        method, message
                    );
                    if attempt == SERVER_CANCELLED_MAX_RETRIES {
                        return Err(Error::LspServerError {
                            code,
                            message: message.clone(),
                            data: data.clone(),
                        });
                    }
                    // continue loop for next attempt
                }
                Err(e) => return Err(e),
            }
        }

        Err(Error::ServerTerminated)
    }

    /// Returns true when the error data from a `ServerCancelled` (-32802) response
    /// indicates the server wants the client to retrigger the request.
    ///
    /// Per the LSP specification, `data.retriggerRequest == true` is the signal.
    /// When `data` is absent (older servers), we default to retrying anyway because
    /// code -32802 is exclusively used for this purpose.
    fn should_retrigger(data: Option<&Value>) -> bool {
        data.is_none_or(|v| {
            v.get("retriggerRequest")
                .and_then(Value::as_bool)
                .unwrap_or(true)
        })
    }

    /// Fail every request still parked in `pending_requests` with
    /// `Error::ServerTerminated`, instead of leaving each to discover a dead
    /// connection only when its own timeout elapses.
    ///
    /// Intended for a client that is about to be discarded -- e.g.
    /// superseded by a respawned replacement for the same server -- so
    /// callers still waiting on it unblock immediately.
    pub(crate) async fn fail_pending_requests(&self) {
        let mut pending = self.pending_requests.lock().await;
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(Error::ServerTerminated));
        }
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
                                        data: error.data,
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
                        InboundMessage::Request(request) => {
                            debug!(
                                "Received server request: {} (id={:?})",
                                request.method, request.id
                            );
                            let response = Self::server_request_response(request);
                            let value = serde_json::to_value(&response)?;
                            transport.send(&value).await?;
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

    fn server_request_response(request: JsonRpcRequest) -> JsonRpcResponse {
        match Self::server_request_result(&request.method, request.params.as_ref()) {
            Ok(result) => JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: request.id,
                result: Some(result),
                error: None,
            },
            Err(error) => JsonRpcResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: request.id,
                result: None,
                error: Some(error),
            },
        }
    }

    fn server_request_result(
        method: &str,
        params: Option<&Value>,
    ) -> std::result::Result<Value, JsonRpcError> {
        match method {
            "client/registerCapability"
            | "client/unregisterCapability"
            | "workspace/workspaceFolders"
            | "workspace/diagnostic/refresh"
            | "workspace/semanticTokens/refresh"
            | "workspace/inlayHint/refresh"
            | "workspace/codeLens/refresh"
            | "window/showMessageRequest" => Ok(Value::Null),
            "workspace/configuration" => Ok(Self::workspace_configuration_result(params)),
            "workspace/applyEdit" => Ok(serde_json::json!({ "applied": false })),
            _ => Err(JsonRpcError {
                code: -32601,
                message: format!("Unhandled server request: {method}"),
                data: None,
            }),
        }
    }

    fn workspace_configuration_result(params: Option<&Value>) -> Value {
        let item_count = params
            .and_then(|value| value.get("items"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len);

        Value::Array(vec![Value::Null; item_count])
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

    #[test]
    fn test_request_timeout_and_completion_timeout_at_default() {
        let config = LspServerConfig::rust_analyzer();
        let client = LspClient::new(config);

        assert_eq!(client.request_timeout(), Duration::from_secs(30));
        assert_eq!(client.completion_timeout(), Duration::from_secs(10));
    }

    #[test]
    fn test_completion_timeout_clamps_to_ten_seconds() {
        for secs in [1, 2, 3, 30, 300] {
            let mut config = LspServerConfig::rust_analyzer();
            config.request_timeout_seconds = secs;
            let client = LspClient::new(config);

            assert_eq!(
                client.completion_timeout(),
                Duration::from_secs(secs.min(10)),
                "request_timeout_seconds={secs}"
            );
            assert!(client.completion_timeout() <= client.request_timeout());
        }
    }

    #[test]
    fn test_request_timeout_clamps_zero_to_one_second() {
        let mut config = LspServerConfig::rust_analyzer();
        config.request_timeout_seconds = 0;
        let client = LspClient::new(config);

        assert_eq!(client.request_timeout(), Duration::from_secs(1));
        assert_eq!(client.completion_timeout(), Duration::from_secs(1));
    }

    #[test]
    fn test_request_timeout_independent_per_server() {
        let mut config_a = LspServerConfig::rust_analyzer();
        config_a.request_timeout_seconds = 5;
        let mut config_b = LspServerConfig::pyright();
        config_b.request_timeout_seconds = 15;

        let client_a = LspClient::new(config_a);
        let client_b = LspClient::new(config_b);

        assert_eq!(client_a.request_timeout(), Duration::from_secs(5));
        assert_eq!(client_b.request_timeout(), Duration::from_secs(15));
    }

    #[test]
    fn test_register_capability_request_is_acknowledged() {
        let request = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: RequestId::String("ts1".to_string()),
            method: "client/registerCapability".to_string(),
            params: Some(serde_json::json!({ "registrations": [] })),
        };

        let response = LspClient::server_request_response(request);

        assert_eq!(response.id, RequestId::String("ts1".to_string()));
        assert_eq!(response.result, Some(Value::Null));
        assert!(response.error.is_none());
    }

    #[test]
    fn test_workspace_configuration_request_returns_null_per_item() {
        let result = LspClient::workspace_configuration_result(Some(&serde_json::json!({
            "items": [{ "section": "typescript" }, { "section": "editor" }]
        })));

        assert_eq!(result, serde_json::json!([null, null]));
    }

    #[test]
    fn test_unknown_server_request_returns_method_not_found() {
        let request = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: RequestId::String("unknown-1".to_string()),
            method: "custom/request".to_string(),
            params: None,
        };

        let response = LspClient::server_request_response(request);

        assert!(response.result.is_none());
        match response.error {
            Some(error) => {
                assert_eq!(error.code, -32601);
                assert_eq!(error.message, "Unhandled server request: custom/request");
            }
            None => panic!("unknown request should return error"),
        }
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
        if let Some(sender) = sender
            && let Some(error) = error_response.error
        {
            let _ = sender.send(Err(Error::LspServerError {
                code: error.code,
                message: error.message,
                data: error.data,
            }));
        }

        let result = response_rx.await.unwrap();
        assert!(result.is_err(), "Should receive error");

        if let Err(Error::LspServerError { code, message, .. }) = result {
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
        if let Some(sender) = sender
            && let Some(error) = error_response.error
        {
            let _ = sender.send(Err(Error::LspServerError {
                code: error.code,
                message: error.message,
                data: error.data,
            }));
        }

        let result = response_rx.await.unwrap();
        assert!(result.is_err());

        if let Err(Error::LspServerError { code, message, .. }) = result {
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

    /// #239 regression: a request that times out must remove its own entry
    /// from `pending_requests` instead of leaking it. `sleep` is used as the
    /// "server": it never writes anything to stdout, so no response can ever
    /// arrive and the request is guaranteed to time out rather than race a
    /// real answer.
    ///
    /// Unix-only: spawns a real `sleep` subprocess, which is unavailable on
    /// the Windows CI runner.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_request_timeout_removes_pending_entry() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("2")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let transport = LspTransport::new(stdin, stdout);
        let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);

        let result: Result<Value> = client
            .request(
                "textDocument/hover",
                serde_json::json!({}),
                Duration::from_millis(50),
            )
            .await;

        assert!(matches!(result, Err(Error::Timeout(_))), "got {result:?}");
        assert!(
            client.pending_requests.lock().await.is_empty(),
            "timed-out request must not remain in pending_requests"
        );
    }

    /// #249 continuation: a client about to be discarded (e.g. superseded by
    /// a respawned replacement) must fail every still-pending request
    /// immediately rather than leaving callers to wait out their timeout.
    #[tokio::test]
    async fn test_fail_pending_requests_resolves_all_as_server_terminated() {
        let pending_requests: Arc<Mutex<PendingRequests>> = Arc::new(Mutex::new(HashMap::new()));
        let (command_tx, _command_rx) = mpsc::channel(1);

        let client = LspClient {
            config: LspServerConfig::rust_analyzer(),
            state: Arc::new(Mutex::new(super::super::ServerState::Ready)),
            request_counter: Arc::new(AtomicI64::new(1)),
            command_tx,
            pending_requests: Arc::clone(&pending_requests),
            receiver_task: None,
        };

        let (tx1, rx1) = oneshot::channel::<Result<Value>>();
        let (tx2, rx2) = oneshot::channel::<Result<Value>>();
        pending_requests
            .lock()
            .await
            .insert(RequestId::Number(1), tx1);
        pending_requests
            .lock()
            .await
            .insert(RequestId::Number(2), tx2);

        client.fail_pending_requests().await;

        assert!(pending_requests.lock().await.is_empty());
        assert!(matches!(rx1.await.unwrap(), Err(Error::ServerTerminated)));
        assert!(matches!(rx2.await.unwrap(), Err(Error::ServerTerminated)));
    }

    #[test]
    fn test_should_retrigger_defaults_to_true_when_data_absent() {
        assert!(LspClient::should_retrigger(None));
    }

    #[test]
    fn test_should_retrigger_false_when_flag_false() {
        assert!(!LspClient::should_retrigger(Some(&serde_json::json!({
            "retriggerRequest": false
        }))));
    }

    #[test]
    fn test_should_retrigger_true_when_flag_true() {
        assert!(LspClient::should_retrigger(Some(&serde_json::json!({
            "retriggerRequest": true
        }))));
    }

    mod retry_behavior {
        use std::process::Stdio;

        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::process::{Child, ChildStdin, ChildStdout, Command};

        use super::*;
        use crate::config::LspServerConfig;

        struct FakeServer {
            _write_half: Child,
            _read_half: Child,
            read_half_stdin: ChildStdin,
            write_stdout: ChildStdout,
        }

        fn fake_lsp_client() -> (LspClient, FakeServer) {
            let mut write_half = Command::new("cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let write_stdin = write_half.stdin.take().unwrap();
            let write_stdout = write_half.stdout.take().unwrap();

            let mut read_half = Command::new("cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let read_stdout = read_half.stdout.take().unwrap();
            let read_stdin = read_half.stdin.take().unwrap();

            let transport = LspTransport::new(write_stdin, read_stdout);
            let client = LspClient::from_transport(LspServerConfig::rust_analyzer(), transport);

            (
                client,
                FakeServer {
                    _write_half: write_half,
                    _read_half: read_half,
                    read_half_stdin: read_stdin,
                    write_stdout,
                },
            )
        }

        /// Reads one `Content-Length`-framed JSON-RPC message off `reader`.
        async fn read_framed_message(reader: &mut BufReader<&mut ChildStdout>) -> Value {
            let mut content_length = None;
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" || line == "\n" {
                    break;
                }
                if let Some((key, value)) = line.trim_end().split_once(':')
                    && key.trim().eq_ignore_ascii_case("content-length")
                {
                    content_length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut buf = vec![0u8; content_length.unwrap()];
            reader.read_exact(&mut buf).await.unwrap();
            serde_json::from_slice(&buf).unwrap()
        }

        /// Writes a framed JSON-RPC `ServerCancelled` (-32802) error response.
        async fn write_server_cancelled_response(
            stdin: &mut ChildStdin,
            id: &Value,
            retrigger: bool,
        ) {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": SERVER_CANCELLED_CODE,
                    "message": "server cancelled the request",
                    "data": { "retriggerRequest": retrigger },
                },
            });
            let content = serde_json::to_string(&response).unwrap();
            let header = format!("Content-Length: {}\r\n\r\n", content.len());
            stdin.write_all(header.as_bytes()).await.unwrap();
            stdin.write_all(content.as_bytes()).await.unwrap();
            stdin.flush().await.unwrap();
        }

        /// Writes a framed JSON-RPC success response.
        async fn write_success_response(stdin: &mut ChildStdin, id: &Value, result: Value) {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            });
            let content = serde_json::to_string(&response).unwrap();
            let header = format!("Content-Length: {}\r\n\r\n", content.len());
            stdin.write_all(header.as_bytes()).await.unwrap();
            stdin.write_all(content.as_bytes()).await.unwrap();
            stdin.flush().await.unwrap();
        }

        // Not `start_paused`: the retry loop's real backoff sleeps
        // interleave with real subprocess pipe I/O below, and paused
        // virtual time does not reliably auto-advance across both.
        #[tokio::test]
        async fn test_retry_exhaustion_returns_original_server_cancelled_error() {
            let (client, mut server) = fake_lsp_client();

            let request_task = tokio::spawn(async move {
                client
                    .request::<_, Value>(
                        "textDocument/hover",
                        serde_json::json!({}),
                        Duration::from_secs(30),
                    )
                    .await
            });

            let mut reader = BufReader::new(&mut server.write_stdout);
            // Initial attempt plus SERVER_CANCELLED_MAX_RETRIES retries: every
            // attempt gets ServerCancelled, so retries must exhaust rather
            // than loop forever or swallow the error.
            for _ in 0..=SERVER_CANCELLED_MAX_RETRIES {
                let request = read_framed_message(&mut reader).await;
                let id = request["id"].clone();
                write_server_cancelled_response(&mut server.read_half_stdin, &id, true).await;
            }

            let result = request_task.await.unwrap();

            match result {
                Err(Error::LspServerError {
                    code,
                    message,
                    data,
                }) => {
                    // Assert the exact original error surfaces, not merely
                    // "some error with this code" -- a freshly constructed
                    // placeholder error would satisfy a code-only check.
                    assert_eq!(code, SERVER_CANCELLED_CODE);
                    assert_eq!(message, "server cancelled the request");
                    assert_eq!(data, Some(serde_json::json!({ "retriggerRequest": true })));
                }
                other => panic!("expected exhausted ServerCancelled error, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn test_retrigger_false_returns_immediately_without_retry() {
            let (client, mut server) = fake_lsp_client();

            let request_task = tokio::spawn(async move {
                client
                    .request::<_, Value>(
                        "textDocument/hover",
                        serde_json::json!({}),
                        Duration::from_secs(30),
                    )
                    .await
            });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;
            let id = request["id"].clone();
            write_server_cancelled_response(&mut server.read_half_stdin, &id, false).await;

            // With `retriggerRequest: false`, `should_retrigger`'s gate on
            // the retry branch must short-circuit the loop: the error
            // returns well under the first 500ms backoff, and no second
            // request is ever sent. If the `&& Self::should_retrigger(..)`
            // guard were ever dropped from the retry match arm, this would
            // instead retry and both assertions below would fail.
            let result = tokio::time::timeout(Duration::from_millis(200), request_task)
                .await
                .unwrap()
                .unwrap();

            match result {
                Err(Error::LspServerError { code, .. }) => {
                    assert_eq!(code, SERVER_CANCELLED_CODE);
                }
                other => panic!("expected immediate ServerCancelled error, got {other:?}"),
            }

            let second_request =
                tokio::time::timeout(Duration::from_millis(200), read_framed_message(&mut reader))
                    .await;
            assert!(
                second_request.is_err(),
                "no retry should have been sent after retriggerRequest: false"
            );
        }

        #[tokio::test]
        async fn test_retry_succeeds_after_one_server_cancelled_response() {
            let (client, mut server) = fake_lsp_client();

            let request_task = tokio::spawn(async move {
                client
                    .request::<_, Value>(
                        "textDocument/hover",
                        serde_json::json!({}),
                        Duration::from_secs(30),
                    )
                    .await
            });

            let mut reader = BufReader::new(&mut server.write_stdout);

            // First attempt is cancelled and must retrigger.
            let first = read_framed_message(&mut reader).await;
            write_server_cancelled_response(
                &mut server.read_half_stdin,
                &first["id"].clone(),
                true,
            )
            .await;

            // Second attempt (after backoff) succeeds -- proves the loop
            // genuinely re-sends the request rather than just counting down.
            let second = read_framed_message(&mut reader).await;
            assert_ne!(
                first["id"], second["id"],
                "retry must use a fresh request id"
            );
            let expected_result = serde_json::json!({ "contents": "resolved on retry" });
            write_success_response(
                &mut server.read_half_stdin,
                &second["id"].clone(),
                expected_result.clone(),
            )
            .await;

            let result = request_task.await.unwrap();
            assert_eq!(result.unwrap(), expected_result);
        }
    }
}
