//! MCP client simulator for end-to-end testing.
//!
//! This module provides a synchronous MCP client that spawns the mcpls binary
//! and communicates via stdio using the JSON-RPC 2.0 protocol.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// Simulates an MCP client (like Claude Code) for E2E testing.
///
/// This client spawns the mcpls binary as a child process and communicates
/// with it via stdio using JSON-RPC 2.0 protocol.
///
/// # Examples
///
/// ```no_run
/// use mcpls_core::tests::e2e::mcp_client::McpClient;
///
/// let mut client = McpClient::spawn()?;
/// let response = client.initialize()?;
/// assert!(response.get("result").is_some());
/// # Ok::<(), anyhow::Error>(())
/// ```
pub struct McpClient {
    process: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    request_id: i64,
}

impl McpClient {
    /// Spawn mcpls process and connect via stdio.
    ///
    /// Uses an empty configuration file for testing the MCP protocol layer only.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The mcpls binary cannot be found or spawned
    /// - stdin or stdout cannot be captured
    pub fn spawn() -> Result<Self> {
        // Use empty config to avoid LSP server initialization timeouts
        let config_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/empty_config.toml");

        Self::spawn_with_args(&[
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid config path"))?,
        ])
    }

    /// Spawn mcpls process with custom arguments.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The mcpls binary cannot be found or spawned
    /// - stdin or stdout cannot be captured
    pub fn spawn_with_args(args: &[&str]) -> Result<Self> {
        // Get binary path from cargo test environment
        // CARGO_BIN_EXE_mcpls is only set for tests in the mcpls-cli crate.
        // For tests in mcpls-core, compute workspace root from CARGO_MANIFEST_DIR.
        let binary_path = std::env::var("CARGO_BIN_EXE_mcpls")
            .ok()
            .or_else(|| {
                let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
                manifest_dir
                    .ancestors()
                    .nth(2) // mcpls-core -> crates -> workspace
                    .map(|root| {
                        root.join("target/debug/mcpls")
                            .to_string_lossy()
                            .into_owned()
                    })
            })
            .unwrap_or_else(|| "target/debug/mcpls".to_string());

        let mut process = Command::new(binary_path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to spawn mcpls binary")?;

        let stdin = process
            .stdin
            .take()
            .context("failed to capture stdin of mcpls process")?;

        let stdout = process
            .stdout
            .take()
            .context("failed to capture stdout of mcpls process")?;

        Ok(Self {
            process,
            stdin,
            stdout: BufReader::new(stdout),
            request_id: 0,
        })
    }

    /// Send MCP initialize request.
    ///
    /// This establishes the MCP connection and negotiates protocol version.
    /// After receiving the initialize response, sends the initialized notification
    /// as required by the MCP protocol.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The request cannot be sent
    /// - The response cannot be read or parsed
    /// - The server returns an error response
    pub fn initialize(&mut self) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "mcpls-e2e-test",
                    "version": "0.1.0"
                }
            }
        });

        let response = self.send_request(&request)?;

        // Send initialized notification as required by MCP protocol
        self.send_notification("notifications/initialized", &json!({}))?;

        Ok(response)
    }

    /// List available MCP tools.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The request cannot be sent
    /// - The response cannot be read or parsed
    /// - The server returns an error response
    pub fn list_tools(&mut self) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "tools/list",
            "params": {}
        });

        self.send_request(&request)
    }

    /// Call a tool by name with parameters.
    ///
    /// # Parameters
    ///
    /// - `name`: The name of the tool to call
    /// - `arguments`: JSON object with tool-specific parameters
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The request cannot be sent
    /// - The response cannot be read or parsed
    /// - The server returns an error response
    /// - The tool does not exist
    /// - The parameters are invalid
    pub fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": arguments
            }
        });

        self.send_request(&request)
    }

    /// Send a raw JSON-RPC request and return the response.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The request cannot be serialized or sent
    /// - The response cannot be read or parsed
    /// - The server returns an error response
    fn send_request(&mut self, request: &Value) -> Result<Value> {
        let request_str = serde_json::to_string(request)?;
        writeln!(self.stdin, "{request_str}")?;
        self.stdin.flush()?;

        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .context("failed to read response from mcpls")?;

        let response: Value =
            serde_json::from_str(&line).context("failed to parse JSON-RPC response")?;

        if let Some(error) = response.get("error") {
            anyhow::bail!("MCP error: {error:?}");
        }

        Ok(response)
    }

    /// Send a notification (request without expecting a response).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The notification cannot be serialized or sent
    fn send_notification(&mut self, method: &str, params: &Value) -> Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let notification_str = serde_json::to_string(&notification)?;
        writeln!(self.stdin, "{notification_str}")?;
        self.stdin.flush()?;

        Ok(())
    }

    /// Get the next request ID and increment the counter.
    // False positive: clippy suggests const fn, but const fn cannot mutate self
    #[allow(clippy::missing_const_for_fn)]
    fn next_id(&mut self) -> i64 {
        self.request_id += 1;
        self.request_id
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "Requires mcpls binary built"]
    fn test_mcp_client_spawn() {
        let client = McpClient::spawn();
        assert!(client.is_ok(), "Should successfully spawn mcpls binary");
    }

    #[test]
    #[ignore = "Requires mcpls binary built"]
    fn test_request_id_increment() -> Result<()> {
        let mut client = McpClient::spawn()?;
        assert_eq!(client.next_id(), 1);
        assert_eq!(client.next_id(), 2);
        assert_eq!(client.next_id(), 3);
        Ok(())
    }
}
