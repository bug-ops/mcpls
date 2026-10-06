//! The closed states a diagnostics answer can be in.
//!
//! Shared by `get_diagnostics`, `get_cached_diagnostics` and the diagnostics
//! resource, so the three report the same state for the same file.

use schemars::JsonSchema;
use serde::Serialize;

/// Whether the diagnostics cache has an answer for a file.
///
/// An empty list under [`Self::Published`] is a real answer: the server said
/// the file is clean. [`Self::Pending`] and [`Self::Evicted`] carry no answer
/// at all, so an empty list next to them must not be read as clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsAvailability {
    /// The server published diagnostics for the file, possibly an empty list.
    Published,
    /// Nothing has been published for the file yet.
    Pending,
    /// A publish arrived but was dropped to bound the cache, so what the
    /// server last said about the file is no longer known.
    Evicted,
}

/// Where a `get_diagnostics` answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsOrigin {
    /// A `textDocument/diagnostic` pull, merged with the push cache.
    Pull,
    /// The push cache alone, because the server advertises no pull provider.
    PushCache,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_availability_wire_names() {
        for (value, name) in [
            (DiagnosticsAvailability::Published, "published"),
            (DiagnosticsAvailability::Pending, "pending"),
            (DiagnosticsAvailability::Evicted, "evicted"),
        ] {
            assert_eq!(serde_json::to_value(value).unwrap(), name);
        }
    }

    #[test]
    fn test_origin_wire_names() {
        assert_eq!(
            serde_json::to_value(DiagnosticsOrigin::Pull).unwrap(),
            "pull"
        );
        assert_eq!(
            serde_json::to_value(DiagnosticsOrigin::PushCache).unwrap(),
            "push_cache"
        );
    }
}
