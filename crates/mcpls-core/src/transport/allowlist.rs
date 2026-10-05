//! `Host` and `Origin` allowlist types for the HTTP transport's DNS-rebinding
//! protection.

/// Why a string is not a valid [`HttpPath`].
///
/// # Examples
///
/// ```
/// use mcpls_core::{HttpPath, InvalidHttpPath};
///
/// assert_eq!("mcp".parse::<HttpPath>(), Err(InvalidHttpPath::MissingLeadingSlash));
/// ```
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpPath(Box<str>);

impl HttpPath {
    /// The validated path, starting with `/`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for HttpPath {
    fn default() -> Self {
        Self("/mcp".into())
    }
}

impl std::fmt::Display for HttpPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OriginScheme {
    Http,
    Https,
}

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedOrigin {
    scheme: OriginScheme,
    host: Box<str>,
    port: u16,
}

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

impl std::fmt::Display for AllowedOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}:{}", self.scheme.as_str(), self.host, self.port)
    }
}

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthorityPort {
    /// No `:` after the host.
    Absent,
    /// A `:` with no digits after it.
    Empty,
    /// A decimal port that fits in `u16`.
    Number(u16),
}

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PinnedPort(std::num::NonZeroU16);

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedHost {
    host: Box<str>,
    port: Option<PinnedPort>,
}

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

impl std::fmt::Display for AllowedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.port() {
            Some(port) => write!(f, "{}:{port}", self.host),
            None => f.write_str(&self.host),
        }
    }
}

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

/// The `Host` values the server accepts: the loopback names, the bound IP
/// literal when it is neither unspecified nor loopback, and `configured`.
///
/// The bound IP literal is safe to allow because DNS rebinding needs a
/// hostname, and it is allowed on any port because a client reaching a `:80`
/// or `:443` bind omits the port from `Host`.
pub(super) fn effective_allowed_hosts(
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

#[cfg(test)]
mod tests {
    use super::*;

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
            effective_allowed_hosts(bind.parse().unwrap(), &configured)
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
}
