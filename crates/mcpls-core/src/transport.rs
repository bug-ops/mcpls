//! Transport selection for the MCP server.
//!
//! This module defines the [`Transport`] enum that controls how the MCP server
//! communicates with clients. Stdio is always available; HTTP transport is
//! opt-in via the `transport-http` Cargo feature.
//!
//! # Selecting a transport
//!
//! Pass a [`Transport`] value to [`crate::serve_with`] to choose the runtime
//! binding. The default entry point [`crate::serve`] always uses
//! [`Transport::Stdio`].
//!
//! # Layout
//!
//! - `config`: `HttpConfig` and its limit newtypes.
//! - `allowlist`: the `Host` and `Origin` allowlist types.
//! - `stdio` and `http`: the transport runners; `shutdown` registers the
//!   process shutdown signal they share.
//! - `session_manager`, `body`, `connection_io`, `lease` and `liveness`: the
//!   HTTP session, request-body and connection plumbing.

/// The transport over which the MCP server communicates with clients.
///
/// # Examples
///
/// See [`crate::serve_with`]'s "Shutdown" section before copying this
/// verbatim: under [`Transport::Stdio`], `main` must call
/// `std::process::exit` rather than returning normally, or `SIGTERM`/
/// `SIGINT` can hang while an MCP client's stdin write end is still open
/// (#308).
///
/// ```rust,ignore
/// use mcpls_core::{Transport, serve_with, ServerConfig};
///
/// #[tokio::main]
/// async fn main() {
///     let config = ServerConfig::load().expect("failed to load config");
///     let result = serve_with(config, Transport::Stdio).await;
///     std::process::exit(if result.is_ok() { 0 } else { 1 });
/// }
/// ```
#[non_exhaustive]
#[allow(
    clippy::large_enum_variant,
    reason = "one `Transport` is built per process; boxing the HTTP variant would only break `Transport::Http(cfg)`"
)]
pub enum Transport {
    /// Standard I/O transport (default).
    ///
    /// Reads from `stdin` and writes to `stdout`. This is the transport used
    /// by MCP clients that launch mcpls as a child process.
    Stdio,

    /// Streamable HTTP transport (MCP spec 2025-11-25).
    ///
    /// Binds a TCP listener and serves the MCP protocol over HTTP, enabling
    /// network-accessible deployments and clients that speak HTTP rather than
    /// stdio. Only available when the `transport-http` feature is enabled.
    #[cfg(feature = "transport-http")]
    #[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
    Http(HttpConfig),
}

#[cfg(feature = "transport-http")]
mod allowlist;
#[cfg(feature = "transport-http")]
mod body;
#[cfg(feature = "transport-http")]
mod config;
#[cfg(feature = "transport-http")]
mod connection_io;
#[cfg(feature = "transport-http")]
mod http;
#[cfg(all(test, feature = "transport-http"))]
mod http_tests;
#[cfg(feature = "transport-http")]
mod lease;
#[cfg(feature = "transport-http")]
mod liveness;
#[cfg(feature = "transport-http")]
mod session_manager;
mod shutdown;
mod stdio;
#[cfg(all(test, feature = "transport-http"))]
mod test_support;

#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
pub use allowlist::{
    AllowedHost, AllowedOrigin, HttpPath, InvalidAllowedHost, InvalidAllowedOrigin, InvalidHttpPath,
};
#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
pub use config::{
    ConnectionLimit, HeaderReadTimeout, HttpConfig, ProbeDeadline, ProbeInterval, RequestBodyLimit,
    ResponseStreamDeadline, SessionLimit, StreamLiveness, WriteStallTimeout,
};
#[cfg(feature = "transport-http")]
pub(crate) use http::run_http;
#[cfg(feature = "transport-http")]
pub(crate) use lease::ListenLeaseSlot;
#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
pub use lease::{LeaseWindow, ListenLease};
pub(crate) use shutdown::ShutdownSignal;
pub(crate) use stdio::run_stdio;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `Transport::Stdio` is always constructible regardless of feature flags.
    #[test]
    fn test_transport_stdio_variant() {
        let t = Transport::Stdio;
        assert!(matches!(t, Transport::Stdio));
    }

    #[cfg(feature = "transport-http")]
    #[test]
    fn test_transport_http_variant() {
        let cfg = HttpConfig::new("127.0.0.1:3002".parse().unwrap());
        let t = Transport::Http(cfg);
        assert!(matches!(t, Transport::Http(_)));
    }
}
