//! Document state management.
//!
//! Tracks open documents and their versions for LSP synchronization.

use lsp_types::Uri;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// State of a single document.
#[derive(Debug, Clone)]
pub struct DocumentState {
    /// Document URI.
    pub uri: Uri,
    /// Language identifier.
    pub language_id: String,
    /// Document version (monotonically increasing).
    pub version: i32,
    /// Document content.
    pub content: String,
}

/// Tracks document state across the workspace.
#[derive(Debug, Default)]
pub struct DocumentTracker {
    /// Open documents by file path.
    documents: HashMap<PathBuf, DocumentState>,
}

impl DocumentTracker {
    /// Create a new document tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if a document is currently open.
    #[must_use]
    pub fn is_open(&self, path: &Path) -> bool {
        self.documents.contains_key(path)
    }

    /// Get the state of an open document.
    #[must_use]
    pub fn get(&self, path: &Path) -> Option<&DocumentState> {
        self.documents.get(path)
    }

    /// Get the number of open documents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Check if there are no open documents.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Open a document and track its state.
    ///
    /// Returns the document URI for use in LSP requests.
    pub fn open(&mut self, path: PathBuf, content: String) -> Uri {
        let uri = path_to_uri(&path);
        let language_id = detect_language(&path);

        let state = DocumentState {
            uri: uri.clone(),
            language_id,
            version: 1,
            content,
        };

        self.documents.insert(path, state);
        uri
    }

    /// Update a document's content and increment its version.
    ///
    /// Returns `None` if the document is not open.
    pub fn update(&mut self, path: &Path, content: String) -> Option<i32> {
        if let Some(state) = self.documents.get_mut(path) {
            state.version += 1;
            state.content = content;
            Some(state.version)
        } else {
            None
        }
    }

    /// Close a document and remove it from tracking.
    ///
    /// Returns the document state if it was open.
    pub fn close(&mut self, path: &Path) -> Option<DocumentState> {
        self.documents.remove(path)
    }

    /// Close all documents.
    pub fn close_all(&mut self) -> Vec<DocumentState> {
        self.documents.drain().map(|(_, state)| state).collect()
    }
}

/// Convert a file path to a URI.
#[must_use]
pub fn path_to_uri(path: &Path) -> Uri {
    // Convert path to file:// URI string and parse
    let uri_string = if cfg!(windows) {
        format!("file:///{}", path.display().to_string().replace('\\', "/"))
    } else {
        format!("file://{}", path.display())
    };
    uri_string.parse().expect("failed to create URI from path")
}

/// Detect the language ID from a file path.
#[must_use]
pub fn detect_language(path: &Path) -> String {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    match extension {
        "rs" => "rust",
        "py" | "pyw" | "pyi" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "jsx" => "javascriptreact",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "scala" | "sc" => "scala",
        "zig" => "zig",
        "lua" => "lua",
        "sh" | "bash" | "zsh" => "shellscript",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "xml" => "xml",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        "md" | "markdown" => "markdown",
        _ => "plaintext",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_language() {
        assert_eq!(detect_language(Path::new("main.rs")), "rust");
        assert_eq!(detect_language(Path::new("script.py")), "python");
        assert_eq!(detect_language(Path::new("app.ts")), "typescript");
        assert_eq!(detect_language(Path::new("unknown.xyz")), "plaintext");
    }

    #[test]
    fn test_document_tracker() {
        let mut tracker = DocumentTracker::new();
        let path = PathBuf::from("/test/file.rs");

        assert!(!tracker.is_open(&path));

        tracker.open(path.clone(), "fn main() {}".to_string());
        assert!(tracker.is_open(&path));
        assert_eq!(tracker.len(), 1);

        let state = tracker.get(&path).unwrap();
        assert_eq!(state.version, 1);
        assert_eq!(state.language_id, "rust");

        let new_version = tracker.update(&path, "fn main() { println!() }".to_string());
        assert_eq!(new_version, Some(2));

        tracker.close(&path);
        assert!(!tracker.is_open(&path));
        assert!(tracker.is_empty());
    }
}
