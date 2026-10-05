//! Shared test fixtures for the `translator` module's sibling `tests`
//! submodules: an `EncodingCtx` builder, a fake in-memory LSP server (see
//! `crate::test_lsp`), and JSON-RPC framing helpers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;

use super::Translator;
use super::dto::{BoundedRange, Position, PositionRange};
use super::encoding_ctx::EncodingCtx;
use crate::bridge::encoding::PositionEncoding;
use crate::bridge::state::ResourceLimits;
use crate::bridge::{DiagnosticInfo, DocumentTracker, WorkspaceRoots};
use crate::config::{ServerId, ToolRouter};
use crate::lsp::LspServer;
pub(super) use crate::test_lsp::{
    FakeServer, fake_lsp_client, read_framed_message, write_error_response, write_response,
};

/// The canonical form of `dir`: URIs servers publish, and so the paths
/// workspace-containment checks see, are canonical (`/var` vs `/private/var`).
pub(super) fn canonical_dir(dir: &TempDir) -> PathBuf {
    dunce::canonicalize(dir.path()).unwrap()
}

/// Shorthand for building a [`Position`] test fixture.
pub(super) fn pos(line: u32, character: u32) -> Position {
    Position::at(line, character)
}

/// An ordered range fixture, panicking when `start` lies after `end`.
pub(super) fn span(start: Position, end: Position) -> PositionRange {
    PositionRange::new(start, end).expect("fixture range is ordered")
}

/// A range fixture within the size cap.
pub(super) fn bounded(start: Position, end: Position) -> BoundedRange {
    BoundedRange::try_from(span(start, end)).expect("fixture range is within the size cap")
}

/// A UTF-16 `EncodingCtx`, matching the pre-negotiation behavior: no
/// disk reads, pure line/column offsetting.
pub(super) fn test_ctx() -> EncodingCtx {
    test_ctx_with(PositionEncoding::Utf16)
}

/// An `EncodingCtx` with a fresh, empty `DocumentTracker` -- suitable for
/// tests that need a non-UTF-16 encoding and don't care about the
/// tracker fast path (e.g. exercising the disk-read fallback directly).
pub(super) fn test_ctx_with(encoding: PositionEncoding) -> EncodingCtx {
    test_ctx_with_roots(encoding, WorkspaceRoots::default())
}

/// [`test_ctx_with`] with explicit workspace roots, for tests exercising
/// [`EncodingCtx::is_out_of_workspace`](super::encoding_ctx::EncodingCtx::is_out_of_workspace).
pub(super) fn test_ctx_with_roots(
    encoding: PositionEncoding,
    workspace_roots: WorkspaceRoots,
) -> EncodingCtx {
    EncodingCtx::new(
        encoding,
        Arc::new(DocumentTracker::new(
            ResourceLimits::default(),
            HashMap::new(),
        )),
        workspace_roots,
    )
}

pub(super) fn test_uri() -> lsp_types::Uri {
    lsp_types::Uri::from("file:///test.rs")
}

/// A fresh, empty `DocumentTracker` for tests that call
/// `diagnostics_from_cache_entry`/`merge_diagnostics` directly and don't
/// care about the tracker fast path.
pub(super) fn test_tracker() -> Arc<DocumentTracker> {
    Arc::new(DocumentTracker::new(
        ResourceLimits::default(),
        HashMap::new(),
    ))
}

/// Builds an LSP-side diagnostic for `merge_diagnostics` cache fixtures.
pub(super) fn lsp_diag(
    line: u32,
    end_character: u32,
    severity: lsp_types::DiagnosticSeverity,
    message: &str,
    code: Option<&str>,
) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: lsp_types::Range {
            start: lsp_types::Position { line, character: 0 },
            end: lsp_types::Position {
                line,
                character: end_character,
            },
        },
        severity: Some(severity),
        message: message.to_string().into(),
        code: code.map(|c| lsp_types::Code::String(c.to_string())),
        source: None,
        code_description: None,
        related_information: None,
        tags: None,
        data: None,
    }
}

pub(super) fn diag_info(diagnostics: Vec<lsp_types::Diagnostic>) -> DiagnosticInfo {
    DiagnosticInfo {
        uri: lsp_types::Uri::from("file:///test.rs"),
        version: Some(1),
        diagnostics,
    }
}

/// Builds a single-server translator routed to `server_id` for every tool,
/// with a registered `LspServer` fixture carrying `capabilities` (default
/// capabilities advertise nothing).
pub(super) fn translator_with_capabilities(
    dir: &TempDir,
    server_id: &ServerId,
    capabilities: lsp_types::ServerCapabilities,
) -> (Translator, FakeServer) {
    let mut extensions = HashMap::new();
    extensions.insert("rs".to_string(), "rust".to_string());

    let mut translator =
        Translator::new()
            .with_extensions(extensions)
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                "rust".to_string(),
            )]));
    translator
        .set_workspace_roots(WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap());

    let (client, server) = fake_lsp_client();
    translator.register_client(server_id.clone(), client);
    translator.register_server(server_id.clone(), LspServer::new_for_test(capabilities));

    (translator, server)
}

/// As [`translator_with_capabilities`], but with a caller-chosen
/// negotiated `position_encoding` -- for tests exercising a non-UTF-16
/// `EncodingCtx` conversion path through a full mocked LSP round trip.
pub(super) fn translator_with_capabilities_and_encoding(
    dir: &TempDir,
    server_id: &ServerId,
    capabilities: lsp_types::ServerCapabilities,
    position_encoding: lsp_types::PositionEncodingKind,
) -> (Translator, FakeServer) {
    let mut extensions = HashMap::new();
    extensions.insert("rs".to_string(), "rust".to_string());

    let mut translator =
        Translator::new()
            .with_extensions(extensions)
            .with_router(ToolRouter::catch_all([(
                server_id.clone(),
                "rust".to_string(),
            )]));
    translator
        .set_workspace_roots(WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap());

    let (client, server) = fake_lsp_client();
    translator.register_client(server_id.clone(), client);
    translator.register_server(
        server_id.clone(),
        LspServer::new_for_test_with_encoding(capabilities, position_encoding),
    );

    (translator, server)
}

/// Fake `sh` LSP server fixtures: a hand-written script has no equivalent on
/// Windows, so everything below is unix-only.
#[cfg(unix)]
mod sh_servers {
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use crate::config::LspServerConfig;
    use crate::lsp::ServerInitConfig;
    use crate::test_lsp::with_read_preamble;

    /// Writes a `sh` script that answers the LSP `initialize` handshake
    /// with a canned response -- request id `1`, since a freshly spawned
    /// `LspClient`'s request counter always starts there -- and then
    /// exits shortly after, so `LspServer::spawn` succeeds but the
    /// process is already dead moments later. Stands in for "the server
    /// was alive, then crashed" without needing a real language server
    /// binary.
    ///
    /// The brief sleep before exiting matters: `LspServer::spawn` sends
    /// the `initialized` notification right after the `initialize`
    /// response arrives, and without it the process can (racily) have
    /// already exited by the time that notification is written to its
    /// stdin, failing the spawn itself instead of the respawn this is
    /// meant to seed.
    pub(in crate::bridge::translator) fn write_crash_after_init_script(dir: &Path) -> PathBuf {
        let script_path = dir.join("crash_after_init.sh");
        let body = with_read_preamble(
            r#"body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
    printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
    sleep 0.3
    "#,
        );
        fs::write(&script_path, body).unwrap();
        script_path
    }

    /// Like [`write_crash_after_init_script`], but stays alive for
    /// `sleep_secs` after responding instead of exiting immediately.
    pub(in crate::bridge::translator) fn write_responder_script(
        dir: &Path,
        sleep_secs: u64,
    ) -> PathBuf {
        let script_path = dir.join("responder.sh");
        let template = with_read_preamble(
            r#"body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
    printf 'Content-Length: %d\r\n\r\n%s' ${#body} "$body"
    sleep __SLEEP__
    "#,
        );
        fs::write(
            &script_path,
            template.replace("__SLEEP__", &sleep_secs.to_string()),
        )
        .unwrap();
        script_path
    }

    pub(in crate::bridge::translator) fn stub_server_config(
        id: &str,
        script: &Path,
    ) -> ServerInitConfig {
        ServerInitConfig {
            server_config: LspServerConfig {
                language_id: id.to_string(),
                command: "sh".to_string(),
                args: vec![script.to_string_lossy().to_string()],
                env: HashMap::new(),
                file_patterns: vec![],
                initialization_options: None,
                settings: None,
                // Generous relative to the sub-second fake scripts these
                // tests spawn, to absorb CI scheduling jitter under
                // concurrent nextest load (a bare `sh` invocation has no
                // real work to do, so this never lengthens the happy path).
                timeout_seconds: 20,
                request_timeout_seconds: 20,
                heuristics: None,
                name: Some(id.to_string()),
                handles: None,
                indexing: crate::bridge::IndexingPolicy::Auto,
            },
            workspace_roots: vec![],
            initialization_options: None,
            position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
        }
    }

    pub(in crate::bridge::translator) fn pid_is_running(pid: u32) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&output.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
    }

    const PROTOCOL_SERVER_BODY: &str = r#"LOG='__LOG__'
echo "started $$" >> "$LOG"
reply() { printf 'Content-Length: %d\r\n\r\n%s' ${#1} "$1"; }
read_msg() {
  content_length=0
  while IFS= read -r header; do
    header=$(printf '%s' "$header" | tr -d '\r')
    [ -z "$header" ] && break
    case "$header" in
      Content-Length:*) content_length=$(printf '%s' "$header" | sed 's/^Content-Length: *//') ;;
    esac
  done
  msg=$(dd bs=1 count="$content_length" 2>/dev/null)
}
reply '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
__PUBLISH__
while true; do
  read_msg
  [ -z "$msg" ] && exit 0
  echo "$msg" >> "$LOG"
  case "$msg" in
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$msg" | sed 's/.*"id":\([0-9]*\).*/\1/')
      reply "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}"
      ;;
    *'"method":"exit"'*) sleep __EXIT_DELAY__; echo exiting >> "$LOG"; exit 0 ;;
  esac
done
"#;

    /// A server that completes `initialize` and then answers the LSP
    /// `shutdown` request, exiting on `exit`. It appends `started <pid>` and
    /// every message it receives to `log`, and publishes one diagnostic for
    /// `publish_uri` straight after `initialize` when given. Any other request
    /// is left unanswered.
    pub(in crate::bridge::translator) fn write_protocol_server_script(
        dir: &Path,
        log: &Path,
        publish_uri: Option<&str>,
    ) -> PathBuf {
        write_protocol_script(dir, log, publish_uri, 0)
    }

    /// As [`write_protocol_server_script`], but waits `exit_delay_secs` after
    /// the `exit` notification before logging `exiting` and exiting: a healthy
    /// server that is slow to stop.
    pub(in crate::bridge::translator) fn write_slow_exit_server_script(
        dir: &Path,
        log: &Path,
        exit_delay_secs: u32,
    ) -> PathBuf {
        write_protocol_script(dir, log, None, exit_delay_secs)
    }

    fn write_protocol_script(
        dir: &Path,
        log: &Path,
        publish_uri: Option<&str>,
        exit_delay_secs: u32,
    ) -> PathBuf {
        let publish = publish_uri.map_or_else(String::new, |uri| {
            format!(
                r#"reply '{{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{{"uri":"{uri}","diagnostics":[{{"range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":1}}}},"message":"boom"}}]}}}}'"#
            )
        });
        let script_path = dir.join("protocol_server.sh");
        let body = with_read_preamble(
            &PROTOCOL_SERVER_BODY
                .replace("__LOG__", &log.display().to_string())
                .replace("__PUBLISH__", &publish)
                .replace("__EXIT_DELAY__", &exit_delay_secs.to_string()),
        );
        fs::write(&script_path, body).unwrap();
        script_path
    }
}

#[cfg(unix)]
pub(super) use sh_servers::{
    pid_is_running, stub_server_config, write_crash_after_init_script,
    write_protocol_server_script, write_responder_script, write_slow_exit_server_script,
};
