//! Source text of the symbol each location names or sits in.
//!
//! Callers that pass [`LocationContext::SymbolSource`](super::LocationContext) get,
//! per location, a [`SourceOutcome`]: the bounded text of the symbol the item
//! names (a definition) or sits in (a reference), cut out of the document text
//! the tracker already holds. The pass runs inside the enclosing-symbol lookup
//! of [`super::enclosing`], under the same pin, so it adds no LSP request and
//! no disk read.

use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::dto::{Location, Range, Symbol};
use super::enclosing::{EnclosingSymbolOutcome, Hit, outcome_for};
use super::symbols::DocumentSymbolTree;
use crate::bridge::notifications::DocumentStamp;
use crate::bridge::state::{DocumentTracker, ExcerptError, ExcerptLimits};
use crate::config::ServerId;
use crate::redaction::{Redactions, ServerText};

/// Most lines one item's source may span.
const ITEM_MAX_LINES: NonZeroU32 = NonZeroU32::MIN.saturating_add(199);

/// Most bytes one item's source may hold (16 KiB).
const ITEM_MAX_BYTES: NonZeroUsize = NonZeroUsize::MIN.saturating_add(16 * 1024 - 1);

/// Most bytes of `included` text one call may return (64 KiB).
const CALL_BUDGET_BYTES: usize = 64 * 1024;

/// Most distinct spans one call cuts; later new spans are `call_budget`, so a
/// call with many tiny spans cannot make unbounded cuts.
const MAX_SOURCE_CUTS_PER_CALL: usize = 256;

/// Position of an earlier item in the same `locations` list.
///
/// Built only by the source pass, so it always names an `included` item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(inline)]
pub struct ItemIndex(u32);

impl ItemIndex {
    /// The zero-based index in the `locations` list.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    fn of(index: usize) -> Self {
        Self(u32::try_from(index).unwrap_or(u32::MAX))
    }
}

/// Why an item has no source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceSkipReason {
    /// No symbol or link range names a span for this item.
    NoSpan,
    /// The span does not fit the remaining per-call budget.
    CallBudget,
    /// The document changed after the symbol lookup, so the ranges no longer fit its text.
    DocumentChanged,
    /// The file's symbol lookup was skipped or failed; the item's `enclosing_symbol` says why.
    SymbolLookupSkipped,
}

/// The source of one location.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::SourceOutcome;
///
/// let outcome = SourceOutcome::Included { text: "fn f() {}".to_owned(), truncated: false };
/// let json = serde_json::to_value(&outcome).unwrap();
/// assert_eq!(json["status"], "included");
/// assert_eq!(json["text"], "fn f() {}");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SourceOutcome {
    /// The symbol's text, at most 200 lines and 16 KiB.
    Included {
        /// The text, lines joined with `\n`.
        text: String,
        /// Whether the text was cut at a size limit.
        truncated: bool,
    },
    /// The same span as an earlier item, whose entry carries the text.
    SameAs {
        /// Index in `locations` of the item that carries the text.
        item: ItemIndex,
    },
    /// No source text for this item, with the reason why.
    NotComputed {
        /// Why the item has no source text.
        reason: SourceSkipReason,
    },
}

impl SourceOutcome {
    const fn skipped(reason: SourceSkipReason) -> Self {
        Self::NotComputed { reason }
    }
}

impl ServerText for SourceOutcome {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        match self {
            Self::Included { text, truncated: _ } => redactions.note_payload(text),
            Self::SameAs { item: _ } | Self::NotComputed { reason: _ } => {}
        }
    }
}

/// A [`Location`] optionally carrying its source text.
///
/// Serializes as the location's own fields with `source` alongside, so the
/// default output of a location tool is unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "Location")]
pub struct NavigationLocation {
    /// The location itself.
    #[serde(flatten)]
    pub location: Location,
    /// Present only when `context: "symbol_source"` was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "SourceOutcome")]
    pub source: Option<SourceOutcome>,
}

impl From<Location> for NavigationLocation {
    fn from(location: Location) -> Self {
        Self {
            location,
            source: None,
        }
    }
}

impl ServerText for NavigationLocation {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self { location, source } = self;
        location.redact_server_text(redactions);
        source.redact_server_text(redactions);
    }
}

/// Which span of a file an item's source is cut from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SpanRule {
    /// The go-to tools: the symbol the location names, else its link's target range.
    Definition,
    /// `get_references`: the symbol the location sits in.
    EnclosingBody,
}

/// What the pass knows about a span it already met in this call.
#[derive(Debug, Clone, Copy)]
enum SpanSeen {
    Included(ItemIndex),
    OverBudget,
}

/// The tracked file the spans of one lookup are cut from.
pub(super) struct SourceFile<'a> {
    pub(super) tracker: &'a DocumentTracker,
    pub(super) path: &'a Path,
    pub(super) server: &'a ServerId,
    /// The document state before the symbol request; `None` when it was not tracked.
    pub(super) stamp: Option<DocumentStamp>,
}

/// One call's slicing state: the remaining byte budget, the spans already
/// sliced and the outcome of every item.
pub(super) struct SourcePass<'a> {
    rule: SpanRule,
    remaining: usize,
    cuts: usize,
    seen: HashMap<(&'a str, Range), SpanSeen>,
    outcomes: Vec<SourceOutcome>,
}

impl<'a> SourcePass<'a> {
    /// A pass over `items` items, all skipped until their file is filled.
    pub(super) fn new(rule: SpanRule, items: usize) -> Self {
        Self {
            rule,
            remaining: CALL_BUDGET_BYTES,
            cuts: 0,
            seen: HashMap::with_capacity(items.min(MAX_SOURCE_CUTS_PER_CALL)),
            outcomes: vec![SourceOutcome::skipped(SourceSkipReason::SymbolLookupSkipped); items],
        }
    }

    /// The outcome of every item, in input order.
    pub(super) fn into_outcomes(self) -> Vec<SourceOutcome> {
        self.outcomes
    }

    /// Cuts the source of `hits` (index, hit), all in `file` and in item order.
    pub(super) fn fill_file(
        &mut self,
        file: &SourceFile<'_>,
        tree: &DocumentSymbolTree,
        hits: impl Iterator<Item = (usize, &'a Hit<'a>)>,
    ) {
        for (index, hit) in hits {
            let outcome = self.outcome_for_hit(file, tree, index, hit);
            if let Some(slot) = self.outcomes.get_mut(index) {
                *slot = outcome;
            }
        }
    }

    fn outcome_for_hit(
        &mut self,
        file: &SourceFile<'_>,
        tree: &DocumentSymbolTree,
        index: usize,
        hit: &'a Hit<'a>,
    ) -> SourceOutcome {
        let Some(span) = self.span_of(tree, hit) else {
            return SourceOutcome::skipped(SourceSkipReason::NoSpan);
        };
        match self.seen.get(&(hit.uri, span)) {
            Some(SpanSeen::Included(item)) => return SourceOutcome::SameAs { item: *item },
            Some(SpanSeen::OverBudget) => {
                return SourceOutcome::skipped(SourceSkipReason::CallBudget);
            }
            None => {}
        }
        let Some(stamp) = file.stamp else {
            return SourceOutcome::skipped(SourceSkipReason::DocumentChanged);
        };
        if self.remaining == 0 || self.cuts >= MAX_SOURCE_CUTS_PER_CALL {
            return SourceOutcome::skipped(SourceSkipReason::CallBudget);
        }
        self.cuts = self.cuts.saturating_add(1);
        let limits = ExcerptLimits {
            max_lines: ITEM_MAX_LINES,
            max_bytes: ITEM_MAX_BYTES,
        };
        match file
            .tracker
            .excerpt(file.path, file.server, stamp, &span, limits)
        {
            Err(ExcerptError::Changed | ExcerptError::Untracked) => {
                SourceOutcome::skipped(SourceSkipReason::DocumentChanged)
            }
            Ok(excerpt) if excerpt.text.len() > self.remaining => {
                self.seen.insert((hit.uri, span), SpanSeen::OverBudget);
                SourceOutcome::skipped(SourceSkipReason::CallBudget)
            }
            Ok(excerpt) => {
                self.remaining = self.remaining.saturating_sub(excerpt.text.len());
                self.seen
                    .insert((hit.uri, span), SpanSeen::Included(ItemIndex::of(index)));
                SourceOutcome::Included {
                    text: excerpt.text,
                    truncated: excerpt.truncated,
                }
            }
        }
    }

    fn span_of(&self, tree: &DocumentSymbolTree, hit: &Hit<'_>) -> Option<Range> {
        match self.rule {
            SpanRule::EnclosingBody => match outcome_for(tree, hit.range) {
                EnclosingSymbolOutcome::Resolved(symbol) => Some(symbol.range),
                _ => None,
            },
            SpanRule::Definition => definition_span(tree, hit.range).or(hit.target_range),
        }
    }
}

fn contains(outer: &Range, inner: &Range) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// The `range` of the innermost hierarchical symbol whose `selection_range`
/// contains `item`; a flat symbol list never matches.
fn definition_span(tree: &DocumentSymbolTree, item: &Range) -> Option<Range> {
    let DocumentSymbolTree::Hierarchical(symbols) = tree else {
        return None;
    };
    let mut level: &[Symbol] = symbols;
    let mut found = None;
    while let Some(symbol) = level.iter().find(|symbol| contains(&symbol.range, item)) {
        if contains(&symbol.selection_range, item) {
            found = Some(symbol.range);
        }
        level = symbol.children.as_deref().unwrap_or_default();
    }
    found
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::bridge::{Position2D, ResourceLimits};

    fn span(line: u32, from: u32, to: u32) -> Range {
        Range {
            start: Position2D::at(line, from),
            end: Position2D::at(line, to),
        }
    }

    fn line_symbol(line: u32, width: u32) -> Symbol {
        Symbol {
            name: format!("s{line}"),
            kind: 12,
            range: span(line, 1, width + 1),
            selection_range: span(line, 1, 2),
            children: None,
        }
    }

    fn open(tracker: &DocumentTracker, path: &str, text: &str) -> (PathBuf, ServerId) {
        let path = PathBuf::from(path);
        let server = ServerId::from_static("rust");
        tracker.open(path.clone(), text.to_owned()).unwrap();
        tracker.set_synced_version_for_test(&path, &server, 1);
        (path, server)
    }

    fn tracker() -> DocumentTracker {
        DocumentTracker::new(ResourceLimits::default(), HashMap::new())
    }

    fn hits<'a>(uri: &'a str, ranges: &'a [Range]) -> Vec<Hit<'a>> {
        ranges
            .iter()
            .map(|range| Hit {
                uri,
                range,
                target_range: None,
            })
            .collect()
    }

    #[test]
    fn test_no_excerpt_is_cut_once_the_call_budget_is_spent() {
        let tracker = tracker();
        let wide = "a".repeat(20_000);
        let (a, server) = open(&tracker, "/test/a.rs", &format!("{wide}\n").repeat(4));
        let (b, _) = open(&tracker, "/test/b.rs", "x\n");
        let stale = tracker.stamp(&b, &server);
        tracker.set_synced_version_for_test(&b, &server, 99);
        let a_tree =
            DocumentSymbolTree::Hierarchical((1..=4).map(|l| line_symbol(l, 20_000)).collect());
        let b_tree = DocumentSymbolTree::Hierarchical(vec![line_symbol(1, 1)]);
        let a_ranges: Vec<Range> = (1..=4).map(|l| span(l, 2, 4)).collect();
        let a_hits = hits("file:///test/a.rs", &a_ranges);
        let b_ranges = [span(1, 1, 2)];
        let b_hits = hits("file:///test/b.rs", &b_ranges);
        let mut pass = SourcePass::new(SpanRule::EnclosingBody, 5);

        pass.fill_file(
            &SourceFile {
                tracker: &tracker,
                path: &a,
                server: &server,
                stamp: tracker.stamp(&a, &server),
            },
            &a_tree,
            a_hits.iter().enumerate(),
        );
        pass.fill_file(
            &SourceFile {
                tracker: &tracker,
                path: &b,
                server: &server,
                stamp: stale,
            },
            &b_tree,
            b_hits.iter().map(|hit| (4, hit)),
        );

        let outcomes = pass.into_outcomes();
        assert!(
            outcomes[..4]
                .iter()
                .all(|outcome| matches!(outcome, SourceOutcome::Included { .. }))
        );
        assert_eq!(
            outcomes[4],
            SourceOutcome::skipped(SourceSkipReason::CallBudget),
            "a stale stamp would read document_changed if an excerpt were cut"
        );
    }

    #[test]
    fn test_the_257th_distinct_span_is_call_budget() {
        let tracker = tracker();
        let (path, server) = open(&tracker, "/test/many.rs", &"x\n".repeat(300));
        let tree = DocumentSymbolTree::Hierarchical((1..=300).map(|l| line_symbol(l, 1)).collect());
        let ranges: Vec<Range> = (1..=300).map(|l| span(l, 1, 2)).collect();
        let all_hits = hits("file:///test/many.rs", &ranges);
        let mut pass = SourcePass::new(SpanRule::EnclosingBody, all_hits.len());

        pass.fill_file(
            &SourceFile {
                tracker: &tracker,
                path: &path,
                server: &server,
                stamp: tracker.stamp(&path, &server),
            },
            &tree,
            all_hits.iter().enumerate(),
        );

        let outcomes = pass.into_outcomes();
        assert!(matches!(outcomes[255], SourceOutcome::Included { .. }));
        assert_eq!(
            outcomes[256],
            SourceOutcome::skipped(SourceSkipReason::CallBudget)
        );
        assert_eq!(
            outcomes[299],
            SourceOutcome::skipped(SourceSkipReason::CallBudget)
        );
    }
}
