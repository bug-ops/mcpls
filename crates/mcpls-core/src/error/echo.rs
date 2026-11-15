//! Bounded, escaped echoes of configured launch text and workspace paths.

use std::fmt;
use std::path::Path;

#[cfg(doc)]
use super::Error;
use crate::util::{
    MAX_ECHOED_NAME_BYTES, MAX_ECHOED_PATH_BYTES, TRUNCATION_MARKER, escape_bounded,
};

/// A piece of configured launch text that may be shown to callers: an option
/// name or a program name, never a value, an inline program or an
/// assignment's value.
///
/// The text is cut at the first `=` or whitespace, escaped and bounded to
/// 256 bytes (the escaped text is what is bounded), because
/// configured arguments can hold secrets and reach MCP clients unredacted.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::EchoedArgument;
///
/// assert_eq!(EchoedArgument::name("--token=abc").as_str(), "--token");
/// assert_eq!(EchoedArgument::name("API_KEY=abc").as_str(), "API_KEY");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoedArgument(String);

impl EchoedArgument {
    /// The option or program name `arg` spells: the text before the first `=`
    /// or whitespace, so an assignment's value or an argument glued to the
    /// name is not echoed. A short cluster is named by its offending letter
    /// (`-x`), built by the caller.
    #[must_use]
    pub fn name(arg: &str) -> Self {
        let name = arg
            .split_once(|c: char| c == '=' || c.is_whitespace())
            .map_or(arg, |(name, _)| name);
        Self(escape_bounded(name, MAX_ECHOED_NAME_BYTES))
    }

    /// The echoed text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A path the workspace names, shown whole, escaped and bounded.
///
/// The workspace names a program a launcher starts or a symlink target's final
/// component. Unlike [`EchoedArgument`], the text is never cut at a space or an
/// `=`, because a path is not an option with a value. It is bounded to
/// 1024 bytes of escaped text.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// use mcpls_core::error::EchoedPath;
///
/// let path = EchoedPath::new(Path::new("/opt/my tools/a=b/srv"));
/// assert_eq!(path.as_str(), "/opt/my tools/a=b/srv");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoedPath(String);

impl EchoedPath {
    /// The whole of `path`, escaped and bounded.
    #[must_use]
    pub fn new(path: &Path) -> Self {
        Self(escape_bounded(
            &path.to_string_lossy(),
            MAX_ECHOED_PATH_BYTES,
        ))
    }

    /// The program an argument names: its whole path, unless the text reads
    /// as an assignment (an `=` before any path separator), which is cut at
    /// the `=` like [`EchoedArgument`] so a value that is not a program is not
    /// echoed.
    #[must_use]
    pub fn program(arg: &str) -> Self {
        let assignment = arg
            .split_once('=')
            .is_some_and(|(name, _)| !name.contains(std::path::is_separator));
        if assignment {
            Self(EchoedArgument::name(arg).0)
        } else {
            Self::new(Path::new(arg))
        }
    }

    /// The echoed text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Renders a path as `{path:?}` does, cut after about 1024 bytes of that text.
///
/// The quoted form is for the paths a caller supplied or a file operation
/// failed on ([`Error::MalformedPath`], [`Error::FileIo`]), where the quotes
/// keep an empty or spaced path unambiguous and non-UTF-8 bytes visible.
/// [`EchoedPath`] is the bare form for paths that read inside a sentence
/// (`NotARegularFile`, `DocumentNotFound`, `PathOutsideWorkspace`,
/// `NoWorkspaceRoots`), lossy for non-UTF-8 bytes. The two differ on purpose.
///
/// The cut is made while the debug text is produced, so the work is bounded by
/// the cut and not by the path, and it never leaves half an escape sequence
/// (`\u{..}`, `\x..`) in front of the truncation marker. Text within the bound
/// is byte-identical to the plain debug form.
pub(super) struct BoundedDebugPath<'a>(pub(super) &'a Path);

/// Collects text until `left` bytes are used, then refuses the rest.
struct CutWriter {
    text: String,
    left: usize,
    cut: bool,
}

impl fmt::Write for CutWriter {
    fn write_str(&mut self, piece: &str) -> fmt::Result {
        if self.cut {
            return Err(fmt::Error);
        }
        if piece.len() <= self.left {
            self.text.push_str(piece);
            self.left = self.left.saturating_sub(piece.len());
            return Ok(());
        }
        let keep = piece.floor_char_boundary(self.left);
        self.text.push_str(piece.get(..keep).unwrap_or_default());
        self.cut = true;
        Err(fmt::Error)
    }
}

/// The length of `text` without a trailing incomplete debug escape (`\u{1`,
/// `\x4`, a lone `\`).
fn complete_escapes_len(text: &str) -> usize {
    let mut at = 0;
    while let Some(offset) = text.get(at..).and_then(|tail| tail.find('\\')) {
        let start = at.saturating_add(offset);
        let after = start.saturating_add(1);
        let rest = text.get(after..).unwrap_or_default();
        let escape_len = match rest.chars().next() {
            Some('u') => rest.find('}').map(|end| end.saturating_add(1)),
            Some('x') => (rest.len() >= 3).then_some(3),
            Some(other) => Some(other.len_utf8()),
            None => None,
        };
        let Some(escape_len) = escape_len else {
            return start;
        };
        at = after.saturating_add(escape_len);
    }
    text.len()
}

impl fmt::Display for BoundedDebugPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write as _;

        let mut writer = CutWriter {
            text: String::new(),
            left: MAX_ECHOED_PATH_BYTES,
            cut: false,
        };
        // An error only means the writer refused text after the cut.
        #[expect(
            clippy::unnecessary_debug_formatting,
            reason = "the quoted, escaped debug form is the message format"
        )]
        let formatted = write!(writer, "{:?}", self.0);
        debug_assert!(formatted.is_ok() || writer.cut);
        if !writer.cut {
            return f.write_str(&writer.text);
        }
        let whole = complete_escapes_len(&writer.text);
        f.write_str(writer.text.get(..whole).unwrap_or_default())?;
        f.write_str(TRUNCATION_MARKER)
    }
}

impl fmt::Display for EchoedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for EchoedArgument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::{MAX_ECHOED_NAME_BYTES, MAX_ECHOED_PATH_BYTES, TRUNCATION_MARKER};

    /// The docs of the echo types state the bounds as numbers (the constants
    /// live in a private module and cannot be linked); this keeps them in step.
    #[test]
    fn test_echo_bound_docs_state_the_constants() {
        let source = include_str!("echo.rs");
        for (bytes, doc_marker) in [
            (MAX_ECHOED_NAME_BYTES, "pub struct EchoedArgument"),
            (MAX_ECHOED_PATH_BYTES, "pub struct EchoedPath"),
        ] {
            let before = source.split(doc_marker).next().unwrap();
            let doc = before.rsplit("\n\n").next().unwrap();
            assert!(
                doc.contains(&format!("{bytes} bytes")),
                "{doc_marker}: {doc}"
            );
        }
        let debug_doc = source
            .split("struct BoundedDebugPath")
            .next()
            .and_then(|before| before.rsplit("\n\n").next())
            .unwrap();
        assert!(
            debug_doc.contains(&format!("{MAX_ECHOED_PATH_BYTES} bytes")),
            "{debug_doc}"
        );
    }

    fn debug_of(path: &str) -> String {
        BoundedDebugPath(Path::new(path)).to_string()
    }

    /// #749 FR-013: the debug-quoted path is cut at the bound, on a character
    /// boundary, and within the bound it is the plain `{:?}` text.
    #[test]
    fn test_bounded_debug_path_cuts_at_the_bound() {
        let at_bound = "a".repeat(MAX_ECHOED_PATH_BYTES - 2);
        assert_eq!(debug_of(&at_bound), format!("{:?}", Path::new(&at_bound)));
        assert!(!debug_of(&at_bound).ends_with(TRUNCATION_MARKER));

        let over = "a".repeat(MAX_ECHOED_PATH_BYTES - 1);
        let cut = debug_of(&over);
        assert!(cut.ends_with(TRUNCATION_MARKER), "{cut}");
        assert_eq!(cut.len(), MAX_ECHOED_PATH_BYTES + TRUNCATION_MARKER.len());

        let straddling = "é".repeat(MAX_ECHOED_PATH_BYTES);
        let cut = debug_of(&straddling);
        assert!(cut.ends_with(TRUNCATION_MARKER), "{cut}");
        assert!(cut.len() <= MAX_ECHOED_PATH_BYTES + TRUNCATION_MARKER.len());

        let honest = debug_of("/ws/quote\"d\\ name.rs");
        assert_eq!(honest, format!("{:?}", Path::new("/ws/quote\"d\\ name.rs")));
    }

    /// #749 FR-013: a cut never leaves half an escape sequence before the marker.
    #[test]
    fn test_bounded_debug_path_never_splits_an_escape() {
        for prefix in 0..8 {
            let text = format!(
                "{}{}",
                "a".repeat(prefix),
                "\u{1b}".repeat(MAX_ECHOED_PATH_BYTES)
            );
            let cut = debug_of(&text);
            let body = cut.strip_suffix(TRUNCATION_MARKER).expect("cut");
            let tail = body.rsplit('\\').next().unwrap_or_default();
            assert!(
                tail.is_empty() || (tail.starts_with("u{1b}") && tail == "u{1b}"),
                "prefix {prefix}: ends with {tail:?}"
            );
            assert!(body.len() <= MAX_ECHOED_PATH_BYTES, "{}", body.len());
        }
    }

    #[test]
    fn echoed_path_keeps_spaces_and_equals() {
        let path = EchoedPath::new(Path::new("/tmp/out/no such dir/a=b/srv"));
        assert_eq!(path.as_str(), "/tmp/out/no such dir/a=b/srv");
    }

    #[test]
    fn echoed_path_never_cuts_an_escape_in_half() {
        let escape = "\\u{202e}";
        let hostile = "\u{202e}".repeat(MAX_ECHOED_PATH_BYTES);
        let echoed = EchoedPath::new(Path::new(&hostile));
        let kept = echoed
            .as_str()
            .strip_suffix(TRUNCATION_MARKER)
            .expect("an over-long path is marked as truncated");
        assert!(kept.len() <= MAX_ECHOED_PATH_BYTES);
        assert!(
            !kept.is_empty() && kept.len().is_multiple_of(escape.len()),
            "{kept}"
        );
        assert_eq!(kept, escape.repeat(kept.len() / escape.len()));
    }

    #[test]
    fn echoed_program_cuts_an_assignment_but_keeps_a_path() {
        assert_eq!(EchoedPath::program("API_TOKEN=abc").as_str(), "API_TOKEN");
        assert_eq!(EchoedPath::program("X=/tmp/secret").as_str(), "X");
        assert_eq!(
            EchoedPath::program("/tmp/out/a=b/srv").as_str(),
            "/tmp/out/a=b/srv"
        );
        assert_eq!(EchoedPath::program("no such dir").as_str(), "no such dir");
    }

    #[test]
    fn echoed_path_within_the_bound_is_not_marked() {
        let path = "a".repeat(MAX_ECHOED_PATH_BYTES);
        assert_eq!(EchoedPath::new(Path::new(&path)).as_str(), path);
    }
}
