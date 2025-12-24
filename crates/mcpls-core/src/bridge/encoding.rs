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
    pub fn to_lsp(&self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
            Self::Utf32 => "utf-32",
        }
    }
}

/// Convert MCP position (1-based) to LSP position (0-based).
///
/// MCP tools use 1-based line and column numbers for human readability.
/// LSP uses 0-based positions internally.
#[must_use]
pub fn mcp_to_lsp_position(line: u32, character: u32) -> Position {
    Position {
        line: line.saturating_sub(1),
        character: character.saturating_sub(1),
    }
}

/// Convert LSP position (0-based) to MCP position (1-based).
#[must_use]
pub fn lsp_to_mcp_position(pos: Position) -> (u32, u32) {
    (pos.line + 1, pos.character + 1)
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
    pub fn new(encoding: PositionEncoding) -> Self {
        Self { encoding }
    }

    /// Convert byte offset to character offset in the configured encoding.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The byte offset is not on a character boundary
    /// - The encoding is unsupported
    pub fn byte_offset_to_character(&self, text: &str, byte_offset: usize) -> Result<u32, String> {
        if byte_offset > text.len() {
            return Err(format!(
                "Byte offset {} exceeds text length {}",
                byte_offset,
                text.len()
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
    pub fn character_to_byte_offset(
        &self,
        text: &str,
        character_offset: u32,
    ) -> Result<usize, String> {
        match self.encoding {
            PositionEncoding::Utf8 => {
                let byte_offset = character_offset as usize;
                if byte_offset > text.len() {
                    return Err(format!(
                        "Character offset {} exceeds text length {}",
                        character_offset,
                        text.len()
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
                        "Character offset {} out of bounds (max UTF-16 units: {})",
                        character_offset, utf16_count
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
                    format!(
                        "Character offset {} out of bounds (max code points: {})",
                        character_offset,
                        text.chars().count()
                    )
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_to_lsp_position() {
        let lsp_pos = mcp_to_lsp_position(1, 1);
        assert_eq!(lsp_pos.line, 0);
        assert_eq!(lsp_pos.character, 0);

        let lsp_pos = mcp_to_lsp_position(10, 5);
        assert_eq!(lsp_pos.line, 9);
        assert_eq!(lsp_pos.character, 4);
    }

    #[test]
    fn test_lsp_to_mcp_position() {
        let (line, char) = lsp_to_mcp_position(Position {
            line: 0,
            character: 0,
        });
        assert_eq!(line, 1);
        assert_eq!(char, 1);

        let (line, char) = lsp_to_mcp_position(Position {
            line: 9,
            character: 4,
        });
        assert_eq!(line, 10);
        assert_eq!(char, 5);
    }

    #[test]
    fn test_roundtrip() {
        for line in 1..100 {
            for char in 1..100 {
                let lsp_pos = mcp_to_lsp_position(line, char);
                let (mcp_line, mcp_char) = lsp_to_mcp_position(lsp_pos);
                assert_eq!(line, mcp_line);
                assert_eq!(char, mcp_char);
            }
        }
    }

    #[test]
    fn test_saturating_sub_zero() {
        // Edge case: MCP position 0 should not underflow
        let lsp_pos = mcp_to_lsp_position(0, 0);
        assert_eq!(lsp_pos.line, 0);
        assert_eq!(lsp_pos.character, 0);
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
            let back_to_byte = converter.character_to_byte_offset(text, char_offset).unwrap();
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
}
