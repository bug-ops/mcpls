//! Small helpers shared across `mcpls-core` modules.

/// Marker appended to a truncated string; the returned string can be up to
/// `max_bytes + TRUNCATION_MARKER.len()` bytes, not exactly `max_bytes`.
const TRUNCATION_MARKER: &str = "... (truncated)";

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
