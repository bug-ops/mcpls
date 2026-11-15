//! Captured server stderr and the hints rendered beside initialization failures.

use std::fmt;

use crate::config::BuiltinServer;
use crate::redaction::Redactions;

/// What a failed server startup wrote to stderr, bounded and sanitized.
///
/// Holds the first and last bytes of the output (or all of it when short), so
/// both the opening banner and the final error survive.
///
/// # Sanitizing
///
/// Control characters other than newline and tab are dropped, as are
/// characters that forge or reorder text in logs and terminals: the Unicode
/// line and paragraph separators, bidirectional controls and zero-width
/// characters.
///
/// # Redaction
///
/// Values that look secret are replaced with `[redacted:NAME]` before the text
/// leaves mcpls. A value counts as secret (and must be at least 8 bytes) when
/// it is:
/// - an environment variable value, from the server's configured `env` or
///   mcpls's own environment, whose name contains `TOKEN`, `KEY`, `SECRET`,
///   `PASSW`, `CRED` or `AUTH` (case-insensitive);
/// - the value of a `--flag=value` or `--flag value` argument whose flag name
///   matches the same patterns;
/// - a string under a matching key of `initialization_options`.
///
/// Other values, such as `RUSTUP_TOOLCHAIN`, stay visible so the cause of a
/// failure can still be read. Matching is by exact value: encoded forms (URL,
/// base64, JSON-escaped) are not found. A secret cut by the head/tail elision
/// is hidden only for the fragment of at least 4 bytes on either side of the
/// cut. Put secrets in a name-matching `env` entry rather than in other
/// places.
///
/// `Display` renders one line: trimmed non-empty lines joined with ` | `,
/// with `...` between head and tail when elided. The raw multi-line text is
/// available through [`Self::head`] and [`Self::tail`].
///
/// # Examples
///
/// ```
/// use mcpls_core::Error;
/// use mcpls_core::error::InitPhase;
///
/// fn server_output(err: &Error) -> Option<String> {
///     match err {
///         Error::LspInitFailed { stderr: Some(stderr), .. }
///         | Error::ServerExitedDuringInit { stderr: Some(stderr), .. } => {
///             Some(stderr.to_string())
///         }
///         _ => None,
///     }
/// }
///
/// let err = Error::LspInitFailed {
///     phase: InitPhase::Initialize,
///     cause: Box::new(Error::ServerTerminated),
///     hint: None,
///     stderr: None,
/// };
/// assert_eq!(server_output(&err), None);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StderrExcerpt {
    body: ExcerptBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExcerptBody {
    Complete(Box<str>),
    Elided { head: Box<str>, tail: Box<str> },
}

impl StderrExcerpt {
    /// Excerpt of output that fit entirely in the capture buffer; `None` when
    /// it holds no visible text.
    pub(crate) fn complete(bytes: &[u8], redactions: &Redactions) -> Option<Self> {
        let text = clean_stderr(&String::from_utf8_lossy(bytes), redactions);
        (!text.is_empty()).then(|| Self {
            body: ExcerptBody::Complete(text.into()),
        })
    }

    /// Excerpt of output whose middle was dropped. The cut can split a UTF-8
    /// sequence at the end of `head` and at the start of `tail`; both partial
    /// sequences are discarded rather than shown as replacement characters.
    /// A secret split by the cut is masked as far as `redactions` allows.
    pub(crate) fn elided(head: &[u8], tail: &[u8], redactions: &Redactions) -> Option<Self> {
        let head = clean_stderr(
            &redactions.mask_cut_head(&decode_cut_head(head)),
            redactions,
        );
        let tail_start = tail
            .iter()
            .position(|byte| byte & 0xC0 != 0x80)
            .unwrap_or(tail.len());
        let tail = clean_stderr(
            &redactions.mask_cut_tail(&String::from_utf8_lossy(
                tail.get(tail_start..).unwrap_or_default(),
            )),
            redactions,
        );
        match (head.is_empty(), tail.is_empty()) {
            (true, true) => None,
            (false, true) => Some(Self {
                body: ExcerptBody::Complete(head.into()),
            }),
            (true, false) => Some(Self {
                body: ExcerptBody::Complete(tail.into()),
            }),
            (false, false) => Some(Self {
                body: ExcerptBody::Elided {
                    head: head.into(),
                    tail: tail.into(),
                },
            }),
        }
    }

    /// The start of the output, or all of it when nothing was dropped.
    #[must_use]
    pub fn head(&self) -> &str {
        match &self.body {
            ExcerptBody::Complete(text) => text,
            ExcerptBody::Elided { head, .. } => head,
        }
    }

    /// The end of the output, present only when the middle was dropped.
    #[must_use]
    pub fn tail(&self) -> Option<&str> {
        match &self.body {
            ExcerptBody::Complete(_) => None,
            ExcerptBody::Elided { tail, .. } => Some(tail),
        }
    }

    /// Whether the middle of the output was dropped.
    #[must_use]
    pub const fn is_elided(&self) -> bool {
        matches!(self.body, ExcerptBody::Elided { .. })
    }
}

impl fmt::Display for StderrExcerpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn visible_lines(text: &str) -> impl Iterator<Item = &str> {
            text.lines().map(str::trim).filter(|line| !line.is_empty())
        }

        let mut parts: Vec<&str> = visible_lines(self.head()).collect();
        if let Some(tail) = self.tail() {
            parts.push("...");
            parts.extend(visible_lines(tail));
        }
        f.write_str(&parts.join(" | "))
    }
}

/// Decodes `head`, dropping a UTF-8 sequence cut off at its end.
fn decode_cut_head(head: &[u8]) -> String {
    match std::str::from_utf8(head) {
        Ok(text) => text.to_owned(),
        Err(e) if e.error_len().is_none() => {
            String::from_utf8_lossy(head.get(..e.valid_up_to()).unwrap_or_default()).into_owned()
        }
        Err(_) => String::from_utf8_lossy(head).into_owned(),
    }
}

/// Redacts secrets, drops control and deceptive format characters (other than
/// `\n`/`\t`: they could forge log lines or drive a terminal) and trims.
/// Redaction runs before and after the filter so a dropped character cannot
/// split a secret into a form that escapes it.
fn clean_stderr(text: &str, redactions: &Redactions) -> String {
    let filtered: String = redactions
        .apply(text)
        .chars()
        .filter(|c| {
            (!c.is_control() || matches!(c, '\n' | '\t'))
                && !crate::util::is_deceptive_format_char(*c)
        })
        .collect();
    redactions.apply(&filtered).trim().to_owned()
}

/// `Display` suffix appending a server's stderr excerpt to a startup error.
pub(super) struct StderrSuffix<'a>(pub(super) &'a Option<StderrExcerpt>);

impl fmt::Display for StderrSuffix<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .as_ref()
            .map_or(Ok(()), |excerpt| write!(f, "; stderr: {excerpt}"))
    }
}

/// Guidance attached to an `initialize` failure whose likely cause is known.
///
/// # Examples
///
/// ```
/// use mcpls_core::error::InitFailureHint;
///
/// let text = InitFailureHint::NativeTypescriptOnly.to_string();
/// assert!(text.contains("TypeScript 7"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InitFailureHint {
    /// typescript-language-server found only TypeScript 7 or later, which
    /// ships no `tsserver` for it to start.
    NativeTypescriptOnly,
}

impl fmt::Display for InitFailureHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NativeTypescriptOnly => write!(
                f,
                "typescript-language-server found only TypeScript 7 or later, which ships no \
                 tsserver; install the TypeScript version the server supports next to it (for a \
                 global install: `{}`), or change the `command` of your existing `typescript` \
                 server entry to the absolute path of the `tsc` of a TypeScript 7 install \
                 outside the workspace, with `args = [\"--lsp\", \"--stdio\"]` (a `tsc` from the \
                 workspace, or a bare `tsc` that PATH may resolve into the workspace, is \
                 workspace-supplied code)",
                BuiltinServer::TypescriptLanguageServer.install_hint()
            ),
        }
    }
}

/// `Display` suffix appending an [`InitFailureHint`] to a startup error.
pub(super) struct HintSuffix<'a>(pub(super) &'a Option<InitFailureHint>);

impl fmt::Display for HintSuffix<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.map_or(Ok(()), |hint| write!(f, "; {hint}"))
    }
}

/// `Display` suffix for [`Error::ServerExitedDuringInit`] carrying the exit
/// status and, for builtin servers with a known early-exit cause, a hint.
pub(super) struct EarlyExitDetail<'a>(pub(super) &'a str, pub(super) Option<i32>);

impl fmt::Display for EarlyExitDetail<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(command, exit_code) = *self;
        match exit_code {
            Some(code) => write!(f, " with exit code {code}")?,
            None => f.write_str(" (terminated by a signal)")?,
        }
        BuiltinServer::from_command(command)
            .and_then(BuiltinServer::early_exit_hint)
            .map_or(Ok(()), |hint| write!(f, "; {hint}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stderr_excerpt_none_when_nothing_visible() {
        assert_eq!(
            StderrExcerpt::complete(b"  \n\x1b\r\n ", &Redactions::default()),
            None
        );
        assert_eq!(StderrExcerpt::complete(b"", &Redactions::default()), None);
    }

    #[test]
    fn test_stderr_excerpt_strips_control_characters_but_keeps_newline_and_tab() {
        let excerpt =
            StderrExcerpt::complete(b"a\x1b[31mb\x00\r\nc\td", &Redactions::default()).unwrap();
        assert_eq!(excerpt.head(), "a[31mb\nc\td");
    }

    #[test]
    fn test_stderr_excerpt_display_is_single_line() {
        let excerpt =
            StderrExcerpt::complete(b"first\n\n  second  \nthird\n", &Redactions::default())
                .unwrap();
        assert_eq!(excerpt.to_string(), "first | second | third");
    }

    #[test]
    fn test_stderr_excerpt_elided_display_marks_the_gap() {
        let excerpt =
            StderrExcerpt::elided(b"start\nbanner", b"last\nerror", &Redactions::default())
                .unwrap();
        assert!(excerpt.is_elided());
        assert_eq!(excerpt.tail(), Some("last\nerror"));
        assert_eq!(excerpt.to_string(), "start | banner | ... | last | error");
    }

    #[test]
    fn test_stderr_excerpt_elided_drops_utf8_sequences_cut_by_the_split() {
        let text = "é".as_bytes();
        let head = [b"ok ".as_slice(), &text[..1]].concat();
        let tail = [&text[1..], b" end".as_slice()].concat();

        let excerpt = StderrExcerpt::elided(&head, &tail, &Redactions::default()).unwrap();

        assert_eq!(excerpt.head(), "ok");
        assert_eq!(excerpt.tail(), Some("end"));
    }

    fn secrets(pairs: &[(&str, &str)]) -> Redactions {
        Redactions::new(
            pairs
                .iter()
                .map(|(label, value)| ((*label).to_owned(), (*value).to_owned())),
        )
    }

    #[test]
    fn test_stderr_excerpt_redacts_secrets_with_their_label() {
        let excerpt = StderrExcerpt::complete(
            b"token=hunter2hunter2 id=abc",
            &secrets(&[("API_TOKEN", "hunter2hunter2"), ("SHORT_KEY", "abc")]),
        )
        .unwrap();
        assert_eq!(excerpt.head(), "token=[redacted:API_TOKEN] id=abc");
    }

    #[test]
    fn test_stderr_excerpt_redacts_longest_overlapping_secret_whole() {
        let excerpt = StderrExcerpt::complete(
            b"v=abcdefghXYZ12345",
            &secrets(&[("A_KEY", "abcdefgh"), ("B_KEY", "abcdefghXYZ12345")]),
        )
        .unwrap();
        assert_eq!(excerpt.head(), "v=[redacted:B_KEY]");
    }

    #[test]
    fn test_stderr_excerpt_keeps_non_secret_values_visible() {
        let excerpt = StderrExcerpt::complete(
            b"toolchain 'nightly-2024-01-01' is not installed",
            &secrets(&[("API_TOKEN", "hunter2hunter2")]),
        )
        .unwrap();
        assert_eq!(
            excerpt.head(),
            "toolchain 'nightly-2024-01-01' is not installed"
        );
    }

    #[test]
    fn test_stderr_excerpt_redacts_a_secret_split_by_a_control_character() {
        let excerpt = StderrExcerpt::complete(
            "tok=hunter2\u{200B}hunter2".as_bytes(),
            &secrets(&[("API_TOKEN", "hunter2hunter2")]),
        )
        .unwrap();
        assert_eq!(excerpt.head(), "tok=[redacted:API_TOKEN]");
    }

    /// A secret cut in two by the elision boundary: the fragments of at least
    /// four bytes on each side are masked.
    #[test]
    fn test_stderr_excerpt_masks_a_secret_split_across_the_elision_boundary() {
        let redactions = secrets(&[("API_TOKEN", "supersecretvalue")]);

        let excerpt =
            StderrExcerpt::elided(b"token=supersec", b"retvalue done", &redactions).unwrap();

        let shown = excerpt.to_string();
        assert!(!shown.contains("supersec"), "{shown}");
        assert!(!shown.contains("retvalue"), "{shown}");
        assert!(shown.contains("[redacted:API_TOKEN]"), "{shown}");
    }

    #[test]
    fn test_stderr_excerpt_strips_separators_bidi_and_zero_width_characters() {
        let text = "a\u{2028}b\u{2029}c\u{202E}d\u{2066}e\u{200B}f\u{FEFF}g\u{200F}h";

        let excerpt = StderrExcerpt::complete(text.as_bytes(), &Redactions::default()).unwrap();

        assert_eq!(excerpt.head(), "abcdefgh");
    }

    #[test]
    fn test_escape_control_and_stderr_agree_on_deceptive_characters() {
        let deceptive = [
            '\u{061C}',
            '\u{200B}',
            '\u{200C}',
            '\u{200D}',
            '\u{200E}',
            '\u{200F}',
            '\u{2028}',
            '\u{2029}',
            '\u{202A}',
            '\u{202B}',
            '\u{202C}',
            '\u{202D}',
            '\u{202E}',
            '\u{2060}',
            '\u{2066}',
            '\u{2067}',
            '\u{2068}',
            '\u{2069}',
            '\u{FEFF}',
            '\u{00AD}',
            '\u{180E}',
            '\u{2061}',
            '\u{2064}',
            '\u{206A}',
            '\u{206F}',
            '\u{FFF9}',
            '\u{FFFB}',
            '\u{E0000}',
            '\u{E0041}',
            '\u{E007F}',
        ];
        for c in deceptive {
            let text = format!("a{c}b");
            let escaped = crate::util::escape_control(&text);
            assert!(
                !escaped.contains(c),
                "escape_control kept U+{:04X}",
                c as u32
            );
            let excerpt = StderrExcerpt::complete(text.as_bytes(), &Redactions::default()).unwrap();
            assert_eq!(excerpt.head(), "ab", "stderr kept U+{:04X}", c as u32);
        }
    }
}
