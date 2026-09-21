//! Small helpers shared across `mcpls-core` modules.

use std::string::FromUtf8Error;

/// Byte cap for a bounded read against a `max`-byte size limit: `max + 1`
/// when `max` is a real limit, so a read that reaches the cap is known to
/// have exceeded it, or unbounded (`u64::MAX`) when `max == 0`, the
/// documented "unlimited" sentinel used by
/// [`crate::bridge::state::ResourceLimits::max_file_size`].
pub const fn bounded_read_cap(max: u64) -> u64 {
    if max == 0 {
        u64::MAX
    } else {
        max.saturating_add(1)
    }
}

/// Outcome of checking a bounded read's raw bytes against `max` and decoding
/// them as UTF-8.
pub enum BoundedReadOutcome {
    /// `buf` was within `max` bytes and valid UTF-8.
    Ok(String),
    /// `buf` was longer than `max` bytes; carries the actual byte count.
    TooLarge {
        /// Number of bytes actually read.
        size: u64,
    },
    /// `buf` was within `max` bytes but not valid UTF-8.
    InvalidUtf8(FromUtf8Error),
}

/// Checks `buf`'s length against `max` *before* UTF-8-validating it, so that
/// a multibyte character split by a bounded read's cap (see
/// [`bounded_read_cap`]) is reported as oversized rather than as invalid
/// UTF-8. `max == 0` means unlimited -- the size check is skipped in that
/// case, matching [`bounded_read_cap`]'s sentinel.
///
/// Callers own the bounded read itself (sync or async filesystem I/O
/// differs by caller) and map the outcome onto their own error type.
pub fn check_bounded_utf8(buf: Vec<u8>, max: u64) -> BoundedReadOutcome {
    if max != 0 && buf.len() as u64 > max {
        return BoundedReadOutcome::TooLarge {
            size: buf.len() as u64,
        };
    }
    match String::from_utf8(buf) {
        Ok(s) => BoundedReadOutcome::Ok(s),
        Err(e) => BoundedReadOutcome::InvalidUtf8(e),
    }
}

/// Marker appended to a truncated string; the returned string can be up to
/// `max_bytes + TRUNCATION_MARKER.len()` bytes, not exactly `max_bytes`.
const TRUNCATION_MARKER: &str = "... (truncated)";

/// Byte-length threshold for truncating an attacker-influenceable string
/// (an LSP server's error message, a malformed protocol line, ...) before
/// passing it to `truncate_str`/`truncate_string` for a single log line.
/// Shared by every call site with this purpose so they don't each pick their
/// own value -- see `lsp::client::MAX_ERROR_MESSAGE_CALLER_BYTES` for the
/// separate, deliberately larger budget for text forwarded to the MCP
/// caller rather than logged.
pub const MAX_LOG_STRING_BYTES: usize = 200;

/// Truncate `s` to at most `max_bytes` bytes, cutting on the last UTF-8 char
/// boundary at or before the limit and appending a truncation marker. The
/// returned string can be up to `max_bytes + TRUNCATION_MARKER.len()` bytes
/// when truncation occurs -- the marker is appended after the cut, not
/// counted against the limit.
///
/// `s` is typically attacker-influenceable (forwarded from a spawned LSP
/// server), so the cut point is found via `char_indices` rather than a raw
/// byte index, which would panic if it fell inside a multi-byte codepoint.
///
/// Always allocates a fresh `String`, even when `s` is already within the
/// limit. Prefer [`truncate_string`] when the caller already owns `s` and
/// truncation is expected to be rare, to skip that allocation on the common
/// path.
pub fn truncate_str(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let cut = s
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= max_bytes)
        .last()
        .unwrap_or(0);
    format!("{}{TRUNCATION_MARKER}", &s[..cut])
}

/// Truncate an owned `String` to at most `max_bytes` bytes in place (same
/// cut/marker semantics as [`truncate_str`]), returning `s` unchanged and
/// without allocating when it is already within the limit.
///
/// This is the common case on the hot paths that call it --
/// `NotificationCache::store_log`/`store_message` on every
/// `window/logMessage`/`showMessage`, and each diagnostic's `message` field
/// on every `publishDiagnostics` -- where [`truncate_str`]'s unconditional
/// `s.to_string()` would otherwise clone the message on every call just to
/// hand back an equivalent copy.
pub fn truncate_string(mut s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    let cut = s
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= max_bytes)
        .last()
        .unwrap_or(0);
    s.truncate(cut);
    s.push_str(TRUNCATION_MARKER);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_read_cap_is_max_plus_one() {
        assert_eq!(bounded_read_cap(100), 101);
        assert_eq!(bounded_read_cap(u64::MAX - 1), u64::MAX);
    }

    #[test]
    fn bounded_read_cap_zero_means_unlimited() {
        assert_eq!(bounded_read_cap(0), u64::MAX);
    }

    #[test]
    fn check_bounded_utf8_within_limit() {
        let outcome = check_bounded_utf8(b"hello".to_vec(), 10);
        assert!(matches!(outcome, BoundedReadOutcome::Ok(s) if s == "hello"));
    }

    #[test]
    fn check_bounded_utf8_too_large() {
        let outcome = check_bounded_utf8(b"hello".to_vec(), 4);
        assert!(matches!(outcome, BoundedReadOutcome::TooLarge { size: 5 }));
    }

    #[test]
    fn check_bounded_utf8_unlimited_when_max_zero() {
        let outcome = check_bounded_utf8(b"a".repeat(1000), 0);
        assert!(matches!(outcome, BoundedReadOutcome::Ok(s) if s.len() == 1000));
    }

    #[test]
    fn check_bounded_utf8_invalid_utf8_within_limit() {
        let outcome = check_bounded_utf8(vec![0xFF, 0xFE], 10);
        assert!(matches!(outcome, BoundedReadOutcome::InvalidUtf8(_)));
    }

    /// A multibyte character split by the bound must be reported as
    /// oversized, not as invalid UTF-8 -- the ordering this helper exists to
    /// preserve across both call sites.
    #[test]
    fn check_bounded_utf8_reports_oversized_before_invalid_utf8() {
        let mut buf = "é".repeat(3).into_bytes();
        buf.truncate(5);
        let outcome = check_bounded_utf8(buf, 4);
        assert!(matches!(outcome, BoundedReadOutcome::TooLarge { size: 5 }));
    }

    #[test]
    fn no_truncation_at_or_below_limit() {
        let exact = "a".repeat(10);
        assert_eq!(truncate_str(&exact, 10), exact);
        assert_eq!(truncate_str("", 10), "");
    }

    #[test]
    fn truncates_just_above_limit() {
        let message = "a".repeat(11);
        assert_eq!(
            truncate_str(&message, 10),
            format!("{}... (truncated)", "a".repeat(10))
        );
    }

    #[test]
    fn handles_multibyte_char_boundary() {
        // Each 'é' is 2 bytes; a raw byte-index cut at 5 would fall inside one.
        let message = "é".repeat(10);
        let truncated = truncate_str(&message, 5);
        assert!(truncated.starts_with(&"é".repeat(2)));
        assert!(truncated.ends_with("... (truncated)"));
    }

    #[test]
    fn truncate_string_no_truncation_at_or_below_limit() {
        let exact = "a".repeat(10);
        assert_eq!(truncate_string(exact.clone(), 10), exact);
        assert_eq!(truncate_string(String::new(), 10), "");
    }

    #[test]
    fn truncate_string_truncates_just_above_limit() {
        let message = "a".repeat(11);
        assert_eq!(
            truncate_string(message, 10),
            format!("{}... (truncated)", "a".repeat(10))
        );
    }

    #[test]
    fn truncate_string_handles_multibyte_char_boundary() {
        let message = "é".repeat(10);
        let truncated = truncate_string(message, 5);
        assert!(truncated.starts_with(&"é".repeat(2)));
        assert!(truncated.ends_with("... (truncated)"));
    }

    #[test]
    fn truncate_str_and_truncate_string_agree() {
        let message = "x".repeat(500);
        assert_eq!(truncate_str(&message, 100), truncate_string(message, 100));
    }
}
