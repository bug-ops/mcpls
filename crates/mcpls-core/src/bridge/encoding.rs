//! Position encoding conversion utilities.
//!
//! Handles conversion between MCP (1-based) and LSP (0-based) positions,
//! as well as UTF-8/UTF-16/UTF-32 encoding conversions.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::translator::{Position, Position2D};
use crate::spelling::{Spelled, spelled_enum};

spelled_enum! {
    /// Supported position encodings per LSP 3.17.
    ///
    /// Serializes as the LSP kind string (`"utf-8"`, `"utf-16"`, `"utf-32"`) and
    /// rejects any other string on deserialize.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::bridge::PositionEncoding;
    ///
    /// assert_eq!(PositionEncoding::from_lsp("utf-32"), Some(PositionEncoding::Utf32));
    /// assert!(PositionEncoding::try_from("utf-7".to_string()).is_err());
    /// ```
    #[derive(Default, Serialize, Deserialize)]
    #[serde(try_from = "String", into = "&'static str")]
    PositionEncoding {
        /// UTF-8 code units.
        #[default]
        Utf8 => "utf-8",
        /// UTF-16 code units (LSP default).
        Utf16 => "utf-16",
        /// UTF-32 code units (Unicode code points).
        Utf32 => "utf-32",
    }
}

/// A string that is not an LSP position encoding kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidPositionEncoding(String);

impl fmt::Display for InvalidPositionEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid position encoding '{}'; expected one of ",
            self.0
        )?;
        for (index, encoding) in PositionEncoding::ALL.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "\"{encoding}\"")?;
        }
        Ok(())
    }
}

impl std::error::Error for InvalidPositionEncoding {}

impl PositionEncoding {
    /// Parse from LSP position encoding kind string.
    #[must_use]
    pub fn from_lsp(kind: &str) -> Option<Self> {
        Self::from_spelling(kind)
    }

    /// Convert to the [`lsp_types::PositionEncodingKind`] sent in `initialize`.
    #[must_use]
    pub const fn to_kind(self) -> lsp_types::PositionEncodingKind {
        match self {
            Self::Utf8 => lsp_types::PositionEncodingKind::UTF8,
            Self::Utf16 => lsp_types::PositionEncodingKind::UTF16,
            Self::Utf32 => lsp_types::PositionEncodingKind::UTF32,
        }
    }
}

impl TryFrom<String> for PositionEncoding {
    type Error = InvalidPositionEncoding;

    fn try_from(kind: String) -> Result<Self, Self::Error> {
        Self::from_lsp(&kind).ok_or(InvalidPositionEncoding(kind))
    }
}

impl From<PositionEncoding> for &'static str {
    fn from(encoding: PositionEncoding) -> Self {
        encoding.as_str()
    }
}

/// Whether a column conversion preserved the exact character the caller named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnFidelity {
    /// The column is correct in the target encoding.
    Exact,
    /// The column could not be converted and was passed through as-is.
    PassedThrough,
}

/// A converted value together with whether its column conversion was exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct Converted<T> {
    /// The converted value.
    pub value: T,
    /// Whether the column inside `value` is exact in the target encoding.
    pub fidelity: ColumnFidelity,
}

impl<T> Converted<T> {
    const fn exact(value: T) -> Self {
        Self {
            value,
            fidelity: ColumnFidelity::Exact,
        }
    }

    const fn passed_through(value: T) -> Self {
        Self {
            value,
            fidelity: ColumnFidelity::PassedThrough,
        }
    }
}

/// Convert MCP position (1-based) to LSP position (0-based), translating the
/// character column into `encoding`'s units.
///
/// MCP character columns are defined in UTF-16 code units -- the LSP default
/// and what nearly every server negotiates -- so `PositionEncoding::Utf16`
/// is a pure line/column offset with no further work, and column 0 is
/// identical in every encoding. Otherwise `line_text` (the exact text of the
/// target 0-based LSP line, without a line terminator) is used to re-derive
/// the column in `encoding`'s units; a column past the end of the line clamps
/// to the line length, per LSP 3.17. If `line_text` is unavailable (e.g. the
/// file could not be read) or the column lands inside a multi-unit
/// character, the raw MCP character is used unconverted rather than failing
/// the request, and the result's [`Converted::fidelity`] is
/// [`ColumnFidelity::PassedThrough`].
pub fn mcp_to_lsp_position(
    position: Position,
    line_text: Option<&str>,
    encoding: PositionEncoding,
) -> Converted<lsp_types::Position> {
    let column = convert_column(
        position.lsp_character(),
        line_text,
        PositionEncoding::Utf16,
        encoding,
    );
    Converted {
        value: lsp_types::Position {
            line: position.lsp_line(),
            character: column.value,
        },
        fidelity: column.fidelity,
    }
}

/// Convert LSP position (0-based, in `encoding`'s units) to MCP position
/// (1-based, UTF-16 code units).
///
/// The inverse of [`mcp_to_lsp_position`]; see its docs for the fast path
/// and fallback behavior. `character == u32::MAX`, the LSP "end of line"
/// idiom, is always [`ColumnFidelity::Exact`] and returned unchanged.
pub fn lsp_to_mcp_position(
    pos: lsp_types::Position,
    line_text: Option<&str>,
    encoding: PositionEncoding,
) -> Converted<Position2D> {
    let column = if pos.character == u32::MAX {
        Converted::exact(u32::MAX)
    } else {
        convert_column(pos.character, line_text, encoding, PositionEncoding::Utf16)
    };

    Converted {
        value: Position2D {
            line: pos.line.saturating_add(1),
            character: column.value.saturating_add(1),
        },
        fidelity: column.fidelity,
    }
}

/// Re-derive a 0-based `column` from `from` units into `to` units, deciding
/// fidelity in one place for both conversion directions.
fn convert_column(
    column: u32,
    line_text: Option<&str>,
    from: PositionEncoding,
    to: PositionEncoding,
) -> Converted<u32> {
    if from == to || column == 0 {
        return Converted::exact(column);
    }
    let Some(text) = line_text else {
        return Converted::passed_through(column);
    };

    let line_len_in = |encoding| {
        EncodingConverter::new(encoding)
            .byte_offset_to_character(text, text.len())
            .ok()
    };

    let located = EncodingConverter::new(from).character_to_byte_offset(text, column);
    if let Ok(byte_offset) = located
        && let Some(converted) = exact_character(text, byte_offset, column, from, to)
    {
        return Converted::exact(converted);
    }
    if matches!(located, Err(OffsetError::PastEnd { .. }))
        && let Some(clamped) = line_len_in(to)
    {
        // A server column past its own line end means its text differs from ours.
        return if from == PositionEncoding::Utf16 {
            Converted::exact(clamped)
        } else {
            Converted {
                value: clamped,
                fidelity: ColumnFidelity::PassedThrough,
            }
        };
    }
    Converted::passed_through(column)
}

/// Re-express `byte_offset` in `to` units, requiring that `column` in `from`
/// units named exactly that offset.
///
/// `EncodingConverter::character_to_byte_offset` finds the byte boundary at
/// or after the requested offset, so a column inside a multi-unit character
/// (e.g. a UTF-16 surrogate pair) resolves to the *next* boundary instead of
/// erroring. Round-tripping through `from` detects that case, and `None`
/// tells the caller to fall back rather than use a rounded-forward position.
fn exact_character(
    text: &str,
    byte_offset: usize,
    column: u32,
    from: PositionEncoding,
    to: PositionEncoding,
) -> Option<u32> {
    let round_trip = EncodingConverter::new(from)
        .byte_offset_to_character(text, byte_offset)
        .ok()?;
    if round_trip != column {
        return None;
    }
    EncodingConverter::new(to)
        .byte_offset_to_character(text, byte_offset)
        .ok()
}

/// Byte offsets for a chosen set of code-unit offsets into one label,
/// resolved in a single pass over the label.
///
/// Resolves signature-help parameter labels given as offset pairs
/// (`ParameterInformationLabel::Tuple`) in the negotiated encoding's units.
/// Per-pair scans would cost O(label) each, which a server naming many
/// parameters could amplify; a full per-unit table would cost memory
/// proportional to the label. This keeps one entry per distinct requested
/// offset.
#[derive(Debug)]
pub struct LabelOffsets<'a> {
    label: &'a str,
    /// `(unit offset, byte offset)` sorted by unit; the byte offset is `None`
    /// for an offset out of range or inside a multi-unit character.
    resolved: Vec<(u32, Option<usize>)>,
}

impl<'a> LabelOffsets<'a> {
    /// Resolves each of `offsets` (code units in `encoding`) against `label`.
    #[must_use]
    pub fn new(
        label: &'a str,
        offsets: impl IntoIterator<Item = u32>,
        encoding: PositionEncoding,
    ) -> Self {
        let mut wanted: Vec<u32> = offsets.into_iter().collect();
        wanted.sort_unstable();
        wanted.dedup();

        let mut resolved = Vec::with_capacity(wanted.len());
        let mut next = 0;
        let mut settle = |unit: usize, byte: usize| {
            while let Some(&offset) = wanted.get(next) {
                if offset as usize > unit {
                    break;
                }
                resolved.push((offset, (offset as usize == unit).then_some(byte)));
                next = next.saturating_add(1);
            }
        };
        let mut unit = 0;
        for (byte, ch) in label.char_indices() {
            settle(unit, byte);
            unit = unit.saturating_add(match encoding {
                PositionEncoding::Utf8 => ch.len_utf8(),
                PositionEncoding::Utf16 => ch.len_utf16(),
                PositionEncoding::Utf32 => 1,
            });
        }
        settle(unit, label.len());
        resolved.extend(
            wanted
                .get(next..)
                .unwrap_or_default()
                .iter()
                .map(|&offset| (offset, None)),
        );
        Self { label, resolved }
    }

    /// The substring of the indexed label between
    /// `start` (inclusive) and `end` (exclusive) code units. `None` when
    /// either offset was not among those requested, is out of range or inside
    /// a multi-unit character, or `start > end`.
    #[must_use]
    pub fn substring(&self, start: u32, end: u32) -> Option<&'a str> {
        let at = |unit: u32| {
            let index = self
                .resolved
                .binary_search_by_key(&unit, |&(offset, _)| offset)
                .ok()?;
            self.resolved.get(index)?.1
        };
        self.label.get(at(start)?..at(end)?)
    }
}

/// Why an offset has no counterpart in the other unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OffsetError {
    /// The offset is beyond the end of the text; `len` is the text length in
    /// the unit of the failed conversion's input.
    #[error("offset {offset} is past the end ({len})")]
    PastEnd {
        /// The requested offset.
        offset: usize,
        /// The text length in the same unit as `offset`.
        len: usize,
    },
    /// The byte offset is not on a character boundary.
    #[error("offset {0} is inside a character")]
    InsideCharacter(usize),
    /// The converted offset does not fit in `u32`.
    #[error("line longer than u32::MAX code units")]
    TooLong,
}

/// Position encoding converter for handling UTF-8/UTF-16/UTF-32 conversions.
///
/// Different LSP servers may use different character encodings. This converter
/// handles the conversion between byte offsets and character offsets based on
/// the negotiated encoding.
#[derive(Debug, Clone)]
pub struct EncodingConverter {
    encoding: PositionEncoding,
}

impl EncodingConverter {
    /// Create a new encoding converter with the specified encoding.
    #[must_use]
    pub const fn new(encoding: PositionEncoding) -> Self {
        Self { encoding }
    }

    /// Convert byte offset to character offset in the configured encoding.
    ///
    /// # Errors
    ///
    /// Returns [`OffsetError::PastEnd`] if `byte_offset` exceeds `text.len()`,
    /// [`OffsetError::InsideCharacter`] if it is not on a character boundary,
    /// and [`OffsetError::TooLong`] if the result does not fit in `u32`.
    pub fn byte_offset_to_character(
        &self,
        text: &str,
        byte_offset: usize,
    ) -> Result<u32, OffsetError> {
        if byte_offset > text.len() {
            return Err(OffsetError::PastEnd {
                offset: byte_offset,
                len: text.len(),
            });
        }
        // `text[..byte_offset]` below panics if `byte_offset` lands mid-character. A
        // server-reported offset should always be on a boundary, but this is
        // untrusted external input, so it is checked rather than trusted.
        let Some(prefix) = text.get(..byte_offset) else {
            return Err(OffsetError::InsideCharacter(byte_offset));
        };

        let units = match self.encoding {
            PositionEncoding::Utf8 => byte_offset,
            PositionEncoding::Utf16 => prefix.encode_utf16().count(),
            PositionEncoding::Utf32 => prefix.chars().count(),
        };
        u32::try_from(units).map_err(|_| OffsetError::TooLong)
    }

    /// Convert character offset to byte offset in the configured encoding.
    ///
    /// For UTF-16 and UTF-32, an offset inside a multi-unit character resolves
    /// to the next character boundary.
    ///
    /// # Errors
    ///
    /// Returns [`OffsetError::PastEnd`] (with `len` in the configured
    /// encoding's units) if the offset is beyond the end of `text`, and
    /// [`OffsetError::InsideCharacter`] for a UTF-8 offset that is not on a
    /// character boundary.
    pub fn character_to_byte_offset(
        &self,
        text: &str,
        character_offset: u32,
    ) -> Result<usize, OffsetError> {
        let wanted = character_offset as usize;
        match self.encoding {
            PositionEncoding::Utf8 => {
                if wanted > text.len() {
                    return Err(OffsetError::PastEnd {
                        offset: wanted,
                        len: text.len(),
                    });
                }
                // A UTF-8 "character offset" *is* a byte offset, taken
                // directly from untrusted input (an LSP position from the
                // server, or a re-derived offset from another encoding). It
                // must land on a boundary before any caller slices `text`
                // with it -- see `byte_offset_to_character`'s matching guard.
                if !text.is_char_boundary(wanted) {
                    return Err(OffsetError::InsideCharacter(wanted));
                }
                Ok(wanted)
            }
            PositionEncoding::Utf16 => Self::scan_units(text, wanted, char::len_utf16),
            PositionEncoding::Utf32 => Self::scan_units(text, wanted, |_| 1),
        }
    }

    /// Walks `text` once, returning the first boundary at or after `wanted`
    /// units, or `text.len()` when `wanted` equals the total unit count.
    fn scan_units(
        text: &str,
        wanted: usize,
        units_of: impl Fn(char) -> usize,
    ) -> Result<usize, OffsetError> {
        let mut units = 0usize;
        for (byte_idx, ch) in text.char_indices() {
            if units >= wanted {
                return Ok(byte_idx);
            }
            units = units.saturating_add(units_of(ch));
        }
        if units == wanted {
            Ok(text.len())
        } else {
            Err(OffsetError::PastEnd {
                offset: wanted,
                len: units,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_kind_and_enum_round_trip_for_every_encoding() {
        for encoding in PositionEncoding::ALL {
            let kind = encoding.as_str();
            assert_eq!(PositionEncoding::try_from(kind.to_string()), Ok(encoding));
            assert_eq!(<&str>::from(encoding), kind);
            assert_eq!(encoding.to_kind().as_str(), kind);
        }
    }

    #[test]
    fn test_invalid_encoding_error_lists_valid_values() {
        let err = PositionEncoding::try_from("utf-7".to_string())
            .unwrap_err()
            .to_string();
        assert!(err.contains("utf-7"));
        for encoding in PositionEncoding::ALL {
            assert!(err.contains(encoding.as_str()), "{err}");
        }
    }

    fn label_substring(
        label: &str,
        start: u32,
        end: u32,
        encoding: PositionEncoding,
    ) -> Option<&str> {
        LabelOffsets::new(label, [start, end], encoding).substring(start, end)
    }

    #[test]
    fn test_label_substring_resolves_offsets_per_encoding() {
        let label = "é𝄞xy";
        for (encoding, start, end) in [
            (PositionEncoding::Utf8, 6, 8),
            (PositionEncoding::Utf16, 3, 5),
            (PositionEncoding::Utf32, 2, 4),
        ] {
            assert_eq!(
                label_substring(label, start, end, encoding),
                Some("xy"),
                "{encoding:?}"
            );
            assert_eq!(label_substring(label, 0, 0, encoding), Some(""));
        }
    }

    #[test]
    fn test_label_substring_rejects_invalid_offsets() {
        let label = "é𝄞x";
        assert_eq!(label_substring(label, 0, 99, PositionEncoding::Utf16), None);
        assert_eq!(label_substring(label, 0, 99, PositionEncoding::Utf8), None);
        assert_eq!(label_substring(label, 3, 1, PositionEncoding::Utf16), None);
        assert_eq!(label_substring(label, 4, 3, PositionEncoding::Utf8), None);
        // Mid-character: inside `é` (UTF-8) and inside the surrogate pair (UTF-16).
        assert_eq!(label_substring(label, 1, 2, PositionEncoding::Utf8), None);
        assert_eq!(label_substring(label, 0, 2, PositionEncoding::Utf16), None);
    }

    fn lsp_at(
        line: u32,
        character: u32,
        line_text: Option<&str>,
        encoding: PositionEncoding,
    ) -> Converted<lsp_types::Position> {
        mcp_to_lsp_position(Position::at(line, character), line_text, encoding)
    }

    fn mcp_at(
        line: u32,
        character: u32,
        line_text: Option<&str>,
        encoding: PositionEncoding,
    ) -> Converted<(u32, u32)> {
        let converted =
            lsp_to_mcp_position(lsp_types::Position { line, character }, line_text, encoding);
        Converted {
            value: (converted.value.line, converted.value.character),
            fidelity: converted.fidelity,
        }
    }

    #[test]
    fn test_mcp_to_lsp_position() {
        let lsp_pos = lsp_at(1, 1, None, PositionEncoding::Utf16).value;
        assert_eq!(lsp_pos.line, 0);
        assert_eq!(lsp_pos.character, 0);

        let lsp_pos = lsp_at(10, 5, None, PositionEncoding::Utf16).value;
        assert_eq!(lsp_pos.line, 9);
        assert_eq!(lsp_pos.character, 4);
    }

    #[test]
    fn test_lsp_to_mcp_position() {
        let (line, char) = mcp_at(0, 0, None, PositionEncoding::Utf16).value;
        assert_eq!(line, 1);
        assert_eq!(char, 1);

        let (line, char) = mcp_at(9, 4, None, PositionEncoding::Utf16).value;
        assert_eq!(line, 10);
        assert_eq!(char, 5);
    }

    #[test]
    fn test_roundtrip() {
        for line in 1..100 {
            for char in 1..100 {
                let lsp_pos = lsp_at(line, char, None, PositionEncoding::Utf16).value;
                let Position2D {
                    line: mcp_line,
                    character: mcp_char,
                } = lsp_to_mcp_position(lsp_pos, None, PositionEncoding::Utf16).value;
                assert_eq!(line, mcp_line);
                assert_eq!(char, mcp_char);
            }
        }
    }

    /// Requirement: UTF-16 negotiated encoding must be byte-for-byte
    /// identical to the pre-negotiation behavior, even when `line_text` is
    /// supplied and contains multi-byte characters -- the fast path must
    /// never consult it.
    #[test]
    fn test_utf16_negotiated_ignores_line_text() {
        let line_text = "let 😀 = \"héllo\";";
        let lsp_pos = lsp_at(1, 6, Some(line_text), PositionEncoding::Utf16).value;
        assert_eq!(lsp_pos.character, 5);

        let (_, mcp_char) = mcp_at(0, 5, Some(line_text), PositionEncoding::Utf16).value;
        assert_eq!(mcp_char, 6);
    }

    /// A UTF-8 negotiated server counts columns in bytes. `héllo` has one
    /// multi-byte character (`é`, 2 bytes in UTF-8, 1 UTF-16 unit): the MCP
    /// (UTF-16) column after `é` must be re-derived as one byte further in
    /// UTF-8 terms.
    #[test]
    fn test_mcp_to_lsp_position_utf8_negotiated_multibyte() {
        let line_text = "héllo";
        // 1-based MCP column 3 sits right after "hé" (2 UTF-16 units).
        let lsp_pos = lsp_at(1, 3, Some(line_text), PositionEncoding::Utf8).value;
        // In UTF-8 bytes, "hé" is 3 bytes (h=1, é=2).
        assert_eq!(lsp_pos.character, 3);
    }

    #[test]
    fn test_lsp_to_mcp_position_utf8_negotiated_multibyte() {
        let line_text = "héllo";
        // LSP (UTF-8 byte) position 3 = right after "hé".
        let (_, mcp_char) = mcp_at(0, 3, Some(line_text), PositionEncoding::Utf8).value;
        // In UTF-16 units, "hé" is 2 units (h=1, é=1).
        assert_eq!(mcp_char, 3);
    }

    #[test]
    fn test_mcp_to_lsp_position_ascii_identical_across_encodings() {
        let line_text = "let x = 5;";
        for encoding in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            let pos = lsp_at(1, 5, Some(line_text), encoding).value;
            assert_eq!(
                pos.character, 4,
                "encoding {encoding:?} must agree on ASCII"
            );
        }
    }

    /// An out-of-bounds MCP character (e.g. stale client-side coordinates)
    /// clamps to the line length in the *target* encoding's units (LSP 3.17)
    /// rather than erroring or passing through: `"éééé"` is 4 UTF-16 units
    /// but 8 bytes.
    #[test]
    fn test_mcp_to_lsp_position_past_end_of_line_clamps_exactly() {
        for (text, column, expected) in [("short", 1000, 5), ("éééé", 6, 8)] {
            let pos = lsp_at(1, column, Some(text), PositionEncoding::Utf8);
            assert_eq!(pos.value.character, expected, "{text}");
            assert_eq!(pos.fidelity, ColumnFidelity::Exact, "{text}");
        }
    }

    #[test]
    fn test_lsp_to_mcp_position_past_end_of_line_clamps_but_is_flagged_as_stale_text() {
        let pos = mcp_at(0, 100, Some("éééé"), PositionEncoding::Utf8);
        assert_eq!(pos.value, (1, 5));
        assert_eq!(pos.fidelity, ColumnFidelity::PassedThrough);
    }

    #[test]
    fn test_mcp_to_lsp_position_missing_line_text_passes_through_flagged() {
        let pos = lsp_at(1, 4, None, PositionEncoding::Utf8);
        assert_eq!(pos.value.character, 3);
        assert_eq!(pos.fidelity, ColumnFidelity::PassedThrough);
    }

    #[test]
    fn test_column_zero_is_exact_without_line_text_in_both_directions() {
        let to_lsp = lsp_at(3, 1, None, PositionEncoding::Utf8);
        assert_eq!(to_lsp.value.character, 0);
        assert_eq!(to_lsp.fidelity, ColumnFidelity::Exact);

        let to_mcp = mcp_at(2, 0, None, PositionEncoding::Utf32);
        assert_eq!(to_mcp.value, (3, 1));
        assert_eq!(to_mcp.fidelity, ColumnFidelity::Exact);
    }

    #[test]
    fn test_lsp_to_mcp_position_missing_line_text_passes_through_flagged() {
        let pos = mcp_at(0, 4, None, PositionEncoding::Utf8);
        assert_eq!(pos.value, (1, 5));
        assert_eq!(pos.fidelity, ColumnFidelity::PassedThrough);
    }

    #[test]
    fn test_utf16_is_always_exact() {
        let pos = lsp_at(1, 500, Some("x"), PositionEncoding::Utf16);
        assert_eq!(pos.value.character, 499);
        assert_eq!(pos.fidelity, ColumnFidelity::Exact);
    }

    #[test]
    fn test_position_encoding_parsing() {
        assert_eq!(
            PositionEncoding::from_lsp("utf-8"),
            Some(PositionEncoding::Utf8)
        );
        assert_eq!(
            PositionEncoding::from_lsp("utf-16"),
            Some(PositionEncoding::Utf16)
        );
        assert_eq!(
            PositionEncoding::from_lsp("utf-32"),
            Some(PositionEncoding::Utf32)
        );
        assert_eq!(PositionEncoding::from_lsp("invalid"), None);
    }

    #[test]
    fn test_utf8_encoding() {
        let converter = EncodingConverter::new(PositionEncoding::Utf8);
        let text = "Hello, world!";

        let char_offset = converter.byte_offset_to_character(text, 7).unwrap();
        assert_eq!(char_offset, 7);

        let byte_offset = converter.character_to_byte_offset(text, 7).unwrap();
        assert_eq!(byte_offset, 7);
    }

    #[test]
    fn test_utf16_encoding_with_emoji() {
        let converter = EncodingConverter::new(PositionEncoding::Utf16);
        let text = "Hello 😀 world";

        let char_offset = converter.byte_offset_to_character(text, 6).unwrap();
        assert_eq!(char_offset, 6);

        let char_offset = converter.byte_offset_to_character(text, 10).unwrap();
        assert_eq!(char_offset, 8);

        let byte_offset = converter.character_to_byte_offset(text, 6).unwrap();
        assert_eq!(byte_offset, 6);

        let byte_offset = converter.character_to_byte_offset(text, 8).unwrap();
        assert_eq!(byte_offset, 10);
    }

    #[test]
    fn test_utf16_encoding_roundtrip() {
        let converter = EncodingConverter::new(PositionEncoding::Utf16);
        let text = "Hello 🌍 world!";

        for byte_idx in [0, 6, 10, 11] {
            let char_offset = converter.byte_offset_to_character(text, byte_idx).unwrap();
            let back_to_byte = converter
                .character_to_byte_offset(text, char_offset)
                .unwrap();
            assert_eq!(byte_idx, back_to_byte);
        }
    }

    #[test]
    fn test_utf32_encoding() {
        let converter = EncodingConverter::new(PositionEncoding::Utf32);
        let text = "Hello 😀 world";

        let char_offset = converter.byte_offset_to_character(text, 6).unwrap();
        assert_eq!(char_offset, 6);

        let char_offset = converter.byte_offset_to_character(text, 10).unwrap();
        assert_eq!(char_offset, 7);

        let byte_offset = converter.character_to_byte_offset(text, 7).unwrap();
        assert_eq!(byte_offset, 10);
    }

    #[test]
    fn test_encoding_edge_cases() {
        let converter = EncodingConverter::new(PositionEncoding::Utf8);

        assert!(converter.byte_offset_to_character("test", 100).is_err());
        assert!(converter.character_to_byte_offset("test", 100).is_err());

        let end_offset = converter.byte_offset_to_character("test", 4).unwrap();
        assert_eq!(end_offset, 4);
    }

    /// C1 regression: a byte offset that lands mid-character must error, not
    /// panic. `"héllo"` encodes `é` as the 2 bytes `0xC3 0xA9`; byte offset 2
    /// sits between them. Before the boundary guard this reached
    /// `text[..2].encode_utf16().count()` and panicked.
    #[test]
    fn test_byte_offset_to_character_mid_char_boundary_does_not_panic() {
        let text = "héllo";
        let byte_offset = 2; // inside 'é', not on a char boundary

        for encoding in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            let converter = EncodingConverter::new(encoding);
            assert!(
                converter
                    .byte_offset_to_character(text, byte_offset)
                    .is_err(),
                "encoding {encoding:?} must reject a mid-character byte offset instead of panicking"
            );
        }
    }

    /// C1 regression, `mcp_to_lsp_position`/`lsp_to_mcp_position` level: a
    /// UTF-8-negotiated conversion whose intermediate byte offset lands
    /// mid-character must fall back to the raw value, flagged, rather than
    /// propagate a panic.
    #[test]
    fn test_lsp_to_mcp_position_utf8_mid_char_lsp_offset_falls_back() {
        let line_text = "héllo";
        let converted = mcp_at(0, 2, Some(line_text), PositionEncoding::Utf8);
        assert_eq!(converted.value.1, 3); // pos.character + 1, the raw fallback
        assert_eq!(converted.fidelity, ColumnFidelity::PassedThrough);
    }

    /// Astral (non-BMP) characters on the UTF-8 negotiated path: `𝄞` (U+1D11E,
    /// the musical G-clef) is 4 bytes in UTF-8 and 2 UTF-16 code units (a
    /// surrogate pair).
    #[test]
    fn test_mcp_to_lsp_position_utf8_negotiated_astral_char() {
        let line_text = "𝄞x";
        // 1-based MCP column 3 sits right after the surrogate pair (2 UTF-16
        // units) + 1 for 1-based indexing.
        let lsp_pos = lsp_at(1, 3, Some(line_text), PositionEncoding::Utf8).value;
        assert_eq!(lsp_pos.character, 4); // 4 UTF-8 bytes for the astral char

        let (_, mcp_char) = mcp_at(0, 4, Some(line_text), PositionEncoding::Utf8).value;
        assert_eq!(mcp_char, 3);
    }

    /// Copilot review finding: an MCP character offset landing inside a
    /// UTF-16 surrogate pair (e.g. a client miscounting an astral character)
    /// must fall back to the raw offset, not silently round forward to the
    /// byte offset *after* the whole character. `𝄞` (U+1D11E) is a surrogate
    /// pair (2 UTF-16 units); MCP column 2 (1-based) sits between them.
    #[test]
    fn test_mcp_to_lsp_position_mid_surrogate_falls_back() {
        let line_text = "𝄞x";
        let lsp_pos = lsp_at(1, 2, Some(line_text), PositionEncoding::Utf8);
        // Falls back to the raw (unconverted) MCP character rather than
        // rounding forward to byte offset 4 (right after the astral char).
        assert_eq!(lsp_pos.value.character, 1);
        assert_eq!(lsp_pos.fidelity, ColumnFidelity::PassedThrough);
    }

    /// CRLF line endings: `line_text` (as sourced by callers via `str::lines`)
    /// never includes the terminator, so conversion math is identical to the
    /// LF case -- this locks in that CRLF content doesn't shift columns.
    #[test]
    fn test_mcp_to_lsp_position_utf8_negotiated_crlf_line_text() {
        let line_text = "héllo"; // as it would be yielded by "héllo\r\n".lines()
        let lsp_pos = lsp_at(1, 3, Some(line_text), PositionEncoding::Utf8).value;
        assert_eq!(lsp_pos.character, 3);
    }

    /// Issue #413: LSP 3.17 permits `character: u32::MAX` as an idiom for
    /// "end of line". A plain `+ 1` on that value overflows; `saturating_add`
    /// must clamp to `u32::MAX` instead of panicking or wrapping to 0.
    #[test]
    fn test_lsp_to_mcp_position_character_max_does_not_overflow() {
        let (_, mcp_char) = mcp_at(0, u32::MAX, None, PositionEncoding::Utf16).value;
        assert_eq!(mcp_char, u32::MAX);
    }

    /// The `u32::MAX` end-of-line sentinel is exact in every encoding, with
    /// or without line text.
    #[test]
    fn test_lsp_to_mcp_position_character_max_is_exact_for_non_utf16() {
        for line_text in [None, Some("héllo")] {
            let converted = mcp_at(0, u32::MAX, line_text, PositionEncoding::Utf8);
            assert_eq!(converted.value.1, u32::MAX);
            assert_eq!(converted.fidelity, ColumnFidelity::Exact);
        }
    }

    /// Issue #413: same overflow hazard on `pos.line`, since it also comes
    /// directly from the untrusted LSP server.
    #[test]
    fn test_lsp_to_mcp_position_line_max_does_not_overflow() {
        let (mcp_line, _) = mcp_at(u32::MAX, 0, None, PositionEncoding::Utf16).value;
        assert_eq!(mcp_line, u32::MAX);
    }

    /// Issue #413: both components at `u32::MAX` simultaneously.
    #[test]
    fn test_lsp_to_mcp_position_both_max_does_not_overflow() {
        let (mcp_line, mcp_char) = mcp_at(u32::MAX, u32::MAX, None, PositionEncoding::Utf16).value;
        assert_eq!(mcp_line, u32::MAX);
        assert_eq!(mcp_char, u32::MAX);
    }

    #[test]
    fn test_offset_errors_are_typed() {
        let utf8 = EncodingConverter::new(PositionEncoding::Utf8);
        assert_eq!(
            utf8.byte_offset_to_character("test", 100),
            Err(OffsetError::PastEnd {
                offset: 100,
                len: 4
            })
        );
        assert_eq!(
            utf8.byte_offset_to_character("a\u{1F600}", 2),
            Err(OffsetError::InsideCharacter(2))
        );
        assert_eq!(
            utf8.character_to_byte_offset("a\u{1F600}", 2),
            Err(OffsetError::InsideCharacter(2))
        );
        let utf16 = EncodingConverter::new(PositionEncoding::Utf16);
        assert_eq!(
            utf16.character_to_byte_offset("a\u{1F600}", 9),
            Err(OffsetError::PastEnd { offset: 9, len: 3 })
        );
        let utf32 = EncodingConverter::new(PositionEncoding::Utf32);
        assert_eq!(
            utf32.character_to_byte_offset("a\u{1F600}", 3),
            Err(OffsetError::PastEnd { offset: 3, len: 2 })
        );
    }
}
