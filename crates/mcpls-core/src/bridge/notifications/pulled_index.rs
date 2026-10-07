//! Deciding whether a pushed diagnostic duplicates a pulled one, without
//! comparing every pair.

use std::collections::{HashMap, HashSet};

use lsp_types::Diagnostic as LspDiagnostic;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::message_as_str;

/// Start-line distance within which same-code, same-severity diagnostics from
/// the pull and push models are still considered the same underlying problem.
///
/// Derived from a captured rust-analyzer E0046 case (1 line apart between the
/// pulled trait-name span and the pushed `impl` span): wide enough to absorb
/// span drift between rust-analyzer's own spans and rustc's, narrow enough
/// that two genuinely distinct same-code errors elsewhere in a file are not
/// collapsed into one.
pub(super) const DUPLICATE_RANGE_PROXIMITY_LINES: u32 = 3;

/// A diagnostic's severity as `get_diagnostics` reports it: no severity and
/// unrecognized ones read as information.
///
/// Ordered from most to least severe, which is also the order the size cap
/// keeps diagnostics in.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
#[schemars(rename = "DiagnosticSeverity")]
#[schemars(description = "Diagnostic severity.")]
pub enum ReportedSeverity {
    /// Error diagnostic.
    Error,
    /// Warning diagnostic.
    Warning,
    /// Informational diagnostic; also what a diagnostic without a usable
    /// severity reads as.
    Information,
    /// Hint diagnostic.
    Hint,
}

impl ReportedSeverity {
    /// The severity `diagnostic` is reported with.
    pub const fn of(diagnostic: &LspDiagnostic) -> Self {
        match diagnostic.severity {
            Some(lsp_types::DiagnosticSeverity::Error) => Self::Error,
            Some(lsp_types::DiagnosticSeverity::Warning) => Self::Warning,
            Some(lsp_types::DiagnosticSeverity::Hint) => Self::Hint,
            _ => Self::Information,
        }
    }
}

/// A diagnostic's code as `get_diagnostics` reports it: integers read as their
/// decimal string.
pub fn reported_code(diagnostic: &LspDiagnostic) -> Option<std::borrow::Cow<'_, str>> {
    diagnostic.code.as_ref().map(|code| match code {
        lsp_types::Code::Int(n) => std::borrow::Cow::Owned(n.to_string()),
        lsp_types::Code::String(s) => std::borrow::Cow::Borrowed(s.as_str()),
    })
}

/// `(line, character)` of a position, ordered the way positions are.
pub(super) const fn point(position: lsp_types::Position) -> (u32, u32) {
    (position.line, position.character)
}

/// The ranges of the pulled diagnostics that share one `(severity, code)`,
/// answering "does a range overlap one of them or start within
/// `DUPLICATE_RANGE_PROXIMITY_LINES` lines of one of them" without scanning.
#[derive(Debug, Default)]
struct RangeIndex {
    /// `(start, end)` ordered by start.
    ranges: Vec<((u32, u32), (u32, u32))>,
    /// Greatest end among `ranges[..=i]`, for each `i`.
    max_end_through: Vec<(u32, u32)>,
}

impl RangeIndex {
    fn seal(&mut self) {
        self.ranges.sort_unstable();
        let mut max_end = (0, 0);
        self.max_end_through = self
            .ranges
            .iter()
            .map(|&(_, end)| {
                max_end = max_end.max(end);
                max_end
            })
            .collect();
    }

    fn is_near(&self, range: lsp_types::Range) -> bool {
        let (start, end) = (point(range.start), point(range.end));
        let started_before_end = self.ranges.partition_point(|&(s, _)| s <= end);
        let overlaps = started_before_end
            .checked_sub(1)
            .and_then(|last| self.max_end_through.get(last))
            .is_some_and(|&max_end| max_end >= start);
        if overlaps {
            return true;
        }
        let first_line = start.0.saturating_sub(DUPLICATE_RANGE_PROXIMITY_LINES);
        let from = self.ranges.partition_point(|&(s, _)| s.0 < first_line);
        self.ranges
            .get(from)
            .is_some_and(|&(s, _)| s.0 <= start.0.saturating_add(DUPLICATE_RANGE_PROXIMITY_LINES))
    }
}

/// What identifies a diagnostic that has no `code`: with no stable identity,
/// only the whole of range, severity and message counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UncodedKey<'a> {
    severity: ReportedSeverity,
    start: (u32, u32),
    end: (u32, u32),
    message: &'a str,
}

impl<'a> UncodedKey<'a> {
    fn of(diagnostic: &'a LspDiagnostic) -> Self {
        Self {
            severity: ReportedSeverity::of(diagnostic),
            start: point(diagnostic.range.start),
            end: point(diagnostic.range.end),
            message: message_as_str(&diagnostic.message),
        }
    }
}

/// The pulled diagnostics of a file, indexed so that deciding whether a pushed
/// one duplicates one of them (see [`Self::contains_same_problem`]) costs a
/// lookup instead of a scan of every pulled diagnostic.
///
/// `(severity, code)` is normalized once per pulled diagnostic, and a pushed
/// one is only compared inside its own group.
#[derive(Debug, Default)]
pub(super) struct PulledIndex<'a> {
    coded: HashMap<ReportedSeverity, HashMap<String, RangeIndex>>,
    uncoded: HashSet<UncodedKey<'a>>,
}

impl<'a> PulledIndex<'a> {
    pub(super) fn new(pulled: &'a [LspDiagnostic]) -> Self {
        let mut index = Self::default();
        for diagnostic in pulled {
            let severity = ReportedSeverity::of(diagnostic);
            let range = diagnostic.range;
            match reported_code(diagnostic) {
                Some(code) => {
                    index
                        .coded
                        .entry(severity)
                        .or_default()
                        .entry(code.into_owned())
                        .or_default()
                        .ranges
                        .push((point(range.start), point(range.end)));
                }
                None => {
                    index.uncoded.insert(UncodedKey::of(diagnostic));
                }
            }
        }
        for group in index.coded.values_mut().flat_map(HashMap::values_mut) {
            group.seal();
        }
        index
    }

    /// Whether `pushed` duplicates a pulled diagnostic.
    ///
    /// rust-analyzer reports the same logical problem through both paths with
    /// different `range` and rendered `message` (#244), so exact equality never
    /// collapses them. Two diagnostics that both carry a `code` are the same
    /// problem when `(severity, code)` match and their ranges overlap or start
    /// within `DUPLICATE_RANGE_PROXIMITY_LINES` lines of each other. Without a
    /// `code` on both sides there is no stable identity, and only equality of
    /// range, severity and message counts.
    pub(super) fn contains_same_problem(&self, pushed: &LspDiagnostic) -> bool {
        let severity = ReportedSeverity::of(pushed);
        reported_code(pushed).map_or_else(
            || self.uncoded.contains(&UncodedKey::of(pushed)),
            |code| {
                self.coded
                    .get(&severity)
                    .and_then(|groups| groups.get(code.as_ref()))
                    .is_some_and(|group| group.is_near(pushed.range))
            },
        )
    }
}
