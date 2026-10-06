//! HTTP transport configuration: `HttpConfig`, its limit newtypes and
//! stream-liveness policy.

use super::allowlist::{AllowedHost, AllowedOrigin, HttpPath};
use super::lease::{LeaseWindow, ListenLease};
use super::session_manager::IdleTimeout;

/// Configuration for the HTTP transport.
///
/// Passed inside [`Transport::Http`](crate::Transport::Http) to control the TCP bind address and the
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
    pub session_idle_timeout: IdleTimeout,
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

    /// Override how long a session may go without client activity before it
    /// is closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use mcpls_core::HttpConfig;
    /// use mcpls_core::transport::IdleTimeout;
    ///
    /// let cfg = HttpConfig::new("127.0.0.1:3000".parse().unwrap())
    ///     .with_session_idle_timeout(IdleTimeout::new(Duration::from_mins(10)).unwrap());
    /// assert_eq!(cfg.session_idle_timeout.get(), Duration::from_mins(10));
    /// ```
    #[must_use]
    pub const fn with_session_idle_timeout(mut self, timeout: IdleTimeout) -> Self {
        self.session_idle_timeout = timeout;
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

macro_rules! non_zero_duration {
    ($(#[$meta:meta])* $vis:vis $name:ident, $default:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        $vis struct $name(std::time::Duration);

        impl $name {
            /// The default duration.
            $vis const DEFAULT: Self = match Self::new($default) {
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

pub(super) use non_zero_duration;

macro_rules! non_zero_limit {
    (
        $(#[$meta:meta])* $name:ident,
        $default:expr, $default_doc:literal,
        $max:expr, $max_doc:literal
    ) => {
        $(#[$meta])*
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
    pub ProbeInterval, std::time::Duration::from_mins(1)
}

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
    pub ProbeDeadline, std::time::Duration::from_secs(30)
}

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
    pub ResponseStreamDeadline, std::time::Duration::from_hours(1)
}

const _: () = assert!(
    ResponseStreamDeadline::DEFAULT.get().as_secs()
        >= 2 * crate::config::MAX_TIMEOUT_SECONDS
            + crate::bridge::INDEXING_STALENESS_BOUND.as_secs()
            + 300,
    "the default response stream deadline must outlast the longest single tool call"
);

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

impl StreamLiveness {
    /// Probe every 60 seconds with a 30 second answer deadline.
    pub const DEFAULT: Self = Self::Probe {
        interval: ProbeInterval::DEFAULT,
        deadline: ProbeDeadline::DEFAULT,
    };
}

impl Default for StreamLiveness {
    fn default() -> Self {
        Self::DEFAULT
    }
}

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
    pub HeaderReadTimeout, std::time::Duration::from_secs(30)
}

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
    pub WriteStallTimeout, std::time::Duration::from_secs(30)
}

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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;
    use crate::transport::test_support::short_lease;

    #[test]
    fn test_http_config_fields() {
        let addr: SocketAddr = "127.0.0.1:3000".parse().unwrap();
        let cfg = HttpConfig::new(addr);
        assert_eq!(cfg.bind, addr);
        assert_eq!(cfg.path, crate::HttpPath::default());
    }

    #[test]
    fn test_http_config_clone() {
        let cfg =
            HttpConfig::new("127.0.0.1:3001".parse().unwrap()).with_path("/test".parse().unwrap());
        let cloned = cfg.clone();
        assert_eq!(cloned.bind, cfg.bind);
        assert_eq!(cloned.path, cfg.path);
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

    #[test]
    fn test_probe_settings_reject_zero() {
        assert_eq!(ProbeInterval::new(std::time::Duration::ZERO), None);
        assert_eq!(ProbeDeadline::new(std::time::Duration::ZERO), None);
        assert!(ProbeInterval::new(std::time::Duration::from_nanos(1)).is_some());
        assert!(ProbeDeadline::new(std::time::Duration::from_nanos(1)).is_some());
    }

    #[test]
    fn test_http_config_stream_liveness_defaults_to_probe_and_is_overridable() {
        let cfg = HttpConfig::new("127.0.0.1:3003".parse().unwrap());
        assert_eq!(cfg.stream_liveness, StreamLiveness::DEFAULT);
        let cfg = cfg.with_stream_liveness(StreamLiveness::Disabled);
        assert_eq!(cfg.stream_liveness, StreamLiveness::Disabled);
    }

    #[test]
    fn test_listen_lease_follows_stream_liveness_unless_overridden() {
        use crate::transport::{LeaseWindow, ListenLease};

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
}
