//! LSP notification storage and management.
//!
//! Stores diagnostics, log messages, and server messages received from LSP servers.

use std::collections::{BTreeMap, HashMap, VecDeque};

use chrono::{DateTime, Utc};
use lsp_types::{Diagnostic as LspDiagnostic, Uri};
use serde::{Deserialize, Serialize};

use crate::config::ServerId;

/// Maximum number of log entries to store.
const MAX_LOG_ENTRIES: usize = 100;

/// Global budget for distinct-URI diagnostic entries, shared fairly across
/// every registered diagnostics-route server rather than claimed by one
/// server alone.
///
/// Guards against unbounded growth when a spawned LSP server publishes
/// diagnostics for an unbounded number of distinct URIs over a long-running
/// session, matching the bounding already applied to `logs`/`messages`.
/// Each server's own share is `MAX_DIAGNOSTIC_ENTRIES / diagnostics_route_count`
/// (see [`NotificationCache::set_diagnostics_route_count`]): a noisy server
/// can only exhaust its own share and evict its own least-recently-written
/// entries, never another server's (#266).
const MAX_DIAGNOSTIC_ENTRIES: usize = 1000;

/// Normalize a URI string to a stable cache key.
///
/// On Windows, URI comparisons must be case-insensitive: the filesystem is
/// case-insensitive and different tools (e.g. rust-analyzer vs std) may
/// produce drive letters in different cases (`C:` vs `c:`).
/// Lowercasing the entire URI is safe for `file://` URIs because they have
/// no case-sensitive query or fragment components.
fn uri_cache_key(uri: &str) -> std::borrow::Cow<'_, str> {
    if cfg!(windows) {
        std::borrow::Cow::Owned(uri.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(uri)
    }
}

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

impl From<lsp_types::MessageType> for LogLevel {
    fn from(msg_type: lsp_types::MessageType) -> Self {
        match msg_type {
            lsp_types::MessageType::ERROR => Self::Error,
            lsp_types::MessageType::WARNING => Self::Warning,
            lsp_types::MessageType::INFO => Self::Info,
            // LOG and unknown message types default to Debug
            _ => Self::Debug,
        }
    }
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

impl From<lsp_types::MessageType> for MessageType {
    fn from(msg_type: lsp_types::MessageType) -> Self {
        match msg_type {
            lsp_types::MessageType::ERROR => Self::Error,
            lsp_types::MessageType::WARNING => Self::Warning,
            lsp_types::MessageType::INFO => Self::Info,
            // LOG and unknown message types default to Log
            _ => Self::Log,
        }
    }
}

/// Cache for LSP server notifications.
#[derive(Debug)]
pub struct NotificationCache {
    /// Diagnostics indexed by document URI.
    diagnostics: HashMap<String, DiagnosticInfo>,
    /// Server that currently owns each cached URI, so an entry's order map
    /// can be found without scanning every server's.
    diagnostics_owners: HashMap<String, ServerId>,
    /// Per-server `diagnostics` keys ordered oldest-write-first, keyed by a
    /// monotonic sequence number rather than position: a re-publish removes
    /// its old entry by key in `O(log n)` (via `diagnostic_seq`) instead of
    /// scanning for it, which a plain `VecDeque` would require. Bounded per
    /// server to a fair share of `MAX_DIAGNOSTIC_ENTRIES` (see
    /// [`NotificationCache::set_diagnostics_route_count`]), so one server's
    /// write volume can never evict another's entries (#266). Kept in sync
    /// with `diagnostics` by every method that adds or removes an entry.
    diagnostic_order: HashMap<ServerId, BTreeMap<u64, String>>,
    /// Maps each cached URI to its current sequence number in its owner's
    /// `diagnostic_order` map, so a re-publish or clear can find and remove
    /// its old order entry without scanning.
    diagnostic_seq: HashMap<String, u64>,
    /// Next sequence number to assign in `diagnostic_order`. Shared across
    /// every server's order map and monotonically increasing for the
    /// cache's lifetime; never reused, so it never collides with an older
    /// entry still pending eviction.
    next_diagnostic_seq: u64,
    /// Number of registered diagnostics-route servers currently sharing the
    /// `MAX_DIAGNOSTIC_ENTRIES` budget; see
    /// [`NotificationCache::set_diagnostics_route_count`].
    diagnostics_route_count: usize,
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
            diagnostics_owners: HashMap::with_capacity(32),
            diagnostic_order: HashMap::new(),
            diagnostic_seq: HashMap::with_capacity(32),
            next_diagnostic_seq: 0,
            diagnostics_route_count: 1,
            logs: VecDeque::with_capacity(MAX_LOG_ENTRIES),
            messages: VecDeque::with_capacity(MAX_SERVER_MESSAGES),
        }
    }

    /// Configure how many diagnostics-route servers share the global
    /// `MAX_DIAGNOSTIC_ENTRIES` budget.
    ///
    /// Each server's own cap becomes `MAX_DIAGNOSTIC_ENTRIES / count`
    /// (minimum 1): registering more diagnostics-capable servers gives each
    /// a smaller but still fair share, rather than each getting an
    /// independent full `MAX_DIAGNOSTIC_ENTRIES` budget of its own -- which
    /// would let the aggregate cache size grow without bound as more
    /// servers register (#266). Call once after server registration
    /// completes and before diagnostics start flowing. Defaults to `1` if
    /// never called (a single implicit server gets the whole budget).
    pub fn set_diagnostics_route_count(&mut self, count: usize) {
        self.diagnostics_route_count = count.max(1);
    }

    /// Current per-server diagnostics budget: `MAX_DIAGNOSTIC_ENTRIES`
    /// divided fairly across `diagnostics_route_count` servers, floored at 1
    /// so a large server count can never reduce a server's share to zero.
    ///
    /// The floor means the aggregate cache size (`count * per_server_budget`)
    /// only stays within `MAX_DIAGNOSTIC_ENTRIES` while
    /// `count <= MAX_DIAGNOSTIC_ENTRIES`; beyond that, every server still
    /// gets its minimum share of 1 and the aggregate grows with `count`
    /// instead of staying capped. Registering more than `MAX_DIAGNOSTIC_ENTRIES`
    /// diagnostics-route servers is not a realistic deployment today, so this
    /// is documented rather than additionally guarded against.
    fn per_server_budget(&self) -> usize {
        (MAX_DIAGNOSTIC_ENTRIES / self.diagnostics_route_count.max(1)).max(1)
    }

    /// Store diagnostics for a document published by `server_id`.
    ///
    /// If diagnostics already exist for the URI, they are replaced and the
    /// entry is repositioned to the back of its owner's eviction order, so
    /// a URI republished on every edit is tracked as most-recently-written
    /// and evicted last, not first. Each registered server's distinct-URI
    /// entries are bounded independently by its fair share of
    /// `MAX_DIAGNOSTIC_ENTRIES` (see
    /// [`Self::set_diagnostics_route_count`]): once a server's own share is
    /// exhausted, storing diagnostics for a new URI evicts that same
    /// server's least-recently-written entry, never another server's.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::bridge::NotificationCache;
    /// use mcpls_core::config::ServerId;
    /// use lsp_types::Uri;
    ///
    /// let mut cache = NotificationCache::new();
    /// let server: ServerId = "rust-analyzer".into();
    /// let uri: Uri = "file:///main.rs".parse().unwrap();
    /// cache.store_diagnostics(&server, &uri, Some(1), vec![]);
    /// assert!(cache.get_diagnostics(uri.as_str()).is_some());
    /// ```
    pub fn store_diagnostics(
        &mut self,
        server_id: &ServerId,
        uri: &Uri,
        version: Option<i32>,
        diagnostics: Vec<LspDiagnostic>,
    ) {
        let key = uri_cache_key(uri.as_str()).into_owned();
        let info = DiagnosticInfo {
            uri: uri.clone(),
            version,
            diagnostics,
        };

        // Remove the URI's existing order entry, if any -- from its
        // previous owner's order map, whether that's this same server (a
        // republish, repositioned to the back below) or a different one
        // (the diagnostics route changed, e.g. on respawn).
        if let Some(old_seq) = self.diagnostic_seq.remove(&key)
            && let Some(previous_owner) = self.diagnostics_owners.get(&key)
            && let Some(order) = self.diagnostic_order.get_mut(previous_owner)
        {
            order.remove(&old_seq);
        }

        let budget = self.per_server_budget();
        let order = self.diagnostic_order.entry(server_id.clone()).or_default();
        // `while`, not `if`: normally at most one eviction is ever needed
        // here (each store adds exactly one entry), but `budget` can shrink
        // out from under an already-populated server if a caller invokes
        // `set_diagnostics_route_count` again with a larger count after
        // diagnostics have started flowing. `while` guarantees convergence
        // to the new, smaller budget in that case instead of leaving the
        // server permanently one (or more) entries over it.
        while order.len() >= budget
            && let Some((&oldest_seq, oldest_key)) = order.iter().next()
        {
            let oldest_key = oldest_key.clone();
            order.remove(&oldest_seq);
            self.diagnostic_seq.remove(&oldest_key);
            self.diagnostics_owners.remove(&oldest_key);
            self.diagnostics.remove(&oldest_key);
        }

        self.diagnostics_owners
            .insert(key.clone(), server_id.clone());
        let seq = self.next_diagnostic_seq;
        self.next_diagnostic_seq += 1;
        order.insert(seq, key.clone());
        self.diagnostic_seq.insert(key.clone(), seq);
        self.diagnostics.insert(key, info);
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
        self.diagnostics.get(uri_cache_key(uri).as_ref())
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
        let key = uri_cache_key(uri).into_owned();
        if let Some(owner) = self.diagnostics_owners.remove(&key)
            && let Some(seq) = self.diagnostic_seq.remove(&key)
            && let Some(order) = self.diagnostic_order.get_mut(&owner)
        {
            order.remove(&seq);
        }
        self.diagnostics.remove(&key)
    }

    /// Clear all diagnostics owned by a single server.
    ///
    /// Used when a server crashes and respawns: its own stale entries must
    /// be invalidated without disturbing any other server's cache entries
    /// (#266), unlike [`Self::clear_all_diagnostics`].
    ///
    /// # Examples
    ///
    /// ```
    /// use mcpls_core::bridge::NotificationCache;
    /// use mcpls_core::config::ServerId;
    /// use lsp_types::Uri;
    ///
    /// let mut cache = NotificationCache::new();
    /// let crashed: ServerId = "pyright".into();
    /// let healthy: ServerId = "rust-analyzer".into();
    /// let crashed_uri: Uri = "file:///main.py".parse().unwrap();
    /// let healthy_uri: Uri = "file:///main.rs".parse().unwrap();
    /// cache.store_diagnostics(&crashed, &crashed_uri, Some(1), vec![]);
    /// cache.store_diagnostics(&healthy, &healthy_uri, Some(1), vec![]);
    ///
    /// cache.clear_server_diagnostics(&crashed);
    ///
    /// assert!(cache.get_diagnostics(crashed_uri.as_str()).is_none());
    /// assert!(cache.get_diagnostics(healthy_uri.as_str()).is_some());
    /// ```
    pub fn clear_server_diagnostics(&mut self, server_id: &ServerId) {
        let Some(order) = self.diagnostic_order.remove(server_id) else {
            return;
        };
        for (_, key) in order {
            self.diagnostics.remove(&key);
            self.diagnostics_owners.remove(&key);
            self.diagnostic_seq.remove(&key);
        }
    }

    /// Clear all diagnostics, for every server.
    pub fn clear_all_diagnostics(&mut self) {
        self.diagnostics.clear();
        self.diagnostics_owners.clear();
        self.diagnostic_order.clear();
        self.diagnostic_seq.clear();
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

    /// Every test in this module that doesn't exercise multi-server
    /// fairness routes through one implicit server, so `set_diagnostics_route_count`
    /// is left at its default of `1` (full `MAX_DIAGNOSTIC_ENTRIES` budget).
    fn test_server() -> ServerId {
        ServerId::from("test-server")
    }

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

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);

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

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        assert_eq!(cache.diagnostics_count(), 1);

        cache.store_diagnostics(&test_server(), &uri, Some(2), vec![]);
        assert_eq!(cache.diagnostics_count(), 1);

        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.version, Some(2));
    }

    #[test]
    fn test_clear_diagnostics() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///test.rs".parse().unwrap();

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
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

        cache.store_diagnostics(&test_server(), &uri1, Some(1), vec![]);
        cache.store_diagnostics(&test_server(), &uri2, Some(1), vec![]);
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

        cache.store_diagnostics(&test_server(), &uri, Some(1), vec![diagnostic]);
        assert_eq!(
            cache
                .get_diagnostics(uri.as_str())
                .unwrap()
                .diagnostics
                .len(),
            1
        );

        cache.store_diagnostics(&test_server(), &uri, Some(2), vec![]);
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

        cache.store_diagnostics(&test_server(), &uri, Some(1), diagnostics);

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
    fn test_diagnostics_max_capacity() {
        let mut cache = NotificationCache::new();

        for i in 0..MAX_DIAGNOSTIC_ENTRIES + 10 {
            let uri: Uri = format!("file:///test{i}.rs").parse().unwrap();
            cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        }

        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);

        // Oldest entries should be evicted (FIFO).
        let evicted: Uri = "file:///test0.rs".parse().unwrap();
        assert!(cache.get_diagnostics(evicted.as_str()).is_none());
        let newest: Uri = format!("file:///test{}.rs", MAX_DIAGNOSTIC_ENTRIES + 9)
            .parse()
            .unwrap();
        assert!(cache.get_diagnostics(newest.as_str()).is_some());
    }

    #[test]
    fn test_diagnostics_replacing_existing_uri_does_not_trigger_eviction() {
        let mut cache = NotificationCache::new();
        let uri: Uri = "file:///stable.rs".parse().unwrap();

        for i in 0..MAX_DIAGNOSTIC_ENTRIES {
            cache.store_diagnostics(
                &test_server(),
                &uri,
                Some(i32::try_from(i).unwrap()),
                vec![],
            );
        }
        assert_eq!(cache.diagnostics_count(), 1);
        assert!(cache.get_diagnostics(uri.as_str()).is_some());
    }

    #[test]
    fn test_diagnostics_republish_refreshes_eviction_order() {
        // #234 S2 / #266 S3 regression: an actively-edited file, republished
        // on every keystroke, must not be evicted ahead of a file that was
        // merely opened once and never touched again.
        let mut cache = NotificationCache::new();
        let actively_edited: Uri = "file:///keep.rs".parse().unwrap();
        cache.store_diagnostics(&test_server(), &actively_edited, Some(1), vec![]);

        // Fill the rest of the cache with untouched entries.
        for i in 0..MAX_DIAGNOSTIC_ENTRIES - 1 {
            let uri: Uri = format!("file:///untouched{i}.rs").parse().unwrap();
            cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        }
        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);

        // Republish the actively-edited file -- this must move it to the
        // back of the eviction order, not leave it at its original (oldest)
        // position.
        cache.store_diagnostics(&test_server(), &actively_edited, Some(2), vec![]);

        // One more new URI arrives, exceeding the cap by one: the oldest
        // *untouched* entry must be evicted, not the republished one.
        let overflow: Uri = "file:///overflow.rs".parse().unwrap();
        cache.store_diagnostics(&test_server(), &overflow, Some(1), vec![]);

        assert!(
            cache.get_diagnostics(actively_edited.as_str()).is_some(),
            "republished entry must survive eviction after being refreshed"
        );
        let oldest_untouched: Uri = "file:///untouched0.rs".parse().unwrap();
        assert!(
            cache.get_diagnostics(oldest_untouched.as_str()).is_none(),
            "the oldest never-republished entry must be evicted instead"
        );
        assert!(cache.get_diagnostics(overflow.as_str()).is_some());
    }

    #[test]
    fn test_clear_diagnostics_then_refill_does_not_evict_early() {
        let mut cache = NotificationCache::new();
        let first: Uri = "file:///first.rs".parse().unwrap();
        cache.store_diagnostics(&test_server(), &first, Some(1), vec![]);
        cache.clear_diagnostics(first.as_str());
        assert_eq!(cache.diagnostics_count(), 0);

        for i in 0..MAX_DIAGNOSTIC_ENTRIES {
            let uri: Uri = format!("file:///test{i}.rs").parse().unwrap();
            cache.store_diagnostics(&test_server(), &uri, Some(1), vec![]);
        }
        assert_eq!(cache.diagnostics_count(), MAX_DIAGNOSTIC_ENTRIES);
        // Every entry from this batch must still be present -- the earlier
        // clear must not have left a stale `diagnostic_order` entry that
        // causes a premature eviction here.
        let first_of_batch: Uri = "file:///test0.rs".parse().unwrap();
        assert!(cache.get_diagnostics(first_of_batch.as_str()).is_some());
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

        cache.store_diagnostics(&test_server(), &uri, None, vec![]);
        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.version, None);
    }

    /// #266: with a single registered diagnostics-route server (the
    /// default), a noisy server publishing diagnostics for more distinct
    /// URIs than the global budget allows must only evict its own oldest
    /// entries, never a quiet server's, even though both share one
    /// `NotificationCache`.
    #[test]
    fn test_noisy_server_does_not_evict_quiet_server_entries() {
        let mut cache = NotificationCache::new();
        cache.set_diagnostics_route_count(2);
        let noisy = ServerId::from("noisy");
        let quiet = ServerId::from("quiet");

        let quiet_uri: Uri = "file:///quiet/only_file.rs".parse().unwrap();
        cache.store_diagnostics(&quiet, &quiet_uri, Some(1), vec![]);

        let per_server_budget = MAX_DIAGNOSTIC_ENTRIES / 2;
        for i in 0..per_server_budget + 50 {
            let uri: Uri = format!("file:///noisy/file{i}.rs").parse().unwrap();
            cache.store_diagnostics(&noisy, &uri, Some(1), vec![]);
        }

        assert!(
            cache.get_diagnostics(quiet_uri.as_str()).is_some(),
            "quiet server's only entry must survive the noisy server's overflow"
        );

        let noisy_first: Uri = "file:///noisy/file0.rs".parse().unwrap();
        assert!(
            cache.get_diagnostics(noisy_first.as_str()).is_none(),
            "noisy server's own oldest entries must be evicted once its share is exceeded"
        );
    }

    /// #266 M6: registering more diagnostics-route servers gives each a
    /// smaller, fair share of the global budget rather than each getting an
    /// independent full `MAX_DIAGNOSTIC_ENTRIES` -- otherwise the aggregate
    /// cache size would grow unboundedly with the number of servers.
    #[test]
    fn test_fair_share_budget_divided_by_registered_server_count() {
        let mut cache = NotificationCache::new();
        cache.set_diagnostics_route_count(4);
        let server = ServerId::from("one-of-four");

        let expected_share = MAX_DIAGNOSTIC_ENTRIES / 4;
        for i in 0..expected_share + 10 {
            let uri: Uri = format!("file:///file{i}.rs").parse().unwrap();
            cache.store_diagnostics(&server, &uri, Some(1), vec![]);
        }

        assert_eq!(
            cache.diagnostics_count(),
            expected_share,
            "one of four servers must be capped at a quarter of the global budget"
        );
    }

    /// Re-publishing diagnostics for a URI under its existing owner must not
    /// count as a new entry against that server's budget.
    #[test]
    fn test_repeated_writes_same_owner_do_not_grow_order_map() {
        let mut cache = NotificationCache::new();
        let server = ServerId::from("server");
        let uri: Uri = "file:///test.rs".parse().unwrap();

        let max_version = i32::try_from(MAX_DIAGNOSTIC_ENTRIES).unwrap() + 10;
        for version in 0..max_version {
            cache.store_diagnostics(&server, &uri, Some(version), vec![]);
        }

        assert_eq!(cache.diagnostics_count(), 1);
        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.version, Some(max_version - 1));
    }

    /// If a URI's diagnostics route changes to a different server (e.g.
    /// after a respawn rebind), the entry must move to the new owner's
    /// order map rather than staying attributed to the old one.
    #[test]
    fn test_store_diagnostics_reassigns_ownership() {
        let mut cache = NotificationCache::new();
        let old_owner = ServerId::from("old");
        let new_owner = ServerId::from("new");
        let uri: Uri = "file:///test.rs".parse().unwrap();

        cache.store_diagnostics(&old_owner, &uri, Some(1), vec![]);
        cache.store_diagnostics(&new_owner, &uri, Some(2), vec![]);

        assert_eq!(cache.diagnostics_count(), 1);
        let stored = cache.get_diagnostics(uri.as_str()).unwrap();
        assert_eq!(stored.version, Some(2));

        // The old owner's order map must no longer reference this URI:
        // filling the old owner's budget with fresh entries must not evict
        // this URI a second time (it's not there to evict) nor corrupt state.
        for i in 0..MAX_DIAGNOSTIC_ENTRIES + 5 {
            let other: Uri = format!("file:///old/file{i}.rs").parse().unwrap();
            cache.store_diagnostics(&old_owner, &other, Some(1), vec![]);
        }
        assert!(cache.get_diagnostics(uri.as_str()).is_some());
    }

    /// #266 S2: clearing one server's diagnostics must not disturb another
    /// server's cached entries, unlike `clear_all_diagnostics`.
    #[test]
    fn test_clear_server_diagnostics_scopes_to_one_server() {
        let mut cache = NotificationCache::new();
        let crashed = ServerId::from("crashed");
        let healthy = ServerId::from("healthy");

        let crashed_uri: Uri = "file:///crashed/main.py".parse().unwrap();
        let healthy_uri: Uri = "file:///healthy/main.rs".parse().unwrap();
        cache.store_diagnostics(&crashed, &crashed_uri, Some(1), vec![]);
        cache.store_diagnostics(&healthy, &healthy_uri, Some(1), vec![]);

        cache.clear_server_diagnostics(&crashed);

        assert!(cache.get_diagnostics(crashed_uri.as_str()).is_none());
        assert!(cache.get_diagnostics(healthy_uri.as_str()).is_some());
        assert_eq!(cache.diagnostics_count(), 1);

        // Idempotent / no-op for a server with no (or no longer any) entries.
        cache.clear_server_diagnostics(&crashed);
        assert_eq!(cache.diagnostics_count(), 1);
    }

    /// M8: `set_diagnostics_route_count` can shrink a server's budget out
    /// from under entries it already holds (e.g. more servers register
    /// later). A single subsequent `store_diagnostics` call must evict
    /// enough entries in one pass to converge to the new, smaller budget --
    /// an `if` here would only ever evict one entry per call, leaving the
    /// server permanently over budget.
    #[test]
    fn test_shrinking_budget_converges_in_a_single_store_call() {
        let mut cache = NotificationCache::new();
        let server = ServerId::from("server");

        for i in 0..5 {
            let uri: Uri = format!("file:///file{i}.rs").parse().unwrap();
            cache.store_diagnostics(&server, &uri, Some(1), vec![]);
        }
        assert_eq!(cache.diagnostics_count(), 5);

        // MAX_DIAGNOSTIC_ENTRIES / 500 == 2: a drastic shrink relative to
        // the 5 entries already held.
        cache.set_diagnostics_route_count(500);

        let new_uri: Uri = "file:///new.rs".parse().unwrap();
        cache.store_diagnostics(&server, &new_uri, Some(1), vec![]);

        assert_eq!(
            cache.diagnostics_count(),
            2,
            "a single store after a budget shrink must evict down to the new budget in one pass"
        );
    }
}
