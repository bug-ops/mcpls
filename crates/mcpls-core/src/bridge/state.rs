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
    /// Custom file extension to language ID mappings.
    extension_map: HashMap<String, String>,
}

impl DocumentTracker {
    /// Create a new document tracker with custom limits and extension mappings.
    #[must_use]
    pub fn new(limits: ResourceLimits, extension_map: HashMap<String, String>) -> Self {
        Self {
            documents: HashMap::new(),
            limits,
            extension_map,
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
        let language_id = detect_language(&path, &self.extension_map);

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
///
/// Consults the extension map to determine the language ID for a file.
/// If the extension is not found in the map, returns "plaintext".
#[must_use]
pub fn detect_language(path: &Path, extension_map: &HashMap<String, String>) -> String {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    extension_map
        .get(extension)
        .cloned()
        .unwrap_or_else(|| "plaintext".to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_language() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());
        map.insert("py".to_string(), "python".to_string());
        map.insert("ts".to_string(), "typescript".to_string());

        assert_eq!(detect_language(Path::new("main.rs"), &map), "rust");
        assert_eq!(detect_language(Path::new("script.py"), &map), "python");
        assert_eq!(detect_language(Path::new("app.ts"), &map), "typescript");
        assert_eq!(detect_language(Path::new("unknown.xyz"), &map), "plaintext");
    }

    #[test]
    fn test_document_tracker() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let mut tracker = DocumentTracker::new(ResourceLimits::default(), map);
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
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let mut tracker = DocumentTracker::new(limits, map);

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
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let mut tracker = DocumentTracker::new(limits, map);

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
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let mut tracker = DocumentTracker::new(limits, map);

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
        let map = HashMap::new();
        let mut tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/nonexistent.rs");

        let version = tracker.update(&path, "new content".to_string());
        assert_eq!(
            version, None,
            "Updating non-existent document should return None"
        );
    }

    #[test]
    fn test_close_nonexistent_document() {
        let map = HashMap::new();
        let mut tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/nonexistent.rs");

        let state = tracker.close(&path);
        assert_eq!(
            state, None,
            "Closing non-existent document should return None"
        );
    }

    #[test]
    fn test_close_all_documents() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let mut tracker = DocumentTracker::new(ResourceLimits::default(), map);

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
        let map = HashMap::new();
        let tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/nonexistent.rs");

        let state = tracker.get(&path);
        assert!(
            state.is_none(),
            "Getting non-existent document should return None"
        );
    }

    #[test]
    fn test_document_version_increments() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let mut tracker = DocumentTracker::new(ResourceLimits::default(), map);
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
    #[allow(clippy::too_many_lines)]
    fn test_detect_language_all_extensions() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());
        map.insert("py".to_string(), "python".to_string());
        map.insert("pyw".to_string(), "python".to_string());
        map.insert("pyi".to_string(), "python".to_string());
        map.insert("js".to_string(), "javascript".to_string());
        map.insert("mjs".to_string(), "javascript".to_string());
        map.insert("cjs".to_string(), "javascript".to_string());
        map.insert("ts".to_string(), "typescript".to_string());
        map.insert("mts".to_string(), "typescript".to_string());
        map.insert("cts".to_string(), "typescript".to_string());
        map.insert("tsx".to_string(), "typescriptreact".to_string());
        map.insert("jsx".to_string(), "javascriptreact".to_string());
        map.insert("go".to_string(), "go".to_string());
        map.insert("c".to_string(), "c".to_string());
        map.insert("h".to_string(), "c".to_string());
        map.insert("cpp".to_string(), "cpp".to_string());
        map.insert("cc".to_string(), "cpp".to_string());
        map.insert("cxx".to_string(), "cpp".to_string());
        map.insert("hpp".to_string(), "cpp".to_string());
        map.insert("hh".to_string(), "cpp".to_string());
        map.insert("hxx".to_string(), "cpp".to_string());
        map.insert("java".to_string(), "java".to_string());
        map.insert("rb".to_string(), "ruby".to_string());
        map.insert("php".to_string(), "php".to_string());
        map.insert("swift".to_string(), "swift".to_string());
        map.insert("kt".to_string(), "kotlin".to_string());
        map.insert("kts".to_string(), "kotlin".to_string());
        map.insert("scala".to_string(), "scala".to_string());
        map.insert("sc".to_string(), "scala".to_string());
        map.insert("zig".to_string(), "zig".to_string());
        map.insert("lua".to_string(), "lua".to_string());
        map.insert("sh".to_string(), "shellscript".to_string());
        map.insert("bash".to_string(), "shellscript".to_string());
        map.insert("zsh".to_string(), "shellscript".to_string());
        map.insert("json".to_string(), "json".to_string());
        map.insert("toml".to_string(), "toml".to_string());
        map.insert("yaml".to_string(), "yaml".to_string());
        map.insert("yml".to_string(), "yaml".to_string());
        map.insert("xml".to_string(), "xml".to_string());
        map.insert("html".to_string(), "html".to_string());
        map.insert("htm".to_string(), "html".to_string());
        map.insert("css".to_string(), "css".to_string());
        map.insert("scss".to_string(), "scss".to_string());
        map.insert("less".to_string(), "less".to_string());
        map.insert("md".to_string(), "markdown".to_string());
        map.insert("markdown".to_string(), "markdown".to_string());

        assert_eq!(detect_language(Path::new("main.rs"), &map), "rust");
        assert_eq!(detect_language(Path::new("script.py"), &map), "python");
        assert_eq!(detect_language(Path::new("script.pyw"), &map), "python");
        assert_eq!(detect_language(Path::new("script.pyi"), &map), "python");
        assert_eq!(detect_language(Path::new("app.js"), &map), "javascript");
        assert_eq!(detect_language(Path::new("app.mjs"), &map), "javascript");
        assert_eq!(detect_language(Path::new("app.cjs"), &map), "javascript");
        assert_eq!(detect_language(Path::new("app.ts"), &map), "typescript");
        assert_eq!(detect_language(Path::new("app.mts"), &map), "typescript");
        assert_eq!(detect_language(Path::new("app.cts"), &map), "typescript");
        assert_eq!(
            detect_language(Path::new("component.tsx"), &map),
            "typescriptreact"
        );
        assert_eq!(
            detect_language(Path::new("component.jsx"), &map),
            "javascriptreact"
        );
        assert_eq!(detect_language(Path::new("main.go"), &map), "go");
        assert_eq!(detect_language(Path::new("main.c"), &map), "c");
        assert_eq!(detect_language(Path::new("header.h"), &map), "c");
        assert_eq!(detect_language(Path::new("main.cpp"), &map), "cpp");
        assert_eq!(detect_language(Path::new("main.cc"), &map), "cpp");
        assert_eq!(detect_language(Path::new("main.cxx"), &map), "cpp");
        assert_eq!(detect_language(Path::new("header.hpp"), &map), "cpp");
        assert_eq!(detect_language(Path::new("header.hh"), &map), "cpp");
        assert_eq!(detect_language(Path::new("header.hxx"), &map), "cpp");
        assert_eq!(detect_language(Path::new("Main.java"), &map), "java");
        assert_eq!(detect_language(Path::new("script.rb"), &map), "ruby");
        assert_eq!(detect_language(Path::new("index.php"), &map), "php");
        assert_eq!(detect_language(Path::new("App.swift"), &map), "swift");
        assert_eq!(detect_language(Path::new("Main.kt"), &map), "kotlin");
        assert_eq!(detect_language(Path::new("script.kts"), &map), "kotlin");
        assert_eq!(detect_language(Path::new("Main.scala"), &map), "scala");
        assert_eq!(detect_language(Path::new("script.sc"), &map), "scala");
        assert_eq!(detect_language(Path::new("main.zig"), &map), "zig");
        assert_eq!(detect_language(Path::new("script.lua"), &map), "lua");
        assert_eq!(detect_language(Path::new("script.sh"), &map), "shellscript");
        assert_eq!(
            detect_language(Path::new("script.bash"), &map),
            "shellscript"
        );
        assert_eq!(
            detect_language(Path::new("script.zsh"), &map),
            "shellscript"
        );
        assert_eq!(detect_language(Path::new("data.json"), &map), "json");
        assert_eq!(detect_language(Path::new("config.toml"), &map), "toml");
        assert_eq!(detect_language(Path::new("config.yaml"), &map), "yaml");
        assert_eq!(detect_language(Path::new("config.yml"), &map), "yaml");
        assert_eq!(detect_language(Path::new("data.xml"), &map), "xml");
        assert_eq!(detect_language(Path::new("index.html"), &map), "html");
        assert_eq!(detect_language(Path::new("index.htm"), &map), "html");
        assert_eq!(detect_language(Path::new("styles.css"), &map), "css");
        assert_eq!(detect_language(Path::new("styles.scss"), &map), "scss");
        assert_eq!(detect_language(Path::new("styles.less"), &map), "less");
        assert_eq!(detect_language(Path::new("README.md"), &map), "markdown");
        assert_eq!(
            detect_language(Path::new("README.markdown"), &map),
            "markdown"
        );
        assert_eq!(detect_language(Path::new("unknown.xyz"), &map), "plaintext");
        assert_eq!(
            detect_language(Path::new("no_extension"), &map),
            "plaintext"
        );
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
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let mut tracker = DocumentTracker::new(ResourceLimits::default(), map);
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
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let mut tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/empty.rs");

        tracker.open(path.clone(), String::new()).unwrap();
        assert!(tracker.is_open(&path));
        assert_eq!(tracker.get(&path).unwrap().content, "");
    }

    #[test]
    fn test_unicode_content() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let mut tracker = DocumentTracker::new(ResourceLimits::default(), map);
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
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let mut tracker = DocumentTracker::new(limits, map);

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
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let mut tracker = DocumentTracker::new(limits, map);

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
    fn test_detect_language_with_custom_extension() {
        let mut map = HashMap::new();
        map.insert("nu".to_string(), "nushell".to_string());

        assert_eq!(detect_language(Path::new("script.nu"), &map), "nushell");

        let empty_map = HashMap::new();
        assert_eq!(
            detect_language(Path::new("script.nu"), &empty_map),
            "plaintext"
        );
    }

    #[test]
    fn test_detect_language_custom_overrides_default() {
        let mut custom_map = HashMap::new();
        custom_map.insert("rs".to_string(), "custom-rust".to_string());

        assert_eq!(
            detect_language(Path::new("main.rs"), &custom_map),
            "custom-rust"
        );

        let mut default_map = HashMap::new();
        default_map.insert("rs".to_string(), "rust".to_string());

        assert_eq!(detect_language(Path::new("main.rs"), &default_map), "rust");
    }

    #[test]
    fn test_detect_language_fallback_to_plaintext() {
        let mut map = HashMap::new();
        map.insert("nu".to_string(), "nushell".to_string());

        // .rs not in custom map, should return plaintext
        assert_eq!(detect_language(Path::new("main.rs"), &map), "plaintext");
    }

    #[test]
    fn test_detect_language_empty_map() {
        let map = HashMap::new();
        assert_eq!(detect_language(Path::new("main.rs"), &map), "plaintext");
    }

    #[test]
    fn test_document_tracker_with_extensions() {
        let mut map = HashMap::new();
        map.insert("nu".to_string(), "nushell".to_string());

        let mut tracker = DocumentTracker::new(ResourceLimits::default(), map);

        let path = PathBuf::from("/test/script.nu");
        tracker
            .open(path.clone(), "# nushell script".to_string())
            .unwrap();

        let state = tracker.get(&path).unwrap();
        assert_eq!(state.language_id, "nushell");
    }

    #[test]
    fn test_document_tracker_uses_provided_map() {
        let mut map = HashMap::new();
        map.insert("rs".to_string(), "rust".to_string());

        let mut tracker = DocumentTracker::new(ResourceLimits::default(), map);
        let path = PathBuf::from("/test/main.rs");
        tracker
            .open(path.clone(), "fn main() {}".to_string())
            .unwrap();

        let state = tracker.get(&path).unwrap();
        assert_eq!(state.language_id, "rust");
    }

    #[test]
    fn test_multiple_extensions_same_language() {
        let mut map = HashMap::new();
        map.insert("cpp".to_string(), "c++".to_string());
        map.insert("cc".to_string(), "c++".to_string());
        map.insert("cxx".to_string(), "c++".to_string());

        assert_eq!(detect_language(Path::new("main.cpp"), &map), "c++");
        assert_eq!(detect_language(Path::new("main.cc"), &map), "c++");
        assert_eq!(detect_language(Path::new("main.cxx"), &map), "c++");
    }

    #[test]
    fn test_case_sensitive_extensions() {
        let mut map = HashMap::new();
        map.insert("NU".to_string(), "nushell".to_string());

        // Lowercase .nu should not match uppercase "NU" in map
        assert_eq!(detect_language(Path::new("script.nu"), &map), "plaintext");
    }
}
