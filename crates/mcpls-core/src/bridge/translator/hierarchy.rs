//! Conversion shared by the call and type hierarchy handlers.
//!
//! LSP's `CallHierarchyItem` and `TypeHierarchyItem` carry the same fields, as
//! do their MCP-facing DTOs, so both families convert through one pair of
//! functions instead of two copies.

use super::dto::{HierarchyItem, lsp_kind_to_u32};
use super::encoding_ctx::EncodingCtx;

/// The fields common to an LSP call or type hierarchy item, in the routed
/// server's coordinates.
pub(super) struct LspHierarchyItem {
    pub(super) name: String,
    pub(super) kind: lsp_types::SymbolKind,
    pub(super) detail: Option<String>,
    pub(super) uri: lsp_types::Uri,
    pub(super) range: lsp_types::Range,
    pub(super) selection_range: lsp_types::Range,
    pub(super) data: Option<serde_json::Value>,
}

impl From<lsp_types::CallHierarchyItem> for LspHierarchyItem {
    fn from(item: lsp_types::CallHierarchyItem) -> Self {
        Self {
            name: item.name,
            kind: item.kind,
            detail: item.detail,
            uri: item.uri,
            range: item.range,
            selection_range: item.selection_range,
            data: item.data,
        }
    }
}

impl From<lsp_types::TypeHierarchyItem> for LspHierarchyItem {
    fn from(item: lsp_types::TypeHierarchyItem) -> Self {
        Self {
            name: item.name,
            kind: item.kind,
            detail: item.detail,
            uri: item.uri,
            range: item.range,
            selection_range: item.selection_range,
            data: item.data,
        }
    }
}

impl From<LspHierarchyItem> for lsp_types::CallHierarchyItem {
    fn from(item: LspHierarchyItem) -> Self {
        Self {
            name: item.name,
            kind: item.kind,
            tags: None,
            detail: item.detail,
            uri: item.uri,
            range: item.range,
            selection_range: item.selection_range,
            data: item.data,
        }
    }
}

impl From<LspHierarchyItem> for lsp_types::TypeHierarchyItem {
    fn from(item: LspHierarchyItem) -> Self {
        Self {
            name: item.name,
            kind: item.kind,
            tags: None,
            detail: item.detail,
            uri: item.uri,
            range: item.range,
            selection_range: item.selection_range,
            data: item.data,
        }
    }
}

/// Convert an LSP hierarchy item into its MCP form, normalizing both ranges
/// into 1-based coordinates through `ctx`.
pub(super) async fn hierarchy_item_to_mcp<Lsp>(item: Lsp, ctx: &EncodingCtx) -> HierarchyItem
where
    Lsp: Into<LspHierarchyItem>,
{
    let item = item.into();
    let out_of_workspace = ctx.is_out_of_workspace(&item.uri);
    let range = ctx.normalize_range(&item.uri, item.range).await;
    let selection_range = ctx.normalize_range(&item.uri, item.selection_range).await;

    HierarchyItem {
        name: item.name,
        kind: lsp_kind_to_u32(item.kind),
        detail: item.detail,
        uri: item.uri.to_string(),
        range,
        selection_range,
        data: item.data,
        out_of_workspace,
    }
}

/// Convert an MCP hierarchy item (1-based) back into an LSP item in `ctx`'s
/// negotiated encoding -- the inverse of [`hierarchy_item_to_mcp`].
///
/// `uri` is the already-parsed form of `item`'s own URI.
pub(super) async fn hierarchy_item_to_lsp<Lsp>(
    item: HierarchyItem,
    uri: lsp_types::Uri,
    ctx: &EncodingCtx,
) -> Lsp
where
    Lsp: From<LspHierarchyItem>,
{
    // TODO(#636): hierarchy item input ranges bypass Position validation (line 0 saturates to 1)
    let range = ctx.denormalize_range(&uri, &item.range).await;
    let selection_range = ctx.denormalize_range(&uri, &item.selection_range).await;

    Lsp::from(LspHierarchyItem {
        name: item.name,
        // `SymbolKind: From<u32>` is infallible (see `lsp_kind_to_u32`'s
        // docs), so this exactly reverses the forward conversion.
        kind: lsp_types::SymbolKind::from(item.kind),
        detail: item.detail,
        uri,
        range,
        selection_range,
        data: item.data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::translator::dto::Position2D;
    use crate::bridge::translator::testing::test_ctx;

    fn lsp_type_item(uri: &lsp_types::Uri) -> lsp_types::TypeHierarchyItem {
        let range = lsp_types::Range {
            start: lsp_types::Position {
                line: 2,
                character: 4,
            },
            end: lsp_types::Position {
                line: 3,
                character: 0,
            },
        };
        lsp_types::TypeHierarchyItem {
            name: "Base".to_string(),
            kind: lsp_types::SymbolKind::from(5),
            tags: None,
            detail: Some("class".to_string()),
            uri: uri.clone(),
            range,
            selection_range: range,
            data: Some(serde_json::json!({"usr": "c:@S@Base"})),
        }
    }

    /// Pins the #617 follow-up marker: an input range at line 0 is clamped
    /// to line 1 (LSP line 0) instead of being rejected.
    #[tokio::test]
    async fn input_range_at_line_zero_clamps_to_the_first_line() {
        let ctx = test_ctx();
        let uri = lsp_types::Uri::from("file:///a.cpp");
        let zero = crate::bridge::translator::dto::Range {
            start: Position2D {
                line: 0,
                character: 0,
            },
            end: Position2D {
                line: 0,
                character: 0,
            },
        };
        let item = HierarchyItem {
            name: "x".into(),
            kind: 5,
            detail: None,
            uri: uri.to_string(),
            range: zero.clone(),
            selection_range: zero,
            data: None,
            out_of_workspace: false,
        };

        let lsp: lsp_types::TypeHierarchyItem = hierarchy_item_to_lsp(item, uri, &ctx).await;

        assert_eq!(lsp.range.start.line, 0);
    }

    #[tokio::test]
    async fn type_item_round_trips_through_mcp_form() {
        let ctx = test_ctx();
        let uri = lsp_types::Uri::from("file:///a.cpp");
        let original = lsp_type_item(&uri);

        let dto: HierarchyItem = hierarchy_item_to_mcp(original.clone(), &ctx).await;
        assert_eq!(dto.kind, 5);
        assert_eq!(
            dto.range.start,
            Position2D {
                line: 3,
                character: 5
            }
        );

        let back: lsp_types::TypeHierarchyItem = hierarchy_item_to_lsp(dto, uri, &ctx).await;
        assert_eq!(back, original);
    }

    #[tokio::test]
    async fn call_item_converts_through_the_same_path() {
        let ctx = test_ctx();
        let uri = lsp_types::Uri::from("file:///a.cpp");
        let item = lsp_type_item(&uri);
        let call = lsp_types::CallHierarchyItem {
            name: item.name,
            kind: item.kind,
            tags: None,
            detail: item.detail,
            uri: item.uri,
            range: item.range,
            selection_range: item.selection_range,
            data: item.data,
        };

        let dto: HierarchyItem = hierarchy_item_to_mcp(call.clone(), &ctx).await;
        let back: lsp_types::CallHierarchyItem =
            hierarchy_item_to_lsp(dto, call.uri.clone(), &ctx).await;
        assert_eq!(back, call);
    }
}
