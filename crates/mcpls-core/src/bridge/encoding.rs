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
        assert_eq!(PositionEncoding::from_lsp("utf-8"), Some(PositionEncoding::Utf8));
        assert_eq!(PositionEncoding::from_lsp("utf-16"), Some(PositionEncoding::Utf16));
        assert_eq!(PositionEncoding::from_lsp("utf-32"), Some(PositionEncoding::Utf32));
        assert_eq!(PositionEncoding::from_lsp("invalid"), None);
    }
}
