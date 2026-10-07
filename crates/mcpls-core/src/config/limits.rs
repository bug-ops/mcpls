//! Bounded numeric limits for configuration values.
//!
//! A value of these types is in range by construction, so neither
//! `ServerConfig::validate` nor the use sites need to re-check or clamp it.

use std::num::{NonZeroU64, NonZeroUsize};

use serde::{Deserialize, Serialize};

use super::bounded_number::impl_bounded_number;
use super::text_newtype::impl_text_newtype;

/// Default max depth for recursive marker search.
pub const DEFAULT_HEURISTICS_MAX_DEPTH: usize = 10;

/// Maximum allowed value, in seconds, for both [`super::LspServerConfig::timeout_seconds`]
/// and [`super::LspServerConfig::request_timeout_seconds`], enforced by [`super::TimeoutSecs`].
///
/// tokio's `timeout`/`sleep` fall back to `Instant::far_future()` for
/// astronomically large durations instead of panicking, so an unbounded value
/// on either field (misconfiguration or typo) would silently disable the
/// timeout rather than fail with a diagnosable error.
///
/// Set to 900 (15 minutes), not a rounder 3600 (1 hour): [`LspClient::request`]
/// retries a request up to 4 times total on a `-32802` (`ServerCancelled`) or
/// `-32801` (`ContentModified`) response (one shared budget across both
/// codes), so the worst-case latency for a single call bounded by this value
/// is `4 * 900 + 3.5s` ≈ 1 hour, not 4 hours — this constant bounds one
/// attempt, so it is chosen such that the actually-experienced worst case
/// (the retried total) stays within about an hour.
///
/// [`LspClient::request`]: crate::lsp::LspClient::request
pub const MAX_TIMEOUT_SECONDS: u64 = 900;

/// Upper bound on `workspace.heuristics_max_depth`.
///
/// A guard against typos and misconfiguration (e.g. `999999`), not a bound on
/// walk cost: the recursive project-marker walk in
/// `MarkerScan::collect` does not follow links, so
/// its cost is bounded by the size of the tree regardless of this value.
/// 64 is several times the default of 10 and well beyond any realistic
/// project nesting.
pub const MAX_HEURISTICS_DEPTH: usize = 64;

/// Why a value is not a valid [`ServerStartConcurrency`].
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("max_concurrent_server_starts must be at least 1")]
pub struct InvalidServerStartConcurrency;

/// How many LSP servers may be starting at the same time.
///
/// A fixed default keeps a generated configuration machine-independent.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ServerStartConcurrency;
///
/// assert!(ServerStartConcurrency::new(0).is_err());
/// assert_eq!(ServerStartConcurrency::new(2).unwrap().get(), 2);
/// assert_eq!(ServerStartConcurrency::default(), ServerStartConcurrency::DEFAULT);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "usize", into = "usize")]
pub struct ServerStartConcurrency(NonZeroUsize);

impl ServerStartConcurrency {
    /// Builds a limit.
    ///
    /// # Errors
    ///
    /// [`InvalidServerStartConcurrency`] for zero.
    pub const fn new(limit: usize) -> Result<Self, InvalidServerStartConcurrency> {
        match NonZeroUsize::new(limit) {
            Some(limit) => Ok(Self(limit)),
            None => Err(InvalidServerStartConcurrency),
        }
    }

    /// The wrapped limit, at least 1.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl_bounded_number!(
    ServerStartConcurrency,
    usize,
    InvalidServerStartConcurrency,
    default = 8,
    "Eight servers at a time.",
    into = ServerStartConcurrency::get
);

/// Why a number is not a valid [`SearchDepth`].
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("heuristics_max_depth ({value}) exceeds the maximum of {max}")]
pub struct InvalidSearchDepth {
    /// The rejected value.
    pub value: usize,
    /// Largest accepted value.
    pub max: usize,
}

/// How many directory levels the recursive project-marker walk descends:
/// at most [`MAX_HEURISTICS_DEPTH`].
///
/// Deserializes from a TOML integer and rejects an out-of-range value at load
/// time.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::SearchDepth;
///
/// assert_eq!(SearchDepth::default().get(), 10);
/// assert_eq!(SearchDepth::new(64).unwrap().get(), 64);
/// assert!(SearchDepth::new(65).is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "usize", into = "usize")]
pub struct SearchDepth(u8);

impl SearchDepth {
    /// Builds a depth.
    ///
    /// # Errors
    ///
    /// [`InvalidSearchDepth`] when `depth` exceeds [`MAX_HEURISTICS_DEPTH`].
    pub const fn new(depth: usize) -> Result<Self, InvalidSearchDepth> {
        const { assert!(MAX_HEURISTICS_DEPTH <= u8::MAX as usize) };
        if depth > MAX_HEURISTICS_DEPTH {
            return Err(InvalidSearchDepth {
                value: depth,
                max: MAX_HEURISTICS_DEPTH,
            });
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "bounded by MAX_HEURISTICS_DEPTH, asserted to fit u8 above"
        )]
        Ok(Self(depth as u8))
    }

    /// The wrapped depth, at most [`MAX_HEURISTICS_DEPTH`].
    #[must_use]
    pub const fn get(self) -> usize {
        self.0 as usize
    }
}

impl_bounded_number!(
    SearchDepth,
    usize,
    InvalidSearchDepth,
    default = DEFAULT_HEURISTICS_MAX_DEPTH,
    "The built-in default, [`DEFAULT_HEURISTICS_MAX_DEPTH`].",
    into = SearchDepth::get
);

/// Upper bound for `workspace.max_file_size` (1 GiB).
///
/// Keeps the derived per-response disk-read budget (a multiple of it) far from
/// overflow, and a misconfiguration from making every response a
/// gigabyte-scale scan.
pub const MAX_FILE_SIZE_LIMIT: u64 = 1 << 30;

/// Why a number is not a valid [`SizeLimit`].
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error(
    "max_file_size ({value}) exceeds the hard cap of {max} bytes; use a lower value, or 0 to \
     disable the per-file limit"
)]
pub struct InvalidSizeLimit {
    /// The rejected value.
    pub value: u64,
    /// Largest accepted value.
    pub max: u64,
}

/// A byte limit that is either absent or a positive number of bytes up to
/// [`MAX_FILE_SIZE_LIMIT`].
///
/// The TOML spelling of "unlimited" is `0`; it exists only at that edge, so no
/// consumer re-checks for it. Deserializes from an integer and rejects an
/// out-of-range value at load time.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::SizeLimit;
///
/// let limit = SizeLimit::new(100).unwrap();
/// assert!(limit.admits(100));
/// assert!(!limit.admits(101));
/// assert!(SizeLimit::UNLIMITED.admits(u64::MAX));
/// assert_eq!(SizeLimit::new(0).unwrap(), SizeLimit::UNLIMITED);
/// assert!(SizeLimit::new(u64::MAX).is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct SizeLimit(Option<NonZeroU64>);

impl SizeLimit {
    /// No limit.
    pub const UNLIMITED: Self = Self(None);

    /// A limit of `bytes`, where `0` means unlimited.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidSizeLimit`] above [`MAX_FILE_SIZE_LIMIT`].
    pub const fn new(bytes: u64) -> Result<Self, InvalidSizeLimit> {
        if bytes > MAX_FILE_SIZE_LIMIT {
            return Err(InvalidSizeLimit {
                value: bytes,
                max: MAX_FILE_SIZE_LIMIT,
            });
        }
        Ok(Self(NonZeroU64::new(bytes)))
    }

    /// A limit from a literal, checked at compile time when evaluated in a
    /// `const` context.
    ///
    /// # Panics
    ///
    /// Panics if `bytes` is above [`MAX_FILE_SIZE_LIMIT`].
    #[must_use]
    pub const fn from_static(bytes: u64) -> Self {
        match Self::new(bytes) {
            Ok(limit) => limit,
            Err(_) => panic!("size limit above MAX_FILE_SIZE_LIMIT"),
        }
    }

    /// The limit in bytes, or `None` when unlimited.
    #[must_use]
    pub const fn get(self) -> Option<NonZeroU64> {
        self.0
    }

    /// Whether a file of `bytes` is within the limit.
    #[must_use]
    pub const fn admits(self, bytes: u64) -> bool {
        match self.0 {
            Some(max) => bytes <= max.get(),
            None => true,
        }
    }

    /// Byte cap for a bounded read: one more than the limit, so a read that
    /// reaches the cap is known to have exceeded it, or `u64::MAX` when
    /// unlimited.
    #[must_use]
    pub const fn read_cap(self) -> u64 {
        match self.0 {
            Some(max) => Self::read_cap_for(max),
            None => u64::MAX,
        }
    }

    /// [`Self::read_cap`] of a limit of `max` bytes.
    #[must_use]
    pub(crate) const fn read_cap_for(max: NonZeroU64) -> u64 {
        max.get().saturating_add(1)
    }
}

impl_bounded_number!(
    SizeLimit,
    u64,
    InvalidSizeLimit,
    default = 10 * 1024 * 1024,
    "The built-in default, 10 MiB.",
    into = |limit: SizeLimit| limit.0.map_or(0, NonZeroU64::get)
);

/// A count limit that is either absent or a positive number of documents.
///
/// The TOML spelling of "unlimited" is `0`; it exists only at that edge.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::DocumentLimit;
///
/// assert_eq!(DocumentLimit::default().get().unwrap().get(), 100);
/// assert_eq!(DocumentLimit::new(0), DocumentLimit::UNLIMITED);
/// assert!(DocumentLimit::UNLIMITED.get().is_none());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "usize", into = "usize")]
pub struct DocumentLimit(Option<NonZeroUsize>);

impl DocumentLimit {
    /// No limit.
    pub const UNLIMITED: Self = Self(None);

    /// The built-in default, 100 documents.
    pub const DEFAULT: Self = Self::new(100);

    /// A limit of `count` documents, where `0` means unlimited.
    #[must_use]
    pub const fn new(count: usize) -> Self {
        Self(NonZeroUsize::new(count))
    }

    /// The limit, or `None` when unlimited.
    #[must_use]
    pub const fn get(self) -> Option<NonZeroUsize> {
        self.0
    }
}

impl Default for DocumentLimit {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl From<usize> for DocumentLimit {
    fn from(count: usize) -> Self {
        Self::new(count)
    }
}

impl From<DocumentLimit> for usize {
    fn from(limit: DocumentLimit) -> Self {
        limit.0.map_or(0, NonZeroUsize::get)
    }
}

/// Why a string is not a valid [`BoundedText`].
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidBoundedText {
    /// The text was empty or whitespace-only.
    #[error("cannot be empty")]
    Blank,
    /// The text was longer than the byte cap.
    #[error("exceeds the maximum of {max} bytes ({len} given)")]
    TooLong {
        /// The byte length of the rejected text.
        len: usize,
        /// The byte cap.
        max: usize,
    },
}

/// Non-blank text of at most `MAX` UTF-8 bytes.
///
/// Deserializes from a TOML string and rejects a blank or over-long value at
/// load time. The cap counts bytes, not chars.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::BoundedText;
///
/// let text = BoundedText::<8>::new("hello").unwrap();
/// assert_eq!(text.as_str(), "hello");
/// assert!(BoundedText::<8>::new("  ").is_err());
/// assert!(BoundedText::<8>::new("123456789").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BoundedText<const MAX: usize>(String);

impl<const MAX: usize> BoundedText<MAX> {
    /// Builds the text from any string.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidBoundedText::Blank`] before [`InvalidBoundedText::TooLong`],
    /// so a whitespace-only value reports as blank.
    pub fn new(text: impl Into<String>) -> Result<Self, InvalidBoundedText> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(InvalidBoundedText::Blank);
        }
        let len = text.len();
        if len > MAX {
            return Err(InvalidBoundedText::TooLong { len, max: MAX });
        }
        Ok(Self(text))
    }
}

impl_text_newtype!(@impl [const MAX: usize] BoundedText<MAX>, InvalidBoundedText);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bounded_text_blank_is_reported_before_length() {
        assert_eq!(BoundedText::<4>::new(""), Err(InvalidBoundedText::Blank));
        assert_eq!(
            BoundedText::<4>::new("      "),
            Err(InvalidBoundedText::Blank)
        );
    }

    #[test]
    fn test_bounded_text_counts_bytes_not_chars() {
        assert!(BoundedText::<4>::new("éé").is_ok());
        assert_eq!(
            BoundedText::<4>::new("ééé"),
            Err(InvalidBoundedText::TooLong { len: 6, max: 4 })
        );
        assert_eq!(
            BoundedText::<4>::new("ééé").unwrap_err().to_string(),
            "exceeds the maximum of 4 bytes (6 given)"
        );
    }

    #[test]
    fn test_bounded_text_has_the_shared_text_conversions() {
        let text: BoundedText<8> = "hello".parse().unwrap();
        assert_eq!(text.as_ref() as &str, "hello");
        assert_eq!(std::borrow::Borrow::<str>::borrow(&text), "hello");
        assert_eq!(text.to_string(), "hello");
        assert_eq!(String::from(text), "hello");
        assert!("123456789".parse::<BoundedText<8>>().is_err());
    }

    #[test]
    fn test_bounded_text_serde() {
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Holder {
            text: BoundedText<5>,
        }

        let holder: Holder = toml::from_str(r#"text = "abc""#).unwrap();
        assert_eq!(holder.text, "abc");
        assert_eq!(toml::to_string(&holder).unwrap().trim(), r#"text = "abc""#);
        assert!(toml::from_str::<Holder>(r#"text = "abcdef""#).is_err());
        assert!(toml::from_str::<Holder>(r#"text = " ""#).is_err());
    }

    #[test]
    fn test_size_limit_zero_is_unlimited_and_cap_is_enforced() {
        assert_eq!(SizeLimit::new(0), Ok(SizeLimit::UNLIMITED));
        assert!(SizeLimit::new(MAX_FILE_SIZE_LIMIT).is_ok());
        assert_eq!(
            SizeLimit::new(MAX_FILE_SIZE_LIMIT + 1),
            Err(InvalidSizeLimit {
                value: MAX_FILE_SIZE_LIMIT + 1,
                max: MAX_FILE_SIZE_LIMIT
            })
        );
    }

    #[test]
    fn test_size_limit_admits_up_to_and_including_the_limit() {
        let limit = SizeLimit::from_static(4);
        assert!(limit.admits(0));
        assert!(limit.admits(4));
        assert!(!limit.admits(5));
        assert!(SizeLimit::UNLIMITED.admits(u64::MAX));
    }

    #[test]
    fn test_read_cap_for_is_the_limit_plus_one_and_saturates() {
        assert_eq!(SizeLimit::read_cap_for(NonZeroU64::new(100).unwrap()), 101);
        assert_eq!(SizeLimit::read_cap_for(NonZeroU64::MAX), u64::MAX);
    }

    #[test]
    fn test_server_start_concurrency_rejects_zero_with_a_typed_error() {
        assert_eq!(
            ServerStartConcurrency::new(0),
            Err(InvalidServerStartConcurrency)
        );
        assert_eq!(ServerStartConcurrency::new(3).unwrap().get(), 3);
    }

    #[test]
    fn test_size_limit_read_cap_is_limit_plus_one_or_unbounded() {
        assert_eq!(SizeLimit::from_static(100).read_cap(), 101);
        assert_eq!(
            SizeLimit::from_static(MAX_FILE_SIZE_LIMIT).read_cap(),
            MAX_FILE_SIZE_LIMIT + 1
        );
        assert_eq!(SizeLimit::UNLIMITED.read_cap(), u64::MAX);
    }

    #[test]
    fn test_size_limit_and_document_limit_serde_use_zero_for_unlimited() {
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Holder {
            size: SizeLimit,
            docs: DocumentLimit,
        }

        let holder: Holder = toml::from_str("size = 0\ndocs = 0").unwrap();
        assert_eq!(holder.size, SizeLimit::UNLIMITED);
        assert_eq!(holder.docs, DocumentLimit::UNLIMITED);
        assert_eq!(
            toml::to_string(&holder).unwrap().trim(),
            "size = 0\ndocs = 0"
        );
        let holder: Holder = toml::from_str("size = 7\ndocs = 3").unwrap();
        assert_eq!(holder.size.get().unwrap().get(), 7);
        assert_eq!(holder.docs.get().unwrap().get(), 3);
        assert!(toml::from_str::<Holder>("size = 1073741825\ndocs = 1").is_err());
        assert!(toml::from_str::<Holder>("size = 1\ndocs = -1").is_err());
    }

    #[test]
    fn test_defaults_match_documented_values() {
        assert_eq!(SizeLimit::default().get().unwrap().get(), 10 * 1024 * 1024);
        assert_eq!(DocumentLimit::default().get().unwrap().get(), 100);
    }

    #[test]
    fn test_search_depth_bounds() {
        assert_eq!(SearchDepth::new(0).unwrap().get(), 0);
        assert_eq!(
            SearchDepth::new(MAX_HEURISTICS_DEPTH).unwrap().get(),
            MAX_HEURISTICS_DEPTH
        );
        assert!(SearchDepth::new(MAX_HEURISTICS_DEPTH + 1).is_err());
        assert_eq!(SearchDepth::default().get(), DEFAULT_HEURISTICS_MAX_DEPTH);
    }

    #[test]
    fn test_search_depth_error_reports_range() {
        let err = SearchDepth::try_from(999_999).unwrap_err();
        assert_eq!(
            err.to_string(),
            "heuristics_max_depth (999999) exceeds the maximum of 64"
        );
    }

    #[test]
    fn test_search_depth_serde() {
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Holder {
            depth: SearchDepth,
        }

        let holder: Holder = toml::from_str("depth = 5").unwrap();
        assert_eq!(holder.depth.get(), 5);
        assert_eq!(toml::to_string(&holder).unwrap().trim(), "depth = 5");
        assert!(toml::from_str::<Holder>("depth = 65").is_err());
        assert!(toml::from_str::<Holder>("depth = -1").is_err());
    }
}
