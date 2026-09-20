//! LSP client implementation with async request/response handling.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use lsp_types::LspErrorCodes;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};
use tracing::{debug, error, trace, warn};

use crate::config::LspServerConfig;
use crate::error::{Error, Result};
use crate::lsp::transport::{LspTransport, LspTransportReader};
use crate::lsp::types::{
    InboundMessage, JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    LspNotification, RequestId,
};

/// JSON-RPC protocol version.
const JSONRPC_VERSION: &str = "2.0";

/// LSP error code returned when the server cancels a request and wants the client to retry.
const SERVER_CANCELLED_CODE: i32 = -32802;

/// Maximum number of retry attempts for server-cancelled requests.
const SERVER_CANCELLED_MAX_RETRIES: u32 = 3;

/// Initial backoff delay for server-cancelled retries (milliseconds).
const SERVER_CANCELLED_INITIAL_DELAY_MS: u64 = 500;

/// Bounded capacity for the channel carrying fully-decoded inbound LSP
/// messages from [`spawn_reader_task`]'s background task to
/// [`LspClient::message_loop_inner`].
///
/// Backpressured (`send().await`, not `try_send`) unlike the best-effort
/// notification/lifecycle lanes: dropping a frame here would desync
/// request/response correlation or silently swallow a server-initiated
/// request. Matches the command channel's capacity -- both lanes carry
/// protocol-critical traffic at a similar cadence.
const READER_CHANNEL_CAPACITY: usize = 100;

/// How long [`LspClient::message_loop`] waits, after aborting the reader
/// task it no longer drains, for that task to actually finish dropping its
/// [`LspTransportReader`] (and the `ChildStdout` it owns).
///
/// `abort()` only requests cancellation -- the task's locals are dropped
/// once the runtime next polls it, not synchronously at the call site.
/// Mirrors `await_lsp_init_handle`'s reasoning in `crate::lib` for the same
/// pattern. Short: the task is either already parked in a cancel-safe
/// `.await` (aborts promptly) or has nothing left to do.
const READER_TASK_ABORT_GRACE: Duration = Duration::from_secs(1);

/// LSP request methods for which a `-32801` (`ContentModified`) error
/// response is safe to retry automatically -- also declared to servers via
/// `general.staleRequestSupport.retryOnContentModified` during initialize
/// (see [`crate::lsp::LspServer`]'s handshake).
///
/// Per the LSP spec, `ContentModified` means the server noticed the document
/// changed while it was computing a response; the (possibly stale) result may
/// still be useful, or the client may choose to cancel the request instead.
/// mcpls chooses to retry, which is safe for a read-only/idempotent request
/// (hover, references, diagnostics, ...): a stale response is simply
/// discarded and superseded by a fresh one.
///
/// Deliberately excludes every method in
/// `crate::bridge::translator::edits` (`textDocument/rename`,
/// `textDocument/formatting`, `textDocument/codeAction`): their result is an
/// edit the MCP caller applies, and `-32801` means the document changed since
/// the request was issued, so a retry at the original position could return
/// an edit for content the caller no longer expects (e.g. renaming a
/// different symbol than the one originally at that position).
///
/// This list only gates `-32801`. `-32802` (`ServerCancelled`) retry is
/// unaffected and keeps retrying unconditionally for every method, as before.
pub const CONTENT_MODIFIED_RETRY_METHODS: &[&str] = &[
    "textDocument/signatureHelp",
    "textDocument/inlayHint",
    "textDocument/completion",
    "textDocument/prepareCallHierarchy",
    "callHierarchy/incomingCalls",
    "callHierarchy/outgoingCalls",
    "textDocument/diagnostic",
    "textDocument/hover",
    "textDocument/definition",
    "textDocument/references",
    "textDocument/implementation",
    "textDocument/typeDefinition",
    "textDocument/documentSymbol",
    "workspace/symbol",
];

/// Byte-length threshold for truncating an LSP error message before logging it.
///
/// Kept short since this feeds a single log line in [`LspClient::request`]
/// -- `warn!` while a transient error is being retried, `error!` once it is
/// actually surfaced to the caller -- not the MCP caller itself; see
/// `MAX_ERROR_MESSAGE_CALLER_BYTES` for that budget.
const MAX_ERROR_MESSAGE_LOG_BYTES: usize = 200;

/// Byte-length threshold for the LSP error message forwarded to the MCP
/// caller in [`Error::LspServerError`] (#313).
///
/// Deliberately much larger than `MAX_ERROR_MESSAGE_LOG_BYTES`: a
/// legitimate LSP error (e.g. a verbose rust-analyzer type-mismatch
/// diagnostic reported through an error response) can run into the low
/// kilobytes, and that detail is useful to the calling model -- a log line
/// should stay terse, but a truncated-to-200-bytes error handed to the
/// model would cut off real content on every longer-but-honest error. Still
/// far below #311's 256 KiB cache-entry cap: this string is echoed directly
/// into the MCP tool result / model context, not merely cached.
const MAX_ERROR_MESSAGE_CALLER_BYTES: usize = 4 * 1024;

/// Upper bound on the effective timeout for completion requests, regardless
/// of `request_timeout_seconds`.
///
/// Completions are latency-sensitive: a completion list that takes longer
/// than this is no longer useful to the caller. This is a deliberate MVP
/// ceiling, not an oversight — completions cannot be configured above this
/// value today. See [`LspClient::completion_timeout`].
const COMPLETION_TIMEOUT_CAP: Duration = Duration::from_secs(10);

/// Upper bound on the effective timeout for a single `codeAction/resolve`
/// request, regardless of `request_timeout_seconds`.
///
/// `handle_code_actions` (`bridge::translator::edits`) resolves up to
/// `MAX_CODE_ACTION_RESOLVES` deferred actions concurrently after the
/// initial `textDocument/codeAction` response; an uncapped per-resolve
/// timeout would let a large `request_timeout_seconds` configuration make
/// one tool call wait far longer than a caller expects for what is meant to
/// be a best-effort follow-up. Mirrors [`COMPLETION_TIMEOUT_CAP`]'s
/// reasoning. See [`LspClient::code_action_resolve_timeout`].
const CODE_ACTION_RESOLVE_TIMEOUT_CAP: Duration = Duration::from_secs(10);

/// Type alias for pending request tracking map.
type PendingRequests = HashMap<RequestId, oneshot::Sender<Result<Value>>>;

/// Spawns the dedicated background task that owns `reader` exclusively and
/// decodes inbound LSP frames in a loop, handing each one to
/// [`LspClient::message_loop_inner`] over the returned channel.
///
/// This is the fix for #451: [`LspTransportReader::receive`] is not
/// cancel-safe, so it must never run as a branch of the `select!` in
/// `message_loop_inner`, which also waits on `command_rx`. Running it here
/// instead, on a task driven only by its own `.await`s, means it can never
/// be cancelled mid-frame.
///
/// The task exits after sending one `Err` (I/O failure or EOF), once
/// `message_loop_inner` drops its end of the channel (e.g. on shutdown), or
/// when the returned [`JoinHandle`] is aborted. Callers must abort it once
/// they stop draining the channel -- see [`LspClient::message_loop`], the
/// only caller -- otherwise it stays parked in a blocking read holding the
/// underlying `ChildStdout` open indefinitely: not itself a correctness bug
/// (`has_exited()`, lifecycle.rs, checks the child process directly via
/// `try_wait()` and doesn't care whether we still hold its stdout open), but
/// a leaked task and file descriptor for the lifetime of that connection.
fn spawn_reader_task(
    mut reader: LspTransportReader,
) -> (JoinHandle<()>, mpsc::Receiver<Result<InboundMessage>>) {
    let (tx, rx) = mpsc::channel(READER_CHANNEL_CAPACITY);
    let handle = tokio::spawn(async move {
        loop {
            let message = reader.receive().await;
            let is_err = message.is_err();
            if tx.send(message).await.is_err() || is_err {
                break;
            }
        }
    });
    (handle, rx)
}

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
    SendRequest { request: JsonRpcRequest },
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
    pub(crate) fn from_transport(
        config: LspServerConfig,
        transport: (LspTransport, LspTransportReader),
    ) -> Self {
        let state = Arc::new(Mutex::new(super::ServerState::Initializing));
        let request_counter = Arc::new(AtomicI64::new(1));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));

        let (command_tx, command_rx) = mpsc::channel(100);

        let receiver_task = tokio::spawn(Self::message_loop(
            transport,
            command_rx,
            Arc::clone(&pending_requests),
            None,
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
    /// Notifications are parsed and split across two lanes (P3): diagnostics/
    /// log/showMessage go through `notification_tx`; `$/progress` `begin`/
    /// `end` frames and unrecognized notifications (`LspNotification::Other`,
    /// which carries rust-analyzer's `experimental/serverStatus`) go through
    /// `lifecycle_tx` instead. A `$/progress` `report` frame is never
    /// enqueued on either lane -- see [`Self::message_loop_inner`].
    pub(crate) fn from_transport_with_notifications(
        config: LspServerConfig,
        transport: (LspTransport, LspTransportReader),
        notification_tx: mpsc::Sender<LspNotification>,
        lifecycle_tx: mpsc::Sender<LspNotification>,
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
            Some(lifecycle_tx),
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
    /// `-32802` (`ServerCancelled`) or `-32801` (`ContentModified`) response,
    /// sharing one attempt budget between the two codes, so the worst-case
    /// latency for a single tool call is `4 * request_timeout() + 3.5s` (the
    /// sum of the retry backoff delays).
    ///
    /// Exception: `get_code_actions` (`bridge::translator::edits`) can add a
    /// second, concurrent round of requests on top of this bound -- up to
    /// `MAX_CODE_ACTION_RESOLVES` `codeAction/resolve` calls, each retried
    /// under the same rules but bounded by [`Self::code_action_resolve_timeout`]
    /// rather than this timeout. Since those run concurrently with each
    /// other (not with the initial `textDocument/codeAction` request), the
    /// worst case for that one tool call is
    /// `(4 * request_timeout() + 3.5s) + (4 * code_action_resolve_timeout() + 3.5s)`,
    /// not a multiple scaling with the number of resolved actions.
    ///
    /// The configured value is clamped to the range from 1 second to
    /// [`MAX_TIMEOUT_SECONDS`]. [`crate::serve`]/[`crate::serve_with`] now
    /// validate the top-level `ServerConfig` (via [`ServerConfig::validate`],
    /// which rejects `request_timeout_seconds` that is `0` or greater than
    /// [`MAX_TIMEOUT_SECONDS`]) regardless of whether it came from
    /// [`ServerConfig::load_from`] or was built programmatically by the
    /// caller. But `Self::new`, [`super::LspServer::spawn`], and
    /// [`super::LspServer::spawn_batch`] are all `pub` and take an
    /// [`LspServerConfig`] (or [`super::ServerInitConfig`] wrapping one)
    /// directly, bypassing that top-level validation entirely — it operates
    /// on the top-level `ServerConfig`, not the per-server one. This clamp is
    /// the last line of defense against a zero-duration timeout that would
    /// fail every request instantly, or an astronomically large one that
    /// tokio's `timeout`/`sleep` would silently treat as unbounded (they fall
    /// back to `Instant::far_future()` rather than panicking), for a caller
    /// reaching either of these levels directly.
    ///
    /// [`ServerConfig::load_from`]: crate::config::ServerConfig::load_from
    /// [`ServerConfig::validate`]: crate::config::ServerConfig::validate
    /// [`MAX_TIMEOUT_SECONDS`]: crate::config::MAX_TIMEOUT_SECONDS
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
        Duration::from_secs(
            self.config
                .request_timeout_seconds
                .clamp(1, crate::config::MAX_TIMEOUT_SECONDS),
        )
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

    /// The timeout applied to a single `codeAction/resolve` request.
    ///
    /// Equal to [`Self::request_timeout`], capped at 10 seconds.
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
    /// assert_eq!(client.code_action_resolve_timeout(), Duration::from_secs(10));
    /// assert!(client.code_action_resolve_timeout() <= client.request_timeout());
    /// ```
    #[must_use]
    pub fn code_action_resolve_timeout(&self) -> Duration {
        self.request_timeout().min(CODE_ACTION_RESOLVE_TIMEOUT_CAP)
    }

    /// Registers before enqueuing the send command (not before the message
    /// loop's own transport read), cleaning up the entry if the send fails.
    async fn register_and_send_request(
        &self,
        request: JsonRpcRequest,
        response_tx: oneshot::Sender<Result<Value>>,
    ) -> Result<()> {
        let id = request.id.clone();

        self.pending_requests
            .lock()
            .await
            .insert(id.clone(), response_tx);

        if self
            .command_tx
            .send(ClientCommand::SendRequest { request })
            .await
            .is_err()
        {
            self.pending_requests.lock().await.remove(&id);
            return Err(Error::ServerTerminated);
        }

        Ok(())
    }

    /// Send request and wait for response with timeout.
    ///
    /// Automatically retries up to 3 times when the server returns error code
    /// -32802 (`ServerCancelled`, any method) or -32801 (`ContentModified`,
    /// only for methods in `CONTENT_MODIFIED_RETRY_METHODS`) -- gated by
    /// `data.retriggerRequest` when present -- using exponential backoff
    /// starting at 500 ms. Both codes share the same attempt budget: a
    /// request that hits -32801 then -32802 does not get 8 attempts, only 4.
    ///
    /// This method owns the severity of LSP error-response logging (#392):
    /// a transient error about to be retried logs at `warn!`, while `error!`
    /// is reserved for an error actually surfaced to the caller -- retry
    /// exhaustion or a non-retryable code/method combination. This is
    /// deliberately not decided in `message_loop_inner`, which parses the
    /// response before knowing whether a retry will follow.
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
        let params_value = Self::omit_null_params(serde_json::to_value(params)?);
        let mut delay_ms = SERVER_CANCELLED_INITIAL_DELAY_MS;

        for attempt in 0..=SERVER_CANCELLED_MAX_RETRIES {
            if attempt > 0 {
                debug!(
                    "Retrying {} (attempt {}/{}), backoff={}ms",
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
                params: params_value.clone(),
            };

            debug!("Sending request: {} (id={:?})", method, id);

            self.register_and_send_request(request, response_tx).await?;

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
                    message,
                    data,
                }) if (code == SERVER_CANCELLED_CODE
                    || (LspErrorCodes::from(code) == LspErrorCodes::ContentModified
                        && CONTENT_MODIFIED_RETRY_METHODS.contains(&method)))
                    && Self::should_retrigger(data.as_ref()) =>
                {
                    if attempt == SERVER_CANCELLED_MAX_RETRIES {
                        // Same "LSP error response: ..." prefix as the
                        // non-retryable branch below -- a log-grep alert on
                        // that prefix must catch every error actually
                        // surfaced to the caller, retry exhaustion included.
                        error!(
                            "LSP error response: {} (code {}) on '{}' (id={:?}), retries exhausted",
                            Self::truncate_error_message_for_log(&message),
                            code,
                            method,
                            id
                        );
                        return Err(Error::LspServerError {
                            code,
                            message,
                            data,
                        });
                    }
                    warn!(
                        "LSP error response: {} (code {}) on '{}' (id={:?}), will retry",
                        Self::truncate_error_message_for_log(&message),
                        code,
                        method,
                        id
                    );
                    // continue loop for next attempt
                }
                Err(Error::LspServerError {
                    code,
                    message,
                    data,
                }) => {
                    error!(
                        "LSP error response: {} (code {}) on '{}' (id={:?})",
                        Self::truncate_error_message_for_log(&message),
                        code,
                        method,
                        id
                    );
                    return Err(Error::LspServerError {
                        code,
                        message,
                        data,
                    });
                }
                Err(e) => return Err(e),
            }
        }

        Err(Error::ServerTerminated)
    }

    /// Send a typed LSP request, deriving both the method string and the
    /// result type from `R`'s [`lsp_types::Request`] implementation so they
    /// cannot drift independently -- unlike [`Self::request`], which takes
    /// the method string and result type as two unchecked, hand-picked
    /// values.
    ///
    /// # Errors
    ///
    /// See [`Self::request`].
    pub async fn request_typed<R>(
        &self,
        params: R::Params,
        timeout_duration: Duration,
    ) -> Result<R::Result>
    where
        R: lsp_types::Request,
    {
        self.request(R::METHOD.as_str(), params, timeout_duration)
            .await
    }

    /// Returns true when the error data from a retryable "please retry" LSP
    /// response (`ServerCancelled` -32802 or `ContentModified` -32801)
    /// indicates a retry should happen.
    ///
    /// The LSP spec defines `data.retriggerRequest` only for diagnostic
    /// requests' `ServerCancelled` responses (`DiagnosticServerCancellationData`)
    /// -- it is not part of the general `ServerCancelled` or `ContentModified`
    /// contract for other methods. mcpls checks this field whenever it is
    /// present regardless of method (harmless for non-diagnostic methods,
    /// since a compliant server won't send it there) and defaults to
    /// retrying when the field is absent, since retrying is the more useful
    /// default for a request that would otherwise surface as a hard error to
    /// the MCP caller.
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
        let params_value = Self::omit_null_params(serde_json::to_value(params)?);

        debug!("Sending notification: {}", method);

        self.command_tx
            .send(ClientCommand::SendNotification {
                method: method.to_string(),
                params: params_value,
            })
            .await
            .map_err(|_| Error::ServerTerminated)?;

        Ok(())
    }

    /// Send a typed LSP notification, deriving the method string from `N`'s
    /// [`lsp_types::Notification`] implementation so it cannot drift from the
    /// params type -- the notification counterpart to [`Self::request_typed`].
    ///
    /// # Errors
    ///
    /// See [`Self::notify`].
    pub async fn notify_typed<N>(&self, params: N::Params) -> Result<()>
    where
        N: lsp_types::Notification,
    {
        self.notify(N::METHOD.as_str(), params).await
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
        transport: (LspTransport, LspTransportReader),
        mut command_rx: mpsc::Receiver<ClientCommand>,
        pending_requests: Arc<Mutex<PendingRequests>>,
        notification_tx: Option<mpsc::Sender<LspNotification>>,
        lifecycle_tx: Option<mpsc::Sender<LspNotification>>,
    ) -> Result<()> {
        debug!("Message loop started");
        let (mut transport, reader) = transport;
        let (reader_handle, mut msg_rx) = spawn_reader_task(reader);
        let result = {
            // Aborts the reader task the instant `message_loop_inner` returns, even via `?` (#451).
            let _abort_reader_on_drop = crate::AbortOnDrop(&reader_handle);
            Self::message_loop_inner(
                &mut transport,
                &mut msg_rx,
                &mut command_rx,
                &pending_requests,
                notification_tx.as_ref(),
                lifecycle_tx.as_ref(),
            )
            .await
        };
        let _ = timeout(READER_TASK_ABORT_GRACE, reader_handle).await;
        if let Err(ref e) = result {
            error!("Message loop exiting with error: {}", e);
        } else {
            debug!("Message loop exiting normally");
        }
        result
    }

    /// Maps a `null` params value to an omitted `params` field. LSP methods
    /// with `params: void` (`shutdown`, `exit`) must go out without the key:
    /// tsgo rejects `"params": null` with `-32602 expected empty, got: null`.
    fn omit_null_params(params: Value) -> Option<Value> {
        if params.is_null() { None } else { Some(params) }
    }

    /// Truncate an LSP server's error message for the `tracing::error!` log
    /// line, bounding it to at most [`MAX_ERROR_MESSAGE_LOG_BYTES`] bytes
    /// (the full formatted string is slightly longer).
    ///
    /// Log-line use only -- the message forwarded to the MCP caller in
    /// [`Error::LspServerError`] is truncated separately, to the larger
    /// [`MAX_ERROR_MESSAGE_CALLER_BYTES`] (#313).
    fn truncate_error_message_for_log(message: &str) -> String {
        crate::util::truncate_str(message, MAX_ERROR_MESSAGE_LOG_BYTES)
    }

    /// Which of the two notification lanes (P3) `notification` belongs on,
    /// or `None` if it must be dropped before reaching either.
    ///
    /// `notification_tx` (returned as `"notification"`) carries diagnostics/
    /// log/showMessage; `lifecycle_tx` (returned as `"lifecycle"`) carries
    /// `$/progress` `begin`/`end` frames and `Other` (which carries e.g.
    /// rust-analyzer's `experimental/serverStatus`) -- splitting them means
    /// a high-volume diagnostics publisher can never starve out a
    /// low-volume readiness signal, or vice versa. The lane name is
    /// returned alongside the sender purely for the drop-warning log at the
    /// call site (Fix 6) -- it plays no role in routing.
    ///
    /// A `$/progress` `report` frame -- the high-volume case a
    /// `report`-per-package emitter like gopls can produce -- is classified
    /// via [`crate::lsp::types::ProgressKind::from_value`] (shared with
    /// `bridge::indexing::IndexingTracker::observe_progress` so the two
    /// can't drift out of sync on which `kind`s are recognized -- Fix 9)
    /// and dropped before it ever reaches either channel (S3). An
    /// *unparseable* `$/progress` notification
    /// (e.g. missing the mandatory `token` field) falls back to
    /// `LspNotification::Other { method: "$/progress", .. }` at parse time
    /// (`LspNotification::parse`) rather than `Progress`, so it must be
    /// dropped here too by matching on `method` -- otherwise a server whose
    /// `report` payloads fail to deserialize could bypass the `kind`-based
    /// filter above entirely by sending malformed frames (security LOW /
    /// M1).
    fn notification_lane<'a>(
        notification: &LspNotification,
        notification_tx: Option<&'a mpsc::Sender<LspNotification>>,
        lifecycle_tx: Option<&'a mpsc::Sender<LspNotification>>,
    ) -> Option<(&'static str, &'a mpsc::Sender<LspNotification>)> {
        match notification {
            LspNotification::PublishDiagnostics(_)
            | LspNotification::LogMessage(_)
            | LspNotification::ShowMessage(_) => notification_tx.map(|tx| ("notification", tx)),
            LspNotification::Progress(params) => {
                crate::lsp::types::ProgressKind::from_value(&params.value)
                    .and(lifecycle_tx)
                    .map(|tx| ("lifecycle", tx))
            }
            LspNotification::Other { method, .. } if method.as_ref() == "$/progress" => None,
            LspNotification::Other { .. } => lifecycle_tx.map(|tx| ("lifecycle", tx)),
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn message_loop_inner(
        transport: &mut LspTransport,
        msg_rx: &mut mpsc::Receiver<Result<InboundMessage>>,
        command_rx: &mut mpsc::Receiver<ClientCommand>,
        pending_requests: &Arc<Mutex<PendingRequests>>,
        notification_tx: Option<&mpsc::Sender<LspNotification>>,
        lifecycle_tx: Option<&mpsc::Sender<LspNotification>>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                Some(command) = command_rx.recv() => {
                    match command {
                        ClientCommand::SendRequest { request } => {
                            let value = serde_json::to_value(&request)?;
                            transport.send(&value).await?;
                        }
                        ClientCommand::SendNotification { method, params } => {
                            let notification = serde_json::to_value(JsonRpcNotification {
                                jsonrpc: JSONRPC_VERSION.to_string(),
                                method,
                                params,
                            })?;
                            transport.send(&notification).await?;
                        }
                        ClientCommand::Shutdown => {
                            debug!("Client shutdown requested");
                            // The reader task can run ahead of us (#451): drain whatever it
                            // already queued instead of dropping it, or a response to a
                            // concurrent caller's in-flight request would be silently lost.
                            while let Ok(message) = msg_rx.try_recv() {
                                match message {
                                    Ok(m) => {
                                        Self::handle_inbound_message(
                                            transport,
                                            m,
                                            pending_requests,
                                            notification_tx,
                                            lifecycle_tx,
                                        )
                                        .await?;
                                    }
                                    Err(e) => {
                                        error!("Transport receive error while draining on shutdown: {}", e);
                                        break;
                                    }
                                }
                            }
                            break;
                        }
                    }
                }

                // Cancel-safe, unlike the `transport.receive()` this replaces (#451): reading itself happens off this `select!`.
                message = msg_rx.recv() => {
                    let message = match message {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => {
                            error!("Transport receive error: {}", e);
                            return Err(e);
                        }
                        None => {
                            // Reader task only exits after sending an `Err`, so this means it panicked.
                            error!("Transport receive error: reader task ended unexpectedly");
                            return Err(Error::ServerTerminated);
                        }
                    };
                    Self::handle_inbound_message(
                        transport,
                        message,
                        pending_requests,
                        notification_tx,
                        lifecycle_tx,
                    )
                    .await?;
                }
            }
        }

        Ok(())
    }

    /// Processes one fully-decoded inbound LSP message: resolves a matching
    /// pending request, answers a server-initiated request, or forwards a
    /// notification to its lane. Shared by `message_loop_inner`'s normal
    /// `msg_rx.recv()` branch and its `Shutdown` drain, so a message handled
    /// during either path behaves identically.
    async fn handle_inbound_message(
        transport: &mut LspTransport,
        message: InboundMessage,
        pending_requests: &Arc<Mutex<PendingRequests>>,
        notification_tx: Option<&mpsc::Sender<LspNotification>>,
        lifecycle_tx: Option<&mpsc::Sender<LspNotification>>,
    ) -> Result<()> {
        match message {
            InboundMessage::Response(response) => {
                trace!("Received response: id={:?}", response.id);

                let sender = pending_requests.lock().await.remove(&response.id);

                if let Some(sender) = sender {
                    if let Some(error) = response.error {
                        // Deliberately not logged at `error!` here: this fires
                        // for every attempt, before `LspClient::request`'s retry
                        // loop knows whether the error is transient and about to
                        // be retried (-32802, or -32801 for an allowlisted
                        // method). Logging unconditionally at this point would
                        // emit a spurious ERROR line for errors that are retried
                        // and succeed. `request` logs at `warn!` on retry and
                        // `error!` once the error is actually surfaced to the
                        // caller (retry exhaustion or a non-retryable error);
                        // the response id is already traced above.
                        trace!(
                            "LSP error response: {} (code {})",
                            Self::truncate_error_message_for_log(&error.message),
                            error.code
                        );
                        // Truncated separately from the log line, to the larger
                        // MAX_ERROR_MESSAGE_CALLER_BYTES -- the raw message is
                        // unbounded and attacker-influenceable (#313), but a
                        // log-line-sized cut would also clip legitimate long
                        // errors before the model ever sees them (S2).
                        let caller_message = crate::util::truncate_str(
                            &error.message,
                            MAX_ERROR_MESSAGE_CALLER_BYTES,
                        );
                        let _ = sender.send(Err(Error::LspServerError {
                            code: error.code,
                            message: caller_message,
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
                    warn!(
                        "Received response for unknown request ID: {:?}",
                        response.id
                    );
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

                let destination = Self::notification_lane(&typed, notification_tx, lifecycle_tx);

                if let Some((lane, tx)) = destination {
                    // Log diagnostics count since it's useful for debugging
                    if let LspNotification::PublishDiagnostics(ref params) = typed {
                        debug!(
                            "Forwarding diagnostics for {}: {} items",
                            params.uri.as_ref(),
                            params.diagnostics.len()
                        );
                    } else {
                        trace!("Forwarding notification: {:?}", typed);
                    }

                    // Names lane and method -- the only diagnostic for a dropped frame.
                    if tx.try_send(typed).is_err() {
                        warn!(
                            "Dropping notification: lane={lane}, method={} \
                             (channel full or closed)",
                            notification.method
                        );
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
            | "window/showMessageRequest"
            | "window/workDoneProgress/create" => Ok(Value::Null),
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
    fn test_code_action_resolve_timeout_clamps_to_ten_seconds() {
        for secs in [1, 2, 3, 30, 300] {
            let mut config = LspServerConfig::rust_analyzer();
            config.request_timeout_seconds = secs;
            let client = LspClient::new(config);

            assert_eq!(
                client.code_action_resolve_timeout(),
                Duration::from_secs(secs.min(10)),
                "request_timeout_seconds={secs}"
            );
            assert!(client.code_action_resolve_timeout() <= client.request_timeout());
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
    fn test_request_timeout_clamps_above_max_to_max() {
        let mut config = LspServerConfig::rust_analyzer();
        config.request_timeout_seconds = u64::MAX;
        let client = LspClient::new(config);

        assert_eq!(
            client.request_timeout(),
            Duration::from_secs(crate::config::MAX_TIMEOUT_SECONDS)
        );
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

    /// P1: without this, no spec-compliant LSP server may ever initiate
    /// `$/progress` at all (per LSP 3.17, a server needs a successful
    /// `window/workDoneProgress/create` response before it may report
    /// server-initiated progress for a token).
    #[test]
    fn test_work_done_progress_create_request_is_acknowledged() {
        let request = JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: RequestId::String("wdp1".to_string()),
            method: "window/workDoneProgress/create".to_string(),
            params: Some(serde_json::json!({ "token": "indexing" })),
        };

        let response = LspClient::server_request_response(request);

        assert_eq!(response.result, Some(Value::Null));
        assert!(response.error.is_none());
    }

    /// S3 (Fix 7): a `report`-kind `$/progress` frame must never be
    /// enqueued on either notification lane -- the actual mechanism
    /// protecting against a `report`-per-package emitter like gopls
    /// overrunning the bounded lifecycle channel.
    #[test]
    fn test_report_progress_frame_reaches_neither_lane() {
        let (notification_tx, _notification_rx) = mpsc::channel(8);
        let (lifecycle_tx, _lifecycle_rx) = mpsc::channel(8);

        let report = LspNotification::Progress(lsp_types::ProgressParams {
            token: lsp_types::ProgressToken::Int(1),
            value: serde_json::json!({ "kind": "report", "percentage": 50 }),
        });

        let destination =
            LspClient::notification_lane(&report, Some(&notification_tx), Some(&lifecycle_tx));

        assert!(
            destination.is_none(),
            "a report-kind frame must be dropped before reaching either lane"
        );
    }

    /// Fix 3 / M1: an unparseable `$/progress` notification (e.g. missing
    /// the mandatory `token` field) falls back to `LspNotification::Other {
    /// method: "$/progress", .. }` at parse time -- this must be dropped
    /// the same as a well-formed `report` frame, not routed to the
    /// lifecycle lane, or a server whose payloads fail to deserialize could
    /// bypass the kind-based filter entirely.
    #[test]
    fn test_malformed_progress_other_reaches_neither_lane() {
        let (notification_tx, _notification_rx) = mpsc::channel(8);
        let (lifecycle_tx, _lifecycle_rx) = mpsc::channel(8);

        let malformed = LspNotification::Other {
            method: std::borrow::Cow::Borrowed("$/progress"),
            params: None,
        };

        let destination =
            LspClient::notification_lane(&malformed, Some(&notification_tx), Some(&lifecycle_tx));

        assert!(
            destination.is_none(),
            "a malformed $/progress frame must be dropped, not routed to the lifecycle lane"
        );
    }

    /// P3 sanity check alongside the two tests above: a `begin` frame and a
    /// genuine `Other` notification (e.g. rust-analyzer's
    /// `experimental/serverStatus`) must still reach the lifecycle lane --
    /// the report/malformed filters must not have overcorrected.
    #[test]
    fn test_begin_and_other_notifications_reach_lifecycle_lane() {
        let (notification_tx, _notification_rx) = mpsc::channel(8);
        let (lifecycle_tx, _lifecycle_rx) = mpsc::channel(8);

        let begin = LspNotification::Progress(lsp_types::ProgressParams {
            token: lsp_types::ProgressToken::Int(1),
            value: serde_json::json!({ "kind": "begin", "title": "Indexing" }),
        });
        assert_eq!(
            LspClient::notification_lane(&begin, Some(&notification_tx), Some(&lifecycle_tx))
                .map(|(lane, _)| lane),
            Some("lifecycle")
        );

        let server_status = LspNotification::Other {
            method: std::borrow::Cow::Borrowed("experimental/serverStatus"),
            params: Some(serde_json::json!({ "quiescent": false })),
        };
        assert_eq!(
            LspClient::notification_lane(
                &server_status,
                Some(&notification_tx),
                Some(&lifecycle_tx)
            )
            .map(|(lane, _)| lane),
            Some("lifecycle")
        );
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

    #[test]
    fn test_truncate_error_message_for_log_handles_multibyte_boundary() {
        // 199 ASCII bytes followed by a 3-byte UTF-8 char ('€') straddles the byte-200 cut.
        let message = format!("{}€{}", "x".repeat(199), "y".repeat(50));

        let truncated = LspClient::truncate_error_message_for_log(&message);

        // Cutting before the multi-byte char keeps the message valid UTF-8 (no panic) and
        // pins the payload to 199 bytes, not 200.
        assert_eq!(truncated, format!("{}... (truncated)", "x".repeat(199)));
    }

    #[test]
    fn test_truncate_error_message_for_log_no_truncation_at_or_below_limit() {
        let exact = "x".repeat(200);
        assert_eq!(LspClient::truncate_error_message_for_log(&exact), exact);
        assert_eq!(LspClient::truncate_error_message_for_log(""), "");
    }

    #[test]
    fn test_truncate_error_message_for_log_truncates_just_above_limit() {
        let message = "x".repeat(201);
        assert_eq!(
            LspClient::truncate_error_message_for_log(&message),
            format!("{}... (truncated)", "x".repeat(200))
        );
    }

    #[test]
    fn test_truncate_error_message_for_log_handles_wide_char_at_limit() {
        // A 4-byte emoji run straddling every possible alignment near the byte-200 boundary.
        let message = format!("{}{}", "x".repeat(197), "🦀".repeat(10));

        let truncated = LspClient::truncate_error_message_for_log(&message);

        assert_eq!(truncated, format!("{}... (truncated)", "x".repeat(197)));
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

    /// Wire-level checks that `params: void` LSP methods go out without a
    /// `params` key. tsgo rejects `"params": null` on `shutdown` with
    /// `-32602 expected empty, got: null` and then ignores the `exit` that
    /// follows, so mcpls fell through to the kill-on-timeout path.
    mod void_params_wire {
        use tokio::io::BufReader;

        use super::*;
        use crate::test_lsp::{fake_lsp_client, read_framed_message, write_response};

        #[tokio::test]
        async fn test_request_with_null_params_omits_params_key() {
            let (client, mut server) = fake_lsp_client();

            let request_task = tokio::spawn(async move {
                client
                    .request::<_, Value>("shutdown", Value::Null, Duration::from_secs(5))
                    .await
            });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;

            assert_eq!(request["method"], "shutdown");
            assert!(
                request.get("params").is_none(),
                "null params must be omitted, got: {request}"
            );

            write_response(&mut server.read_half_stdin, &request["id"], Value::Null).await;
            request_task.await.unwrap().unwrap();
        }

        #[tokio::test]
        async fn test_notify_with_null_params_omits_params_key() {
            let (client, mut server) = fake_lsp_client();

            client.notify("exit", Value::Null).await.unwrap();

            let mut reader = BufReader::new(&mut server.write_stdout);
            let notification = read_framed_message(&mut reader).await;

            assert_eq!(notification["method"], "exit");
            assert!(
                notification.get("params").is_none(),
                "null params must be omitted, got: {notification}"
            );
        }

        #[tokio::test]
        async fn test_notify_with_empty_object_params_keeps_params_key() {
            let (client, mut server) = fake_lsp_client();

            client
                .notify("initialized", lsp_types::InitializedParams {})
                .await
                .unwrap();

            let mut reader = BufReader::new(&mut server.write_stdout);
            let notification = read_framed_message(&mut reader).await;

            assert_eq!(notification["method"], "initialized");
            assert_eq!(
                notification["params"],
                serde_json::json!({}),
                "non-null params must still be sent"
            );
        }
    }

    mod retry_behavior {
        use tokio::io::{AsyncWriteExt, BufReader, DuplexStream};

        use super::*;
        use crate::test_lsp::{
            CapturedLogs, fake_lsp_client, read_framed_message, write_error_response,
            write_response as write_success_response,
        };

        /// Writes a framed JSON-RPC retryable error response — either
        /// `ServerCancelled` (-32802) or `ContentModified` (-32801) — with a
        /// `data.retriggerRequest` flag.
        ///
        /// Kept local rather than promoted to the shared `test_lsp` harness:
        /// the `data.retriggerRequest` field is specific to this retry-logic
        /// test suite, unlike the generic success/error responses above.
        async fn write_retryable_error_response(
            stdin: &mut DuplexStream,
            id: &Value,
            code: i32,
            message: &str,
            retrigger: bool,
        ) {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": code,
                    "message": message,
                    "data": { "retriggerRequest": retrigger },
                },
            });
            let content = serde_json::to_string(&response).unwrap();
            let header = format!("Content-Length: {}\r\n\r\n", content.len());
            stdin.write_all(header.as_bytes()).await.unwrap();
            stdin.write_all(content.as_bytes()).await.unwrap();
            stdin.flush().await.unwrap();
        }

        // Not `start_paused`: the retry loop's real backoff sleeps
        // interleave with real async I/O on the duplex pipes below, and
        // paused virtual time does not reliably auto-advance across both.
        //
        // Also captures tracing output (#392): retry exhaustion is the one
        // scenario where every attempt but the last logs `warn!` and only
        // the last logs `error!`, so this doubles as that regression test
        // rather than duplicating the same ~3.5s wire choreography in a
        // second test just to assert on log severity.
        #[tokio::test]
        async fn test_retry_exhaustion_returns_original_server_cancelled_error() {
            use tracing_subscriber::layer::SubscriberExt as _;

            let (client, mut server) = fake_lsp_client();
            let captured = CapturedLogs::default();
            let subscriber = tracing_subscriber::registry().with(captured.clone());
            let guard = tracing::subscriber::set_default(subscriber);

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
                write_retryable_error_response(
                    &mut server.read_half_stdin,
                    &id,
                    SERVER_CANCELLED_CODE,
                    "server cancelled the request",
                    true,
                )
                .await;
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

            drop(guard);
            let logs = captured.entries();
            assert_eq!(
                logs.iter()
                    .filter(|(level, _)| *level == tracing::Level::ERROR)
                    .count(),
                1,
                "exactly the final exhausted attempt must log at ERROR, got: {logs:?}"
            );
            assert!(
                logs.iter()
                    .any(|(level, msg)| *level == tracing::Level::ERROR
                        && msg.contains("LSP error response")
                        && msg.contains("retries exhausted")),
                "expected an ERROR log sharing the 'LSP error response' prefix and naming \
                 retry exhaustion, got: {logs:?}"
            );
            assert_eq!(
                logs.iter()
                    .filter(
                        |(level, msg)| *level == tracing::Level::WARN && msg.contains("will retry")
                    )
                    .count(),
                usize::try_from(SERVER_CANCELLED_MAX_RETRIES).unwrap(),
                "every attempt before the last must log a WARN 'will retry' line, got: {logs:?}"
            );
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
            write_retryable_error_response(
                &mut server.read_half_stdin,
                &id,
                SERVER_CANCELLED_CODE,
                "server cancelled the request",
                false,
            )
            .await;

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
            write_retryable_error_response(
                &mut server.read_half_stdin,
                &first["id"].clone(),
                SERVER_CANCELLED_CODE,
                "server cancelled the request",
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

        #[tokio::test]
        async fn test_retry_exhaustion_returns_original_content_modified_error() {
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
            // attempt gets ContentModified, so retries must exhaust rather
            // than loop forever or swallow the error. -32801 shares the same
            // attempt budget as -32802 (FR-002), not an independent one.
            for _ in 0..=SERVER_CANCELLED_MAX_RETRIES {
                let request = read_framed_message(&mut reader).await;
                let id = request["id"].clone();
                write_retryable_error_response(
                    &mut server.read_half_stdin,
                    &id,
                    i32::from(LspErrorCodes::ContentModified),
                    "content modified",
                    true,
                )
                .await;
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
                    assert_eq!(code, i32::from(LspErrorCodes::ContentModified));
                    assert_eq!(message, "content modified");
                    assert_eq!(data, Some(serde_json::json!({ "retriggerRequest": true })));
                }
                other => panic!("expected exhausted ContentModified error, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn test_retrigger_false_returns_immediately_without_retry_for_content_modified() {
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
            write_retryable_error_response(
                &mut server.read_half_stdin,
                &id,
                i32::from(LspErrorCodes::ContentModified),
                "content modified",
                false,
            )
            .await;

            // Same `should_retrigger` gate as -32802: a non-spec-compliant
            // server sending `retriggerRequest: false` on -32801 must still
            // be honored (FR-006 resolution), short-circuiting the loop well
            // under the first 500ms backoff.
            let result = tokio::time::timeout(Duration::from_millis(200), request_task)
                .await
                .unwrap()
                .unwrap();

            match result {
                Err(Error::LspServerError { code, .. }) => {
                    assert_eq!(code, i32::from(LspErrorCodes::ContentModified));
                }
                other => panic!("expected immediate ContentModified error, got {other:?}"),
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
        async fn test_retry_succeeds_after_one_content_modified_response() {
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

            // First attempt gets ContentModified and must retrigger.
            let first = read_framed_message(&mut reader).await;
            write_retryable_error_response(
                &mut server.read_half_stdin,
                &first["id"].clone(),
                i32::from(LspErrorCodes::ContentModified),
                "content modified",
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

        #[tokio::test]
        async fn test_content_modified_on_non_allowlisted_method_does_not_retry() {
            let (client, mut server) = fake_lsp_client();

            // `textDocument/rename` is deliberately excluded from
            // `CONTENT_MODIFIED_RETRY_METHODS` (its result is an edit the
            // caller applies at a position that may no longer be valid once
            // the document changed) -- a -32801 response for it must return
            // immediately even though `retriggerRequest: true` would pass
            // `should_retrigger`'s gate on its own.
            let request_task = tokio::spawn(async move {
                client
                    .request::<_, Value>(
                        "textDocument/rename",
                        serde_json::json!({}),
                        Duration::from_secs(30),
                    )
                    .await
            });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;
            let id = request["id"].clone();
            write_retryable_error_response(
                &mut server.read_half_stdin,
                &id,
                i32::from(LspErrorCodes::ContentModified),
                "content modified",
                true,
            )
            .await;

            let result = tokio::time::timeout(Duration::from_millis(200), request_task)
                .await
                .unwrap()
                .unwrap();

            match result {
                Err(Error::LspServerError { code, .. }) => {
                    assert_eq!(code, i32::from(LspErrorCodes::ContentModified));
                }
                other => panic!("expected immediate ContentModified error, got {other:?}"),
            }

            let second_request =
                tokio::time::timeout(Duration::from_millis(200), read_framed_message(&mut reader))
                    .await;
            assert!(
                second_request.is_err(),
                "no retry should have been sent for a non-allowlisted method"
            );
        }

        /// #313: an oversized, server-controlled error message must be
        /// truncated before it reaches the MCP caller in
        /// `Error::LspServerError`, not just before it is logged. Routes
        /// through the real `message_loop_inner` (via `fake_lsp_client`)
        /// rather than constructing the error by hand, so it actually
        /// exercises the fix.
        #[tokio::test]
        async fn test_oversized_error_message_truncated_for_caller() {
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
            let oversized_message = "x".repeat(MAX_ERROR_MESSAGE_CALLER_BYTES + 500);
            write_error_response(&mut server.read_half_stdin, &id, -32603, &oversized_message)
                .await;

            let result = request_task.await.unwrap();

            match result {
                Err(Error::LspServerError { code, message, .. }) => {
                    assert_eq!(code, -32603);
                    assert!(
                        message.len() < oversized_message.len(),
                        "caller-facing message must be truncated, got {} bytes",
                        message.len()
                    );
                    assert!(message.ends_with("... (truncated)"));
                }
                other => panic!("expected truncated LspServerError, got {other:?}"),
            }
        }

        /// #313 S2: a legitimate error message longer than the log-line cap
        /// (`MAX_ERROR_MESSAGE_LOG_BYTES`, 200 bytes) but shorter than the
        /// caller-facing cap must reach the MCP caller intact -- the
        /// caller-facing budget must not silently collapse to the log
        /// budget.
        #[tokio::test]
        async fn test_error_message_between_log_and_caller_caps_reaches_caller_intact() {
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
            let message = "x".repeat(MAX_ERROR_MESSAGE_LOG_BYTES + 50);
            write_error_response(&mut server.read_half_stdin, &id, -32603, &message).await;

            let result = request_task.await.unwrap();

            match result {
                Err(Error::LspServerError {
                    message: returned, ..
                }) => {
                    assert_eq!(
                        returned, message,
                        "message under the caller cap must not be truncated"
                    );
                }
                other => panic!("expected untruncated LspServerError, got {other:?}"),
            }
        }

        /// #392: a `-32802`/`-32801` error that gets retried and then
        /// succeeds must not log at `error!` -- only a `warn!` "will retry"
        /// line -- so log-based monitoring does not false-positive on a
        /// transient error the retry loop silently recovers from.
        #[tokio::test]
        async fn test_retried_error_that_recovers_does_not_log_error_level() {
            use tracing_subscriber::layer::SubscriberExt as _;

            let (client, mut server) = fake_lsp_client();
            let captured = CapturedLogs::default();
            let subscriber = tracing_subscriber::registry().with(captured.clone());
            let guard = tracing::subscriber::set_default(subscriber);

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

            let first = read_framed_message(&mut reader).await;
            write_retryable_error_response(
                &mut server.read_half_stdin,
                &first["id"].clone(),
                SERVER_CANCELLED_CODE,
                "server cancelled the request",
                true,
            )
            .await;

            let second = read_framed_message(&mut reader).await;
            write_success_response(
                &mut server.read_half_stdin,
                &second["id"].clone(),
                serde_json::json!({ "contents": "resolved on retry" }),
            )
            .await;

            let result = request_task.await.unwrap();
            assert!(result.is_ok(), "expected retry to recover, got {result:?}");

            drop(guard);
            let logs = captured.entries();
            assert!(
                !logs
                    .iter()
                    .any(|(level, _)| *level == tracing::Level::ERROR),
                "a retried-and-recovered error must not log at ERROR, got: {logs:?}"
            );
            assert!(
                logs.iter().any(
                    |(level, msg)| *level == tracing::Level::WARN && msg.contains("will retry")
                ),
                "expected a WARN 'will retry' log line, got: {logs:?}"
            );
        }

        /// #392: a `-32801` (`ContentModified`) error for a method outside
        /// `CONTENT_MODIFIED_RETRY_METHODS` is never retried, so it must
        /// still surface at `error!` on the very first attempt.
        #[tokio::test]
        async fn test_non_retryable_error_logs_error_level() {
            use tracing_subscriber::layer::SubscriberExt as _;

            let (client, mut server) = fake_lsp_client();
            let captured = CapturedLogs::default();
            let subscriber = tracing_subscriber::registry().with(captured.clone());
            let guard = tracing::subscriber::set_default(subscriber);

            let request_task = tokio::spawn(async move {
                client
                    .request::<_, Value>(
                        "textDocument/rename",
                        serde_json::json!({}),
                        Duration::from_secs(30),
                    )
                    .await
            });

            let mut reader = BufReader::new(&mut server.write_stdout);
            let request = read_framed_message(&mut reader).await;
            let id = request["id"].clone();
            write_retryable_error_response(
                &mut server.read_half_stdin,
                &id,
                i32::from(LspErrorCodes::ContentModified),
                "content modified",
                true,
            )
            .await;

            let result = request_task.await.unwrap();
            assert!(result.is_err(), "expected a non-retryable error");

            drop(guard);
            let logs = captured.entries();
            assert!(
                logs.iter()
                    .any(|(level, msg)| *level == tracing::Level::ERROR
                        && msg.contains("LSP error response")
                        && msg.contains("content modified")),
                "a non-retryable error must still surface an ERROR log sharing the \
                 'LSP error response' prefix, got: {logs:?}"
            );
        }
    }

    /// Regression coverage for #451 (`LspTransportReader::receive` moved off
    /// the `select!` and onto a dedicated reader task, see
    /// [`super::spawn_reader_task`]).
    mod reader_task_regression {
        use tokio::io::BufReader;

        use super::*;
        use crate::test_lsp::{
            fake_lsp_client, inert_transport, read_framed_message, write_response,
        };

        /// The reader task only ever sends `Err` through the channel before
        /// exiting (see `spawn_reader_task`) -- a `None` from `msg_rx.recv()`
        /// means the task disappeared some other way (e.g. panicked). This is
        /// the one branch in `message_loop_inner` that has no equivalent in
        /// the pre-#451 code, so it needs its own direct test rather than
        /// relying on the full `fake_lsp_client` harness to provoke it.
        #[tokio::test]
        async fn test_message_loop_inner_treats_reader_channel_close_as_server_terminated() {
            let (mut transport, _reader) = inert_transport();
            let (_command_tx, mut command_rx) = mpsc::channel::<ClientCommand>(1);
            let (msg_tx, mut msg_rx) = mpsc::channel::<Result<InboundMessage>>(1);
            let pending_requests: Arc<Mutex<PendingRequests>> =
                Arc::new(Mutex::new(HashMap::new()));

            drop(msg_tx);

            let result = LspClient::message_loop_inner(
                &mut transport,
                &mut msg_rx,
                &mut command_rx,
                &pending_requests,
                None,
                None,
            )
            .await;

            assert!(
                matches!(result, Err(Error::ServerTerminated)),
                "got {result:?}"
            );
        }

        /// The reader task can run ahead of `select!` and have already queued
        /// a fully-decoded response in `msg_rx` by the time a concurrent
        /// `Shutdown` command is what `select!` happens to pick. Before the
        /// drain fix, that queued response was silently dropped instead of
        /// resolving its caller's pending request -- the caller would then
        /// block until its own `request_timeout_seconds` elapsed instead of
        /// failing fast or succeeding.
        #[tokio::test]
        async fn test_shutdown_drains_buffered_responses_instead_of_dropping_them() {
            let (mut transport, _reader) = inert_transport();
            let (command_tx, mut command_rx) = mpsc::channel::<ClientCommand>(1);
            let (msg_tx, mut msg_rx) = mpsc::channel::<Result<InboundMessage>>(1);
            let pending_requests: Arc<Mutex<PendingRequests>> =
                Arc::new(Mutex::new(HashMap::new()));

            let id = RequestId::Number(1);
            let (response_tx, response_rx) = oneshot::channel::<Result<Value>>();
            pending_requests
                .lock()
                .await
                .insert(id.clone(), response_tx);

            msg_tx
                .send(Ok(InboundMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.clone(),
                    result: Some(serde_json::json!({ "ok": true })),
                    error: None,
                })))
                .await
                .unwrap();
            command_tx.send(ClientCommand::Shutdown).await.unwrap();
            drop(command_tx);

            let result = LspClient::message_loop_inner(
                &mut transport,
                &mut msg_rx,
                &mut command_rx,
                &pending_requests,
                None,
                None,
            )
            .await;

            assert!(result.is_ok(), "got {result:?}");
            // `Err` here means the sender was dropped without a reply -- i.e. the
            // buffered response was lost instead of resolving this request.
            let received = response_rx.await.unwrap();
            assert_eq!(received.unwrap(), serde_json::json!({ "ok": true }));
            assert!(
                pending_requests.lock().await.is_empty(),
                "the drained response must resolve its pending request entry"
            );
        }

        /// Drives dense, concurrent request/response traffic through the real
        /// `message_loop`/`spawn_reader_task` pair (via `fake_lsp_client`) and
        /// answers deliberately out of arrival order, so a bug that matched
        /// responses positionally instead of by id -- the failure mode a
        /// mid-frame desync would eventually cause -- would surface as a
        /// mismatched payload rather than a hang.
        #[tokio::test]
        async fn test_dense_concurrent_requests_all_resolve_to_matching_responses() {
            const REQUEST_COUNT: usize = 20;
            let (client, mut server) = fake_lsp_client();

            // A manual push loop, not `.map(..).collect()`: `tokio::spawn`
            // must run eagerly for every `i` right here, before
            // `server_task` starts answering below -- a lazily-iterated
            // combinator would spawn (and thus send) each request only as
            // its `JoinHandle` is later awaited, one at a time, defeating
            // the "dense concurrent" setup this test needs.
            let mut request_tasks = Vec::with_capacity(REQUEST_COUNT);
            for i in 0..REQUEST_COUNT {
                let client = client.clone();
                request_tasks.push(tokio::spawn(async move {
                    client
                        .request::<_, Value>(
                            "textDocument/hover",
                            serde_json::json!({ "n": i }),
                            Duration::from_secs(30),
                        )
                        .await
                }));
            }

            let server_task = tokio::spawn(async move {
                let mut reader = BufReader::new(&mut server.write_stdout);
                let mut requests = Vec::with_capacity(REQUEST_COUNT);
                for _ in 0..REQUEST_COUNT {
                    requests.push(read_framed_message(&mut reader).await);
                }
                for request in requests.into_iter().rev() {
                    let id = request["id"].clone();
                    let n = request["params"]["n"].clone();
                    write_response(
                        &mut server.read_half_stdin,
                        &id,
                        serde_json::json!({ "echo": n }),
                    )
                    .await;
                }
            });

            server_task.await.unwrap();

            for (i, task) in request_tasks.into_iter().enumerate() {
                let value = task
                    .await
                    .unwrap()
                    .unwrap_or_else(|e| panic!("request {i} failed: {e:?}"));
                assert_eq!(
                    value["echo"],
                    serde_json::json!(i),
                    "response for request {i} carried the wrong payload -- id/response mismatch"
                );
            }
        }
    }
}
