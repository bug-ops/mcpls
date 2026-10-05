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

/// Configuration for the HTTP transport.
///
/// Passed inside [`Transport::Http`] to control the TCP bind address and the
/// URL path the MCP service is mounted at.
///
/// # Note on DNS rebinding
///
/// `rmcp`'s `StreamableHttpService` validates the `Host` header against an
/// allow-list. mcpls builds it from the loopback names (`localhost`,
/// `127.0.0.1`, `::1`, any port), the bound address when it is a specific
/// non-loopback IP literal (any port), and
/// [`HttpConfig::allowed_hosts`]. There is no wildcard: a bind to `0.0.0.0`
/// or `[::]` allows only the loopback names and the configured hosts, so
/// clients must send a `Host` that matches one of them, or use a reverse
/// proxy that rewrites the `Host` header.
///
/// A request carrying an `Origin` header is accepted only when it names a
/// loopback origin (`localhost`, `127.0.0.1`, `[::1]`) on the bound port or
/// one of [`HttpConfig::allowed_origins`]; anything else, including
/// `Origin: null`, is answered with `403`. A request without `Origin` (every
/// non-browser client) is unaffected.
///
/// [`HttpConfig::allowed_origins`] serves browser pages whose requests reach
/// mcpls with an allowed `Host`, through a tunnel or a reverse proxy. It does
/// not make a `Host` acceptable: the `Host` check runs first, so a deployment
/// behind a non-loopback name needs [`HttpConfig::allowed_hosts`] as well.
///
/// # Examples
///
/// ```rust,ignore
/// use std::net::SocketAddr;
/// use mcpls_core::{HttpConfig, Transport};
///
/// let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap());
/// let transport = Transport::Http(cfg);
/// ```
#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HttpConfig {
    /// TCP address to bind (e.g. `127.0.0.1:3000`).
    pub bind: std::net::SocketAddr,
    /// URL path prefix the MCP service is mounted at (e.g. `"/mcp"`).
    ///
    /// The same service also answers at the root path `/`, regardless of
    /// this value, so a reverse proxy must not rely on `path` alone to
    /// restrict which URLs reach mcpls.
    pub path: HttpPath,
    /// Maximum size of a single POST request body.
    ///
    /// Enforced by `rmcp`'s `StreamableHttpService` while streaming the body,
    /// independent of `Content-Length` or chunked transfer encoding. Requests
    /// exceeding this limit receive `413 Payload Too Large`. Defaults to
    /// [`RequestBodyLimit::DEFAULT`] (4 MiB), which comfortably covers MCP
    /// tool-call request bodies (large results, e.g. from `workspace/symbol`
    /// or bulk edits, are returned in the response, which this limit does not
    /// constrain). Larger values are clamped by [`RequestBodyLimit::new`].
    pub max_request_body: RequestBodyLimit,
    /// Maximum number of concurrent HTTP sessions.
    ///
    /// This is a hard bound, enforced atomically at session creation via a
    /// semaphore — never more than this many sessions can be active at once,
    /// regardless of request concurrency.
    /// Requests that would start a new session beyond this limit receive
    /// `429 Too Many Requests`. Defaults to [`SessionLimit::DEFAULT`].
    pub max_concurrent_sessions: SessionLimit,
    /// How long a session may go without client activity before it is closed.
    pub(crate) session_idle_timeout: IdleTimeout,
    /// Longest a client may take to send a complete request header, and
    /// the longest it may pause between request-body chunks. It is also how
    /// long an idle keep-alive connection stays open.
    ///
    /// Bounds slow-header and slow-body ("slowloris") clients: a stalled
    /// header closes the connection, a stalled body is answered with
    /// `408 Request Timeout`. A whole request body must also arrive within
    /// four times this timeout, and within at least two minutes, so a client
    /// trickling one chunk per window is cut too. It never interrupts a
    /// response, so long-lived SSE streams are unaffected, and it does not
    /// bound a client that stops *reading* a response
    /// ([`HttpConfig::write_stall_timeout`] does). Defaults to
    /// [`HeaderReadTimeout::DEFAULT`].
    pub header_read_timeout: HeaderReadTimeout,
    /// Maximum number of concurrently open TCP connections.
    ///
    /// Further connections wait in the kernel accept queue until one
    /// closes. Defaults to [`ConnectionLimit::DEFAULT`]. Values above what
    /// the underlying semaphore supports are clamped by
    /// [`ConnectionLimit::new`].
    pub max_concurrent_connections: ConnectionLimit,
    /// Liveness probing of each session's standalone GET (SSE) stream.
    ///
    /// Defaults to [`StreamLiveness::DEFAULT`].
    pub stream_liveness: StreamLiveness,
    /// Lifetime of stateless `subscriptions/listen` streams.
    ///
    /// `None` (the default) follows [`HttpConfig::stream_liveness`]: the
    /// default lease while probing, none when it is
    /// [`StreamLiveness::Disabled`].
    pub listen_lease: Option<ListenLease>,
    /// Browser origins accepted in addition to the loopback origins on the
    /// bound port. Empty by default.
    pub allowed_origins: Box<[AllowedOrigin]>,
    /// `Host` header values accepted in addition to the loopback names and
    /// the bound IP literal. Empty by default.
    pub allowed_hosts: Box<[AllowedHost]>,
    /// Longest a POST or request-wise resume response stream stays open,
    /// counted from when it opens.
    ///
    /// A total lifetime bound, not an idle timeout: the stream is cut when it
    /// elapses however much it is still sending, which releases its hold on
    /// the session so the idle reaper can expire it. While the connection's
    /// write to a peer that stopped reading is stuck the body is not polled,
    /// so the cut cannot fire; [`HttpConfig::write_stall_timeout`] closes the
    /// connection and frees its [`HttpConfig::max_concurrent_connections`]
    /// permit, and the session slot is freed after the idle timeout.
    /// Defaults to [`ResponseStreamDeadline::DEFAULT`].
    pub response_stream_deadline: ResponseStreamDeadline,
    /// Longest a write to the peer may stay stalled before the connection is
    /// closed.
    ///
    /// Frees the connection permit of a peer that stops reading a response or
    /// reads it too slowly: a write that goes pending opens a window of this
    /// length, which closes when a flush completes or the peer has taken at
    /// least 2 KiB per second of the window (between 4 KiB and 1 MiB); a
    /// window that expires first closes the connection. A streamed (SSE)
    /// response is flushed after every frame, so each flush closes the window
    /// and the floor there is one frame per window, the same a quiet
    /// legitimate subscriber gives; the rate floor binds responses written in
    /// large buffers. Defaults to [`WriteStallTimeout::DEFAULT`].
    pub write_stall_timeout: WriteStallTimeout,
}

#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
impl HttpConfig {
    /// Create an [`HttpConfig`] mounted at [`HttpPath::default`] with default
    /// body-size, session, timeout and connection caps.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::HttpConfig;
    ///
    /// let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap());
    /// assert_eq!(cfg.path.as_str(), "/mcp");
    /// ```
    #[must_use]
    pub fn new(bind: std::net::SocketAddr) -> Self {
        Self {
            bind,
            path: HttpPath::default(),
            max_request_body: RequestBodyLimit::DEFAULT,
            max_concurrent_sessions: SessionLimit::DEFAULT,
            session_idle_timeout: IdleTimeout::DEFAULT,
            header_read_timeout: HeaderReadTimeout::DEFAULT,
            max_concurrent_connections: ConnectionLimit::DEFAULT,
            stream_liveness: StreamLiveness::DEFAULT,
            listen_lease: None,
            allowed_origins: Box::default(),
            allowed_hosts: Box::default(),
            response_stream_deadline: ResponseStreamDeadline::DEFAULT,
            write_stall_timeout: WriteStallTimeout::DEFAULT,
        }
    }

    /// Override the URL path the MCP service is mounted at.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::{HttpConfig, HttpPath};
    ///
    /// let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap())
    ///     .with_path("/api/mcp".parse::<HttpPath>().unwrap());
    /// assert_eq!(cfg.path.as_str(), "/api/mcp");
    /// ```
    #[must_use]
    pub fn with_path(mut self, path: HttpPath) -> Self {
        self.path = path;
        self
    }

    /// Override the maximum POST request body size.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::{HttpConfig, RequestBodyLimit};
    ///
    /// let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap())
    ///     .with_max_request_body(RequestBodyLimit::new(1024).unwrap());
    /// assert_eq!(cfg.max_request_body.get(), 1024);
    /// ```
    #[must_use]
    pub const fn with_max_request_body(mut self, limit: RequestBodyLimit) -> Self {
        self.max_request_body = limit;
        self
    }

    /// Override the maximum number of concurrent HTTP sessions.
    #[must_use]
    pub const fn with_max_concurrent_sessions(mut self, max: SessionLimit) -> Self {
        self.max_concurrent_sessions = max;
        self
    }

    /// Override the request read (header, body pause, idle keep-alive) timeout.
    #[must_use]
    pub const fn with_header_read_timeout(mut self, timeout: HeaderReadTimeout) -> Self {
        self.header_read_timeout = timeout;
        self
    }

    /// Override the maximum number of concurrently open connections.
    #[must_use]
    pub const fn with_max_concurrent_connections(mut self, max: ConnectionLimit) -> Self {
        self.max_concurrent_connections = max;
        self
    }

    /// Accept browser requests from `origins` in addition to the loopback
    /// origins on the bound port.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::{AllowedOrigin, HttpConfig};
    ///
    /// let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap())
    ///     .with_allowed_origins(["https://app.example.com".parse::<AllowedOrigin>().unwrap()]);
    /// assert_eq!(cfg.allowed_origins.len(), 1);
    /// ```
    #[must_use]
    pub fn with_allowed_origins(
        mut self,
        origins: impl IntoIterator<Item = AllowedOrigin>,
    ) -> Self {
        self.allowed_origins = origins.into_iter().collect();
        self
    }

    /// Accept requests whose `Host` is one of `hosts` in addition to the
    /// loopback names and the bound IP literal.
    ///
    /// Clients and proxies omit the default port from `Host`, so a pin to
    /// `:80` or `:443` is rejected when the [`AllowedHost`] is parsed: list the
    /// host without a port, which matches any port.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::{AllowedHost, HttpConfig};
    ///
    /// let cfg = HttpConfig::new("0.0.0.0:3000".parse().unwrap())
    ///     .with_allowed_hosts(["mcp.example.com".parse::<AllowedHost>().unwrap()]);
    /// assert_eq!(cfg.allowed_hosts.len(), 1);
    /// ```
    #[must_use]
    pub fn with_allowed_hosts(mut self, hosts: impl IntoIterator<Item = AllowedHost>) -> Self {
        self.allowed_hosts = hosts.into_iter().collect();
        self
    }

    /// Override how long a write may stall before the connection is closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcpls_core::{HttpConfig, WriteStallTimeout};
    ///
    /// let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap())
    ///     .with_write_stall_timeout(WriteStallTimeout::new(Duration::from_secs(10)).unwrap());
    /// assert_eq!(cfg.write_stall_timeout.get(), Duration::from_secs(10));
    /// ```
    #[must_use]
    pub const fn with_write_stall_timeout(mut self, timeout: WriteStallTimeout) -> Self {
        self.write_stall_timeout = timeout;
        self
    }

    /// Override how long a POST or resume response stream may stay open.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcpls_core::{HttpConfig, ResponseStreamDeadline};
    ///
    /// let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap())
    ///     .with_response_stream_deadline(ResponseStreamDeadline::new(Duration::from_mins(30)).unwrap());
    /// assert_eq!(cfg.response_stream_deadline.get(), Duration::from_mins(30));
    /// ```
    #[must_use]
    pub const fn with_response_stream_deadline(mut self, deadline: ResponseStreamDeadline) -> Self {
        self.response_stream_deadline = deadline;
        self
    }

    /// Override how standalone GET streams are probed for liveness.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::{HttpConfig, StreamLiveness};
    ///
    /// let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap())
    ///     .with_stream_liveness(StreamLiveness::Disabled);
    /// assert_eq!(cfg.stream_liveness, StreamLiveness::Disabled);
    /// ```
    #[must_use]
    pub const fn with_stream_liveness(mut self, liveness: StreamLiveness) -> Self {
        self.stream_liveness = liveness;
        self
    }

    /// Override the lifetime of stateless `subscriptions/listen` streams
    /// instead of deriving it from [`HttpConfig::stream_liveness`].
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::HttpConfig;
    /// use mcpls_core::transport::ListenLease;
    ///
    /// let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap())
    ///     .with_listen_lease(ListenLease::Unbounded);
    /// assert_eq!(cfg.effective_listen_lease(), ListenLease::Unbounded);
    /// ```
    #[must_use]
    pub const fn with_listen_lease(mut self, lease: ListenLease) -> Self {
        self.listen_lease = Some(lease);
        self
    }

    /// The listen lease in force: the explicit override, else the default
    /// lease while stream liveness probing is on and none when it is off.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::{HttpConfig, StreamLiveness};
    /// use mcpls_core::transport::ListenLease;
    ///
    /// let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap());
    /// assert_eq!(cfg.effective_listen_lease(), ListenLease::default());
    /// let off = cfg.with_stream_liveness(StreamLiveness::Disabled);
    /// assert_eq!(off.effective_listen_lease(), ListenLease::Unbounded);
    /// ```
    #[must_use]
    pub const fn effective_listen_lease(&self) -> ListenLease {
        match (self.listen_lease, self.stream_liveness) {
            (Some(lease), _) => lease,
            (None, StreamLiveness::Probe { .. }) => ListenLease::Renew(LeaseWindow::DEFAULT),
            (None, StreamLiveness::Disabled) => ListenLease::Unbounded,
        }
    }
}

#[cfg(feature = "transport-http")]
macro_rules! non_zero_duration {
    ($(#[$meta:meta])* $vis:vis $name:ident, $default_secs:expr, $default_doc:literal) => {
        $(#[$meta])*
        #[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        $vis struct $name(std::time::Duration);

        impl $name {
            #[doc = $default_doc]
            $vis const DEFAULT: Self = match Self::new(std::time::Duration::from_secs($default_secs)) {
                Some(duration) => duration,
                None => panic!("the default duration must be non-zero"),
            };

            /// `None` for a zero duration.
            #[must_use]
            $vis const fn new(duration: std::time::Duration) -> Option<Self> {
                if duration.is_zero() {
                    None
                } else {
                    Some(Self(duration))
                }
            }

            /// The wrapped duration, never zero.
            #[must_use]
            $vis const fn get(self) -> std::time::Duration {
                self.0
            }
        }
    };
}

#[cfg(feature = "transport-http")]
macro_rules! non_zero_limit {
    (
        $(#[$meta:meta])* $name:ident,
        $default:expr, $default_doc:literal,
        $max:expr, $max_doc:literal
    ) => {
        $(#[$meta])*
        #[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name(usize);

        const _: () = assert!($default <= $max, "the default limit must not exceed the maximum");

        impl $name {
            #[doc = $default_doc]
            pub const DEFAULT: Self = Self($default);

            #[doc = concat!("`None` for zero; larger values are clamped to ", $max_doc, ".")]
            #[must_use]
            pub fn new(max: usize) -> Option<Self> {
                (max > 0).then(|| Self(max.min($max)))
            }

            #[doc = concat!("The wrapped limit, between 1 and ", $max_doc, ".")]
            #[must_use]
            pub const fn get(self) -> usize {
                self.0
            }
        }
    };
}

#[cfg(feature = "transport-http")]
non_zero_duration! {
    /// Non-zero delay between liveness probes on a GET stream.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcpls_core::ProbeInterval;
    ///
    /// assert!(ProbeInterval::new(Duration::ZERO).is_none());
    /// let interval = ProbeInterval::new(Duration::from_mins(1)).unwrap();
    /// assert_eq!(interval.get(), Duration::from_mins(1));
    /// ```
    pub ProbeInterval, 60, "60 seconds."
}

#[cfg(feature = "transport-http")]
non_zero_duration! {
    /// Non-zero time a client has to answer a liveness probe before its GET
    /// stream is closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcpls_core::ProbeDeadline;
    ///
    /// assert!(ProbeDeadline::new(Duration::ZERO).is_none());
    /// let deadline = ProbeDeadline::new(Duration::from_secs(30)).unwrap();
    /// assert_eq!(deadline.get(), Duration::from_secs(30));
    /// ```
    pub ProbeDeadline, 30, "30 seconds."
}

#[cfg(feature = "transport-http")]
non_zero_duration! {
    /// Non-zero time a POST (or request-wise resume) response stream may stay
    /// open before it is cut, for [`HttpConfig::response_stream_deadline`].
    ///
    /// A zero deadline would cut every stream at once, so it is
    /// unrepresentable. The default covers the longest single tool call: two
    /// maximum-length respawn or request waits, an indexing wait and margin.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcpls_core::ResponseStreamDeadline;
    ///
    /// assert!(ResponseStreamDeadline::new(Duration::ZERO).is_none());
    /// let deadline = ResponseStreamDeadline::new(Duration::from_mins(5)).unwrap();
    /// assert_eq!(deadline.get(), Duration::from_mins(5));
    /// assert_eq!(ResponseStreamDeadline::DEFAULT.get(), Duration::from_hours(1));
    /// ```
    pub ResponseStreamDeadline, 3600, "1 hour."
}

#[cfg(feature = "transport-http")]
const _: () = assert!(
    ResponseStreamDeadline::DEFAULT.get().as_secs()
        >= 2 * crate::config::MAX_TIMEOUT_SECONDS
            + crate::bridge::INDEXING_STALENESS_BOUND.as_secs()
            + 300,
    "the default response stream deadline must outlast the longest single tool call"
);

/// Why a string is not a valid [`HttpPath`].
///
/// # Examples
///
/// ```
/// use mcpls_core::{HttpPath, InvalidHttpPath};
///
/// assert_eq!("mcp".parse::<HttpPath>(), Err(InvalidHttpPath::MissingLeadingSlash));
/// ```
#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidHttpPath {
    /// The path is empty or does not start with `/`.
    #[error("must start with `/`")]
    MissingLeadingSlash,
    /// The path is exactly `/`; the service already answers at the root.
    #[error("must not be `/`: the service already answers at the root path")]
    Root,
    /// The path contains `//` or ends with `/`.
    #[error("must not contain an empty segment (`//` or a trailing `/`)")]
    EmptySegment,
    /// The path contains a `.` or `..` segment.
    #[error("must not contain a `.` or `..` segment")]
    DotSegment,
    /// The path contains a character outside `A-Z a-z 0-9 - . _ ~`.
    #[error("contains `{0}`; only ASCII letters, digits and `-._~` are allowed in a segment")]
    InvalidCharacter(char),
}

/// URL path the MCP service is mounted at, validated so mounting it cannot
/// panic.
///
/// Starts with `/`, is not `/` itself, has no empty or dot segments, and each
/// segment uses only ASCII letters, digits and `-._~`. Wildcard, parameter
/// and percent-encoded forms are rejected rather than normalized.
///
/// # Examples
///
/// ```
/// use mcpls_core::{HttpPath, InvalidHttpPath};
///
/// assert_eq!("/api/mcp".parse::<HttpPath>().unwrap().as_str(), "/api/mcp");
/// assert_eq!(HttpPath::default().as_str(), "/mcp");
/// assert_eq!("/".parse::<HttpPath>(), Err(InvalidHttpPath::Root));
/// assert_eq!("/a/{id}".parse::<HttpPath>(), Err(InvalidHttpPath::InvalidCharacter('{')));
/// ```
#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpPath(Box<str>);

#[cfg(feature = "transport-http")]
impl HttpPath {
    /// The validated path, starting with `/`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(feature = "transport-http")]
impl Default for HttpPath {
    fn default() -> Self {
        Self("/mcp".into())
    }
}

#[cfg(feature = "transport-http")]
impl std::fmt::Display for HttpPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(feature = "transport-http")]
impl std::str::FromStr for HttpPath {
    type Err = InvalidHttpPath;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s
            .strip_prefix('/')
            .ok_or(InvalidHttpPath::MissingLeadingSlash)?;
        if rest.is_empty() {
            return Err(InvalidHttpPath::Root);
        }
        for segment in rest.split('/') {
            if segment.is_empty() {
                return Err(InvalidHttpPath::EmptySegment);
            }
            if matches!(segment, "." | "..") {
                return Err(InvalidHttpPath::DotSegment);
            }
            if let Some(c) = segment
                .chars()
                .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')))
            {
                return Err(InvalidHttpPath::InvalidCharacter(c));
            }
        }
        Ok(Self(s.into()))
    }
}

/// Why a string is not a valid [`AllowedOrigin`].
///
/// # Examples
///
/// ```
/// use mcpls_core::{AllowedOrigin, InvalidAllowedOrigin};
///
/// assert_eq!("*".parse::<AllowedOrigin>(), Err(InvalidAllowedOrigin::Wildcard));
/// assert_eq!("null".parse::<AllowedOrigin>(), Err(InvalidAllowedOrigin::Null));
/// ```
#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidAllowedOrigin {
    /// The string is not a URI, or its authority is not a host with an
    /// optional port (an IPv6 host needs brackets).
    #[error("not a valid origin")]
    Malformed,
    /// `null`, the origin of sandboxed and opaque contexts.
    #[error("`null` is not an origin")]
    Null,
    /// A wildcard such as `*` or `:*`.
    #[error("wildcards are not allowed")]
    Wildcard,
    /// The scheme is missing or is neither `http` nor `https`.
    #[error("the scheme must be `http` or `https`")]
    UnsupportedScheme,
    /// The authority carries `user:password@`.
    #[error("user information is not allowed")]
    UserInfo,
    /// The host is empty.
    #[error("the host is missing")]
    MissingHost,
    /// A path other than `/`.
    #[error("a path is not allowed")]
    Path,
    /// A query string.
    #[error("a query is not allowed")]
    Query,
}

/// `http` or `https`, the schemes a browser page can have.
#[cfg(feature = "transport-http")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OriginScheme {
    Http,
    Https,
}

#[cfg(feature = "transport-http")]
impl OriginScheme {
    const fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// A browser origin allowed to reach the HTTP transport, validated so it
/// round-trips through `rmcp`'s allowlist.
///
/// `rmcp` silently drops allowlist entries it cannot parse, so an invalid
/// string would leave the origin blocked with no error. This type is built
/// only from `http://` or `https://` origins with a host and no userinfo,
/// path or query. The host is lowercased, IPv6 hosts keep their brackets, and
/// a missing port becomes the scheme default, and a port that is not a
/// number up to 65535 is rejected, so every entry names exactly one
/// `(scheme, host, port)`. Surrounding whitespace is ignored.
///
/// # Examples
///
/// ```
/// use mcpls_core::AllowedOrigin;
///
/// let origin: AllowedOrigin = "https://App.Example.com".parse().unwrap();
/// assert_eq!(origin.to_string(), "https://app.example.com:443");
/// assert_eq!("http://[::1]:8080".parse::<AllowedOrigin>().unwrap().to_string(), "http://[::1]:8080");
/// assert!("https://example.com/path".parse::<AllowedOrigin>().is_err());
/// ```
#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedOrigin {
    scheme: OriginScheme,
    host: Box<str>,
    port: u16,
}

#[cfg(feature = "transport-http")]
impl AllowedOrigin {
    /// The origins of a page served by a loopback listener on `port`:
    /// `localhost`, `127.0.0.1` and `[::1]`.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::AllowedOrigin;
    ///
    /// let origins = AllowedOrigin::loopback(3000);
    /// assert_eq!(origins[1].to_string(), "http://127.0.0.1:3000");
    /// ```
    #[must_use]
    pub fn loopback(port: u16) -> [Self; 3] {
        ["localhost", "127.0.0.1", "[::1]"].map(|host| Self {
            scheme: OriginScheme::Http,
            host: host.into(),
            port,
        })
    }
}

#[cfg(feature = "transport-http")]
impl std::fmt::Display for AllowedOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}:{}", self.scheme.as_str(), self.host, self.port)
    }
}

#[cfg(feature = "transport-http")]
impl std::str::FromStr for AllowedOrigin {
    type Err = InvalidAllowedOrigin;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use axum::http::Uri;

        let s = s.trim();
        if s.eq_ignore_ascii_case("null") {
            return Err(InvalidAllowedOrigin::Null);
        }
        if s.contains('*') {
            return Err(InvalidAllowedOrigin::Wildcard);
        }
        let uri: Uri = s.parse().map_err(|_| InvalidAllowedOrigin::Malformed)?;
        let scheme = match uri.scheme_str() {
            Some("http") => OriginScheme::Http,
            Some("https") => OriginScheme::Https,
            _ => return Err(InvalidAllowedOrigin::UnsupportedScheme),
        };
        let authority = uri.authority().ok_or(InvalidAllowedOrigin::Malformed)?;
        if authority.as_str().contains('@') {
            return Err(InvalidAllowedOrigin::UserInfo);
        }
        if uri.path() != "/" {
            return Err(InvalidAllowedOrigin::Path);
        }
        if uri.query().is_some() {
            return Err(InvalidAllowedOrigin::Query);
        }
        let host = authority.host();
        if host.is_empty() {
            return Err(InvalidAllowedOrigin::MissingHost);
        }
        let port = match AuthorityPort::of(authority).ok_or(InvalidAllowedOrigin::Malformed)? {
            AuthorityPort::Absent | AuthorityPort::Empty => scheme.default_port(),
            AuthorityPort::Number(port) => port,
        };
        Ok(Self {
            scheme,
            host: host.to_ascii_lowercase().into(),
            port,
        })
    }
}

/// The port part of an [`axum::http::uri::Authority`].
#[cfg(feature = "transport-http")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthorityPort {
    /// No `:` after the host.
    Absent,
    /// A `:` with no digits after it.
    Empty,
    /// A decimal port that fits in `u16`.
    Number(u16),
}

#[cfg(feature = "transport-http")]
impl AuthorityPort {
    /// `None` when the text after the host is not `:` followed by decimal
    /// digits of a `u16`.
    fn of(authority: &axum::http::uri::Authority) -> Option<Self> {
        match authority.as_str().get(authority.host().len()..)? {
            "" => Some(Self::Absent),
            ":" => Some(Self::Empty),
            suffix => suffix
                .strip_prefix(':')
                .filter(|digits| digits.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|digits| digits.parse().ok())
                .map(Self::Number),
        }
    }
}

/// Why a string is not a valid [`AllowedHost`].
///
/// # Examples
///
/// ```
/// use mcpls_core::{AllowedHost, InvalidAllowedHost};
///
/// assert_eq!("*".parse::<AllowedHost>(), Err(InvalidAllowedHost::Wildcard));
/// assert_eq!("example.com:".parse::<AllowedHost>(), Err(InvalidAllowedHost::InvalidPort));
/// assert_eq!("example.com:443".parse::<AllowedHost>(), Err(InvalidAllowedHost::DefaultPort));
/// ```
#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidAllowedHost {
    /// The string is not a host with an optional port (an IPv6 host needs
    /// brackets), or carries a scheme or path.
    #[error("not a valid host")]
    Malformed,
    /// A wildcard such as `*` or `*.example.com`.
    #[error("wildcards are not allowed")]
    Wildcard,
    /// The authority carries `user:password@`.
    #[error("user information is not allowed")]
    UserInfo,
    /// The host is empty.
    #[error("the host is missing")]
    MissingHost,
    /// The port is empty (`host:`), `0`, or is not a number up to 65535.
    #[error("the port must be a number from 1 to 65535")]
    InvalidPort,
    /// The port is `80` or `443`. Clients and proxies omit these from `Host`,
    /// so a pin to one would answer `403` to nearly every client.
    #[error(
        "ports 80 and 443 are omitted from `Host` by clients, so the pin would never match; \
         list the host without a port"
    )]
    DefaultPort,
    /// The string has a non-ASCII character; `Host` carries the punycode
    /// (`xn--`) form of an internationalized name.
    #[error("only ASCII is allowed; write an internationalized name in punycode (`xn--...`)")]
    NonAscii,
    /// The host ends with a `.` (a fully qualified name). `rmcp` compares
    /// hosts as plain lowercase strings, so the dotted and undotted forms
    /// are different hosts; list the form clients send, without the dot.
    #[error("a trailing dot is not allowed; list the name without it")]
    TrailingDot,
}

/// A port an [`AllowedHost`] can be pinned to: 1 to 65535, never `80` or `443`.
///
/// Clients and proxies omit the default ports from `Host`, and `0` never
/// appears in it, so none of the three can match a request.
#[cfg(feature = "transport-http")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PinnedPort(std::num::NonZeroU16);

#[cfg(feature = "transport-http")]
impl PinnedPort {
    fn new(port: u16) -> Result<Self, InvalidAllowedHost> {
        match port {
            80 | 443 => Err(InvalidAllowedHost::DefaultPort),
            _ => std::num::NonZeroU16::new(port)
                .map(Self)
                .ok_or(InvalidAllowedHost::InvalidPort),
        }
    }

    const fn get(self) -> u16 {
        self.0.get()
    }
}

/// A `Host` header value allowed to reach the HTTP transport, validated so it
/// round-trips through `rmcp`'s allowlist.
///
/// `rmcp` falls back to matching an entry it cannot parse as a raw host name,
/// so an invalid string would leave the host blocked with no error. This type
/// is a host with an optional port, nothing else: no wildcard, userinfo,
/// scheme or path. The host is lowercased and IPv6 hosts keep their brackets.
/// `rmcp` compares hosts as plain strings, so a trailing dot and non-ASCII
/// (use punycode) are rejected rather than normalized. Without a port, any
/// port matches; with one, only that port does, and a request that omits the
/// port (clients and proxies omit `:80` and `:443`) does not match, so list
/// the host without a port unless a non-default port is really meant. A pin to
/// `80`, `443` or `0` is therefore rejected at parse time.
/// Surrounding whitespace is ignored.
///
/// # Examples
///
/// ```
/// use mcpls_core::AllowedHost;
///
/// let any_port: AllowedHost = "MCP.Example.com".parse().unwrap();
/// assert_eq!(any_port.to_string(), "mcp.example.com");
/// assert_eq!(any_port.port(), None);
///
/// let pinned: AllowedHost = "[::1]:8080".parse().unwrap();
/// assert_eq!(pinned.to_string(), "[::1]:8080");
/// assert_eq!(pinned.port(), Some(8080));
/// assert!("https://example.com".parse::<AllowedHost>().is_err());
/// assert!("example.com:443".parse::<AllowedHost>().is_err());
/// ```
#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedHost {
    host: Box<str>,
    port: Option<PinnedPort>,
}

#[cfg(feature = "transport-http")]
impl AllowedHost {
    /// The loopback names `localhost`, `127.0.0.1` and `[::1]`, on any port.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::AllowedHost;
    ///
    /// assert_eq!(AllowedHost::loopback()[1].to_string(), "127.0.0.1");
    /// ```
    #[must_use]
    pub fn loopback() -> [Self; 3] {
        ["localhost", "127.0.0.1", "[::1]"].map(|host| Self {
            host: host.into(),
            port: None,
        })
    }

    /// The host name or IP literal, lowercased; an IPv6 literal keeps its
    /// brackets.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port the host is pinned to, or `None` for any port. Never `0`,
    /// `80` or `443`.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.port.map(PinnedPort::get)
    }

    /// An IP literal on any port: a client reaching a `:80` or `:443` bind
    /// sends no port in `Host`.
    fn ip_literal(ip: std::net::IpAddr) -> Self {
        let host = match ip {
            std::net::IpAddr::V4(ip) => ip.to_string(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        };
        Self {
            host: host.into(),
            port: None,
        }
    }
}

#[cfg(feature = "transport-http")]
impl std::fmt::Display for AllowedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.port() {
            Some(port) => write!(f, "{}:{port}", self.host),
            None => f.write_str(&self.host),
        }
    }
}

#[cfg(feature = "transport-http")]
impl std::str::FromStr for AllowedHost {
    type Err = InvalidAllowedHost;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use axum::http::uri::Authority;

        let s = s.trim();
        if s.contains('*') {
            return Err(InvalidAllowedHost::Wildcard);
        }
        if s.contains('@') {
            return Err(InvalidAllowedHost::UserInfo);
        }
        if !s.is_ascii() {
            return Err(InvalidAllowedHost::NonAscii);
        }
        let authority = Authority::try_from(s).map_err(|_| InvalidAllowedHost::Malformed)?;
        let host = authority.host();
        if host.is_empty() {
            return Err(InvalidAllowedHost::MissingHost);
        }
        if host.ends_with('.') {
            return Err(InvalidAllowedHost::TrailingDot);
        }
        let port = match AuthorityPort::of(&authority).ok_or(InvalidAllowedHost::InvalidPort)? {
            AuthorityPort::Absent => None,
            AuthorityPort::Empty => return Err(InvalidAllowedHost::InvalidPort),
            AuthorityPort::Number(port) => Some(PinnedPort::new(port)?),
        };
        Ok(Self {
            host: host.to_ascii_lowercase().into(),
            port,
        })
    }
}

/// How a session's standalone GET (SSE) stream is checked for a dead peer.
///
/// With [`StreamLiveness::Probe`], mcpls sends an MCP `ping` request on the
/// stream every `interval`. A client that does not answer it (by sending the
/// JSON-RPC response in a POST) within `deadline` has its stream closed and the
/// resources behind it released. This is portable, needs no socket options,
/// and also detects a peer that vanished behind a reverse proxy, which TCP
/// keepalive cannot.
///
/// A client that ignores server `ping` requests is closed and reconnects
/// every `interval + deadline`; use [`StreamLiveness::Disabled`] for such
/// clients.
///
/// # Examples
///
/// ```
/// use std::assert_matches;
///
/// use mcpls_core::StreamLiveness;
///
/// assert_matches!(StreamLiveness::default(), StreamLiveness::Probe { .. });
/// ```
#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamLiveness {
    /// Never probe; a vanished peer is noticed only when the OS gives up on
    /// the connection. A POST or resume stream is still cut once it has been
    /// open for [`ResponseStreamDeadline`]; a peer that stopped reading a
    /// response loses its connection after [`HttpConfig::write_stall_timeout`]
    /// and its session slot after the idle timeout, which no probe could
    /// free sooner either. Without proof of life an
    /// open GET stream does not keep its session alive: the session expires
    /// after the idle timeout (5 minutes) without inbound requests, even while
    /// the stream receives notifications.
    Disabled,
    /// Probe every `interval`; close the stream when unanswered for
    /// `deadline`.
    Probe {
        /// Delay between the end of one probe and the start of the next.
        interval: ProbeInterval,
        /// Time a client has to answer a probe.
        deadline: ProbeDeadline,
    },
}

#[cfg(feature = "transport-http")]
impl StreamLiveness {
    /// Probe every 60 seconds with a 30 second answer deadline.
    pub const DEFAULT: Self = Self::Probe {
        interval: ProbeInterval::DEFAULT,
        deadline: ProbeDeadline::DEFAULT,
    };
}

#[cfg(feature = "transport-http")]
impl Default for StreamLiveness {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(feature = "transport-http")]
non_zero_duration! {
    /// Non-zero request read timeout for [`HttpConfig::header_read_timeout`].
    ///
    /// A zero duration would drop every connection, so it is unrepresentable.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcpls_core::HeaderReadTimeout;
    ///
    /// assert!(HeaderReadTimeout::new(Duration::ZERO).is_none());
    /// let timeout = HeaderReadTimeout::new(Duration::from_secs(10)).unwrap();
    /// assert_eq!(timeout.get(), Duration::from_secs(10));
    /// ```
    pub HeaderReadTimeout, 30, "30 seconds."
}

#[cfg(feature = "transport-http")]
non_zero_duration! {
    /// Non-zero time a write to the peer may make no progress before the
    /// connection is closed, for [`HttpConfig::write_stall_timeout`].
    ///
    /// A zero timeout would close every connection that cannot flush at once,
    /// so it is unrepresentable.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcpls_core::WriteStallTimeout;
    ///
    /// assert!(WriteStallTimeout::new(Duration::ZERO).is_none());
    /// assert_eq!(WriteStallTimeout::DEFAULT.get(), Duration::from_secs(30));
    /// ```
    pub WriteStallTimeout, 30, "30 seconds."
}

#[cfg(feature = "transport-http")]
non_zero_limit! {
    /// Non-zero open-connection cap for [`HttpConfig::max_concurrent_connections`],
    /// clamped to what [`tokio::sync::Semaphore`] supports.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::ConnectionLimit;
    ///
    /// assert!(ConnectionLimit::new(0).is_none());
    /// assert_eq!(ConnectionLimit::new(8).unwrap().get(), 8);
    /// assert!(ConnectionLimit::new(usize::MAX).is_some());
    /// ```
    ConnectionLimit,
    512,
    "512: room for 100 sessions each holding a GET stream, a `subscriptions/listen` stream and one in-flight POST.",
    tokio::sync::Semaphore::MAX_PERMITS,
    "the most a [`tokio::sync::Semaphore`] supports"
}

#[cfg(feature = "transport-http")]
non_zero_limit! {
    /// Non-zero cap on concurrent HTTP sessions for
    /// [`HttpConfig::max_concurrent_sessions`], clamped to what
    /// [`tokio::sync::Semaphore`] supports.
    ///
    /// A zero cap would reject every session, so it is unrepresentable.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::SessionLimit;
    ///
    /// assert!(SessionLimit::new(0).is_none());
    /// assert_eq!(SessionLimit::new(5).unwrap().get(), 5);
    /// assert_eq!(SessionLimit::DEFAULT.get(), 100);
    /// ```
    SessionLimit,
    100,
    "100 concurrent sessions.",
    tokio::sync::Semaphore::MAX_PERMITS,
    "the most a [`tokio::sync::Semaphore`] supports"
}

#[cfg(feature = "transport-http")]
non_zero_limit! {
    /// Non-zero cap, in bytes, on a single POST request body for
    /// [`HttpConfig::max_request_body`], clamped to 64 MiB.
    ///
    /// `rmcp` buffers a body in memory while enforcing the cap, so the
    /// maximum bounds that buffering. A zero cap would reject every POST
    /// body, so it is unrepresentable.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::RequestBodyLimit;
    ///
    /// assert!(RequestBodyLimit::new(0).is_none());
    /// assert_eq!(RequestBodyLimit::new(1024).unwrap().get(), 1024);
    /// assert_eq!(RequestBodyLimit::DEFAULT.get(), 4 * 1024 * 1024);
    /// assert_eq!(RequestBodyLimit::new(usize::MAX).unwrap().get(), 64 * 1024 * 1024);
    /// ```
    RequestBodyLimit,
    4 * 1024 * 1024,
    "4 MiB, matching `rmcp`'s own default.",
    64 * 1024 * 1024,
    "64 MiB"
}

#[cfg(feature = "transport-http")]
use std::sync::Mutex as StdMutex;

use rmcp::ServiceExt as _;
#[cfg(feature = "transport-http")]
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
#[cfg(feature = "transport-http")]
use rmcp::transport::streamable_http_server::session::local::{
    LocalSessionManager, LocalSessionManagerError,
};
#[cfg(feature = "transport-http")]
use rmcp::transport::streamable_http_server::session::{
    ServerSseMessage, SessionId, SessionManager,
};

#[cfg(feature = "transport-http")]
use crate::bridge::lock_std;

#[cfg(feature = "transport-http")]
mod connection_io;
#[cfg(feature = "transport-http")]
mod lease;
#[cfg(feature = "transport-http")]
mod liveness;
#[cfg(feature = "transport-http")]
use connection_io::ConnectionIo;
#[cfg(feature = "transport-http")]
pub(crate) use lease::ListenLeaseSlot;
#[cfg(feature = "transport-http")]
#[cfg_attr(docsrs, doc(cfg(feature = "transport-http")))]
pub use lease::{LeaseWindow, ListenLease};
#[cfg(feature = "transport-http")]
use liveness::{ProbeId, SessionLiveness, StreamProbe, is_common_channel_event_id};

/// Log-safe correlation handle for a session: eight hex digits of a hash of
/// the id, so log lines can be matched without disclosing the bearer secret.
#[cfg(feature = "transport-http")]
struct SessionFingerprint<'a>(&'a SessionId);

#[cfg(feature = "transport-http")]
impl std::fmt::Display for SessionFingerprint<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::hash::{DefaultHasher, Hash as _, Hasher as _};

        let mut hasher = DefaultHasher::new();
        self.0.hash(&mut hasher);
        write!(f, "{:08x}", hasher.finish() >> 32)
    }
}

/// A registered handle for waiting on a shutdown signal: `SIGTERM`/`SIGINT`
/// on Unix (as sent by containers, systemd, and `Ctrl-C`) or `Ctrl-C` on
/// Windows.
///
/// Constructed once by [`crate::serve_with`], *before* any startup work
/// (LSP-server discovery heuristics, `spawn_lsp_servers_background`) runs,
/// and moved by value into whichever transport (`run_stdio`/`run_http`) ends
/// up serving. Registering this early — rather than inside the transport
/// function itself — closes the startup window between process start and the
/// transport loop, during which a signal would otherwise hit the OS's
/// default disposition (immediate termination, bypassing
/// [`crate::bridge::Translator::shutdown_servers`] and risking an orphaned
/// LSP child process that `spawn_lsp_servers_background` is mid-spawning;
/// see #270).
///
/// Every signal kind is held as its own persistent stream
/// (`tokio::signal::unix::Signal` / `tokio::signal::windows::CtrlC`) for the
/// lifetime of this value, rather than re-registered on every
/// [`ShutdownSignal::recv`] call via `tokio::signal::ctrl_c()`: a signal
/// delivered while a *specific* listener isn't being polled is only observed
/// by that same listener's next poll — a freshly (re-)subscribed one starts
/// at the broadcast's current version and never sees it (tokio
/// `signal/registry.rs`). Since [`recv`](ShutdownSignal::recv) is awaited
/// from more than one call site — both by [`run_stdio`], which races it
/// against the MCP handshake and then the post-handshake serve loop, and
/// across the gap between construction in `serve_with` and the first await
/// inside the transport — a fresh registration per call would risk losing a
/// signal delivered in between.
///
/// This instance is dropped as soon as the transport function it was moved
/// into returns — but that does *not* deregister the OS-level handler:
/// `tokio::signal` installs it once per process and never uninstalls it, no
/// matter how many `ShutdownSignal`s are constructed or dropped. What
/// dropping the last live instance actually does is remove the only
/// receiver a delivered signal could be broadcast to, so until a new one
/// subscribes, a signal is recorded and then silently discarded rather than
/// observed by anything — making that stretch of code uninterruptible
/// rather than unsafe. [`crate::shutdown`] (the post-transport cleanup run
/// immediately after) registers a *second* `ShutdownSignal` of its own so a
/// repeat signal during cleanup has a receiver again and can force an exit;
/// see #329.
pub(crate) struct ShutdownSignal {
    #[cfg(unix)]
    sigterm: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    sigint: Option<tokio::signal::unix::Signal>,
    #[cfg(windows)]
    ctrl_c: Option<tokio::signal::windows::CtrlC>,
}

impl ShutdownSignal {
    /// Registers the process's shutdown signal handler(s) up front.
    pub(crate) fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let sigterm = match signal(SignalKind::terminate()) {
                Ok(sigterm) => Some(sigterm),
                Err(e) => {
                    tracing::warn!(
                        "SIGTERM handler registration failed ({e}), SIGTERM will not be caught"
                    );
                    None
                }
            };
            let sigint = match signal(SignalKind::interrupt()) {
                Ok(sigint) => Some(sigint),
                Err(e) => {
                    tracing::warn!(
                        "SIGINT handler registration failed ({e}), SIGINT will not be caught"
                    );
                    None
                }
            };
            Self { sigterm, sigint }
        }
        #[cfg(windows)]
        {
            let ctrl_c = match tokio::signal::windows::ctrl_c() {
                Ok(ctrl_c) => Some(ctrl_c),
                Err(e) => {
                    tracing::warn!("Ctrl-C handler registration failed ({e})");
                    None
                }
            };
            Self { ctrl_c }
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self {}
        }
    }

    /// Waits for the next shutdown signal. May be awaited repeatedly.
    pub(crate) async fn recv(&mut self) {
        #[cfg(unix)]
        {
            match (self.sigterm.as_mut(), self.sigint.as_mut()) {
                (Some(sigterm), Some(sigint)) => {
                    tokio::select! {
                        _ = sigterm.recv() => {},
                        _ = sigint.recv() => {},
                    }
                }
                (Some(sigterm), None) => {
                    sigterm.recv().await;
                }
                (None, Some(sigint)) => {
                    sigint.recv().await;
                }
                (None, None) => {
                    // Both registrations failed above; fall back to a
                    // one-shot listener so shutdown is still possible, even
                    // though it doesn't carry the same across-calls
                    // durability the held streams above do (see the struct
                    // docs).
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
        #[cfg(windows)]
        {
            match self.ctrl_c.as_mut() {
                Some(ctrl_c) => {
                    ctrl_c.recv().await;
                }
                None => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            // No persistent listener is available on this platform; same
            // caveat as the Unix double-registration-failure fallback above.
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// Run the MCP server over stdio.
///
/// Serves the given `mcp_server` using stdin/stdout. Returns as soon as either
/// the stdio transport closes (client disconnect / stdin EOF) or a `SIGTERM`/
/// `SIGINT` is received, so callers can run orderly cleanup — such as
/// [`crate::bridge::Translator::shutdown_servers`] — before the process
/// exits. `shutdown_signal` is dropped when this function returns, and this
/// function does no draining of its own after a signal arrives — so a
/// repeat signal during the post-return cleanup in [`crate::shutdown`] is
/// caught only by the second `ShutdownSignal` that function registers for
/// itself, not by this one (see [`ShutdownSignal`]'s docs and #329).
///
/// `shutdown_signal` is constructed by [`crate::serve_with`] *before* any
/// startup work runs (see [`ShutdownSignal`]'s docs) and is raced here
/// against both the MCP handshake and, once it completes, the
/// post-handshake serve loop. `serve(..)` awaits the full MCP `initialize`
/// handshake internally (reading the client's request and writing the
/// response) before resolving, so a signal arriving during that wait — which
/// can be indefinite if the client is slow to send `initialize` — must be
/// caught there too, not only after the handshake finishes. On signal, the
/// in-flight handshake or `RunningService` is dropped rather than awaited to
/// completion; `rmcp` closes it asynchronously in that case, which is
/// acceptable here since the process exits shortly after -- callers must
/// exit via `std::process::exit` rather than returning normally from `main`,
/// or an uncancellable `tokio::io::stdin()` blocking thread can stall
/// runtime shutdown indefinitely (see `mcpls-cli`'s `main.rs` and #308).
pub(crate) async fn run_stdio(
    mcp_server: crate::mcp::McplsServer,
    mut shutdown_signal: ShutdownSignal,
) -> Result<(), crate::Error> {
    let service = tokio::select! {
        result = mcp_server.serve(rmcp::transport::stdio()) => {
            result.map_err(|e| crate::Error::McpServerStart(Box::new(e)))?
        }
        () = shutdown_signal.recv() => {
            tracing::info!("shutdown signal received during handshake, stopping stdio transport");
            return Ok(());
        }
    };

    tokio::select! {
        result = service.waiting() => result
            .map(|_| ())
            .map_err(|source| crate::Error::TaskFailed {
                task: crate::error::BackgroundTask::McpService,
                source,
            }),
        () = shutdown_signal.recv() => {
            tracing::info!("shutdown signal received, stopping stdio transport");
            Ok(())
        }
    }
}

/// Run the MCP server over Streamable HTTP (MCP spec 2025-11-25).
///
/// Binds `cfg.bind`, mounts the MCP service at `cfg.path` (and `/`), and
/// serves until `Ctrl-C` or `SIGTERM` is received.
///
/// Each HTTP session receives its own `McplsServer` instance (see
/// [`crate::mcp::McplsServer::for_new_session`]). The shared `Arc<Translator>`
/// inside is the same across all sessions, so LSP state is still global per
/// process.
///
/// # Resource update notifications
///
/// `resources/updated` goes to each session's standalone GET (SSE) stream, and
/// only for the URIs that session itself subscribed to (`resources/subscribe`
/// must come from a session established via the `initialize` handshake).
/// rmcp caches the last 16 GET-stream events per session
/// (`SessionConfig::DEFAULT_CHANNEL_CAPACITY`) and replays them when a GET
/// opens, including when a dead primary GET is replaced, so a client may see
/// duplicates. A second GET opened while the first is still considered alive
/// is a shadow stream that receives no notifications; once the first ends, a
/// newly opened GET becomes the primary.
///
/// # Stream liveness
///
/// With [`StreamLiveness::Probe`] (the default), each session's standalone GET
/// stream is probed with an MCP `ping` request every
/// [`ProbeInterval::DEFAULT`] and closed when the client does not answer by
/// sending the JSON-RPC response in a POST within [`ProbeDeadline::DEFAULT`]. The probe
/// runs inside mcpls, so it needs no socket options and also catches a peer
/// that vanished behind a reverse proxy. Probe replies are consumed and never
/// forwarded to the MCP service. Closing ends only the GET stream, not the
/// session; the client's reconnect (with `Last-Event-ID`) resumes it. A client
/// that never answers server `ping` requests would be disconnected every
/// interval plus deadline, so [`StreamLiveness::Disabled`] switches probing
/// off. Request-wise resumes are not probed.
///
/// Stateless `subscriptions/listen` streams cannot be probed (no session, and
/// a client cannot answer a server `ping`), so they get a lease instead
/// ([`ListenLease`], default 15 to 30 minutes with random jitter): the HTTP
/// body then ends abruptly, without a final result, which a client reads as a
/// transport close and answers by listening again. The replay on listen covers
/// the gap. A client that never re-listens stops receiving push updates after
/// one lease but can still read resources. The lease is off together with
/// [`StreamLiveness::Disabled`] unless [`HttpConfig::with_listen_lease`] says
/// otherwise.
///
/// A session is closed, freeing its `max_concurrent_sessions` permit, once it
/// has had no inbound client activity and no open response stream (POST or
/// probed GET) for [`IdleTimeout::DEFAULT`] (5 minutes), swept every fifth of
/// that. This reaper is the only expiry owner: rmcp's own `keep_alive` timer is
/// switched off, so neither outbound `resources/updated` notifications (#521)
/// nor SSE pings affect expiry. The idle clock starts at the later of the last
/// inbound request and the moment the last stream closed. A client that closes
/// its connections cleanly is noticed on the next write, at most one SSE
/// keep-alive (15 s) later. A silently vanished
/// peer (half-open TCP: sleeping laptop, dropped NAT mapping) is detected by
/// the liveness probe above: its stream closes within one probe interval plus
/// deadline and the session then expires after the idle timeout. The probe
/// works on every platform and behind a reverse proxy. As a kernel-level
/// complement, on Linux and Android every accepted socket also gets
/// `TCP_USER_TIMEOUT` of `HALF_OPEN_TIMEOUT` (60 s), so unacknowledged data
/// (the 15 s SSE pings guarantee some) drops the connection about 75 s after
/// the peer vanishes (#552). macOS and Windows keep the kernel default, and
/// behind the recommended reverse proxy the accepted socket faces the proxy,
/// so the proxy's own timeouts govern that hop. A client that stops reading
/// with a full receive window for longer than the timeout may be dropped too.
/// A client that answers probes and keeps a GET stream open is never reaped
/// (#573). With [`StreamLiveness::Disabled`] there is no proof of life, so an
/// open GET stream does not hold its session: the session expires
/// [`IdleTimeout::DEFAULT`] after the last inbound request even while the stream
/// receives notifications, and such clients must send a request (any `POST`,
/// for example a `ping`) more often than that. A POST or request-wise resume
/// response stream is cut [`ResponseStreamDeadline::DEFAULT`] (1 hour) after it
/// opens, in both liveness modes, whatever it is still sending (a stream
/// lifetime bound sized for the longest legitimate tool call): the stream then
/// stops holding the session open and the reaper expires the session after the
/// idle timeout, so a vanished peer no longer pins its slot for good. While
/// hyper's write of a response to a peer that stopped reading is stuck, the
/// body is not polled and the cut cannot fire: `write_stall_timeout` closes
/// the connection and frees its `max_concurrent_connections` permit, and the
/// session slot is freed after the idle timeout. Clients should send `DELETE` on
/// shutdown; after an expiry they must re-initialize and re-subscribe.
///
/// On rmcp's stateless request path, "one instance per session" narrows to
/// "one instance per request"; `resources/subscribe`/`unsubscribe` detect
/// that path and return an explicit error rather than silently accepting a
/// subscription that would never be observed -- see
/// [`SubscriptionRegistry`](crate::mcp::SubscriptionRegistry)'s docs.
///
/// Per-session isolation assumes the `Mcp-Session-Id` stays secret: whoever
/// holds it can open that session's GET stream once its previous stream is
/// gone. mcpls performs no authentication of its own, so keep the id out of
/// logs and place a non-loopback bind behind an authenticating reverse proxy.
///
/// # Resource limits
///
/// POST bodies exceeding `cfg.max_request_body` are rejected with
/// `413 Payload Too Large` (enforced by `rmcp`). Once `cfg.max_concurrent_sessions`
/// sessions are active, a request that would start a new one is rejected with
/// `429 Too Many Requests` — enforced as a hard bound at session creation by
/// [`CappedSessionManager`] and surfaced over HTTP by [`enforce_session_cap`].
///
/// # Shutdown
///
/// On `SIGTERM`/`SIGINT`, in-flight connections get up to
/// [`HTTP_GRACEFUL_SHUTDOWN_TIMEOUT`] to finish before this function returns
/// regardless — bounding shutdown this way lets the caller run its own
/// post-shutdown cleanup (e.g. closing registered LSP servers) even if a
/// connection never observes the cancellation (a stuck SSE stream, say).
/// `shutdown_signal` is constructed by [`crate::serve_with`] before any
/// startup work runs (see [`ShutdownSignal`]'s docs), so its registration
/// predates this function's own `TcpListener::bind` call — a signal between
/// bind and the graceful-shutdown future's first poll is still caught.
/// `shutdown_signal` is moved into (and dropped by) the
/// shutdown-signal future below once it resolves — i.e. as soon as
/// the *first* signal is received, well before this function returns. A
/// second, freshly constructed `ShutdownSignal` then covers the
/// connection-drain wait that follows (bounded by
/// [`HTTP_GRACEFUL_SHUTDOWN_TIMEOUT`]): a repeat signal caught there cuts the
/// drain short (dropping `serve` the same way the timeout branch already
/// does) instead of making the operator wait out the full timeout.
///
/// Cutting the drain short is *not* an immediate process exit: this function
/// still returns `Ok(())` normally, and its caller ([`crate::serve_with`])
/// proceeds straight into the ordinary post-transport shutdown sequence
/// ([`crate::shutdown`] — LSP server shutdown plus any pending background
/// init task, bounded by its own ~15s worst case). [`crate::shutdown`]'s own
/// registration (#329) takes over once *this* function returns, covering
/// that cleanup window and escalating to a forced `std::process::exit(1)` on
/// any *further* repeat signal — so an operator wanting a true immediate exit
/// needs a third signal, not a second. This is a deliberate choice, not an
/// oversight: calling `exit(1)` directly from this branch would skip
/// unwinding and cut short the graceful LSP `exit` delivery to still-running
/// servers, which is worse than requiring one more signal.
#[cfg(feature = "transport-http")]
pub(crate) async fn run_http(
    mcp_server: crate::mcp::McplsServer,
    cfg: HttpConfig,
    shutdown_signal: ShutdownSignal,
) -> Result<(), crate::Error> {
    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .map_err(|source| crate::Error::HttpBind {
            addr: cfg.bind,
            source,
        })?;
    serve_http(listener, mcp_server, cfg, shutdown_signal).await
}

/// The `Host` values the server accepts: the loopback names, the bound IP
/// literal when it is neither unspecified nor loopback, and `configured`.
///
/// The bound IP literal is safe to allow because DNS rebinding needs a
/// hostname, and it is allowed on any port because a client reaching a `:80`
/// or `:443` bind omits the port from `Host`.
#[cfg(feature = "transport-http")]
fn effective_allowed_hosts(
    local_addr: std::net::SocketAddr,
    configured: &[AllowedHost],
) -> Vec<AllowedHost> {
    let ip = local_addr.ip();
    let bound_ip = (!ip.is_unspecified() && !ip.is_loopback()).then(|| AllowedHost::ip_literal(ip));
    AllowedHost::loopback()
        .into_iter()
        .chain(bound_ip)
        .chain(configured.iter().cloned())
        .collect()
}

/// The `rmcp` service configuration for `cfg` served on `local_addr`.
///
/// `rmcp` accepts a request without `Origin`, so non-browser clients are
/// unaffected; a browser page on any origin outside the allowlist gets `403`.
#[cfg(feature = "transport-http")]
fn rmcp_service_config(
    local_addr: std::net::SocketAddr,
    cfg: &HttpConfig,
    cancel: tokio_util::sync::CancellationToken,
) -> rmcp::transport::streamable_http_server::StreamableHttpServerConfig {
    use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;

    let allowed_origins = AllowedOrigin::loopback(local_addr.port())
        .into_iter()
        .chain(cfg.allowed_origins.iter().cloned())
        .map(|origin| origin.to_string());
    let allowed_hosts = effective_allowed_hosts(local_addr, &cfg.allowed_hosts)
        .into_iter()
        .map(|host| host.to_string());
    // `StreamableHttpServerConfig` is `#[non_exhaustive]`: construct via Default, then mutate.
    let mut http_cfg = StreamableHttpServerConfig::default()
        .with_allowed_hosts(allowed_hosts)
        .with_allowed_origins(allowed_origins)
        .enforce_origin_validation();
    http_cfg.cancellation_token = cancel;
    http_cfg.max_request_body_bytes = cfg.max_request_body.get();
    http_cfg
}

/// Serves the MCP HTTP transport on an already-bound `listener`.
///
/// Split out of [`run_http`] so callers (tests in particular) can bind the
/// listener themselves and know the exact address before serving starts.
/// `cfg.bind` is ignored; the listener's local address is authoritative.
/// Shutdown behavior is documented on [`run_http`].
#[cfg(feature = "transport-http")]
// `session_manager` and `service` are moved into `app`, which is served until
// shutdown — clippy's drop-tightening heuristic misreads that as an
// early-droppable temporary because both types embed `tokio::sync` lock types
// (`CappedSessionManager`'s `Mutex`, `StreamableHttpService`'s `RwLock`s).
#[allow(clippy::significant_drop_tightening)]
pub(crate) async fn serve_http(
    listener: tokio::net::TcpListener,
    mcp_server: crate::mcp::McplsServer,
    cfg: HttpConfig,
    mut shutdown_signal: ShutdownSignal,
) -> Result<(), crate::Error> {
    use std::sync::Arc;

    use rmcp::transport::streamable_http_server::StreamableHttpService;
    use tokio_util::sync::CancellationToken;

    let session_manager = Arc::new(
        CappedSessionManager::new(cfg.max_concurrent_sessions, cfg.session_idle_timeout)
            .with_stream_liveness(cfg.stream_liveness)
            .with_response_stream_deadline(cfg.response_stream_deadline),
    );
    let reaper_manager = Arc::clone(&session_manager);
    let cancel = CancellationToken::new();

    // `mcp_server` is moved (not cloned): `McplsServer` is deliberately not
    // `Clone` (#478) so this is the only value the factory below can build
    // new sessions from, rather than a shared instance a caller could
    // accidentally hand to multiple sessions.
    let mcp_for_factory = mcp_server;
    let local_addr = listener.local_addr()?;
    let http_cfg = rmcp_service_config(local_addr, &cfg, cancel.clone());

    // `for_new_session`, not `.clone()`: every session must get its own
    // subscription state (#478) rather than sharing `mcp_for_factory`'s, while
    // still sharing its `Arc<Translator>` and the rest of the LSP-facing state
    // via a cheap `Arc` bump. On rmcp's stateless path this factory runs once
    // per request, not per session; those instances never subscribe, so they
    // never register with the `SubscriptionRegistry`.
    let service = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(mcp_for_factory.for_new_session()),
        session_manager,
        http_cfg,
    );

    let app = axum::Router::new()
        .nest_service(cfg.path.as_str(), service.clone())
        .route_service("/", service)
        .layer(axum::middleware::from_fn_with_state(
            cfg.effective_listen_lease(),
            lease::attach_listen_lease,
        ))
        .layer(axum::middleware::from_fn(enforce_session_cap))
        .layer(axum::middleware::from_fn_with_state(
            cfg.header_read_timeout,
            enforce_body_inactivity,
        ));

    let reaper_cancel = cancel.child_token();
    // Stops the reaper on every return path below, not only on shutdown.
    let _reaper_guard = reaper_cancel.clone().drop_guard();
    tokio::spawn(run_idle_reaper(reaper_manager, reaper_cancel));

    tracing::info!(addr = %local_addr, path = %cfg.path, "MCP HTTP transport listening");
    if !local_addr.ip().is_loopback() {
        tracing::warn!(
            addr = %local_addr,
            "binding to a non-loopback address: mcpls performs no authentication of its own on \
             any transport — place this endpoint behind a reverse proxy that enforces \
             authentication. mcpls itself enforces only a header-read/idle timeout, a write-stall \
             timeout and a connection cap. The Host header must be localhost, 127.0.0.1, ::1, \
             the bound IP or one of the configured allowed hosts (a proxy can rewrite it), and \
             browser clients may use only loopback origins on the bound port or the \
             configured allowed origins"
        );
    }

    // `cancel` is cancelled exactly once, when the shutdown signal fires
    // (below). Cloned first so the force-timeout and repeat-signal branches
    // can each observe that same moment independently of the
    // shutdown-signal future, which consumes its own clone.
    let cancel_for_force_timeout = cancel.clone();
    let cancel_for_repeat_signal = cancel.clone();
    let cancel_for_serve = cancel.clone();
    let serve = async move {
        let signal = async move {
            shutdown_signal.recv().await;
            cancel.cancel();
        };
        tokio::join!(
            serve_http1(
                listener,
                app,
                cancel_for_serve,
                cfg.header_read_timeout,
                cfg.write_stall_timeout,
                cfg.max_concurrent_connections,
            ),
            signal
        );
    };

    // The force-timeout only starts counting once `cancel` is actually
    // cancelled — i.e. once a shutdown signal has been received — not from
    // server startup. Without that ordering, `tokio::time::timeout` wrapping
    // `serve` directly would tear down the listener after
    // `HTTP_GRACEFUL_SHUTDOWN_TIMEOUT` of ordinary uptime, signal or not.
    // This bounds only the "drain in-flight connections after shutdown was
    // requested" phase, so a connection that never observes `cancel` (e.g. a
    // stuck SSE stream) can't hang the caller's post-shutdown cleanup
    // (draining/closing LSP servers) indefinitely.
    tokio::select! {
        () = serve => Ok(()),
        () = async move {
            cancel_for_force_timeout.cancelled().await;
            tokio::time::sleep(HTTP_GRACEFUL_SHUTDOWN_TIMEOUT).await;
        } => {
            tracing::warn!(
                timeout = ?HTTP_GRACEFUL_SHUTDOWN_TIMEOUT,
                "HTTP graceful shutdown did not complete in time, proceeding with shutdown anyway"
            );
            Ok(())
        }
        // #349: `shutdown_signal` above is consumed and dropped as soon as
        // the first signal arrives, leaving no listener for a repeat signal
        // during the connection-drain wait that follows. Wait for `cancel`
        // first and only then construct a fresh `ShutdownSignal` -- rather
        // than registering one up front, alongside `shutdown_signal` -- so
        // it starts with no pending signal of its own: `tokio::signal`
        // fans a delivered signal out to every live listener, so a listener
        // already registered before the first signal arrived would
        // independently observe that same signal and misreport it as a
        // repeat. This leaves a much smaller, accepted gap instead: between
        // `cancel.cancel()` firing and `ShutdownSignal::new()` actually
        // registering on the next scheduler hop, there is no pollable
        // listener at all, so a signal delivered in that sub-millisecond
        // window is lost. Registering up front would only trade this for
        // the coalescing problem above -- it is not fully closable either
        // way, and the window is far below human reaction time to a second
        // keypress.
        () = async move {
            cancel_for_repeat_signal.cancelled().await;
            let mut repeat_signal = ShutdownSignal::new();
            repeat_signal.recv().await;
        } => {
            tracing::warn!(
                "repeat shutdown signal received during HTTP connection drain, cutting drain short"
            );
            Ok(())
        }
    }
}

/// Accepts connections on `listener` and serves `app` over HTTP/1 until
/// `cancel` fires, then drains the open connections gracefully.
///
/// Replaces `axum::serve` because it never installs a hyper timer, which
/// leaves `header_read_timeout` inert: a client that opens a connection and
/// stalls would hold it (and, with the cap, a permit) forever. A permit is
/// taken before `accept` so a full house leaves new connections queued in
/// the kernel rather than accepted-and-idle.
///
/// Every stream goes through [`ConnectionIo`]: a write the peer drains slower
/// than the minimum rate over a `write_stall` window closes the connection,
/// and a clean close lingers to discard the unread request body.
#[cfg(feature = "transport-http")]
async fn serve_http1(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    cancel: tokio_util::sync::CancellationToken,
    header_read_timeout: HeaderReadTimeout,
    write_stall: WriteStallTimeout,
    max_connections: ConnectionLimit,
) {
    use std::sync::Arc;

    use hyper_util::rt::{TokioIo, TokioTimer};
    use hyper_util::service::TowerToHyperService;
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout.get());
    let permits = Arc::new(Semaphore::new(max_connections.get()));
    // A `JoinSet` aborts its tasks when dropped, so a force-timeout in
    // `run_http` cannot leave connections being served after it returns.
    let mut connections = JoinSet::new();

    loop {
        while connections.try_join_next().is_some() {}
        let permit = tokio::select! {
            () = cancel.cancelled() => break,
            permit = Arc::clone(&permits).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };
        let stream = tokio::select! {
            () = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(e) if is_connection_error(&e) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "HTTP accept failed, backing off");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            },
        };

        #[cfg(any(target_os = "linux", target_os = "android"))]
        set_half_open_timeout(&stream);

        let io = TokioIo::new(ConnectionIo::new(
            stream,
            write_stall,
            header_read_timeout,
            cancel.clone(),
        ));
        let conn = builder.serve_connection(io, TowerToHyperService::new(app.clone()));
        let cancel = cancel.clone();
        connections.spawn(async move {
            let _permit = permit;
            let mut conn = std::pin::pin!(conn);
            tokio::select! {
                result = conn.as_mut() => {
                    if let Err(e) = result {
                        tracing::trace!(error = %e, "HTTP connection ended with error");
                    }
                }
                () = cancel.cancelled() => {
                    conn.as_mut().graceful_shutdown();
                    if let Err(e) = conn.as_mut().await {
                        tracing::trace!(error = %e, "HTTP connection ended with error");
                    }
                }
            }
        });
    }

    while connections.join_next().await.is_some() {}
}

/// Shortest total time a request body may take, whatever the configured
/// `header_read_timeout` is.
#[cfg(feature = "transport-http")]
const MIN_BODY_DEADLINE: std::time::Duration = std::time::Duration::from_mins(2);

/// Total time a request body may take: four idle windows, at least
/// [`MIN_BODY_DEADLINE`].
#[cfg(feature = "transport-http")]
fn body_deadline(header_read_timeout: HeaderReadTimeout) -> std::time::Duration {
    MIN_BODY_DEADLINE.max(header_read_timeout.get().saturating_mul(4))
}

/// Request body that fails once no frame has arrived for `timeout`, or the
/// whole body has taken longer than its total deadline, flagging `expired` so
/// [`enforce_body_inactivity`] can answer `408`.
#[cfg(feature = "transport-http")]
struct InactivityBody {
    inner: axum::body::Body,
    timeout: std::time::Duration,
    sleep: std::pin::Pin<Box<tokio::time::Sleep>>,
    total: std::pin::Pin<Box<tokio::time::Sleep>>,
    expired: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(feature = "transport-http")]
#[derive(Debug)]
struct BodyInactivity;

#[cfg(feature = "transport-http")]
impl std::fmt::Display for BodyInactivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body stalled")
    }
}

#[cfg(feature = "transport-http")]
impl std::error::Error for BodyInactivity {}

#[cfg(feature = "transport-http")]
impl http_body::Body for InactivityBody {
    type Data = axum::body::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        use std::future::Future as _;
        use std::task::Poll;

        let this = self.get_mut();
        if this.total.as_mut().poll(cx).is_ready() {
            this.expired
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return Poll::Ready(Some(Err(axum::Error::new(BodyInactivity))));
        }
        match std::pin::Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(frame) => {
                this.sleep = Box::pin(tokio::time::sleep(this.timeout));
                Poll::Ready(frame)
            }
            Poll::Pending => {
                if this.sleep.as_mut().poll(cx).is_ready() {
                    this.expired
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    Poll::Ready(Some(Err(axum::Error::new(BodyInactivity))))
                } else {
                    Poll::Pending
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Bounds the pause between request-body chunks by `timeout`, and the whole
/// body by [`body_deadline`], answering `408 Request Timeout` when a client
/// stalls or trickles mid-body.
///
/// `header_read_timeout` only covers the request head, so without this a POST
/// announcing a body it never sends, or sending one byte per window, would
/// pin its connection and permit forever.
#[cfg(feature = "transport-http")]
async fn enforce_body_inactivity(
    axum::extract::State(timeout): axum::extract::State<HeaderReadTimeout>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let expired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (parts, body) = request.into_parts();
    let body = if http_body::Body::is_end_stream(&body) {
        body
    } else {
        axum::body::Body::new(InactivityBody {
            inner: body,
            timeout: timeout.get(),
            sleep: Box::pin(tokio::time::sleep(timeout.get())),
            total: Box::pin(tokio::time::sleep_until(connection_io::deadline_after(
                tokio::time::Instant::now(),
                body_deadline(timeout),
            ))),
            expired: std::sync::Arc::clone(&expired),
        })
    };

    let response = next
        .run(axum::extract::Request::from_parts(parts, body))
        .await;
    if expired.load(std::sync::atomic::Ordering::Relaxed) {
        return axum::http::StatusCode::REQUEST_TIMEOUT.into_response();
    }
    response
}

/// Whether an `accept` error is about the one peer's connection rather than
/// the listener, so the loop can retry at once instead of backing off.
#[cfg(feature = "transport-http")]
fn is_connection_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}

/// `TCP_USER_TIMEOUT` applied to accepted HTTP sockets on Linux and Android.
///
/// Must exceed the 15 s SSE keep-alive interval so a healthy but quiet stream
/// is never dropped.
#[cfg(all(
    feature = "transport-http",
    any(target_os = "linux", target_os = "android")
))]
const HALF_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(1);

/// Bounds how long unacknowledged data may linger on `stream` before the
/// kernel drops the connection; failures are logged and the connection kept.
#[cfg(all(
    feature = "transport-http",
    any(target_os = "linux", target_os = "android")
))]
fn set_half_open_timeout(stream: &tokio::net::TcpStream) {
    if let Err(e) = socket2::SockRef::from(stream).set_tcp_user_timeout(Some(HALF_OPEN_TIMEOUT)) {
        tracing::debug!(error = %e, "failed to set TCP_USER_TIMEOUT on accepted connection");
    }
}

/// Upper bound [`run_http`] waits, once shutdown has been signaled, for
/// in-flight connections to finish draining before giving up and returning
/// anyway.
#[cfg(feature = "transport-http")]
const HTTP_GRACEFUL_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Wraps [`LocalSessionManager`], bounding concurrent HTTP sessions to a
/// fixed capacity.
///
/// A [`tokio::sync::Semaphore`] permit is acquired atomically inside
/// [`create_session`](SessionManager::create_session) — before delegating to
/// the inner manager — and held in a [`SessionSlot`], next to the session's
/// inbound-activity record, for the session's lifetime. The slot is removed,
/// releasing the permit, by [`close_session`](SessionManager::close_session)
/// or by the idle reaper ([`run_idle_reaper`]). Every response stream the
/// session hands out (POST, GET, resume) carries a [`StreamGuard`], so a
/// session with an open stream is never reaped. This makes the cap a
/// true hard bound: the check and the reservation happen as one step, so no
/// number of concurrent requests can observe spare capacity and all proceed
/// past it (a "check-then-create" race that a separate read of the session
/// count could not avoid).
///
/// Enforcement lives here, at the `SessionManager` layer, rather than in Axum
/// middleware sniffing request headers, because that is the only place
/// guaranteed to run exactly when — and only when — a session is actually
/// created. `rmcp` 3.2.0's `StreamableHttpService::handle_post` classifies
/// every `initialize` request as legacy and always calls `create_session`,
/// whatever protocol version it names — the handshake only exists in
/// revisions before `2026-07-28`, so a version named in its params never
/// routes it to the stateless path. Only *non*-`initialize` requests that
/// carry SEP-2575 per-request `_meta` (`io.modelcontextprotocol/protocolVersion`
/// = `2026-07-28` plus the required `clientCapabilities` key), and
/// `server/discover` requests, take the stateless discover-lifecycle path
/// that never calls `create_session`. A header-based middleware heuristic
/// can't tell these apart without duplicating `rmcp`'s internal protocol
/// classification, so it either 429s traffic that never consumed a session
/// slot, or — in an all-stateless deployment — never fires at all.
///
/// `restore_session` and `event_store` deliberately use
/// [`SessionManager`]'s trait defaults (`NotSupported` / `None`) instead of
/// delegating to `inner`: `HttpConfig` exposes no session-store knob, so
/// these are unreachable today, but delegating them would let a restored
/// session skip the semaphore entirely — a cap bypass. Leave them as
/// defaults; overriding them to delegate is not a bug fix.
#[cfg(feature = "transport-http")]
struct CappedSessionManager {
    inner: std::sync::Arc<LocalSessionManager>,
    semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    slots: StdMutex<std::collections::HashMap<SessionId, SessionSlot>>,
    idle: IdleTimeout,
    stream_liveness: StreamLiveness,
    response_stream_deadline: ResponseStreamDeadline,
}

#[cfg(feature = "transport-http")]
impl CappedSessionManager {
    fn new(max_sessions: SessionLimit, idle: IdleTimeout) -> Self {
        let mut inner = LocalSessionManager::default();
        inner.session_config.keep_alive = None;
        Self {
            inner: std::sync::Arc::new(inner),
            semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(max_sessions.get())),
            slots: StdMutex::new(std::collections::HashMap::new()),
            idle,
            stream_liveness: StreamLiveness::Disabled,
            response_stream_deadline: ResponseStreamDeadline::DEFAULT,
        }
    }

    const fn with_stream_liveness(mut self, stream_liveness: StreamLiveness) -> Self {
        self.stream_liveness = stream_liveness;
        self
    }

    const fn with_response_stream_deadline(mut self, deadline: ResponseStreamDeadline) -> Self {
        self.response_stream_deadline = deadline;
        self
    }

    fn session_liveness(&self, id: &SessionId) -> Option<std::sync::Arc<SessionLiveness>> {
        lock_std(&self.slots)
            .get(id)
            .map(|slot| std::sync::Arc::clone(&slot.liveness))
    }

    /// Wraps a standalone-channel `stream` in a probing forwarding task when
    /// liveness probing is on; otherwise only guards it.
    ///
    /// Fails closed when probing is on but the session's slot is gone (the
    /// reaper removed it ahead of closing the inner session), rather than
    /// handing out an unprobed stream.
    fn standalone<S>(
        &self,
        id: &SessionId,
        guard: Option<StreamGuard>,
        stream: S,
    ) -> Result<
        impl futures::Stream<Item = ServerSseMessage> + Send + Sync + 'static + use<S>,
        CappedSessionManagerError,
    >
    where
        S: futures::Stream<Item = ServerSseMessage> + Send + Sync + 'static,
    {
        use futures::StreamExt as _;

        Ok(match self.stream_liveness {
            StreamLiveness::Probe { interval, deadline } => {
                let liveness = self
                    .session_liveness(id)
                    .ok_or_else(|| LocalSessionManagerError::SessionNotFound(id.clone()))?;
                StreamProbe {
                    liveness,
                    interval,
                    deadline,
                    manager: std::sync::Arc::clone(&self.inner),
                    session: id.clone(),
                }
                .forward(stream, guard)
                .left_stream()
            }
            StreamLiveness::Disabled => {
                drop(guard);
                stream.right_stream()
            }
        })
    }

    fn activity(&self, id: &SessionId) -> Option<std::sync::Arc<SessionActivity>> {
        lock_std(&self.slots)
            .get(id)
            .map(|slot| std::sync::Arc::clone(&slot.activity))
    }

    fn touch(&self, id: &SessionId) {
        if let Some(activity) = self.activity(id) {
            activity.touch();
        }
    }

    /// Counts a response stream about to open on `id` as activity; `None` for
    /// an unknown session.
    fn open_guard(&self, id: &SessionId) -> Option<StreamGuard> {
        self.activity(id).map(|activity| activity.open_stream())
    }

    /// Wraps `stream` so the session counts as active until it is dropped.
    ///
    /// The guard is taken before the inner call so a sweep cannot slip in
    /// between the call and the stream existing.
    fn guarded<S: futures::Stream>(
        guard: Option<StreamGuard>,
        stream: S,
    ) -> impl futures::Stream<Item = S::Item> {
        use futures::StreamExt as _;

        stream.map(move |message| {
            let _keep_open = &guard;
            message
        })
    }

    /// [`Self::guarded`], cut once the response stream deadline, counted from
    /// this call, passes.
    fn bounded<S: futures::Stream>(
        guard: Option<StreamGuard>,
        stream: S,
        deadline: ResponseStreamDeadline,
        session: SessionId,
    ) -> impl futures::Stream<Item = S::Item> + use<S> {
        use futures::StreamExt as _;

        let timer = tokio::time::sleep(deadline.get());
        Self::guarded(guard, stream).take_until(async move {
            timer.await;
            tracing::debug!(
                session = %SessionFingerprint(&session),
                "closing response stream at its deadline"
            );
        })
    }

    /// Removes every session idle at `now` (freeing its permit at once) and
    /// closes it in the inner manager on a detached task, so one wedged
    /// session worker cannot stall the sweep. Returns how many were reaped.
    ///
    /// Benign race: a request touching or opening a stream on a session between
    /// its idle check and its removal here is not seen, so that session is
    /// reaped anyway. It had been idle for the whole timeout, and the client
    /// gets a 404 on its next call and re-initializes.
    fn reap_idle(&self, now: tokio::time::Instant) -> usize {
        let idle_ids: Vec<SessionId> = {
            let mut slots = lock_std(&self.slots);
            let ids: Vec<_> = slots
                .iter()
                .filter(|(_, slot)| slot.activity.is_idle(now, self.idle))
                .map(|(id, _)| id.clone())
                .collect();
            for id in &ids {
                slots.remove(id);
            }
            ids
        };
        for id in &idle_ids {
            tracing::debug!(session = %SessionFingerprint(id), "closing idle HTTP session");
            let inner = std::sync::Arc::clone(&self.inner);
            let id = id.clone();
            tokio::spawn(async move {
                close_session_bounded(&id, inner.close_session(&id)).await;
            });
        }
        idle_ids.len()
    }
}

/// Awaits `close` for at most [`liveness::SESSION_CLOSE_TIMEOUT`], so a wedged
/// session worker cannot park the detached closing task forever.
#[cfg(feature = "transport-http")]
async fn close_session_bounded<E: std::fmt::Display>(
    id: &SessionId,
    close: impl std::future::Future<Output = Result<(), E>>,
) {
    match tokio::time::timeout(liveness::SESSION_CLOSE_TIMEOUT, close).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!(
            session = %SessionFingerprint(id),
            "closing idle HTTP session failed: {e}"
        ),
        Err(_elapsed) => tracing::debug!(
            session = %SessionFingerprint(id),
            "closing idle HTTP session timed out"
        ),
    }
}

#[cfg(feature = "transport-http")]
non_zero_duration! {
    /// Non-zero duration after which a session without inbound client activity
    /// or open response stream is closed by [`run_idle_reaper`].
    ///
    /// The sole session expiry owner: rmcp's own `keep_alive` is disabled
    /// because it measures any event on the session -- including outbound
    /// notifications and, for an answering GET listener, nothing at all -- so it
    /// both never fired for an abandoned but subscribed session (#521) and cut
    /// off a healthy one (#573). An open response stream holds the session only
    /// while it is proven alive (a POST stream, or a probed GET stream).
    pub(crate) IdleTimeout, 300, "5 minutes."
}

#[cfg(feature = "transport-http")]
impl IdleTimeout {
    /// A fifth of the timeout, so an idle session closes within 1.2x of it;
    /// never zero, which `tokio::time::interval` rejects.
    fn sweep_interval(self) -> std::time::Duration {
        self.get()
            .checked_div(5)
            .unwrap_or_default()
            .max(std::time::Duration::from_millis(1))
    }
}

#[cfg(feature = "transport-http")]
#[derive(Debug)]
struct ActivityState {
    open_streams: usize,
    idle_since: tokio::time::Instant,
}

/// Inbound-activity record of one session: how many response streams are
/// open and since when none has been.
#[cfg(feature = "transport-http")]
#[derive(Debug)]
struct SessionActivity(StdMutex<ActivityState>);

#[cfg(feature = "transport-http")]
impl SessionActivity {
    fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self(StdMutex::new(ActivityState {
            open_streams: 0,
            idle_since: tokio::time::Instant::now(),
        })))
    }

    fn touch(&self) {
        lock_std(&self.0).idle_since = tokio::time::Instant::now();
    }

    fn open_stream(self: &std::sync::Arc<Self>) -> StreamGuard {
        let mut state = lock_std(&self.0);
        state.open_streams = state.open_streams.saturating_add(1);
        drop(state);
        StreamGuard(std::sync::Arc::clone(self))
    }

    fn is_idle(&self, now: tokio::time::Instant, timeout: IdleTimeout) -> bool {
        let state = lock_std(&self.0);
        state.open_streams == 0 && now.saturating_duration_since(state.idle_since) >= timeout.get()
    }
}

/// Keeps a session non-idle while one of its response streams is open;
/// dropping it restarts the idle clock.
#[cfg(feature = "transport-http")]
#[derive(Debug)]
struct StreamGuard(std::sync::Arc<SessionActivity>);

#[cfg(feature = "transport-http")]
impl Drop for StreamGuard {
    fn drop(&mut self) {
        let mut state = lock_std(&self.0.0);
        state.open_streams = state.open_streams.saturating_sub(1);
        state.idle_since = tokio::time::Instant::now();
    }
}

/// One live session's cap permit and activity record, kept together so the
/// two cannot diverge.
#[cfg(feature = "transport-http")]
#[derive(Debug)]
struct SessionSlot {
    _permit: tokio::sync::OwnedSemaphorePermit,
    activity: std::sync::Arc<SessionActivity>,
    liveness: std::sync::Arc<SessionLiveness>,
}

/// Periodically closes the sessions of `manager` that are idle, until
/// `cancel` fires.
#[cfg(feature = "transport-http")]
async fn run_idle_reaper(
    manager: std::sync::Arc<CappedSessionManager>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut ticker = tokio::time::interval(manager.idle.sweep_interval());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticker.tick() => {
                manager.reap_idle(tokio::time::Instant::now());
            }
        }
    }
}

/// Marker embedded in [`CappedSessionManagerError::CapReached`]'s rendered
/// message.
///
/// `rmcp`'s `StreamableHttpService` always maps `create_session` failures to
/// a generic `500 Internal Server Error` (`internal_error_response` in
/// `server_side_http.rs` is a fixed, non-configurable mapping — `rmcp` gives
/// callers no other hook). [`enforce_session_cap`] looks for this marker in
/// the response body to translate a capacity rejection into
/// `429 Too Many Requests` without misclassifying other `create_session`
/// failures as capacity issues.
#[cfg(feature = "transport-http")]
const SESSION_CAP_MARKER: &str = "mcpls-http-session-cap-reached";

/// Error type for [`CappedSessionManager`].
#[cfg(feature = "transport-http")]
#[derive(Debug, thiserror::Error)]
enum CappedSessionManagerError {
    /// The concurrent-session cap was already reached.
    #[error("{SESSION_CAP_MARKER}: maximum concurrent HTTP sessions already active")]
    CapReached,
    /// The session is gone. Carries no id: rmcp logs this error and the id is
    /// a bearer secret (#555).
    #[error("session not found")]
    SessionGone,
    /// The wrapped [`LocalSessionManager`] failed.
    #[error(transparent)]
    Inner(LocalSessionManagerError),
}

#[cfg(feature = "transport-http")]
impl From<LocalSessionManagerError> for CappedSessionManagerError {
    fn from(error: LocalSessionManagerError) -> Self {
        match error {
            LocalSessionManagerError::SessionNotFound(_) => Self::SessionGone,
            other => Self::Inner(other),
        }
    }
}

#[cfg(feature = "transport-http")]
impl SessionManager for CappedSessionManager {
    type Error = CappedSessionManagerError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let permit = self
            .semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| CappedSessionManagerError::CapReached)?;
        let (id, transport) = self.inner.create_session().await?;
        lock_std(&self.slots).insert(
            id.clone(),
            SessionSlot {
                _permit: permit,
                activity: SessionActivity::new(),
                liveness: std::sync::Arc::default(),
            },
        );
        Ok((id, transport))
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, Self::Error> {
        self.touch(id);
        Ok(self.inner.initialize_session(id, message).await?)
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        Ok(self.inner.has_session(id).await?)
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        // Release the permit unconditionally, before propagating any error from
        // `inner.close_session`: on error the inner manager has already dropped
        // the session from its own table (see `LocalSessionManager::close_session`),
        // so skipping the removal here would leak the permit permanently and
        // monotonically shrink capacity.
        lock_std(&self.slots).remove(id);
        self.inner.close_session(id).await?;
        Ok(())
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl futures::Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error>
    {
        let guard = self.open_guard(id);
        let stream = self.inner.create_stream(id, message).await?;
        Ok(Self::bounded(
            guard,
            stream,
            self.response_stream_deadline,
            id.clone(),
        ))
    }

    async fn accept_message(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<(), Self::Error> {
        self.touch(id);
        if let Some(probe) = ProbeId::answered_by(&message) {
            if let Some(liveness) = self.session_liveness(id) {
                liveness.acknowledge(probe);
            }
            return Ok(());
        }
        Ok(self.inner.accept_message(id, message).await?)
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl futures::Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error>
    {
        let guard = self.open_guard(id);
        let stream = self.inner.create_standalone_stream(id).await?;
        self.standalone(id, guard, stream)
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl futures::Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error>
    {
        use futures::StreamExt as _;

        let guard = self.open_guard(id);
        let common = is_common_channel_event_id(&last_event_id);
        let stream = self.inner.resume(id, last_event_id).await?;
        Ok(if common {
            self.standalone(id, guard, stream)?.left_stream()
        } else {
            Self::bounded(guard, stream, self.response_stream_deadline, id.clone()).right_stream()
        })
    }
}

/// Axum middleware that rewrites `rmcp`'s generic `500 Internal Server Error`
/// into `429 Too Many Requests` when the failure was
/// [`CappedSessionManagerError::CapReached`] (detected via
/// [`SESSION_CAP_MARKER`] in the response body), adding a `Retry-After`
/// header.
///
/// This runs as response post-processing rather than a request pre-check
/// because only the real [`SessionManager::create_session`] call — deep
/// inside `rmcp` — knows whether a given request actually attempts to create
/// a session; see [`CappedSessionManager`]'s docs for why that can't be
/// determined from the request alone.
#[cfg(feature = "transport-http")]
async fn enforce_session_cap(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let response = next.run(request).await;
    if response.status() != axum::http::StatusCode::INTERNAL_SERVER_ERROR {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    // `create_session` failures always render as a small `Full<Bytes>` body
    // (`internal_error_response` in rmcp's `server_side_http.rs`); the large
    // streaming SSE/JSON success bodies never carry a 500 status, so this
    // never touches them. 64 KiB is far beyond any realistic error message.
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        // Buffering the original error body failed (e.g. it exceeded the 64
        // KiB cap, which should never happen per the comment above, or the
        // body stream errored). Preserve the 500 status but substitute a
        // minimal fallback body rather than dropping the error entirely.
        return axum::response::Response::from_parts(
            parts,
            axum::body::Body::from("Internal Server Error"),
        );
    };

    if bytes
        .windows(SESSION_CAP_MARKER.len())
        .any(|window| window == SESSION_CAP_MARKER.as_bytes())
    {
        parts.status = axum::http::StatusCode::TOO_MANY_REQUESTS;
        parts.headers.insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from_static("1"),
        );
        return axum::response::Response::from_parts(
            parts,
            axum::body::Body::from("Too Many Requests: maximum concurrent HTTP sessions reached"),
        );
    }

    axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::assert_matches;

    use crate::bridge::WorkspaceRoots;

    #[cfg(feature = "transport-http")]
    #[test]
    fn test_session_fingerprint_hides_id() {
        let id: super::SessionId = "61429d44-e35a-4615-bd7f-1ccb38acecae".into();
        let shown = super::SessionFingerprint(&id).to_string();

        assert_eq!(shown.len(), 8);
        assert!(shown.chars().all(|c| c.is_ascii_hexdigit()), "{shown}");
        assert!(!id.contains(&shown));
        assert_eq!(shown, super::SessionFingerprint(&id).to_string());
    }

    /// An accepted stream must read back the half-open `TCP_USER_TIMEOUT`.
    #[cfg(all(
        feature = "transport-http",
        any(target_os = "linux", target_os = "android")
    ))]
    #[tokio::test]
    async fn test_set_half_open_timeout_applies_tcp_user_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (accepted, _) = listener.accept().await.unwrap();

        super::set_half_open_timeout(&accepted);

        let timeout = socket2::SockRef::from(&accepted)
            .tcp_user_timeout()
            .unwrap();
        assert_eq!(timeout, Some(super::HALF_OPEN_TIMEOUT));
    }

    /// `Transport::Stdio` is always constructible regardless of feature flags.
    #[test]
    fn test_transport_stdio_variant() {
        let t = super::Transport::Stdio;
        assert!(matches!(t, super::Transport::Stdio));
    }

    /// #329 regression: `crate::shutdown`'s cleanup-window fix hinges on a
    /// freshly constructed `ShutdownSignal` still receiving real OS signals
    /// after an *earlier* `ShutdownSignal` (e.g. the one `run_stdio`/
    /// `run_http` held) has already been dropped — proving there is no
    /// window in which the OS handler itself gets deregistered (per
    /// `ShutdownSignal`'s corrected struct doc: `tokio::signal` never
    /// uninstalls it, regardless of how many instances are constructed or
    /// dropped). Exercises this with a real self-sent `SIGTERM` via the
    /// external `kill` binary rather than mocking `ShutdownSignal`, since
    /// that's the exact mechanism `crate::shutdown`'s force-exit task relies
    /// on. Safe under `cargo nextest`'s one-process-per-test model, so no
    /// other test's signal disposition is affected.
    ///
    /// Deliberately does not go through `crate::shutdown` itself: a signal
    /// caught there unconditionally calls `std::process::exit(1)`, which
    /// would kill this test's own process for real rather than fail an
    /// assertion — see the #329 regression-test handoff for why a test
    /// triggering `std::process::exit(1)` isn't attempted here.
    ///
    /// Unix-only: `SIGTERM` and the external `kill` binary this test relies
    /// on don't exist on Windows, where `ShutdownSignal` listens for
    /// Ctrl-C instead (see the struct's `#[cfg(windows)]` arm above).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fresh_shutdown_signal_still_receives_sigterm_after_prior_instance_dropped() {
        let earlier = super::ShutdownSignal::new();
        drop(earlier);

        let mut cleanup_signal = super::ShutdownSignal::new();

        let pid = std::process::id();
        let signal_sender = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let status = std::process::Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status()
                .unwrap();
            assert!(status.success(), "`kill -TERM {pid}` must succeed");
        });

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), cleanup_signal.recv()).await;
        signal_sender.await.unwrap();

        assert!(
            result.is_ok(),
            "a freshly constructed ShutdownSignal must still receive a real SIGTERM sent after \
             an earlier instance was dropped — this is the exact mechanism crate::shutdown's \
             cleanup-window force-exit task depends on"
        );
    }

    /// #241: `run_stdio` must not hang when the transport never even
    /// establishes — it must surface the failure promptly.
    ///
    /// This is the closest portable coverage of `run_stdio`'s non-signal
    /// path achievable here: `run_stdio` is hardcoded to the process's real
    /// stdin/stdout (no injectable transport), and this crate is
    /// `forbid(unsafe_code)`, so a test can't redirect the fd to simulate "the
    /// MCP handshake completes, *then* stdin closes" — the specific
    /// scenario that would drive `service.waiting()` to resolve inside the
    /// `tokio::select!` and hit its `Ok(())` arm. What a test *can* rely on:
    /// under `cargo nextest`, each test's stdin is already closed before the
    /// test body runs, so `mcp_server.serve(...)` fails during the initial
    /// `initialize` handshake — before `run_stdio` ever reaches the
    /// `select!`. That still exercises real production code (the `.serve()`
    /// call and its error mapping) and proves `run_stdio` returns promptly
    /// rather than hanging, which is what a broken `select!` (e.g. one
    /// missing a branch, or awaiting the wrong future) would look like.
    #[tokio::test]
    async fn test_run_stdio_returns_promptly_when_stdin_is_already_closed() {
        use std::sync::Arc;

        use tokio::sync::Mutex;

        use crate::bridge::{NotificationCache, Translator};
        use crate::config::McpConfig;
        use crate::mcp::{McplsServer, SubscriptionRegistry};

        let translator = Arc::new(Translator::new());
        let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
        let workspace_roots = WorkspaceRoots::default();
        let subs = SubscriptionRegistry::new();
        let server = McplsServer::new(
            translator,
            notification_cache,
            workspace_roots,
            subs,
            crate::ProjectConfigStatus::NotIgnored,
            McpConfig::default(),
        );
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::run_stdio(server, super::ShutdownSignal::new()),
        )
        .await;

        assert!(
            outcome.is_ok(),
            "run_stdio must not hang when stdin is already closed"
        );
        let result = outcome.unwrap();
        assert_matches!(
            result,
            Err(crate::Error::McpServerStart(_)),
            "expected a McpServerStart error from the failed handshake, got: {result:?}"
        );
    }

    #[cfg(feature = "transport-http")]
    mod http_tests {
        use std::assert_matches;
        use std::net::SocketAddr;

        use rmcp::model::ClientJsonRpcMessage;

        use super::super::{
            CappedSessionManager, CappedSessionManagerError, ConnectionLimit, HeaderReadTimeout,
            HttpConfig, IdleTimeout, ProbeDeadline, ProbeInterval, SessionActivity,
            SessionManager as _, StreamLiveness, Transport, run_idle_reaper,
        };
        use crate::bridge::WorkspaceRoots;
        use crate::config::LanguageId;
        use crate::test_lsp::CapturedLogs;

        #[test]
        fn test_http_config_fields() {
            let addr: SocketAddr = "127.0.0.1:3000".parse().unwrap();
            let cfg = HttpConfig::new(addr);
            assert_eq!(cfg.bind, addr);
            assert_eq!(cfg.path, crate::HttpPath::default());
        }

        #[test]
        fn test_http_path_accepts_valid_paths() {
            for ok in [
                "/mcp",
                "/a",
                "/api/v1/mcp",
                "/a-b_c.d~e",
                "/A1/2b",
                "/.well-known",
            ] {
                assert_eq!(ok.parse::<crate::HttpPath>().unwrap().as_str(), ok);
            }
        }

        #[test]
        fn test_http_path_rejects_each_invalid_shape() {
            use crate::InvalidHttpPath as E;

            let cases = [
                ("", E::MissingLeadingSlash),
                ("mcp", E::MissingLeadingSlash),
                ("/", E::Root),
                ("//", E::EmptySegment),
                ("/a//b", E::EmptySegment),
                ("/a/", E::EmptySegment),
                ("/.", E::DotSegment),
                ("/a/../b", E::DotSegment),
                ("/{id}", E::InvalidCharacter('{')),
                ("/a/*rest", E::InvalidCharacter('*')),
                ("/:id", E::InvalidCharacter(':')),
                ("/a?b", E::InvalidCharacter('?')),
                ("/a#b", E::InvalidCharacter('#')),
                ("/a%20b", E::InvalidCharacter('%')),
                ("/a b", E::InvalidCharacter(' ')),
                ("/\u{e9}", E::InvalidCharacter('\u{e9}')),
            ];
            for (input, expected) in cases {
                assert_eq!(input.parse::<crate::HttpPath>(), Err(expected), "{input:?}");
            }
        }

        proptest::proptest! {
            #[test]
            fn test_every_valid_http_path_nests_without_panic(
                raw in "(/[A-Za-z0-9._~-]{1,6}){1,4}|[ -~]{0,12}"
            ) {
                if let Ok(path) = raw.parse::<crate::HttpPath>() {
                    let _ = axum::Router::<()>::new()
                        .nest_service(path.as_str(), axum::routing::get(|| async {}));
                }
            }
        }

        #[test]
        fn test_http_config_clone() {
            let cfg = HttpConfig::new("127.0.0.1:3001".parse().unwrap())
                .with_path("/test".parse().unwrap());
            let cloned = cfg.clone();
            assert_eq!(cloned.bind, cfg.bind);
            assert_eq!(cloned.path, cfg.path);
        }

        #[test]
        fn test_transport_http_variant() {
            let cfg = HttpConfig::new("127.0.0.1:3002".parse().unwrap());
            let t = Transport::Http(cfg);
            assert!(matches!(t, Transport::Http(_)));
        }

        #[test]
        fn test_http_config_new_uses_default_limits() {
            let cfg = HttpConfig::new("127.0.0.1:3003".parse().unwrap());
            assert_eq!(cfg.max_request_body, crate::RequestBodyLimit::DEFAULT);
            assert_eq!(cfg.max_concurrent_sessions, crate::SessionLimit::DEFAULT);
        }

        #[test]
        fn test_http_config_with_max_request_body_overrides_default() {
            let cfg = HttpConfig::new("127.0.0.1:3004".parse().unwrap())
                .with_max_request_body(crate::RequestBodyLimit::new(1024).unwrap());
            assert_eq!(cfg.max_request_body.get(), 1024);
            assert_eq!(cfg.max_concurrent_sessions, crate::SessionLimit::DEFAULT);
        }

        #[test]
        fn test_http_config_with_max_concurrent_sessions_overrides_default() {
            let cfg = HttpConfig::new("127.0.0.1:3005".parse().unwrap())
                .with_max_concurrent_sessions(crate::SessionLimit::new(5).unwrap());
            assert_eq!(cfg.max_concurrent_sessions.get(), 5);
            assert_eq!(cfg.max_request_body, crate::RequestBodyLimit::DEFAULT);
        }

        /// Verifies `run_http` binds successfully and accepts TCP connections.
        #[tokio::test]
        async fn test_run_http_binds() {
            use std::sync::Arc;

            use tokio::sync::Mutex;

            use crate::bridge::{NotificationCache, Translator};
            use crate::config::McpConfig;
            use crate::mcp::{McplsServer, SubscriptionRegistry};

            let translator = Arc::new(Translator::new());
            let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
            let workspace_roots = WorkspaceRoots::default();
            let subs = SubscriptionRegistry::new();
            let server = McplsServer::new(
                translator,
                notification_cache,
                workspace_roots,
                subs,
                crate::ProjectConfigStatus::NotIgnored,
                McpConfig::default(),
            );

            // Bind port 0 so the OS assigns a free port.
            let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = probe.local_addr().unwrap();
            drop(probe);

            let cfg = HttpConfig::new(addr);

            let server_task = tokio::spawn(super::super::run_http(
                server,
                cfg,
                super::super::ShutdownSignal::new(),
            ));

            // A successful TCP connect proves the listener is up.
            assert!(
                connect_with_retry(addr).await.is_some(),
                "HTTP listener should accept TCP connections"
            );

            server_task.abort();
        }

        /// #241 C1 regression: `run_http` must not self-terminate after
        /// `HTTP_GRACEFUL_SHUTDOWN_TIMEOUT` of ordinary uptime when no
        /// shutdown signal has been sent — the graceful-shutdown timeout
        /// must only start counting once a signal actually arrives, not
        /// from server startup.
        ///
        /// Uses `#[tokio::test(start_paused = true)]` plus
        /// `tokio::time::advance` to fast-forward virtual time past the
        /// timeout instead of sleeping the real 30s. Under the bug this
        /// regresses against — `tokio::time::timeout(HTTP_GRACEFUL_SHUTDOWN_TIMEOUT,
        /// serve)` wrapping the whole `serve` future from construction —
        /// advancing virtual time past the timeout resolves that timer and
        /// finishes the task immediately, even with no signal sent. Under
        /// the fix, nothing inside `run_http` starts a timer until `cancel`
        /// is cancelled, so this advance must have no effect and the task
        /// must still be running.
        #[tokio::test(start_paused = true)]
        async fn test_run_http_does_not_self_terminate_without_signal() {
            let (_addr, server_task) = spawn_http_server(test_server(), |cfg| cfg).await;

            // Let the spawned task make initial progress (bind the
            // listener, enter its `select!`) without depending on any real
            // or virtual delay.
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }

            // Fast-forward well past `HTTP_GRACEFUL_SHUTDOWN_TIMEOUT` with
            // no shutdown signal ever sent.
            tokio::time::advance(
                super::super::HTTP_GRACEFUL_SHUTDOWN_TIMEOUT + std::time::Duration::from_secs(5),
            )
            .await;
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }

            assert!(
                !server_task.is_finished(),
                "run_http must still be serving after HTTP_GRACEFUL_SHUTDOWN_TIMEOUT of uptime \
                 with no shutdown signal sent"
            );

            server_task.abort();
        }

        /// Verifies `run_http` returns an error when the bind address is already in use.
        #[tokio::test]
        async fn test_run_http_bind_error() {
            use std::sync::Arc;

            use tokio::sync::Mutex;

            use crate::bridge::{NotificationCache, Translator};
            use crate::config::McpConfig;
            use crate::mcp::{McplsServer, SubscriptionRegistry};

            // Hold a listener to make the port unavailable.
            let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = occupied.local_addr().unwrap();

            let translator = Arc::new(Translator::new());
            let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
            let workspace_roots = WorkspaceRoots::default();
            let subs = SubscriptionRegistry::new();
            let server = McplsServer::new(
                translator,
                notification_cache,
                workspace_roots,
                subs,
                crate::ProjectConfigStatus::NotIgnored,
                McpConfig::default(),
            );

            let cfg = HttpConfig::new(addr);

            let result =
                super::super::run_http(server, cfg, super::super::ShutdownSignal::new()).await;
            assert!(
                matches!(
                    &result,
                    Err(crate::Error::HttpBind { addr: bound, source })
                        if *bound == addr && source.kind() == std::io::ErrorKind::AddrInUse
                ),
                "run_http should fail with HttpBind(AddrInUse) when the port is occupied, got {result:?}"
            );

            drop(occupied);
        }

        /// Spawns `run_http` on a fresh loopback port.
        async fn spawn_run_http(
            cfg_for: impl FnOnce(SocketAddr) -> HttpConfig,
        ) -> (
            SocketAddr,
            tokio::task::JoinHandle<Result<(), crate::Error>>,
        ) {
            let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = probe.local_addr().unwrap();
            drop(probe);
            let task = tokio::spawn(super::super::run_http(
                test_server(),
                cfg_for(addr),
                super::super::ShutdownSignal::new(),
            ));
            let mut listening = false;
            for _ in 0..100 {
                if tokio::net::TcpStream::connect(addr).await.is_ok() {
                    listening = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert!(listening, "run_http never started listening on {addr}");
            (addr, task)
        }

        /// #465: a client that sends an incomplete request header and stalls
        /// is disconnected once `header_read_timeout` elapses.
        #[tokio::test]
        async fn test_run_http_closes_connection_that_stalls_mid_header() {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            let timeout = std::time::Duration::from_millis(200);
            let (addr, server_task) = spawn_run_http(|addr| {
                HttpConfig::new(addr)
                    .with_header_read_timeout(HeaderReadTimeout::new(timeout).unwrap())
            })
            .await;

            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();

            let mut sink = Vec::new();
            tokio::time::timeout(
                timeout + std::time::Duration::from_secs(1),
                stream.read_to_end(&mut sink),
            )
            .await
            .unwrap_or_else(|_| {
                panic!("server must close a stalled connection within the header timeout")
            })
            .ok();

            server_task.abort();
        }

        /// #465: with `max_concurrent_connections = 1`, a second connection is
        /// not served until the first closes.
        #[tokio::test]
        async fn test_run_http_connection_cap_queues_second_connection() {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            let (addr, server_task) = spawn_run_http(|addr| {
                HttpConfig::new(addr)
                    .with_max_concurrent_connections(ConnectionLimit::new(1).unwrap())
            })
            .await;

            let first = tokio::net::TcpStream::connect(addr).await.unwrap();

            let mut second = tokio::net::TcpStream::connect(addr).await.unwrap();
            let request =
                format!("GET /nowhere HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
            second.write_all(request.as_bytes()).await.unwrap();
            let mut buf = [0u8; 64];
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(300), second.read(&mut buf))
                    .await
                    .is_err(),
                "second connection must not be served while the cap is held"
            );

            drop(first);
            let n = tokio::time::timeout(std::time::Duration::from_secs(5), second.read(&mut buf))
                .await
                .unwrap_or_else(|_| {
                    panic!("second connection must be served once the first closes")
                })
                .unwrap();
            assert!(n > 0);

            server_task.abort();
        }

        /// A body trickling one chunk just inside every idle window still
        /// fails once its total deadline passes, flagging the 408.
        #[tokio::test(start_paused = true)]
        async fn test_a_trickling_request_body_expires_at_the_total_deadline() {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::time::Duration;

            let idle = HeaderReadTimeout::new(Duration::from_secs(30)).unwrap();
            let chunks = futures::stream::unfold(0_u32, |count| async move {
                tokio::time::sleep(Duration::from_secs(29)).await;
                Some((
                    Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"x")),
                    count + 1,
                ))
            });
            let expired = Arc::new(AtomicBool::new(false));
            let mut body = Box::pin(super::super::InactivityBody {
                inner: axum::body::Body::from_stream(chunks),
                timeout: idle.get(),
                sleep: Box::pin(tokio::time::sleep(idle.get())),
                total: Box::pin(tokio::time::sleep(super::super::body_deadline(idle))),
                expired: Arc::clone(&expired),
            });
            let start = tokio::time::Instant::now();

            let outcome = loop {
                let frame =
                    std::future::poll_fn(|cx| http_body::Body::poll_frame(body.as_mut(), cx)).await;
                if let Some(Err(error)) = frame {
                    break error;
                }
            };

            assert!(outcome.to_string().contains("stalled"), "{outcome}");
            assert!(expired.load(Ordering::Relaxed));
            assert!(
                start.elapsed() >= Duration::from_mins(2),
                "{:?}",
                start.elapsed()
            );
            assert!(
                start.elapsed() < Duration::from_mins(3),
                "{:?}",
                start.elapsed()
            );
        }

        #[test]
        fn test_body_deadline_is_four_idle_windows_with_a_floor_and_never_overflows() {
            use std::time::Duration;

            let deadline =
                |idle: Duration| super::super::body_deadline(HeaderReadTimeout::new(idle).unwrap());
            assert_eq!(deadline(Duration::from_secs(5)), Duration::from_mins(2));
            assert_eq!(deadline(Duration::from_secs(60)), Duration::from_mins(4));
            assert_eq!(deadline(Duration::MAX), Duration::MAX);
        }

        /// Security M1: a POST announcing a body it never sends is answered
        /// `408` after the inactivity timeout and releases its connection
        /// permit, so a one-connection cap still serves the next client.
        #[tokio::test]
        async fn test_run_http_answers_408_to_stalled_request_body_and_frees_the_permit() {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            let timeout = std::time::Duration::from_millis(200);
            let (addr, server_task) = spawn_run_http(|addr| {
                HttpConfig::new(addr)
                    .with_header_read_timeout(HeaderReadTimeout::new(timeout).unwrap())
                    .with_max_concurrent_connections(ConnectionLimit::new(1).unwrap())
            })
            .await;

            let mut stalled = tokio::net::TcpStream::connect(addr).await.unwrap();
            let head = format!(
                "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n"
            );
            stalled.write_all(head.as_bytes()).await.unwrap();

            let mut response = Vec::new();
            tokio::time::timeout(
                timeout + std::time::Duration::from_secs(2),
                stalled.read_to_end(&mut response),
            )
            .await
            .unwrap_or_else(|_| panic!("a stalled body must not pin the connection"))
            .ok();
            assert!(
                String::from_utf8_lossy(&response).starts_with("HTTP/1.1 408"),
                "got {:?}",
                String::from_utf8_lossy(&response)
            );

            let mut next = tokio::net::TcpStream::connect(addr).await.unwrap();
            let request =
                format!("GET /nowhere HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
            next.write_all(request.as_bytes()).await.unwrap();
            let mut buf = [0u8; 16];
            let n = tokio::time::timeout(std::time::Duration::from_secs(5), next.read(&mut buf))
                .await
                .unwrap_or_else(|_| panic!("the permit must be free after the 408"))
                .unwrap();
            assert!(n > 0);

            server_task.abort();
        }

        #[test]
        fn test_connection_settings_reject_zero_and_clamp_to_the_semaphore_maximum() {
            assert_eq!(HeaderReadTimeout::new(std::time::Duration::ZERO), None);
            assert_eq!(ConnectionLimit::new(0), None);
            assert_eq!(
                ConnectionLimit::new(usize::MAX).unwrap().get(),
                tokio::sync::Semaphore::MAX_PERMITS
            );
            let semaphore =
                tokio::sync::Semaphore::new(ConnectionLimit::new(usize::MAX).unwrap().get());
            assert_eq!(
                semaphore.available_permits(),
                tokio::sync::Semaphore::MAX_PERMITS
            );
        }

        /// Starts `serve_http1` over an empty router (every path is a 404).
        async fn spawn_serve_http1(
            limit: ConnectionLimit,
        ) -> (
            SocketAddr,
            tokio_util::sync::CancellationToken,
            tokio::task::JoinHandle<()>,
        ) {
            spawn_serve_http1_with(
                axum::Router::new(),
                crate::WriteStallTimeout::DEFAULT,
                limit,
            )
            .await
        }

        /// Starts `serve_http1` over `app` with an explicit write-stall timeout.
        async fn spawn_serve_http1_with(
            app: axum::Router,
            write_stall: crate::WriteStallTimeout,
            limit: ConnectionLimit,
        ) -> (
            SocketAddr,
            tokio_util::sync::CancellationToken,
            tokio::task::JoinHandle<()>,
        ) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let cancel = tokio_util::sync::CancellationToken::new();
            let task = tokio::spawn(super::super::serve_http1(
                listener,
                app,
                cancel.clone(),
                HeaderReadTimeout::DEFAULT,
                write_stall,
                limit,
            ));
            (addr, cancel, task)
        }

        /// A response body that never ends.
        struct EndlessBody;

        impl http_body::Body for EndlessBody {
            type Data = axum::body::Bytes;
            type Error = std::convert::Infallible;

            fn poll_frame(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>>
            {
                std::task::Poll::Ready(Some(Ok(http_body::Frame::data(
                    axum::body::Bytes::from_static(&[b'x'; 16 * 1024]),
                ))))
            }
        }

        /// A peer that requests an endless response and never reads it loses
        /// its connection after the write-stall timeout, so a one-connection
        /// cap serves the next client.
        #[tokio::test]
        async fn test_serve_http1_write_stall_frees_the_permit_of_a_non_reading_peer() {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            let stall = std::time::Duration::from_millis(300);
            let app = axum::Router::new().route(
                "/stream",
                axum::routing::get(|| async { axum::body::Body::new(EndlessBody) }),
            );
            let (addr, cancel, task) = spawn_serve_http1_with(
                app,
                crate::WriteStallTimeout::new(stall).unwrap(),
                ConnectionLimit::new(1).unwrap(),
            )
            .await;

            let mut hog = tokio::net::TcpStream::connect(addr).await.unwrap();
            hog.write_all(format!("GET /stream HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let started = tokio::time::Instant::now();

            let mut next = tokio::net::TcpStream::connect(addr).await.unwrap();
            next.write_all(
                format!("GET /nowhere HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
            let mut buf = [0u8; 16];
            let n = tokio::time::timeout(
                stall + std::time::Duration::from_secs(5),
                next.read(&mut buf),
            )
            .await
            .unwrap_or_else(|_| panic!("the stalled connection must release its permit"))
            .unwrap();

            assert!(n > 0);
            assert!(
                started.elapsed() >= stall,
                "the permit was freed before the stall deadline"
            );
            drop((hog, next));
            cancel.cancel();
            task.await.unwrap();
        }

        /// Cancelling drains an idle keep-alive connection instead of
        /// waiting on it, and closes it.
        #[tokio::test]
        async fn test_serve_http1_cancel_closes_idle_keep_alive_connection_and_returns() {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            let (addr, cancel, task) = spawn_serve_http1(ConnectionLimit::DEFAULT).await;
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let request = format!("GET /nowhere HTTP/1.1\r\nHost: {addr}\r\n\r\n");
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut buf = [0u8; 512];
            assert!(stream.read(&mut buf).await.unwrap() > 0);

            cancel.cancel();

            tokio::time::timeout(std::time::Duration::from_secs(5), task)
                .await
                .unwrap_or_else(|_| panic!("serve_http1 must return after cancel"))
                .unwrap();
            assert_eq!(stream.read(&mut buf).await.unwrap(), 0);
        }

        /// A cancel arriving while every permit is taken and another client
        /// waits in the accept queue must still end the loop.
        #[tokio::test]
        async fn test_serve_http1_cancel_while_at_the_connection_cap_returns() {
            let (addr, cancel, task) = spawn_serve_http1(ConnectionLimit::new(1).unwrap()).await;
            let _held = tokio::net::TcpStream::connect(addr).await.unwrap();
            let _queued = tokio::net::TcpStream::connect(addr).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            cancel.cancel();

            tokio::time::timeout(std::time::Duration::from_secs(5), task)
                .await
                .unwrap_or_else(|_| panic!("serve_http1 must return after cancel at the cap"))
                .unwrap();
        }

        /// Builds a `McplsServer` with default collaborators and the given
        /// workspace roots, matching the setup shared by every
        /// `run_http`-driving test in this module.
        fn test_server_with_roots(workspace_roots: WorkspaceRoots) -> crate::mcp::McplsServer {
            use std::sync::Arc;

            use tokio::sync::Mutex;

            use crate::bridge::{NotificationCache, Translator};
            use crate::config::McpConfig;
            use crate::mcp::{McplsServer, SubscriptionRegistry};

            let translator = Arc::new(Translator::new());
            let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
            let subs = SubscriptionRegistry::new();
            McplsServer::new(
                translator,
                notification_cache,
                workspace_roots,
                subs,
                crate::ProjectConfigStatus::NotIgnored,
                McpConfig::default(),
            )
        }

        /// [`test_server_with_roots`] with no workspace roots configured.
        fn test_server() -> crate::mcp::McplsServer {
            test_server_with_roots(WorkspaceRoots::default())
        }

        /// Polls a TCP connect until it succeeds or the 5s budget runs out;
        /// for tests that go through `run_http`, which binds internally.
        async fn connect_with_retry(addr: SocketAddr) -> Option<tokio::net::TcpStream> {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Ok(stream) = tokio::net::TcpStream::connect(addr).await {
                    return Some(stream);
                }
                if tokio::time::Instant::now() >= deadline {
                    return None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }

        /// Binds an ephemeral loopback listener and serves `server` on it via
        /// `serve_http`. The listener is bound before this returns, so the
        /// kernel queues connections immediately and no readiness wait is needed.
        async fn spawn_http_server(
            server: crate::mcp::McplsServer,
            configure: impl FnOnce(HttpConfig) -> HttpConfig,
        ) -> (
            SocketAddr,
            tokio::task::JoinHandle<Result<(), crate::Error>>,
        ) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let cfg = configure(HttpConfig::new(addr));
            let task = tokio::spawn(super::super::serve_http(
                listener,
                server,
                cfg,
                super::super::ShutdownSignal::new(),
            ));
            (addr, task)
        }

        /// Sends a raw HTTP/1.1 POST request over TCP and returns the raw response
        /// text (status line, headers, and body). Used because neither `reqwest`
        /// nor `tower`/`http-body-util` are available as dev-dependencies here.
        async fn raw_http_post(
            addr: SocketAddr,
            path: &str,
            extra_headers: &str,
            body: &[u8],
        ) -> String {
            raw_http_post_as(addr, &addr.to_string(), path, extra_headers, body).await
        }

        /// [`raw_http_post`] with an explicit `Host` header value.
        async fn raw_http_post_as(
            addr: SocketAddr,
            host: &str,
            path: &str,
            extra_headers: &str,
            body: &[u8],
        ) -> String {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let request = format!(
                "POST {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n{extra_headers}Content-Length: {}\r\n\r\n",
                body.len()
            );
            // One write, so a server that answers from the headers alone does
            // not close on an unread body and reset the connection.
            let wire = [request.as_bytes(), body].concat();
            stream.write_all(&wire).await.unwrap();

            let mut response = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                match tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut buf))
                    .await
                {
                    Ok(Ok(0)) | Err(_) => break,
                    Ok(Ok(n)) => response.extend_from_slice(&buf[..n]),
                    // Server closes after the early 403 with the body unread, which surfaces as RST.
                    Ok(Err(e))
                        if e.kind() == std::io::ErrorKind::ConnectionReset
                            && !response.is_empty() =>
                    {
                        break;
                    }
                    Ok(Err(e)) => panic!("read error: {e}"),
                }
            }
            String::from_utf8_lossy_owned(response)
        }

        /// A POST body exceeding `cfg.max_request_body` must be rejected
        /// with `413 Payload Too Large`, proving the config value reaches
        /// `StreamableHttpServerConfig::max_request_body_bytes`.
        #[tokio::test]
        async fn test_run_http_rejects_oversized_body_with_413() {
            let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
                cfg.with_max_request_body(crate::RequestBodyLimit::new(64).unwrap())
            })
            .await;

            let oversized_body = vec![b'a'; 65];
            let response = raw_http_post(
                addr,
                "/mcp",
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n",
                &oversized_body,
            )
            .await;

            assert!(
                response.starts_with("HTTP/1.1 413"),
                "expected 413 Payload Too Large, got: {response}"
            );

            server_task.abort();
        }

        /// A POST body within `cfg.max_request_body` must not be rejected
        /// for size — it reaches JSON deserialization instead (the body here is
        /// intentionally not valid JSON-RPC, so a non-413 error distinguishes
        /// "passed the size check" from "was a valid request").
        #[tokio::test]
        async fn test_run_http_accepts_body_within_limit() {
            let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
                cfg.with_max_request_body(crate::RequestBodyLimit::new(64).unwrap())
            })
            .await;

            let small_body = vec![b'a'; 32];
            let response = raw_http_post(
                addr,
                "/mcp",
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n",
                &small_body,
            )
            .await;

            assert!(
                !response.starts_with("HTTP/1.1 413"),
                "body within limit must not be rejected as too large, got: {response}"
            );

            server_task.abort();
        }

        /// `CappedSessionManager::create_session` must enforce a hard bound:
        /// once `max_sessions` sessions exist, the next `create_session` call
        /// fails with the capacity marker, and closing a session frees the
        /// slot back up for a subsequent `create_session` to succeed.
        // `manager` is used until the end of the test — clippy's drop-tightening
        // heuristic misreads that as an early-droppable temporary because
        // `CappedSessionManager` embeds a `tokio::sync::Mutex`.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test]
        async fn test_capped_session_manager_enforces_hard_bound() {
            use rmcp::transport::streamable_http_server::session::SessionManager as _;

            let manager = super::super::CappedSessionManager::new(
                crate::SessionLimit::new(1).unwrap(),
                super::super::IdleTimeout::DEFAULT,
            );

            let (first_id, _transport) = manager.create_session().await.unwrap();

            let second_err = manager.create_session().await.map(|_| ()).unwrap_err();
            assert_matches!(
                second_err,
                super::super::CappedSessionManagerError::CapReached,
                "expected CapReached once at capacity, got: {second_err:?}"
            );

            manager.close_session(&first_id).await.unwrap();

            let (third_id, _transport) = manager.create_session().await.unwrap();
            assert_ne!(first_id, third_id);
        }

        /// Regression guard for S2: concurrent `create_session` calls must not
        /// overshoot `max_sessions`. Unlike the sequential test above (which
        /// would pass even against a racy check-then-create implementation),
        /// this spawns `N > max_sessions` calls at once and asserts exactly
        /// `max_sessions` succeed — the one test shape that actually
        /// distinguishes the atomic-semaphore design from a TOCTOU race.
        // `manager` is used until the end of the test — see the identical
        // drop-tightening note on `test_capped_session_manager_enforces_hard_bound`.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test]
        async fn test_capped_session_manager_bounds_concurrent_create_session() {
            use rmcp::transport::streamable_http_server::session::SessionManager as _;

            const MAX_SESSIONS: usize = 5;
            const CONCURRENT_ATTEMPTS: usize = 25;

            let manager = std::sync::Arc::new(super::super::CappedSessionManager::new(
                crate::SessionLimit::new(MAX_SESSIONS).unwrap(),
                super::super::IdleTimeout::DEFAULT,
            ));

            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..CONCURRENT_ATTEMPTS {
                let manager = manager.clone();
                tasks.spawn(async move { manager.create_session().await.is_ok() });
            }

            let mut succeeded = 0usize;
            while let Some(result) = tasks.join_next().await {
                if result.unwrap() {
                    succeeded += 1;
                }
            }

            assert_eq!(
                succeeded, MAX_SESSIONS,
                "exactly max_sessions concurrent create_session calls must succeed"
            );
        }

        /// Narrower unit test of the `enforce_session_cap` middleware itself
        /// (rather than the full `run_http` wiring): a `500` response whose
        /// body carries `SESSION_CAP_MARKER` must be rewritten to `429` with
        /// a `Retry-After` header.
        #[tokio::test]
        async fn test_enforce_session_cap_rewrites_capacity_marker_to_429() {
            let app = axum::Router::new()
                .route(
                    "/",
                    axum::routing::post(|| async {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            format!(
                                "Encounter an error when create session: {}: maximum concurrent \
                                 HTTP sessions already active",
                                super::super::SESSION_CAP_MARKER
                            ),
                        )
                    }),
                )
                .layer(axum::middleware::from_fn(super::super::enforce_session_cap));

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let response = raw_http_post(addr, "/", "", b"{}").await;
            assert!(
                response.starts_with("HTTP/1.1 429"),
                "expected 429 for a marker-carrying 500, got: {response}"
            );
            assert!(
                response.to_lowercase().contains("retry-after"),
                "expected a Retry-After header, got: {response}"
            );

            server_task.abort();
        }

        /// A `500` response whose body does *not* carry `SESSION_CAP_MARKER`
        /// (an unrelated internal error) must pass through unchanged, proving
        /// the middleware doesn't misclassify every `500` as a capacity
        /// rejection.
        #[tokio::test]
        async fn test_enforce_session_cap_leaves_unrelated_500_untouched() {
            let app = axum::Router::new()
                .route(
                    "/",
                    axum::routing::post(|| async {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            "Encounter an error when create session: some unrelated failure",
                        )
                    }),
                )
                .layer(axum::middleware::from_fn(super::super::enforce_session_cap));

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let response = raw_http_post(addr, "/", "", b"{}").await;
            assert!(
                response.starts_with("HTTP/1.1 500"),
                "unrelated 500s must not be rewritten to 429, got: {response}"
            );

            server_task.abort();
        }

        /// #556: a browser request from a foreign origin is rejected with `403`
        /// on POST and GET; loopback origins on the bound port and requests
        /// without `Origin` still work.
        #[tokio::test]
        async fn test_run_http_enforces_loopback_origin() {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            let (addr, server_task) = spawn_http_server(test_server(), |cfg| cfg).await;
            let initialize_body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
            let post = |origin: Option<String>| async move {
                let origin_header = origin.map_or_else(String::new, |o| format!("Origin: {o}\r\n"));
                raw_http_post(
                    addr,
                    "/mcp",
                    &format!(
                        "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n{origin_header}"
                    ),
                    initialize_body,
                )
                .await
            };

            for rejected in [
                "http://evil.example",
                "null",
                "http://127.0.0.1:1",
                "http://localhost:1",
            ] {
                let response = post(Some(rejected.to_owned())).await;
                assert!(
                    response.starts_with("HTTP/1.1 403"),
                    "Origin {rejected} should be rejected, got: {response}"
                );
            }
            for accepted in [
                Some(format!("http://127.0.0.1:{}", addr.port())),
                Some(format!("http://localhost:{}", addr.port())),
                Some(format!("http://[::1]:{}", addr.port())),
                None,
            ] {
                let response = post(accepted.clone()).await;
                assert!(
                    response.starts_with("HTTP/1.1 200"),
                    "Origin {accepted:?} should be accepted, got: {response}"
                );
            }

            for method in ["GET", "DELETE"] {
                let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                let request = format!(
                    "{method} /mcp HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: text/event-stream\r\nOrigin: http://evil.example\r\n\r\n"
                );
                stream.write_all(request.as_bytes()).await.unwrap();
                let mut response = Vec::new();
                stream.read_to_end(&mut response).await.unwrap();
                let response = String::from_utf8_lossy(&response);
                assert!(
                    response.starts_with("HTTP/1.1 403"),
                    "{method} with a foreign Origin should be rejected, got: {response}"
                );
            }

            server_task.abort();
        }

        #[test]
        fn test_allowed_origin_normalizes_scheme_host_and_port() {
            for (input, expected) in [
                ("https://app.example.com", "https://app.example.com:443"),
                ("http://app.example.com", "http://app.example.com:80"),
                (
                    "HTTPS://App.Example.COM:8443",
                    "https://app.example.com:8443",
                ),
                ("http://app.example.com/", "http://app.example.com:80"),
                ("http://[::1]:8080", "http://[::1]:8080"),
                ("http://[2001:DB8::1]", "http://[2001:db8::1]:80"),
                ("http://127.0.0.1:3000", "http://127.0.0.1:3000"),
                ("  https://a.example.com  ", "https://a.example.com:443"),
            ] {
                let origin: crate::AllowedOrigin = input.parse().unwrap();
                assert_eq!(origin.to_string(), expected, "{input}");
                assert_eq!(origin.to_string().parse(), Ok(origin), "{input}");
            }
        }

        #[test]
        fn test_allowed_origin_rejects_each_invalid_shape() {
            use crate::InvalidAllowedOrigin as Invalid;

            for (input, expected) in [
                ("null", Invalid::Null),
                ("NULL", Invalid::Null),
                ("*", Invalid::Wildcard),
                ("http://example.com:*", Invalid::Wildcard),
                ("http://*.example.com", Invalid::Wildcard),
                ("ftp://example.com", Invalid::UnsupportedScheme),
                ("example.com", Invalid::UnsupportedScheme),
                ("//example.com", Invalid::UnsupportedScheme),
                ("http://user:pass@example.com", Invalid::UserInfo),
                ("http://example.com/app", Invalid::Path),
                ("http://example.com?x=1", Invalid::Query),
                ("http://::1:8080", Invalid::Malformed),
                ("https://a.com:99999", Invalid::Malformed),
                ("https://a.com:+80", Invalid::Malformed),
                ("https://[::1]x:80", Invalid::Malformed),
                ("http://", Invalid::Malformed),
                ("", Invalid::Malformed),
            ] {
                assert_eq!(
                    input.parse::<crate::AllowedOrigin>(),
                    Err(expected),
                    "{input:?}"
                );
            }
        }

        /// #584: every configured origin is matched by rmcp's own parser, which
        /// silently drops entries it cannot read, against the `Origin` a
        /// browser would send; unlisted origins stay rejected.
        #[tokio::test]
        async fn test_run_http_accepts_configured_allowed_origins() {
            let configured = [
                "https://app.example.com",
                "http://[::1]:8080",
                "http://[2001:db8::1]:8080",
                "HTTPS://Cased.Example.com:8443",
                "http://example.org:9000",
            ];
            let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
                cfg.with_allowed_origins(
                    configured
                        .iter()
                        .map(|origin| origin.parse::<crate::AllowedOrigin>().unwrap()),
                )
            })
            .await;
            let initialize_body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
            let post = |origin: &'static str| async move {
                raw_http_post(
                    addr,
                    "/mcp",
                    &format!(
                        "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nOrigin: {origin}\r\n"
                    ),
                    initialize_body,
                )
                .await
            };

            for accepted in [
                "https://app.example.com",
                "https://app.example.com:443",
                "http://[::1]:8080",
                "http://[2001:db8::1]:8080",
                "https://cased.example.com:8443",
                "http://example.org:9000",
            ] {
                let response = post(accepted).await;
                assert!(
                    response.starts_with("HTTP/1.1 200"),
                    "Origin {accepted} should be accepted, got: {response}"
                );
            }
            for rejected in [
                "http://app.example.com",
                "https://app.example.com:444",
                "http://example.org:9001",
                "http://evil.example",
            ] {
                let response = post(rejected).await;
                assert!(
                    response.starts_with("HTTP/1.1 403"),
                    "Origin {rejected} should be rejected, got: {response}"
                );
            }

            server_task.abort();
        }

        #[test]
        fn test_allowed_host_normalizes_case_and_keeps_ipv6_brackets() {
            for (input, expected, port) in [
                ("example.com", "example.com", None),
                ("Example.COM:8080", "example.com:8080", Some(8080)),
                ("[::1]", "[::1]", None),
                ("[2001:DB8::1]:8443", "[2001:db8::1]:8443", Some(8443)),
                ("127.0.0.1:3000", "127.0.0.1:3000", Some(3000)),
                ("  a.example  ", "a.example", None),
            ] {
                let host: crate::AllowedHost = input.parse().unwrap();
                assert_eq!(host.to_string(), expected, "{input}");
                assert_eq!(host.port(), port, "{input}");
                assert_eq!(host.to_string().parse(), Ok(host), "{input}");
            }
        }

        #[test]
        fn test_allowed_host_rejects_each_invalid_shape() {
            use crate::InvalidAllowedHost as Invalid;

            for (input, expected) in [
                ("*", Invalid::Wildcard),
                ("*.example.com", Invalid::Wildcard),
                ("example.com:*", Invalid::Wildcard),
                ("user@example.com", Invalid::UserInfo),
                ("user:pass@example.com:80", Invalid::UserInfo),
                ("", Invalid::Malformed),
                ("https://example.com", Invalid::Malformed),
                ("example.com/path", Invalid::Malformed),
                ("::1", Invalid::Malformed),
                (":8080", Invalid::MissingHost),
                ("example.com:", Invalid::InvalidPort),
                ("example.com:99999", Invalid::InvalidPort),
                ("example.com.", Invalid::TrailingDot),
                ("example.com.:8080", Invalid::TrailingDot),
                ("b\u{fc}cher.example", Invalid::NonAscii),
                ("example.com:+80", Invalid::InvalidPort),
            ] {
                assert_eq!(
                    input.parse::<crate::AllowedHost>(),
                    Err(expected),
                    "{input:?}"
                );
            }
        }

        #[test]
        fn test_allowed_host_rejects_default_and_zero_ports_on_the_parsed_value() {
            use crate::InvalidAllowedHost as Invalid;

            for (input, expected) in [
                ("x.example:80", Invalid::DefaultPort),
                ("x.example:443", Invalid::DefaultPort),
                ("[::1]:443", Invalid::DefaultPort),
                ("[2001:db8::1]:80", Invalid::DefaultPort),
                ("1.2.3.4:80", Invalid::DefaultPort),
                ("localhost:443", Invalid::DefaultPort),
                ("X.EXAMPLE:443", Invalid::DefaultPort),
                ("  x.example:443  ", Invalid::DefaultPort),
                ("x.example:0443", Invalid::DefaultPort),
                ("x.example:080", Invalid::DefaultPort),
                ("x.example:000080", Invalid::DefaultPort),
                ("x.example:0", Invalid::InvalidPort),
                ("x.example:00", Invalid::InvalidPort),
                ("x.example:65536", Invalid::InvalidPort),
                ("x.example:", Invalid::InvalidPort),
                (":443", Invalid::MissingHost),
                ("*.example:443", Invalid::Wildcard),
                ("u@x.example:443", Invalid::UserInfo),
                ("::1:443", Invalid::Malformed),
                ("[::1:443", Invalid::Malformed),
            ] {
                assert_eq!(
                    input.parse::<crate::AllowedHost>(),
                    Err(expected),
                    "{input:?}"
                );
            }
        }

        #[test]
        fn test_allowed_host_accepts_every_other_port_unchanged() {
            for input in [
                "x.example",
                "x.example:8443",
                "x.example:81",
                "x.example:442",
                "x.example:1",
                "x.example:65535",
                "[::1]:8080",
            ] {
                let host: crate::AllowedHost = input.parse().unwrap();
                assert_eq!(host.to_string(), input, "{input}");
            }
        }

        #[test]
        fn test_default_port_error_carries_the_guidance() {
            let message = crate::InvalidAllowedHost::DefaultPort.to_string();
            assert!(
                message.contains("list the host without a port"),
                "{message}"
            );
        }

        #[test]
        fn test_effective_allowed_hosts_by_bind_address() {
            let names = |bind: &str, configured: &[&str]| -> Vec<String> {
                let configured: Vec<crate::AllowedHost> =
                    configured.iter().map(|h| h.parse().unwrap()).collect();
                super::super::effective_allowed_hosts(bind.parse().unwrap(), &configured)
                    .iter()
                    .map(ToString::to_string)
                    .collect()
            };
            let loopback = ["localhost", "127.0.0.1", "[::1]"];

            assert_eq!(names("127.0.0.1:3000", &[]), loopback);
            assert_eq!(names("[::1]:3000", &[]), loopback);
            assert_eq!(names("0.0.0.0:3000", &[]), loopback);
            assert_eq!(names("[::]:3000", &[]), loopback);
            assert_eq!(
                names("192.168.1.5:3000", &["mcp.example.com"]),
                [loopback.as_slice(), &["192.168.1.5", "mcp.example.com"]].concat()
            );
            assert_eq!(
                names("[2001:db8::5]:3000", &[]),
                [loopback.as_slice(), &["[2001:db8::5]"]].concat()
            );
        }

        /// A client reaching a `:80` bind by IP sends `Host` without a port,
        /// so the bound IP must reach `rmcp` portless.
        #[test]
        fn test_rmcp_config_allows_the_bound_ip_without_a_port() {
            let cfg = HttpConfig::new("192.168.1.5:80".parse().unwrap());

            let rmcp_cfg = super::super::rmcp_service_config(
                "192.168.1.5:80".parse().unwrap(),
                &cfg,
                tokio_util::sync::CancellationToken::new(),
            );

            assert!(rmcp_cfg.allowed_hosts.iter().any(|h| h == "192.168.1.5"));
            assert!(rmcp_cfg.allowed_hosts.iter().all(|h| h != "192.168.1.5:80"));
        }

        /// #597: a `Host` outside the loopback names is rejected with `403`
        /// unless it is configured; a host without a port matches any port and
        /// one with a port matches only that port.
        #[tokio::test]
        async fn test_run_http_enforces_configured_allowed_hosts() {
            let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
                cfg.with_allowed_hosts(
                    ["evil.example", "Pinned.example:8443"]
                        .map(|host| host.parse::<crate::AllowedHost>().unwrap()),
                )
            })
            .await;
            let initialize_body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
            let post = |host: String| async move {
                raw_http_post_as(
                    addr,
                    &host,
                    "/mcp",
                    "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n",
                    initialize_body,
                )
                .await
            };

            for accepted in [
                format!("localhost:{}", addr.port()),
                format!("127.0.0.1:{}", addr.port()),
                "localhost".to_owned(),
                "evil.example".to_owned(),
                "evil.example:80".to_owned(),
                "EVIL.example:1234".to_owned(),
                "pinned.example:8443".to_owned(),
            ] {
                let response = post(accepted.clone()).await;
                assert!(
                    response.starts_with("HTTP/1.1 200"),
                    "Host {accepted} should be accepted, got: {response}"
                );
            }
            for rejected in [
                "other.example",
                "evil.example.attacker.test",
                "pinned.example:1",
                "pinned.example",
            ] {
                let response = post(rejected.to_owned()).await;
                assert!(
                    response.starts_with("HTTP/1.1 403"),
                    "Host {rejected} should be rejected, got: {response}"
                );
            }

            server_task.abort();
        }

        /// Reads to EOF or error, returning what arrived and the error kind.
        async fn read_until_close(
            stream: &mut (impl tokio::io::AsyncRead + Unpin),
            limit: std::time::Duration,
        ) -> (String, Option<std::io::ErrorKind>) {
            use tokio::io::AsyncReadExt as _;

            let mut response = Vec::new();
            let mut buf = [0u8; 4096];
            let deadline = tokio::time::Instant::now() + limit;
            let error = loop {
                match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
                    Ok(Ok(0)) => break None,
                    Ok(Ok(n)) => response.extend_from_slice(&buf[..n]),
                    Ok(Err(e)) => break Some(e.kind()),
                    Err(elapsed) => panic!("the connection stayed open past {limit:?}: {elapsed}"),
                }
            };
            (String::from_utf8_lossy_owned(response), error)
        }

        /// POSTs `extra_headers` with a request body of `body_len` bytes (all of
        /// it, or never-ending when `None`) streamed while the response is read.
        async fn post_streamed_body(
            addr: SocketAddr,
            extra_headers: &str,
            body_len: Option<usize>,
            read_delay: std::time::Duration,
            limit: std::time::Duration,
        ) -> (
            String,
            Option<std::io::ErrorKind>,
            tokio::task::JoinHandle<()>,
        ) {
            use tokio::io::AsyncWriteExt as _;

            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let (mut reader, mut writer) = stream.into_split();
            let head = format!(
                "POST /mcp HTTP/1.1\r\nHost: {addr}\r\n{extra_headers}Content-Length: {}\r\n\r\n",
                body_len.unwrap_or(1 << 40)
            );
            let sender = tokio::spawn(async move {
                if writer.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                let chunk = [b'a'; 16 * 1024];
                let mut remaining = body_len;
                loop {
                    let len = remaining.map_or(chunk.len(), |r| r.min(chunk.len()));
                    if len == 0 || writer.write_all(&chunk[..len]).await.is_err() {
                        return;
                    }
                    remaining = remaining.map(|r| r - len);
                }
            });
            tokio::time::sleep(read_delay).await;
            let (response, error) = read_until_close(&mut reader, limit).await;
            (response, error, sender)
        }

        const JSON_POST_HEADERS: &str =
            "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n";

        /// #602: a `403` sent while a large request body is unread arrives
        /// intact and the connection then ends cleanly, not with a reset.
        #[tokio::test]
        async fn test_early_403_with_a_large_unread_body_delivers_the_status() {
            let (addr, server_task) = spawn_http_server(test_server(), |cfg| cfg).await;
            let headers = format!("{JSON_POST_HEADERS}Origin: http://evil.example\r\n");

            let (response, error, sender) = post_streamed_body(
                addr,
                &headers,
                Some(512 << 10),
                std::time::Duration::from_millis(300),
                std::time::Duration::from_secs(10),
            )
            .await;

            assert!(response.starts_with("HTTP/1.1 403"), "got: {response}");
            assert_eq!(error, None);
            sender.abort();
            server_task.abort();
        }

        /// #602: a `413` for a body over the limit also arrives intact.
        #[tokio::test]
        async fn test_early_413_with_a_large_unread_body_delivers_the_status() {
            let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
                cfg.with_max_request_body(crate::RequestBodyLimit::new(64).unwrap())
            })
            .await;

            let (response, error, sender) = post_streamed_body(
                addr,
                JSON_POST_HEADERS,
                Some(512 << 10),
                std::time::Duration::from_millis(300),
                std::time::Duration::from_secs(10),
            )
            .await;

            assert!(response.starts_with("HTTP/1.1 413"), "got: {response}");
            assert_eq!(error, None);
            sender.abort();
            server_task.abort();
        }

        /// #602: a peer that keeps sending after an early `403` is cut once the
        /// linger budget (`header_read_timeout` here) is spent, which frees
        /// the connection permit.
        #[tokio::test]
        async fn test_early_403_with_an_endless_body_frees_the_permit_within_the_linger_cap() {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
                cfg.with_header_read_timeout(
                    HeaderReadTimeout::new(std::time::Duration::from_millis(500)).unwrap(),
                )
                .with_max_concurrent_connections(ConnectionLimit::new(1).unwrap())
            })
            .await;
            let headers = format!("{JSON_POST_HEADERS}Origin: http://evil.example\r\n");

            let (response, _, sender) = post_streamed_body(
                addr,
                &headers,
                None,
                std::time::Duration::ZERO,
                std::time::Duration::from_secs(10),
            )
            .await;
            assert!(response.starts_with("HTTP/1.1 403"), "got: {response}");

            let mut next = tokio::net::TcpStream::connect(addr).await.unwrap();
            next.write_all(
                format!("GET /nowhere HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
            let mut buf = [0u8; 16];
            let n = tokio::time::timeout(std::time::Duration::from_secs(5), next.read(&mut buf))
                .await
                .unwrap_or_else(|_| panic!("the lingering connection must release its permit"))
                .unwrap();

            assert!(n > 0);
            sender.abort();
            server_task.abort();
        }

        /// End-to-end: with `max_concurrent_sessions(1)`, a second concurrent
        /// `initialize` handshake over `run_http` must be rejected with `429`
        /// once the first session is established.
        #[tokio::test]
        async fn test_run_http_rejects_new_session_at_capacity_with_429() {
            let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
                cfg.with_max_concurrent_sessions(crate::SessionLimit::new(1).unwrap())
            })
            .await;

            let initialize_body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
            let accept_headers =
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n";

            // First handshake must succeed and establish a session.
            let first = raw_http_post(addr, "/mcp", accept_headers, initialize_body).await;
            assert!(
                first.starts_with("HTTP/1.1 200"),
                "first initialize handshake should succeed, got: {first}"
            );

            // Second handshake, with the sole slot still held, must be capped.
            let second = raw_http_post(addr, "/mcp", accept_headers, initialize_body).await;
            assert!(
                second.starts_with("HTTP/1.1 429"),
                "second initialize handshake should be rejected once at capacity, got: {second}"
            );

            server_task.abort();
        }

        /// S1 non-regression: a non-`initialize` request carrying SEP-2575
        /// per-request `_meta` protocol-version metadata
        /// (`io.modelcontextprotocol/protocolVersion` = `2026-07-28` plus the
        /// required `clientCapabilities` key) takes rmcp 3.2.0's stateless
        /// discover-lifecycle path and never calls
        /// `SessionManager::create_session` — `rmcp` serves it directly
        /// without touching the session table — so it must not be rejected
        /// by the cap even while `max_concurrent_sessions` legacy sessions
        /// are already active. (An `initialize` request is always
        /// classified legacy in 3.2.0 regardless of the protocol version it
        /// names, so it cannot be used to probe the stateless path.) This
        /// guards against a future refactor reintroducing request-header
        /// sniffing for the cap decision (the bug this design replaced).
        #[tokio::test]
        async fn test_run_http_stateless_request_bypasses_session_cap() {
            let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
                cfg.with_max_concurrent_sessions(crate::SessionLimit::new(1).unwrap())
            })
            .await;

            let accept_headers =
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n";

            // Fill the sole legacy-session slot.
            let legacy_initialize = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
            let legacy = raw_http_post(addr, "/mcp", accept_headers, legacy_initialize).await;
            assert!(
                legacy.starts_with("HTTP/1.1 200"),
                "legacy initialize should succeed and consume the sole session slot, got: {legacy}"
            );

            // A non-`initialize` request carrying per-request `_meta`
            // protocol-version metadata takes rmcp 3.2.0's stateless
            // discover-lifecycle path and never creates a session, so it
            // must bypass the cap entirely even though the slot above is
            // still held. The `MCP-Protocol-Version` header must match the
            // `_meta` value once the latter is present, and declaring
            // `2026-07-28` also brings in SEP-2243's `Mcp-Method` header
            // requirement.
            let stateless_headers = "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: resources/list\r\n";
            let stateless_request = br#"{"jsonrpc":"2.0","id":2,"method":"resources/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
            let stateless = raw_http_post(addr, "/mcp", stateless_headers, stateless_request).await;
            assert!(
                stateless.starts_with("HTTP/1.1 200"),
                "stateless requests must bypass the session cap entirely, got: {stateless}"
            );

            server_task.abort();
        }

        /// #478/#482/#492 regression pin: on rmcp's stateless path (see
        /// `test_run_http_stateless_request_bypasses_session_cap` above), the
        /// service factory -- and therefore `McplsServer::for_new_session` --
        /// runs once per *request*. Such an instance never subscribes, so it
        /// must never enter `SubscriptionRegistry` at all, however many
        /// requests are served.
        #[tokio::test]
        async fn test_stateless_requests_never_register_with_subscription_registry() {
            const REQUEST_COUNT: u32 = 20;

            let server = test_server();
            let registry = server.subscription_registry();

            let (addr, server_task) = spawn_http_server(server, |cfg| cfg).await;

            let stateless_headers = "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: resources/list\r\n";

            for id in 0..REQUEST_COUNT {
                let body = format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"method":"resources/list","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#
                );
                let response =
                    raw_http_post(addr, "/mcp", stateless_headers, body.as_bytes()).await;
                assert!(
                    response.starts_with("HTTP/1.1 200"),
                    "stateless request {id} should succeed, got: {response}"
                );
            }

            assert_eq!(
                registry.raw_len(),
                0,
                "stateless instances must never register with the subscription registry"
            );

            server_task.abort();
        }

        /// Shared setup for the `#482` regression tests below: a real
        /// `run_http` server with one file inside its sole workspace root, so
        /// `resources/subscribe` requests validate and reach the handler.
        struct SubscribeTestServer {
            addr: SocketAddr,
            registry: crate::mcp::SubscriptionRegistry,
            uri: String,
            server_task: tokio::task::JoinHandle<Result<(), crate::Error>>,
            // Held so the file `subscribe` canonicalizes stays on disk.
            _workspace: tempfile::TempDir,
        }

        async fn spawn_subscribe_test_server() -> SubscribeTestServer {
            let workspace = tempfile::TempDir::new().unwrap();
            let file_path = workspace.path().join("main.rs");
            std::fs::write(&file_path, "fn main() {}").unwrap();
            let uri = crate::bridge::resources::make_uri(&file_path).unwrap();

            let server = test_server_with_roots(
                WorkspaceRoots::from_configured(&[workspace.path().to_path_buf()]).unwrap(),
            );
            let registry = server.subscription_registry();

            let (addr, server_task) = spawn_http_server(server, |cfg| cfg).await;

            SubscribeTestServer {
                addr,
                registry,
                uri,
                server_task,
                _workspace: workspace,
            }
        }

        /// #482: `_meta` negotiating `2026-07-28` per request is stateless by
        /// both rmcp and mcpls' reckoning, so rmcp itself answers
        /// `-32601 method not found` before dispatch.
        #[tokio::test]
        async fn test_stateless_subscribe_negotiated_per_request_is_rejected_by_rmcp() {
            let srv = spawn_subscribe_test_server().await;
            let uri = &srv.uri;

            let headers = format!(
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: resources/subscribe\r\nMcp-Name: {uri}\r\n"
            );
            let body = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"resources/subscribe","params":{{"uri":"{uri}","_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#
            );
            let response = raw_http_post(srv.addr, "/mcp", &headers, body.as_bytes()).await;
            assert!(
                response.contains("-32601"),
                "expected rmcp to refuse dispatching a 2026-07-28-negotiated resources/subscribe \
                 (method not found), got: {response}"
            );

            srv.server_task.abort();
        }

        /// #482 regression matrix: `_meta` naming a pre-`2026-07-28` version
        /// (see [`super::super::request_uses_discover_lifecycle_meta`]'s docs
        /// for why rmcp still serves this statelessly) must be rejected by
        /// mcpls' own guard for both `subscribe` and `unsubscribe`, and a
        /// fabricated `Mcp-Session-Id` must not bypass it.
        #[tokio::test]
        async fn test_stateless_lifecycle_mismatch_is_rejected_by_mcpls() {
            let srv = spawn_subscribe_test_server().await;
            let uri = &srv.uri;

            for (method, extra_header) in [
                ("resources/subscribe", ""),
                (
                    "resources/subscribe",
                    "Mcp-Session-Id: not-a-real-session\r\n",
                ),
                ("resources/unsubscribe", ""),
            ] {
                let headers = format!(
                    "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2025-06-18\r\n{extra_header}"
                );
                let body = format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{{"uri":"{uri}","_meta":{{"io.modelcontextprotocol/protocolVersion":"2025-06-18","io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#
                );
                let response = raw_http_post(srv.addr, "/mcp", &headers, body.as_bytes()).await;
                assert!(
                    response.contains("-32052") && !response.contains(r#""result":{}"#),
                    "{method} (extra header: {extra_header:?}) must be rejected by mcpls' \
                     stateless-subscription guard, not silently succeed, got: {response}"
                );
            }
            assert_eq!(
                srv.registry.raw_len(),
                0,
                "a rejected stateless subscribe must never register a session"
            );

            srv.server_task.abort();
        }

        /// #482 non-regression: a legacy session's `resources/subscribe`,
        /// sent with the session's assigned `Mcp-Session-Id` echoed back and
        /// no per-request `_meta`, must not be rejected by either guard above.
        #[tokio::test]
        async fn test_legacy_session_subscribe_is_not_rejected_as_stateless() {
            let srv = spawn_subscribe_test_server().await;
            let uri = &srv.uri;
            let session_id = initialize_legacy_session(srv.addr).await;

            let session_headers = format!(
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMcp-Session-Id: {session_id}\r\n"
            );
            let subscribe_body = format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"resources/subscribe","params":{{"uri":"{uri}"}}}}"#
            );
            let session_response = raw_http_post(
                srv.addr,
                "/mcp",
                &session_headers,
                subscribe_body.as_bytes(),
            )
            .await;
            assert!(
                !session_response.contains("-32601") && !session_response.contains("-32052"),
                "a legacy session's subscribe must not be rejected as stateless, got: \
                 {session_response}"
            );

            srv.server_task.abort();
        }

        /// #482: a live session's `resources/subscribe` that also carries
        /// per-request discover-lifecycle `_meta` is rejected too -- rmcp
        /// serves it statelessly regardless of the session id, so this is
        /// intentional, not a regression.
        #[tokio::test]
        async fn test_legacy_session_subscribe_with_discover_meta_is_rejected_as_stateless() {
            let srv = spawn_subscribe_test_server().await;
            let uri = &srv.uri;
            let session_id = initialize_legacy_session(srv.addr).await;

            let headers = format!(
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMcp-Session-Id: {session_id}\r\nMCP-Protocol-Version: 2025-06-18\r\n"
            );
            let body = format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"resources/subscribe","params":{{"uri":"{uri}","_meta":{{"io.modelcontextprotocol/protocolVersion":"2025-06-18","io.modelcontextprotocol/clientCapabilities":{{}}}}}}}}"#
            );
            let response = raw_http_post(srv.addr, "/mcp", &headers, body.as_bytes()).await;
            assert!(
                response.contains("-32052"),
                "a live session's subscribe with per-request discover _meta must still be \
                 rejected -- rmcp serves it statelessly regardless of the session id, got: \
                 {response}"
            );

            srv.server_task.abort();
        }

        /// Performs the `initialize` handshake for a legacy HTTP session and
        /// returns its assigned `Mcp-Session-Id`.
        async fn initialize_legacy_session(addr: SocketAddr) -> String {
            let accept_headers =
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n";
            let initialize_body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
            let init_response = raw_http_post(addr, "/mcp", accept_headers, initialize_body).await;
            assert!(
                init_response.starts_with("HTTP/1.1 200"),
                "legacy initialize should succeed, got: {init_response}"
            );
            init_response
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("mcp-session-id")
                        .then(|| value.trim().to_string())
                })
                .unwrap()
        }

        /// A raw-TCP Server-Sent-Events GET stream for one MCP session.
        struct SseStream {
            stream: tokio::net::TcpStream,
            buf: String,
        }

        impl SseStream {
            /// Opens the session's standalone GET stream and returns once the
            /// `200` response headers have arrived, so nothing published
            /// afterwards can race the stream's registration.
            async fn open(addr: SocketAddr, session_id: &str) -> Self {
                let request = format!(
                    "GET /mcp HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\nMcp-Session-Id: {session_id}\r\n\r\n"
                );
                Self::send(addr, request.as_bytes()).await
            }

            /// Opens a stateless 2026-07-28 `subscriptions/listen` stream.
            async fn open_listen(addr: SocketAddr, notifications: &serde_json::Value) -> Self {
                let body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "subscriptions/listen",
                    "params": {
                        "notifications": notifications,
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {},
                        },
                    },
                })
                .to_string();
                let request = format!(
                    "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: subscriptions/listen\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                Self::send(addr, request.as_bytes()).await
            }

            async fn send(addr: SocketAddr, request: &[u8]) -> Self {
                use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

                let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                stream.write_all(request).await.unwrap();

                let mut head = String::new();
                let mut chunk = [0u8; 4096];
                let body_start = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let n = stream.read(&mut chunk).await.unwrap();
                        assert!(n > 0, "GET stream closed before headers arrived");
                        head.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        if let Some(pos) = head.find("\r\n\r\n") {
                            return pos + 4;
                        }
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("GET stream headers not received within 5 s"));
                assert!(
                    head.starts_with("HTTP/1.1 200"),
                    "GET stream must open with 200, got: {head}"
                );
                Self {
                    stream,
                    buf: head[body_start..].to_owned(),
                }
            }

            /// Next JSON-RPC message on the stream, skipping `retry:` priming
            /// events, keep-alive comments and chunked-encoding framing.
            async fn next_message(&mut self) -> serde_json::Value {
                use tokio::io::AsyncReadExt as _;

                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        while let Some(pos) = self.buf.find('\n') {
                            let line: String = self.buf.drain(..=pos).collect();
                            let Some(data) = line.trim().strip_prefix("data:") else {
                                continue;
                            };
                            if let Ok(message) =
                                serde_json::from_str::<serde_json::Value>(data.trim())
                            {
                                return message;
                            }
                        }
                        let mut chunk = [0u8; 4096];
                        let n = self.stream.read(&mut chunk).await.unwrap();
                        assert!(n > 0, "stream closed before the expected message");
                        self.buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("no message on the stream within 5 s"))
            }

            /// Next liveness `ping` request on the stream.
            async fn next_ping(&mut self) -> serde_json::Value {
                loop {
                    let message = self.next_message().await;
                    if message["method"] == "ping" {
                        return message;
                    }
                }
            }

            /// Whether the server ends the stream within `limit`.
            async fn ends_within(&mut self, limit: std::time::Duration) -> bool {
                use tokio::io::AsyncReadExt as _;

                tokio::time::timeout(limit, async {
                    loop {
                        if self.buf.contains("0\r\n\r\n") {
                            return;
                        }
                        let mut chunk = [0u8; 4096];
                        let n = self.stream.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        self.buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    }
                })
                .await
                .is_ok()
            }

            /// Next `notifications/resources/updated` message.
            async fn next_resource_update_message(&mut self) -> serde_json::Value {
                loop {
                    let message = self.next_message().await;
                    if message["method"] == "notifications/resources/updated" {
                        return message;
                    }
                }
            }

            /// Next `notifications/resources/updated` URI on the stream.
            async fn next_resource_update(&mut self) -> String {
                self.next_resource_update_message().await["params"]["uri"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            }
        }

        /// POSTs `body` within `session_id` and returns the raw response.
        async fn post_in_session(addr: SocketAddr, session_id: &str, body: &str) -> String {
            let headers = format!(
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMcp-Session-Id: {session_id}\r\n"
            );
            raw_http_post(addr, "/mcp", &headers, body.as_bytes()).await
        }

        /// Completes the handshake for a fresh legacy session and opens its
        /// GET stream.
        async fn establish_session(addr: SocketAddr) -> (String, SseStream) {
            let session_id = initialize_legacy_session(addr).await;
            let initialized = post_in_session(
                addr,
                &session_id,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            )
            .await;
            assert!(
                initialized.starts_with("HTTP/1.1 202"),
                "notifications/initialized must be accepted, got: {initialized}"
            );
            let stream = SseStream::open(addr, &session_id).await;
            (session_id, stream)
        }

        async fn subscribe_in_session(addr: SocketAddr, session_id: &str, uri: &str) {
            let body = format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"resources/subscribe","params":{{"uri":"{uri}"}}}}"#
            );
            let response = post_in_session(addr, session_id, &body).await;
            assert!(
                response.starts_with("HTTP/1.1 200") && !response.contains(r#""error""#),
                "subscribe to {uri} must succeed, got: {response}"
            );
        }

        /// #468 end to end over HTTP: each session's GET stream carries
        /// `resources/updated` only for that session's own subscriptions.
        /// A subscribes to X and Y, B only to Y; X is published first, so B's
        /// first update being Y proves X never reached B.
        #[tokio::test]
        async fn test_http_sessions_receive_updates_only_for_own_subscriptions() {
            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let file_x = root.join("x.rs");
            let file_y = root.join("y.rs");
            std::fs::write(&file_x, "fn x() {}").unwrap();
            std::fs::write(&file_y, "fn y() {}").unwrap();
            let uri_x = crate::bridge::resources::make_uri(&file_x).unwrap();
            let uri_y = crate::bridge::resources::make_uri(&file_y).unwrap();

            let server = test_server_with_roots(
                WorkspaceRoots::from_configured(std::slice::from_ref(&root)).unwrap(),
            );
            let registry = server.subscription_registry();
            let (addr, server_task) = spawn_http_server(server, |cfg| cfg).await;

            let (session_a, mut stream_a) = establish_session(addr).await;
            let (session_b, mut stream_b) = establish_session(addr).await;
            subscribe_in_session(addr, &session_a, &uri_x).await;
            subscribe_in_session(addr, &session_a, &uri_y).await;
            subscribe_in_session(addr, &session_b, &uri_y).await;

            let (tx, _cancel_tx) = crate::test_lsp::spawn_test_pump(
                registry,
                WorkspaceRoots::from_configured(&[root]).unwrap(),
            );
            let publish = |file: &std::path::Path| {
                let notification = crate::lsp::LspNotification::PublishDiagnostics(
                    lsp_types::PublishDiagnosticsParams {
                        uri: crate::bridge::path_to_uri(file).unwrap(),
                        diagnostics: vec![],
                        version: None,
                    },
                );
                let tx = tx.clone();
                async move { tx.send(notification).await.unwrap() }
            };

            publish(&file_x).await;
            assert_eq!(stream_a.next_resource_update().await, uri_x);
            // Gives a wrongly queued X time to reach B's stream before Y exists.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            publish(&file_y).await;
            assert_eq!(stream_b.next_resource_update().await, uri_y);
            assert_eq!(stream_a.next_resource_update().await, uri_y);

            server_task.abort();
        }

        const TEST_IDLE: std::time::Duration = std::time::Duration::from_secs(2);
        const E2E_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

        #[test]
        fn test_idle_timeout_rejects_zero() {
            assert_eq!(IdleTimeout::new(std::time::Duration::ZERO), None);
            assert!(IdleTimeout::new(std::time::Duration::from_nanos(1)).is_some());
        }

        #[test]
        fn test_idle_timeout_sweep_interval_is_never_zero() {
            let tiny = IdleTimeout::new(std::time::Duration::from_nanos(1)).unwrap();
            assert!(!tiny.sweep_interval().is_zero());
            assert_eq!(
                IdleTimeout::new(std::time::Duration::from_secs(10))
                    .unwrap()
                    .sweep_interval(),
                std::time::Duration::from_secs(2)
            );
        }

        fn idle_secs(secs: u64) -> IdleTimeout {
            IdleTimeout::new(std::time::Duration::from_secs(secs)).unwrap()
        }

        #[tokio::test(start_paused = true)]
        async fn test_session_with_open_stream_is_never_idle() {
            let activity = SessionActivity::new();
            let guard = activity.open_stream();
            tokio::time::advance(std::time::Duration::from_mins(1)).await;
            assert!(!activity.is_idle(tokio::time::Instant::now(), idle_secs(5)));

            drop(guard);
            assert!(
                !activity.is_idle(tokio::time::Instant::now(), idle_secs(5)),
                "closing the last stream restarts the idle clock"
            );
            tokio::time::advance(std::time::Duration::from_secs(5)).await;
            assert!(activity.is_idle(tokio::time::Instant::now(), idle_secs(5)));
        }

        #[tokio::test(start_paused = true)]
        async fn test_touch_restarts_idle_clock() {
            let activity = SessionActivity::new();
            tokio::time::advance(std::time::Duration::from_secs(4)).await;
            activity.touch();
            tokio::time::advance(std::time::Duration::from_secs(4)).await;
            assert!(!activity.is_idle(tokio::time::Instant::now(), idle_secs(5)));
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
            assert!(activity.is_idle(tokio::time::Instant::now(), idle_secs(5)));
        }

        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_reap_idle_frees_permit_and_spares_touched_sessions() {
            let manager =
                CappedSessionManager::new(crate::SessionLimit::new(2).unwrap(), idle_secs(10));
            let (idle_id, _idle_transport) = manager.create_session().await.unwrap();
            let (busy_id, _busy_transport) = manager.create_session().await.unwrap();
            assert!(matches!(
                manager.create_session().await,
                Err(CappedSessionManagerError::CapReached)
            ));

            tokio::time::advance(std::time::Duration::from_secs(6)).await;
            manager.touch(&busy_id);
            tokio::time::advance(std::time::Duration::from_secs(6)).await;

            assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 1);
            assert_eq!(manager.semaphore.available_permits(), 1);
            assert!(manager.activity(&idle_id).is_none());
            assert!(manager.activity(&busy_id).is_some());
            assert!(manager.create_session().await.is_ok());
        }

        #[tokio::test(start_paused = true)]
        async fn test_close_session_bounded_drops_a_wedged_close_at_the_timeout() {
            struct SetOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
            impl Drop for SetOnDrop {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }

            let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let guard = SetOnDrop(std::sync::Arc::clone(&dropped));
            let id: rmcp::transport::streamable_http_server::session::SessionId = "wedged".into();
            let closer = tokio::spawn(async move {
                super::super::close_session_bounded(&id, async move {
                    let _guard = guard;
                    std::future::pending::<Result<(), std::convert::Infallible>>().await
                })
                .await;
            });

            tokio::task::yield_now().await;
            tokio::time::advance(
                super::super::liveness::SESSION_CLOSE_TIMEOUT + std::time::Duration::from_secs(1),
            )
            .await;
            tokio::time::timeout(std::time::Duration::from_secs(1), closer)
                .await
                .unwrap()
                .unwrap();
            assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        }

        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_run_idle_reaper_closes_idle_session_and_stops_on_cancel() {
            let manager = std::sync::Arc::new(CappedSessionManager::new(
                crate::SessionLimit::new(1).unwrap(),
                idle_secs(10),
            ));
            let (id, _transport) = manager.create_session().await.unwrap();
            let cancel = tokio_util::sync::CancellationToken::new();
            let reaper = tokio::spawn(run_idle_reaper(
                std::sync::Arc::clone(&manager),
                cancel.clone(),
            ));

            tokio::time::sleep(std::time::Duration::from_secs(13)).await;
            assert!(manager.activity(&id).is_none());
            assert_eq!(manager.semaphore.available_permits(), 1);
            assert!(!manager.has_session(&id).await.unwrap());

            cancel.cancel();
            tokio::time::timeout(std::time::Duration::from_secs(1), reaper)
                .await
                .unwrap()
                .unwrap();
        }

        /// Creates a session and serves an initialized MCP server on it, since
        /// the session worker answers stream requests only after the handshake.
        async fn initialized_session(
            manager: &CappedSessionManager,
        ) -> (
            rmcp::transport::streamable_http_server::session::SessionId,
            tokio::task::JoinHandle<()>,
        ) {
            initialized_session_serving(manager, test_server()).await
        }

        /// [`initialized_session`] serving `server`.
        async fn initialized_session_serving(
            manager: &CappedSessionManager,
            server: crate::mcp::McplsServer,
        ) -> (
            rmcp::transport::streamable_http_server::session::SessionId,
            tokio::task::JoinHandle<()>,
        ) {
            use rmcp::ServiceExt as _;

            let (id, transport) = manager.create_session().await.unwrap();
            let serving = tokio::spawn(async move {
                if let Ok(running) = server.serve(transport).await {
                    running.waiting().await.ok();
                }
            });
            let initialize: ClientJsonRpcMessage = serde_json::from_value(serde_json::json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0"},
                },
            }))
            .unwrap();
            manager.initialize_session(&id, initialize).await.unwrap();
            let initialized: ClientJsonRpcMessage = serde_json::from_value(
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            )
            .unwrap();
            manager.accept_message(&id, initialized).await.unwrap();
            (id, serving)
        }

        /// Holds `stream` (a response stream of the manager's only session)
        /// unpolled past the idle timeout and checks it keeps the session from
        /// being reaped until it is dropped.
        async fn assert_open_stream_blocks_reaping<S>(manager: &CappedSessionManager, stream: S) {
            tokio::time::advance(std::time::Duration::from_secs(30)).await;
            assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 0);

            drop(stream);
            tokio::task::yield_now().await;
            tokio::time::advance(std::time::Duration::from_secs(10)).await;
            assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 1);
        }

        /// #587: a response stream ends at its deadline, and only then does
        /// the session become reapable (after the idle timeout).
        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_bounded_stream_ends_at_deadline_then_session_is_reaped() {
            use futures::StreamExt as _;

            let deadline =
                crate::ResponseStreamDeadline::new(std::time::Duration::from_secs(20)).unwrap();
            let manager =
                CappedSessionManager::new(crate::SessionLimit::new(1).unwrap(), idle_secs(10))
                    .with_response_stream_deadline(deadline);
            let (id, _transport) = manager.create_session().await.unwrap();
            let mut stream = Box::pin(CappedSessionManager::bounded(
                manager.open_guard(&id),
                futures::stream::pending::<u8>(),
                deadline,
                id.clone(),
            ));
            let reader = tokio::spawn(async move { stream.next().await });

            tokio::time::advance(std::time::Duration::from_secs(19)).await;
            tokio::task::yield_now().await;
            assert!(!reader.is_finished());
            assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 0);

            tokio::time::advance(std::time::Duration::from_secs(1)).await;
            assert_eq!(reader.await.unwrap(), None);
            assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 0);

            tokio::time::advance(std::time::Duration::from_secs(10)).await;
            assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 1);
        }

        /// A server whose only language server never answers, and a `tools/call`
        /// that therefore keeps its response stream open.
        fn hung_tool_call() -> (
            tempfile::TempDir,
            crate::test_lsp::FakeServer,
            crate::mcp::McplsServer,
            ClientJsonRpcMessage,
        ) {
            use std::sync::Arc;

            use tokio::sync::Mutex;

            use crate::bridge::{NotificationCache, Translator};
            use crate::config::{McpConfig, ServerId, ToolRouter};
            use crate::mcp::{McplsServer, SubscriptionRegistry};

            let dir = tempfile::TempDir::new().unwrap();
            let file = dir.path().join("lib.rs");
            std::fs::write(&file, "fn main() {}").unwrap();
            let roots = WorkspaceRoots::from_configured(&[dir.path().to_path_buf()]).unwrap();
            let mut translator = Translator::new()
                .with_extensions(crate::test_lsp::test_extensions())
                .with_router(ToolRouter::catch_all([(
                    ServerId::from("rust"),
                    LanguageId::from_static("rust"),
                )]));
            translator.set_workspace_roots(roots.clone());
            let (client, fake_lsp) = crate::test_lsp::fake_lsp_client();
            translator.register_client("rust".to_string(), client);
            let server = McplsServer::new(
                Arc::new(translator),
                Arc::new(Mutex::new(NotificationCache::new())),
                roots,
                SubscriptionRegistry::new(),
                crate::ProjectConfigStatus::NotIgnored,
                McpConfig::default(),
            );
            let call = serde_json::from_value(serde_json::json!({
                "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": {
                    "name": "get_hover",
                    "arguments": {"file_path": file.to_string_lossy(), "line": 1, "character": 4}
                }
            }))
            .unwrap();
            (dir, fake_lsp, server, call)
        }

        /// A session manager whose response streams are cut after 20 s.
        fn manager_with_short_deadline() -> CappedSessionManager {
            let deadline =
                crate::ResponseStreamDeadline::new(std::time::Duration::from_secs(20)).unwrap();
            CappedSessionManager::new(crate::SessionLimit::new(1).unwrap(), idle_secs(3600))
                .with_response_stream_deadline(deadline)
        }

        /// #587 wiring: `create_stream` hands out a bounded stream. The tool
        /// call never completes, so only the deadline can end the stream.
        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_create_stream_is_cut_at_the_response_stream_deadline() {
            use futures::StreamExt as _;

            let manager = manager_with_short_deadline();
            let (_dir, _unread_lsp, server, call) = hung_tool_call();
            let (id, serving) = initialized_session_serving(&manager, server).await;

            let mut stream = Box::pin(manager.create_stream(&id, call).await.unwrap());
            assert!(
                stream.next().await.is_some(),
                "the open stream announces itself"
            );
            let early =
                tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await;
            assert!(
                early.is_err(),
                "control: open before the deadline, got {early:?}"
            );

            let late =
                tokio::time::timeout(std::time::Duration::from_secs(30), stream.next()).await;
            assert_matches!(late, Ok(None), "stream outlived its deadline");
            serving.abort();
        }

        /// #587 wiring: a request-wise `resume` (not the common channel) is
        /// bounded too.
        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_request_wise_resume_is_cut_at_the_response_stream_deadline() {
            use futures::StreamExt as _;

            let manager = manager_with_short_deadline();
            let (_dir, _unread_lsp, server, call) = hung_tool_call();
            let (id, serving) = initialized_session_serving(&manager, server).await;
            let mut first = Box::pin(manager.create_stream(&id, call).await.unwrap());
            let primed = tokio::time::timeout(std::time::Duration::from_secs(1), first.next())
                .await
                .unwrap()
                .unwrap();
            let primed_id = primed.event_id.unwrap();
            let (_, request) = primed_id.split_once('/').unwrap();

            let mut resumed = Box::pin(manager.resume(&id, format!("0/{request}")).await.unwrap());

            let late = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                while resumed.next().await.is_some() {}
            })
            .await;
            assert!(late.is_ok(), "resumed stream outlived its deadline");
            serving.abort();
        }

        /// A one-session manager with a 10 s idle timeout whose probes are far
        /// enough apart that they never fire within a test.
        fn quietly_probing_manager() -> CappedSessionManager {
            let hour = std::time::Duration::from_secs(3600);
            CappedSessionManager::new(crate::SessionLimit::new(1).unwrap(), idle_secs(10))
                .with_stream_liveness(StreamLiveness::Probe {
                    interval: ProbeInterval::new(hour).unwrap(),
                    deadline: ProbeDeadline::new(hour).unwrap(),
                })
        }

        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_open_resume_stream_blocks_reaping() {
            let manager = quietly_probing_manager();
            let (id, serving) = initialized_session(&manager).await;
            let stream = manager.resume(&id, "0".to_owned()).await.unwrap();
            assert_open_stream_blocks_reaping(&manager, stream).await;
            serving.abort();
        }

        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_open_post_stream_blocks_reaping() {
            let manager =
                CappedSessionManager::new(crate::SessionLimit::new(1).unwrap(), idle_secs(10));
            let (id, serving) = initialized_session(&manager).await;
            let ping: ClientJsonRpcMessage = serde_json::from_value(
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}),
            )
            .unwrap();
            let stream = manager.create_stream(&id, ping).await.unwrap();
            assert_open_stream_blocks_reaping(&manager, stream).await;
            serving.abort();
        }

        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_open_standalone_stream_blocks_reaping() {
            let manager = quietly_probing_manager();
            let (id, serving) = initialized_session(&manager).await;
            let stream = manager.create_standalone_stream(&id).await.unwrap();
            assert_open_stream_blocks_reaping(&manager, stream).await;
            serving.abort();
        }

        fn probing_manager(
            interval: std::time::Duration,
            deadline: std::time::Duration,
        ) -> std::sync::Arc<CappedSessionManager> {
            std::sync::Arc::new(
                CappedSessionManager::new(crate::SessionLimit::new(4).unwrap(), idle_secs(3600))
                    .with_stream_liveness(StreamLiveness::Probe {
                        interval: ProbeInterval::new(interval).unwrap(),
                        deadline: ProbeDeadline::new(deadline).unwrap(),
                    }),
            )
        }

        fn probe_reply(
            message: &rmcp::transport::streamable_http_server::session::ServerSseMessage,
        ) -> Option<ClientJsonRpcMessage> {
            let json = serde_json::to_value(&**message.message.as_ref()?).ok()?;
            (json["method"] == "ping").then(|| {
                serde_json::from_value(
                    serde_json::json!({"jsonrpc": "2.0", "id": json["id"], "result": {}}),
                )
                .unwrap()
            })
        }

        /// Drains `stream`, answering every probe in `answer_in` when `answering`;
        /// finishes when the stream ends.
        fn spawn_drain<S>(
            manager: &std::sync::Arc<CappedSessionManager>,
            answer_in: &rmcp::transport::streamable_http_server::session::SessionId,
            stream: S,
            answering: bool,
        ) -> tokio::task::JoinHandle<()>
        where
            S: futures::Stream<
                    Item = rmcp::transport::streamable_http_server::session::ServerSseMessage,
                > + Send
                + 'static,
        {
            use futures::StreamExt as _;

            let manager = std::sync::Arc::clone(manager);
            let answer_in = answer_in.clone();
            tokio::spawn(async move {
                let mut stream = Box::pin(stream);
                while let Some(message) = stream.next().await {
                    if let Some(reply) = probe_reply(&message).filter(|_| answering) {
                        manager.accept_message(&answer_in, reply).await.unwrap();
                    }
                }
            })
        }

        const PROBE_STEP: std::time::Duration = std::time::Duration::from_millis(100);

        /// A probing manager with one initialized session; `serving` is the
        /// session worker's task.
        async fn probed_session() -> (
            std::sync::Arc<CappedSessionManager>,
            rmcp::transport::streamable_http_server::session::SessionId,
            tokio::task::JoinHandle<()>,
        ) {
            let manager = probing_manager(PROBE_STEP, PROBE_STEP * 2);
            let (id, serving) = initialized_session(&manager).await;
            (manager, id, serving)
        }

        /// #573: rmcp's own `keep_alive` must not end a session whose client
        /// answers probes on an open GET stream.
        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_answering_get_listener_outlives_rmcp_keep_alive() {
            let (manager, id, serving) = probed_session().await;
            let stream = manager.create_standalone_stream(&id).await.unwrap();
            let listener = spawn_drain(&manager, &id, stream, true);

            tokio::time::sleep(rmcp::transport::streamable_http_server::session::local::SessionConfig::DEFAULT_KEEP_ALIVE * 2).await;

            assert!(manager.has_session(&id).await.unwrap());
            assert!(!listener.is_finished());
            listener.abort();
            serving.abort();
        }

        /// With probing off, an open GET stream no longer holds a session: a
        /// quiet peer that may have vanished expires within the idle timeout.
        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_disabled_liveness_get_stream_does_not_block_reaping() {
            let manager =
                CappedSessionManager::new(crate::SessionLimit::new(1).unwrap(), idle_secs(10));
            let (id, serving) = initialized_session(&manager).await;
            let _stream = manager.create_standalone_stream(&id).await.unwrap();

            tokio::time::advance(std::time::Duration::from_secs(30)).await;

            assert_eq!(manager.reap_idle(tokio::time::Instant::now()), 1);
            serving.abort();
        }

        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_primary_timeout_closes_standalone_stream_and_clears_shadows() {
            let (manager, id, serving) = probed_session().await;
            let primary = manager.create_standalone_stream(&id).await.unwrap();
            let shadow = manager.create_standalone_stream(&id).await.unwrap();
            let primary_task = spawn_drain(&manager, &id, primary, false);
            let shadow_task = spawn_drain(&manager, &id, shadow, true);

            tokio::time::timeout(std::time::Duration::from_secs(2), primary_task)
                .await
                .unwrap_or_else(|_| panic!("silent primary must end"))
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), shadow_task)
                .await
                .unwrap_or_else(|_| {
                    panic!("closing the primary's stream must end the answering shadow too")
                })
                .unwrap();
            serving.abort();
        }

        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_shadow_timeout_leaves_primary_stream_open() {
            let (manager, id, serving) = probed_session().await;
            let primary = manager.create_standalone_stream(&id).await.unwrap();
            let shadow = manager.create_standalone_stream(&id).await.unwrap();
            let primary_task = spawn_drain(&manager, &id, primary, true);
            let shadow_task = spawn_drain(&manager, &id, shadow, false);

            tokio::time::timeout(std::time::Duration::from_secs(2), shadow_task)
                .await
                .unwrap_or_else(|_| panic!("silent shadow must end"))
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            assert!(
                !primary_task.is_finished(),
                "a shadow timeout must not close the primary stream"
            );
            primary_task.abort();
            serving.abort();
        }

        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_wedged_session_does_not_block_stream_end() {
            use futures::StreamExt as _;

            let (manager, id, serving) = probed_session().await;
            let mut stream = Box::pin(manager.create_standalone_stream(&id).await.unwrap());
            let wedge = manager.inner.sessions.write().await;

            let ended = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while stream.next().await.is_some() {}
            })
            .await;
            assert!(
                ended.is_ok(),
                "the stream must end although close is wedged"
            );

            tokio::time::advance(std::time::Duration::from_secs(10)).await;
            drop(wedge);
            serving.abort();
        }

        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_ack_from_another_session_is_ignored_by_manager() {
            let manager = probing_manager(PROBE_STEP, PROBE_STEP * 2);
            let (probed, serving_a) = initialized_session(&manager).await;
            let (other, serving_b) = initialized_session(&manager).await;
            let stream = manager.create_standalone_stream(&probed).await.unwrap();
            let task = spawn_drain(&manager, &other, stream, true);

            tokio::time::timeout(std::time::Duration::from_secs(2), task)
                .await
                .unwrap_or_else(|_| {
                    panic!("a probe answered in another session must still time out")
                })
                .unwrap();
            serving_a.abort();
            serving_b.abort();
        }

        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_probe_replies_are_consumed_and_other_messages_forwarded() {
            let manager = probing_manager(PROBE_STEP, PROBE_STEP * 2);
            let (id, _transport) = manager.create_session().await.unwrap();
            manager.inner.sessions.write().await.remove(&id);
            let message = |json: serde_json::Value| -> ClientJsonRpcMessage {
                serde_json::from_value(json).unwrap()
            };

            let response = message(
                serde_json::json!({"jsonrpc": "2.0", "id": "mcpls-liveness-99", "result": {}}),
            );
            assert!(manager.accept_message(&id, response).await.is_ok());

            let probe_error = message(serde_json::json!({
                "jsonrpc": "2.0", "id": "mcpls-liveness-99",
                "error": {"code": -32601, "message": "no ping"},
            }));
            assert!(manager.accept_message(&id, probe_error).await.is_ok());

            let anonymous_error = message(serde_json::json!({
                "jsonrpc": "2.0", "error": {"code": -32700, "message": "parse"},
            }));
            assert_matches!(
                manager.accept_message(&id, anonymous_error).await,
                Err(CappedSessionManagerError::SessionGone)
            );
            let foreign = message(serde_json::json!({"jsonrpc": "2.0", "id": 5, "result": {}}));
            assert_matches!(
                manager.accept_message(&id, foreign).await,
                Err(CappedSessionManagerError::SessionGone)
            );
        }

        #[test]
        fn test_probe_settings_reject_zero() {
            assert_eq!(ProbeInterval::new(std::time::Duration::ZERO), None);
            assert_eq!(ProbeDeadline::new(std::time::Duration::ZERO), None);
            assert!(ProbeInterval::new(std::time::Duration::from_nanos(1)).is_some());
            assert!(ProbeDeadline::new(std::time::Duration::from_nanos(1)).is_some());
        }

        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_disabled_liveness_never_pings_or_ends_stream() {
            use futures::StreamExt as _;

            let manager = std::sync::Arc::new(CappedSessionManager::new(
                crate::SessionLimit::new(1).unwrap(),
                idle_secs(3600),
            ));
            let (id, serving) = initialized_session(&manager).await;
            let mut stream = Box::pin(manager.create_standalone_stream(&id).await.unwrap());

            let next = tokio::time::timeout(std::time::Duration::from_mins(2), stream.next()).await;
            assert!(next.is_err(), "a disabled stream must stay silent and open");
            serving.abort();
        }

        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_request_wise_resume_is_not_tracked_as_standalone_stream() {
            let (manager, id, serving) = probed_session().await;
            let liveness = manager.session_liveness(&id).unwrap();

            manager.resume(&id, "3/2".to_owned()).await.err();
            assert!(!liveness.has_primary());

            let _common = manager.resume(&id, "0".to_owned()).await.unwrap();
            assert!(liveness.has_primary());
            serving.abort();
        }

        // `manager` lives to the end of the test.
        #[allow(clippy::significant_drop_tightening)]
        #[tokio::test(start_paused = true)]
        async fn test_probing_fails_closed_when_session_slot_is_gone() {
            let (manager, id, serving) = probed_session().await;
            crate::bridge::lock_std(&manager.slots).remove(&id);

            let error = manager.create_standalone_stream(&id).await.err().unwrap();
            assert_matches!(error, CappedSessionManagerError::SessionGone);
            assert!(!error.to_string().contains(&*id), "{error}");
            serving.abort();
        }

        #[test]
        fn test_http_config_stream_liveness_defaults_to_probe_and_is_overridable() {
            let cfg = HttpConfig::new("127.0.0.1:3003".parse().unwrap());
            assert_eq!(cfg.stream_liveness, StreamLiveness::DEFAULT);
            let cfg = cfg.with_stream_liveness(StreamLiveness::Disabled);
            assert_eq!(cfg.stream_liveness, StreamLiveness::Disabled);
        }

        /// A probe reaches the GET stream, a reply sent by POST keeps it open, and a
        /// client that goes silent has the stream closed. Real time with
        /// sub-second settings.
        #[tokio::test]
        async fn test_http_probe_pings_get_stream_keeps_answering_client_and_closes_silent_one() {
            let interval = std::time::Duration::from_millis(200);
            let deadline = std::time::Duration::from_secs(2);
            let (addr, server_task) = spawn_http_server(test_server(), |cfg| {
                cfg.with_stream_liveness(StreamLiveness::Probe {
                    interval: ProbeInterval::new(interval).unwrap(),
                    deadline: ProbeDeadline::new(deadline).unwrap(),
                })
            })
            .await;
            let (session, mut stream) = establish_session(addr).await;

            let mut previous = None;
            for _ in 0..2 {
                let ping = stream.next_ping().await;
                assert_ne!(previous.as_ref(), Some(&ping["id"]));
                let reply = serde_json::json!({"jsonrpc": "2.0", "id": ping["id"], "result": {}});
                let response = post_in_session(addr, &session, &reply.to_string()).await;
                assert!(
                    response.starts_with("HTTP/1.1 202"),
                    "a probe reply must be accepted, got: {response}"
                );
                previous = Some(ping["id"].clone());
            }

            assert!(
                stream
                    .ends_within(interval + deadline + std::time::Duration::from_secs(2))
                    .await,
                "a client that stops answering must have its GET stream closed"
            );
            server_task.abort();
        }

        /// Starts `run_http` with the e2e idle timeout and a session cap of
        /// `max_sessions`.
        async fn spawn_idle_test_server(
            max_sessions: usize,
            server: crate::mcp::McplsServer,
        ) -> (
            SocketAddr,
            tokio::task::JoinHandle<Result<(), crate::Error>>,
        ) {
            let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = probe.local_addr().unwrap();
            drop(probe);
            let mut cfg = HttpConfig::new(addr)
                .with_max_concurrent_sessions(crate::SessionLimit::new(max_sessions).unwrap());
            cfg.session_idle_timeout = IdleTimeout::new(TEST_IDLE).unwrap();
            let task = tokio::spawn(super::super::run_http(
                server,
                cfg,
                super::super::ShutdownSignal::new(),
            ));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            (addr, task)
        }

        fn publish_notification(file: &std::path::Path) -> crate::lsp::LspNotification {
            crate::lsp::LspNotification::PublishDiagnostics(lsp_types::PublishDiagnosticsParams {
                uri: crate::bridge::path_to_uri(file).unwrap(),
                diagnostics: vec![],
                version: None,
            })
        }

        /// #521: a subscribed session whose client vanished must still
        /// expire, even though its files keep producing notifications.
        /// Keeps publishing until a fresh `initialize` finds the sole slot
        /// free, since releasing the dead stream needs a failed write.
        #[tokio::test]
        async fn test_abandoned_subscribed_session_expires_despite_notifications() {
            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let file = root.join("main.rs");
            std::fs::write(&file, "fn main() {}").unwrap();
            let uri = crate::bridge::resources::make_uri(&file).unwrap();

            let server = test_server_with_roots(
                WorkspaceRoots::from_configured(std::slice::from_ref(&root)).unwrap(),
            );
            let registry = server.subscription_registry();
            let (addr, server_task) = spawn_idle_test_server(1, server).await;

            let (session, stream) = establish_session(addr).await;
            subscribe_in_session(addr, &session, &uri).await;
            drop(stream);

            let (tx, _cancel_tx) = crate::test_lsp::spawn_test_pump(
                registry,
                WorkspaceRoots::from_configured(&[root]).unwrap(),
            );
            let accept_headers =
                "Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n";
            let initialize = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
            let deadline = tokio::time::Instant::now() + E2E_DEADLINE;
            loop {
                tx.send(publish_notification(&file)).await.unwrap();
                let response = raw_http_post(addr, "/mcp", accept_headers, initialize).await;
                if response.starts_with("HTTP/1.1 200") {
                    break;
                }
                assert!(
                    response.starts_with("HTTP/1.1 429"),
                    "unexpected response while waiting for expiry: {response}"
                );
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "abandoned session still holds its slot after {E2E_DEADLINE:?}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            server_task.abort();
        }

        /// #521 non-regression: a healthy session that only listens on its
        /// GET stream (no POSTs) is never reaped, and keeps receiving updates.
        #[tokio::test]
        async fn test_healthy_get_only_listener_is_not_reaped() {
            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let file = root.join("main.rs");
            std::fs::write(&file, "fn main() {}").unwrap();
            let uri = crate::bridge::resources::make_uri(&file).unwrap();

            let server = test_server_with_roots(
                WorkspaceRoots::from_configured(std::slice::from_ref(&root)).unwrap(),
            );
            let registry = server.subscription_registry();
            let (addr, server_task) = spawn_idle_test_server(1, server).await;

            let (session, mut stream) = establish_session(addr).await;
            subscribe_in_session(addr, &session, &uri).await;
            let (tx, _cancel_tx) = crate::test_lsp::spawn_test_pump(
                registry,
                WorkspaceRoots::from_configured(&[root]).unwrap(),
            );

            let started = tokio::time::Instant::now();
            while started.elapsed() < TEST_IDLE * 3 {
                tx.send(publish_notification(&file)).await.unwrap();
                assert_eq!(stream.next_resource_update().await, uri);
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }

            let ping = post_in_session(
                addr,
                &session,
                r#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#,
            )
            .await;
            assert!(
                ping.starts_with("HTTP/1.1 200"),
                "session with an open GET stream must survive 3x the idle timeout, got: {ping}"
            );

            server_task.abort();
        }

        const SUBSCRIPTION_ID_KEY: &str = "io.modelcontextprotocol/subscriptionId";

        /// Binds `run_http` over a fresh workspace holding `main.rs` and returns
        /// what the listen tests need.
        struct ListenFixture {
            addr: SocketAddr,
            registry: crate::mcp::SubscriptionRegistry,
            root: std::path::PathBuf,
            file: std::path::PathBuf,
            uri: String,
            server_task: tokio::task::JoinHandle<Result<(), crate::Error>>,
            _workspace: tempfile::TempDir,
        }

        async fn spawn_listen_fixture() -> ListenFixture {
            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let file = root.join("main.rs");
            std::fs::write(&file, "fn main() {}").unwrap();
            let uri = crate::bridge::resources::make_uri(&file).unwrap();
            let server = test_server_with_roots(
                WorkspaceRoots::from_configured(std::slice::from_ref(&root)).unwrap(),
            );
            let registry = server.subscription_registry();
            let (addr, server_task) = spawn_idle_test_server(1, server).await;
            ListenFixture {
                addr,
                registry,
                root,
                file,
                uri,
                server_task,
                _workspace: workspace,
            }
        }

        /// Polls until `registry` holds no live session, or fails after the
        /// e2e deadline.
        async fn assert_registry_empties(registry: &crate::mcp::SubscriptionRegistry) {
            let deadline = tokio::time::Instant::now() + E2E_DEADLINE;
            while !registry.live_sessions().is_empty() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "listen registration outlived its connection"
                );
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }

        /// #522: a stateless `subscriptions/listen` stream acknowledges only
        /// syntactically valid URIs and then delivers one `resources/updated`
        /// per raw spelling of a published file, tagged with the subscription id.
        #[tokio::test]
        async fn test_http_listen_delivers_one_update_per_raw_uri() {
            let fx = spawn_listen_fixture().await;
            let alias = fx.uri.replace("main.rs", "%6Dain.rs");
            let other = tempfile::TempDir::new().unwrap();
            let outside = crate::bridge::resources::make_uri(
                &dunce::canonicalize(other.path()).unwrap().join("x.rs"),
            )
            .unwrap();
            let requested = [
                &fx.uri,
                &alias,
                &outside,
                &"file:///not-a-diagnostics-uri".to_owned(),
            ];
            let mut stream = SseStream::open_listen(
                fx.addr,
                &serde_json::json!({"resourceSubscriptions": requested}),
            )
            .await;

            let ack = loop {
                let message = stream.next_message().await;
                if message["method"] == "notifications/subscriptions/acknowledged" {
                    break message;
                }
            };
            assert_eq!(
                ack["params"]["notifications"]["resourceSubscriptions"],
                serde_json::json!([fx.uri, alias, outside])
            );

            // The acknowledgment precedes registration; a publish before it is
            // only recovered by cache replay, which this pump's own cache lacks.
            let deadline = tokio::time::Instant::now() + E2E_DEADLINE;
            while fx.registry.live_sessions().is_empty() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "listen never registered"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let (tx, _cancel_tx) = crate::test_lsp::spawn_test_pump(
                fx.registry.clone(),
                WorkspaceRoots::from_configured(std::slice::from_ref(&fx.root)).unwrap(),
            );
            tx.send(publish_notification(&fx.file)).await.unwrap();

            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..2 {
                let update = stream.next_resource_update_message().await;
                assert_eq!(update["params"]["_meta"][SUBSCRIPTION_ID_KEY], 7);
                seen.insert(update["params"]["uri"].as_str().unwrap().to_owned());
            }
            assert_eq!(seen, [fx.uri.clone(), alias].into_iter().collect());

            drop(stream);
            assert_registry_empties(&fx.registry).await;
            fx.server_task.abort();
        }

        /// #522: once all listen slots are taken, the next listen is
        /// acknowledged and then fails with the retryable `-32053`.
        #[tokio::test]
        async fn test_http_listen_beyond_slot_limit_fails_with_retryable_error() {
            let fx = spawn_listen_fixture().await;
            let notifications = serde_json::json!({"resourceSubscriptions": [fx.uri]});
            let mut streams = Vec::new();
            for _ in 0..crate::bridge::resources::MAX_LISTEN_STREAMS {
                streams.push(SseStream::open_listen(fx.addr, &notifications).await);
            }
            let deadline = tokio::time::Instant::now() + E2E_DEADLINE;
            while fx.registry.live_sessions().len() < streams.len() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "listen streams never all registered"
                );
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }

            let mut refused = SseStream::open_listen(fx.addr, &notifications).await;
            let error = loop {
                let message = refused.next_message().await;
                if message["error"].is_object() {
                    break message["error"].clone();
                }
            };
            assert_eq!(
                error["code"],
                crate::error::LISTEN_STREAMS_EXHAUSTED_ERROR_CODE
            );
            assert_eq!(
                error["data"]["max_listen_streams"],
                crate::bridge::resources::MAX_LISTEN_STREAMS
            );

            drop(streams);
            assert_registry_empties(&fx.registry).await;
            fx.server_task.abort();
        }

        /// #522: diagnostics already cached when the stream opens are replayed
        /// right after the acknowledgment.
        #[tokio::test]
        async fn test_http_listen_replays_cached_diagnostics() {
            let (_workspace, root, file) = crate::test_lsp::workspace_with_main_rs();
            let uri = crate::bridge::resources::make_uri(&file).unwrap();
            let cache = std::sync::Arc::new(tokio::sync::Mutex::new(
                crate::bridge::NotificationCache::new(),
            ));
            cache.lock().await.store_diagnostics(
                &crate::config::ServerId::from("rust"),
                &crate::bridge::path_to_uri(&file).unwrap(),
                None,
                vec![],
            );
            let server = crate::mcp::McplsServer::new(
                std::sync::Arc::new(crate::bridge::Translator::new()),
                cache,
                WorkspaceRoots::from_configured(&[root]).unwrap(),
                crate::mcp::SubscriptionRegistry::new(),
                crate::ProjectConfigStatus::NotIgnored,
                crate::config::McpConfig::default(),
            );
            let registry = server.subscription_registry();
            let (addr, server_task) = spawn_idle_test_server(1, server).await;

            let mut stream =
                SseStream::open_listen(addr, &serde_json::json!({"resourceSubscriptions": [uri]}))
                    .await;
            assert_eq!(stream.next_resource_update().await, uri);

            drop(stream);
            assert_registry_empties(&registry).await;
            server_task.abort();
        }

        /// #522: a listen whose URIs all fall outside the workspace is
        /// acknowledged and then fails with invalid-params, holding no slot.
        #[tokio::test]
        async fn test_http_listen_with_only_unresolvable_uris_fails() {
            let fx = spawn_listen_fixture().await;
            let other = tempfile::TempDir::new().unwrap();
            let outside = crate::bridge::resources::make_uri(
                &dunce::canonicalize(other.path()).unwrap().join("x.rs"),
            )
            .unwrap();
            let mut stream = SseStream::open_listen(
                fx.addr,
                &serde_json::json!({"resourceSubscriptions": [outside]}),
            )
            .await;

            let error = loop {
                let message = stream.next_message().await;
                if message["error"].is_object() {
                    break message["error"].clone();
                }
            };
            assert_eq!(error["code"], -32602);
            assert!(fx.registry.live_sessions().is_empty());
            fx.server_task.abort();
        }

        /// #522: more URIs than a stream may watch yields an empty
        /// acknowledgment followed by an invalid-params error.
        #[tokio::test]
        async fn test_http_listen_oversized_request_is_rejected() {
            let fx = spawn_listen_fixture().await;
            let uris: Vec<String> = (0..=crate::bridge::resources::MAX_SUBSCRIPTIONS)
                .map(|i| format!("lsp-diagnostics:///f{i}.rs"))
                .collect();
            let mut stream = SseStream::open_listen(
                fx.addr,
                &serde_json::json!({"resourceSubscriptions": uris}),
            )
            .await;

            let mut acknowledged = false;
            let error = loop {
                let message = stream.next_message().await;
                if message["method"] == "notifications/subscriptions/acknowledged" {
                    assert!(
                        message["params"]["notifications"]["resourceSubscriptions"].is_null(),
                        "oversized request must not echo any URI: {message}"
                    );
                    acknowledged = true;
                }
                if message["error"].is_object() {
                    break message["error"].clone();
                }
            };
            assert!(acknowledged);
            assert_eq!(error["code"], -32602);
            fx.server_task.abort();
        }

        fn short_lease(ms: u64) -> super::super::ListenLease {
            let d = std::time::Duration::from_millis(ms);
            super::super::ListenLease::Renew(super::super::LeaseWindow::new(d, d).unwrap())
        }

        #[test]
        fn test_listen_lease_follows_stream_liveness_unless_overridden() {
            use super::super::{LeaseWindow, ListenLease};

            let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap());
            assert_eq!(
                cfg.effective_listen_lease(),
                ListenLease::Renew(LeaseWindow::DEFAULT)
            );
            let off = cfg.with_stream_liveness(StreamLiveness::Disabled);
            assert_eq!(off.effective_listen_lease(), ListenLease::Unbounded);
            let forced = off.with_listen_lease(short_lease(5));
            assert_eq!(forced.effective_listen_lease(), short_lease(5));
        }

        /// #551: a listen stream ends abruptly after its lease -- no final
        /// result and no SSE event id, so a client reads it as a transport
        /// close and listens again -- freeing its slot, and a re-listen works.
        #[tokio::test]
        async fn test_http_listen_lease_ends_stream_abruptly_and_frees_slot() {
            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
            let uri = crate::bridge::resources::make_uri(&root.join("main.rs")).unwrap();
            let server = test_server_with_roots(WorkspaceRoots::from_configured(&[root]).unwrap());
            let registry = server.subscription_registry();
            let (addr, server_task) =
                spawn_http_server(server, |cfg| cfg.with_listen_lease(short_lease(300))).await;
            let notifications = serde_json::json!({"resourceSubscriptions": [uri]});

            let mut stream = SseStream::open_listen(addr, &notifications).await;
            assert!(
                stream.ends_within(std::time::Duration::from_secs(10)).await,
                "the lease must end the stream"
            );
            assert!(
                !stream.buf.contains("\"result\""),
                "a lease end must not carry a final result: {}",
                stream.buf
            );
            assert!(
                !stream.buf.lines().any(|line| line.starts_with("id:")),
                "stateless SSE must not emit event ids: {}",
                stream.buf
            );
            assert_registry_empties(&registry).await;

            let mut again = SseStream::open_listen(addr, &notifications).await;
            assert!(
                again.ends_within(std::time::Duration::from_secs(10)).await,
                "a re-listen gets a fresh lease"
            );
            server_task.abort();
        }

        /// #551: with the lease off (`--http-stream-liveness off` resolves to
        /// this) a listen stream is not ended on a timer.
        #[tokio::test]
        async fn test_http_listen_without_lease_is_not_ended() {
            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
            let uri = crate::bridge::resources::make_uri(&root.join("main.rs")).unwrap();
            let server = test_server_with_roots(WorkspaceRoots::from_configured(&[root]).unwrap());
            let (addr, server_task) = spawn_http_server(server, |cfg| {
                cfg.with_stream_liveness(StreamLiveness::Disabled)
            })
            .await;
            let mut stream =
                SseStream::open_listen(addr, &serde_json::json!({"resourceSubscriptions": [uri]}))
                    .await;
            assert!(
                !stream
                    .ends_within(std::time::Duration::from_millis(1500))
                    .await
            );
            server_task.abort();
        }

        /// #551 with a real rmcp client at its default 64-slot subscription
        /// buffer: 200 subscribed URIs of which 10 are cached and one had its
        /// clear evicted. Each listen replays only those 11 (never all 200, which
        /// would overflow the buffer), ends abruptly at the lease, and the
        /// re-listen replays them again.
        #[tokio::test]
        async fn test_http_listen_lease_with_real_client_replays_cache_and_evictions() {
            use rmcp::ClientServiceExt as _;
            use rmcp::model::{
                ClientConfig, ProtocolVersion, ServerNotification, SubscriptionFilter,
            };
            use rmcp::service::SubscriptionEnd;
            use rmcp::transport::StreamableHttpClientTransport;
            use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

            const REQUESTED: usize = 200;
            const CACHED: usize = 10;
            let server_id = crate::config::ServerId::from("rust");
            let workspace = tempfile::TempDir::new().unwrap();
            let root = dunce::canonicalize(workspace.path()).unwrap();
            let make = |name: &str| {
                let path = root.join(name);
                std::fs::write(&path, "").unwrap();
                (
                    crate::bridge::path_to_uri(&path).unwrap(),
                    crate::bridge::resources::make_uri(&path).unwrap(),
                )
            };
            let mut cache = crate::bridge::NotificationCache::new();
            let (evicted_lsp, evicted_uri) = make("evicted.rs");
            cache.store_diagnostics(&server_id, &evicted_lsp, None, vec![]);
            let mut expected = std::collections::BTreeSet::from([evicted_uri.clone()]);
            let mut requested = vec![evicted_uri];
            for i in 0..REQUESTED - 1 {
                let (lsp, uri) = make(&format!("f{i}.rs"));
                if i < CACHED {
                    let diagnostic = lsp_types::Diagnostic {
                        message: "broken".to_owned().into(),
                        ..lsp_types::Diagnostic::default()
                    };
                    cache.store_diagnostics(&server_id, &lsp, None, vec![diagnostic]);
                    expected.insert(uri.clone());
                }
                requested.push(uri);
            }
            let padding = lsp_types::Diagnostic {
                message: "filler".to_owned().into(),
                ..lsp_types::Diagnostic::default()
            };
            let mut fillers = 0;
            while cache.has_diagnostics(&evicted_lsp) {
                let uri = lsp_types::Uri::from(format!("file:///filler{fillers}.rs"));
                cache.store_diagnostics(&server_id, &uri, None, vec![padding.clone()]);
                fillers += 1;
            }
            assert_eq!(expected.len(), CACHED + 1);

            let server = crate::mcp::McplsServer::new(
                std::sync::Arc::new(crate::bridge::Translator::new()),
                std::sync::Arc::new(tokio::sync::Mutex::new(cache)),
                WorkspaceRoots::from_configured(&[root]).unwrap(),
                crate::mcp::SubscriptionRegistry::new(),
                crate::ProjectConfigStatus::NotIgnored,
                crate::config::McpConfig::default(),
            );
            let (addr, server_task) =
                spawn_http_server(server, |cfg| cfg.with_listen_lease(short_lease(500))).await;

            let transport = StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp")),
            );
            let client = ClientConfig::default()
                .serve_with_lifecycle(
                    transport,
                    rmcp::ClientLifecycleMode::Discover {
                        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                    },
                )
                .await
                .unwrap();
            let mut filter = SubscriptionFilter::new();
            filter.resource_subscriptions = Some(requested);

            for round in 0..2 {
                let mut subscription = client.listen(filter.clone()).await.unwrap();
                let mut seen = std::collections::BTreeSet::new();
                while let Some(notification) =
                    tokio::time::timeout(std::time::Duration::from_secs(10), subscription.next())
                        .await
                        .unwrap()
                        .unwrap()
                {
                    if let ServerNotification::ResourceUpdatedNotification(update) = notification {
                        seen.insert(update.params.uri);
                    }
                }
                assert_eq!(seen, expected, "round {round} replay");
                assert!(
                    matches!(subscription.end(), Some(SubscriptionEnd::Abrupt)),
                    "round {round} must end abruptly, got {:?}",
                    subscription.end()
                );
            }

            client.cancel().await.unwrap();
            server_task.abort();
        }

        /// #233: binding to a non-loopback address must log a warning that
        /// tells operators to put the endpoint behind a reverse proxy that
        /// *enforces* authentication (not the inverted "ensure no
        /// authentication is required" wording it replaced).
        #[tokio::test]
        async fn test_run_http_non_loopback_bind_warns_to_use_reverse_proxy() {
            use tracing_subscriber::layer::SubscriberExt as _;

            let addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
            let cfg = HttpConfig::new(addr);

            let captured = CapturedLogs::default();
            let subscriber = tracing_subscriber::registry().with(captured.clone());
            let guard = tracing::subscriber::set_default(subscriber);

            // The warning fires synchronously right after bind, before
            // `axum::serve` starts running indefinitely, so a short timeout
            // is enough to observe it.
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                super::super::run_http(test_server(), cfg, super::super::ShutdownSignal::new()),
            )
            .await;

            drop(guard);

            let messages = captured.messages();
            assert!(
                messages.iter().any(|m| m.contains(
                    "place this endpoint behind a reverse proxy that enforces authentication"
                )),
                "expected reverse-proxy warning in captured tracing events, got: {messages:?}"
            );
        }
    }
}
