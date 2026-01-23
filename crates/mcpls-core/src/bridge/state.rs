//! Document state management.
//!
//! Tracks open documents and their versions for LSP synchronization.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::{DidOpenTextDocumentParams, TextDocumentItem, Uri};

use crate::error::{Error, Result};
use crate::lsp::LspClient;

/// State of a single document.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Resource limits for document tracking.
#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    /// Maximum number of open documents (0 = unlimited).
    pub max_documents: usize,
    /// Maximum file size in bytes (0 = unlimited).
    pub max_file_size: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_documents: 100,
            max_file_size: 10 * 1024 * 1024, // 10MB
        }
    }
}

/// Tracks document state across the workspace.
#[derive(Debug)]
pub struct DocumentTracker {
    /// Open documents by file path.
    documents: HashMap<PathBuf, DocumentState>,
    /// Resource limits for tracking.
    limits: ResourceLimits,
}

impl Default for DocumentTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DocumentTracker {
    /// Create a new document tracker with default limits.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(ResourceLimits::default())
    }

    /// Create a new document tracker with custom limits.
    #[must_use]
    pub fn with_limits(limits: ResourceLimits) -> Self {
        Self {
            documents: HashMap::new(),
            limits,
        }
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

    /// Get all tracked documents.
    #[must_use]
    pub const fn documents(&self) -> &HashMap<PathBuf, DocumentState> {
        &self.documents
    }

    /// Open a document and track its state.
    ///
    /// Returns the document URI for use in LSP requests.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Document limit is exceeded
    /// - File size limit is exceeded
    pub fn open(&mut self, path: PathBuf, content: String) -> Result<Uri> {
        // Check document limit
        if self.limits.max_documents > 0 && self.documents.len() >= self.limits.max_documents {
            return Err(Error::DocumentLimitExceeded {
                current: self.documents.len(),
                max: self.limits.max_documents,
            });
        }

        // Check file size limit
        let size = content.len() as u64;
        if self.limits.max_file_size > 0 && size > self.limits.max_file_size {
            return Err(Error::FileSizeLimitExceeded {
                size,
                max: self.limits.max_file_size,
            });
        }

        let uri = path_to_uri(&path);
        let language_id = detect_language(&path);

        let state = DocumentState {
            uri: uri.clone(),
            language_id,
            version: 1,
            content,
        };

        self.documents.insert(path, state);
        Ok(uri)
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

    /// Ensure a document is open, opening it lazily if necessary.
    ///
    /// If the document is already open, returns its URI immediately.
    /// Otherwise, reads the file from disk, opens it in the tracker,
    /// and sends a `didOpen` notification to the LSP server.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file cannot be read from disk
    /// - The `didOpen` notification fails to send
    /// - Resource limits are exceeded
    pub async fn ensure_open(&mut self, path: &Path, lsp_client: &LspClient) -> Result<Uri> {
        if let Some(state) = self.documents.get(path) {
            return Ok(state.uri.clone());
        }

        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| Error::FileIo {
                path: path.to_path_buf(),
                source: e,
            })?;

        let uri = self.open(path.to_path_buf(), content.clone())?;
        let state = self
            .documents
            .get(path)
            .ok_or_else(|| Error::DocumentNotFound(path.to_path_buf()))?;

        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: state.language_id.clone(),
                version: state.version,
                text: content,
            },
        };

        lsp_client.notify("textDocument/didOpen", params).await?;

        Ok(uri)
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
    // Path-to-URI conversion should always succeed for valid paths
    #[allow(clippy::expect_used)]
    uri_string.parse().expect("failed to create URI from path")
}

/// Detect the language ID from a file path.
#[must_use]
pub fn detect_language(path: &Path) -> String {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");

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
#[allow(clippy::unwrap_used)]
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

        tracker
            .open(path.clone(), "fn main() {}".to_string())
            .unwrap();
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

    #[test]
    fn test_document_limit() {
        let limits = ResourceLimits {
            max_documents: 2,
            max_file_size: 100,
        };
        let mut tracker = DocumentTracker::with_limits(limits);

        // First two documents should succeed
        tracker
            .open(PathBuf::from("/test/file1.rs"), "fn test1() {}".to_string())
            .unwrap();
        tracker
            .open(PathBuf::from("/test/file2.rs"), "fn test2() {}".to_string())
            .unwrap();

        // Third should fail
        let result = tracker.open(PathBuf::from("/test/file3.rs"), "fn test3() {}".to_string());
        assert!(matches!(result, Err(Error::DocumentLimitExceeded { .. })));
    }

    #[test]
    fn test_file_size_limit() {
        let limits = ResourceLimits {
            max_documents: 10,
            max_file_size: 10,
        };
        let mut tracker = DocumentTracker::with_limits(limits);

        // Small file should succeed
        tracker
            .open(PathBuf::from("/test/small.rs"), "fn f(){}".to_string())
            .unwrap();

        // Large file should fail
        let large_content = "x".repeat(100);
        let result = tracker.open(PathBuf::from("/test/large.rs"), large_content);
        assert!(matches!(result, Err(Error::FileSizeLimitExceeded { .. })));
    }

    #[test]
    fn test_resource_limits_default() {
        let limits = ResourceLimits::default();
        assert_eq!(limits.max_documents, 100);
        assert_eq!(limits.max_file_size, 10 * 1024 * 1024);
    }

    #[test]
    fn test_resource_limits_custom() {
        let limits = ResourceLimits {
            max_documents: 50,
            max_file_size: 5 * 1024 * 1024,
        };
        assert_eq!(limits.max_documents, 50);
        assert_eq!(limits.max_file_size, 5 * 1024 * 1024);
    }

    #[test]
    fn test_resource_limits_zero_unlimited() {
        let limits = ResourceLimits {
            max_documents: 0,
            max_file_size: 0,
        };
        let mut tracker = DocumentTracker::with_limits(limits);

        // Should allow many documents when limit is 0
        for i in 0..200 {
            tracker
                .open(
                    PathBuf::from(format!("/test/file{i}.rs")),
                    "content".to_string(),
                )
                .unwrap();
        }
        assert_eq!(tracker.len(), 200);

        // Should allow large files when limit is 0
        let huge_content = "x".repeat(100_000_000);
        tracker
            .open(PathBuf::from("/test/huge.rs"), huge_content)
            .unwrap();
    }

    #[test]
    fn test_document_tracker_default() {
        let tracker = DocumentTracker::default();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn test_document_state_clone() {
        let state = DocumentState {
            uri: "file:///test.rs".parse().unwrap(),
            language_id: "rust".to_string(),
            version: 5,
            content: "fn main() {}".to_string(),
        };

        #[allow(clippy::redundant_clone)]
        let cloned = state.clone();
        assert_eq!(cloned.uri, state.uri);
        assert_eq!(cloned.language_id, state.language_id);
        assert_eq!(cloned.version, 5);
        assert_eq!(cloned.content, state.content);
    }

    #[test]
    fn test_update_nonexistent_document() {
        let mut tracker = DocumentTracker::new();
        let path = PathBuf::from("/test/nonexistent.rs");

        let version = tracker.update(&path, "new content".to_string());
        assert_eq!(
            version, None,
            "Updating non-existent document should return None"
        );
    }

    #[test]
    fn test_close_nonexistent_document() {
        let mut tracker = DocumentTracker::new();
        let path = PathBuf::from("/test/nonexistent.rs");

        let state = tracker.close(&path);
        assert_eq!(
            state, None,
            "Closing non-existent document should return None"
        );
    }

    #[test]
    fn test_close_all_documents() {
        let mut tracker = DocumentTracker::new();

        tracker
            .open(PathBuf::from("/test/file1.rs"), "content1".to_string())
            .unwrap();
        tracker
            .open(PathBuf::from("/test/file2.rs"), "content2".to_string())
            .unwrap();
        tracker
            .open(PathBuf::from("/test/file3.rs"), "content3".to_string())
            .unwrap();

        assert_eq!(tracker.len(), 3);

        let closed = tracker.close_all();
        assert_eq!(closed.len(), 3);
        assert!(tracker.is_empty());
    }

    #[test]
    fn test_get_nonexistent_document() {
        let tracker = DocumentTracker::new();
        let path = PathBuf::from("/test/nonexistent.rs");

        let state = tracker.get(&path);
        assert!(
            state.is_none(),
            "Getting non-existent document should return None"
        );
    }

    #[test]
    fn test_document_version_increments() {
        let mut tracker = DocumentTracker::new();
        let path = PathBuf::from("/test/versioned.rs");

        tracker.open(path.clone(), "v1".to_string()).unwrap();
        assert_eq!(tracker.get(&path).unwrap().version, 1);

        tracker.update(&path, "v2".to_string());
        assert_eq!(tracker.get(&path).unwrap().version, 2);

        tracker.update(&path, "v3".to_string());
        assert_eq!(tracker.get(&path).unwrap().version, 3);

        tracker.update(&path, "v4".to_string());
        assert_eq!(tracker.get(&path).unwrap().version, 4);
    }

    #[test]
    fn test_detect_language_all_extensions() {
        assert_eq!(detect_language(Path::new("main.rs")), "rust");
        assert_eq!(detect_language(Path::new("script.py")), "python");
        assert_eq!(detect_language(Path::new("script.pyw")), "python");
        assert_eq!(detect_language(Path::new("script.pyi")), "python");
        assert_eq!(detect_language(Path::new("app.js")), "javascript");
        assert_eq!(detect_language(Path::new("app.mjs")), "javascript");
        assert_eq!(detect_language(Path::new("app.cjs")), "javascript");
        assert_eq!(detect_language(Path::new("app.ts")), "typescript");
        assert_eq!(detect_language(Path::new("app.mts")), "typescript");
        assert_eq!(detect_language(Path::new("app.cts")), "typescript");
        assert_eq!(
            detect_language(Path::new("component.tsx")),
            "typescriptreact"
        );
        assert_eq!(
            detect_language(Path::new("component.jsx")),
            "javascriptreact"
        );
        assert_eq!(detect_language(Path::new("main.go")), "go");
        assert_eq!(detect_language(Path::new("main.c")), "c");
        assert_eq!(detect_language(Path::new("header.h")), "c");
        assert_eq!(detect_language(Path::new("main.cpp")), "cpp");
        assert_eq!(detect_language(Path::new("main.cc")), "cpp");
        assert_eq!(detect_language(Path::new("main.cxx")), "cpp");
        assert_eq!(detect_language(Path::new("header.hpp")), "cpp");
        assert_eq!(detect_language(Path::new("header.hh")), "cpp");
        assert_eq!(detect_language(Path::new("header.hxx")), "cpp");
        assert_eq!(detect_language(Path::new("Main.java")), "java");
        assert_eq!(detect_language(Path::new("script.rb")), "ruby");
        assert_eq!(detect_language(Path::new("index.php")), "php");
        assert_eq!(detect_language(Path::new("App.swift")), "swift");
        assert_eq!(detect_language(Path::new("Main.kt")), "kotlin");
        assert_eq!(detect_language(Path::new("script.kts")), "kotlin");
        assert_eq!(detect_language(Path::new("Main.scala")), "scala");
        assert_eq!(detect_language(Path::new("script.sc")), "scala");
        assert_eq!(detect_language(Path::new("main.zig")), "zig");
        assert_eq!(detect_language(Path::new("script.lua")), "lua");
        assert_eq!(detect_language(Path::new("script.sh")), "shellscript");
        assert_eq!(detect_language(Path::new("script.bash")), "shellscript");
        assert_eq!(detect_language(Path::new("script.zsh")), "shellscript");
        assert_eq!(detect_language(Path::new("data.json")), "json");
        assert_eq!(detect_language(Path::new("config.toml")), "toml");
        assert_eq!(detect_language(Path::new("config.yaml")), "yaml");
        assert_eq!(detect_language(Path::new("config.yml")), "yaml");
        assert_eq!(detect_language(Path::new("data.xml")), "xml");
        assert_eq!(detect_language(Path::new("index.html")), "html");
        assert_eq!(detect_language(Path::new("index.htm")), "html");
        assert_eq!(detect_language(Path::new("styles.css")), "css");
        assert_eq!(detect_language(Path::new("styles.scss")), "scss");
        assert_eq!(detect_language(Path::new("styles.less")), "less");
        assert_eq!(detect_language(Path::new("README.md")), "markdown");
        assert_eq!(detect_language(Path::new("README.markdown")), "markdown");
        assert_eq!(detect_language(Path::new("unknown.xyz")), "plaintext");
        assert_eq!(detect_language(Path::new("no_extension")), "plaintext");
    }

    #[test]
    fn test_path_to_uri_unix() {
        #[cfg(not(windows))]
        {
            let path = Path::new("/home/user/project/main.rs");
            let uri = path_to_uri(path);
            assert!(
                uri.as_str()
                    .starts_with("file:///home/user/project/main.rs")
            );
        }
    }

    #[test]
    fn test_path_to_uri_with_special_chars() {
        let path = Path::new("/home/user/project-test/main.rs");
        let uri = path_to_uri(path);
        assert!(uri.as_str().starts_with("file://"));
        assert!(uri.as_str().contains("project-test"));
    }

    #[test]
    fn test_document_tracker_concurrent_operations() {
        let mut tracker = DocumentTracker::new();
        let path1 = PathBuf::from("/test/file1.rs");
        let path2 = PathBuf::from("/test/file2.rs");

        tracker.open(path1.clone(), "content1".to_string()).unwrap();
        tracker.open(path2.clone(), "content2".to_string()).unwrap();

        assert_eq!(tracker.len(), 2);
        assert!(tracker.is_open(&path1));
        assert!(tracker.is_open(&path2));

        tracker.update(&path1, "new content1".to_string());
        assert_eq!(tracker.get(&path1).unwrap().content, "new content1");
        assert_eq!(tracker.get(&path2).unwrap().content, "content2");

        tracker.close(&path1);
        assert_eq!(tracker.len(), 1);
        assert!(!tracker.is_open(&path1));
        assert!(tracker.is_open(&path2));
    }

    #[test]
    fn test_empty_content() {
        let mut tracker = DocumentTracker::new();
        let path = PathBuf::from("/test/empty.rs");

        tracker.open(path.clone(), String::new()).unwrap();
        assert!(tracker.is_open(&path));
        assert_eq!(tracker.get(&path).unwrap().content, "");
    }

    #[test]
    fn test_unicode_content() {
        let mut tracker = DocumentTracker::new();
        let path = PathBuf::from("/test/unicode.rs");
        let content = "fn テスト() { println!(\"こんにちは\"); }";

        tracker.open(path.clone(), content.to_string()).unwrap();
        assert_eq!(tracker.get(&path).unwrap().content, content);
    }

    #[test]
    fn test_document_limit_exact_boundary() {
        let limits = ResourceLimits {
            max_documents: 5,
            max_file_size: 1000,
        };
        let mut tracker = DocumentTracker::with_limits(limits);

        for i in 0..5 {
            tracker
                .open(
                    PathBuf::from(format!("/test/file{i}.rs")),
                    "content".to_string(),
                )
                .unwrap();
        }

        assert_eq!(tracker.len(), 5);

        let result = tracker.open(PathBuf::from("/test/file6.rs"), "content".to_string());
        assert!(matches!(result, Err(Error::DocumentLimitExceeded { .. })));
    }

    #[test]
    fn test_file_size_exact_boundary() {
        let limits = ResourceLimits {
            max_documents: 10,
            max_file_size: 100,
        };
        let mut tracker = DocumentTracker::with_limits(limits);

        let exact_size_content = "x".repeat(100);
        tracker
            .open(PathBuf::from("/test/exact.rs"), exact_size_content)
            .unwrap();

        let over_size_content = "x".repeat(101);
        let result = tracker.open(PathBuf::from("/test/over.rs"), over_size_content);
        assert!(matches!(result, Err(Error::FileSizeLimitExceeded { .. })));
    }

    #[test]
    fn test_documents_accessor_returns_empty_map_for_new_tracker() {
        let tracker = DocumentTracker::new();
        let docs = tracker.documents();
        assert!(docs.is_empty());
    }

    #[test]
    fn test_documents_accessor_returns_all_open_documents() {
        let mut tracker = DocumentTracker::new();
        let path1 = PathBuf::from("/test/file1.rs");
        let path2 = PathBuf::from("/test/file2.rs");

        tracker.open(path1.clone(), "content1".to_string()).unwrap();
        tracker.open(path2.clone(), "content2".to_string()).unwrap();

        let docs = tracker.documents();
        assert_eq!(docs.len(), 2);
        assert!(docs.contains_key(&path1));
        assert!(docs.contains_key(&path2));
    }

    #[test]
    fn test_documents_accessor_reflects_document_state() {
        let mut tracker = DocumentTracker::new();
        let path = PathBuf::from("/test/file.rs");

        tracker.open(path.clone(), "initial".to_string()).unwrap();
        tracker.update(&path, "updated".to_string());

        let docs = tracker.documents();
        let state = docs.get(&path).unwrap();
        assert_eq!(state.content, "updated");
        assert_eq!(state.version, 2);
    }
}
