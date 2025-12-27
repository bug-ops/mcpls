//! LSP notification storage and management.
//!
//! Stores diagnostics, log messages, and server messages received from LSP servers.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
use lsp_types::{Diagnostic as LspDiagnostic, Uri};
use serde::{Deserialize, Serialize};

/// Maximum number of log entries to store.
const MAX_LOG_ENTRIES: usize = 100;

/// Maximum number of server messages to store.
const MAX_SERVER_MESSAGES: usize = 50;

/// Information about diagnostics for a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticInfo {
    /// URI of the document.
    pub uri: Uri,
    /// Document version when diagnostics were received.
    pub version: Option<i32>,
    /// List of diagnostics.
    pub diagnostics: Vec<LspDiagnostic>,
}

/// A log entry from the LSP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Log level.
    pub level: LogLevel,
    /// Log message.
    pub message: String,
    /// Timestamp when the log was received.
    pub timestamp: DateTime<Utc>,
}

/// Log severity level.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Error log level.
    Error,
    /// Warning log level.
    Warning,
    /// Info log level.
    Info,
    /// Debug log level.
    Debug,
}

/// A message from the LSP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMessage {
    /// Message type.
    pub message_type: MessageType,
    /// Message content.
    pub message: String,
    /// Timestamp when the message was received.
    pub timestamp: DateTime<Utc>,
}

/// Server message type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageType {
    /// Error message.
    Error,
    /// Warning message.
    Warning,
    /// Info message.
    Info,
    /// Log message.
    Log,
}

/// Cache for LSP server notifications.
#[derive(Debug)]
pub struct NotificationCache {
    /// Diagnostics indexed by document URI.
    diagnostics: HashMap<String, DiagnosticInfo>,
    /// Recent log entries (FIFO queue with max size).
    logs: VecDeque<LogEntry>,
    /// Recent server messages (FIFO queue with max size).
    messages: VecDeque<ServerMessage>,
}

impl Default for NotificationCache {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationCache {
    /// Create a new notification cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            diagnostics: HashMap::with_capacity(32),
            logs: VecDeque::with_capacity(MAX_LOG_ENTRIES),
            messages: VecDeque::with_capacity(MAX_SERVER_MESSAGES),
        }
    }

    /// Store diagnostics for a document.
    ///
    /// If diagnostics already exist for the URI, they are replaced.
    pub fn store_diagnostics(
        &mut self,
        uri: &Uri,
        version: Option<i32>,
        diagnostics: Vec<LspDiagnostic>,
    ) {
        let info = DiagnosticInfo {
            uri: uri.clone(),
            version,
            diagnostics,
        };
        self.diagnostics.insert(uri.to_string(), info);
    }

    /// Store a log entry.
    ///
    /// Maintains a maximum of `MAX_LOG_ENTRIES` entries, removing oldest when full.
    pub fn store_log(&mut self, level: LogLevel, message: String) {
        let entry = LogEntry {
            level,
            message,
            timestamp: Utc::now(),
        };

        if self.logs.len() >= MAX_LOG_ENTRIES {
            self.logs.pop_front();
        }
        self.logs.push_back(entry);
    }

    /// Store a server message.
    ///
    /// Maintains a maximum of `MAX_SERVER_MESSAGES` entries, removing oldest when full.
    pub fn store_message(&mut self, message_type: MessageType, message: String) {
        let msg = ServerMessage {
            message_type,
            message,
            timestamp: Utc::now(),
        };

        if self.messages.len() >= MAX_SERVER_MESSAGES {
            self.messages.pop_front();
        }
        self.messages.push_back(msg);
    }

    /// Get diagnostics for a document URI.
    #[inline]
    #[must_use]
    pub fn get_diagnostics(&self, uri: &str) -> Option<&DiagnosticInfo> {
        self.diagnostics.get(uri)
    }

    /// Get all stored log entries.
    #[inline]
    #[must_use]
    pub const fn get_logs(&self) -> &VecDeque<LogEntry> {
        &self.logs
    }

    /// Get all stored server messages.
    #[inline]
    #[must_use]
    pub const fn get_messages(&self) -> &VecDeque<ServerMessage> {
        &self.messages
    }

    /// Clear diagnostics for a specific document URI.
    ///
    /// Returns the cleared diagnostics if they existed.
    pub fn clear_diagnostics(&mut self, uri: &str) -> Option<DiagnosticInfo> {
        self.diagnostics.remove(uri)
    }

    /// Clear all diagnostics.
    pub fn clear_all_diagnostics(&mut self) {
        self.diagnostics.clear();
    }

    /// Clear all logs.
    pub fn clear_logs(&mut self) {
        self.logs.clear();
    }

    /// Clear all messages.
    pub fn clear_messages(&mut self) {
        self.messages.clear();
    }

    /// Get the number of documents with stored diagnostics.
    #[inline]
    #[must_use]
    pub fn diagnostics_count(&self) -> usize {
        self.diagnostics.len()
    }

    /// Get the number of stored log entries.
    #[inline]
    #[must_use]
    pub fn logs_count(&self) -> usize {
        self.logs.len()
    }

    /// Get the number of stored server messages.
    #[inline]
    #[must_use]
    pub fn messages_count(&self) -> usize {
        self.messages.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use lsp_types::{Position, Range};

    use super::*;

    #[test]
    fn test_notification_cache_new() {
        let cache = NotificationCache::new();
        assert_eq!(cache.diagnostics_count(), 0);
        assert_eq!(cache.logs_count(), 0);
        assert_eq!(cache.messages_count(), 0);
    }

    #[test]
    fn test_store_and_get_diagnostics() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        let diagnostic = LspDiagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: None,
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&uri, Some(1), vec![diagnostic]);

        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.uri, uri);
        assert_eq!(stored.version, Some(1));
        assert_eq!(stored.diagnostics.len(), 1);
        assert_eq!(stored.diagnostics[0].message, "test error");
    }

    #[test]
    fn test_store_diagnostics_replaces_existing() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        cache.store_diagnostics(&uri, Some(1), vec![]);
        assert_eq!(cache.diagnostics_count(), 1);

        cache.store_diagnostics(&uri, Some(2), vec![]);
        assert_eq!(cache.diagnostics_count(), 1);

        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.version, Some(2));
    }

    #[test]
    fn test_clear_diagnostics() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        cache.store_diagnostics(&uri, Some(1), vec![]);
        assert_eq!(cache.diagnostics_count(), 1);

        let cleared = cache.clear_diagnostics(uri.as_str());
        assert!(cleared.is_some());
        assert_eq!(cache.diagnostics_count(), 0);
    }

    #[test]
    fn test_clear_all_diagnostics() {
        let mut cache = NotificationCache::new();
        let uri1: Uri = "file:///test1.rs".parse().unwrap();
        let uri2: Uri = "file:///test2.rs".parse().unwrap();

        cache.store_diagnostics(&uri1, Some(1), vec![]);
        cache.store_diagnostics(&uri2, Some(1), vec![]);
        assert_eq!(cache.diagnostics_count(), 2);

        cache.clear_all_diagnostics();
        assert_eq!(cache.diagnostics_count(), 0);
    }

    #[test]
    fn test_store_and_get_logs() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error message".to_string());
        cache.store_log(LogLevel::Info, "info message".to_string());

        let logs = cache.get_logs();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].level, LogLevel::Error);
        assert_eq!(logs[0].message, "error message");
        assert_eq!(logs[1].level, LogLevel::Info);
        assert_eq!(logs[1].message, "info message");
    }

    #[test]
    fn test_logs_max_capacity() {
        let mut cache = NotificationCache::new();

        // Add more than MAX_LOG_ENTRIES
        for i in 0..MAX_LOG_ENTRIES + 10 {
            cache.store_log(LogLevel::Info, format!("message {i}"));
        }

        assert_eq!(cache.logs_count(), MAX_LOG_ENTRIES);

        // Oldest entries should be removed (FIFO)
        let logs = cache.get_logs();
        assert_eq!(logs.front().unwrap().message, "message 10");
        assert_eq!(
            logs.back().unwrap().message,
            format!("message {}", MAX_LOG_ENTRIES + 9)
        );
    }

    #[test]
    fn test_clear_logs() {
        let mut cache = NotificationCache::new();
        cache.store_log(LogLevel::Info, "test".to_string());
        assert_eq!(cache.logs_count(), 1);

        cache.clear_logs();
        assert_eq!(cache.logs_count(), 0);
    }

    #[test]
    fn test_store_and_get_messages() {
        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Error, "error msg".to_string());
        cache.store_message(MessageType::Warning, "warning msg".to_string());

        let messages = cache.get_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].message_type, MessageType::Error);
        assert_eq!(messages[0].message, "error msg");
        assert_eq!(messages[1].message_type, MessageType::Warning);
        assert_eq!(messages[1].message, "warning msg");
    }

    #[test]
    fn test_messages_max_capacity() {
        let mut cache = NotificationCache::new();

        // Add more than MAX_SERVER_MESSAGES
        for i in 0..MAX_SERVER_MESSAGES + 10 {
            cache.store_message(MessageType::Info, format!("message {i}"));
        }

        assert_eq!(cache.messages_count(), MAX_SERVER_MESSAGES);

        // Oldest entries should be removed (FIFO)
        let messages = cache.get_messages();
        assert_eq!(messages.front().unwrap().message, "message 10");
        assert_eq!(
            messages.back().unwrap().message,
            format!("message {}", MAX_SERVER_MESSAGES + 9)
        );
    }

    #[test]
    fn test_clear_messages() {
        let mut cache = NotificationCache::new();
        cache.store_message(MessageType::Info, "test".to_string());
        assert_eq!(cache.messages_count(), 1);

        cache.clear_messages();
        assert_eq!(cache.messages_count(), 0);
    }

    #[test]
    fn test_log_levels() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Error, "error".to_string());
        cache.store_log(LogLevel::Warning, "warning".to_string());
        cache.store_log(LogLevel::Info, "info".to_string());
        cache.store_log(LogLevel::Debug, "debug".to_string());

        let logs = cache.get_logs();
        assert_eq!(logs[0].level, LogLevel::Error);
        assert_eq!(logs[1].level, LogLevel::Warning);
        assert_eq!(logs[2].level, LogLevel::Info);
        assert_eq!(logs[3].level, LogLevel::Debug);
    }

    #[test]
    fn test_message_types() {
        let mut cache = NotificationCache::new();

        cache.store_message(MessageType::Error, "error".to_string());
        cache.store_message(MessageType::Warning, "warning".to_string());
        cache.store_message(MessageType::Info, "info".to_string());
        cache.store_message(MessageType::Log, "log".to_string());

        let messages = cache.get_messages();
        assert_eq!(messages[0].message_type, MessageType::Error);
        assert_eq!(messages[1].message_type, MessageType::Warning);
        assert_eq!(messages[2].message_type, MessageType::Info);
        assert_eq!(messages[3].message_type, MessageType::Log);
    }

    #[test]
    fn test_timestamp_ordering() {
        let mut cache = NotificationCache::new();

        cache.store_log(LogLevel::Info, "first".to_string());
        std::thread::sleep(std::time::Duration::from_millis(10));
        cache.store_log(LogLevel::Info, "second".to_string());

        let logs = cache.get_logs();
        assert!(logs[0].timestamp < logs[1].timestamp);
    }

    #[test]
    fn test_store_diagnostics_empty_list() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        let diagnostic = LspDiagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 5,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: "test error".to_string(),
            code: None,
            source: None,
            code_description: None,
            related_information: None,
            tags: None,
            data: None,
        };

        cache.store_diagnostics(&uri, Some(1), vec![diagnostic]);
        assert_eq!(
            cache
                .get_diagnostics(uri.as_str())
                .unwrap()
                .diagnostics
                .len(),
            1
        );

        cache.store_diagnostics(&uri, Some(2), vec![]);
        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.diagnostics.len(), 0);
        assert_eq!(stored.version, Some(2));
    }

    #[test]
    fn test_store_many_diagnostics_single_file() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        let diagnostics: Vec<LspDiagnostic> = (0..100)
            .map(|i| LspDiagnostic {
                range: Range {
                    start: Position {
                        line: i,
                        character: 0,
                    },
                    end: Position {
                        line: i,
                        character: 10,
                    },
                },
                message: format!("Error {i}"),
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                code: None,
                source: None,
                code_description: None,
                related_information: None,
                tags: None,
                data: None,
            })
            .collect();

        cache.store_diagnostics(&uri, Some(1), diagnostics);

        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.diagnostics.len(), 100);
    }

    #[test]
    fn test_logs_exact_capacity_boundary() {
        let mut cache = NotificationCache::new();

        for i in 0..MAX_LOG_ENTRIES {
            cache.store_log(LogLevel::Info, format!("message {i}"));
        }
        assert_eq!(cache.logs_count(), MAX_LOG_ENTRIES);

        cache.store_log(LogLevel::Info, "overflow".to_string());
        assert_eq!(cache.logs_count(), MAX_LOG_ENTRIES);
        assert_eq!(cache.get_logs().front().unwrap().message, "message 1");
    }

    #[test]
    fn test_messages_exact_capacity_boundary() {
        let mut cache = NotificationCache::new();

        for i in 0..MAX_SERVER_MESSAGES {
            cache.store_message(MessageType::Info, format!("message {i}"));
        }
        assert_eq!(cache.messages_count(), MAX_SERVER_MESSAGES);

        cache.store_message(MessageType::Info, "overflow".to_string());
        assert_eq!(cache.messages_count(), MAX_SERVER_MESSAGES);
        assert_eq!(cache.get_messages().front().unwrap().message, "message 1");
    }

    #[test]
    fn test_clear_diagnostics_nonexistent() {
        let mut cache = NotificationCache::new();
        let result = cache.clear_diagnostics("file:///nonexistent.rs");
        assert!(result.is_none());
    }

    #[test]
    fn test_store_diagnostics_no_version() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        cache.store_diagnostics(&uri, None, vec![]);
        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.version, None);
    }
}
