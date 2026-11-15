//! Opt-in enclosing-symbol context for locations and diagnostics.
//!
//! Callers that pass [`ResultContext::EnclosingSymbol`] get, per item, the
//! innermost symbol of the item's file that contains it. The default
//! ([`ResultContext::None`]) issues no extra request and adds no field, so
//! existing output stays byte-identical. The location tools additionally
//! accept [`LocationContext::SymbolSource`], which reuses this pass to cut
//! each item's source text (see [`super::source`]).

use std::cmp::Reverse;
use std::collections::HashMap;
use std::ops::Deref;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::Translator;
use super::dto::{Diagnostic, PositionDegradation, Range, Symbol};
use super::navigation::NormalizedLocation;
use super::routing::{FileLocalCapability, IndexingGate};
use super::source::{NavigationLocation, SourceFile, SourceOutcome, SourcePass, SpanRule};
use super::symbols::{DocumentSymbolTree, FetchedSymbols, FlatSymbol, symbol_tree};
use crate::config::DocumentLimit;
use crate::error::Error;
use crate::redaction::{Redactions, ServerText};

/// Upper bound on the number of distinct files one call enriches.
const MAX_ENRICHED_FILES: usize = 16;

/// Overall time budget for the `documentSymbol` lookups of one call, so a
/// slow server cannot multiply the call's latency by the file cap.
const ENRICHMENT_DEADLINE: Duration = Duration::from_secs(30);

/// Extra context a tool can attach to each returned item.
///
/// Only tools whose input schema declares `context` act on it.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::ResultContext;
///
/// assert_eq!(ResultContext::default(), ResultContext::None);
/// let parsed: ResultContext = serde_json::from_str("\"enclosing_symbol\"").unwrap();
/// assert_eq!(parsed, ResultContext::EnclosingSymbol);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResultContext {
    /// Return the plain result with no extra lookups.
    #[default]
    None,
    /// Attach the innermost enclosing symbol to each item, one documentSymbol request per file.
    ///
    /// The symbol carries its name path, kind and range; requests are capped per call.
    EnclosingSymbol,
}

/// What extra to attach to each returned location.
///
/// Superset of [`ResultContext`] for the location tools: additionally accepts
/// `symbol_source`.
///
/// # Examples
///
/// ```
/// use mcpls_core::bridge::LocationContext;
///
/// assert_eq!(LocationContext::default(), LocationContext::None);
/// let parsed: LocationContext = serde_json::from_str("\"symbol_source\"").unwrap();
/// assert_eq!(parsed, LocationContext::SymbolSource);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LocationContext {
    /// Return the plain result with no extra lookups.
    #[default]
    None,
    /// Attach the innermost enclosing symbol to each location.
    EnclosingSymbol,
    /// Attach the source text of the symbol each location resolves to.
    SymbolSource,
}

/// How much ancestry an [`EnclosingSymbol`] name path carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SymbolFidelity {
    /// Built from the server's `DocumentSymbol` ancestor chain: complete.
    Hierarchical,
    /// Built from a flat `SymbolInformation` list: at most one container
    /// segment from `containerName`, so ancestors may be missing.
    Flat,
}

/// The innermost symbol containing an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EnclosingSymbol {
    /// Symbol names from the outermost ancestor down to the symbol itself.
    pub name_path: Vec<String>,
    /// LSP numeric symbol kind, the same value `get_document_symbols`
    /// reports (usable as a `kind_filter`).
    pub kind: u32,
    /// Range of the symbol (1-based).
    pub range: Range,
    /// How complete `name_path` is.
    pub fidelity: SymbolFidelity,
}

/// Why an item's enclosing symbol was not looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotComputedReason {
    /// The call already enriched the maximum number of distinct files.
    FileCap,
    /// The file is outside every workspace root, so it is not opened.
    OutOfWorkspace,
    /// The document tracker is at its limit and could not open the file.
    TrackerLimit,
    /// The call's overall enrichment time budget was spent.
    Deadline,
}

/// Why a lookup was attempted but yielded no symbol tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// The routed server does not advertise `documentSymbolProvider`.
    CapabilityAbsent,
    /// The `documentSymbol` request or file open failed.
    RequestFailed,
    /// The `documentSymbol` request timed out.
    TimedOut,
}

/// What is known about the enclosing symbol of one item.
///
/// `top_level` means the file's symbols were read and none contains the
/// item. `not_computed` and `unavailable` mean nothing is known and must
/// never be read as top level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EnclosingSymbolOutcome {
    /// The innermost containing symbol.
    Resolved(EnclosingSymbol),
    /// No symbol of the file contains the item.
    TopLevel,
    /// The lookup was skipped.
    NotComputed {
        /// Why it was skipped.
        reason: NotComputedReason,
    },
    /// The lookup was attempted and failed.
    Unavailable {
        /// Why it failed.
        reason: UnavailableReason,
    },
}

/// Result-level statement about how complete the enrichment is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EnrichmentSummary {
    /// Distinct files whose symbol tree was obtained.
    pub files_enriched: usize,
    /// Distinct files for which no symbol tree was obtained, for any reason.
    pub files_skipped: usize,
    /// Whether the file cap or the time budget skipped some files, so a
    /// retry with fewer items may enrich more.
    pub cut_short: bool,
}

/// An item optionally carrying its enclosing symbol.
///
/// Serializes as the item's own fields with `enclosing_symbol` alongside, so
/// the default output of a tool is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "Contextual{T}")]
pub struct Contextual<T> {
    /// The item itself.
    #[serde(flatten)]
    pub inner: T,
    /// Present only when `context: "enclosing_symbol"` was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "EnclosingSymbolOutcome")]
    pub enclosing_symbol: Option<EnclosingSymbolOutcome>,
}

/// A location optionally carrying its enclosing symbol and source text.
pub type ContextualLocation = Contextual<NavigationLocation>;

/// A [`Diagnostic`] optionally carrying its enclosing symbol.
pub type ContextualDiagnostic = Contextual<Diagnostic>;

impl<T> From<T> for Contextual<T> {
    fn from(inner: T) -> Self {
        Self {
            inner,
            enclosing_symbol: None,
        }
    }
}

impl<T> Deref for Contextual<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

/// Items after the optional enrichment pass.
pub(super) struct Contextualized<T> {
    pub(super) items: Vec<Contextual<T>>,
    pub(super) enrichment: Option<EnrichmentSummary>,
    pub(super) positions_degraded: Option<PositionDegradation>,
}

impl<T> Contextualized<T> {
    /// Folds in the degradation the items' own normalization already found.
    fn with_input_degradation(mut self, degraded: Option<PositionDegradation>) -> Self {
        self.positions_degraded = degraded.max(self.positions_degraded);
        self
    }
}

/// Number of distinct files one call may enrich: 16 when the tracker limit is
/// disabled, else a quarter of the limit clamped to `1..=16`, so one
/// call cannot evict most of the documents other handlers depend on.
pub(super) fn enrichment_file_cap(max_documents: DocumentLimit) -> usize {
    max_documents.get().map_or(MAX_ENRICHED_FILES, |max| {
        (max.get() / 4).clamp(1, MAX_ENRICHED_FILES)
    })
}

/// How strictly a symbol must contain an item to enclose it.
#[derive(Clone, Copy)]
pub(super) enum Containment {
    /// The symbol contains the whole item range.
    Whole,
    /// The symbol contains the item's start; used when none contains it whole.
    Start,
}

impl Containment {
    pub(super) fn admits(self, symbol: &Range, item: &Range) -> bool {
        let starts_inside = symbol.start <= item.start;
        match self {
            Self::Whole => starts_inside && item.end <= symbol.end,
            Self::Start => starts_inside && item.start <= symbol.end,
        }
    }
}

const CONTAINMENT_RULES: [Containment; 2] = [Containment::Whole, Containment::Start];

pub(super) fn innermost_hierarchical(symbols: &[Symbol], item: &Range) -> Option<EnclosingSymbol> {
    CONTAINMENT_RULES
        .into_iter()
        .find_map(|rule| descend(symbols, item, rule))
}

/// Which of several siblings admitting an item is the innermost: the latest
/// start, then the earliest end, then the name and kind, so the choice does
/// not depend on the server's sibling order.
fn sibling_order(symbol: &Symbol) -> impl Ord {
    (
        symbol.range.start,
        Reverse(symbol.range.end),
        Reverse(symbol.name.as_str()),
        Reverse(symbol.kind),
    )
}

fn innermost_sibling<'s>(
    level: &'s [Symbol],
    item: &Range,
    rule: Containment,
) -> Option<&'s Symbol> {
    level
        .iter()
        .filter(|symbol| rule.admits(&symbol.range, item))
        .max_by_key(|symbol| sibling_order(symbol))
}

/// The chain of nested symbols admitting `item` under `rule`, outermost first:
/// the innermost admitting sibling at each level, then the same inside its
/// children. Empty when no top-level symbol admits `item`.
pub(super) fn descend_path(
    symbols: &[Symbol],
    item: Range,
    rule: Containment,
) -> impl Iterator<Item = &Symbol> {
    std::iter::successors(innermost_sibling(symbols, &item, rule), move |symbol| {
        innermost_sibling(symbol.children.as_deref().unwrap_or_default(), &item, rule)
    })
}

fn descend(symbols: &[Symbol], item: &Range, rule: Containment) -> Option<EnclosingSymbol> {
    let path: Vec<&Symbol> = descend_path(symbols, *item, rule).collect();
    let innermost = path.last()?;
    Some(EnclosingSymbol {
        name_path: path.iter().map(|symbol| symbol.name.clone()).collect(),
        kind: innermost.kind,
        range: innermost.range,
        fidelity: SymbolFidelity::Hierarchical,
    })
}

pub(super) fn innermost_flat(symbols: &[FlatSymbol], item: &Range) -> Option<EnclosingSymbol> {
    CONTAINMENT_RULES.into_iter().find_map(|rule| {
        symbols
            .iter()
            .filter(|flat| rule.admits(&flat.symbol.range, item))
            .max_by_key(|flat| sibling_order(&flat.symbol))
            .map(|flat| EnclosingSymbol {
                name_path: flat
                    .container_name
                    .iter()
                    .filter(|container| !container.is_empty())
                    .cloned()
                    .chain([flat.symbol.name.clone()])
                    .collect(),
                kind: flat.symbol.kind,
                range: flat.symbol.range,
                fidelity: SymbolFidelity::Flat,
            })
    })
}

pub(super) fn outcome_for(tree: &DocumentSymbolTree, item: &Range) -> EnclosingSymbolOutcome {
    let found = match tree {
        DocumentSymbolTree::Hierarchical(symbols) => innermost_hierarchical(symbols, item),
        DocumentSymbolTree::Flat(symbols) => innermost_flat(symbols, item),
    };
    found.map_or(
        EnclosingSymbolOutcome::TopLevel,
        EnclosingSymbolOutcome::Resolved,
    )
}

/// An item to resolve: the URI string of its file and its normalized range.
pub(super) struct Hit<'a> {
    pub(super) uri: &'a str,
    pub(super) range: &'a Range,
    /// The link target range of a go-to answer, when the source pass needs it.
    pub(super) target_range: Option<Range>,
}

/// What one file contributed to a call's enrichment.
enum FileResolution {
    Tree(DocumentSymbolTree),
    Skipped(EnclosingSymbolOutcome),
}

/// What the pass found for one item: its enclosing symbol and, when the source
/// pass ran, its source.
struct ItemResolution {
    enclosing: EnclosingSymbolOutcome,
    source: Option<SourceOutcome>,
}

/// Outcomes for every hit, in input order, plus the call-level summary.
struct Resolution {
    outcomes: Vec<ItemResolution>,
    summary: EnrichmentSummary,
    positions_degraded: Option<PositionDegradation>,
}

const fn skipped_outcome_for(error: &Error) -> EnclosingSymbolOutcome {
    match error {
        Error::CapabilityNotSupported { .. } => EnclosingSymbolOutcome::Unavailable {
            reason: UnavailableReason::CapabilityAbsent,
        },
        Error::DocumentLimitExceeded { .. } => EnclosingSymbolOutcome::NotComputed {
            reason: NotComputedReason::TrackerLimit,
        },
        Error::Timeout(_) => EnclosingSymbolOutcome::Unavailable {
            reason: UnavailableReason::TimedOut,
        },
        _ => EnclosingSymbolOutcome::Unavailable {
            reason: UnavailableReason::RequestFailed,
        },
    }
}

const fn not_computed(reason: NotComputedReason) -> FileResolution {
    FileResolution::Skipped(EnclosingSymbolOutcome::NotComputed { reason })
}

impl Translator {
    /// Attaches enclosing symbols, and for [`LocationContext::SymbolSource`]
    /// source text, to `locations` when `context` asks for it.
    ///
    /// `rule` picks the span a source is cut from.
    pub(super) async fn contextualize_locations(
        &self,
        locations: Vec<NormalizedLocation>,
        context: LocationContext,
        rule: SpanRule,
        positions_degraded: Option<PositionDegradation>,
    ) -> Contextualized<NavigationLocation> {
        let enclosing = match context {
            LocationContext::None => ResultContext::None,
            LocationContext::EnclosingSymbol | LocationContext::SymbolSource => {
                ResultContext::EnclosingSymbol
            }
        };
        let source_rule = (context == LocationContext::SymbolSource).then_some(rule);
        self.contextualize_items(
            locations,
            &(),
            enclosing,
            source_rule,
            |(), item| Hit {
                uri: &item.location.uri,
                range: &item.location.range,
                target_range: item.target_range,
            },
            |item, source| NavigationLocation {
                location: item.location,
                source,
            },
        )
        .await
        .with_input_degradation(positions_degraded)
    }

    /// Attaches enclosing symbols to the diagnostics of the file `uri` when
    /// `context` asks for it.
    pub(super) async fn contextualize_diagnostics(
        &self,
        uri: &str,
        diagnostics: Vec<Diagnostic>,
        context: ResultContext,
        positions_degraded: Option<PositionDegradation>,
    ) -> Contextualized<Diagnostic> {
        self.contextualize_items(
            diagnostics,
            uri,
            context,
            None,
            |uri, diagnostic| Hit {
                uri,
                range: &diagnostic.range,
                target_range: None,
            },
            |diagnostic, _| diagnostic,
        )
        .await
        .with_input_degradation(positions_degraded)
    }

    /// `shared` is data every hit borrows from besides its own item, such as
    /// the one URI of a file's diagnostics. `finish` builds each output item
    /// from its input and, when `source_rule` is set, its [`SourceOutcome`].
    async fn contextualize_items<I: Send, T, S: Sync + ?Sized>(
        &self,
        items: Vec<I>,
        shared: &S,
        context: ResultContext,
        source_rule: Option<SpanRule>,
        hit: impl for<'a> Fn(&'a S, &'a I) -> Hit<'a> + Sync,
        finish: impl Fn(I, Option<SourceOutcome>) -> T,
    ) -> Contextualized<T> {
        if context == ResultContext::None || items.is_empty() {
            return Contextualized {
                items: items
                    .into_iter()
                    .map(|item| Contextual::from(finish(item, None)))
                    .collect(),
                enrichment: None,
                positions_degraded: None,
            };
        }
        let hits: Vec<Hit<'_>> = items.iter().map(|item| hit(shared, item)).collect();
        let resolution = self.resolve_enclosing(&hits, source_rule).await;
        let items = items
            .into_iter()
            .zip(resolution.outcomes)
            .map(|(item, resolved)| Contextual {
                inner: finish(item, resolved.source),
                enclosing_symbol: Some(resolved.enclosing),
            })
            .collect();
        Contextualized {
            items,
            enrichment: Some(resolution.summary),
            positions_degraded: resolution.positions_degraded,
        }
    }

    async fn resolve_enclosing<'a>(
        &self,
        hits: &'a [Hit<'a>],
        source_rule: Option<SpanRule>,
    ) -> Resolution {
        let cap = enrichment_file_cap(self.document_tracker.limits().max_documents);
        let started = Instant::now();
        let mut attempted = 0_usize;
        let mut positions_degraded = None;
        let mut files: HashMap<&str, FileResolution> = HashMap::new();
        let mut pass = source_rule.map(|rule| SourcePass::new(rule, hits.len()));

        for hit in hits {
            if files.contains_key(hit.uri) {
                continue;
            }
            let resolution = self.resolve_file(
                hit.uri,
                cap,
                &mut attempted,
                started,
                &mut positions_degraded,
                pass.as_mut().map(|pass| (pass, hits)),
            );
            let resolution = Box::pin(resolution).await;
            files.insert(hit.uri, resolution);
        }

        let outcomes = hits
            .iter()
            .enumerate()
            .map(|(index, hit)| ItemResolution {
                enclosing: match files.get(hit.uri) {
                    Some(FileResolution::Tree(tree)) => outcome_for(tree, hit.range),
                    Some(FileResolution::Skipped(outcome)) => outcome.clone(),
                    None => EnclosingSymbolOutcome::Unavailable {
                        reason: UnavailableReason::RequestFailed,
                    },
                },
                source: pass.as_mut().map(|pass| pass.take(index)),
            })
            .collect();

        let files_enriched = files
            .values()
            .filter(|file| matches!(file, FileResolution::Tree(_)))
            .count();
        let cut_short = files.values().any(|file| {
            matches!(
                file,
                FileResolution::Skipped(EnclosingSymbolOutcome::NotComputed {
                    reason: NotComputedReason::FileCap | NotComputedReason::Deadline
                })
            )
        });
        Resolution {
            outcomes,
            summary: EnrichmentSummary {
                files_enriched,
                files_skipped: files.len().saturating_sub(files_enriched),
                cut_short,
            },
            positions_degraded,
        }
    }

    /// Resolves one file's symbol tree. The URI is the server's text, so it
    /// is validated against the workspace roots before the file is opened.
    async fn resolve_file<'a>(
        &self,
        uri: &str,
        cap: usize,
        attempted: &mut usize,
        started: Instant,
        positions_degraded: &mut Option<PositionDegradation>,
        mut source: Option<(&mut SourcePass<'a>, &'a [Hit<'a>])>,
    ) -> FileResolution {
        if *attempted >= cap {
            return not_computed(NotComputedReason::FileCap);
        }
        let remaining = ENRICHMENT_DEADLINE.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return not_computed(NotComputedReason::Deadline);
        }
        let Ok(path) = self.parse_file_uri(&lsp_types::Uri::from(uri)).await else {
            return not_computed(NotComputedReason::OutOfWorkspace);
        };
        *attempted = attempted.saturating_add(1);

        let lookup = async {
            let doc = self
                .prepare_gated_document_for_path(
                    &path,
                    IndexingGate::FileLocal(FileLocalCapability::DocumentSymbols),
                )
                .await?;
            let stamp = self.document_tracker.stamp(doc.path(), doc.server_id());
            let FetchedSymbols { doc, ctx, response } = self.fetch_document_symbols(doc).await?;
            let tree = symbol_tree(response, &ctx, doc.uri()).await;
            if let Some((pass, hits)) = source.as_mut() {
                let file = SourceFile {
                    tracker: &self.document_tracker,
                    path: doc.path(),
                    server: doc.server_id(),
                    stamp,
                };
                let in_file = hits.iter().enumerate().filter(|(_, hit)| hit.uri == uri);
                pass.fill_file(&file, &tree, in_file);
            }
            Ok((tree, ctx.positions_degraded()))
        };
        match tokio::time::timeout(remaining, lookup).await {
            Err(_elapsed) => FileResolution::Skipped(EnclosingSymbolOutcome::Unavailable {
                reason: UnavailableReason::TimedOut,
            }),
            Ok(Err(error)) => FileResolution::Skipped(skipped_outcome_for(&error)),
            Ok(Ok((tree, degraded))) => {
                *positions_degraded = (*positions_degraded).max(degraded);
                FileResolution::Tree(tree)
            }
        }
    }
}

impl ServerText for EnclosingSymbol {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            name_path,
            kind: _,
            range: _,
            fidelity: _,
        } = self;
        for segment in name_path {
            redactions.note_payload(segment);
        }
    }
}

impl ServerText for EnclosingSymbolOutcome {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        match self {
            Self::Resolved(symbol) => symbol.redact_server_text(redactions),
            Self::TopLevel | Self::NotComputed { reason: _ } | Self::Unavailable { reason: _ } => {}
        }
    }
}

impl<T: ServerText> ServerText for Contextual<T> {
    fn redact_server_text(&mut self, redactions: &Redactions) {
        let Self {
            inner,
            enclosing_symbol,
        } = self;
        inner.redact_server_text(redactions);
        enclosing_symbol.redact_server_text(redactions);
    }
}

#[cfg(test)]
mod contextual_tests {
    use super::*;

    #[test]
    fn test_contextual_serializes_flat_for_any_inner_type() {
        let item = Contextual {
            inner: serde_json::json!({"name": "f"}),
            enclosing_symbol: Some(EnclosingSymbolOutcome::TopLevel),
        };
        let value = serde_json::to_value(&item).unwrap();
        assert_eq!(value["name"], "f");
        assert_eq!(value["enclosing_symbol"]["status"], "top_level");

        let plain = Contextual::from(serde_json::json!({"name": "f"}));
        assert!(
            serde_json::to_value(&plain)
                .unwrap()
                .get("enclosing_symbol")
                .is_none()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enrichment_file_cap() {
        assert_eq!(enrichment_file_cap(DocumentLimit::UNLIMITED), 16);
        assert_eq!(enrichment_file_cap(DocumentLimit::new(1)), 1);
        assert_eq!(enrichment_file_cap(DocumentLimit::new(3)), 1);
        assert_eq!(enrichment_file_cap(DocumentLimit::new(8)), 2);
        assert_eq!(enrichment_file_cap(DocumentLimit::new(64)), 16);
        assert_eq!(enrichment_file_cap(DocumentLimit::new(10_000)), 16);
    }
}

#[cfg(test)]
mod server_text_tests {
    use super::super::dto::Position2D;
    use super::*;

    #[test]
    fn test_enclosing_symbol_name_path_passes_through() {
        let secret = "SuperSecretValue123";
        let set = Redactions::new([("API_TOKEN".to_owned(), secret.to_owned())]);
        let mut outcome = EnclosingSymbolOutcome::Resolved(EnclosingSymbol {
            name_path: vec![format!("mod_{secret}"), "f".to_owned()],
            kind: 12,
            range: Range {
                start: Position2D::at(1, 1),
                end: Position2D::at(1, 2),
            },
            fidelity: SymbolFidelity::Hierarchical,
        });
        let before = outcome.clone();

        outcome.redact_server_text(&set);

        assert_eq!(outcome, before);
    }
}
