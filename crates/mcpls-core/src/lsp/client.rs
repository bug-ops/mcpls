//! LSP client implementation with async request/response handling.

use crate::config::LspServerConfig;
use crate::error::{Error, Result};
use crate::lsp::transport::LspTransport;
use crate::lsp::types::{InboundMessage, JsonRpcRequest, RequestId};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tracing::{debug, error, trace, warn};

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

/// Commands for client control.
enum ClientCommand {
    /// Send a request and wait for response.
    SendRequest {
        request: JsonRpcRequest,
        response_tx: oneshot::Sender<Result<Value>>,
    },
    /// Send a notification (no response expected).
    SendNotification { method: String, params: Option<Value> },
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
        let (command_tx, _command_rx) = mpsc::channel(100);

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
    pub(crate) fn from_transport(
        config: LspServerConfig,
        transport: LspTransport,
    ) -> Self {
        let state = Arc::new(Mutex::new(super::ServerState::Initializing));
        let request_counter = Arc::new(AtomicI64::new(1));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));

        let (command_tx, command_rx) = mpsc::channel(100);

        let receiver_task = tokio::spawn(Self::message_loop(
            transport,
            command_rx,
            pending_requests,
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
            jsonrpc: "2.0".to_string(),
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
            .map_err(|e| Error::LspProtocolError(format!("Failed to deserialize response: {}", e)))
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
                .map_err(|e| Error::Transport(format!("Receiver task failed: {}", e)))??;
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
        pending_requests: Arc<Mutex<HashMap<RequestId, oneshot::Sender<Result<Value>>>>>,
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
                    let message = message?;
                    match message {
                        InboundMessage::Response(response) => {
                            trace!("Received response: id={:?}", response.id);

                            let sender = pending_requests.lock().await.remove(&response.id);

                            if let Some(sender) = sender {
                                if let Some(error) = response.error {
                                    error!("LSP error response: {} (code {})", error.message, error.code);
                                    let _ = sender.send(Err(Error::LspServerError {
                                        code: error.code,
                                        message: error.message,
                                    }));
                                } else if let Some(result) = response.result {
                                    let _ = sender.send(Ok(result));
                                } else {
                                    warn!("Response with neither result nor error: {:?}", response.id);
                                }
                            } else {
                                warn!("Received response for unknown request ID: {:?}", response.id);
                            }
                        }
                        InboundMessage::Notification(notification) => {
                            debug!("Received notification: {}", notification.method);
                            // TODO: Handle server notifications (diagnostics, etc.)
                            // For Phase 2, just log and ignore
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
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
}
