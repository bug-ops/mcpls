//! Validated file-name tokens used to detect and route project files.
//!
//! Each type rejects at construction the inputs that would silently never
//! match, or always match, at the use site.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use super::text_newtype::impl_text_newtype;

/// Why a string is not a valid [`ProjectMarker`].
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidProjectMarker {
    /// The marker was empty.
    #[error("project marker cannot be empty")]
    Empty,
    /// The marker was `.` or `..`.
    #[error("project marker cannot be '.' or '..'")]
    RelativeComponent,
    /// The marker contained a path separator or a drive designator, so it is
    /// not one file name.
    #[error("project marker must be a single file name, without '/', '\\' or ':'")]
    NotAFileName,
}

impl InvalidProjectMarker {
    const fn check(marker: &str) -> Option<Self> {
        match marker.as_bytes() {
            [] => return Some(Self::Empty),
            [b'.'] | [b'.', b'.'] => return Some(Self::RelativeComponent),
            _ => {}
        }
        let mut rest = marker.as_bytes();
        while let [byte, tail @ ..] = rest {
            if matches!(byte, b'/' | b'\\' | b':') {
                return Some(Self::NotAFileName);
            }
            rest = tail;
        }
        None
    }
}

/// The file or directory name whose presence marks a project root, such as
/// `Cargo.toml`.
///
/// Exactly one file name: an absolute path would always match and a
/// multi-component path would match at the workspace root but never deeper.
/// Deserializes from a TOML string and rejects both at load time.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::ProjectMarker;
///
/// assert_eq!(ProjectMarker::new("Cargo.toml").unwrap(), "Cargo.toml");
/// assert!(ProjectMarker::new("/etc/hosts").is_err());
/// assert!(ProjectMarker::new("src/lib.rs").is_err());
/// assert!(ProjectMarker::new("..").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectMarker(Cow<'static, str>);

impl ProjectMarker {
    /// Builds a marker from a literal, checked at compile time when evaluated
    /// in a `const` context.
    ///
    /// # Panics
    ///
    /// Panics if `marker` is not a valid marker.
    #[must_use]
    pub const fn from_static(marker: &'static str) -> Self {
        assert!(
            InvalidProjectMarker::check(marker).is_none(),
            "invalid project marker"
        );
        Self(Cow::Borrowed(marker))
    }

    /// Builds a marker from any string.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidProjectMarker`] if `marker` is not exactly one file
    /// name.
    pub fn new(marker: impl Into<String>) -> Result<Self, InvalidProjectMarker> {
        let marker = marker.into();
        if let Some(reason) = InvalidProjectMarker::check(&marker) {
            return Err(reason);
        }
        Ok(Self(Cow::Owned(marker)))
    }
}

impl_text_newtype!(ProjectMarker, InvalidProjectMarker);

/// Why a string is not a valid [`FileExtension`].
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidFileExtension {
    /// The extension was empty.
    #[error("file extension cannot be empty")]
    Empty,
    /// The extension started with a dot, which `Path::extension` never reports.
    #[error("file extension must be written without the leading dot")]
    LeadingDot,
    /// The extension contained a character other than an ASCII letter, an
    /// ASCII digit, `_`, `-` or `+`.
    #[error("file extension may contain only ASCII letters, digits, '_', '-' and '+', found {0:?}")]
    InvalidChar(char),
}

/// Whether `byte` may appear in a file extension: an ASCII letter or digit,
/// `_`, `-` or `+`.
pub const fn is_pattern_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+')
}

/// The first rule a name token breaks, without the offending character.
#[derive(Clone, Copy)]
enum TokenFault {
    Empty,
    LeadingDot,
    InvalidByteAt(usize),
}

/// The one rule shared by [`FileExtension`] and [`FileName`]: non-empty and
/// made only of [`is_pattern_name_byte`] bytes. A non-ASCII byte is always
/// offending, so the index it reports is the start of a character.
const fn first_token_fault(token: &str) -> Option<TokenFault> {
    let bytes = token.as_bytes();
    let mut rest = bytes;
    if let [b'.', ..] = rest {
        return Some(TokenFault::LeadingDot);
    }
    if rest.is_empty() {
        return Some(TokenFault::Empty);
    }
    while let [byte, tail @ ..] = rest {
        if !is_pattern_name_byte(*byte) {
            return Some(TokenFault::InvalidByteAt(
                bytes.len().saturating_sub(rest.len()),
            ));
        }
        rest = tail;
    }
    None
}

/// The character of `token` that starts at byte `index`.
fn offender_at(token: &str, index: usize) -> char {
    token
        .get(index..)
        .and_then(|rest| rest.chars().next())
        .unwrap_or(char::REPLACEMENT_CHARACTER)
}

/// A file extension as `Path::extension` reports it: no leading dot, and only
/// ASCII letters, digits, `_`, `-` and `+`, so no glob or path characters
/// that make a pattern ambiguous.
///
/// `extensions = [".rs"]` would load but never match, so a leading dot is
/// rejected at load time. Case is significant, as in the extension map.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::FileExtension;
///
/// assert_eq!(FileExtension::new("rs").unwrap(), "rs");
/// assert!(FileExtension::new(".rs").is_err());
/// assert!(FileExtension::new("{cpp,h}").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FileExtension(Cow<'static, str>);

impl FileExtension {
    /// Builds an extension from an ASCII literal, checked at compile time when
    /// evaluated in a `const` context.
    ///
    /// # Panics
    ///
    /// Panics if `extension` is not a valid extension.
    #[must_use]
    pub const fn from_static(extension: &'static str) -> Self {
        assert!(
            first_token_fault(extension).is_none(),
            "invalid file extension"
        );
        Self(Cow::Borrowed(extension))
    }

    /// Builds an extension from any string.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidFileExtension`] naming the first rule `extension`
    /// breaks.
    pub fn new(extension: impl Into<String>) -> Result<Self, InvalidFileExtension> {
        let extension = extension.into();
        match first_token_fault(&extension) {
            None => Ok(Self(Cow::Owned(extension))),
            Some(TokenFault::Empty) => Err(InvalidFileExtension::Empty),
            Some(TokenFault::LeadingDot) => Err(InvalidFileExtension::LeadingDot),
            Some(TokenFault::InvalidByteAt(index)) => Err(InvalidFileExtension::InvalidChar(
                offender_at(&extension, index),
            )),
        }
    }
}

impl_text_newtype!(FileExtension, InvalidFileExtension);

/// Why a string is not a valid [`FileName`].
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidFileName {
    /// The name was empty.
    #[error("file name cannot be empty")]
    Empty,
    /// The name contained a character other than an ASCII letter, an ASCII
    /// digit, `_`, `-` or `+`; a dot is rejected, so a name never has an
    /// extension.
    #[error("file name may contain only ASCII letters, digits, '_', '-' and '+', found {0:?}")]
    InvalidChar(char),
}

/// The name of an extensionless file such as `Makefile` or `Dockerfile`, as
/// `Path::file_name` reports it.
///
/// A name has no dot, so `Path::extension` never reports one for it and the
/// extension and name maps cannot both claim a file. Case is significant.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::FileName;
///
/// assert_eq!(FileName::new("Makefile").unwrap(), "Makefile");
/// assert!(FileName::new("Cargo.toml").is_err());
/// assert!(FileName::new(".eslintrc").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FileName(Cow<'static, str>);

impl FileName {
    /// Builds a name from an ASCII literal, checked at compile time when
    /// evaluated in a `const` context.
    ///
    /// # Panics
    ///
    /// Panics if `name` is not a valid file name.
    #[must_use]
    pub const fn from_static(name: &'static str) -> Self {
        assert!(first_token_fault(name).is_none(), "invalid file name");
        Self(Cow::Borrowed(name))
    }

    /// Builds a name from any string.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidFileName`] naming the first rule `name` breaks.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidFileName> {
        let name = name.into();
        match first_token_fault(&name) {
            None => Ok(Self(Cow::Owned(name))),
            Some(TokenFault::Empty) => Err(InvalidFileName::Empty),
            Some(TokenFault::LeadingDot) => Err(InvalidFileName::InvalidChar('.')),
            Some(TokenFault::InvalidByteAt(index)) => {
                Err(InvalidFileName::InvalidChar(offender_at(&name, index)))
            }
        }
    }
}

impl_text_newtype!(FileName, InvalidFileName);

/// A `file_patterns` entry that cannot be mapped to a file extension or name.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "file pattern '{pattern}' is not supported: the final path segment must be `*.EXT`, where EXT \
     is a run of letters, digits, '_', '-' or '+' (for example `**/*.rs`), or the bare name of an \
     extensionless file written as `NAME` or `**/NAME` (for example `**/Makefile`); to cover \
     several extensions list one pattern per extension, such as [\"**/*.cpp\", \"**/*.h\"]"
)]
pub struct UnsupportedFilePattern {
    pattern: String,
}

impl UnsupportedFilePattern {
    /// The rejected pattern text.
    #[must_use]
    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

/// What a [`FilePattern`] matches: every file with an extension, or the one
/// extensionless file name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PatternTarget {
    /// Files with this extension, from a `*.EXT` final segment.
    Extension(FileExtension),
    /// Files with exactly this name, from a bare `NAME` or `**/NAME` pattern.
    Name(FileName),
}

/// A `file_patterns` entry: either an optional directory part followed by a
/// final segment of the form `*.EXT`, or the bare name of an extensionless
/// file written as `NAME` or `**/NAME`.
///
/// The extension or name is the only part mcpls reads, so the directory part
/// of an extension pattern (such as `**/`) is kept for display and ignored for
/// routing. A name pattern admits no directory part other than `**/`, so it
/// never claims every file of that name below one directory. Every other form
/// (brace expansion, character classes, `?`, dotted names, dotfiles, single
/// files below a directory) has nothing to map and is rejected rather than
/// dropped silently. [`Self::parse`] is the only constructor, so the config
/// validator and the language-map builder cannot disagree on what is
/// supported.
///
/// # Examples
///
/// ```
/// use std::assert_matches;
///
/// use mcpls_core::config::{FilePattern, PatternTarget};
///
/// let pattern = FilePattern::parse("**/*.rs").unwrap();
/// assert_matches!(pattern.target(), PatternTarget::Extension(e) if e == "rs");
/// assert_eq!(pattern.as_str(), "**/*.rs");
///
/// let makefile = FilePattern::parse("**/Makefile").unwrap();
/// assert_matches!(makefile.target(), PatternTarget::Name(n) if n == "Makefile");
///
/// assert!(FilePattern::parse("**/*.{cpp,h}").is_err());
/// assert!(FilePattern::parse("src/main.rs").is_err());
/// assert!(FilePattern::parse("docs/Makefile").is_err());
/// assert!(FilePattern::parse("/Makefile").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(into = "String")]
pub struct FilePattern {
    raw: String,
    target: PatternTarget,
}

impl FilePattern {
    /// Parses `pattern`.
    ///
    /// # Errors
    ///
    /// Returns [`UnsupportedFilePattern`] if the final path segment (after the
    /// last `/`) is neither `*.EXT` with a valid [`FileExtension`] nor a valid
    /// [`FileName`] reached through no directory part but `**/`.
    pub fn parse(pattern: &str) -> Result<Self, UnsupportedFilePattern> {
        let (directory, basename) = pattern.rsplit_once('/').unwrap_or(("", pattern));
        let target = match basename.strip_prefix("*.") {
            Some(extension) => FileExtension::new(extension)
                .ok()
                .map(PatternTarget::Extension),
            None if directory == "**" || !pattern.contains('/') => {
                FileName::new(basename).ok().map(PatternTarget::Name)
            }
            None => None,
        };
        target
            .map(|target| Self {
                raw: pattern.to_owned(),
                target,
            })
            .ok_or_else(|| UnsupportedFilePattern {
                pattern: pattern.to_owned(),
            })
    }

    /// Parses a literal known to be supported, such as a built-in default.
    ///
    /// # Panics
    ///
    /// Panics if `pattern` is not a supported form.
    #[must_use]
    pub fn from_static(pattern: &'static str) -> Self {
        match Self::parse(pattern) {
            Ok(parsed) => parsed,
            Err(e) => panic!("{e}"),
        }
    }

    /// The pattern text as configured.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The extension or file name this pattern maps.
    #[must_use]
    pub const fn target(&self) -> &PatternTarget {
        &self.target
    }
}

impl std::fmt::Display for FilePattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

impl From<FilePattern> for String {
    fn from(pattern: FilePattern) -> Self {
        pattern.raw
    }
}

impl PartialEq<str> for FilePattern {
    fn eq(&self, other: &str) -> bool {
        self.raw == other
    }
}

impl PartialEq<&str> for FilePattern {
    fn eq(&self, other: &&str) -> bool {
        self.raw == *other
    }
}

/// Every form the file pattern tests must see rejected, shared with the
/// config loading tests.
#[cfg(test)]
pub(super) const UNSUPPORTED_FILE_PATTERNS: &[&str] = &[
    "**/*.{cpp,h}",
    "**/*.[ch]",
    "**/*.ts?",
    "**/*",
    "src/**",
    "docs/Makefile",
    "/Makefile",
    "src/**/Dockerfile",
    "**/Makefile.am",
    "**/.eslintrc",
    ".eslintrc",
    "**/*.",
    "**/*.tar.gz",
    "src/main.rs",
    "Cargo.toml",
    "",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_extension_rejects_dot_empty_and_glob_forms() {
        assert_eq!(FileExtension::new(""), Err(InvalidFileExtension::Empty));
        assert_eq!(
            FileExtension::new(".rs"),
            Err(InvalidFileExtension::LeadingDot)
        );
        for (bad, ch) in [
            ("{cpp,h}", '{'),
            ("[ch]", '['),
            ("ts?", '?'),
            ("*", '*'),
            ("tar.gz", '.'),
            ("a/b", '/'),
            (" rs", ' '),
        ] {
            assert_eq!(
                FileExtension::new(bad),
                Err(InvalidFileExtension::InvalidChar(ch)),
                "{bad}"
            );
        }
    }

    #[test]
    fn test_file_extension_rejects_non_ascii_with_the_offending_char() {
        assert_eq!(
            FileExtension::new("r\u{e9}s"),
            Err(InvalidFileExtension::InvalidChar('\u{e9}'))
        );
        assert_eq!(
            FileExtension::new("\u{4e2d}"),
            Err(InvalidFileExtension::InvalidChar('\u{4e2d}'))
        );
    }

    #[test]
    #[should_panic(expected = "invalid file extension")]
    fn test_file_extension_from_static_panics_on_non_ascii() {
        let _ = FileExtension::from_static("r\u{e9}s");
    }

    #[test]
    fn test_file_extension_accepts_case_and_symbols() {
        for good in ["rs", "R", "c++", "d_ts", "tar-gz", "x1"] {
            assert_eq!(FileExtension::new(good).unwrap(), good);
        }
        assert_ne!(
            FileExtension::from_static("r"),
            FileExtension::from_static("R")
        );
    }

    #[test]
    fn test_file_extension_serde_and_borrowed_lookup() {
        let extension: FileExtension = serde_json::from_str("\"rs\"").unwrap();
        assert_eq!(serde_json::to_string(&extension).unwrap(), "\"rs\"");
        assert!(serde_json::from_str::<FileExtension>("\".rs\"").is_err());
        let map = std::collections::HashMap::from([(extension, 1)]);
        assert_eq!(map.get("rs"), Some(&1));
    }

    #[test]
    #[should_panic(expected = "invalid file extension")]
    fn test_file_extension_from_static_panics_on_invalid() {
        let _ = FileExtension::from_static(".rs");
    }

    #[test]
    fn test_file_pattern_accepts_trailing_extension_forms() {
        for (raw, ext) in [
            ("**/*.rs", "rs"),
            ("*.h", "h"),
            ("src/**/*.tsx", "tsx"),
            ("**/*.c++", "c++"),
            ("**/*.R", "R"),
        ] {
            let pattern = FilePattern::parse(raw).unwrap();
            assert_eq!(
                pattern.target(),
                &PatternTarget::Extension(FileExtension::from_static(ext)),
                "{raw}"
            );
            assert_eq!(pattern.as_str(), raw);
        }
    }

    #[test]
    fn test_file_pattern_accepts_bare_and_globstar_names() {
        for (raw, name) in [
            ("Makefile", "Makefile"),
            ("**/Dockerfile", "Dockerfile"),
            ("**/Justfile", "Justfile"),
        ] {
            let pattern = FilePattern::parse(raw).unwrap();
            assert_eq!(
                pattern.target(),
                &PatternTarget::Name(FileName::from_static(name)),
                "{raw}"
            );
            assert_eq!(pattern.as_str(), raw);
        }
    }

    #[test]
    fn test_file_name_rejects_dots_empty_and_glob_forms() {
        assert_eq!(FileName::new(""), Err(InvalidFileName::Empty));
        for (bad, ch) in [
            (".eslintrc", '.'),
            ("Cargo.toml", '.'),
            ("Make*", '*'),
            ("a/b", '/'),
            ("m\u{e9}", '\u{e9}'),
        ] {
            assert_eq!(
                FileName::new(bad),
                Err(InvalidFileName::InvalidChar(ch)),
                "{bad}"
            );
        }
    }

    #[test]
    fn test_file_name_serde_and_borrowed_lookup() {
        let name: FileName = serde_json::from_str("\"Makefile\"").unwrap();
        assert_eq!(serde_json::to_string(&name).unwrap(), "\"Makefile\"");
        assert!(serde_json::from_str::<FileName>("\"a.b\"").is_err());
        let map = std::collections::HashMap::from([(name, 1)]);
        assert_eq!(map.get("Makefile"), Some(&1));
    }

    #[test]
    #[should_panic(expected = "invalid file name")]
    fn test_file_name_from_static_panics_on_invalid() {
        let _ = FileName::from_static("a.b");
    }

    #[test]
    fn test_file_pattern_rejects_every_unsupported_form() {
        for &raw in UNSUPPORTED_FILE_PATTERNS {
            let err = FilePattern::parse(raw).unwrap_err();
            assert_eq!(err.pattern(), raw);
            assert!(err.to_string().contains(&format!("'{raw}'")), "{raw}");
            assert!(err.to_string().contains("*.EXT"), "{raw}");
        }
    }

    #[test]
    fn test_file_pattern_serializes_as_configured_text() {
        let pattern = FilePattern::from_static("**/*.rs");
        assert_eq!(serde_json::to_string(&pattern).unwrap(), "\"**/*.rs\"");
        assert_eq!(pattern.to_string(), "**/*.rs");
    }

    #[test]
    #[should_panic(expected = "is not supported")]
    fn test_file_pattern_from_static_panics_on_unsupported() {
        let _ = FilePattern::from_static("**/*.{a,b}");
    }

    #[test]
    fn test_project_marker_rejects_non_file_names() {
        assert_eq!(ProjectMarker::new(""), Err(InvalidProjectMarker::Empty));
        assert_eq!(
            ProjectMarker::new("."),
            Err(InvalidProjectMarker::RelativeComponent)
        );
        assert_eq!(
            ProjectMarker::new(".."),
            Err(InvalidProjectMarker::RelativeComponent)
        );
        for bad in ["/etc/hosts", "../x", "src/x", r"a\b", "C:", "C:\\x"] {
            assert_eq!(
                ProjectMarker::new(bad),
                Err(InvalidProjectMarker::NotAFileName),
                "{bad}"
            );
        }
    }

    #[test]
    fn test_project_marker_accepts_dotfiles_and_dotted_names() {
        for good in ["Cargo.toml", ".clangd", "..hidden", "build.zig.zon"] {
            assert_eq!(ProjectMarker::new(good).unwrap(), good);
        }
    }

    #[test]
    fn test_project_marker_serde_round_trip_and_rejection() {
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Holder {
            marker: ProjectMarker,
        }

        let holder: Holder = toml::from_str(r#"marker = "go.mod""#).unwrap();
        assert_eq!(holder.marker, ProjectMarker::from_static("go.mod"));
        assert_eq!(
            toml::to_string(&holder).unwrap().trim(),
            r#"marker = "go.mod""#
        );
        assert!(toml::from_str::<Holder>(r#"marker = "a/b""#).is_err());
    }

    #[test]
    #[should_panic(expected = "invalid project marker")]
    fn test_project_marker_from_static_panics_on_invalid() {
        let _ = ProjectMarker::from_static("a/b");
    }
}
