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

/// The first rule an extension breaks, without the offending character.
#[derive(Clone, Copy)]
enum ExtensionFault {
    Empty,
    LeadingDot,
    InvalidByteAt(usize),
}

/// The one extension rule, shared by [`FileExtension::new`] and
/// [`FileExtension::from_static`]. A non-ASCII byte is always offending, so
/// the index it reports is the start of a character.
const fn first_extension_fault(extension: &str) -> Option<ExtensionFault> {
    let bytes = extension.as_bytes();
    let mut rest = bytes;
    if let [b'.', ..] = rest {
        return Some(ExtensionFault::LeadingDot);
    }
    if rest.is_empty() {
        return Some(ExtensionFault::Empty);
    }
    while let [byte, tail @ ..] = rest {
        if !is_pattern_name_byte(*byte) {
            return Some(ExtensionFault::InvalidByteAt(
                bytes.len().saturating_sub(rest.len()),
            ));
        }
        rest = tail;
    }
    None
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
            first_extension_fault(extension).is_none(),
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
        match first_extension_fault(&extension) {
            None => Ok(Self(Cow::Owned(extension))),
            Some(ExtensionFault::Empty) => Err(InvalidFileExtension::Empty),
            Some(ExtensionFault::LeadingDot) => Err(InvalidFileExtension::LeadingDot),
            Some(ExtensionFault::InvalidByteAt(index)) => {
                let offender = extension
                    .get(index..)
                    .and_then(|rest| rest.chars().next())
                    .unwrap_or(char::REPLACEMENT_CHARACTER);
                Err(InvalidFileExtension::InvalidChar(offender))
            }
        }
    }
}

impl_text_newtype!(FileExtension, InvalidFileExtension);

/// A `file_patterns` entry that cannot be mapped to a file extension.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "file pattern '{pattern}' is not supported: the final path segment must be `*.EXT`, where EXT \
     is a run of letters, digits, '_', '-' or '+' (for example `**/*.rs`); to cover several \
     extensions list one pattern per extension, such as [\"**/*.cpp\", \"**/*.h\"]"
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

/// A `file_patterns` entry: an optional directory part followed by a final
/// segment of the form `*.EXT`.
///
/// The extension is the only part mcpls reads, so the directory part (such as
/// `**/`) is kept for display and ignored for routing. Every other form
/// (brace expansion, character classes, `?`, extensionless names, single
/// files) has no extension to map and is rejected rather than dropped
/// silently. [`Self::parse`] is the only constructor, so the config validator
/// and the extension-map builder cannot disagree on what is supported.
///
/// # Examples
///
/// ```
/// use mcpls_core::config::FilePattern;
///
/// let pattern = FilePattern::parse("**/*.rs").unwrap();
/// assert_eq!(pattern.extension(), "rs");
/// assert_eq!(pattern.as_str(), "**/*.rs");
/// assert!(FilePattern::parse("**/*.{cpp,h}").is_err());
/// assert!(FilePattern::parse("src/main.rs").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(into = "String")]
pub struct FilePattern {
    raw: String,
    extension: FileExtension,
}

impl FilePattern {
    /// Parses `pattern`.
    ///
    /// # Errors
    ///
    /// Returns [`UnsupportedFilePattern`] if the final path segment (after the
    /// last `/`) is not `*.EXT` with a valid [`FileExtension`].
    pub fn parse(pattern: &str) -> Result<Self, UnsupportedFilePattern> {
        let basename = pattern.rsplit('/').next().unwrap_or(pattern);
        basename
            .strip_prefix("*.")
            .and_then(|extension| FileExtension::new(extension).ok())
            .map(|extension| Self {
                raw: pattern.to_owned(),
                extension,
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

    /// The extension this pattern maps.
    #[must_use]
    pub const fn extension(&self) -> &FileExtension {
        &self.extension
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
    "Makefile",
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
            assert_eq!(pattern.extension(), ext, "{raw}");
            assert_eq!(pattern.as_str(), raw);
        }
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
