//! File-to-language detection.
//!
//! [`LanguageMap`] is the one place that decides which language a file path
//! belongs to: by extension when the file has one, otherwise by its
//! extensionless name. The translator, the document tracker and the
//! tool-support report all read it, so they cannot disagree.

use std::collections::HashMap;
use std::path::Path;

use super::{FileExtension, FileName, LanguageId, PatternTarget};

/// What identifies a file to a [`LanguageMap`].
///
/// A file with an extension is identified by it, so a name key never competes
/// with an extension key: [`FileName`] has no dot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileKey {
    /// The file's extension, as `Path::extension` reports it.
    Extension(FileExtension),
    /// The name of a file without an extension.
    Name(FileName),
    /// The file has no extension or name that can be configured, such as a
    /// dotfile or a name with non-ASCII characters.
    Unmappable,
}

impl FileKey {
    /// The key identifying `path`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::Path;
    ///
    /// use mcpls_core::config::{FileExtension, FileKey, FileName};
    ///
    /// assert_eq!(
    ///     FileKey::of(Path::new("src/main.rs")),
    ///     FileKey::Extension(FileExtension::from_static("rs"))
    /// );
    /// assert_eq!(
    ///     FileKey::of(Path::new("build/Makefile")),
    ///     FileKey::Name(FileName::from_static("Makefile"))
    /// );
    /// assert_eq!(FileKey::of(Path::new(".gitignore")), FileKey::Unmappable);
    /// ```
    #[must_use]
    pub fn of(path: &Path) -> Self {
        let by_name = || {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| FileName::new(name).ok())
                .map_or(Self::Unmappable, Self::Name)
        };
        path.extension().map_or_else(by_name, |extension| {
            extension
                .to_str()
                .and_then(|extension| FileExtension::new(extension).ok())
                .map_or(Self::Unmappable, Self::Extension)
        })
    }
}

/// Maps file extensions and extensionless file names to language ids.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// use mcpls_core::config::{FileName, LanguageId, LanguageMap, PatternTarget};
///
/// let mut map = LanguageMap::default();
/// map.insert(
///     PatternTarget::Name(FileName::from_static("Makefile")),
///     LanguageId::new("make").unwrap(),
/// );
/// assert_eq!(map.detect(Path::new("Makefile")), "make");
/// assert_eq!(map.detect(Path::new("main.rs")), LanguageId::PLAINTEXT);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LanguageMap {
    by_extension: HashMap<FileExtension, LanguageId>,
    by_name: HashMap<FileName, LanguageId>,
}

impl LanguageMap {
    /// Maps `target` to `language`, replacing an earlier mapping of the same
    /// extension or name.
    pub fn insert(&mut self, target: PatternTarget, language: LanguageId) {
        match target {
            PatternTarget::Extension(extension) => {
                self.by_extension.insert(extension, language);
            }
            PatternTarget::Name(name) => {
                self.by_name.insert(name, language);
            }
        }
    }

    /// The language of `path`, or [`LanguageId::PLAINTEXT`] when its extension
    /// or name is not mapped.
    #[must_use]
    pub fn detect(&self, path: &Path) -> LanguageId {
        match FileKey::of(path) {
            FileKey::Extension(extension) => self.by_extension.get(&extension),
            FileKey::Name(name) => self.by_name.get(&name),
            FileKey::Unmappable => None,
        }
        .cloned()
        .unwrap_or(LanguageId::PLAINTEXT)
    }

    /// Every language some extension or name maps to, with repeats when
    /// several keys map to one language.
    pub fn languages(&self) -> impl Iterator<Item = &LanguageId> {
        self.by_extension.values().chain(self.by_name.values())
    }
}

impl From<HashMap<FileExtension, LanguageId>> for LanguageMap {
    fn from(by_extension: HashMap<FileExtension, LanguageId>) -> Self {
        Self {
            by_extension,
            by_name: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> LanguageMap {
        let mut map = LanguageMap::from(HashMap::from([
            (
                FileExtension::from_static("rs"),
                LanguageId::from_static("rust"),
            ),
            (
                FileExtension::from_static("R"),
                LanguageId::from_static("r"),
            ),
        ]));
        map.insert(
            PatternTarget::Name(FileName::from_static("Dockerfile")),
            LanguageId::from_static("dockerfile"),
        );
        map
    }

    #[test]
    fn test_detect_by_extension_and_name() {
        let map = map();
        assert_eq!(map.detect(Path::new("src/main.rs")), "rust");
        assert_eq!(map.detect(Path::new("a/b/Dockerfile")), "dockerfile");
    }

    #[test]
    fn test_detect_is_case_sensitive() {
        let map = map();
        assert_eq!(map.detect(Path::new("x.r")), LanguageId::PLAINTEXT);
        assert_eq!(map.detect(Path::new("x.R")), "r");
        assert_eq!(map.detect(Path::new("dockerfile")), LanguageId::PLAINTEXT);
    }

    #[test]
    fn test_detect_falls_back_to_plaintext() {
        let map = map();
        for unmapped in ["x.unknown", "Makefile", ".gitignore", "x.", "x.r\u{e9}"] {
            assert_eq!(map.detect(Path::new(unmapped)), LanguageId::PLAINTEXT);
        }
    }

    #[test]
    fn test_a_file_with_an_extension_never_matches_a_name() {
        let map = map();
        assert_eq!(
            map.detect(Path::new("Dockerfile.bak")),
            LanguageId::PLAINTEXT
        );
    }

    #[test]
    fn test_file_key_of() {
        assert_eq!(
            FileKey::of(Path::new("a.tar.gz")),
            FileKey::Extension(FileExtension::from_static("gz"))
        );
        assert_eq!(
            FileKey::of(Path::new("Makefile")),
            FileKey::Name(FileName::from_static("Makefile"))
        );
        for unmappable in [".gitignore", "x.", "r\u{e9}", "a.\u{e9}", "", ".."] {
            assert_eq!(FileKey::of(Path::new(unmappable)), FileKey::Unmappable);
        }
    }

    #[test]
    fn test_languages_lists_both_maps() {
        let map = map();
        let languages: Vec<&LanguageId> = map.languages().collect();
        assert_eq!(languages.len(), 3);
        assert!(languages.contains(&&LanguageId::from_static("dockerfile")));
    }
}
