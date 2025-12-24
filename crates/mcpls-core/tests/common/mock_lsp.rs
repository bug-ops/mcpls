use std::collections::HashMap;

use serde_json::Value;
use tokio::sync::mpsc;

/// A simple mock LSP server for testing.
///
/// This mock server responds to LSP requests with pre-configured responses.
/// It uses channels for communication instead of spawning actual processes.
#[allow(dead_code)]
pub struct MockLspServer {
    responses: HashMap<String, Value>,
}

#[allow(dead_code)]
impl MockLspServer {
    /// Creates a new mock LSP server.
    pub fn new() -> Self {
        Self {
            responses: HashMap::new(),
        }
    }

    /// Registers a response for a specific LSP method.
    ///
    /// When the mock server receives a request for the given method,
    /// it will respond with the provided value.
    pub fn register_response(&mut self, method: &str, response: Value) {
        self.responses.insert(method.to_string(), response);
    }

    /// Gets the response for a given method.
    pub fn get_response(&self, method: &str) -> Option<&Value> {
        self.responses.get(method)
    }
}

impl Default for MockLspServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Mock behavior types for testing different scenarios.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum MockBehavior {
    /// Return a fixed response.
    FixedResponse(Value),
    /// Echo the request back.
    Echo,
    /// Return an error with the given code and message.
    Error(i64, String),
}

/// Builder for configuring mock LSP server behavior.
#[allow(dead_code)]
pub struct MockBehaviorBuilder {
    behaviors: HashMap<String, MockBehavior>,
}

#[allow(dead_code)]
impl MockBehaviorBuilder {
    /// Creates a new behavior builder.
    pub fn new() -> Self {
        Self {
            behaviors: HashMap::new(),
        }
    }

    /// Sets the behavior for a specific method.
    pub fn on_method(mut self, method: &str, behavior: MockBehavior) -> Self {
        self.behaviors.insert(method.to_string(), behavior);
        self
    }

    /// Builds the configured behaviors.
    pub fn build(self) -> HashMap<String, MockBehavior> {
        self.behaviors
    }
}

impl Default for MockBehaviorBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A simple channel-based mock for testing LSP communication.
#[allow(dead_code)]
pub struct MockLspChannel {
    tx: mpsc::UnboundedSender<Value>,
    rx: mpsc::UnboundedReceiver<Value>,
}

#[allow(dead_code)]
impl MockLspChannel {
    /// Creates a new mock LSP channel pair.
    pub fn new() -> (Self, Self) {
        let (tx1, rx1) = mpsc::unbounded_channel();
        let (tx2, rx2) = mpsc::unbounded_channel();

        (Self { tx: tx1, rx: rx2 }, Self { tx: tx2, rx: rx1 })
    }

    /// Sends a message through the channel.
    pub fn send(&self, msg: Value) -> Result<(), String> {
        self.tx.send(msg).map_err(|e| e.to_string())
    }

    /// Receives a message from the channel.
    pub async fn recv(&mut self) -> Option<Value> {
        self.rx.recv().await
    }
}
