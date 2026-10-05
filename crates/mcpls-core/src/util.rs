//! Small helpers shared across `mcpls-core` modules.

use std::borrow::Cow;
use std::io::Read as _;
use std::num::NonZeroU64;
use std::path::Path;
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
#[derive(Debug)]
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

/// Why [`read_regular_file_bounded`] did not return the file's bytes.
#[derive(thiserror::Error, Debug)]
pub enum BoundedFileError {
    /// Opening or reading the file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The path is not a regular file (a FIFO, device, directory, ...).
    #[error("not a regular file")]
    NotRegular,
    /// The file is larger than the allowed number of bytes.
    #[error("larger than {max} bytes")]
    TooLarge {
        /// The byte limit that was exceeded.
        max: u64,
    },
}

/// Reads the regular file at `path` if it is at most `max` bytes, without ever
/// blocking on a special file.
///
/// On Unix the open uses `O_NONBLOCK`, so opening a FIFO (or a symlink to
/// one) returns at once instead of waiting for a writer, and the file type is
/// then checked on the open handle rather than on a separately stat'd path.
/// `O_NOCTTY` keeps a symlink to a tty from becoming the controlling terminal.
/// On Windows the handle must report `FILE_TYPE_DISK`, which rejects device
/// names such as `NUL`; Win32 has no non-blocking open, so the open itself
/// can still wait on a hostile path (see #442).
///
/// # Errors
///
/// [`BoundedFileError::NotRegular`] for anything but a regular file,
/// [`BoundedFileError::TooLarge`] past `max` bytes, and
/// [`BoundedFileError::Io`] for open or read failures.
pub fn read_regular_file_bounded(
    path: &Path,
    max: NonZeroU64,
) -> Result<Vec<u8>, BoundedFileError> {
    let max = max.get();
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    let file = options.open(path)?;
    #[cfg(windows)]
    if !winapi_util::file::typ(&file).is_ok_and(|file_type| file_type.is_disk()) {
        return Err(BoundedFileError::NotRegular);
    }
    if !file.metadata()?.is_file() {
        return Err(BoundedFileError::NotRegular);
    }
    let mut buf = Vec::new();
    file.take(bounded_read_cap(max)).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        return Err(BoundedFileError::TooLarge { max });
    }
    Ok(buf)
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
/// server), so the cut point is found via `floor_char_boundary` rather than a
/// raw byte index, which would panic if it fell inside a multi-byte codepoint.
///
/// Always allocates a fresh `String`, even when `s` is already within the
/// limit. Prefer [`truncate_string`] when the caller already owns `s` and
/// truncation is expected to be rare, to skip that allocation on the common
/// path.
pub fn truncate_str(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let cut = s.floor_char_boundary(max_bytes);
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
    let cut = s.floor_char_boundary(max_bytes);
    s.truncate(cut);
    s.push_str(TRUNCATION_MARKER);
    s
}

/// Characters that forge or reorder text without being visible: zero-width
/// and bidirectional marks, line/paragraph separators and bidi overrides.
pub const fn is_deceptive_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// Whether `c` could forge a log line or reorder displayed text, i.e. whether
/// [`escape_control`] rewrites it.
///
/// # Examples
///
/// ```
/// assert!(mcpls_core::needs_control_escape('\n'));
/// assert!(!mcpls_core::needs_control_escape('a'));
/// ```
#[must_use]
pub const fn needs_control_escape(c: char) -> bool {
    c.is_control() || is_deceptive_format_char(c)
}

/// Escape every control character, line/paragraph separator and bidi control
/// in `s` (`\n`, `\r`, `\t` by name, the rest as `\u{..}`), borrowing `s`
/// unchanged when it has none.
///
/// Applied wherever attacker-influenceable text (an LSP server's message)
/// reaches an error `Display`, and by the `mcpls` binary to every log field,
/// so it cannot forge log lines or reorder what an operator reads.
///
/// # Examples
///
/// ```
/// assert_eq!(mcpls_core::escape_control("ok\nERROR forged"), "ok\\nERROR forged");
/// assert_eq!(mcpls_core::escape_control("plain"), "plain");
/// ```
pub fn escape_control(s: &str) -> Cow<'_, str> {
    if !s.chars().any(needs_control_escape) {
        return Cow::Borrowed(s);
    }
    let mut escaped = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if needs_control_escape(c) => escaped.extend(c.escape_unicode()),
            c => escaped.push(c),
        }
    }
    Cow::Owned(escaped)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::assert_matches;

    use super::*;

    #[test]
    fn read_regular_file_bounded_reads_within_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(read_regular_file_bounded(&path, limit(2)).unwrap(), b"{}");
        let empty = dir.path().join("empty.json");
        std::fs::write(&empty, b"").unwrap();
        assert!(
            read_regular_file_bounded(&empty, limit(2))
                .unwrap()
                .is_empty()
        );
    }

    fn limit(max: u64) -> NonZeroU64 {
        NonZeroU64::new(max).unwrap()
    }

    #[test]
    fn read_regular_file_bounded_rejects_oversize() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        std::fs::write(&path, b"{ }").unwrap();
        assert_matches!(
            read_regular_file_bounded(&path, limit(2)),
            Err(BoundedFileError::TooLarge { max: 2 })
        );
    }

    #[test]
    fn read_regular_file_bounded_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_regular_file_bounded(dir.path(), limit(10));
        #[cfg(unix)]
        assert_matches!(result, Err(BoundedFileError::NotRegular));
        // Windows refuses to open a directory as a file before the type check runs.
        #[cfg(windows)]
        assert_matches!(result, Err(BoundedFileError::Io(ref e)) if e.kind() == std::io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn read_regular_file_bounded_does_not_block_on_fifo_or_symlink_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo must succeed to set up this test");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&fifo, &link).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcomes = [fifo, link].map(|path| read_regular_file_bounded(&path, limit(10)));
            tx.send(outcomes).unwrap();
        });
        let outcomes = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("reading a FIFO must not block"));
        for outcome in outcomes {
            assert_matches!(outcome, Err(BoundedFileError::NotRegular));
        }
    }

    #[test]
    fn escape_control_borrows_clean_text() {
        assert_matches!(escape_control("plain ✓"), Cow::Borrowed("plain ✓"));
    }

    #[test]
    fn escape_control_escapes_newlines_and_escape_sequences() {
        assert_eq!(
            escape_control("a\nb\r\tc\x1b[31m"),
            "a\\nb\\r\\tc\\u{1b}[31m"
        );
    }

    #[test]
    fn escape_control_escapes_line_separators_and_bidi_controls() {
        assert_eq!(
            escape_control("a\u{2028}b\u{2029}c\u{202E}d\u{2066}e\u{200F}f\u{061C}g"),
            "a\\u{2028}b\\u{2029}c\\u{202e}d\\u{2066}e\\u{200f}f\\u{61c}g"
        );
    }

    #[test]
    fn escape_control_escapes_forged_log_line() {
        assert_eq!(escape_control("ok\nERROR forged"), "ok\\nERROR forged");
    }

    #[test]
    fn truncate_with_zero_budget_keeps_only_marker() {
        assert_eq!(truncate_str("é", 0), TRUNCATION_MARKER);
        assert_eq!(truncate_string("abc".to_owned(), 0), TRUNCATION_MARKER);
    }

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
        assert_matches!(outcome, BoundedReadOutcome::Ok(s) if s == "hello");
    }

    #[test]
    fn check_bounded_utf8_too_large() {
        let outcome = check_bounded_utf8(b"hello".to_vec(), 4);
        assert_matches!(outcome, BoundedReadOutcome::TooLarge { size: 5 });
    }

    #[test]
    fn check_bounded_utf8_unlimited_when_max_zero() {
        let outcome = check_bounded_utf8(b"a".repeat(1000), 0);
        assert_matches!(outcome, BoundedReadOutcome::Ok(s) if s.len() == 1000);
    }

    #[test]
    fn check_bounded_utf8_invalid_utf8_within_limit() {
        let outcome = check_bounded_utf8(vec![0xFF, 0xFE], 10);
        assert_matches!(outcome, BoundedReadOutcome::InvalidUtf8(_));
    }

    /// A multibyte character split by the bound must be reported as
    /// oversized, not as invalid UTF-8 -- the ordering this helper exists to
    /// preserve across both call sites.
    #[test]
    fn check_bounded_utf8_reports_oversized_before_invalid_utf8() {
        let mut buf = "é".repeat(3).into_bytes();
        buf.truncate(5);
        let outcome = check_bounded_utf8(buf, 4);
        assert_matches!(outcome, BoundedReadOutcome::TooLarge { size: 5 });
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
