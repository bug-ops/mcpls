---
aliases:
  - MCP resources diagnostics
  - Diagnostics subscriptions
tags:
  - sdd
  - spec
  - enhancement
  - mcp
  - resources
  - diagnostics
created: 2026-07-30
status: draft
related:
  - "[[constitution]]"
  - "[[bridge/004-get-diagnostics-flycheck-gap/spec]]"
---

# Feature: Expose LSP Diagnostics as MCP Resources with Subscriptions

> [!info] Metadata
> **Type**: enhancement
> **Priority**: P3

## 1. Overview

### Problem Statement

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

## 2. User Stories

### US-001: Push notifications for diagnostics changes

AS AN AI agent editing a file,
I WANT to receive a notification when new diagnostics arrive rather than polling `get_diagnostics` repeatedly,
SO THAT I can respond to errors and warnings in real time.

### US-002: Subscribe to diagnostics resources

AS AN MCP client,
I WANT to subscribe to `lsp-diagnostics://path/to/file` and receive updates without issuing repeated tool calls,
SO THAT my implementation can be event-driven rather than polling-based.

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | mcpls must declare `resources` capability with `subscribe: true` in its `ServerInfo` | must |
| FR-002 | mcpls must implement `resources/list` returning URIs of the form `lsp-diagnostics://<absolute-path>` for each open document | must |
| FR-003 | mcpls must implement `resources/read` for `lsp-diagnostics://` URIs, returning the current `NotificationCache` contents for that path as JSON | must |
| FR-004 | mcpls must implement `resources/subscribe` — when a client subscribes, the notification pump must emit `notifications/resources/updated` whenever the diagnostics cache for that path changes | must |
| FR-005 | Existing `get_cached_diagnostics` tool must remain for clients that do not support resources | must |

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Zero extra LSP round-trips: use the existing notification pump output |
| NFR-002 | Compatibility | rmcp crate must expose resource registration and `notifications/resources/updated` send API — verify before implementation (rmcp 3.2.0 currently used) |

## 5. See Also

- [[constitution]] — project principles
- [[bridge/004-get-diagnostics-flycheck-gap/spec]] — uses and extends the `NotificationCache` design from this spec
- MCP 2025-11-25 resources spec: https://modelcontextprotocol.io/specification/2025-11-25/server/resources
- Tritlo/lsp-mcp: https://github.com/Tritlo/lsp-mcp
- mcpls NotificationCache: `crates/mcpls-core/src/bridge/notifications.rs`
