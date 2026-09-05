# Spec mcp/002: Expose LSP Diagnostics as MCP Resources with Subscriptions

**Type**: enhancement
**Priority**: P3
**Status**: draft

## Problem Statement

mcpls currently exposes LSP diagnostics only as polling tools (`get_diagnostics`,
`get_cached_diagnostics`). The MCP 2025-11-25 specification introduces first-class
**Resources** with **subscriptions** — clients can subscribe to a resource URI and receive
`notifications/resources/updated` push events when the resource changes.

Competing implementations already use this pattern:
- **Tritlo/lsp-mcp** (Haskell) exposes `lsp-diagnostics://` as a subscribable resource with
  real-time updates when files change.

mcpls's current diagnostics model is a pull-based cache (`NotificationCache`) populated by
the LSP push pump. This is already the right data structure — the gap is surfacing it through
the MCP resource/subscription interface instead of (or in addition to) tool calls.

## User Stories

- As an AI agent editing a file, I want to receive a notification when new diagnostics arrive
  rather than polling `get_diagnostics` repeatedly.
- As an MCP client, I want to subscribe to `lsp-diagnostics://path/to/file` and receive
  updates without issuing repeated tool calls.

## Functional Requirements

1. mcpls must declare `resources` capability with `subscribe: true` in its `ServerInfo`.
2. mcpls must implement `resources/list` returning URIs of the form
   `lsp-diagnostics://<absolute-path>` for each open document.
3. mcpls must implement `resources/read` for `lsp-diagnostics://` URIs, returning the current
   `NotificationCache` contents for that path as JSON.
4. mcpls must implement `resources/subscribe` — when a client subscribes, the notification
   pump must emit `notifications/resources/updated` whenever the diagnostics cache for that
   path changes.
5. Existing `get_cached_diagnostics` tool must remain for clients that do not support resources.

## Non-Functional Requirements

- Zero extra LSP round-trips: use the existing notification pump output.
- rmcp crate must expose resource registration and `notifications/resources/updated` send API
  — verify before implementation (rmcp 1.5.0 currently used).

## See Also

- MCP 2025-11-25 resources spec: https://modelcontextprotocol.io/specification/2025-11-25/server/resources
- Tritlo/lsp-mcp: https://github.com/Tritlo/lsp-mcp
- mcpls NotificationCache: `crates/mcpls-core/src/bridge/notifications.rs`
