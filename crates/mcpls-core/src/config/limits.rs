//! Bounded numeric limits for configuration values.
//!
//! A value of these types is in range by construction, so neither
//! `ServerConfig::validate` nor the use sites need to re-check or clamp it.

use std::num::{NonZeroU64, NonZeroUsize};

use serde::{Deserialize, Serialize};

use super::server::{DEFAULT_HEURISTICS_MAX_DEPTH, MAX_HEURISTICS_DEPTH};

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
    /// The built-in default, [`DEFAULT_HEURISTICS_MAX_DEPTH`].
    pub const DEFAULT: Self = match Self::new(DEFAULT_HEURISTICS_MAX_DEPTH) {
        Ok(depth) => depth,
        Err(_) => panic!("the default search depth must be in range"),
    };

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

impl Default for SearchDepth {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<usize> for SearchDepth {
    type Error = InvalidSearchDepth;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SearchDepth> for usize {
    fn from(depth: SearchDepth) -> Self {
        depth.get()
    }
}

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

    /// The built-in default, 10 MiB.
    pub const DEFAULT: Self = Self::from_static(10 * 1024 * 1024);

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
            Some(max) => max.get().saturating_add(1),
            None => u64::MAX,
        }
    }
}

impl Default for SizeLimit {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<u64> for SizeLimit {
    type Error = InvalidSizeLimit;

    fn try_from(bytes: u64) -> Result<Self, Self::Error> {
        Self::new(bytes)
    }
}

impl From<SizeLimit> for u64 {
    fn from(limit: SizeLimit) -> Self {
        limit.0.map_or(0, NonZeroU64::get)
    }
}

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

    /// The validated text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<const MAX: usize> std::fmt::Display for BoundedText<MAX> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<const MAX: usize> TryFrom<String> for BoundedText<MAX> {
    type Error = InvalidBoundedText;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::new(text)
    }
}

impl<const MAX: usize> From<BoundedText<MAX>> for String {
    fn from(text: BoundedText<MAX>) -> Self {
        text.0
    }
}

impl<const MAX: usize> PartialEq<str> for BoundedText<MAX> {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl<const MAX: usize> PartialEq<&str> for BoundedText<MAX> {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

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
