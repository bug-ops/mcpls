//! Acceptance tests for #913: 1-based positions are non-zero by construction and
//! the 0-based UTF-16 column is its own type.

#![expect(
    clippy::expect_used,
    clippy::missing_const_for_fn,
    reason = "test code; a panic is the failure signal"
)]

use std::assert_matches;
use std::num::NonZeroU32;

use mcpls_core::bridge::{
    CheckedHierarchyItem, HierarchyItem, InvalidHierarchyItem, LspLine, MAX_POSITION_VALUE,
    Position2D, Range, Utf16Column,
};
use serde_json::{Value, json};

fn nz(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("fixture value is non-zero")
}

#[test]
fn test_position2d_from_mcp_keeps_one_based_values() {
    let position = Position2D::from_mcp(nz(3), nz(5));

    assert_eq!((position.line(), position.character()), (nz(3), nz(5)));
    assert_eq!(position.lsp_line(), LspLine::new(2));
    assert_eq!(position.lsp_character(), Utf16Column::new(4));
}

#[test]
fn test_position2d_orders_by_line_then_character() {
    let at = |line, character| Position2D::from_mcp(nz(line), nz(character));

    assert!(at(2, 1) > at(1, 99));
    assert!(at(1, 2) > at(1, 1));
    assert_eq!(at(4, 4).cmp(&at(4, 4)), std::cmp::Ordering::Equal);
}

#[test]
fn test_utf16_column_round_trips_its_value() {
    assert_eq!(Utf16Column::new(0).get(), 0);
    assert_eq!(Utf16Column::new(u32::MAX).get(), u32::MAX);
    assert_ne!(Utf16Column::new(1), Utf16Column::new(2));
}

#[test]
fn test_lsp_line_lines_through_counts_inclusive_lines() {
    assert_eq!(LspLine::new(3).lines_through(LspLine::new(3)), 1);
    assert_eq!(LspLine::new(3).lines_through(LspLine::new(5)), 3);
    assert_eq!(LspLine::new(5).lines_through(LspLine::new(3)), 1);
    assert_eq!(
        LspLine::new(0).lines_through(LspLine::new(u32::MAX)),
        u32::MAX
    );
}

#[test]
fn test_lsp_line_saturating_add_clamps_at_the_maximum() {
    assert_eq!(LspLine::new(2).saturating_add(3), LspLine::new(5));
    assert_eq!(
        LspLine::new(u32::MAX).saturating_add(1),
        LspLine::new(u32::MAX)
    );
}

#[test]
fn test_position2d_deserializes_a_one_based_position() {
    let position: Position2D = serde_json::from_value(json!({"line": 1, "character": 1})).unwrap();

    assert_eq!(position, Position2D::from_mcp(nz(1), nz(1)));
}

#[test]
fn test_position2d_deserialize_rejects_a_zero_line() {
    let result = serde_json::from_value::<Position2D>(json!({"line": 0, "character": 1}));

    assert!(result.is_err());
}

#[test]
fn test_position2d_deserialize_rejects_a_zero_character() {
    let result = serde_json::from_value::<Position2D>(json!({"line": 1, "character": 0}));

    assert!(result.is_err());
}

#[test]
fn test_range_deserialize_rejects_a_zero_position() {
    let result = serde_json::from_value::<Range>(json!({
        "start": {"line": 0, "character": 1},
        "end": {"line": 1, "character": 1},
    }));

    assert!(result.is_err());
}

fn hierarchy_item(range_start_line: u32) -> Value {
    json!({
        "name": "Base", "kind": 5, "uri": "file:///a.cpp",
        "range": {
            "start": {"line": range_start_line, "character": 1},
            "end": {"line": 2, "character": 1},
        },
        "selectionRange": {
            "start": {"line": 1, "character": 7},
            "end": {"line": 1, "character": 11},
        },
    })
}

#[test]
fn test_hierarchy_item_deserialize_rejects_a_zero_position() {
    let result = serde_json::from_value::<HierarchyItem>(hierarchy_item(0));

    assert!(result.is_err());
}

#[test]
fn test_checked_hierarchy_item_from_client_accepts_a_valid_item() {
    let item: HierarchyItem = serde_json::from_value(hierarchy_item(1)).unwrap();

    assert!(CheckedHierarchyItem::from_client(item).is_ok());
}

#[test]
fn test_checked_hierarchy_item_from_client_rejects_an_oversized_position() {
    let item: HierarchyItem =
        serde_json::from_value(hierarchy_item(MAX_POSITION_VALUE + 1)).unwrap();

    assert_matches!(
        CheckedHierarchyItem::from_client(item),
        Err(InvalidHierarchyItem::Range(_))
    );
}

fn golden_tools() -> Vec<Value> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcp/tool_surface.json");
    let text = std::fs::read_to_string(path).expect("golden tools/list payload");
    serde_json::from_str(&text).expect("golden is JSON")
}

#[test]
fn test_golden_position2d_schemas_require_one_based_values() {
    let mut seen = 0;
    for tool in golden_tools() {
        for side in ["inputSchema", "outputSchema"] {
            let Some(position) = tool.pointer(&format!("/{side}/$defs/Position2D")) else {
                continue;
            };
            seen += 1;
            for field in ["line", "character"] {
                let minimum = position.pointer(&format!("/properties/{field}/minimum"));
                assert_eq!(
                    minimum,
                    Some(&json!(1)),
                    "{} {side} Position2D.{field}",
                    tool["name"]
                );
            }
        }
    }
    assert!(seen > 0, "the golden has Position2D definitions");
}
