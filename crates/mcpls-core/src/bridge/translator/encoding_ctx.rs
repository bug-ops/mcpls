//! Per-response position/range encoding conversion between MCP's 1-based
//! UTF-16 columns and an LSP server's negotiated encoding.

use std::sync::Arc;

use super::dto::{Position2D, Range};
use crate::bridge::DocumentTracker;
use crate::bridge::encoding::{PositionEncoding, lsp_to_mcp_position, mcp_to_lsp_position};
use crate::bridge::state::uri_to_path;

/// Per-response encoding context: the negotiated [`PositionEncoding`] of the
/// LSP server that produced a response, used to convert every
/// position/range in that response between MCP's 1-based UTF-16 columns and
/// the server's own 0-based columns.
///
/// A single MCP tool call is always answered by exactly one LSP server, so
/// one context covers every location in its response -- even when
/// individual locations point into other files (e.g. `references` results
/// spanning multiple documents): each conversion resolves the *referenced*
/// file's line text independently rather than assuming it matches the
/// originally queried document.
#[derive(Debug, Clone)]
pub(super) struct EncodingCtx {
    pub(super) encoding: PositionEncoding,
    /// Source of a tracked document's in-memory content -- the text mcpls
    /// actually sent the server via `didOpen`/`didChange` -- consulted
    /// before falling back to disk. See [`read_line_text`].
    pub(super) tracker: Arc<DocumentTracker>,
}

/// Text of the 0-based `line`'th line of the file at `uri`, or `None` if it
/// cannot be resolved to a path, read, or has no such line.
///
/// Only ever consulted when the negotiated encoding is not UTF-16 (see
/// [`EncodingCtx::to_lsp`]/[`EncodingCtx::to_mcp`]). Checks `tracker` first
/// (in-memory, no I/O) -- this is by construction both cheaper and more
/// correct than disk for any document mcpls has opened, since it is exactly
/// the text the server was told about, so it can't diverge from the
/// server's own view even if the file has since been edited on disk (see
/// #290 S1). Only a document `tracker` has never seen falls through to an
/// async disk read, matching `state.rs`'s `tokio::fs` convention so this
/// never blocks the executor thread.
async fn read_line_text(
    uri: &lsp_types::Uri,
    line: u32,
    tracker: &DocumentTracker,
) -> Option<String> {
    let path = uri_to_path(uri)?;
    if let Some(text) = tracker.line_text(&path, line) {
        return Some(text);
    }
    let content = tokio::fs::read_to_string(&path).await.ok()?;
    content.lines().nth(line as usize).map(str::to_string)
}

impl EncodingCtx {
    /// Convert an MCP position for the document at `uri` into an LSP
    /// position in this context's negotiated encoding.
    pub(super) async fn to_lsp(
        &self,
        uri: &lsp_types::Uri,
        line: u32,
        character: u32,
    ) -> lsp_types::Position {
        let line_text = if self.encoding == PositionEncoding::Utf16 {
            None
        } else {
            let text = read_line_text(uri, line.saturating_sub(1), &self.tracker).await;
            if text.is_none() {
                tracing::warn!(
                    uri = uri.as_str(),
                    line,
                    encoding = self.encoding.to_lsp(),
                    "could not resolve line text for position conversion; passing MCP column \
                     through unconverted, which is wrong for a non-UTF-16 server"
                );
            }
            text
        };
        mcp_to_lsp_position(line, character, line_text.as_deref(), self.encoding)
    }

    /// Convert an LSP position (in this context's negotiated encoding) from
    /// the document at `uri` into an MCP position.
    pub(super) async fn to_mcp(
        &self,
        uri: &lsp_types::Uri,
        pos: lsp_types::Position,
    ) -> Position2D {
        let line_text = if self.encoding == PositionEncoding::Utf16 {
            None
        } else {
            let text = read_line_text(uri, pos.line, &self.tracker).await;
            if text.is_none() {
                tracing::warn!(
                    uri = uri.as_str(),
                    line = pos.line,
                    encoding = self.encoding.to_lsp(),
                    "could not resolve line text for position conversion; passing server \
                     column through unconverted, which is wrong for a non-UTF-16 server"
                );
            }
            text
        };
        let (line, character) = lsp_to_mcp_position(pos, line_text.as_deref(), self.encoding);
        Position2D { line, character }
    }

    /// Convert an LSP range (in this context's negotiated encoding) from the
    /// document at `uri` into an MCP range.
    pub(super) async fn normalize_range(
        &self,
        uri: &lsp_types::Uri,
        range: lsp_types::Range,
    ) -> Range {
        Range {
            start: self.to_mcp(uri, range.start).await,
            end: self.to_mcp(uri, range.end).await,
        }
    }

    /// Convert an MCP range for the document at `uri` back into an LSP range
    /// in this context's negotiated encoding -- the inverse of
    /// [`Self::normalize_range`].
    pub(super) async fn denormalize_range(
        &self,
        uri: &lsp_types::Uri,
        range: &Range,
    ) -> lsp_types::Range {
        lsp_types::Range {
            start: self
                .to_lsp(uri, range.start.line, range.start.character)
                .await,
            end: self.to_lsp(uri, range.end.line, range.end.character).await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::bridge::path_to_uri;
    use crate::bridge::state::ResourceLimits;
    use crate::bridge::translator::testing::*;

    #[tokio::test]
    async fn test_normalize_range() {
        let lsp_range = lsp_types::Range {
            start: lsp_types::Position {
                line: 0,
                character: 0,
            },
            end: lsp_types::Position {
                line: 2,
                character: 5,
            },
        };

        let mcp_range = test_ctx().normalize_range(&test_uri(), lsp_range).await;
        assert_eq!(mcp_range.start.line, 1);
        assert_eq!(mcp_range.start.character, 1);
        assert_eq!(mcp_range.end.line, 3);
        assert_eq!(mcp_range.end.character, 6);
    }

    /// End-to-end proof that a non-UTF-16 `EncodingCtx` is actually wired to
    /// `read_line_text`/disk, not just correct in isolation at the
    /// `encoding.rs` function level: a real temp file with a multibyte line
    /// ("héllo"), converted through `EncodingCtx::to_lsp` for a document the
    /// tracker has never seen (forcing the disk-read fallback).
    #[tokio::test]
    async fn test_encoding_ctx_utf8_reads_disk_line_text_for_untracked_document() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("multibyte.rs");
        fs::write(&path, "héllo").unwrap();
        let uri = path_to_uri(&path).unwrap();

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        let lsp_pos = ctx.to_lsp(&uri, 1, 3).await;
        // "hé" is 3 bytes in UTF-8 (h=1, é=2); MCP column 3 (UTF-16, after
        // "hé") must re-derive to that byte offset via the disk-read line
        // text, matching the `encoding.rs`-level math for the same input.
        assert_eq!(lsp_pos.character, 3);
    }

    /// C3/S1: when a document is tracked, `EncodingCtx` must prefer its
    /// in-memory content over disk -- both cheaper (no I/O) and more correct
    /// when they've diverged. Here disk holds stale ASCII ("hello", no
    /// accent) while the tracker holds the live multibyte content
    /// ("héllo"); if conversion used disk instead, MCP column 3 would
    /// re-derive to LSP byte offset 2 (ASCII, no multibyte char) instead of
    /// 3 (multibyte-correct) -- so this distinguishes the two sources rather
    /// than merely tolerating either.
    #[tokio::test]
    async fn test_encoding_ctx_utf8_prefers_tracked_content_over_stale_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tracked.rs");
        fs::write(&path, "hello").unwrap(); // stale: no accent

        let tracker = Arc::new(DocumentTracker::new(
            ResourceLimits::default(),
            HashMap::new(),
        ));
        let uri = tracker.open(path.clone(), "héllo".to_string()).unwrap(); // live: accent

        let ctx = EncodingCtx {
            encoding: PositionEncoding::Utf8,
            tracker,
        };
        let lsp_pos = ctx.to_lsp(&uri, 1, 3).await;
        assert_eq!(
            lsp_pos.character, 3,
            "must convert against the tracker's live content (\"héllo\" -> byte 3), not disk's \
             stale content (\"hello\" -> byte 2)"
        );
    }

    /// A single `EncodingCtx` answering one MCP tool call may still need to
    /// convert positions in several different files (e.g. `references`
    /// results spanning multiple documents) -- each conversion must resolve
    /// *that* location's own file, never reuse or leak another file's line
    /// text. Two untracked files with different content at the same
    /// byte offset make a wrong-file conversion produce a visibly different
    /// (wrong) answer: byte offset 3 is UTF-16 column 3 in "héllo" but
    /// column 4 in the all-ASCII "hello".
    #[tokio::test]
    async fn test_normalize_range_multi_file_converts_each_location_against_its_own_uri() {
        let dir = TempDir::new().unwrap();
        let path_a = dir.path().join("a.rs");
        fs::write(&path_a, "héllo").unwrap();
        let uri_a = path_to_uri(&path_a).unwrap();

        let path_b = dir.path().join("b.rs");
        fs::write(&path_b, "hello").unwrap();
        let uri_b = path_to_uri(&path_b).unwrap();

        let lsp_range = lsp_types::Range {
            start: lsp_types::Position {
                line: 0,
                character: 0,
            },
            end: lsp_types::Position {
                line: 0,
                character: 3,
            },
        };

        let ctx = test_ctx_with(PositionEncoding::Utf8);
        let range_a = ctx.normalize_range(&uri_a, lsp_range).await;
        let range_b = ctx.normalize_range(&uri_b, lsp_range).await;

        assert_eq!(
            range_a.end.character, 3,
            "must convert against a.rs's own content"
        );
        assert_eq!(
            range_b.end.character, 4,
            "must convert against b.rs's own content"
        );
    }
}
