//! The client-preference order of position encodings offered to servers.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::bridge::PositionEncoding;

/// Why a list is not a valid [`PositionEncodings`].
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidPositionEncodings {
    /// The list had no entries.
    #[error("position_encodings cannot be empty")]
    Empty,
}

/// A non-empty position-encoding preference order, offered to each spawned
/// server as `capabilities.general.positionEncodings` during `initialize`.
///
/// Deserializes from a list of `"utf-8"`, `"utf-16"` or `"utf-32"` strings and
/// rejects an empty list or any other value at load time.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::PositionEncoding;
/// use mcpls_core::config::PositionEncodings;
///
/// assert!(PositionEncodings::new(vec![]).is_err());
/// let encodings = PositionEncodings::new(vec![PositionEncoding::Utf32]).unwrap();
/// assert_eq!(encodings.as_slice(), [PositionEncoding::Utf32]);
/// assert_eq!(
///     PositionEncodings::DEFAULT.as_slice(),
///     [PositionEncoding::Utf8, PositionEncoding::Utf16]
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<PositionEncoding>", into = "Vec<PositionEncoding>")]
pub struct PositionEncodings(Cow<'static, [PositionEncoding]>);

impl PositionEncodings {
    /// `utf-8` first, then `utf-16`.
    ///
    /// `utf-8` is listed first deliberately: rust-analyzer and clangd both
    /// negotiate down to it, so the non-UTF-16 conversion path in
    /// `bridge/encoding.rs` is the common case, not an edge case.
    pub const DEFAULT: Self = Self(Cow::Borrowed(&[
        PositionEncoding::Utf8,
        PositionEncoding::Utf16,
    ]));

    /// Builds a preference order from `encodings`.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidPositionEncodings::Empty`] for an empty list.
    pub fn new(encodings: Vec<PositionEncoding>) -> Result<Self, InvalidPositionEncodings> {
        if encodings.is_empty() {
            return Err(InvalidPositionEncodings::Empty);
        }
        Ok(Self(Cow::Owned(encodings)))
    }

    /// The encodings in preference order; never empty.
    #[must_use]
    pub fn as_slice(&self) -> &[PositionEncoding] {
        &self.0
    }
}

impl Default for PositionEncodings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<Vec<PositionEncoding>> for PositionEncodings {
    type Error = InvalidPositionEncodings;

    fn try_from(encodings: Vec<PositionEncoding>) -> Result<Self, Self::Error> {
        Self::new(encodings)
    }
}

impl From<PositionEncodings> for Vec<PositionEncoding> {
    fn from(encodings: PositionEncodings) -> Self {
        encodings.0.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Holder {
        encodings: PositionEncodings,
    }

    #[test]
    fn test_rejects_empty() {
        assert_eq!(
            PositionEncodings::new(vec![]),
            Err(InvalidPositionEncodings::Empty)
        );
        assert!(toml::from_str::<Holder>("encodings = []").is_err());
    }

    #[test]
    fn test_rejects_unknown_value_listing_valid_ones() {
        let err = toml::from_str::<Holder>(r#"encodings = ["utf-7"]"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("utf-7"), "{err}");
        assert!(err.contains("utf-8") && err.contains("utf-16") && err.contains("utf-32"));
    }

    #[test]
    fn test_preserves_order_and_round_trips() {
        let holder: Holder = toml::from_str(r#"encodings = ["utf-32", "utf-8"]"#).unwrap();
        assert_eq!(
            holder.encodings.as_slice(),
            [PositionEncoding::Utf32, PositionEncoding::Utf8]
        );
        let text = toml::to_string(&holder).unwrap();
        assert_eq!(toml::from_str::<Holder>(&text).unwrap(), holder);
    }

    #[test]
    fn test_default_round_trips() {
        let holder = Holder {
            encodings: PositionEncodings::DEFAULT,
        };
        let text = toml::to_string(&holder).unwrap();
        assert_eq!(toml::from_str::<Holder>(&text).unwrap(), holder);
    }
}
