//! End-to-end cancellation over an in-memory MCP connection and a scripted
//! fake language server: the MCP client speaks raw JSON-RPC to a served
//! `McplsServer`, whose translator talks to a recording fake LSP endpoint.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt as _;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{
    AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, ReadHalf, WriteHalf, split,
};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::PEER_CLOSE_POLL;
use crate::bridge::{NotificationCache, Translator, WorkspaceRoots};
use crate::config::{
    FileExtension, LanguageId, McpConfig, ProjectConfigStatus, ServerId, ToolRouter,
};
use crate::mcp::{McplsServer, SubscriptionRegistry};
use crate::test_lsp::{FakeServer, fake_lsp_client, read_framed_message, write_response};

/// rmcp 3.5.1 waits this long for in-flight handlers when the transport hits
/// EOF, before it ends the serve loop and closes the peer.
const RMCP_EOF_DRAIN: Duration = Duration::from_secs(5);

struct Wire {
    _workspace: TempDir,
    file: String,
    lsp: Option<FakeServer>,
    reader: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
    serving: JoinHandle<()>,
    stop: CancellationToken,
    /// The server's own shutdown token, as a transport cancels it.
    shutdown: CancellationToken,
    /// Fires the shutdown signal of a wire served through `serve_until`.
    signal: Option<tokio::sync::oneshot::Sender<()>>,
    next_id: i64,
}

fn mcp_server(translator: Arc<Translator>, roots: WorkspaceRoots, mcp: McpConfig) -> McplsServer {
    McplsServer::new(
        translator,
        Arc::new(Mutex::new(NotificationCache::new())),
        roots,
        SubscriptionRegistry::new(),
        ProjectConfigStatus::NotIgnored,
        mcp,
    )
}

impl Wire {
    /// A server whose only language server is a recording fake.
    async fn connect() -> Self {
        let workspace = TempDir::new().unwrap();
        let file = workspace.path().join("lib.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let roots = WorkspaceRoots::from_paths(&[workspace.path().to_path_buf()]).unwrap();

        let server_id = ServerId::from_static("rust");
        let mut translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                LanguageId::from_static("rust"),
            )]))
            .with_extensions(HashMap::from([(
                FileExtension::from_static("rs"),
                LanguageId::from_static("rust"),
            )]));
        translator.set_workspace_roots(roots.clone());
        let translator = Arc::new(translator);
        let (client, lsp) = fake_lsp_client();
        translator.register_client(server_id, client);

        let server = mcp_server(translator, roots, McpConfig::default());
        Self::serve(server, workspace, &file, Some(lsp), false).await
    }

    /// [`Self::connect`], served the way `run_stdio` serves: through
    /// `serve_until`, ended by `signal`.
    async fn connect_via_stdio_path() -> Self {
        let workspace = TempDir::new().unwrap();
        let file = workspace.path().join("lib.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let roots = WorkspaceRoots::from_paths(&[workspace.path().to_path_buf()]).unwrap();
        let server_id = ServerId::from_static("rust");
        let mut translator = Translator::new()
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                LanguageId::from_static("rust"),
            )]))
            .with_extensions(HashMap::from([(
                FileExtension::from_static("rs"),
                LanguageId::from_static("rust"),
            )]));
        translator.set_workspace_roots(roots.clone());
        let translator = Arc::new(translator);
        let (client, lsp) = fake_lsp_client();
        translator.register_client(server_id, client);

        let server = mcp_server(translator, roots, McpConfig::default());
        Self::serve(server, workspace, &file, Some(lsp), true).await
    }

    async fn serve(
        server: McplsServer,
        workspace: TempDir,
        file: &std::path::Path,
        lsp: Option<FakeServer>,
        via_stdio_path: bool,
    ) -> Self {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let stop = CancellationToken::new();
        let serve_stop = stop.clone();
        let shutdown = server.stop_token();
        let (signal_tx, signal_rx) = tokio::sync::oneshot::channel::<()>();
        let serving = if via_stdio_path {
            tokio::spawn(async move {
                crate::transport::serve_until(server, split(server_io), async move {
                    signal_rx.await.ok();
                })
                .await
                .ok();
            })
        } else {
            tokio::spawn(async move {
                if let Ok(running) = server.serve_with_ct(split(server_io), serve_stop).await {
                    running.waiting().await.ok();
                }
            })
        };
        let (read, writer) = split(client_io);
        let mut wire = Self {
            file: file.display().to_string(),
            _workspace: workspace,
            lsp,
            reader: BufReader::new(read).lines(),
            writer,
            serving,
            stop,
            shutdown,
            signal: via_stdio_path.then_some(signal_tx),
            next_id: 0,
        };
        wire.handshake().await;
        wire
    }

    async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn handshake(&mut self) {
        self.send(json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"},
            },
        }))
        .await;
        self.reader.next_line().await.unwrap().unwrap();
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
    }

    /// Sends a `tools/call` and returns its request id.
    async fn call(&mut self, name: &str, arguments: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }))
        .await;
        id
    }

    async fn call_hover(&mut self) -> i64 {
        let arguments = json!({"file_path": self.file, "line": 1, "character": 1});
        self.call("get_hover", arguments).await
    }

    async fn cancel(&mut self, request_id: i64) {
        self.send(json!({
            "jsonrpc": "2.0", "method": "notifications/cancelled",
            "params": {"requestId": request_id, "reason": "test"},
        }))
        .await;
    }

    /// Reads the next frame the language server received.
    async fn lsp_frame(&mut self) -> Value {
        // One byte of buffer: a larger one would swallow the frames behind this
        // one when the reader is dropped.
        let lsp = self.lsp.as_mut().expect("a fake language server");
        let mut reader = BufReader::with_capacity(1, &mut lsp.write_stdout);
        read_framed_message(&mut reader).await
    }

    /// The hover request the language server received after the `didOpen`.
    async fn hover_request(&mut self) -> Value {
        let opened = self.lsp_frame().await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        let request = self.lsp_frame().await;
        assert_eq!(request["method"], "textDocument/hover");
        request
    }

    /// Reads responses until the one for `id` arrives, returning every
    /// response id seen on the way.
    async fn responses_until(&mut self, id: i64) -> Vec<i64> {
        self.responses_until_with(id).await.0
    }

    /// As [`Self::responses_until`], also returning the awaited response.
    async fn responses_until_with(&mut self, id: i64) -> (Vec<i64>, Value) {
        let mut seen = Vec::new();
        loop {
            let line = self.reader.next_line().await.unwrap().unwrap();
            let message: Value = serde_json::from_str(&line).unwrap();
            if let Some(response_id) = message["id"].as_i64() {
                seen.push(response_id);
                if response_id == id {
                    return (seen, message);
                }
            }
        }
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

/// US-001: a client cancel stops the call, tells the language server once and
/// sends the client no response for it.
#[tokio::test(start_paused = true)]
async fn a_client_cancel_cancels_the_lsp_request_and_sends_no_response() {
    let mut wire = Wire::connect().await;
    let hover = wire.call_hover().await;
    let request = wire.hover_request().await;

    wire.cancel(hover).await;
    let cancel = wire.lsp_frame().await;
    assert_eq!(cancel["method"], "$/cancelRequest");
    assert_eq!(cancel["params"]["id"], request["id"]);

    let probe = wire.call("get_tool_support", json!({})).await;
    let seen = wire.responses_until(probe).await;
    assert_eq!(seen, vec![probe], "the cancelled call got a response");
}

/// #807: a cancelled call is stopped without a WARN from the serve loop, whose
/// "response error" line a protocol error would have triggered.
#[tokio::test(start_paused = true)]
async fn a_client_cancel_logs_no_warning() {
    use tracing_subscriber::prelude::*;

    let logs = crate::test_lsp::CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::TRACE)
            .with(logs.clone()),
    );
    let mut wire = Wire::connect().await;
    let hover = wire.call_hover().await;
    wire.hover_request().await;

    wire.cancel(hover).await;
    let cancel = wire.lsp_frame().await;
    assert_eq!(cancel["method"], "$/cancelRequest");
    let probe = wire.call("get_tool_support", json!({})).await;
    wire.responses_until(probe).await;

    let warnings: Vec<_> = logs
        .entries()
        .into_iter()
        .filter(|(level, _)| *level <= tracing::Level::WARN)
        .collect();
    assert!(warnings.is_empty(), "{warnings:?}");
}

/// #807, #815: a call still pending when the serve loop is cancelled after
/// the server's stop token is answered with a tool error that names shutdown.
#[tokio::test(start_paused = true)]
async fn a_call_pending_at_shutdown_gets_a_tool_error_result() {
    let mut wire = Wire::connect().await;
    let hover = wire.call_hover().await;
    wire.hover_request().await;

    wire.shutdown.cancel();
    wire.stop.cancel();
    let (_, response) = wire.responses_until_with(hover).await;

    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(
        response["result"]["content"][0]["text"],
        "mcpls is shutting down"
    );
}

/// #815: the stdio path cancels the server's stop token on the shutdown signal,
/// so a call pending then reports shutdown, not a client cancellation.
#[tokio::test(start_paused = true)]
async fn a_signal_on_the_stdio_path_reports_shutdown_to_a_pending_call() {
    let mut wire = Wire::connect_via_stdio_path().await;
    let hover = wire.call_hover().await;
    wire.hover_request().await;

    wire.signal.take().unwrap().send(()).unwrap();
    let (_, response) = wire.responses_until_with(hover).await;

    assert_eq!(response["result"]["isError"], true);
    assert_eq!(
        response["result"]["content"][0]["text"],
        "mcpls is shutting down"
    );
}

/// #815: a serve loop cancelled while the server's stop token is not does not
/// claim a shutdown.
#[tokio::test(start_paused = true)]
async fn a_call_pending_when_only_the_serve_loop_ends_reports_a_cancellation() {
    let mut wire = Wire::connect().await;
    let hover = wire.call_hover().await;
    wire.hover_request().await;

    wire.stop.cancel();
    let (_, response) = wire.responses_until_with(hover).await;

    assert_eq!(response["result"]["isError"], true);
    assert_eq!(
        response["result"]["content"][0]["text"],
        "request cancelled by the client or its session ended"
    );
}

/// FR-002: a call already cancelled when its handler starts touches nothing.
#[tokio::test(start_paused = true)]
async fn a_call_cancelled_before_it_starts_issues_no_lsp_traffic() {
    let mut wire = Wire::connect().await;
    let hover = wire.call_hover().await;
    wire.cancel(hover).await;

    let probe = wire.call("get_tool_support", json!({})).await;
    wire.responses_until(probe).await;

    let mut reader = BufReader::with_capacity(1, &mut wire.lsp.as_mut().unwrap().write_stdout);
    let traffic =
        tokio::time::timeout(Duration::from_secs(1), read_framed_message(&mut reader)).await;
    assert!(traffic.is_err(), "unexpected LSP traffic: {traffic:?}");
}

/// The server answers a request the language server completed before the
/// cancel: nothing is cancelled for a finished request.
#[tokio::test(start_paused = true)]
async fn a_finished_call_is_not_cancelled_by_a_late_cancel() {
    let mut wire = Wire::connect().await;
    let hover = wire.call_hover().await;
    let request = wire.hover_request().await;
    write_response(
        &mut wire.lsp.as_mut().unwrap().read_half_stdin,
        &request["id"],
        json!(null),
    )
    .await;
    wire.responses_until(hover).await;

    wire.cancel(hover).await;
    let mut reader = BufReader::with_capacity(1, &mut wire.lsp.as_mut().unwrap().write_stdout);
    let traffic =
        tokio::time::timeout(Duration::from_secs(1), read_framed_message(&mut reader)).await;
    assert!(traffic.is_err(), "unexpected LSP traffic: {traffic:?}");
}

/// FR-026 / FR-027: a hover cancelled at each stage of its life (before any
/// traffic, after `didOpen`, after the request) leaves the document synced
/// exactly once and the next call answered.
#[tokio::test(start_paused = true)]
async fn a_call_cancelled_at_each_stage_leaves_the_document_consistent() {
    for stage in 0..=2 {
        let mut wire = Wire::connect().await;
        let cancelled = wire.call_hover().await;
        let mut seen = Vec::new();
        for _ in 0..stage {
            seen.push(wire.lsp_frame().await);
        }
        wire.cancel(cancelled).await;

        let mut lsp = wire.lsp.take().unwrap();
        let frames = Arc::new(std::sync::Mutex::new(seen));
        let responder = tokio::spawn({
            let frames = Arc::clone(&frames);
            async move {
                loop {
                    let frame = {
                        let mut reader = BufReader::with_capacity(1, &mut lsp.write_stdout);
                        read_framed_message(&mut reader).await
                    };
                    if frame["method"] == "textDocument/hover" {
                        write_response(&mut lsp.read_half_stdin, &frame["id"], json!(null)).await;
                    }
                    frames.lock().unwrap().push(frame);
                }
            }
        });
        let next = wire.call_hover().await;
        let (_, response) = wire.responses_until_with(next).await;
        responder.abort();

        assert!(
            response.get("error").is_none() && response["result"]["isError"] != json!(true),
            "stage {stage}: {response}"
        );
        let opens = frames
            .lock()
            .unwrap()
            .iter()
            .filter(|frame| frame["method"] == "textDocument/didOpen")
            .count();
        assert_eq!(opens, 1, "stage {stage}: the document must be opened once");
    }
}

/// US-005 / FR-023: stdio EOF (modelled by closing the client side of the
/// pipe) stops the in-flight handler, within rmcp's drain plus one poll.
///
/// Proves that `Peer::is_transport_closed` turns true at EOF, which the whole
/// connection-close path rests on.
#[tokio::test(start_paused = true)]
async fn transport_eof_cancels_the_in_flight_call_within_the_drain_bound() {
    let mut wire = Wire::connect().await;
    let _hover = wire.call_hover().await;
    let request = wire.hover_request().await;

    let closed_at = tokio::time::Instant::now();
    wire.writer.shutdown().await.unwrap();
    drop(std::mem::replace(
        &mut wire.reader,
        BufReader::new(split(tokio::io::duplex(1).0).0).lines(),
    ));

    let cancel = wire.lsp_frame().await;
    assert_eq!(cancel["method"], "$/cancelRequest");
    assert_eq!(cancel["params"]["id"], request["id"]);
    let waited = closed_at.elapsed();
    assert!(
        waited <= RMCP_EOF_DRAIN + PEER_CLOSE_POLL,
        "the handler outlived the connection by {waited:?}"
    );
}

/// S3 / FR-025: a `restart_server` cancelled after it began still takes the
/// server to its end state, under a tool prefix too; one cancelled before it
/// began restarts nothing.
#[cfg(unix)]
mod restart {
    use super::*;
    use crate::lsp::LspServer;
    use crate::test_lsp::{GatedWiring, answer_initialize_script, sh_script_init_config};

    const PREFIX: &str = "proj";

    async fn restartable_wire(wiring: Arc<GatedWiring>) -> (Wire, Arc<Translator>) {
        let workspace = TempDir::new().unwrap();
        let roots = WorkspaceRoots::from_paths(&[workspace.path().to_path_buf()]).unwrap();
        let mut translator = Translator::new().with_router(ToolRouter::catch_all([(
            ServerId::from_static("rust"),
            LanguageId::from_static("rust"),
        )]));
        translator.set_workspace_roots(roots.clone());
        let translator = Arc::new(translator);

        let dying = format!(
            "{}\nsleep 0.3\n",
            answer_initialize_script(None, None).replace("sleep 60\n", "")
        );
        let mut spawned = LspServer::spawn(sh_script_init_config(workspace.path(), &dying))
            .await
            .unwrap();
        // A restart of a live server first waits out a graceful shutdown.
        tokio::time::timeout(Duration::from_secs(10), async {
            while !spawned.server.is_dead().unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the seed server must exit");
        translator.register_server_complete(spawned.server);
        translator.install_wiring(wiring);

        let mcp = McpConfig {
            tool_prefix: Some(crate::config::ToolPrefix::new(PREFIX).unwrap()),
            ..McpConfig::default()
        };
        let file = workspace.path().join("lib.rs");
        let server = mcp_server(Arc::clone(&translator), roots, mcp);
        (
            Wire::serve(server, workspace, &file, None, false).await,
            translator,
        )
    }

    #[tokio::test]
    async fn a_cancel_after_the_restart_began_does_not_abandon_it() {
        let wiring = Arc::new(GatedWiring::closed());
        let (mut wire, _translator) = restartable_wire(Arc::clone(&wiring)).await;

        let restart = wire
            .call(&format!("{PREFIX}_restart_server"), json!({"all": true}))
            .await;
        // The restart is past its work and held at the announcement.
        tokio::time::timeout(Duration::from_secs(15), wiring.wait_arrived())
            .await
            .expect("the restart must reach its announcement");
        wire.cancel(restart).await;
        // Answered after the cancel was processed: rmcp reads its input in order.
        let probe = wire
            .call(&format!("{PREFIX}_get_tool_support"), json!({}))
            .await;
        let seen = wire.responses_until(probe).await;
        assert_eq!(seen, vec![probe], "the cancelled call got a response");

        wiring.release();
        tokio::time::timeout(Duration::from_secs(5), wiring.wait_published())
            .await
            .expect("the cancelled restart must still finish and announce itself");
    }

    /// On the current-thread runtime rmcp reads the cancel before the spawned
    /// handler first runs; `a_token_cancelled_up_front_never_polls_the_handler`
    /// pins the guarantee itself, this checks it end to end by what a begun
    /// restart would have done to the next one.
    #[tokio::test(flavor = "current_thread")]
    async fn a_restart_cancelled_before_it_began_restarts_nothing() {
        let wiring = Arc::new(GatedWiring::closed());
        let (mut wire, _translator) = restartable_wire(Arc::clone(&wiring)).await;

        let restart = wire
            .call(&format!("{PREFIX}_restart_server"), json!({"all": true}))
            .await;
        wire.cancel(restart).await;

        // A restart that had begun would put this one in its cooldown.
        let second = wire
            .call(&format!("{PREFIX}_restart_server"), json!({"all": true}))
            .await;
        tokio::time::timeout(Duration::from_secs(15), wiring.wait_arrived())
            .await
            .expect("the uncancelled restart must reach its announcement");
        wiring.release();
        let (_, response) = wire.responses_until_with(second).await;
        assert!(
            response.to_string().contains("restarted"),
            "the second restart did not run: {response}"
        );
        assert_eq!(wiring.arrived(), 1, "the cancelled restart began anyway");
    }
}
