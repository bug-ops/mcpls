//! # mcpls-core
//!
//! Core library for MCP (Model Context Protocol) to LSP (Language Server Protocol) translation.
//!
//! This crate provides the fundamental building blocks for bridging AI agents with
//! language servers, enabling semantic code intelligence through MCP tools.
//!
//! ## Architecture
//!
//! The library is organized into several modules:
//!
//! - [`lsp`] - LSP client implementation for communicating with language servers
//! - [`mcp`] - MCP tool definitions and handlers
//! - [`bridge`] - Translation layer between MCP and LSP protocols
//! - [`config`] - Configuration types and loading
//! - [`mod@error`] - Error types for the library
//!
//! ## Example
//!
//! ```rust,ignore
//! use mcpls_core::{serve, serve_with, Transport, ServerConfig};
//!
//! #[tokio::main]
//! async fn main() {
//!     let config = ServerConfig::load().expect("failed to load config");
//!     // Stdio (default):
//!     let result = serve(config).await;
//!     // HTTP (requires `transport-http` feature):
//!     // let http = mcpls_core::HttpConfig::new("127.0.0.1:3000".parse().unwrap());
//!     // let result = serve_with(config, Transport::Http(http)).await;
//!
//!     // See `serve`/`serve_with`'s "Shutdown" docs: process::exit avoids a
//!     // runtime-shutdown hang under the stdio transport.
//!     std::process::exit(if result.is_ok() { 0 } else { 1 });
//! }
//! ```

#![cfg_attr(
    not(test),
    warn(clippy::arithmetic_side_effects, clippy::indexing_slicing)
)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod bridge;
pub mod config;
pub mod error;
pub mod lsp;
pub mod mcp;
mod redaction;
pub(crate) mod runtime;
pub mod transport;
mod util;

#[cfg(test)]
mod test_lsp;
#[cfg(test)]
mod test_support;

use std::collections::HashSet;
use std::sync::Arc;

use bridge::{NotificationCache, Translator, WorkspaceRoots};
pub use config::{
    ProjectConfigStatus, ProjectConfigTrust, ServerAllowlist, ServerConfig, WorkspaceTrust,
};
use config::{ServerId, ToolRouter};
pub use error::Error;
use mcp::SubscriptionRegistry;
use runtime::{StartPlan, plan_server_starts, shutdown, spawn_lsp_servers_background};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
pub use transport::Transport;
#[cfg(feature = "transport-http")]
use transport::run_http;
#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
pub use transport::{
    AllowedHost, AllowedOrigin, ConnectionLimit, HeaderReadTimeout, HttpConfig, HttpPath,
    InvalidAllowedHost, InvalidAllowedOrigin, InvalidHttpPath, LeaseWindow, ListenLease,
    ProbeDeadline, ProbeInterval, RequestBodyLimit, ResponseStreamDeadline, SessionLimit,
    StreamLiveness, WriteStallTimeout,
};
use transport::{ShutdownSignal, run_stdio};
pub use util::{escape_control, needs_control_escape};

/// Start the MCPLS server with the given configuration over stdio.
///
/// This is the backward-compatible entry point. It is equivalent to calling
/// `serve_with(config, Transport::Stdio)`.
///
/// # Errors
///
/// Returns an error if:
/// - MCP server setup fails
/// - Configuration is invalid
///
/// # Graceful Degradation
///
/// - **All servers succeed**: Service runs normally
/// - **Partial success**: Logs warnings for failures, continues with available servers
/// - **All servers fail**: Keeps serving; tools for the affected languages return `Error::ServerFailedToStart` with the spawn failure
///
/// # Shutdown
///
/// See [`serve_with`]'s "Shutdown" section — this function uses
/// [`Transport::Stdio`], so the same `std::process::exit` requirement
/// applies to callers.
pub async fn serve(config: ServerConfig) -> Result<(), Error> {
    serve_with(config, Transport::Stdio).await
}

/// Plans the server starts on the blocking pool, since it walks the workspace.
///
/// `initialize` still waits for the plan: it is needed to build the router the
/// MCP server answers from. Only the runtime workers stay free.
async fn plan_off_runtime(
    config: &ServerConfig,
    roots: &WorkspaceRoots,
    redactions: &Arc<redaction::Redactions>,
) -> Result<StartPlan, Error> {
    let (config, roots, redactions) = (config.clone(), roots.clone(), Arc::clone(redactions));
    on_blocking_pool(move || plan_server_starts(&config, &roots, &redactions))
        .await
        .map_err(|source| Error::TaskFailed {
            task: error::BackgroundTask::ServerPlanning,
            source,
        })
}

/// Runs `work` on the blocking pool and returns its result, re-raising a panic
/// in it on the caller. A join that failed for any other reason (the runtime
/// shutting down cancelled it) is returned as the error.
pub(crate) async fn on_blocking_pool<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => Ok(value),
        Err(error) => match error.try_into_panic() {
            Ok(payload) => std::panic::resume_unwind(payload),
            Err(cancelled) => Err(cancelled),
        },
    }
}

/// Start the MCPLS server with an explicit transport.
///
/// Performs all shared setup (workspace discovery, LSP spawning, translator
/// initialization, diagnostic pump tasks) and then delegates to the
/// appropriate transport runner.
///
/// # Errors
///
/// Returns an error if:
/// - The MCP server or transport fails to start
/// - Configuration is invalid, including two applicable `[[lsp_servers]]`
///   entries whose per-tool routing is ambiguous in this workspace (shared
///   routing identity, two catch-alls, or the same tool claimed by both) --
///   see `config::ToolRouter::from_configs`
///
/// # DNS rebinding protection (HTTP transport)
///
/// When using `Transport::Http`, the underlying rmcp service validates the
/// inbound `Host` header against an allowlist that defaults to loopback
/// addresses only (`localhost`, `127.0.0.1`, `::1`). Requests with any other
/// `Host` value are rejected with `421 Misdirected Request`.
///
/// If you bind to a non-loopback address (e.g. `0.0.0.0:3000`) and expose the
/// service through a reverse proxy, the proxy must forward `Host: localhost`
/// (or another loopback alias) to the mcpls process. Direct non-loopback
/// access is intentionally blocked to prevent DNS-rebinding attacks.
///
/// # Shutdown
///
/// [`Transport::Stdio`] is backed by `tokio::io::stdin()`, which internally
/// parks an uncancellable blocking-pool thread in a raw `read()` syscall
/// that only returns on more input or EOF. If your `main` uses
/// `#[tokio::main]` and simply returns after awaiting this function, the
/// macro-generated runtime-shutdown wrapper blocks waiting for that thread
/// -- hanging indefinitely on `SIGTERM`/`SIGINT` as long as the MCP
/// client's stdin write end is still open, since that never triggers EOF.
/// Call `std::process::exit` right after this function resolves instead of
/// returning normally from `main`, as in the example below (see mcpls's own
/// `mcpls-cli` binary; tracked as #308). This does not apply to
/// `Transport::Http`, which never touches `tokio::io::stdin()`.
///
/// # Examples
///
/// ```rust,ignore
/// use mcpls_core::{serve_with, Transport, ServerConfig};
///
/// #[tokio::main]
/// async fn main() {
///     let config = ServerConfig::load().expect("failed to load config");
///     let exit_code = match serve_with(config, Transport::Stdio).await {
///         Ok(()) => 0,
///         Err(_) => 1,
///     };
///     // See "Shutdown" above: process::exit avoids a runtime-shutdown hang.
///     std::process::exit(exit_code);
/// }
/// ```
pub async fn serve_with(config: ServerConfig, transport: Transport) -> Result<(), Error> {
    info!("Starting MCPLS server...");

    // Registered before any other startup work -- including
    // `spawn_lsp_servers_background` below, which spawns LSP child processes
    // concurrently on another worker thread -- so a `SIGTERM`/`SIGINT`
    // arriving during config validation, workspace-root heuristics, or LSP
    // spawning is caught rather than hitting the OS's default disposition
    // (immediate termination, skipping the `shutdown()` cleanup below
    // entirely; any LSP child mid-spawn still dies with the lifeline/job
    // binding, see #270 and #526). See
    // `ShutdownSignal`'s docs for why this must be a single instance carried
    // through by value rather than re-registered later.
    let shutdown_signal = ShutdownSignal::new();

    // A programmatic `ServerConfig` skips the validation `load`/`load_from` run.
    config.validate()?;

    let workspace_roots = WorkspaceRoots::from_configured(&config.workspace.roots)?;
    let language_map = config.build_effective_language_map();

    let startup_redactions = Arc::new(redaction::Redactions::for_servers(
        &config.lsp_servers,
        lsp::current_environment(),
    ));
    let plan = plan_off_runtime(&config, &workspace_roots, &startup_redactions).await?;
    let (applicable_configs, refused, refusals) = plan.into_parts();

    info!(
        "Attempting to spawn {} applicable LSP server(s)...",
        applicable_configs.len()
    );

    // Built over the applicable (post-heuristics) configs only: this is where
    // #174's workspace-scoped routing rules (duplicate ServerId, conflicting
    // `handles` claims) are enforced -- a startup error naming the
    // conflicting `[[lsp_servers]]` entries, not a silent drop.
    let router = ToolRouter::from_configs(
        applicable_configs
            .iter()
            .map(lsp::ServerInitConfig::server_config)
            .chain(&refused),
    )?;

    // Built here (rather than alongside `subscription_registry` below) so
    // it can be handed to the translator, which uses it to invalidate a
    // respawned server's stale cached diagnostics -- see
    // `Translator::with_notification_cache`. Independent of `translator`
    // itself, which holds no outer lock: the pump only ever locks this
    // cache, so it never contends with a request handler running an
    // in-flight LSP round-trip.
    let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));

    let mut translator = Translator::new()
        .with_startup_redactions(Arc::clone(&startup_redactions))
        .with_resource_limits(config.workspace.resource_limits())
        .with_extensions(language_map)
        .with_file_patterns(
            config
                .lsp_servers
                .iter()
                .flat_map(|server| server.file_patterns.iter().cloned()),
        )
        .with_router(router)
        .with_notification_cache(Arc::clone(&notification_cache))
        .with_indexing_ready_timeout(config.workspace.indexing_ready_timeout_seconds);
    // moved, not cloned -- `config`'s last use is above
    let (project_config_status, mcp) = (config.project_config_status, config.mcp);
    let max_concurrent_server_starts = config.workspace.max_concurrent_server_starts;
    translator.set_workspace_roots(workspace_roots.clone());

    // Mark applicable servers as "expected" so a tool call that arrives while
    // its server is still initializing gets a clear "still initializing" error
    // (instead of "no server configured"), telling the caller to wait and retry.
    let expected_servers: HashSet<ServerId> = applicable_configs
        .iter()
        .map(|c| c.server_config().id())
        .collect();
    translator.set_expected_servers(expected_servers);
    translator.record_refusals(&refusals);

    // Shared state, built BEFORE LSP initialization so the MCP server can answer
    // `initialize` immediately. LSP servers (which can take minutes to initialize
    // on a large solution, e.g. a 130-project Unity .sln via OmniSharp) are spawned
    // in a background task and registered into this shared translator once ready.
    // Blocking the MCP handshake on LSP init makes slow servers exceed the client's
    // initialize-request timeout (Claude Code: ~60s) -> "Request timed out".
    // `workspace_roots` is fixed for the server's lifetime and cheap to clone,
    // so cache-only handlers (e.g. `get_cached_diagnostics`, `read_resource`)
    // validate a path from their own copy without locking `translator` below.

    let translator = Arc::new(translator);
    // Shared across every session (one per HTTP session, or the sole stdio
    // session): each joins it on its first subscribe -- see
    // `SubscriptionRegistry`.
    let subscription_registry = SubscriptionRegistry::new();

    // Cancels the pump and startup tasks; dropping `serve_with` cancels it too.
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();

    let lsp_init_handle = if applicable_configs.is_empty() {
        if refusals.is_empty() {
            warn!("No applicable LSP servers configured — starting in protocol-only mode");
        } else {
            warn!(
                "The workspace is untrusted and no applicable LSP server was allowed — starting \
                 in protocol-only mode; allow servers with `--allow-server <id>`"
            );
        }
        None
    } else {
        info!(
            "Spawning {} LSP server(s) in the background...",
            applicable_configs.len()
        );
        Some(spawn_lsp_servers_background(
            applicable_configs,
            Arc::clone(&translator),
            Arc::clone(&notification_cache),
            subscription_registry.clone(),
            cancel.clone(),
            workspace_roots.clone(),
            max_concurrent_server_starts,
        ))
    };

    info!("Starting MCP server with rmcp...");
    let mcp_server = mcp::McplsServer::new(
        Arc::clone(&translator),
        Arc::clone(&notification_cache),
        workspace_roots,
        subscription_registry,
        project_config_status,
        mcp,
    );
    info!("MCPLS server initialized successfully");

    let result = match transport {
        Transport::Stdio => {
            info!("Listening for MCP requests on stdio...");
            run_stdio(mcp_server, shutdown_signal).await
        }
        #[cfg(feature = "transport-http")]
        Transport::Http(cfg) => run_http(mcp_server, cfg, shutdown_signal).await,
    };

    shutdown(&cancel, &translator, lsp_init_handle).await;

    info!("MCPLS server shutting down");
    result
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::config::{
        IndexingReadyTimeoutSecs, LanguageId, PositionEncodings, ServerStartConcurrency,
        TimeoutSecs,
    };

    /// #659: a panic in the planning work reaches the caller instead of
    /// being swallowed by the blocking pool.
    #[tokio::test]
    async fn test_on_blocking_pool_returns_the_value_and_propagates_a_panic() {
        assert_eq!(on_blocking_pool(|| 7).await.unwrap(), 7);

        let panicked =
            crate::util::catch_panic(on_blocking_pool(|| -> u8 { panic!("planning exploded") }))
                .await
                .unwrap_err();
        assert_eq!(panicked.message(), "planning exploded");
    }

    // Tests for graceful degradation behavior
    mod graceful_degradation_tests {
        use super::*;
        use crate::config::{DocumentLimit, FilePattern, SearchDepth, ServerCommand, SizeLimit};

        #[tokio::test]
        async fn test_serve_degrades_when_all_servers_fail_to_spawn() {
            use crate::config::{LspServerConfig, ServerStartConcurrency, WorkspaceConfig};

            // A configured server whose command cannot spawn used to make serve()
            // fail synchronously with AllServersFailedToInit.
            // LSP initialization now runs in a background task so the MCP
            // `initialize` handshake is never blocked, which means the spawn
            // failure is handled in the background instead: serve() starts the MCP
            // server in degraded mode (mirroring `test_serve_starts_with_empty_config`)
            // rather than failing fast. Any error it surfaces must therefore be a
            // transport/MCP error from the closed test connection, NOT a fail-fast
            // server-availability error.
            let config = ServerConfig {
                mcp: crate::config::McpConfig::default(),
                workspace: WorkspaceConfig {
                    roots: vec![crate::config::ConfiguredRoot::new("/tmp/test-workspace").unwrap()],
                    position_encodings: PositionEncodings::DEFAULT,
                    language_extensions: vec![],
                    heuristics_max_depth: SearchDepth::DEFAULT,
                    max_documents: DocumentLimit::DEFAULT,
                    max_file_size: SizeLimit::DEFAULT,
                    indexing_ready_timeout_seconds: IndexingReadyTimeoutSecs::DEFAULT,
                    max_concurrent_server_starts: ServerStartConcurrency::DEFAULT,
                },
                lsp_servers: vec![LspServerConfig {
                    language_id: LanguageId::from_static("rust"),
                    command: ServerCommand::from_static("nonexistent-command-that-will-fail-12345")
                        .into(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec![FilePattern::from_static("**/*.rs")],
                    initialization_options: None,
                    settings: None,
                    timeout_seconds: TimeoutSecs::new(10).unwrap(),
                    request_timeout_seconds: TimeoutSecs::new(10).unwrap(),
                    heuristics: None,
                    name: None,
                    handles: None,
                    indexing: crate::bridge::IndexingPolicy::Auto,
                }],
                project_config_status: ProjectConfigStatus::NotIgnored,
                workspace_trust: crate::config::WorkspaceTrust::default(),
            };

            // serve() proceeds to run the MCP server and blocks on the stdio
            // transport until EOF; bound it so the test can't hang if stdin stays
            // open (e.g. under multi-threaded `cargo test`, where several serve()
            // tests share the process stdin).
            let outcome =
                tokio::time::timeout(std::time::Duration::from_secs(2), serve(config)).await;

            match outcome {
                // Still serving after the deadline => it did not fail fast. Good.
                Err(_elapsed) => {}
                // Transport closed cleanly. Also fine.
                Ok(Ok(())) => {}
                // It returned an error: it must not be a fail-fast availability error.
                Ok(Err(err)) => assert!(
                    !matches!(err, Error::AllServersFailedToInit { .. }),
                    "serve() must not fail fast now that LSP init is backgrounded; got: {err:?}"
                ),
            }
        }

        #[tokio::test]
        async fn test_serve_starts_with_empty_config() {
            use crate::config::WorkspaceConfig;

            // Server starts in protocol-only mode when no LSP servers are configured.
            // serve() blocks until the MCP transport closes, so it will error with a
            // connection/transport error — not AllServersFailedToInit.
            let config = ServerConfig {
                mcp: crate::config::McpConfig::default(),
                workspace: WorkspaceConfig {
                    roots: vec![crate::config::ConfiguredRoot::new("/tmp/test-workspace").unwrap()],
                    position_encodings: PositionEncodings::DEFAULT,
                    language_extensions: vec![],
                    heuristics_max_depth: SearchDepth::DEFAULT,
                    max_documents: DocumentLimit::DEFAULT,
                    max_file_size: SizeLimit::DEFAULT,
                    indexing_ready_timeout_seconds: IndexingReadyTimeoutSecs::DEFAULT,
                    max_concurrent_server_starts: ServerStartConcurrency::DEFAULT,
                },
                lsp_servers: vec![],
                project_config_status: ProjectConfigStatus::NotIgnored,
                workspace_trust: crate::config::WorkspaceTrust::default(),
            };

            let result = serve(config).await;

            // serve() may succeed or fail with a transport error, but must NOT
            // return AllServersFailedToInit when the config simply has no servers.
            if let Err(ref err) = result {
                assert!(
                    !matches!(err, Error::AllServersFailedToInit { .. }),
                    "serve() must not return AllServersFailedToInit for empty lsp_servers config"
                );
            }
        }

        /// #348 case 1 (tester-flagged coverage gap): proves `serve_with`
        /// itself skips `current_dir()` for an all-absolute
        /// `workspace.roots`, not just that `WorkspaceRoots::from_configured_with`
        /// tolerates an unread cwd when called directly (see
        /// `test_from_configured_ignores_cwd_when_all_absolute` in
        /// `bridge::workspace_roots`, which never exercises `serve_with`'s
        /// wiring). Mutates the process cwd (chdir into a directory,
        /// then remove it -- `current_dir()` reliably fails afterward on
        /// Unix), so it uses the crate-shared `test_support::CwdGuard` --
        /// the same lock/restore-on-drop `config::tests` uses -- rather than
        /// a one-off guard, since both modules' tests mutate cwd and compile
        /// into one binary. Unix-only since removing a directory that is
        /// still a live process's cwd is a Windows-specific error case, not
        /// the same reproducible `current_dir()` failure.
        #[tokio::test]
        #[cfg(unix)]
        async fn test_serve_with_all_absolute_roots_skips_current_dir() {
            use crate::config::WorkspaceConfig;
            use crate::test_support::CwdGuard;

            // Kept alive for the whole test so the configured workspace root
            // stays a valid, existing absolute directory distinct from the
            // cwd this test is about to remove.
            let workspace_root_dir = tempfile::TempDir::new().unwrap();
            let workspace_root = dunce::canonicalize(workspace_root_dir.path()).unwrap();

            let doomed_cwd = tempfile::TempDir::new().unwrap();
            let _guard = CwdGuard::enter(doomed_cwd.path());
            doomed_cwd.close().unwrap();

            let config = ServerConfig {
                mcp: crate::config::McpConfig::default(),
                workspace: WorkspaceConfig {
                    roots: vec![crate::config::ConfiguredRoot::new(workspace_root).unwrap()],
                    position_encodings: PositionEncodings::DEFAULT,
                    language_extensions: vec![],
                    heuristics_max_depth: SearchDepth::DEFAULT,
                    max_documents: DocumentLimit::DEFAULT,
                    max_file_size: SizeLimit::DEFAULT,
                    indexing_ready_timeout_seconds: IndexingReadyTimeoutSecs::DEFAULT,
                    max_concurrent_server_starts: ServerStartConcurrency::DEFAULT,
                },
                lsp_servers: vec![],
                project_config_status: ProjectConfigStatus::NotIgnored,
                workspace_trust: crate::config::WorkspaceTrust::default(),
            };

            // serve() with no LSP servers configured blocks on the stdio
            // transport, same as `test_serve_starts_with_empty_config`;
            // bound it so the test can't hang.
            let outcome =
                tokio::time::timeout(std::time::Duration::from_secs(2), serve(config)).await;

            match outcome {
                // Still serving after the deadline => it did not fail fast. Good.
                Err(_elapsed) => {}
                // Transport closed cleanly. Also fine.
                Ok(Ok(())) => {}
                // It returned an error: it must not be the `current_dir()`
                // failure this test set up (`ErrorKind::NotFound` from the
                // removed cwd). Narrowed to that specific `io::ErrorKind`
                // rather than any `Error::Io`, since the latter would also
                // match an unrelated IO error from the stdio transport
                // within the timeout window.
                Ok(Err(err)) => assert!(
                    !matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
                    "serve() must not need a working process cwd for an all-absolute \
                     workspace.roots; got: {err:?}"
                ),
            }
        }

        /// #282: a `ServerConfig` built programmatically (not via `load`/
        /// `load_from`, which already run `validate()`) previously skipped
        /// validation entirely, so `serve`/`serve_with` never rejected it —
        /// misconfiguration only surfaced later as silent accessor-level
        /// clamping. `serve` delegates straight to `serve_with`, so
        /// exercising it here also covers `serve_with`'s own `validate()`
        /// call. `validate()` runs before any LSP spawn or transport setup,
        /// so this returns immediately without needing a timeout guard.
        #[tokio::test]
        async fn test_serve_rejects_invalid_caller_supplied_config() {
            use crate::config::{LspServerConfig, ServerStartConcurrency, WorkspaceConfig};

            let config = ServerConfig {
                mcp: crate::config::McpConfig::default(),
                workspace: WorkspaceConfig {
                    roots: vec![],
                    position_encodings: PositionEncodings::DEFAULT,
                    language_extensions: vec![],
                    heuristics_max_depth: SearchDepth::DEFAULT,
                    max_documents: DocumentLimit::DEFAULT,
                    max_file_size: SizeLimit::DEFAULT,
                    indexing_ready_timeout_seconds: IndexingReadyTimeoutSecs::DEFAULT,
                    max_concurrent_server_starts: ServerStartConcurrency::DEFAULT,
                },
                lsp_servers: vec![LspServerConfig {
                    language_id: LanguageId::from_static("rust"),
                    command: ServerCommand::from_static("rust-analyzer").into(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec![FilePattern::from_static("**/*.rs")],
                    initialization_options: None,
                    settings: None,
                    timeout_seconds: TimeoutSecs::new(10).unwrap(),
                    request_timeout_seconds: TimeoutSecs::new(10).unwrap(),
                    heuristics: None,
                    name: None,
                    handles: None,
                    indexing: crate::bridge::IndexingPolicy::Auto,
                }],
                project_config_status: ProjectConfigStatus::NotIgnored,
                workspace_trust: crate::config::WorkspaceTrust::untrusted([
                    crate::config::ServerId::from_static("not-configured"),
                ]),
            };

            // `validate()` runs before any spawn/transport work and should
            // return immediately; bound it anyway so a regression that lets
            // an invalid config reach the stdio transport fails fast with a
            // clear timeout instead of hanging nextest for the default 120s
            // (mirroring the guard on `test_serve_degrades_when_all_servers_fail_to_spawn`).
            let outcome =
                tokio::time::timeout(std::time::Duration::from_secs(2), serve(config)).await;

            match outcome {
                Err(elapsed) => panic!(
                    "serve() must reject the invalid config immediately, not hang until \
                     timeout: {elapsed}"
                ),
                Ok(result) => assert_matches!(
                    result,
                    Err(Error::Config(_)),
                    "serve() must reject a caller-supplied config that allows an unconfigured server \
                     via Error::Config, matching the load_from path; got: {result:?}"
                ),
            }
        }
    }
}
