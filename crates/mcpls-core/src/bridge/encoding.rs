//! Position encoding conversion utilities.
//!
//! Handles conversion between MCP (1-based) and LSP (0-based) positions,
//! as well as UTF-8/UTF-16/UTF-32 encoding conversions.

use lsp_types::Position;

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

/// Convert MCP position (1-based) to LSP position (0-based), translating the
/// character column into `encoding`'s units.
///
/// MCP character columns are defined in UTF-16 code units -- the LSP default
/// and what nearly every server negotiates -- so `PositionEncoding::Utf16`
/// is a pure line/column offset with no further work, byte-for-byte
/// identical to the fixed-encoding behavior this replaces. For any other
/// negotiated encoding, `line_text` (the exact text of the target 0-based
/// LSP line, without a line terminator) is used to re-derive the column in
/// `encoding`'s units. If `line_text` is unavailable (e.g. the file could
/// not be read) or the character offset is out of bounds for that line, the
/// raw MCP character is used unconverted rather than failing the request.
#[must_use]
pub fn mcp_to_lsp_position(
    line: u32,
    character: u32,
    line_text: Option<&str>,
    encoding: PositionEncoding,
) -> Position {
    let lsp_line = line.saturating_sub(1);
    let mcp_character = character.saturating_sub(1);

    let lsp_character = match (encoding, line_text) {
        (PositionEncoding::Utf16, _) | (_, None) => mcp_character,
        (_, Some(text)) => {
            let target = EncodingConverter::new(encoding);
            exact_byte_offset(text, mcp_character, PositionEncoding::Utf16)
                .and_then(|byte_offset| target.byte_offset_to_character(text, byte_offset).ok())
                .unwrap_or(mcp_character)
        }
    };

    Position {
        line: lsp_line,
        character: lsp_character,
    }
}

/// Convert LSP position (0-based, in `encoding`'s units) to MCP position
/// (1-based, UTF-16 code units).
///
/// The inverse of [`mcp_to_lsp_position`]; see its docs for the fast path
/// and fallback behavior.
#[must_use]
pub fn lsp_to_mcp_position(
    pos: Position,
    line_text: Option<&str>,
    encoding: PositionEncoding,
) -> (u32, u32) {
    let mcp_character = match (encoding, line_text) {
        (PositionEncoding::Utf16, _) | (_, None) => pos.character,
        (_, Some(text)) => {
            let utf16 = EncodingConverter::new(PositionEncoding::Utf16);
            exact_byte_offset(text, pos.character, encoding)
                .and_then(|byte_offset| utf16.byte_offset_to_character(text, byte_offset).ok())
                .unwrap_or(pos.character)
        }
    };

    (pos.line + 1, mcp_character + 1)
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
        // `text[..byte_offset]` below panics (and, under `panic = "abort"`,
        // kills the whole process) if `byte_offset` lands mid-character. A
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
                    utf16_count += ch.len_utf16() as u32;
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

    #[test]
    fn test_mcp_to_lsp_position() {
        let lsp_pos = mcp_to_lsp_position(1, 1, None, PositionEncoding::Utf16);
        assert_eq!(lsp_pos.line, 0);
        assert_eq!(lsp_pos.character, 0);

        let lsp_pos = mcp_to_lsp_position(10, 5, None, PositionEncoding::Utf16);
        assert_eq!(lsp_pos.line, 9);
        assert_eq!(lsp_pos.character, 4);
    }

    #[test]
    fn test_lsp_to_mcp_position() {
        let (line, char) = lsp_to_mcp_position(
            Position {
                line: 0,
                character: 0,
            },
            None,
            PositionEncoding::Utf16,
        );
        assert_eq!(line, 1);
        assert_eq!(char, 1);

        let (line, char) = lsp_to_mcp_position(
            Position {
                line: 9,
                character: 4,
            },
            None,
            PositionEncoding::Utf16,
        );
        assert_eq!(line, 10);
        assert_eq!(char, 5);
    }

    #[test]
    fn test_roundtrip() {
        for line in 1..100 {
            for char in 1..100 {
                let lsp_pos = mcp_to_lsp_position(line, char, None, PositionEncoding::Utf16);
                let (mcp_line, mcp_char) =
                    lsp_to_mcp_position(lsp_pos, None, PositionEncoding::Utf16);
                assert_eq!(line, mcp_line);
                assert_eq!(char, mcp_char);
            }
        }
    }

    #[test]
    fn test_saturating_sub_zero() {
        // Edge case: MCP position 0 should not underflow
        let lsp_pos = mcp_to_lsp_position(0, 0, None, PositionEncoding::Utf16);
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
        let lsp_pos = mcp_to_lsp_position(1, 6, Some(line_text), PositionEncoding::Utf16);
        assert_eq!(lsp_pos.character, 5);

        let (_, mcp_char) = lsp_to_mcp_position(
            Position {
                line: 0,
                character: 5,
            },
            Some(line_text),
            PositionEncoding::Utf16,
        );
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
        let lsp_pos = mcp_to_lsp_position(1, 3, Some(line_text), PositionEncoding::Utf8);
        // In UTF-8 bytes, "hé" is 3 bytes (h=1, é=2).
        assert_eq!(lsp_pos.character, 3);
    }

    #[test]
    fn test_lsp_to_mcp_position_utf8_negotiated_multibyte() {
        let line_text = "héllo";
        // LSP (UTF-8 byte) position 3 = right after "hé".
        let (_, mcp_char) = lsp_to_mcp_position(
            Position {
                line: 0,
                character: 3,
            },
            Some(line_text),
            PositionEncoding::Utf8,
        );
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
            let pos = mcp_to_lsp_position(1, 5, Some(line_text), encoding);
            assert_eq!(
                pos.character, 4,
                "encoding {encoding:?} must agree on ASCII"
            );
        }
    }

    /// An out-of-bounds MCP character (e.g. stale client-side coordinates)
    /// must fall back to the raw value rather than erroring the request.
    #[test]
    fn test_mcp_to_lsp_position_out_of_bounds_falls_back() {
        let line_text = "short";
        let pos = mcp_to_lsp_position(1, 1000, Some(line_text), PositionEncoding::Utf8);
        assert_eq!(pos.character, 999);
    }

    #[test]
    fn test_mcp_to_lsp_position_missing_line_text_falls_back() {
        let pos = mcp_to_lsp_position(1, 4, None, PositionEncoding::Utf8);
        assert_eq!(pos.character, 3);
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
    /// `text[..2].encode_utf16().count()` and panicked (and, under
    /// `panic = "abort"`, aborted the whole process).
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
    /// mid-character must fall back to the raw value (existing `unwrap_or`
    /// behavior) rather than propagate a panic.
    #[test]
    fn test_lsp_to_mcp_position_utf8_mid_char_lsp_offset_falls_back() {
        let line_text = "héllo";
        let (_, mcp_char) = lsp_to_mcp_position(
            Position {
                line: 0,
                character: 2, // inside 'é' in UTF-8 byte terms
            },
            Some(line_text),
            PositionEncoding::Utf8,
        );
        assert_eq!(mcp_char, 3); // pos.character + 1, the raw fallback
    }

    /// Astral (non-BMP) characters on the UTF-8 negotiated path: `𝄞` (U+1D11E,
    /// the musical G-clef) is 4 bytes in UTF-8 and 2 UTF-16 code units (a
    /// surrogate pair).
    #[test]
    fn test_mcp_to_lsp_position_utf8_negotiated_astral_char() {
        let line_text = "𝄞x";
        // 1-based MCP column 3 sits right after the surrogate pair (2 UTF-16
        // units) + 1 for 1-based indexing.
        let lsp_pos = mcp_to_lsp_position(1, 3, Some(line_text), PositionEncoding::Utf8);
        assert_eq!(lsp_pos.character, 4); // 4 UTF-8 bytes for the astral char

        let (_, mcp_char) = lsp_to_mcp_position(
            Position {
                line: 0,
                character: 4,
            },
            Some(line_text),
            PositionEncoding::Utf8,
        );
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
        let lsp_pos = mcp_to_lsp_position(1, 2, Some(line_text), PositionEncoding::Utf8);
        // Falls back to the raw (unconverted) MCP character rather than
        // rounding forward to byte offset 4 (right after the astral char).
        assert_eq!(lsp_pos.character, 1);
    }

    /// CRLF line endings: `line_text` (as sourced by callers via `str::lines`)
    /// never includes the terminator, so conversion math is identical to the
    /// LF case -- this locks in that CRLF content doesn't shift columns.
    #[test]
    fn test_mcp_to_lsp_position_utf8_negotiated_crlf_line_text() {
        let line_text = "héllo"; // as it would be yielded by "héllo\r\n".lines()
        let lsp_pos = mcp_to_lsp_position(1, 3, Some(line_text), PositionEncoding::Utf8);
        assert_eq!(lsp_pos.character, 3);
    }
}
