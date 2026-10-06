//! Size bounds the cache applies to every diagnostics list it stores, pushed or
//! pulled.

use lsp_types::{Diagnostic as LspDiagnostic, Uri};
use tracing::warn;

use super::message_as_str;
use crate::util::{truncate_str, truncate_string};

/// Maximum size, in bytes, of a single cached log message, server message,
/// or a single diagnostic's free-form `message` text.
///
/// `MAX_LOG_ENTRIES`/`MAX_SERVER_MESSAGES`/`MAX_DIAGNOSTIC_ENTRIES` bound
/// the *number* of cached entries, but not the size of any one entry -- a
/// spawned LSP server could publish a single pathologically large message
/// and still fit under those caps while consuming unbounded memory (#311).
/// This is independent of the transport-level `MAX_CONTENT_LENGTH` cap in
/// `lsp::transport`, which bounds a whole JSON-RPC frame, not one field
/// within it. 256 KiB comfortably fits any realistic diagnostic or log
/// message while still capping the worst case.
///
/// This alone does not bound a whole diagnostics *entry* (a
/// `Vec<LspDiagnostic>`), only one diagnostic's `message` field -- see
/// `MAX_DIAGNOSTICS_ENTRY_BYTES` for the entry-level cap.
pub(super) const MAX_ENTRY_TEXT_BYTES: usize = 256 * 1024;

/// Maximum serialized size, in bytes, of a single document's *whole*
/// diagnostics list (`Vec<LspDiagnostic>`), enforced by
/// [`cap_diagnostics_entry_size`].
///
/// `MAX_ENTRY_TEXT_BYTES` alone does not bound this: it only truncates one
/// diagnostic's `message` field, but the list's *length* is uncapped, and
/// `LspDiagnostic` carries several more free-form or arbitrary-JSON fields
/// besides `message` (`source`, `code`, `code_description`,
/// `related_information`, `data`). A hostile server can stay under
/// `MAX_ENTRY_TEXT_BYTES` on every individual message while still
/// publishing e.g. 100k diagnostics for one URI, or a single diagnostic
/// with a multi-MiB `data` blob -- both still fit under the transport-level
/// `lsp::transport::MAX_CONTENT_LENGTH` (10 MiB) per notification, and
/// `MAX_DIAGNOSTIC_ENTRIES` bounds only the *number* of distinct cached
/// URIs, not their individual size, so up to 1000 such entries could
/// otherwise accumulate to gigabytes. 1 MiB is far larger than any
/// realistic diagnostics list for one file, and combined with
/// `MAX_DIAGNOSTIC_ENTRIES` bounds the cache's total diagnostics footprint
/// to roughly 1 GiB in the worst case.
pub(super) const MAX_DIAGNOSTICS_ENTRY_BYTES: usize = 1024 * 1024;

/// Diagnostics bounded the way the cache stores them: messages truncated and
/// the list capped, so a pull report cannot skip the limits a push obeys.
#[derive(Debug)]
pub struct BoundedDiagnostics(pub(super) Vec<LspDiagnostic>);

impl BoundedDiagnostics {
    /// Truncates each message to `MAX_ENTRY_TEXT_BYTES` and bounds the list to
    /// `MAX_DIAGNOSTICS_ENTRY_BYTES`, keeping the most severe diagnostics when
    /// the list has to be cut.
    pub(crate) fn new(uri: &Uri, mut diagnostics: Vec<LspDiagnostic>) -> Self {
        for diagnostic in &mut diagnostics {
            let placeholder = lsp_types::Message::String(String::new());
            diagnostic.message = truncate_message(
                std::mem::replace(&mut diagnostic.message, placeholder),
                MAX_ENTRY_TEXT_BYTES,
            );
        }
        cap_diagnostics_entry_size(uri, &mut diagnostics);
        Self(diagnostics)
    }
}

/// Conservative fixed-field/JSON-structure overhead assumed per diagnostic
/// (`range`, `severity`, and object/field-name punctuation) by
/// [`cap_diagnostics_entry_size`]'s cheap size estimate. Deliberately
/// generous relative to the true overhead (`range` alone serializes to
/// roughly 70 bytes) so the estimate can only ever *overcount*, never
/// undercount, actual serialized size.
const DIAGNOSTIC_ESTIMATE_OVERHEAD_BYTES: usize = 256;

/// Worst-case JSON string-escaping expansion factor, applied to each raw
/// string field's byte length in [`cap_diagnostics_entry_size`]'s cheap
/// size estimate.
///
/// A raw byte's serialized JSON form is at most 6 bytes: `"` and `\` and
/// the five control characters with a short escape (`\b \f \n \r \t`) cost
/// 2 bytes, but every other control character (`U+0000`..=`U+001F`, e.g.
/// NUL) has no short escape and is emitted as `\u00XX` -- 6 bytes for 1 raw
/// byte. The original estimate summed raw string lengths directly and
/// could *undercount* an escape-heavy string (e.g. all-NUL) by up to this
/// factor, letting an oversized entry skip the real `fits` check
/// entirely -- multiplying by it keeps the estimate a true upper bound on
/// serialized size rather than merely a typical-case guess.
const JSON_ESCAPE_WORST_CASE_FACTOR: usize = 6;

/// Last-resort message length used by [`cap_diagnostics_entry_size`]'s
/// terminal-enforcement fallback -- small enough that a single diagnostic
/// (fixed-size `range`/`severity` plus this one short string, every other
/// field cleared) can never approach [`MAX_DIAGNOSTICS_ENTRY_BYTES`]
/// regardless of JSON encoding overhead.
pub(super) const DIAGNOSTIC_TERMINAL_FALLBACK_MESSAGE_BYTES: usize = 1024;

/// Ordinal rank used to sort diagnostics by severity before
/// [`cap_diagnostics_entry_size`] truncates an oversized list -- lower rank
/// sorts first, so it is kept preferentially (#311 S6).
///
/// `DiagnosticSeverity`'s inner value is private, so its natural numeric
/// ordering (`ERROR` < `WARNING` < `INFORMATION` < `HINT`) can't be read
/// directly; `Option<DiagnosticSeverity>`'s *derived* `Ord` would also rank
/// `None` before every `Some` value, the opposite of what's wanted here
/// (no reported severity is treated as least important, same as `HINT`).
/// This maps explicitly instead of relying on either.
pub(super) const fn diagnostic_severity_rank(diagnostic: &LspDiagnostic) -> u8 {
    match diagnostic.severity {
        Some(lsp_types::DiagnosticSeverity::Error) => 0,
        Some(lsp_types::DiagnosticSeverity::Warning) => 1,
        Some(lsp_types::DiagnosticSeverity::Information) => 2,
        // An unrecognized (future) severity value is treated the same as
        // no severity at all: least important, not most.
        Some(_) | None => 3,
    }
}

/// Largest `k` such that `fits(&diagnostics[..k])`, found via binary search
/// rather than a linear scan or a flat halve (#311 S6).
///
/// Correct because a JSON array's serialized length is monotonically
/// non-decreasing in its element count -- appending a diagnostic can only
/// add bytes, never remove them -- so `fits(&diagnostics[..k])` is `true`
/// for a contiguous run of small `k` and `false` for every larger `k`,
/// exactly the shape a boundary binary search requires. `fits(&[])` is
/// always `true`, so the search is well-defined even if no diagnostic at
/// all fits individually.
fn largest_fitting_prefix(
    diagnostics: &[LspDiagnostic],
    fits: impl Fn(&[LspDiagnostic]) -> bool,
) -> usize {
    let (mut lo, mut hi) = (0usize, diagnostics.len());
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        reason = "binary search over 0..=len: lo < hi gives mid in 1..=hi"
    )]
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(&diagnostics[..mid]) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// Truncates a diagnostic's free-form `message` to at most `max_bytes`,
/// regardless of whether it is a plain string or `MarkupContent`.
pub(super) fn truncate_message(
    message: lsp_types::Message,
    max_bytes: usize,
) -> lsp_types::Message {
    match message {
        lsp_types::Message::String(s) => lsp_types::Message::String(truncate_string(s, max_bytes)),
        lsp_types::Message::MarkupContent(mut m) => {
            m.value = truncate_string(m.value, max_bytes);
            lsp_types::Message::MarkupContent(m)
        }
    }
}

/// Bounds `diagnostics`' serialized size to at most
/// `MAX_DIAGNOSTICS_ENTRY_BYTES` (#311 C1 fix).
///
/// Measures the list's *actual* serialized size via `serde_json::to_vec`
/// rather than bounding each field individually -- that covers every
/// field on `LspDiagnostic` (`source`, `code`, `code_description`,
/// `related_information`, `data`, `tags`) at once, not just `message`.
///
/// # Guarantee
///
/// The postcondition -- the returned list's serialized size is at most
/// `MAX_DIAGNOSTICS_ENTRY_BYTES` -- is enforced directly by a final,
/// unconditional check at the end of this function, not merely assumed to
/// follow from the field-specific mitigations below it. Those mitigations
/// are best-effort (preserve as much real content as fits) and only cover
/// the fields known today; the terminal step is what actually guarantees
/// the bound holds even if a mitigation is incomplete or `LspDiagnostic`
/// gains a new unbounded field in a future `lsp-types` upgrade.
///
/// # Cost (#311 S5)
///
/// `publishDiagnostics` is a hot path (rust-analyzer republishes
/// whole-workspace diagnostics on every save), so this avoids a full
/// `serde_json` serialization pass whenever every diagnostic's size is
/// cheaply accountable from `message`/`source`/`code` alone (i.e. none
/// carry `data`, `code_description`, `related_information`, or `tags`,
/// each of which needs real serialization to size safely) and a
/// conservative *upper bound* on their sum already fits. The estimate is
/// not their raw byte length: JSON string escaping can expand a byte up to
/// [`JSON_ESCAPE_WORST_CASE_FACTOR`]-fold (a NUL-heavy string previously
/// let this fast path undercount actual serialized size by that much and
/// skip the real `fits` check below entirely), so raw lengths are
/// multiplied by that factor before comparing against the cap.
///
/// # Visibility (#311 S7)
///
/// Every mitigation that drops or truncates real content -- discarding
/// diagnostics entirely, or clearing a survivor's `data` (which the LSP
/// spec says is preserved through to a later `textDocument/codeAction`
/// request, so losing it can silently break that diagnostic's quick fix)
/// -- logs a `tracing::warn!` so the degradation is visible rather than a
/// silent, hard-to-diagnose gap in what a caller sees.
pub(super) fn cap_diagnostics_entry_size(uri: &Uri, diagnostics: &mut Vec<LspDiagnostic>) {
    let fits = |ds: &[LspDiagnostic]| {
        // A serialization error is conservatively treated as "does not
        // fit" (triggers the mitigations below) rather than as success.
        // `LspDiagnostic`'s fields can't actually produce one in practice
        // (no floats, no non-string map keys anywhere in `Diagnostic` or
        // `serde_json::Value`'s own object representation), but failing
        // safe costs nothing here.
        serde_json::to_vec(ds).is_ok_and(|bytes| bytes.len() <= MAX_DIAGNOSTICS_ENTRY_BYTES)
    };

    let cheaply_estimable = diagnostics.iter().all(|d| {
        d.data.is_none()
            && d.code_description.is_none()
            && d.related_information.is_none()
            && d.tags.is_none()
    });
    if cheaply_estimable {
        let estimated: usize = diagnostics
            .iter()
            .map(|d| {
                let raw_string_bytes = message_as_str(&d.message)
                    .len()
                    .saturating_add(d.source.as_deref().map_or(0, str::len))
                    .saturating_add(match &d.code {
                        Some(lsp_types::Code::String(s)) => s.len(),
                        _ => 0,
                    });
                raw_string_bytes
                    .saturating_mul(JSON_ESCAPE_WORST_CASE_FACTOR)
                    .saturating_add(DIAGNOSTIC_ESTIMATE_OVERHEAD_BYTES)
            })
            .sum();
        if estimated <= MAX_DIAGNOSTICS_ENTRY_BYTES {
            return;
        }
    }

    if fits(diagnostics) {
        return;
    }

    let original_count = diagnostics.len();

    // Prefer dropping lower-severity diagnostics first (a stable sort, so
    // same-severity diagnostics keep their original -- typically
    // file-position -- relative order), then keep the largest prefix that
    // actually fits rather than a flat halve, which both overshoots (a
    // list one byte over the cap would otherwise lose half its
    // diagnostics) and was severity-blind (would keep hundreds of leading
    // HINT-level noise over a later ERROR). At least one diagnostic is
    // always kept here so the mitigations below have a survivor to act on.
    diagnostics.sort_by_key(diagnostic_severity_rank);
    let keep = largest_fitting_prefix(diagnostics, fits).max(1);
    diagnostics.truncate(keep);
    if diagnostics.len() < original_count {
        warn!(
            "diagnostics for {} exceeded the {MAX_DIAGNOSTICS_ENTRY_BYTES}-byte cache cap; kept \
             the {} highest-severity of {original_count} diagnostics",
            uri.as_ref(),
            diagnostics.len(),
        );
    }

    // Drop opaque/structured fields first -- cheap, and often enough on
    // its own (e.g. the single-huge-`data`-blob shape).
    if diagnostics.len() == 1 && !fits(diagnostics) {
        #[allow(clippy::indexing_slicing, reason = "len == 1 checked above")]
        let diagnostic = &mut diagnostics[0];
        let had_data = diagnostic.data.is_some();
        diagnostic.data = None;
        diagnostic.code_description = None;
        diagnostic.related_information = None;
        diagnostic.tags = None;
        warn!(
            "diagnostic for {} exceeded the cache cap; dropped its data/code_description/\
             related_information/tags fields{}",
            uri.as_ref(),
            if had_data {
                " (a later code-action request for this diagnostic may not resolve its quick fix)"
            } else {
                ""
            },
        );
    }

    // Still oversized: `source`/`code` (plain strings, unlike the opaque
    // fields above) are truncated rather than dropped, to preserve some
    // content.
    if diagnostics.len() == 1 && !fits(diagnostics) {
        #[allow(clippy::indexing_slicing, reason = "len == 1 checked above")]
        let diagnostic = &mut diagnostics[0];
        if let Some(source) = &diagnostic.source {
            diagnostic.source = Some(truncate_str(source, MAX_ENTRY_TEXT_BYTES));
        }
        if let Some(lsp_types::Code::String(code)) = &diagnostic.code {
            diagnostic.code = Some(lsp_types::Code::String(truncate_str(
                code,
                MAX_ENTRY_TEXT_BYTES,
            )));
        }
    }

    // Terminal enforcement: guarantee the postcondition directly rather
    // than trusting the mitigations above to have covered every case --
    // see this function's doc.
    if !fits(diagnostics) {
        diagnostics.truncate(1);
        if let Some(diagnostic) = diagnostics.first_mut() {
            let placeholder = lsp_types::Message::String(String::new());
            diagnostic.message = truncate_message(
                std::mem::replace(&mut diagnostic.message, placeholder),
                DIAGNOSTIC_TERMINAL_FALLBACK_MESSAGE_BYTES,
            );
            diagnostic.source = None;
            diagnostic.code = None;
            diagnostic.code_description = None;
            diagnostic.related_information = None;
            diagnostic.tags = None;
            diagnostic.data = None;
        }
        warn!(
            "diagnostic for {} still exceeded the cache cap after every other mitigation; \
             truncated its message to {DIAGNOSTIC_TERMINAL_FALLBACK_MESSAGE_BYTES} bytes and \
             cleared all other fields",
            uri.as_ref(),
        );
    }
}
