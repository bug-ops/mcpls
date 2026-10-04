//! Position encoding conversion utilities.
//!
//! Handles conversion between MCP (1-based) and LSP (0-based) positions,
//! as well as UTF-8/UTF-16/UTF-32 encoding conversions.

use super::translator::{Position, Position2D};

/// Supported position encodings per LSP 3.17.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PositionEncoding {
    /// UTF-8 code units.
    #[default]
    Utf8,
    /// UTF-16 code units (LSP default).
    Utf16,
    /// UTF-32 code units (Unicode code points).
    Utf32,
}

impl PositionEncoding {
    /// Parse from LSP position encoding kind string.
    #[must_use]
    pub fn from_lsp(kind: &str) -> Option<Self> {
        match kind {
            "utf-8" => Some(Self::Utf8),
            "utf-16" => Some(Self::Utf16),
            "utf-32" => Some(Self::Utf32),
            _ => None,
        }
    }

    /// Convert to LSP position encoding kind string.
    #[must_use]
    pub const fn to_lsp(&self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
            Self::Utf32 => "utf-32",
        }
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
        position.character.saturating_sub(1),
        line_text,
        PositionEncoding::Utf16,
        encoding,
    );
    Converted {
        value: lsp_types::Position {
            line: position.line.saturating_sub(1),
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
    // Code units never outnumber bytes, so `column > text.len()` is past the end in any encoding.
    let past_end =
        || column as usize > text.len() || line_len_in(from).is_some_and(|len| column > len);

    let exact = (column as usize <= text.len())
        .then(|| exact_byte_offset(text, column, from))
        .flatten()
        .and_then(|byte_offset| {
            EncodingConverter::new(to)
                .byte_offset_to_character(text, byte_offset)
                .ok()
        });
    if let Some(converted) = exact {
        return Converted::exact(converted);
    }
    if past_end()
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

/// Resolve `character_offset` (in `encoding`'s units) to a byte offset in
/// `text`, requiring the mapping to be exact.
///
/// `EncodingConverter::character_to_byte_offset` finds the byte boundary at
/// or after the requested offset, so an offset that lands inside a
/// multi-unit character (e.g. a UTF-16 surrogate pair) silently resolves to
/// the *next* character boundary instead of erroring. Round-tripping the
/// result back through `byte_offset_to_character` detects that case: if it
/// doesn't reproduce `character_offset` exactly, the offset wasn't
/// representable, and `None` signals the caller to fall back to the raw
/// value rather than use a rounded-forward position.
fn exact_byte_offset(
    text: &str,
    character_offset: u32,
    encoding: PositionEncoding,
) -> Option<usize> {
    let converter = EncodingConverter::new(encoding);
    let byte_offset = converter
        .character_to_byte_offset(text, character_offset)
        .ok()?;
    let round_trip = converter.byte_offset_to_character(text, byte_offset).ok()?;
    (round_trip == character_offset).then_some(byte_offset)
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
    /// Returns an error if:
    /// - The byte offset is not on a character boundary
    /// - The encoding is unsupported
    #[allow(clippy::cast_possible_truncation)] // LSP positions use u32, truncation acceptable
    pub fn byte_offset_to_character(&self, text: &str, byte_offset: usize) -> Result<u32, String> {
        if byte_offset > text.len() {
            let text_len = text.len();
            return Err(format!(
                "Byte offset {byte_offset} exceeds text length {text_len}"
            ));
        }
        // `text[..byte_offset]` below panics if `byte_offset` lands mid-character. A
        // server-reported offset should always be on a boundary, but this is
        // untrusted external input, so it is checked rather than trusted.
        if !text.is_char_boundary(byte_offset) {
            return Err(format!(
                "Byte offset {byte_offset} is not on a character boundary"
            ));
        }

        match self.encoding {
            PositionEncoding::Utf8 => Ok(byte_offset as u32),
            PositionEncoding::Utf16 => {
                let utf16_units = text[..byte_offset].encode_utf16().count();
                Ok(utf16_units as u32)
            }
            PositionEncoding::Utf32 => {
                let code_points = text[..byte_offset].chars().count();
                Ok(code_points as u32)
            }
        }
    }

    /// Convert character offset to byte offset in the configured encoding.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The character offset is out of bounds
    /// - The encoding is unsupported
    #[allow(clippy::cast_possible_truncation)] // LSP positions use u32, truncation acceptable
    pub fn character_to_byte_offset(
        &self,
        text: &str,
        character_offset: u32,
    ) -> Result<usize, String> {
        match self.encoding {
            PositionEncoding::Utf8 => {
                let byte_offset = character_offset as usize;
                if byte_offset > text.len() {
                    let text_len = text.len();
                    return Err(format!(
                        "Character offset {character_offset} exceeds text length {text_len}"
                    ));
                }
                // A UTF-8 "character offset" *is* a byte offset, taken
                // directly from untrusted input (an LSP position from the
                // server, or a re-derived offset from another encoding). It
                // must land on a boundary before any caller slices `text`
                // with it -- see `byte_offset_to_character`'s matching guard.
                if !text.is_char_boundary(byte_offset) {
                    return Err(format!(
                        "Character offset {character_offset} is not on a character boundary"
                    ));
                }
                Ok(byte_offset)
            }
            PositionEncoding::Utf16 => {
                let mut utf16_count = 0u32;
                for (byte_idx, ch) in text.char_indices() {
                    if utf16_count >= character_offset {
                        return Ok(byte_idx);
                    }
                    utf16_count = utf16_count.saturating_add(ch.len_utf16() as u32);
                }
                if utf16_count == character_offset {
                    Ok(text.len())
                } else {
                    Err(format!(
                        "Character offset {character_offset} out of bounds (max UTF-16 units: {utf16_count})"
                    ))
                }
            }
            PositionEncoding::Utf32 => text
                .char_indices()
                .nth(character_offset as usize)
                .map(|(byte_idx, _)| byte_idx)
                .or_else(|| {
                    if character_offset == text.chars().count() as u32 {
                        Some(text.len())
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    let max_code_points = text.chars().count();
                    format!(
                        "Character offset {character_offset} out of bounds (max code points: {max_code_points})"
                    )
                }),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
        mcp_to_lsp_position(Position { line, character }, line_text, encoding)
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

    #[test]
    fn test_saturating_sub_zero() {
        // Edge case: MCP position 0 should not underflow
        let lsp_pos = lsp_at(0, 0, None, PositionEncoding::Utf16).value;
        assert_eq!(lsp_pos.line, 0);
        assert_eq!(lsp_pos.character, 0);
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
}
