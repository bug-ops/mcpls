//! Acceptance tests for #909: `DocumentTracker::excerpt` cuts a span out of the
//! pinned text only while the document is still the one the span was computed on.

use std::assert_matches;
use std::num::{NonZeroU32, NonZeroUsize};

use super::*;
use crate::bridge::{Position2D, Range};
use crate::config::ServerId;

fn at(line: u32, character: u32) -> Position2D {
    Position2D::at(line, character)
}

fn range(start: (u32, u32), end: (u32, u32)) -> Range {
    Range {
        start: at(start.0, start.1),
        end: at(end.0, end.1),
    }
}

fn limits(max_lines: u32, max_bytes: usize) -> ExcerptLimits {
    ExcerptLimits {
        max_lines: NonZeroU32::new(max_lines).unwrap(),
        max_bytes: NonZeroUsize::new(max_bytes).unwrap(),
    }
}

fn roomy() -> ExcerptLimits {
    limits(200, 16 * 1024)
}

struct Tracked {
    tracker: DocumentTracker,
    path: PathBuf,
    server: ServerId,
    stamp: DocumentStamp,
}

fn tracked(text: &str) -> Tracked {
    let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
    let path = PathBuf::from("/test/excerpt.rs");
    let server = ServerId::from_static("rust");
    tracker.open(path.clone(), text.to_owned()).unwrap();
    tracker.set_synced_version_for_test(&path, &server, 1);
    let stamp = tracker.stamp(&path, &server).unwrap();
    Tracked {
        tracker,
        path,
        server,
        stamp,
    }
}

impl Tracked {
    fn excerpt(
        &self,
        span: &Range,
        limits: ExcerptLimits,
    ) -> std::result::Result<Excerpt, ExcerptError> {
        self.tracker
            .excerpt(&self.path, &self.server, self.stamp, span, limits)
    }

    fn text(&self, span: &Range) -> String {
        self.excerpt(span, roomy()).expect("excerpt").text
    }
}

#[test]
fn test_excerpt_returns_the_exact_multi_line_slice() {
    let doc = tracked("fn main() {\n    body();\n}\nfn other() {}\n");

    let excerpt = doc.excerpt(&range((1, 1), (3, 2)), roomy()).unwrap();

    assert_eq!(excerpt.text, "fn main() {\n    body();\n}");
    assert!(!excerpt.truncated);
}

#[test]
fn test_excerpt_returns_a_slice_inside_one_line() {
    let doc = tracked("fn main() {}\n");

    assert_eq!(doc.text(&range((1, 4), (1, 8))), "main");
}

#[test]
fn test_excerpt_joins_crlf_lines_with_a_line_feed() {
    let doc = tracked("a\r\nb\r\nc");

    assert_eq!(doc.text(&range((1, 1), (2, 2))), "a\nb");
}

#[test]
fn test_excerpt_end_past_the_line_end_means_the_line_end() {
    let doc = tracked("abc\ndef\n");

    assert_eq!(doc.text(&range((1, 2), (1, 50))), "bc");
    assert_eq!(doc.text(&range((1, 2), (1, u32::MAX))), "bc");
    assert_eq!(doc.text(&range((1, 1), (2, u32::MAX))), "abc\ndef");
}

#[test]
fn test_excerpt_start_inside_a_surrogate_pair_rounds_down_and_keeps_the_line() {
    let doc = tracked("a\u{1F600}b\n");

    assert_eq!(doc.text(&range((1, 3), (1, 5))), "\u{1F600}b");
}

#[test]
fn test_excerpt_end_inside_a_surrogate_pair_rounds_up() {
    let doc = tracked("a\u{1F600}b\n");

    assert_eq!(doc.text(&range((1, 1), (1, 3))), "a\u{1F600}");
}

#[test]
fn test_excerpt_cuts_at_the_line_limit_and_reports_truncation() {
    let doc = tracked("1\n2\n3\n4\n5\n");

    let cut = doc
        .excerpt(&range((1, 1), (5, 2)), limits(2, 1024))
        .unwrap();
    let fit = doc
        .excerpt(&range((1, 1), (2, 2)), limits(2, 1024))
        .unwrap();

    assert_eq!((cut.text.as_str(), cut.truncated), ("1\n2", true));
    assert_eq!((fit.text.as_str(), fit.truncated), ("1\n2", false));
}

#[test]
fn test_excerpt_byte_cut_lands_on_a_line_boundary() {
    let line = "z".repeat(100);
    let doc = tracked(&format!("{line}\n").repeat(30));

    let excerpt = doc
        .excerpt(&range((1, 1), (30, 101)), limits(200, 1000))
        .unwrap();

    assert!(excerpt.truncated);
    assert!(excerpt.text.len() <= 1000);
    assert!(
        excerpt.text.split('\n').all(|l| l == line),
        "whole lines only"
    );
}

#[test]
fn test_excerpt_of_a_huge_single_line_is_bounded_and_char_aligned() {
    let line = "\u{e9}".repeat(2_000_000);
    let doc = tracked(&line);
    let cap = 16 * 1024;

    let excerpt = doc
        .excerpt(&range((1, 1), (1, u32::MAX)), limits(200, cap))
        .unwrap();

    assert!(excerpt.truncated);
    assert!(excerpt.text.len() <= cap);
    assert!(line.starts_with(&excerpt.text));
}

#[test]
fn test_excerpt_is_changed_after_a_reload_the_server_was_not_told_about() {
    let doc = tracked("fn a() {}\n");
    lock_std(&doc.tracker.documents)
        .get_mut(&doc.path)
        .unwrap()
        .commit_reload(
            DocumentVersion::new(2),
            DocumentText::new("fn b() {}\n".to_owned()),
            None,
        );

    let result = doc.excerpt(&range((1, 1), (1, 10)), roomy());

    assert_matches!(result, Err(ExcerptError::Changed));
}

#[test]
fn test_excerpt_is_changed_when_the_synced_version_moved() {
    let doc = tracked("fn a() {}\n");
    doc.tracker
        .set_synced_version_for_test(&doc.path, &doc.server, 7);

    let result = doc.excerpt(&range((1, 1), (1, 10)), roomy());

    assert_matches!(result, Err(ExcerptError::Changed));
}

#[test]
fn test_excerpt_is_changed_when_the_path_was_reopened() {
    let doc = tracked("fn a() {}\n");
    doc.tracker.close(&doc.path);
    doc.tracker
        .open(doc.path.clone(), "fn a() {}\n".to_owned())
        .unwrap();
    doc.tracker
        .set_synced_version_for_test(&doc.path, &doc.server, 1);

    let result = doc.excerpt(&range((1, 1), (1, 10)), roomy());

    assert_matches!(result, Err(ExcerptError::Changed));
}

#[test]
fn test_excerpt_is_untracked_once_the_document_is_gone() {
    let doc = tracked("fn a() {}\n");
    doc.tracker.close(&doc.path);

    let result = doc.excerpt(&range((1, 1), (1, 10)), roomy());

    assert_matches!(result, Err(ExcerptError::Untracked));
}
