//! Work bounds of `DocumentTracker::excerpt`: a cut copies at most the byte
//! cap and scans at most the start column plus the cap.

use std::num::{NonZeroU32, NonZeroUsize};

use super::*;
use crate::bridge::{Position2D, Range};
use crate::config::ServerId;

const CAP: usize = 16 * 1024;
const LINE_UNITS: u32 = 2_000_000;

fn tracked_wide_line() -> (DocumentTracker, PathBuf, ServerId, String) {
    let line = "\u{e9}".repeat(LINE_UNITS as usize);
    let tracker = DocumentTracker::new(ResourceLimits::default(), HashMap::new());
    let path = PathBuf::from("/test/wide.rs");
    let server = ServerId::from_static("rust");
    tracker.open(path.clone(), line.clone()).unwrap();
    tracker.set_synced_version_for_test(&path, &server, 1);
    (tracker, path, server, line)
}

fn limits() -> ExcerptLimits {
    ExcerptLimits {
        max_lines: NonZeroU32::new(200).unwrap(),
        max_bytes: NonZeroUsize::new(CAP).unwrap(),
    }
}

#[test]
fn test_excerpt_of_a_4_mib_line_from_column_zero_stays_within_the_cap() {
    let (tracker, path, server, line) = tracked_wide_line();
    let stamp = tracker.stamp(&path, &server).unwrap();
    let span = Range {
        start: Position2D::at(1, 1),
        end: Position2D::at(1, LINE_UNITS),
    };

    let excerpt = tracker
        .excerpt(&path, &server, stamp, &span, limits())
        .unwrap();

    assert!(excerpt.truncated);
    assert!(excerpt.text.len() <= CAP);
    assert!(line.starts_with(&excerpt.text));
}

#[test]
fn test_excerpt_from_the_middle_of_a_4_mib_line_starts_at_the_column() {
    let (tracker, path, server, line) = tracked_wide_line();
    let stamp = tracker.stamp(&path, &server).unwrap();
    let start = 1_000_000_u32;
    let span = Range {
        start: Position2D::at(1, start + 1),
        end: Position2D::at(1, LINE_UNITS),
    };

    let excerpt = tracker
        .excerpt(&path, &server, stamp, &span, limits())
        .unwrap();

    let from = start as usize * '\u{e9}'.len_utf8();
    assert!(excerpt.truncated);
    assert!(excerpt.text.len() <= CAP);
    assert!(line[from..].starts_with(&excerpt.text));
}

#[test]
fn test_excerpt_that_fits_in_the_cap_is_not_truncated_on_a_long_line() {
    let (tracker, path, server, line) = tracked_wide_line();
    let stamp = tracker.stamp(&path, &server).unwrap();
    let span = Range {
        start: Position2D::at(1, 10),
        end: Position2D::at(1, 20),
    };

    let excerpt = tracker
        .excerpt(&path, &server, stamp, &span, limits())
        .unwrap();

    assert!(!excerpt.truncated);
    assert_eq!(excerpt.text, line[18..38]);
}

#[test]
fn test_excerpt_looks_at_a_bounded_prefix_of_a_line_without_a_terminator() {
    let line = "\u{e9}".repeat(LINE_UNITS as usize);
    let text = DocumentText::new(line.clone());
    let start = 1_000_u32;
    let span = Range {
        start: Position2D::at(1, start + 1),
        end: Position2D::at(1, LINE_UNITS),
    };

    let (excerpt, scanned) = cut_excerpt_scanned(&text, &span, limits());

    assert!(excerpt.truncated);
    assert!(excerpt.text.len() <= CAP);
    assert!(
        scanned <= start as usize * MAX_BYTES_PER_UNIT + CAP + 4,
        "{scanned}"
    );
    assert!(scanned < line.len() / 100);
}

#[test]
fn test_excerpt_scan_bound_holds_across_short_lines_and_a_wide_middle_line() {
    let wide = "a".repeat(1_000_000);
    let text = DocumentText::new(format!("a\nb\n{wide}\n{wide}"));
    let span = Range {
        start: Position2D::at(1, 1),
        end: Position2D::at(4, 1_000_000),
    };

    let (excerpt, scanned) = cut_excerpt_scanned(&text, &span, limits());

    assert!(excerpt.truncated);
    assert_eq!(excerpt.text, "a\nb");
    assert!(scanned <= 3 * (CAP + 4), "{scanned}");
}

#[test]
fn test_excerpt_scan_bound_holds_for_a_wide_last_line() {
    let wide = "a".repeat(1_000_000);
    let text = DocumentText::new(format!("a\nb\n{wide}"));
    let span = Range {
        start: Position2D::at(1, 1),
        end: Position2D::at(3, 1_000_000),
    };

    let (excerpt, scanned) = cut_excerpt_scanned(&text, &span, limits());

    assert!(excerpt.truncated);
    assert_eq!(excerpt.text, "a\nb");
    assert!(scanned <= 3 * (CAP + 4), "{scanned}");
}
