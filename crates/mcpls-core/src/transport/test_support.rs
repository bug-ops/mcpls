//! Constants shared by the HTTP transport's session-manager and end-to-end tests.

/// Idle timeout short enough for a test to see a session reaped.
pub const TEST_IDLE: std::time::Duration = std::time::Duration::from_secs(2);

/// Upper bound on any single end-to-end wait, so a regression fails instead of hanging.
pub const E2E_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// A listen lease of `ms` milliseconds that renews on every touch.
pub const fn short_lease(ms: u64) -> super::ListenLease {
    let d = std::time::Duration::from_millis(ms);
    super::ListenLease::Renew(super::LeaseWindow::new(d, d).unwrap())
}

/// Builds a `McplsServer` with default collaborators and the given
/// workspace roots, matching the setup shared by every
/// `run_http`-driving test in this module.
pub fn test_server_with_roots(
    workspace_roots: crate::bridge::WorkspaceRoots,
) -> crate::mcp::McplsServer {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use crate::bridge::{NotificationCache, Translator};
    use crate::config::McpConfig;
    use crate::mcp::{McplsServer, SubscriptionRegistry};

    let translator = Arc::new(Translator::new());
    let notification_cache = Arc::new(Mutex::new(NotificationCache::new()));
    let subs = SubscriptionRegistry::new();
    McplsServer::new(
        translator,
        notification_cache,
        workspace_roots,
        subs,
        crate::ProjectConfigStatus::NotIgnored,
        McpConfig::default(),
    )
}

/// [`test_server_with_roots`] with no workspace roots configured.
pub fn test_server() -> crate::mcp::McplsServer {
    test_server_with_roots(crate::bridge::WorkspaceRoots::default())
}
