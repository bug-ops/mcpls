//! Bounded whole-second durations for configuration values.
//!
//! A value of these types is in range by construction, so neither
//! `ServerConfig::validate` nor the use sites need to re-check or clamp it.

use std::num::NonZeroU64;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::bounded_number::impl_bounded_number;
use super::limits::MAX_TIMEOUT_SECONDS;
use crate::bridge::{
    DEFAULT_INDEXING_READY_TIMEOUT_SECS, INDEXING_STALENESS_BOUND, PROGRESS_SETTLE,
};

/// Why a number of seconds is not a valid [`BoundedSecs`].
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("must be between {min} and {max} seconds, got {value}")]
pub struct InvalidSecs {
    /// The rejected value.
    pub value: u64,
    /// Smallest accepted value.
    pub min: u64,
    /// Largest accepted value.
    pub max: u64,
}

/// A whole number of seconds in `MIN..=MAX`, with `MIN >= 1`.
///
/// Deserializes from a TOML integer and rejects out-of-range values at load
/// time, so an embedder cannot build an out-of-range value either.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use mcpls_core::config::TimeoutSecs;
///
/// assert!(TimeoutSecs::new(0).is_err());
/// assert!(TimeoutSecs::new(901).is_err());
/// assert_eq!(TimeoutSecs::new(45).unwrap().as_duration(), Duration::from_secs(45));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct BoundedSecs<const MIN: u64, const MAX: u64>(NonZeroU64);

impl<const MIN: u64, const MAX: u64> BoundedSecs<MIN, MAX> {
    /// Builds the value from `secs`.
    ///
    /// # Errors
    ///
    /// [`InvalidSecs`] when `secs` is outside `MIN..=MAX`.
    pub const fn new(secs: u64) -> Result<Self, InvalidSecs> {
        const { assert!(MIN >= 1 && MIN <= MAX, "MIN must satisfy 1 <= MIN <= MAX") };
        if secs >= MIN
            && secs <= MAX
            && let Some(secs) = NonZeroU64::new(secs)
        {
            return Ok(Self(secs));
        }
        Err(InvalidSecs {
            value: secs,
            min: MIN,
            max: MAX,
        })
    }

    /// The wrapped number of seconds, within `MIN..=MAX`.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// The wrapped number of seconds as a [`Duration`].
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        Duration::from_secs(self.0.get())
    }
}

impl_bounded_number!(
    @convert [const MIN: u64, const MAX: u64] BoundedSecs<MIN, MAX>, u64, InvalidSecs,
    |secs: BoundedSecs<MIN, MAX>| secs.get()
);

/// A server handshake or request timeout: 1 to [`MAX_TIMEOUT_SECONDS`] seconds.
pub type TimeoutSecs = BoundedSecs<1, MAX_TIMEOUT_SECONDS>;

impl_bounded_number!(@default TimeoutSecs, 30, "Thirty seconds.");

/// How long a whole-workspace query waits for indexing readiness: strictly
/// between `PROGRESS_SETTLE` and `INDEXING_STALENESS_BOUND`.
///
/// Violating either bound would reopen the cross-caller self-heal race
/// `INDEXING_STALENESS_BOUND` exists to prevent, or time out while the entry
/// is merely mid-settle.
pub type IndexingReadyTimeoutSecs =
    BoundedSecs<{ PROGRESS_SETTLE.as_secs() + 1 }, { INDEXING_STALENESS_BOUND.as_secs() - 1 }>;

impl_bounded_number!(
    @default IndexingReadyTimeoutSecs,
    DEFAULT_INDEXING_READY_TIMEOUT_SECS,
    "The built-in default, `DEFAULT_INDEXING_READY_TIMEOUT_SECS`."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeout_bounds() {
        assert!(TimeoutSecs::new(0).is_err());
        assert_eq!(
            TimeoutSecs::new(901),
            Err(InvalidSecs {
                value: 901,
                min: 1,
                max: MAX_TIMEOUT_SECONDS
            })
        );
        assert_eq!(TimeoutSecs::new(1).unwrap().get(), 1);
        assert_eq!(
            TimeoutSecs::new(MAX_TIMEOUT_SECONDS).unwrap().get(),
            MAX_TIMEOUT_SECONDS
        );
        assert!(TimeoutSecs::new(MAX_TIMEOUT_SECONDS + 1).is_err());
    }

    #[test]
    fn test_indexing_ready_timeout_bounds_are_exclusive() {
        let settle = PROGRESS_SETTLE.as_secs();
        let stale = INDEXING_STALENESS_BOUND.as_secs();
        assert!(IndexingReadyTimeoutSecs::new(settle).is_err());
        assert!(IndexingReadyTimeoutSecs::new(settle + 1).is_ok());
        assert!(IndexingReadyTimeoutSecs::new(stale - 1).is_ok());
        assert!(IndexingReadyTimeoutSecs::new(stale).is_err());
    }

    #[test]
    fn test_defaults_match_documented_values() {
        assert_eq!(TimeoutSecs::default().get(), 30);
        assert_eq!(
            IndexingReadyTimeoutSecs::default().get(),
            DEFAULT_INDEXING_READY_TIMEOUT_SECS
        );
    }

    #[test]
    fn test_try_from_error_reports_range() {
        let err = TimeoutSecs::try_from(0).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("must be between 1 and {MAX_TIMEOUT_SECONDS} seconds, got 0")
        );
    }

    #[test]
    fn test_serde_round_trip_and_rejection() {
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Holder {
            secs: TimeoutSecs,
        }

        let holder: Holder = toml::from_str("secs = 12").unwrap();
        assert_eq!(holder.secs.get(), 12);
        assert_eq!(toml::to_string(&holder).unwrap().trim(), "secs = 12");
        assert!(toml::from_str::<Holder>("secs = -1").is_err());
    }
}
