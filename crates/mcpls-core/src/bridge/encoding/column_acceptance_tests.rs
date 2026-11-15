//! Acceptance tests for #913: each column meaning is its own type, and the byte
//! offset of a column is found only through that type.

use std::assert_matches;
use std::num::NonZeroU32;

use proptest::prelude::*;

use super::*;

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}

#[test]
fn test_utf16_column_from_mcp_is_zero_based() {
    assert_eq!(Utf16Column::from_mcp(nz(1)), Utf16Column::new(0));
    assert_eq!(Utf16Column::from_mcp(nz(5)), Utf16Column::new(4));
}

#[test]
fn test_utf16_column_to_mcp_is_one_based_and_saturates() {
    assert_eq!(Utf16Column::new(0).to_mcp(), nz(1));
    assert_eq!(Utf16Column::new(4).to_mcp(), nz(5));
    assert_eq!(Utf16Column::new(u32::MAX).to_mcp(), NonZeroU32::MAX);
}

#[test]
fn test_lsp_line_mcp_conversion_is_one_based_and_non_zero() {
    assert_eq!(LspLine::from_mcp(nz(1)), LspLine::new(0));
    assert_eq!(LspLine::new(0).to_mcp(), nz(1));
    assert_eq!(LspLine::new(u32::MAX).to_mcp(), NonZeroU32::MAX);
}

#[test]
fn test_utf16_column_byte_offset_in_rounds_forward_to_a_boundary() {
    let text = "a\u{1F600}b";

    assert_eq!(Utf16Column::new(0).byte_offset_in(text), Ok(0));
    assert_eq!(Utf16Column::new(1).byte_offset_in(text), Ok(1));
    assert_eq!(Utf16Column::new(2).byte_offset_in(text), Ok(5));
    assert_eq!(Utf16Column::new(3).byte_offset_in(text), Ok(5));
    assert_eq!(Utf16Column::new(4).byte_offset_in(text), Ok(6));
    assert_matches!(
        Utf16Column::new(5).byte_offset_in(text),
        Err(OffsetError::PastEnd { .. })
    );
}

#[test]
fn test_utf16_column_at_byte_reports_the_unit_count_before_the_byte() {
    let text = "a\u{1F600}b";

    assert_eq!(Utf16Column::at_byte(text, 0), Ok(Utf16Column::new(0)));
    assert_eq!(Utf16Column::at_byte(text, 1), Ok(Utf16Column::new(1)));
    assert_eq!(Utf16Column::at_byte(text, 5), Ok(Utf16Column::new(3)));
    assert_eq!(Utf16Column::at_byte(text, 6), Ok(Utf16Column::new(4)));
    assert_eq!(
        Utf16Column::at_byte(text, 2),
        Err(OffsetError::InsideCharacter(2))
    );
    assert_matches!(
        Utf16Column::at_byte(text, 7),
        Err(OffsetError::PastEnd { .. })
    );
}

#[test]
fn test_server_column_counts_units_of_its_own_encoding() {
    let text = "a\u{1F600}b";

    assert_eq!(
        ServerColumn::new(2, PositionEncoding::Utf32).byte_offset_in(text),
        Ok(5)
    );
    assert_eq!(
        ServerColumn::new(5, PositionEncoding::Utf8).byte_offset_in(text),
        Ok(5)
    );
    assert_eq!(
        ServerColumn::at_byte(text, 5, PositionEncoding::Utf32),
        Ok(ServerColumn::new(2, PositionEncoding::Utf32))
    );
    assert_eq!(
        ServerColumn::at_byte(text, 5, PositionEncoding::Utf16),
        Ok(ServerColumn::new(3, PositionEncoding::Utf16))
    );
}

#[test]
fn test_server_column_utf8_inside_a_character_is_an_error() {
    let text = "a\u{e9}";

    assert_matches!(
        ServerColumn::new(2, PositionEncoding::Utf8).byte_offset_in(text),
        Err(OffsetError::InsideCharacter(_))
    );
    assert_eq!(
        ServerColumn::new(3, PositionEncoding::Utf8).byte_offset_in(text),
        Ok(3)
    );
}

#[test]
fn test_server_columns_of_different_encodings_are_never_equal() {
    assert_ne!(
        ServerColumn::new(3, PositionEncoding::Utf8),
        ServerColumn::new(3, PositionEncoding::Utf16)
    );
}

proptest! {
    #[test]
    fn test_server_column_round_trips_through_every_boundary(
        text in "[a-z\u{e9}\u{4e2d}\u{1F600} ]{0,24}",
        encoding in prop::sample::select(PositionEncoding::ALL.to_vec()),
    ) {
        for byte in (0..=text.len()).filter(|&b| text.is_char_boundary(b)) {
            let column = ServerColumn::at_byte(&text, byte, encoding).unwrap();
            prop_assert_eq!(column.byte_offset_in(&text), Ok(byte));
        }
    }

    #[test]
    fn test_utf16_column_round_trips_through_every_boundary(
        text in "[a-z\u{e9}\u{4e2d}\u{1F600} ]{0,24}",
    ) {
        for byte in (0..=text.len()).filter(|&b| text.is_char_boundary(b)) {
            let column = Utf16Column::at_byte(&text, byte).unwrap();
            prop_assert_eq!(column.byte_offset_in(&text), Ok(byte));
        }
    }

    #[test]
    fn test_utf16_column_mcp_round_trip_is_lossless_below_the_maximum(value in 0..u32::MAX) {
        let column = Utf16Column::new(value);
        prop_assert_eq!(Utf16Column::from_mcp(column.to_mcp()), column);
    }
}
