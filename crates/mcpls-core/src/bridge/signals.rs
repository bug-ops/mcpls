//! Route-level health signals attached to tool results.
//!
//! One definition shared by every tool that reports them, so two tools can
//! never name the same signal differently or sample it differently.

use schemars::JsonSchema;
use serde::Serialize;

use super::{IndexingState, NotificationCache};
use crate::config::ServerId;
use crate::redaction::{Redactions, ServerText};

/// Whether the routed server was still indexing its workspace.
///
/// Attached to the results of tools whose answer depends on the workspace
/// index, so an empty result with the flag set is read as "possibly
/// incomplete" rather than "nothing here".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct IndexingSignal {
    /// `true` if the routed language server reported its initial workspace indexing as
    /// in progress during this read; the result may reflect a partial index.
    pub indexing_in_progress: bool,
}

impl IndexingSignal {
    /// Samples the indexing state of `route_id`'s server.
    ///
    /// Keyed on the routing identity, not cache ownership, which a respawn
    /// clears (#359).
    #[must_use]
    pub fn sample(cache: &NotificationCache, route_id: Option<&ServerId>) -> Self {
        Self {
            indexing_in_progress: route_id
                .is_some_and(|id| cache.indexing_state(id) == IndexingState::Loading),
        }
    }

    /// Combines two samples taken at different times: the signal is set if it
    /// was set in either.
    #[must_use]
    pub const fn union(self, later: Self) -> Self {
        Self {
            indexing_in_progress: self.indexing_in_progress || later.indexing_in_progress,
        }
    }
}

/// A tool result with the [`IndexingSignal`] flattened beside it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Indexed<T> {
    /// The tool's own result.
    #[serde(flatten)]
    pub result: T,
    /// Whether the routed server was still indexing.
    #[serde(flatten)]
    pub indexing: IndexingSignal,
}

impl<T> Indexed<T> {
    /// Pairs `result` with `indexing`.
    #[must_use]
    pub const fn new(result: T, indexing: IndexingSignal) -> Self {
        Self { result, indexing }
    }
}

impl<T: ServerText> ServerText for Indexed<T> {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            result,
            indexing: _,
        } = self;
        result.redact_server_text(redactions);
    }
}

/// Route-level health signals shared by every diagnostics reader
/// (`get_diagnostics`, `get_cached_diagnostics`, and the diagnostics
/// resource), so the three can never report different keys or semantics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RouteSignals {
    /// `true` if the language server publishing this file's diagnostics crashed and was
    /// restarted during this mcpls session. Diagnostics it delivers only by push (e.g.
    /// rust-analyzer's flycheck/clippy) are no longer received, so the returned
    /// diagnostics may be incomplete until mcpls restarts.
    pub push_notifications_degraded: bool,
    /// The indexing half, shared with the tools that only report it.
    #[serde(flatten)]
    pub indexing: IndexingSignal,
}

impl RouteSignals {
    /// Samples both signals of `route_id`'s server.
    #[must_use]
    pub fn sample(cache: &NotificationCache, route_id: Option<&ServerId>) -> Self {
        Self {
            push_notifications_degraded: route_id.is_some_and(|id| cache.is_push_degraded(id)),
            indexing: IndexingSignal::sample(cache, route_id),
        }
    }

    /// Combines two samples taken at different times: a signal is set if it was set in
    /// either.
    #[must_use]
    pub const fn union(self, later: Self) -> Self {
        Self {
            push_notifications_degraded: self.push_notifications_degraded
                || later.push_notifications_degraded,
            indexing: self.indexing.union(later.indexing),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unknown_route_samples_no_signal() {
        let cache = NotificationCache::new();
        assert_eq!(RouteSignals::sample(&cache, None), RouteSignals::default());
        let id = ServerId::from_static("rust");
        assert_eq!(
            RouteSignals::sample(&cache, Some(&id)),
            RouteSignals::default()
        );
    }

    #[test]
    fn test_union_keeps_a_signal_set_in_either_sample() {
        let degraded = RouteSignals {
            push_notifications_degraded: true,
            ..RouteSignals::default()
        };
        let loading = RouteSignals {
            indexing: IndexingSignal {
                indexing_in_progress: true,
            },
            ..RouteSignals::default()
        };
        let both = degraded.union(loading);
        assert!(both.push_notifications_degraded);
        assert!(both.indexing.indexing_in_progress);
        assert_eq!(loading.union(degraded), both);
    }

    #[test]
    fn test_indexed_flattens_the_signal_beside_the_result() {
        #[derive(Serialize)]
        struct Items {
            items: Vec<u8>,
        }
        let indexed = Indexed::new(
            Items { items: vec![1] },
            IndexingSignal {
                indexing_in_progress: true,
            },
        );
        assert_eq!(
            serde_json::to_string(&indexed).unwrap(),
            r#"{"items":[1],"indexing_in_progress":true}"#
        );
    }

    #[test]
    fn test_indexing_half_flattens_into_one_object() {
        let signals = RouteSignals {
            push_notifications_degraded: true,
            indexing: IndexingSignal {
                indexing_in_progress: true,
            },
        };
        assert_eq!(
            serde_json::to_string(&signals).unwrap(),
            r#"{"push_notifications_degraded":true,"indexing_in_progress":true}"#
        );
        assert_eq!(
            serde_json::to_string(&IndexingSignal::default()).unwrap(),
            r#"{"indexing_in_progress":false}"#
        );
    }
}
