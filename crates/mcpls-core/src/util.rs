//! Small helpers shared across `mcpls-core` modules.

use std::borrow::Cow;
use std::future::Future;
use std::io::Read as _;
use std::num::NonZeroU64;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::string::FromUtf8Error;
use std::sync::{Mutex as StdMutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use tokio::task::JoinHandle;

use crate::config::SizeLimit;

/// A byte count that went over its limit.
///
/// The one shape of every "too large" error, so each reader reports the same
/// two numbers and converts it without re-spelling the fields. Only
/// [`Self::check`] and [`Self::check_limit`] build one, so `size` is always
/// above `max`.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("{size} bytes exceed the limit of {max} bytes")]
pub struct SizeExceeded {
    pub(crate) size: u64,
    pub(crate) max: NonZeroU64,
}

impl SizeExceeded {
    /// The size found, in bytes.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// The limit that was exceeded, in bytes; always below [`Self::size`].
    #[must_use]
    pub const fn max(&self) -> NonZeroU64 {
        self.max
    }

    /// Checks `size` against `max`.
    ///
    /// # Errors
    ///
    /// Returns the exceeded limit when `size` is over `max`.
    pub const fn check(size: u64, max: NonZeroU64) -> Result<(), Self> {
        if size > max.get() {
            return Err(Self { size, max });
        }
        Ok(())
    }

    /// Checks `size` against `limit`, which admits everything when unlimited.
    ///
    /// # Errors
    ///
    /// Returns the exceeded limit when `size` is over `limit`.
    pub const fn check_limit(size: u64, limit: SizeLimit) -> Result<(), Self> {
        match limit.get() {
            Some(max) => Self::check(size, max),
            None => Ok(()),
        }
    }
}

/// Why [`check_bounded_utf8`] rejected the bytes of a bounded read.
#[derive(thiserror::Error, Debug)]
pub enum BoundedUtf8Error {
    /// The bytes were longer than the limit.
    #[error(transparent)]
    TooLarge(#[from] SizeExceeded),
    /// The bytes were within the limit but not valid UTF-8.
    #[error(transparent)]
    InvalidUtf8(#[from] FromUtf8Error),
}

/// Checks `buf`'s length against `max` *before* UTF-8-validating it, so that
/// a multibyte character split by a bounded read's cap (see
/// [`SizeLimit::read_cap`]) is reported as oversized rather than as invalid
/// UTF-8.
///
/// Callers own the bounded read itself (sync or async filesystem I/O
/// differs by caller) and map the error onto their own error type.
///
/// # Errors
///
/// [`BoundedUtf8Error::TooLarge`] past `max`, otherwise
/// [`BoundedUtf8Error::InvalidUtf8`].
pub fn check_bounded_utf8(buf: Vec<u8>, max: SizeLimit) -> Result<String, BoundedUtf8Error> {
    SizeExceeded::check_limit(buf.len() as u64, max)?;
    Ok(String::from_utf8(buf)?)
}

/// Why [`RegularFile::open`] refused a path.
#[derive(thiserror::Error, Debug)]
pub enum OpenRegularFileError {
    /// Opening the path or reading its metadata failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The path is not a regular file (a FIFO, device, directory, ...).
    #[error("not a regular file")]
    NotRegular,
}

/// Why [`RegularFile::read_bounded`] did not return the file's bytes.
#[derive(thiserror::Error, Debug)]
pub enum ReadBoundedError {
    /// Reading the open file failed.
    #[error(transparent)]
    Io(std::io::Error),
    /// The file is larger than the allowed number of bytes; the size is its
    /// stat length, or the bytes read when the stat length understated them.
    #[error(transparent)]
    TooLarge(SizeExceeded),
}

/// Why [`read_regular_file_bounded`] did not return the file's bytes.
#[derive(thiserror::Error, Debug)]
pub enum BoundedFileError {
    /// The path could not be opened as a regular file.
    #[error(transparent)]
    Open(#[from] OpenRegularFileError),
    /// The open file could not be read within the limit.
    #[error(transparent)]
    Read(#[from] ReadBoundedError),
}

/// An open handle proven to refer to a regular file.
///
/// The type is checked on the handle itself, not on a separately stat'd path,
/// so an atomic replace between check and open cannot swap in something else.
/// On Unix the open uses `O_NONBLOCK`, so opening a FIFO (or a symlink to
/// one) returns at once instead of waiting for a writer, and `O_NOCTTY` keeps
/// a symlink to a tty from becoming the controlling terminal. On Windows the
/// handle must report `FILE_TYPE_DISK`, which rejects device names such as
/// `NUL`; Win32 has no non-blocking open, so the open itself can still wait on
/// a hostile path (see #442).
#[derive(Debug)]
pub struct RegularFile {
    file: std::fs::File,
    metadata: std::fs::Metadata,
}

impl RegularFile {
    /// Opens `path` and verifies on the open handle that it is a regular file.
    ///
    /// # Errors
    ///
    /// [`OpenRegularFileError::NotRegular`] for anything but a regular file
    /// and [`OpenRegularFileError::Io`] for open or metadata failures.
    pub fn open(path: &Path) -> Result<Self, OpenRegularFileError> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
        }
        let file = options.open(path)?;
        // Must precede metadata(): GetFileInformationByHandle may fail for non-disk handles.
        #[cfg(windows)]
        if !winapi_util::file::typ(&file).is_ok_and(|file_type| file_type.is_disk()) {
            return Err(OpenRegularFileError::NotRegular);
        }
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(OpenRegularFileError::NotRegular);
        }
        Ok(Self { file, metadata })
    }

    /// Splits into the open handle and its metadata.
    #[must_use]
    pub fn into_parts(self) -> (std::fs::File, std::fs::Metadata) {
        (self.file, self.metadata)
    }

    /// Reads the file if it is at most `max` bytes.
    ///
    /// The stat length is checked first, then the read itself is capped at
    /// `max + 1` bytes, so a file that grows after the stat is still bounded.
    ///
    /// # Errors
    ///
    /// [`ReadBoundedError::TooLarge`] past `max` bytes and
    /// [`ReadBoundedError::Io`] when the read fails.
    pub fn read_bounded(self, max: NonZeroU64) -> Result<Vec<u8>, ReadBoundedError> {
        SizeExceeded::check(self.metadata.len(), max).map_err(ReadBoundedError::TooLarge)?;
        let mut buf = Vec::new();
        self.file
            .take(SizeLimit::read_cap_for(max))
            .read_to_end(&mut buf)
            .map_err(ReadBoundedError::Io)?;
        SizeExceeded::check(buf.len() as u64, max).map_err(ReadBoundedError::TooLarge)?;
        Ok(buf)
    }
}

/// Reads the regular file at `path` if it is at most `max` bytes, without ever
/// blocking on a special file. See [`RegularFile`] for the open semantics.
///
/// # Errors
///
/// [`BoundedFileError::Open`] when the path is not a regular file or cannot
/// be opened and [`BoundedFileError::Read`] when it is too large or unreadable.
pub fn read_regular_file_bounded(
    path: &Path,
    max: NonZeroU64,
) -> Result<Vec<u8>, BoundedFileError> {
    Ok(RegularFile::open(path)?.read_bounded(max)?)
}

/// Marker appended to a truncated string; the returned string can be up to
/// `max_bytes + TRUNCATION_MARKER.len()` bytes, not exactly `max_bytes`.
pub const TRUNCATION_MARKER: &str = "... (truncated)";

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

/// [`escape_control`] for an owned string: clean text is returned as is,
/// without a copy.
pub fn escape_control_owned(text: String) -> String {
    match escape_control(&text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(escaped) => escaped,
    }
}

/// Aborts the wrapped [`JoinHandle`] when dropped, including on an unwind out
/// of the enclosing scope -- unlike a bare `.abort()` call placed at the end
/// of a function body, which is skipped if that scope is left early (a
/// panic, or a future `?` added above it).
pub struct AbortOnDrop<'a, T>(pub(crate) &'a JoinHandle<T>);

impl<T> Drop for AbortOnDrop<'_, T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A future that panicked while being driven by [`catch_panic`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("task panicked: {message}")]
pub struct TaskPanicked {
    message: String,
}

impl TaskPanicked {
    /// Best-effort text of the panic payload.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    fn from_payload(payload: &(dyn std::any::Any + Send)) -> Self {
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic payload");
        Self {
            message: message.to_owned(),
        }
    }
}

/// Drives `fut` to completion, turning a panic inside it into
/// [`TaskPanicked`] so supervised tasks share one containment path.
pub async fn catch_panic<T>(fut: impl Future<Output = T>) -> Result<T, TaskPanicked> {
    AssertUnwindSafe(fut)
        .catch_unwind()
        .await
        .map_err(|payload| TaskPanicked::from_payload(payload.as_ref()))
}

/// Decides when a repeating condition may be logged at `warn` again.
///
/// The first occurrence is due at once, later ones only once `every` has
/// passed since the last one that was due, so a source that repeats cannot
/// flood the log.
#[derive(Debug, Default)]
pub struct WarnLimiter {
    last: Option<Instant>,
}

impl WarnLimiter {
    /// The period most warnings use between two lines.
    pub const DEFAULT_PERIOD: Duration = Duration::from_mins(1);

    /// A limiter whose first warning is due at once.
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Whether a warning is due at `now`; when it is, `now` becomes the new
    /// reference point.
    pub fn due(&mut self, now: Instant, every: Duration) -> bool {
        let due = self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) > every);
        if due {
            self.last = Some(now);
        }
        due
    }
}

/// Locks a `std::sync::Mutex`, recovering the guard if a previous holder
/// panicked while holding it.
///
/// Every lock guarded this way protects a short, synchronous, panic-free
/// critical section (a `HashMap`/`HashSet` lookup or insert), so poisoning
/// can only happen if an unrelated bug already panicked; refusing to unwind
/// the whole process a second time over stale poisoning is preferable to
/// deadlocking future calls.
pub fn lock_std<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;

    #[test]
    fn warn_limiter_is_due_at_first_and_then_once_per_interval() {
        let every = Duration::from_secs(60);
        let start = Instant::now();
        let mut limiter = WarnLimiter::default();
        assert!(limiter.due(start, every));
        assert!(!limiter.due(start + Duration::from_secs(1), every));
        assert!(!limiter.due(start + every, every));
        assert!(limiter.due(start + every + Duration::from_millis(1), every));
        assert!(!limiter.due(start + every + Duration::from_secs(2), every));
    }

    #[tokio::test]
    async fn catch_panic_passes_value_through() {
        assert_eq!(catch_panic(async { 7 }).await, Ok(7));
    }

    #[tokio::test]
    async fn catch_panic_reports_str_and_string_payloads() {
        let from_str = catch_panic(async { panic!("static boom") }).await;
        assert_eq!(from_str.unwrap_err().message(), "static boom");
        let detail = 42;
        let from_string = catch_panic(async { panic!("boom {detail}") }).await;
        assert_eq!(from_string.unwrap_err().message(), "boom 42");
    }

    #[test]
    fn lock_std_recovers_poisoned_mutex() {
        let mutex = std::sync::Arc::new(StdMutex::new(1));
        let poisoner = std::sync::Arc::clone(&mutex);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert_eq!(*lock_std(&mutex), 1);
    }

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
    fn regular_file_open_into_parts_returns_handle_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"abc").unwrap();
        let (_handle, metadata) = RegularFile::open(&path).unwrap().into_parts();
        assert!(metadata.is_file());
        assert_eq!(metadata.len(), 3);
    }

    #[test]
    fn regular_file_open_missing_path_is_io_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert_matches!(
            RegularFile::open(&dir.path().join("missing")),
            Err(OpenRegularFileError::Io(ref e)) if e.kind() == std::io::ErrorKind::NotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn regular_file_open_rejects_directory_and_fifo_as_not_regular() {
        let dir = tempfile::tempdir().unwrap();
        assert_matches!(
            RegularFile::open(dir.path()),
            Err(OpenRegularFileError::NotRegular)
        );
        let fifo = dir.path().join("fifo");
        crate::test_lsp::make_fifo(&fifo);
        assert_matches!(
            RegularFile::open(&fifo),
            Err(OpenRegularFileError::NotRegular)
        );
    }

    #[cfg(unix)]
    #[test]
    fn regular_file_open_follows_symlink_to_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, b"abc").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let (_handle, metadata) = RegularFile::open(&link).unwrap().into_parts();
        assert!(metadata.is_file());
        assert_eq!(metadata.len(), 3);
    }

    #[cfg(windows)]
    #[test]
    fn regular_file_open_rejects_nul_device_as_not_regular() {
        assert_matches!(
            RegularFile::open(Path::new("NUL")),
            Err(OpenRegularFileError::NotRegular)
        );
    }

    #[test]
    fn read_regular_file_bounded_rejects_oversize() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        std::fs::write(&path, b"{ }").unwrap();
        assert_matches!(
            read_regular_file_bounded(&path, limit(2)),
            Err(BoundedFileError::Read(ReadBoundedError::TooLarge(SizeExceeded { size: 3, max }))) if max == limit(2)
        );
    }

    #[test]
    fn read_regular_file_bounded_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_regular_file_bounded(dir.path(), limit(10));
        #[cfg(unix)]
        assert_matches!(
            result,
            Err(BoundedFileError::Open(OpenRegularFileError::NotRegular))
        );
        // Windows refuses to open a directory as a file before the type check runs.
        #[cfg(windows)]
        assert_matches!(result, Err(BoundedFileError::Open(OpenRegularFileError::Io(ref e))) if e.kind() == std::io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn read_regular_file_bounded_does_not_block_on_fifo_or_symlink_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        crate::test_lsp::make_fifo(&fifo);
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
            assert_matches!(
                outcome,
                Err(BoundedFileError::Open(OpenRegularFileError::NotRegular))
            );
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

    fn size(max: u64) -> SizeLimit {
        SizeLimit::from_static(max)
    }

    #[test]
    fn size_exceeded_exposes_both_numbers_through_accessors() {
        let max = NonZeroU64::new(4).unwrap();
        let exceeded = SizeExceeded::check(9, max).unwrap_err();

        assert_eq!((exceeded.size(), exceeded.max()), (9, max));
    }

    #[test]
    fn size_exceeded_check_admits_up_to_the_limit_and_reports_both_numbers() {
        let max = NonZeroU64::new(4).unwrap();
        assert_eq!(SizeExceeded::check(4, max), Ok(()));
        assert_eq!(
            SizeExceeded::check(5, max),
            Err(SizeExceeded { size: 5, max })
        );
        assert_eq!(
            SizeExceeded::check_limit(u64::MAX, SizeLimit::UNLIMITED),
            Ok(())
        );
        assert_eq!(
            SizeExceeded::check_limit(5, size(4)),
            Err(SizeExceeded { size: 5, max })
        );
        assert_eq!(
            SizeExceeded { size: 5, max }.to_string(),
            "5 bytes exceed the limit of 4 bytes"
        );
    }

    #[test]
    fn check_bounded_utf8_within_limit() {
        assert_eq!(
            check_bounded_utf8(b"hello".to_vec(), size(10)).unwrap(),
            "hello"
        );
    }

    #[test]
    fn check_bounded_utf8_too_large() {
        assert_matches!(
            check_bounded_utf8(b"hello".to_vec(), size(4)),
            Err(BoundedUtf8Error::TooLarge(SizeExceeded { size: 5, max })) if max.get() == 4
        );
    }

    #[test]
    fn check_bounded_utf8_unlimited() {
        let text = check_bounded_utf8(b"a".repeat(1000), SizeLimit::UNLIMITED).unwrap();
        assert_eq!(text.len(), 1000);
    }

    #[test]
    fn check_bounded_utf8_invalid_utf8_within_limit() {
        assert_matches!(
            check_bounded_utf8(vec![0xFF, 0xFE], size(10)),
            Err(BoundedUtf8Error::InvalidUtf8(_))
        );
    }

    /// A multibyte character split by the bound must be reported as
    /// oversized, not as invalid UTF-8 -- the ordering this helper exists to
    /// preserve across both call sites.
    #[test]
    fn check_bounded_utf8_reports_oversized_before_invalid_utf8() {
        let mut buf = "é".repeat(3).into_bytes();
        buf.truncate(5);
        assert_matches!(
            check_bounded_utf8(buf, size(4)),
            Err(BoundedUtf8Error::TooLarge(SizeExceeded { size: 5, .. }))
        );
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
